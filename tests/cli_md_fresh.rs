//! The committed Markdown command reference (`docs/cli/`) must be exactly what
//! `cargo run --example gen_cli_md` would write from the current clap
//! definitions. A CLI change without a regeneration fails here, naming the
//! page — the reference a GitHub reader sees can never lag the binary.

#[path = "../examples/cli_md/render.rs"]
mod render;

use clap::CommandFactory;
use tapectl::cli::Cli;

#[test]
fn docs_cli_matches_the_clap_definitions() {
    let mut cmd = Cli::command().version(tapectl::build_info::PKG_VERSION);
    cmd.build();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/cli");
    let pages = render::render_all(&cmd);
    assert!(
        pages.len() > 20,
        "expected a page per command, got {}",
        pages.len()
    );
    let mut stale = Vec::new();
    for (name, want) in &pages {
        let have = std::fs::read_to_string(dir.join(name)).unwrap_or_default();
        if &have != want {
            stale.push(name.clone());
        }
    }
    // Pages for commands that no longer exist are stale too.
    let expected: std::collections::HashSet<_> = pages.iter().map(|(n, _)| n.clone()).collect();
    for entry in std::fs::read_dir(&dir).expect("docs/cli exists") {
        let n = entry.unwrap().file_name().to_string_lossy().to_string();
        if n.ends_with(".md") && !expected.contains(&n) {
            stale.push(format!("{n} (no such command)"));
        }
    }
    assert!(
        stale.is_empty(),
        "docs/cli is stale — run `cargo run --example gen_cli_md`: {stale:?}"
    );
}

/// The text of `page` from the heading line `heading` to the next heading of
/// the same level (or the end of the page).
fn section<'a>(page: &'a str, heading: &str) -> &'a str {
    let start = page
        .find(&format!("{heading}\n"))
        .unwrap_or_else(|| panic!("no section {heading:?} in the page"));
    let rest = &page[start..];
    let level = heading.chars().take_while(|c| *c == '#').count();
    let next = format!("\n{} ", "#".repeat(level));
    match rest[heading.len()..].find(&next) {
        Some(end) => &rest[..heading.len() + end],
        None => rest,
    }
}

/// Issue #472: the renderer left out every option whose id is `version`, to
/// hide clap's own `--version`, so a command's own `--version <N>` (the
/// snapshot version a unit command names, which the remedies it prints use:
/// `tapectl staging clean --unit U --version V --force`) was missing from the
/// reference. clap's auto flag is told apart by its action now: the own
/// option is listed, and the auto `-V/--version` and `-h/--help` still are
/// not.
#[test]
fn a_commands_own_version_option_is_in_the_reference() {
    let mut cmd = Cli::command().version(tapectl::build_info::PKG_VERSION);
    cmd.build();
    let pages: std::collections::HashMap<String, String> =
        render::render_all(&cmd).into_iter().collect();

    let create = section(&pages["stage.md"], "### tapectl stage create");
    assert!(
        create.contains("- `--version <VERSION>` — Re-stage a specific snapshot version"),
        "stage create's own --version is missing: {create}"
    );
    let clean = section(&pages["staging.md"], "### tapectl staging clean");
    assert!(
        clean.contains("- `--version <VERSION>` — Narrow release further"),
        "staging clean's own --version is missing: {clean}"
    );

    // The positive control for the filter's other half: clap's own flags are
    // still left out of every page, so a page does not repeat them.
    for (name, page) in &pages {
        assert!(!page.contains("- `-V, --version`"), "{name}");
        assert!(!page.contains("- `-h, --help`"), "{name}");
    }
}
