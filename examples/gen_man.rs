//! Regenerate the committed man pages under `docs/man/`.
//!
//! Run with:
//!
//!     cargo run --example gen_man
//!
//! This rebuilds `docs/man/tapectl.1` from the top-level `Cli` definition
//! plus one page per subcommand **at every depth** — `tapectl-unit.1`,
//! `tapectl-unit-status.1`, and so on. Running it without arguments is
//! idempotent — the only diff between runs should be the embedded
//! version/date, which clap_mangen pulls from `Cargo.toml`.
//!
//! Recursion matters (issue #88): this used to walk only the first level,
//! so a page like `tapectl-unit.1` listed `status` and `mark-tape-only` by
//! name and one-line summary while documenting **none of their flags**
//! anywhere — `grep -c force docs/man/tapectl-unit.1` returned 0 despite
//! `mark-tape-only --force` being long-standing. Those are precisely the
//! flags an operator consults a manual for, since they gate destructive
//! actions.
//!
//! It also silently weakened CI: the man-drift check regenerates and diffs,
//! so while nested flags were never emitted, a nested flag change could not
//! produce drift — the check could not fail for the thing it exists to
//! catch, which is worse than not having it.
//!
//! Committing the output keeps the pages visible without forcing users to
//! have `clap_mangen` to read them.

use std::fs;
use std::path::{Path, PathBuf};

use clap::CommandFactory;
use clap_mangen::Man;
use tapectl::cli::Cli;

/// Render one page per subcommand of `cmd`, recursing to any depth.
///
/// `prefix` is the dash-joined ancestry (`tapectl`, then `tapectl-unit`, …)
/// so each page's header reads as its real invocation path.
fn write_subcommand_pages(
    cmd: &clap::Command,
    prefix: &str,
    out_dir: &Path,
) -> std::io::Result<()> {
    for sub in cmd.get_subcommands() {
        let name = sub.get_name();
        // Skip clap's auto-generated `help` subcommand at every level —
        // `man tapectl-unit-help` would just duplicate `-h`.
        if name == "help" {
            continue;
        }

        let full = format!("{prefix}-{name}");
        let page_path = out_dir.join(format!("{full}.1"));

        // `Man::new` with the bare subcommand would title the page `status`;
        // pre-qualify so it reads `tapectl-unit-status(1)`. The leak is
        // deliberate and bounded — this is a one-shot generator, and clap
        // wants a `'static` name.
        let qualified_name: &'static str = Box::leak(full.clone().into_boxed_str());
        let qualified = sub.clone().name(qualified_name);

        let mut buf = Vec::new();
        Man::new(qualified).render(&mut buf)?;
        fs::write(&page_path, &buf)?;
        println!("wrote {}", page_path.display());

        // Recurse into this subcommand's own children. Walk `sub`, not the
        // renamed clone — renaming is only for the rendered header.
        write_subcommand_pages(sub, &full, out_dir)?;
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/man");
    fs::create_dir_all(&out_dir)?;

    let mut cmd = Cli::command();
    cmd.build();

    // Top-level page.
    let top = out_dir.join("tapectl.1");
    let mut buf = Vec::new();
    Man::new(cmd.clone()).render(&mut buf)?;
    fs::write(&top, &buf)?;
    println!("wrote {}", top.display());

    write_subcommand_pages(&cmd, "tapectl", &out_dir)?;

    Ok(())
}
