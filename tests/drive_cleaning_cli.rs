//! Issue #430: recording drive cleanings (ADR-0012 2026-10-15 item 60) —
//! `drive cleaning-cartridge add`, `drive cleaned`, and `report health`'s
//! cleaning block. Ungated: no drive exists (every node is a path that does
//! not exist), so what a drive would answer — its serial, the chip of the
//! cartridge in it — is injected in `cli::drive`'s unit tests. This file
//! proves what only the binary can: the registry's defaults and refusals, the
//! JSON contract, that `report health` shows the uses and the last cleaning
//! only once the feature is used (the existing `--json` document is
//! otherwise byte-for-byte what it was), and that a cleaning that cannot be
//! recorded records nothing.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

const TAPE: &str = "/nonexistent/tapectl-drive-cleaning-test/nst7";
const SG: &str = "/nonexistent/tapectl-drive-cleaning-test/sg7";

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("TAPECTL_HOME")
        .output()
        .expect("spawn tapectl")
}

fn ok(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn json_of(out: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not one JSON value ({e}): {stdout:?}"))
}

/// A home with one configured (nonexistent) drive.
fn configured_home() -> TempDir {
    let home = TempDir::new().unwrap();
    ok(&run(home.path(), &["init"]), "init");
    ok(
        &run(
            home.path(),
            &[
                "backend",
                "add",
                "--name",
                "lto0",
                "--device-tape",
                TAPE,
                "--device-sg",
                SG,
                "--generation",
                "LTO-6",
            ],
        ),
        "backend add",
    );
    home
}

fn db(home: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(home.join(".tapectl/tapectl.db")).unwrap()
}

fn count(home: &Path, sql: &str) -> i64 {
    db(home).query_row(sql, [], |r| r.get(0)).unwrap()
}

/// The registry's default is 50 cleanings, the cartridge shows up in
/// `report health` with none used, and the same text appears as data. A
/// serial registers once, a zero rating is refused by the parser, and a
/// dry run registers nothing.
#[test]
fn a_cleaning_cartridge_registers_at_50_and_report_health_shows_it() {
    let home = configured_home();
    let out = run(
        home.path(),
        &["drive", "cleaning-cartridge", "add", "CLNU01", "--json"],
    );
    ok(&out, "add");
    let v = json_of(&out);
    assert_eq!(v["serial"], "CLNU01");
    assert_eq!(
        v["rated_uses"], 50,
        "LTO cleaning cartridges are rated for 50"
    );
    assert_eq!(v["registered"], true);

    let out = run(home.path(), &["report", "health"]);
    ok(&out, "report health");
    let t = text(&out);
    assert!(t.contains("Drive cleanings:"), "{t}");
    assert!(
        t.contains("cleaning cartridge CLNU01: 0 of 50 rated uses recorded, 50 left"),
        "{t}"
    );
    let v = json_of(&run(home.path(), &["--json", "report", "health"]));
    assert_eq!(v["cleanings"]["cleaning_cartridges"][0]["serial"], "CLNU01");
    assert_eq!(v["cleanings"]["cleaning_cartridges"][0]["rated_uses"], 50);
    assert_eq!(v["cleanings"]["cleaning_cartridges"][0]["uses"], 0);

    let dup = run(
        home.path(),
        &["drive", "cleaning-cartridge", "add", "CLNU01"],
    );
    assert!(!dup.status.success());
    assert!(text(&dup).contains("already registered"), "{}", text(&dup));
    let zero = run(
        home.path(),
        &[
            "drive",
            "cleaning-cartridge",
            "add",
            "CLNU02",
            "--rated-uses",
            "0",
        ],
    );
    assert_eq!(zero.status.code(), Some(2), "{}", text(&zero));

    let before = count(home.path(), "SELECT COUNT(*) FROM cleaning_cartridges");
    let dry = run(
        home.path(),
        &[
            "drive",
            "cleaning-cartridge",
            "add",
            "CLNU03",
            "--rated-uses",
            "20",
            "--dry-run",
            "--json",
        ],
    );
    ok(&dry, "dry run");
    let v = json_of(&dry);
    assert_eq!(
        (v["dry_run"].as_bool(), v["registered"].as_bool()),
        (Some(true), Some(false))
    );
    assert_eq!(v["rated_uses"], 20);
    assert_eq!(
        count(home.path(), "SELECT COUNT(*) FROM cleaning_cartridges"),
        before,
        "a dry run registers nothing"
    );
}

/// A catalog that never used the feature prints and returns exactly what it
/// did before: no cleaning block in the text, no `cleanings` key in the JSON
/// (the document `tests/cli_smoke.rs` pins whole).
#[test]
fn report_health_is_unchanged_until_a_cleaning_is_registered_or_recorded() {
    let home = configured_home();
    let t = text(&run(home.path(), &["report", "health"]));
    assert!(!t.contains("cleaning"), "{t}");
    let v = json_of(&run(home.path(), &["--json", "report", "health"]));
    assert!(v.get("cleanings").is_none(), "{v}");
    assert!(
        v.get("feed_ratios").is_some(),
        "positive control: the document is there"
    );
}

/// Cleanings on record: each drive's last cleaning and count, and a
/// cartridge's uses against its rating with the warning when it is near or
/// at the end of its life. Seeded as `drive cleaned` writes them (the drive
/// itself is not available to the test), and read back through the binary.
#[test]
fn report_health_shows_each_drives_last_cleaning_and_each_cartridges_life() {
    let home = configured_home();
    ok(
        &run(
            home.path(),
            &[
                "drive",
                "cleaning-cartridge",
                "add",
                "CLNU01",
                "--rated-uses",
                "4",
            ],
        ),
        "add",
    );
    let conn = db(home.path());
    conn.execute("INSERT INTO drives (serial) VALUES ('HUJ808A5L4')", [])
        .unwrap();
    let drive = conn.last_insert_rowid();
    conn.execute("INSERT INTO drives (serial) VALUES ('NEVERCLEANED')", [])
        .unwrap();
    for (at, cartridge) in [
        ("2026-09-01 08:00:00", Some("CLNU01")),
        ("2026-10-01 09:30:00", Some("CLNU01")),
        ("2026-10-10 07:15:00", Some("CLNU01")),
        ("2026-10-12 07:15:00", None),
    ] {
        conn.execute(
            "INSERT INTO events (timestamp, entity_type, entity_id, entity_label, action, field, new_value)
             VALUES (?1, 'drive', ?2, 'HUJ808A5L4', 'cleaned', 'cleaning_cartridge', ?3)",
            rusqlite::params![at, drive, cartridge],
        )
        .unwrap();
    }
    drop(conn);

    let t = text(&run(home.path(), &["report", "health"]));
    assert!(
        t.contains("drive HUJ808A5L4: last cleaned 2026-10-12 07:15:00 (4 cleaning(s) recorded)"),
        "{t}"
    );
    assert!(
        t.contains("drive NEVERCLEANED: no cleaning recorded"),
        "{t}"
    );
    assert!(
        t.contains("CLNU01: 3 of 4 rated uses recorded, 1 left, last used 2026-10-10 07:15:00"),
        "{t}"
    );
    assert!(t.contains("NEAR ITS RATED LIFE"), "{t}");

    let v = json_of(&run(home.path(), &["--json", "report", "health"]));
    let drives = v["cleanings"]["drives"].as_array().unwrap();
    let cleaned = drives
        .iter()
        .find(|d| d["drive_serial"] == "HUJ808A5L4")
        .unwrap();
    assert_eq!(cleaned["cleanings"], 4);
    assert_eq!(cleaned["last_cleaned"], "2026-10-12 07:15:00");
    let never = drives
        .iter()
        .find(|d| d["drive_serial"] == "NEVERCLEANED")
        .unwrap();
    assert!(never["last_cleaned"].is_null());
    let c = &v["cleanings"]["cleaning_cartridges"][0];
    assert_eq!(
        (c["uses"].as_i64(), c["remaining"].as_i64()),
        (Some(3), Some(1))
    );
    assert_eq!(c["life"], "near");
}

/// A cleaning that cannot be recorded records nothing: an unregistered
/// cartridge is refused with the command that registers it, whatever the
/// drive is doing, and a drive that gives no serial (none exists here) is
/// refused rather than recorded against nothing. `--dry-run` is accepted.
#[test]
fn a_cleaning_that_cannot_be_recorded_records_nothing() {
    let home = configured_home();
    let events = |h: &Path| count(h, "SELECT COUNT(*) FROM events WHERE action = 'cleaned'");
    let rows = |h: &Path| count(h, "SELECT COUNT(*) FROM drives");

    let out = run(home.path(), &["drive", "cleaned", "--cartridge", "CLNX"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("drive cleaning-cartridge add CLNX"),
        "{}",
        text(&out)
    );

    ok(
        &run(
            home.path(),
            &["drive", "cleaning-cartridge", "add", "CLNU01"],
        ),
        "add",
    );
    for extra in [&[][..], &["--dry-run"][..]] {
        let mut args = vec!["drive", "cleaned", "--cartridge", "CLNU01"];
        args.extend_from_slice(extra);
        let out = run(home.path(), &args);
        assert!(!out.status.success(), "{}", text(&out));
        assert!(text(&out).contains("gave no serial"), "{}", text(&out));
    }
    assert_eq!(events(home.path()), 0);
    assert_eq!(
        rows(home.path()),
        0,
        "no drive was registered to record against"
    );

    // `--device` is resolved strictly: a path no backend claims is an error.
    let out = run(
        home.path(),
        &["drive", "cleaned", "--device", "/nonexistent/other/nst9"],
    );
    assert!(!out.status.success());
    assert!(
        text(&out).contains("no [[backends.lto]] entry has device_tape"),
        "{}",
        text(&out)
    );
}
