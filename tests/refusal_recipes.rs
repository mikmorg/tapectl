//! Pins the EXISTENCE half of issue #214's rule — "a message printed at a
//! refusal must name a next step that actually runs in the state that
//! produced the message" — nothing more.
//!
//! It walks `src/**/*.rs`, pulls every `tapectl <subcommand> [<sub> ...]`
//! mention out of string literals and doc comments, and resolves each
//! against the real clap tree built from `tapectl::cli::Cli`
//! (`clap::CommandFactory`, confirmed public at `src/cli/mod.rs:27`). Any
//! `--flag` immediately following a resolved command is checked against
//! that (sub)command's own argument list (including tapectl's global
//! flags: `--json`, `--dry-run`, `--verbose`, `--yes`, `--config`,
//! `--home`).
//!
//! # What this test does NOT pin
//!
//! Existence in the clap definitions is necessary but nowhere near
//! sufficient for a recipe to be correct, and **this test would not have
//! caught any of issue #214's three findings**: `snapshot create`, `key
//! import --escrow`, and `staging clean` all exist, are spelled correctly,
//! and take exactly the flags the broken messages gave them. Their defect
//! was behavioural — each is refused or a no-op in the *exact runtime
//! state* that prints the message pointing at it — and no static scan over
//! source text can see runtime state. Reading the target command's
//! preconditions against the caller's state, case by case, is what
//! actually caught those three; this test is the floor under that work,
//! never a replacement for it. A green run here means "no one typo'd a
//! command or flag name" and nothing more.
//!
//! # Known limitations of the scan itself
//!
//! - String-literal extraction is a hand-rolled lexer, not a full Rust
//!   parser: it does not understand raw strings (`r"..."`, `r#"..."#`) or
//!   byte strings. None are used for user-facing messages in this crate
//!   today.
//! - Only line doc comments (`///`, `//!`) are scanned, not block doc
//!   comments (`/** */`, `/*! */`) — this crate does not use the latter.
//! - `<PLACEHOLDER>` and `{format_arg}` spans, and backticks, are stripped
//!   before tokenizing, so e.g. `` `tapectl volume init <LABEL>` `` resolves
//!   as `volume init` with nothing checked past it — this test does not,
//!   and cannot, verify positional argument counts or names, only
//!   subcommand words and `--flags`.
//! - A `--flag` is only checked when it is the token immediately following
//!   a resolved command (matching the issue's own "immediately after"
//!   wording); a flag two commands into a chained recipe
//!   (`a && b --flag`) is checked against `b`, which is what "immediately
//!   after" means there.
//! - Below the top level, once one real subcommand word has matched, a
//!   further word that looks command-shaped (lowercase letters/hyphens)
//!   but isn't a known subcommand of that node is treated as a genuine
//!   typo and fails the test. This check is deliberately NOT applied at
//!   the top level, where "tapectl" is routinely used as an ordinary
//!   English subject ("tapectl never persists...", "tapectl sweeps
//!   crashed sessions...") — applying it there would flag ordinary prose,
//!   not commands.

use clap::CommandFactory;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// One resolvable node in the clap tree: every long-flag name valid here
/// (this command's own, plus every ancestor's global flags), and the
/// names of its own subcommands (empty at a leaf command).
struct Node {
    flags: HashSet<String>,
    subcommands: HashSet<String>,
}

/// Build a `path words -> Node` index of the whole clap tree, `path`
/// being e.g. `["volume", "deposit", "add"]`. The empty path is the root
/// (`tapectl` itself, before any subcommand word).
fn index_tree() -> HashMap<Vec<String>, Node> {
    let root = tapectl::cli::Cli::command();
    let global_flags: HashSet<String> = root
        .get_arguments()
        .filter_map(|a| a.get_long().map(str::to_string))
        .collect();

    let mut out = HashMap::new();
    out.insert(
        Vec::new(),
        Node {
            flags: global_flags.clone(),
            subcommands: root
                .get_subcommands()
                .map(|c| c.get_name().to_string())
                .collect(),
        },
    );
    walk(&root, Vec::new(), &global_flags, &mut out);
    out
}

fn walk(
    cmd: &clap::Command,
    path: Vec<String>,
    global_flags: &HashSet<String>,
    out: &mut HashMap<Vec<String>, Node>,
) {
    for sub in cmd.get_subcommands() {
        let mut p = path.clone();
        p.push(sub.get_name().to_string());

        let mut flags = global_flags.clone();
        flags.extend(
            sub.get_arguments()
                .filter_map(|a| a.get_long().map(str::to_string)),
        );
        let subcommands = sub
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();

        out.insert(p.clone(), Node { flags, subcommands });
        walk(sub, p, global_flags, out);
    }
}

/// Blank out `<...>` and `{...}` spans (metavariables and `format!`
/// placeholders — never real command/flag tokens) so they cannot be
/// mistaken for one and cannot glue two real tokens together across a
/// blanked span. Also blanks backticks, which are pure markdown in this
/// crate's messages and would otherwise stick to an adjacent word (e.g.
/// `` `tapectl `` with no space before the command name).
fn strip_placeholders(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut angle = 0i32;
    let mut brace = 0i32;
    for c in s.chars() {
        match c {
            '<' => {
                angle += 1;
                out.push(' ');
            }
            '>' if angle > 0 => {
                angle -= 1;
                out.push(' ');
            }
            '{' => {
                brace += 1;
                out.push(' ');
            }
            '}' if brace > 0 => {
                brace -= 1;
                out.push(' ');
            }
            '`' => out.push(' '),
            _ if angle > 0 || brace > 0 => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// Pull every `///`/`//!` doc-comment line's text and every `"..."` string
/// literal's raw content out of `src`, discarding everything else (code,
/// plain `//` comments, `/* */` block comments). See the module doc for
/// what this hand-rolled lexer does not handle.
fn extractable_text(src: &str) -> String {
    let bytes = src.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    let mut out = String::new();
    while i < n {
        let c = bytes[i];
        if c == b'/' && i + 1 < n && bytes[i + 1] == b'/' {
            let is_doc =
                (i + 2 < n && bytes[i + 2] == b'/' && !(i + 3 < n && bytes[i + 3] == b'/'))
                    || (i + 2 < n && bytes[i + 2] == b'!');
            let line_start = i;
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            if is_doc {
                out.push_str(&src[line_start..i]);
                out.push('\n');
            }
            continue;
        }
        if c == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
            i += 2;
            let mut depth = 1;
            while i < n && depth > 0 {
                if bytes[i] == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && i + 1 < n && bytes[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if c == b'"' {
            i += 1;
            let start = i;
            while i < n {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    break;
                }
                i += 1;
            }
            out.push_str(&src[start..i.min(n)]);
            out.push('\n');
            if i < n {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    out
}

/// A `tapectl ...` mention resolved (or not) against the clap tree.
struct Mention {
    /// `tapectl <path words...>`, for reporting.
    label: String,
    /// `Some(reason)` when this mention is broken.
    problem: Option<String>,
}

fn is_command_shaped(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c == '-')
}

fn clean_token(t: &str) -> &str {
    t.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
}

/// Scan already-extracted text for `tapectl <word> [<word> ...]`
/// mentions, greedily resolving as many subcommand words as the clap tree
/// allows, then checking one immediately-following `--flag` if present.
fn find_mentions(text: &str, index: &HashMap<Vec<String>, Node>) -> Vec<Mention> {
    let cleaned = strip_placeholders(text);
    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    let mut mentions = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if clean_token(tokens[i]) != "tapectl" {
            i += 1;
            continue;
        }
        let mut path: Vec<String> = Vec::new();
        let mut j = i + 1;
        let mut problem: Option<String> = None;
        loop {
            let node = index
                .get(&path)
                .expect("every prefix pushed below was verified against the tree first");
            let candidate = tokens.get(j).map(|t| clean_token(t));
            match candidate {
                Some(c) if node.subcommands.contains(c) => {
                    path.push(c.to_string());
                    j += 1;
                }
                // Only enforced once at least one real subcommand word has
                // already matched (see the module doc: "tapectl" alone is
                // routinely a prose subject, "tapectl <real-subcommand>
                // <bogus>" essentially never is).
                Some(c)
                    if !path.is_empty() && !node.subcommands.is_empty() && is_command_shaped(c) =>
                {
                    let mut known: Vec<&String> = node.subcommands.iter().collect();
                    known.sort();
                    let known = known
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    problem = Some(format!(
                        "`tapectl {}` has no subcommand \"{c}\" (known: {known})",
                        path.join(" ")
                    ));
                    break;
                }
                _ => break,
            }
        }
        if path.is_empty() && problem.is_none() {
            i += 1;
            continue; // not a real command mention ("tapectl sweeps...", etc.)
        }
        if problem.is_none() {
            if let Some(raw) = tokens.get(j) {
                let flag_tok =
                    raw.trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-');
                if let Some(name) = flag_tok.strip_prefix("--") {
                    let node = index
                        .get(&path)
                        .expect("path was indexed while building it");
                    if !name.is_empty() && !node.flags.contains(name) {
                        problem = Some(format!(
                            "`tapectl {}` has no `--{name}` flag",
                            path.join(" ")
                        ));
                    }
                }
            }
        }
        let label = if path.is_empty() {
            "tapectl".to_string()
        } else {
            format!("tapectl {}", path.join(" "))
        };
        mentions.push(Mention { label, problem });
        i = j.max(i + 1);
    }
    mentions
}

/// `(file path suffix, the resolved "tapectl ..." label, reason)`.
/// Last resort only — see the module doc. Every entry here is a verified
/// false positive of the scan itself, not a real defect.
const ALLOWLIST: &[(&str, &str, &str)] = &[
    (
        "src/cli/config.rs",
        "tapectl config",
        "module doc comment \"`tapectl config` command bodies (issue #112)\" — \
         \"command\" is the ordinary noun in \"command bodies\", not an attempted \
         `config command` subcommand",
    ),
    (
        "src/cli/db.rs",
        "tapectl db",
        "module doc comment \"`tapectl db` command bodies (issue #112)\" — \
         \"command\" is the ordinary noun in \"command bodies\", not an attempted \
         `db command` subcommand",
    ),
];

fn src_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

#[test]
fn every_tapectl_mention_in_src_resolves_against_the_real_cli() {
    let index = index_tree();
    let mut failures: Vec<String> = Vec::new();
    let mut allowlisted: Vec<String> = Vec::new();
    let mut total = 0usize;

    for entry in walkdir::WalkDir::new(src_root())
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("rs"))
    {
        let path = entry.path().to_path_buf();
        let rel = path
            .strip_prefix(Path::new(env!("CARGO_MANIFEST_DIR")))
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        let src = std::fs::read_to_string(&path).unwrap_or_default();
        let text = extractable_text(&src);

        for m in find_mentions(&text, &index) {
            total += 1;
            let Some(reason) = m.problem else { continue };

            if let Some((_, _, why)) = ALLOWLIST
                .iter()
                .find(|(f, l, _)| rel.ends_with(f) && *l == m.label)
            {
                allowlisted.push(format!("{rel}: {} (allowlisted: {why})", m.label));
                continue;
            }
            failures.push(format!("{rel}: {reason}"));
        }
    }

    eprintln!(
        "refusal_recipes: scanned {total} `tapectl ...` mention(s) under src/, \
         {} allowlisted, {} failing",
        allowlisted.len(),
        failures.len()
    );
    for a in &allowlisted {
        eprintln!("  allowlisted: {a}");
    }

    assert!(
        failures.is_empty(),
        "found {} unresolvable tapectl mention(s) out of {total} scanned:\n{}",
        failures.len(),
        failures.join("\n"),
    );
}

// The scan's own self-test: proof it actually catches something, not just
// a green run because nothing is ever flagged.

#[test]
fn the_scan_flags_a_bogus_subcommand() {
    let index = index_tree();
    let mentions = find_mentions("run `tapectl stage createx` to fix it", &index);
    assert_eq!(
        mentions.len(),
        1,
        "{:?}",
        mentions.iter().map(|m| &m.label).collect::<Vec<_>>()
    );
    assert!(
        mentions[0].problem.is_some(),
        "should have flagged the bogus subcommand"
    );
}

#[test]
fn the_scan_flags_a_bogus_flag() {
    let index = index_tree();
    let mentions = find_mentions("run `tapectl stage create --bogus-flag` to fix it", &index);
    assert_eq!(
        mentions.len(),
        1,
        "{:?}",
        mentions.iter().map(|m| &m.label).collect::<Vec<_>>()
    );
    assert!(
        mentions[0].problem.is_some(),
        "should have flagged the bogus flag"
    );
}

#[test]
fn the_scan_resolves_a_real_command_and_flag() {
    let index = index_tree();
    let mentions = find_mentions(
        "run `tapectl stage create foo --version 3` to fix it",
        &index,
    );
    assert_eq!(
        mentions.len(),
        1,
        "{:?}",
        mentions.iter().map(|m| &m.label).collect::<Vec<_>>()
    );
    assert!(
        mentions[0].problem.is_none(),
        "real command+flag wrongly flagged: {:?}",
        mentions[0].problem
    );
}

#[test]
fn the_scan_resolves_a_three_level_command() {
    let index = index_tree();
    let mentions = find_mentions("`tapectl volume deposit add` first", &index);
    assert_eq!(
        mentions.len(),
        1,
        "{:?}",
        mentions.iter().map(|m| &m.label).collect::<Vec<_>>()
    );
    assert!(mentions[0].problem.is_none(), "{:?}", mentions[0].problem);
}

#[test]
fn the_scan_does_not_flag_ordinary_prose_about_tapectl() {
    let index = index_tree();
    let mentions = find_mentions("tapectl never persists this secret anywhere", &index);
    assert!(
        mentions.iter().all(|m| m.problem.is_none()),
        "false positive on ordinary prose: {:?}",
        mentions.iter().map(|m| &m.label).collect::<Vec<_>>()
    );
}
