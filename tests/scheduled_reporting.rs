//! Issue #535, ADR-0012 item 250: no notification is built in; the command
//! line is sufficient for a reporting tool written on top. The four facts
//! such a tool needs, each read from a command's JSON and exit code:
//!
//! 1. the audit's verdict: `audit --json` (`exit_code`, `violations`, `warnings`);
//! 2. the newest catalog backup's age and check: the dated file name `db backup
//!    DIR` writes, and `db import FILE --dry-run` (exit 0 = the file passes);
//! 3. the drive's last health reading: `report health --json` (`readings[0]`);
//! 4. what is pending: `status --json` (`units`).
//!
//! These tests are the contract `docs/operator-guide.md` ("What a reporting
//! tool reads") states. Ungated: no device is opened, and the health reading
//! comes from the catalog.

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

fn ok(home: &Path, args: &[&str]) -> Output {
    let out = tapectl(home, args);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn json(out: &Output) -> serde_json::Value {
    let text = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("one JSON document: {e}\n{text}"))
}

fn initialised_home() -> TempDir {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init", "--no-escrow"]);
    home
}

/// Fact 1: the verdict is in the JSON and is the exit code.
#[test]
fn the_audit_verdict_is_in_its_json_and_its_exit_code() {
    let home = initialised_home();
    let out = tapectl(home.path(), &["--json", "audit"]);
    assert_eq!(out.status.code(), Some(0));
    let doc = json(&out);
    assert_eq!(doc["exit_code"], 0);
    assert_eq!(doc["violations"], 0);
    assert_eq!(doc["warnings"], 0);
    assert_eq!(doc["findings"], serde_json::json!([]));

    // Positive control: a unit nothing has archived is a finding, and the
    // document and the exit code move together.
    let source = TempDir::new().unwrap();
    std::fs::write(source.path().join("a.txt"), b"x").unwrap();
    ok(home.path(), &["tenant", "add", "acme"]);
    ok(
        home.path(),
        &[
            "unit",
            "init",
            source.path().to_str().unwrap(),
            "--tenant",
            "acme",
            "--name",
            "unit1",
        ],
    );
    ok(home.path(), &["snapshot", "create", "unit1"]);
    let out = tapectl(home.path(), &["--json", "audit"]);
    let doc = json(&out);
    let code = out.status.code().expect("an exit code");
    assert_ne!(code, 0, "an unarchived unit is a finding: {doc}");
    assert_eq!(doc["exit_code"], code);
    assert!(
        doc["violations"].as_i64().unwrap() + doc["warnings"].as_i64().unwrap() > 0,
        "{doc}"
    );
}

/// Fact 2: a backup's age is its name's UTC stamp, and whether it passed is
/// `db import --dry-run`'s exit code (it checks the file and changes nothing).
#[test]
fn a_backups_age_is_its_name_and_its_check_is_a_dry_run_import() {
    let home = initialised_home();
    let dir = TempDir::new().unwrap();
    let out = ok(
        home.path(),
        &["--json", "db", "backup", dir.path().to_str().unwrap()],
    );
    let file = json(&out)["backup"].as_str().unwrap().to_string();
    let name = Path::new(&file).file_name().unwrap().to_str().unwrap();
    let stamp = name
        .strip_prefix("tapectl-")
        .and_then(|s| s.strip_suffix(".db"))
        .unwrap_or_else(|| panic!("a dated name: {name}"));
    let at = chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M%SZ")
        .unwrap_or_else(|e| panic!("{stamp}: {e}"))
        .and_utc();
    let age = chrono::Utc::now() - at;
    assert!(
        age >= chrono::Duration::seconds(-5) && age < chrono::Duration::minutes(5),
        "the stamp is when it was made: {age}"
    );

    let live_before = std::fs::read(home.path().join(".tapectl/tapectl.db")).unwrap();
    let out = tapectl(home.path(), &["--json", "db", "import", &file, "--dry-run"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(json(&out)["dry_run"], true);
    assert!(
        std::fs::read(home.path().join(".tapectl/tapectl.db")).unwrap() == live_before,
        "the check changes nothing"
    );

    // Positive control: a file that is not a catalog fails the same check.
    let bad = dir.path().join("tapectl-20200101T000000Z.db");
    std::fs::write(&bad, b"not a database").unwrap();
    let out = tapectl(
        home.path(),
        &["--json", "db", "import", bad.to_str().unwrap(), "--dry-run"],
    );
    assert_ne!(out.status.code(), Some(0));
}

/// Fact 3: the newest health reading, with its time and TapeAlert count, is
/// the first of `report health --json`'s readings; read from the catalog,
/// no drive is opened.
#[test]
fn the_last_health_reading_is_the_first_of_report_health() {
    let home = initialised_home();
    let conn = tapectl::db::open(&home.path().join(".tapectl").join("tapectl.db")).unwrap();
    for (at, alerts) in [("2026-10-01 10:00:00", 0), ("2026-10-02 10:00:00", 3)] {
        conn.execute(
            "INSERT INTO health_logs (operation, tape_alerts, logged_at) VALUES ('poll', ?1, ?2)",
            rusqlite::params![alerts, at],
        )
        .unwrap();
    }
    drop(conn);
    let out = tapectl(home.path(), &["--json", "report", "health"]);
    assert_eq!(out.status.code(), Some(0));
    let doc = json(&out);
    let newest = &doc["readings"][0];
    assert_eq!(newest["tape_alerts"], 3, "{doc}");
    assert!(
        newest["at"].as_str().unwrap().starts_with("2026-10-02"),
        "{doc}"
    );
    assert_eq!(newest["operation"], "poll");
    assert!(doc["tape_alerts_raised"].is_array(), "{doc}");
}

/// Fact 4: what is pending is `status --json`'s `units`.
#[test]
fn what_is_pending_is_in_status_json() {
    let home = initialised_home();
    let doc = json(&ok(home.path(), &["--json", "status"]));
    assert_eq!(doc["units"]["pending"], 0, "{doc}");

    let source = TempDir::new().unwrap();
    std::fs::write(source.path().join("a.txt"), b"x").unwrap();
    ok(home.path(), &["tenant", "add", "acme"]);
    ok(
        home.path(),
        &[
            "unit",
            "init",
            source.path().to_str().unwrap(),
            "--tenant",
            "acme",
            "--name",
            "unit1",
        ],
    );
    let doc = json(&ok(home.path(), &["--json", "status"]));
    let units = &doc["units"];
    assert_eq!(units["pending"], 1, "{doc}");
    assert_eq!(
        units["never_archived"],
        serde_json::json!(["unit1"]),
        "{doc}"
    );
    for key in ["dirty", "unstaged", "not_scanned"] {
        assert!(
            units[key].is_array(),
            "{key} is a list of unit names: {doc}"
        );
    }
    assert!(units["unsynced_folders"].is_number(), "{doc}");
    assert!(
        doc["next"].is_array() && doc["due_verify"].is_array(),
        "{doc}"
    );
}
