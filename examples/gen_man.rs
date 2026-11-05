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
        // `man tapectl-unit-help` would just duplicate `-h` — and every
        // hidden command: a spelling 1.9.0 removed is a stub that only
        // refuses (ADR-0012 item 203), and has no page.
        if name == "help" || sub.is_hide_set() {
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

        fs::write(&page_path, render_page(qualified, false)?)?;
        println!("wrote {}", page_path.display());

        // Recurse into this subcommand's own children. Walk `sub`, not the
        // renamed clone — renaming is only for the rendered header.
        write_subcommand_pages(sub, &full, out_dir)?;
    }
    Ok(())
}

/// `Man::render`, with a command's `after_long_help` titled for what it is
/// (clap_mangen calls it EXTRA): an `Examples:` block is EXAMPLES, its lines
/// kept as typed, and the top page's `Exit status:` paragraph is EXIT
/// STATUS, followed by the CLI's one exit-code table
/// (`tapectl::error::EXIT_CODE_TABLE`, ADR-0012 item 212).
fn render_page(cmd: clap::Command, top: bool) -> std::io::Result<Vec<u8>> {
    let examples = cmd
        .get_after_long_help()
        .is_some_and(|t| t.to_string().starts_with("Examples:"));
    let mut buf = Vec::new();
    Man::new(cmd).render(&mut buf)?;
    let page = String::from_utf8(buf).expect("clap_mangen writes UTF-8");
    const EXTRA: &str = "\n.SH EXTRA\n";
    let Some(at) = page.find(EXTRA) else {
        return Ok(page.into_bytes());
    };
    let body_start = at + EXTRA.len();
    let end = page[body_start..]
        .find("\n.SH ")
        .map_or(page.len(), |e| body_start + e + 1);
    let body = &page[body_start..end];
    let section = if examples {
        let lines: Vec<&str> = body.lines().filter(|l| l.trim() != "Examples:").collect();
        format!(".SH EXAMPLES\n.nf\n{}\n.fi\n", lines.join("\n"))
    } else if top {
        let mut table = String::new();
        for row in tapectl::error::EXIT_CODE_TABLE {
            table.push_str(&format!(
                ".TP\n\\fB{}\\fR\n{} ({})\n",
                row.code,
                roff_text(row.meaning),
                roff_text(row.commands)
            ));
        }
        format!(".SH \"EXIT STATUS\"\n{body}{table}")
    } else {
        format!(".SH EXTRA\n{body}")
    };
    Ok(format!("{}{section}{}", &page[..at + 1], &page[end..]).into_bytes())
}

/// `text` for a roff text line: backslashes and hyphens escaped, and a
/// leading `.` or `'` kept from reading as a request.
fn roff_text(text: &str) -> String {
    let escaped = text.replace('\\', "\\e").replace('-', "\\-");
    match escaped.chars().next() {
        Some('.' | '\'') => format!("\\&{escaped}"),
        _ => escaped,
    }
}

fn main() -> std::io::Result<()> {
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/man");
    fs::create_dir_all(&out_dir)?;

    // `Cli`'s clap version is the BUILD identity (`build_info::VERSION`:
    // package version, commit hash, build day — ADR-0012, 2026-09-24
    // amendment). A man page documents the release, not one commit, and
    // the CI `man-drift` job regenerates these pages and diffs them —
    // with the hash in the `.TH` header that job would fail on every
    // commit forever. So the pages render with the package version alone.
    let mut cmd = Cli::command().version(tapectl::build_info::PKG_VERSION);
    cmd.build();

    // Top-level page.
    let top = out_dir.join("tapectl.1");
    fs::write(&top, render_page(cmd.clone(), true)?)?;
    println!("wrote {}", top.display());

    write_subcommand_pages(&cmd, "tapectl", &out_dir)?;

    Ok(())
}
