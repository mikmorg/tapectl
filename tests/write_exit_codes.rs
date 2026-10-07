//! The write family's exit table, through the real binary (ADR-0012, the
//! 2026-10-07 amendment, item 20; issue #408).
//!
//! `volume write`, `volume resume`, `collection run`, `quick-archive`,
//! `volume compact-write` and `volume compact` exit 0 sealed and confirmed;
//! 2 an error with the medium untouched (refused before anything was
//! written, or a usage error); 3 confirm inconclusive; 4 interrupted,
//! resumable; 5 aborted; 6 the medium proven bad; 75 the catalog busy.
//!
//! Codes 3 to 6 need a session that reached the tape: they are pinned in
//! `src/volume/write.rs` (`exit_3_…` to `exit_6_…`, through `volume write`
//! over an injected store) and the mapping in `src/error.rs`. What only the
//! binary shows is that each command's refusals before the tape moved — and
//! its usage errors — exit 2 through `main`, and that the six commands are
//! the ones the table covers. No tape is touched: the drive paths here do
//! not exist.

use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn tapectl(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("TAPECTL_HOME")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("failed to spawn tapectl binary")
}

fn initialised_home() -> TempDir {
    let home = TempDir::new().unwrap();
    let out = tapectl(home.path(), &["init", "--no-escrow"]);
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    home
}

const NO_DRIVE: &str = "/nonexistent/tapectl-write-exit-nst";

fn assert_exit(out: &Output, want: i32, what: &str) {
    assert_eq!(
        out.status.code(),
        Some(want),
        "{what}: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Every command of the family, refused before the tape moved — an unknown
/// label, a `--dry-run` (which each refuses), a collection that does not
/// exist — exits 2: nothing was written, so 2 is the table's answer.
#[test]
fn each_write_command_refused_before_the_tape_moved_exits_2() {
    let home = initialised_home();
    let src = home.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let src = src.to_str().unwrap();
    for args in [
        &["volume", "write", "NOSUCH", "--device", NO_DRIVE][..],
        &["volume", "resume", "NOSUCH", "--device", NO_DRIVE][..],
        &[
            "volume",
            "compact-write",
            "--destination",
            "NOSUCH",
            "--device",
            NO_DRIVE,
        ][..],
        &["--dry-run", "volume", "write", "NOSUCH"][..],
        &["--dry-run", "volume", "resume", "NOSUCH"][..],
        &["--dry-run", "volume", "compact", "NOSUCH", "--to", "OTHER"][..],
        &[
            "collection",
            "run",
            "--collection",
            "nope",
            "--label",
            "NOSUCH",
            "--device",
            NO_DRIVE,
        ][..],
        &[
            "--dry-run",
            "quick-archive",
            src,
            "--tenant",
            "t",
            "--volume",
            "NOSUCH",
        ][..],
    ] {
        let out = tapectl(home.path(), args);
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("error: "),
            "{args:?}: an error is printed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_exit(&out, 2, &format!("{args:?}"));
    }
}

/// A usage error keeps clap's 2, which the table gives it — unlike
/// `volume verify` (3) and `audit` (70), whose 2 is a verdict.
#[test]
fn a_write_command_line_that_does_not_parse_exits_2() {
    let home = TempDir::new().unwrap();
    for args in [
        &["volume", "write"][..],
        &["volume", "resume", "L", "--fill-ceiling", "99%"][..],
        &["volume", "compact-write"][..],
        &["collection", "run", "--bogus"][..],
        &["quick-archive", "/src"][..],
    ] {
        let out = tapectl(home.path(), args);
        assert_exit(&out, 2, &format!("{args:?}"));
    }
}
