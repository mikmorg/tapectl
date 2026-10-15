//! Issue #428: `tapectl drive status` — what is in the drive right now,
//! read-only. Ungated: every configured node is a path that does not exist,
//! so no device is opened; the "other command" is this test holding a
//! lockfile (a real `flock`, as `tests/drive_poll_cli.rs` does).
//!
//! The states a fake tape can show (empty, loaded, write-protected, cleaning
//! requested, a chip the catalog knows or does not) are proven by value in
//! `cli::drive`'s unit tests, where the st driver's answer is injected; this
//! file proves what only the binary can: the JSON contract on stdout, the
//! exit codes, that a held drive is reported and not touched, and that
//! nothing is written to the catalog.

use std::fs::File;
use std::path::Path;
use std::process::{Command, Output};

use nix::fcntl::{Flock, FlockArg};
use tempfile::TempDir;

const TAPE: &str = "/nonexistent/tapectl-drive-status-test/nst7";
const SG: &str = "/nonexistent/tapectl-drive-status-test/sg7";

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

/// stdout must be exactly one JSON value — the contract a script relies on.
fn json_of(out: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not one JSON value ({e}): {stdout:?}"))
}

/// Everything a contact, a poll or a reading would have written.
fn rows_written(home: &Path) -> i64 {
    let conn = rusqlite::Connection::open(home.join(".tapectl/tapectl.db")).unwrap();
    [
        "cartridge_contacts",
        "log_page_journal",
        "mam_journal",
        "mtget_journal",
        "st_stats_journal",
        "health_logs",
        "drives",
    ]
    .iter()
    .map(|t| {
        conn.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or_else(|e| panic!("{t}: {e}"))
    })
    .sum()
}

fn hold(home: &Path, lockfile: &str) -> Flock<File> {
    let locks = home.join(".tapectl/locks");
    std::fs::create_dir_all(&locks).unwrap();
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(locks.join(lockfile))
        .unwrap();
    Flock::lock(file, FlockArg::LockExclusiveNonblock).unwrap()
}

/// A drive that is not there: the report is still ONE JSON object on stdout
/// (with the reason in it), the exit is 2, the reason is on stderr too, and
/// nothing was recorded. `--dry-run` changes nothing (read-only).
#[test]
fn an_absent_drive_prints_one_json_object_exits_2_and_records_nothing() {
    let home = configured_home();
    for extra in [&[][..], &["--dry-run"][..]] {
        let mut args = vec!["drive", "status", "--json"];
        args.extend_from_slice(extra);
        let out = run(home.path(), &args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let v = json_of(&out);
        assert_eq!(v["device"], TAPE);
        assert_eq!(v["backend"], "lto0");
        assert_eq!(v["tape_node"]["probe"], "absent");
        assert!(v["tape_node"]["error"].is_string(), "{v}");
        assert_eq!(v["drive"]["declared_generation"], "LTO-6");
        assert_eq!(v["drive_lock"], "free");
        assert!(v["medium"].is_null(), "no chip was asked: {v}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("drive status"),
            "the reason is on stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(rows_written(home.path()), 0, "a status records nothing");
}

/// A drive a tapectl command holds: exit 0 (it is an answer), the holder is
/// named with its pid, the tape node is not opened — and the same drive,
/// free, is probed. The lock is the only difference.
#[test]
fn a_held_drive_is_reported_with_its_holder_and_exits_0() {
    let home = configured_home();
    let holder = hold(home.path(), "drive-nst7.lock");

    let out = run(home.path(), &["drive", "status", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json_of(&out);
    assert_eq!(v["drive_lock"], "held");
    assert_eq!(v["tape_node"]["probe"], "skipped_drive_held");
    assert!(v["tape_node"]["error"].is_null(), "never even tried: {v}");
    let holders = v["holders"].as_array().unwrap();
    assert_eq!(holders.len(), 1, "{v}");
    assert_eq!(holders[0]["kind"], "drive");
    assert_eq!(
        holders[0]["pid"],
        std::process::id(),
        "/proc/locks names the holder"
    );

    let text = run(home.path(), &["drive", "status"]);
    assert_eq!(text.status.code(), Some(0));
    let text = String::from_utf8_lossy(&text.stdout).into_owned();
    assert!(text.contains("HELD"), "{text}");
    assert!(
        text.contains(&format!("pid {}", std::process::id())),
        "{text}"
    );

    // Positive control: with the lock free the same command probes the node.
    drop(holder);
    let free = run(home.path(), &["drive", "status", "--json"]);
    assert_eq!(free.status.code(), Some(2));
    let v = json_of(&free);
    assert_eq!(v["drive_lock"], "free");
    assert_eq!(v["tape_node"]["probe"], "absent");
    assert_eq!(rows_written(home.path()), 0, "a status records nothing");
}

/// A volume session is named too (its lock is the volume's), with the
/// volume's label.
#[test]
fn a_live_volume_session_is_named_by_its_label() {
    let home = configured_home();
    let conn = rusqlite::Connection::open(home.path().join(".tapectl/tapectl.db")).unwrap();
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES ('L6-0042', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
        [],
    )
    .unwrap();
    let id: i64 = conn.last_insert_rowid();
    drop(conn);
    let _session = hold(home.path(), &format!("volume-{id}.lock"));

    let out = run(home.path(), &["drive", "status", "--json"]);
    let v = json_of(&out);
    let holders = v["holders"].as_array().unwrap();
    assert_eq!(holders.len(), 1, "{v}");
    assert_eq!(holders[0]["kind"], "volume");
    assert_eq!(holders[0]["name"], "L6-0042");
    assert_eq!(
        v["drive_lock"], "free",
        "the volume lock is not the drive's"
    );
}

/// A node no backend claims is looked at (the lenient read path), with a
/// note that no chip could be read.
#[test]
fn a_device_no_backend_claims_is_looked_at_with_a_note() {
    let home = TempDir::new().unwrap();
    ok(&run(home.path(), &["init"]), "init");
    let out = run(
        home.path(),
        &[
            "drive",
            "status",
            "--device",
            "/nonexistent/tapectl-drive-status-test/nst3",
            "--json",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json_of(&out);
    assert!(v["backend"].is_null());
    assert_eq!(v["tape_node"]["probe"], "absent");
    assert!(
        v["notes"].to_string().contains("no backend"),
        "the note says what was not read: {v}"
    );
}

/// No drive configured and none named: an error, not a report — nothing on
/// stdout, and the reason says how to name one.
#[test]
fn no_drive_configured_and_none_named_is_an_error() {
    let home = TempDir::new().unwrap();
    ok(&run(home.path(), &["init"]), "init");
    let out = run(home.path(), &["drive", "status", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--device"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
