//! Operator-facing runtime text (issue #357), pinned over the whole
//! production source.
//!
//! `tests/help_text.rs` walks the clap tree; runtime text — errors,
//! warnings, notices, printed output, log lines — has no such tree, so this
//! reads `src/**/*.rs` itself. A small Rust lexer separates code, comments
//! and string literals and drops every `#[cfg(test)]` item, then two rules
//! are checked:
//!
//! 1. **No issue numbers in a string literal.** A `#139` in a message tells
//!    an operator nothing; the *why* is said in words, or with an ADR
//!    reference. Comments and tests keep theirs. The exception is text that
//!    is written TO TAPE (the system guide and RESTORE.sh): those bytes are
//!    pinned by `tests/on_tape_golden.rs` and changing them is a format
//!    decision, not a wording fix — so those files are allowed, and are the
//!    positive control that the scan sees inside multi-line raw strings.
//! 2. **No Debug sigil in an operator-level log field.** `info!`, `warn!`
//!    and `error!` print at the default level; `field = ?value` renders
//!    Rust's `{:?}` (`Some(..)`, `Variant { .. }`) into what the operator
//!    reads. `%value` (Display) or a method that returns words is the form.
//!
//! What it deliberately does not check: `{:?}` inside format strings, which
//! is also how a `String` the operator typed is quoted — those sites are
//! pinned one by one by the tests beside them.

use std::path::{Path, PathBuf};

/// Files whose string literals are on-tape bytes (see the module doc).
const ON_TAPE_TEXT: &[&str] = &["src/volume/layout.rs", "src/volume/restore_script.rs"];

/// One Rust source file, lexed: the string literals of its production code
/// (with the line each starts on), and that code with every comment removed
/// and every literal replaced by `""`.
struct Lexed {
    literals: Vec<(usize, String)>,
    code: String,
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn lex(src: &str) -> Lexed {
    let c: Vec<char> = src.chars().collect();
    let n = c.len();
    let at = |k: usize| c.get(k).copied();
    let mut i = 0;
    let mut line = 1;
    let mut code = String::new();
    // (offset in `code` where the literal stood, line, text)
    let mut found: Vec<(usize, usize, String)> = Vec::new();

    while i < n {
        let ch = c[i];
        let after_ident = i > 0 && is_ident(c[i - 1]);

        // `//` comment, doc comments included.
        if ch == '/' && at(i + 1) == Some('/') {
            while i < n && c[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // `/* */` comment; they nest.
        if ch == '/' && at(i + 1) == Some('*') {
            let mut depth = 0;
            while i < n {
                if c[i] == '/' && at(i + 1) == Some('*') {
                    depth += 1;
                    i += 2;
                } else if c[i] == '*' && at(i + 1) == Some('/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    if c[i] == '\n' {
                        line += 1;
                        code.push('\n');
                    }
                    i += 1;
                }
            }
            continue;
        }
        // Raw string: r"..", r#".."#, br"..".
        if !after_ident && (ch == 'r' || (ch == 'b' && at(i + 1) == Some('r'))) {
            let mut j = if ch == 'b' { i + 2 } else { i + 1 };
            let mut hashes = 0;
            while at(j) == Some('#') {
                hashes += 1;
                j += 1;
            }
            if at(j) == Some('"') {
                let start = line;
                let mut text = String::new();
                j += 1;
                while j < n {
                    if c[j] == '"' && (1..=hashes).all(|k| at(j + k) == Some('#')) {
                        j += 1 + hashes;
                        break;
                    }
                    if c[j] == '\n' {
                        line += 1;
                    }
                    text.push(c[j]);
                    j += 1;
                }
                found.push((code.len(), start, text));
                code.push_str("\"\"");
                i = j;
                continue;
            }
        }
        // Ordinary or byte string.
        if ch == '"' || (ch == 'b' && !after_ident && at(i + 1) == Some('"')) {
            let mut j = if ch == 'b' { i + 2 } else { i + 1 };
            let start = line;
            let mut text = String::new();
            while j < n {
                match c[j] {
                    '\\' if at(j + 1) == Some('\n') => {
                        // Line continuation: the newline and the next line's
                        // leading whitespace are not part of the string.
                        j += 1;
                        while j < n && c[j].is_whitespace() {
                            if c[j] == '\n' {
                                line += 1;
                            }
                            j += 1;
                        }
                    }
                    '\\' => {
                        text.push('\\');
                        if let Some(e) = at(j + 1) {
                            text.push(e);
                        }
                        j += 2;
                    }
                    '"' => {
                        j += 1;
                        break;
                    }
                    other => {
                        if other == '\n' {
                            line += 1;
                        }
                        text.push(other);
                        j += 1;
                    }
                }
            }
            found.push((code.len(), start, text));
            code.push_str("\"\"");
            i = j;
            continue;
        }
        // Char literal (so `'"'` and `'{'` are not taken for code), or a
        // lifetime, which is left alone.
        if ch == '\'' {
            if at(i + 1) == Some('\\') {
                let mut j = i + 3;
                while j < n && c[j] != '\'' {
                    j += 1;
                }
                code.push_str("' '");
                i = j + 1;
                continue;
            }
            if at(i + 2) == Some('\'') {
                code.push_str("' '");
                i += 3;
                continue;
            }
        }
        if ch == '\n' {
            line += 1;
        }
        code.push(ch);
        i += 1;
    }

    // Drop every `#[cfg(test)]` item: the attribute through the end of the
    // item it gates — its `{ .. }` block, or the `;` of a one-line item.
    let mut test_spans: Vec<(usize, usize)> = Vec::new();
    let bytes = code.as_bytes();
    let mut from = 0;
    while let Some(k) = code[from..].find("#[cfg(test)]") {
        let start = from + k;
        let mut j = start + "#[cfg(test)]".len();
        let mut nest = 0i32;
        let mut end = code.len();
        while j < bytes.len() {
            match bytes[j] {
                b'(' | b'[' => nest += 1,
                b')' | b']' => nest -= 1,
                b';' if nest == 0 => {
                    end = j + 1;
                    break;
                }
                b'{' if nest == 0 => {
                    let mut depth = 0;
                    while j < bytes.len() {
                        match bytes[j] {
                            b'{' => depth += 1,
                            b'}' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        j += 1;
                    }
                    end = (j + 1).min(code.len());
                    break;
                }
                _ => {}
            }
            j += 1;
        }
        test_spans.push((start, end));
        from = end.min(code.len());
    }
    let in_test = |offset: usize| test_spans.iter().any(|&(s, e)| offset >= s && offset < e);

    let literals = found
        .into_iter()
        .filter(|(offset, _, _)| !in_test(*offset))
        .map(|(_, line, text)| (line, text))
        .collect();
    let mut production = String::new();
    let mut last = 0;
    for &(s, e) in &test_spans {
        production.push_str(&code[last..s.max(last)]);
        last = e.max(last);
    }
    production.push_str(&code[last.min(code.len())..]);
    Lexed {
        literals,
        code: production,
    }
}

/// An issue reference: `#` then a number that does not start with 0 and is
/// not the start of a longer word — so `#139` counts and a colour such as
/// `#000000` or `#1a1a1a` does not.
fn issue_reference(text: &str) -> Option<String> {
    let c: Vec<char> = text.chars().collect();
    (0..c.len()).find_map(|i| {
        if c[i] != '#' || !matches!(c.get(i + 1), Some('1'..='9')) {
            return None;
        }
        let end = (i + 1..c.len())
            .find(|&j| !c[j].is_ascii_digit())
            .unwrap_or(c.len());
        if c.get(end).is_some_and(|&x| is_ident(x)) {
            return None;
        }
        Some(c[i..end].iter().collect())
    })
}

/// Every `info!`/`warn!`/`error!` invocation in `code` (already free of
/// comments and literal text), as the text between its parentheses.
fn operator_log_calls(code: &str) -> Vec<String> {
    let c: Vec<char> = code.chars().collect();
    let mut calls = Vec::new();
    for name in ["info!", "warn!", "error!"] {
        let pat: Vec<char> = name.chars().collect();
        let mut i = 0;
        while i + pat.len() < c.len() {
            let starts_word = i == 0 || !is_ident(c[i - 1]);
            if starts_word && c[i..i + pat.len()] == pat[..] {
                let mut j = i + pat.len();
                while j < c.len() && c[j].is_whitespace() {
                    j += 1;
                }
                if j < c.len() && c[j] == '(' {
                    let open = j;
                    let mut depth = 0;
                    while j < c.len() {
                        match c[j] {
                            '(' => depth += 1,
                            ')' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        j += 1;
                    }
                    calls.push(c[open + 1..j.min(c.len())].iter().collect());
                }
                i = j;
            } else {
                i += 1;
            }
        }
    }
    calls
}

/// A tracing Debug sigil in a macro's arguments: `?` where a field value
/// starts (after `(`, `,` or `=`) — not the `?` operator, which follows an
/// expression.
fn debug_sigil(args: &str) -> Option<String> {
    let c: Vec<char> = args.chars().collect();
    (0..c.len()).find_map(|i| {
        if c[i] != '?' {
            return None;
        }
        let before = c[..i].iter().rev().find(|x| !x.is_whitespace());
        let after = c.get(i + 1).copied();
        let value_start = matches!(before, Some('(' | ',' | '=') | None);
        let names_something = after.is_some_and(|x| is_ident(x) || matches!(x, '&' | '*' | '('));
        (value_start && names_something).then(|| {
            let end = (i + 1..c.len())
                .find(|&j| matches!(c[j], ',' | ')'))
                .unwrap_or(c.len());
            c[i..end].iter().collect()
        })
    })
}

fn production_sources() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(root.join("src"))
        .into_iter()
        .filter_map(|e| e.ok())
        .map(|e| e.into_path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|p| {
            let rel = p
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&p).unwrap();
            (rel, text)
        })
        .collect()
}

#[test]
fn the_lexer_finds_what_it_must_and_nothing_it_must_not() {
    let src = r####"
// a comment names #11
/// a doc comment names #12
fn f() -> Result<()> {
    let q = '"';
    let brace = '{';
    let esc = '\'';
    bail!("see #13 for why");
    let raw = r#"a raw "quoted" line
    naming #14"#;
    let cont = "one \
                two #15";
    let colour = "#000000 and #1a1a1a";
    warn!(x = %y, n = g()?, "fine");
    info!(label, source = ?s, "not fine");
    error!(?e, "not fine either");
    Ok(())
}
#[cfg(test)]
mod tests {
    fn t() { assert!(false, "a test names #16 {}", '}'); }
}
#[cfg(test)]
use something::only_for_tests;
fn g<'a>(x: &'a str) -> &'a str { "after the tests #17" }
"####;
    let lexed = lex(src);
    let refs: Vec<String> = lexed
        .literals
        .iter()
        .filter_map(|(_, t)| issue_reference(t))
        .collect();
    assert_eq!(
        refs,
        vec!["#13", "#14", "#15", "#17"],
        "{:?}",
        lexed.literals
    );
    let line_of = |needle: &str| {
        lexed
            .literals
            .iter()
            .find(|(_, t)| t.contains(needle))
            .map(|(l, _)| *l)
    };
    assert_eq!(line_of("#13"), Some(8));
    assert_eq!(
        line_of("#14"),
        Some(9),
        "a literal is reported on its first line"
    );
    assert_eq!(
        line_of("two #15"),
        Some(11),
        "continuation joins without the newline"
    );
    assert!(!lexed.code.contains("mod tests"), "{}", lexed.code);
    assert!(!lexed.code.contains("only_for_tests"), "{}", lexed.code);
    assert!(lexed.code.contains("fn g<'a>"), "{}", lexed.code);

    let sigils: Vec<String> = operator_log_calls(&lexed.code)
        .iter()
        .filter_map(|a| debug_sigil(a))
        .collect();
    assert_eq!(sigils, vec!["?s", "?e"], "{}", lexed.code);
}

#[test]
fn runtime_strings_carry_no_issue_numbers() {
    let mut literals = 0;
    let mut on_tape_hits = std::collections::BTreeMap::new();
    let mut offenders = Vec::new();
    for (path, text) in production_sources() {
        let lexed = lex(&text);
        literals += lexed.literals.len();
        for (line, lit) in &lexed.literals {
            if let Some(r) = issue_reference(lit) {
                if ON_TAPE_TEXT.contains(&path.as_str()) {
                    *on_tape_hits.entry(path.clone()).or_insert(0) += 1;
                } else {
                    offenders.push(format!("{path}:{line}: {r}"));
                }
            }
        }
    }
    // Real offenders are reported FIRST, so a tree that also happens to be
    // small cannot hide them behind the scan-size control below.
    assert!(
        offenders.is_empty(),
        "runtime text an operator reads carries issue numbers (say the why in words, or \
         cite the ADR; comments and tests may keep theirs):\n{}",
        offenders.join("\n")
    );
    // Positive controls: the scan read the source (thousands of literals —
    // a floor well below today's ~5,000 so ordinary refactors do not trip
    // it), and it sees inside the multi-line raw strings the on-tape files
    // are built from — each of them names issues in its shell comments.
    assert!(
        literals > 2000,
        "the scan found only {literals} string literals — is it reading src/?"
    );
    for file in ON_TAPE_TEXT {
        assert!(
            on_tape_hits.get(*file).copied().unwrap_or(0) > 0,
            "positive control: {file}'s on-tape text names issues, but the scan saw none — \
             either the lexer is blind to its raw strings, or the file no longer needs its \
             allowance and it should be removed from ON_TAPE_TEXT"
        );
    }
}

#[test]
fn operator_level_log_fields_carry_no_debug_rendering() {
    let mut calls = 0;
    let mut offenders = Vec::new();
    for (path, text) in production_sources() {
        for args in operator_log_calls(&lex(&text).code) {
            calls += 1;
            if let Some(s) = debug_sigil(&args) {
                offenders.push(format!("{path}: {s}"));
            }
        }
    }
    assert!(
        calls > 50,
        "the scan found only {calls} info!/warn!/error! calls"
    );
    assert!(
        offenders.is_empty(),
        "a log field an operator reads renders Rust's {{:?}} — use %value or a method \
         that returns words:\n{}",
        offenders.join("\n")
    );
}
