//! Regenerate the committed man pages under `docs/man/`.
//!
//! Run with:
//!
//!     cargo run --example gen_man
//!
//! This rebuilds `docs/man/tapectl.1` from the top-level `Cli` definition
//! plus one `tapectl-<subcommand>.1` per first-level subcommand. Running it
//! without arguments is idempotent — the only diff between runs should be
//! the embedded version/date, which clap_mangen pulls from `Cargo.toml`.
//!
//! Committing the output keeps the pages visible without forcing users to
//! have `clap_mangen` to read them.

use std::fs;
use std::path::PathBuf;

use clap::CommandFactory;
use clap_mangen::Man;
use tapectl::cli::Cli;

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

    // One page per first-level subcommand. Skip clap's auto-generated
    // `help` subcommand — `man tapectl-help` would just duplicate `-h`.
    for sub in cmd.get_subcommands() {
        let name = sub.get_name();
        if name == "help" {
            continue;
        }
        let page_name = format!("tapectl-{name}.1");
        let page_path = out_dir.join(&page_name);
        let mut buf = Vec::new();
        // `Man::new` with the bare subcommand would give it name=<sub>; pre-
        // qualify so the page header reads "tapectl-snapshot(1)".
        let qualified_name: &'static str = Box::leak(format!("tapectl-{name}").into_boxed_str());
        let qualified = sub.clone().name(qualified_name);
        Man::new(qualified).render(&mut buf)?;
        fs::write(&page_path, &buf)?;
        println!("wrote {}", page_path.display());
    }

    Ok(())
}
