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
