//! Operator-facing help text (issue #357).
//!
//! Every `about`, `long_about` and argument help string in the clap tree is
//! read by an operator (and by an heir following `docs/cli/`), so it must
//! not carry GitHub issue numbers — the *why* an operator needs is said in
//! words or with an ADR reference — and it must not teach a `--device
//! /dev/nstN` example: tape device numbering is not stable across reboots,
//! so examples use the `/dev/tape/by-id/…-nst` path. This walks the whole
//! tree rather than a list of commands, so a new subcommand is covered the
//! day it is added.

use clap::CommandFactory;
use tapectl::cli::Cli;

/// `(command path, text)` for every help string under `cmd`.
fn help_texts(cmd: &clap::Command, path: &str, out: &mut Vec<(String, String)>) {
    let own = [
        cmd.get_about(),
        cmd.get_long_about(),
        cmd.get_before_help(),
        cmd.get_before_long_help(),
        cmd.get_after_help(),
        cmd.get_after_long_help(),
    ];
    for text in own.into_iter().flatten() {
        out.push((path.to_string(), text.to_string()));
    }
    for arg in cmd.get_arguments() {
        for text in [arg.get_help(), arg.get_long_help()].into_iter().flatten() {
            out.push((format!("{path} --{}", arg.get_id()), text.to_string()));
        }
    }
    for sub in cmd.get_subcommands() {
        help_texts(sub, &format!("{path} {}", sub.get_name()), out);
    }
}

fn all_help_texts() -> Vec<(String, String)> {
    let mut out = Vec::new();
    help_texts(&Cli::command(), "tapectl", &mut out);
    out
}

/// `#` immediately followed by a digit: an issue reference like `#139`.
fn issue_reference(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    (0..bytes.len().saturating_sub(1))
        .find(|&i| bytes[i] == b'#' && bytes[i + 1].is_ascii_digit())
        .map(|i| {
            let end = (i + 1..bytes.len())
                .find(|&j| !bytes[j].is_ascii_digit())
                .unwrap_or(bytes.len());
            &text[i..end]
        })
}

#[test]
fn help_text_carries_no_issue_numbers() {
    let texts = all_help_texts();
    // Positive control: the walk must actually reach help text, or "found
    // nothing" would pass by searching nothing.
    assert!(
        texts.len() > 200,
        "the clap walk found only {} help strings",
        texts.len()
    );
    assert!(issue_reference("see #139 here") == Some("#139"));
    assert!(issue_reference("slice 1 of 3, ADR-0012").is_none());

    let offenders: Vec<String> = texts
        .iter()
        .filter_map(|(path, text)| issue_reference(text).map(|n| format!("{path}: {n}")))
        .collect();
    assert!(
        offenders.is_empty(),
        "help text an operator reads carries issue numbers (say the why in words, or \
         cite the ADR):\n{}",
        offenders.join("\n")
    );
}

#[test]
fn help_examples_never_name_a_numbered_tape_device() {
    let offenders: Vec<String> = all_help_texts()
        .into_iter()
        .filter(|(_, text)| {
            text.contains("--device /dev/nst") || text.contains("--device-tape /dev/nst")
        })
        .map(|(path, text)| format!("{path}: {text}"))
        .collect();
    assert!(
        offenders.is_empty(),
        "help examples must use /dev/tape/by-id/…-nst — /dev/nstN numbering is not \
         stable across reboots:\n{}",
        offenders.join("\n")
    );
}

/// Issue #361: one meaning per word. "Receipt" is the recipient list a
/// stage set was encrypted to (CONTEXT.md), and a warehouse provider's own
/// receipt for a deposit; the per-stage-set text files are stage reports,
/// in `<home>/stage-reports/`. The two help strings that list the home's
/// contents said "receipts" until then.
#[test]
fn receipt_in_help_means_only_the_recipients_or_a_deposit() {
    // `(path, why the word is right there)`.
    const ALLOWED: &[(&str, &str)] = &[
        (
            "tapectl init --escrow_public_key",
            "the escrow receipt: every tape's recipient list names this key",
        ),
        (
            "tapectl volume deposit add --receipt",
            "the warehouse provider's own receipt for a deposit",
        ),
    ];
    let texts = all_help_texts();
    let mentions = |text: &str| text.to_lowercase().contains("receipt");

    // Positive control: each allowance is still used, so the search reads
    // the texts it claims to and the list does not outlive its reasons.
    for (path, why) in ALLOWED {
        assert!(
            texts.iter().any(|(p, t)| p == path && mentions(t)),
            "{path} no longer says \"receipt\" ({why}) — drop it from ALLOWED"
        );
    }
    let offenders: Vec<String> = texts
        .iter()
        .filter(|(p, t)| mentions(t) && !ALLOWED.iter().any(|(a, _)| a == p))
        .map(|(p, t)| format!("{p}: {t}"))
        .collect();
    assert!(
        offenders.is_empty(),
        "\"receipt\" means the recipient list (CONTEXT.md) or a deposit's receipt; the \
         per-stage-set files are stage reports:\n{}",
        offenders.join("\n")
    );

    // The two home listings name the directory by its current name.
    for flag in ["tapectl --home", "tapectl --config"] {
        let said: Vec<String> = texts
            .iter()
            .filter(|(p, _)| p == flag)
            .map(|(_, t)| t.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();
        assert!(
            said.iter()
                .any(|t| t.contains("stage-reports") || t.contains("stage reports")),
            "{flag}'s help lists the home's contents, stage reports among them: {said:?}"
        );
    }
}

/// The top-level commands Wave 6's two other workers bring to the rules
/// below in parallel with this file (1.9.0: 6B the tape side, 6C sources and
/// reports). The coordinator empties it at integration; from then on every
/// command is held to them.
const PENDING_WAVE6: &[&str] = &[
    "volume",
    "cartridge",
    "restore",
    "catalog",
    "report",
    "collection",
    "unit",
    "snapshot",
    "stage",
];

/// The width `-h` is judged at: clap's own when the output is not a
/// terminal, and the width every docs page and script sees.
const HELP_WIDTH: usize = 100;

/// Every visible command under the grouped, built tree the binary parses,
/// with its path, skipping the top-level commands in [`PENDING_WAVE6`],
/// every hidden stub, and the inside of clap's `help` commands (copies of the
/// commands they name, checked where they are).
fn visible_commands() -> Vec<(String, clap::Command)> {
    fn walk(cmd: &clap::Command, path: &str, depth: usize, out: &mut Vec<(String, clap::Command)>) {
        out.push((path.to_string(), cmd.clone()));
        if cmd.get_name() == "help" && depth > 0 {
            return;
        }
        for sub in cmd.get_subcommands() {
            if sub.is_hide_set() || (depth == 0 && PENDING_WAVE6.contains(&sub.get_name())) {
                continue;
            }
            walk(sub, &format!("{path} {}", sub.get_name()), depth + 1, out);
        }
    }
    let mut out = Vec::new();
    walk(&tapectl::cli::command(), "tapectl", 0, &mut out);
    out
}

/// `(path, every help text)` for every visible command and option: the
/// commands of [`visible_commands`], and each one's own options (a global
/// option once, at the top).
fn visible_help_texts() -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for (path, cmd) in visible_commands() {
        let own = [
            cmd.get_about(),
            cmd.get_long_about(),
            cmd.get_before_help(),
            cmd.get_before_long_help(),
            cmd.get_after_help(),
            cmd.get_after_long_help(),
        ];
        out.push((
            path.clone(),
            own.into_iter().flatten().map(|t| t.to_string()).collect(),
        ));
        let top = path == "tapectl";
        for arg in cmd
            .get_arguments()
            .filter(|a| !a.is_hide_set() && (top || !a.is_global_set()))
        {
            out.push((
                format!("{path} --{}", arg.get_id()),
                [arg.get_help(), arg.get_long_help()]
                    .into_iter()
                    .flatten()
                    .map(|t| t.to_string())
                    .collect(),
            ));
        }
    }
    out
}

/// The entries of `-h` (rendered at [`HELP_WIDTH`]) that take more than one
/// line: the command's own description above the usage, and each command,
/// argument and option listed under a heading.
fn wrapped_short_help(cmd: &clap::Command) -> Vec<String> {
    let help = cmd.clone().term_width(HELP_WIDTH).render_help().to_string();
    let mut wrapped = Vec::new();
    let about: Vec<&str> = help
        .lines()
        .take_while(|l| !l.starts_with("Usage:"))
        .filter(|l| !l.trim().is_empty())
        .collect();
    if about.len() > 1 {
        wrapped.push(format!("its description: {}", about.join(" / ")));
    }
    let mut entry: Option<&str> = None;
    for line in help.lines().skip_while(|l| !l.starts_with("Usage:")) {
        if line.chars().count() > HELP_WIDTH && !line.starts_with("Usage:") {
            wrapped.push(format!("wider than {HELP_WIDTH}: {line}"));
        }
        // An entry starts at column 2 (a command, an argument, `-x, --x`)
        // or at 6 (an option with no short form); anything else indented
        // is the entry before it, carried on.
        let indent = line.len() - line.trim_start().len();
        let rest = line.trim_start();
        if indent == 2 || (indent == 6 && rest.starts_with("--")) {
            entry = Some(line);
        } else if indent > 0 && !rest.is_empty() {
            wrapped.push(format!("{} / {rest}", entry.unwrap_or("?").trim()));
        } else if indent == 0 {
            entry = None;
        }
    }
    wrapped
}

/// ADR-0012 item 211: `-h` gives one line per command and per option (the
/// first paragraph of its doc comment) and `--help` the rest. Judged on the
/// help itself, rendered at [`HELP_WIDTH`]: an entry that wraps, or that
/// clap moves below its name because it would, takes two. A tree walk, so a
/// command added later is held to it the day it is added.
#[test]
fn every_short_help_is_one_line() {
    let commands = visible_commands();
    // Positive controls: the walk reaches the tree, and the check tells a
    // wrapped entry from a short one.
    assert!(
        commands.len() > 40,
        "the walk found only {} commands",
        commands.len()
    );
    let control = clap::Command::new("control")
        .about("Short")
        .arg(clap::Arg::new("fine").long("fine").help("One short line"))
        .arg(
            clap::Arg::new("long")
                .long("long")
                .help("word ".repeat(HELP_WIDTH / 3)),
        );
    let found = wrapped_short_help(&control);
    assert!(
        !found.is_empty() && found.iter().all(|w| w.contains("--long")),
        "the check finds the one wrapped entry: {found:?}"
    );

    let offenders: Vec<String> = commands
        .iter()
        .flat_map(|(path, cmd)| {
            wrapped_short_help(cmd)
                .into_iter()
                .map(move |w| format!("{path} -h: {w}"))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "-h shows one line per command and option: shorten the first paragraph of its doc \
         comment and put the rest in a second paragraph, which --help shows:\n{}",
        offenders.join("\n")
    );
}

/// The code path `text` names, if it names one: a source file, a Rust path,
/// a design document or a section sign (ADR-0012 item 211: help names no
/// code paths; an operator reads it, and the why is said in words or with
/// an ADR).
fn code_path(text: &str) -> Option<String> {
    for needle in ["src/", "docs/design", "§"] {
        if text.contains(needle) {
            return Some(needle.to_string());
        }
    }
    let words = text.split(|c: char| c.is_whitespace() || "`()[],;\"'".contains(c));
    for word in words {
        let word = word.trim_end_matches(['.', ':']);
        if word.ends_with(".rs") && word.len() > 3 {
            return Some(word.to_string());
        }
        if let Some(i) = word.find("::") {
            let (before, after) = (&word[..i], &word[i + 2..]);
            let ident = |s: &str| {
                s.chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
            };
            if ident(before) && ident(after) {
                return Some(word.to_string());
            }
        }
    }
    None
}

#[test]
fn help_names_no_code_paths() {
    // Positive controls: each shape the ruling names is caught, and plain
    // prose (an ADR, a TOML key, a URL) is not.
    for bad in [
        "see cli::consent",
        "src/staging/clean.rs",
        "`staging/clean.rs`",
        "docs/design/layout-session.md",
        "as §7 says",
        "Generation::parse accepts it",
    ] {
        assert!(code_path(bad).is_some(), "{bad}");
    }
    for good in [
        "ADR-0012 item 211",
        "backends.lto[0].enospc_buffer",
        "https://hc-ping.com/x",
        "time: 10:30",
        "docs/troubleshooting.md",
    ] {
        assert_eq!(code_path(good), None, "{good}");
    }
    let items = visible_help_texts();
    assert!(
        items.len() > 150,
        "the walk found only {} items",
        items.len()
    );
    let offenders: Vec<String> = items
        .iter()
        .flat_map(|(path, texts)| {
            texts
                .iter()
                .filter_map(move |t| code_path(t).map(|p| format!("{path}: {p}")))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "help an operator reads names code paths; say it in words or cite the ADR:\n{}",
        offenders.join("\n")
    );
}

/// ADR-0012 item 211: the top-level help groups the commands by job, in
/// workflow order. Every visible top-level command is in exactly one group,
/// no hidden stub of a removed spelling is in any, and the help the binary
/// prints lists them so.
#[test]
fn the_top_level_help_lists_every_visible_command_once_in_its_group() {
    use tapectl::cli::HELP_GROUPS;
    let mut cmd = tapectl::cli::command();
    let visible: std::collections::BTreeSet<String> = cmd
        .get_subcommands()
        .filter(|s| !s.is_hide_set())
        .map(|s| s.get_name().to_string())
        .collect();
    let hidden: Vec<String> = cmd
        .get_subcommands()
        .filter(|s| s.is_hide_set())
        .map(|s| s.get_name().to_string())
        .collect();
    // Positive controls: the tree has both kinds, `help` among the visible.
    assert!(visible.contains("help") && visible.contains("status"));
    assert!(hidden.iter().any(|h| h == "backend"), "{hidden:?}");

    let grouped: Vec<String> = HELP_GROUPS
        .iter()
        .flat_map(|(_, names)| names.iter().map(|n| n.to_string()))
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    for name in &grouped {
        assert!(seen.insert(name.clone()), "{name} is in two groups");
        assert!(
            visible.contains(name),
            "{name} is grouped and not a visible command"
        );
    }
    assert_eq!(seen, visible, "every visible command is in a group");

    for help in [
        cmd.render_long_help().to_string(),
        cmd.render_help().to_string(),
    ] {
        let listed: Vec<String> = help
            .lines()
            .skip_while(|l| !l.starts_with("Usage:"))
            .take_while(|l| !l.starts_with(tapectl::cli::GLOBAL_OPTIONS))
            .filter_map(|l| l.strip_prefix("  "))
            .filter_map(|l| l.split_whitespace().next())
            .map(str::to_string)
            .collect();
        assert_eq!(
            listed, grouped,
            "the help lists the groups in order:\n{help}"
        );
        let headings: Vec<&str> = help
            .lines()
            .filter_map(|l| l.strip_suffix(':'))
            .filter(|l| !l.starts_with(' '))
            .collect();
        let want: Vec<&str> = HELP_GROUPS
            .iter()
            .map(|(h, _)| *h)
            .chain([tapectl::cli::GLOBAL_OPTIONS])
            .collect();
        assert_eq!(headings, want, "{help}");
        for h in &hidden {
            assert!(!listed.contains(h), "the hidden {h} is listed:\n{help}");
        }
    }
}

#[path = "../examples/cli_md/render.rs"]
#[allow(dead_code)]
mod cli_md;

/// ADR-0012 item 212: the CLI reference has one exit-code table, and it is
/// `tapectl::error::EXIT_CODE_TABLE` rendered: every row of the constant is a
/// row of the reference's table, in order, and the README the generator
/// writes carries that table whole.
#[test]
fn the_reference_s_exit_code_table_is_the_constant() {
    let table = cli_md::render_exit_codes();
    let rows: Vec<&str> = table
        .lines()
        .filter(|l| l.starts_with("| ") && !l.starts_with("| Exit"))
        .collect();
    assert_eq!(rows.len(), tapectl::error::EXIT_CODE_TABLE.len(), "{table}");
    for (line, row) in rows.iter().zip(tapectl::error::EXIT_CODE_TABLE) {
        assert!(line.starts_with(&format!("| {} | ", row.code)), "{line}");
        assert!(line.contains(&row.meaning.replace('|', "\\|")), "{line}");
    }
    use clap::CommandFactory;
    let mut cmd = tapectl::cli::Cli::command().version(tapectl::build_info::PKG_VERSION);
    cmd.build();
    let pages = cli_md::render_all(&cmd);
    let readme = &pages.iter().find(|(n, _)| n == "README.md").unwrap().1;
    assert!(readme.contains(&table), "the README renders the constant");
}
