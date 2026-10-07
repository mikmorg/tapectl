//! `audit`'s exit status, through the real binary (ADR-0012, the 2026-10-07
//! amendment, item 19; issue #408).
//!
//! The ruling: 0 = clean, 1 = warnings only, 2 = at least one violation, and
//! **2 only ever means violations**. An audit that stops on an error reached
//! no verdict, so it exits 70 (sysexits' `EX_SOFTWARE`) — as #356 did for
//! `volume verify`'s 3. Before it, every error exited 2, and
//! `contrib/systemd/tapectl-scheduled-audit.sh` paged "VIOLATIONS" for an
//! audit that never ran. A busy catalog keeps its own 75 (issue #377): the
//! wrapper logs it without a fail ping.
//!
//! The verdict half (0/1/2 from the findings) is pinned by the audit's own
//! tests; what only the binary shows is that `main` maps every ERROR of an
//! `audit` invocation, a command line that does not parse included, to 70
//! and leaves every other command's errors at 2.

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

/// An audit of a unit that does not exist stops on an error: no verdict, so
/// 70 — never the 2 a scheduled wrapper reads as "violations".
#[test]
fn an_audit_that_errors_exits_70_not_2() {
    let home = initialised_home();
    let out = tapectl(home.path(), &["audit", "--unit", "no-such-unit"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unit not found: no-such-unit"),
        "the error itself is printed unchanged: {stderr}"
    );
    assert_eq!(out.status.code(), Some(70), "stderr: {stderr}");

    // Positive control: the same error from a command that is not `audit`
    // keeps the ordinary error code, so 70 above is the audit's mapping and
    // not a change to every error.
    let out = tapectl(home.path(), &["unit", "status", "no-such-unit"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// An error raised before the audit itself runs — here, a home that was
/// never initialised — is still an error of the `audit` invocation.
#[test]
fn an_audit_of_an_uninitialised_home_exits_70() {
    let home = TempDir::new().unwrap();
    let out = tapectl(home.path(), &["audit"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not initialized"), "stderr: {stderr}");
    assert_eq!(out.status.code(), Some(70), "stderr: {stderr}");
}

/// A clean audit still exits 0: the mapping is for errors only.
#[test]
fn a_clean_audit_still_exits_0() {
    let home = initialised_home();
    let out = tapectl(home.path(), &["audit"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// An `audit` command line that does not parse reached no verdict either,
/// so clap's own usage code (2) must not leak through. `--help` stays 0,
/// and a usage error on another command keeps clap's 2 (the positive
/// control: a blanket "usage errors exit 70" would pass the first loop).
#[test]
fn an_audit_command_line_that_does_not_parse_exits_70() {
    let home = TempDir::new().unwrap();
    for (args, says) in [
        (&["audit", "--bogus"][..], "unexpected argument"),
        (&["audit", "--unit"][..], "a value is required"),
        (&["audit", "extra"][..], "unexpected argument"),
        (&["--json", "audit", "--bogus"][..], "unexpected argument"),
    ] {
        let out = tapectl(home.path(), args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(says),
            "{args:?}: clap's own message is printed unchanged: {stderr}"
        );
        assert_eq!(out.status.code(), Some(70), "{args:?}: {stderr}");
    }

    let out = tapectl(home.path(), &["audit", "--help"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("Exit status"));

    for args in [&["report", "--bogus"][..], &["audt"][..]] {
        let out = tapectl(home.path(), args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
