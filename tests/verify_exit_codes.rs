//! `volume verify`'s exit status, through the real binary (issue #356).
//!
//! CTO ruling, 2026-09-28: 0 = passed; 2 = the verify failed and PROVED THE
//! MEDIUM BAD (the volume is quarantined); 3 = inconclusive — a drive or
//! transport failure, or any error the verify stopped on, with the volume
//! untouched. Before it, every failure and every error exited 2, so a script
//! could not tell "replace the tape" from "clean the drive" without parsing
//! `--json` (and an error prints no JSON at all).
//!
//! The report-driven half — a quarantine exits 2, a failure without medium
//! evidence exits 3 — is unit-tested beside `verify_exit_code` in
//! `src/cli/volume.rs`, on the same `quarantine` field the message and
//! `--json` read. What only the binary can show is the ERROR half: `main`
//! maps every error of a `volume verify` invocation to 3 and leaves every
//! other command's errors at 2. No tape is touched: the drive paths here do
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

const NO_DRIVE: &str = "/nonexistent/tapectl-verify-exit-nst";

/// An unknown label: the verify stopped before reading anything. Not a
/// finding about any medium, so not 2.
#[test]
fn a_verify_that_errors_before_reading_exits_3() {
    let home = initialised_home();
    let out = tapectl(
        home.path(),
        &["volume", "verify", "NOSUCHVOL", "--device", NO_DRIVE],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("volume not found: NOSUCHVOL"),
        "the error itself is printed unchanged: {stderr}"
    );
    assert_eq!(out.status.code(), Some(3), "stderr: {stderr}");

    // Positive control: the same error from a command that is not a verify
    // keeps the ordinary error code, so 3 above is the verify mapping and not
    // a change to every error.
    let out = tapectl(home.path(), &["volume", "info", "NOSUCHVOL"]);
    assert!(!out.status.success());
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A drive that cannot be opened is the transport side of the line: the
/// verify proves nothing about the tape and must leave the volume as it was.
/// Until #356 this exited 2 — the code a script now reads as "the medium is
/// bad".
#[test]
fn a_verify_whose_drive_cannot_be_opened_exits_3_and_touches_nothing() {
    let home = initialised_home();
    let db = home.path().join(".tapectl").join("tapectl.db");
    {
        let conn = tapectl::db::open(&db).unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes,
                                  status)
             VALUES ('L6-EXIT', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
            [],
        )
        .unwrap();
    }

    let out = tapectl(
        home.path(),
        &[
            "volume", "verify", "L6-EXIT", "--device", NO_DRIVE, "--json",
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("tape I/O error"), "stderr: {stderr}");
    assert_eq!(out.status.code(), Some(3), "stderr: {stderr}");

    let conn = tapectl::db::open(&db).unwrap();
    let (status, condition): (String, String) = conn
        .query_row(
            "SELECT status, observed_condition FROM volumes WHERE label = 'L6-EXIT'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (status.as_str(), condition.as_str()),
        ("sealed", "ok"),
        "an inconclusive verify leaves the volume untouched"
    );
}

/// `--dry-run` is refused (issue #241) — no verdict, so 3 as well; still
/// non-zero, which is all `tests/dry_run_contract.rs` asks.
#[test]
fn a_refused_dry_run_verify_exits_3() {
    let home = initialised_home();
    let out = tapectl(
        home.path(),
        &[
            "--dry-run",
            "volume",
            "verify",
            "NOSUCHVOL",
            "--device",
            NO_DRIVE,
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(3),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
