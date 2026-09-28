//! Regenerate the committed Markdown command reference under `docs/cli/`.
//!
//! Run with:
//!
//!     cargo run --example gen_cli_md
//!
//! GitHub renders the troff pages in `docs/man/` as raw text, so a reader on
//! GitHub gets this instead: one page per top-level command, every subcommand
//! and flag, from the same clap definitions the binary parses. Regenerate
//! after any CLI change, alongside `gen_man`; `tests/cli_md_fresh.rs` fails
//! while the committed pages differ from what this would write.

#[path = "cli_md/render.rs"]
mod render;

use std::fs;
use std::path::PathBuf;

use clap::CommandFactory;
use tapectl::cli::Cli;

fn main() -> std::io::Result<()> {
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/cli");
    fs::create_dir_all(&out_dir)?;
    // The package version, not the build identity: a commit hash here would
    // make every commit "change" the reference (same reason as gen_man).
    let mut cmd = Cli::command().version(tapectl::build_info::PKG_VERSION);
    cmd.build();
    for (name, body) in render::render_all(&cmd) {
        let path = out_dir.join(&name);
        fs::write(&path, body)?;
        println!("wrote {}", path.display());
    }
    Ok(())
}
