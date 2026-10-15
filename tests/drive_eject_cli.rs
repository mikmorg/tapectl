//! Issue #429: `tapectl drive eject` — unload the cartridge, with the catalog
//! told (ADR-0012 2026-10-15 item 61). Ungated: every configured node is a
//! path that does not exist, so no device is opened; the "other command" is
//! this test holding a lockfile (a real `flock`, as `tests/drive_status_cli.rs`
//! does).
//!
//! What a fake tape can show — the unload itself, the contact and event it
//! writes, a failed unload, an empty or not-ready drive, `--dry-run`'s
//! preview, and that the binding (`cartridge_volumes.unmounted_at`) is never
//! touched — is proven by value in `cli::drive`'s unit tests, where the
//! node, the chip and the unload are injected. This file proves what only the
//! binary can: the refusals and their exit codes, the JSON contract on
//! stdout, that the drive is resolved strictly, and that a refusal records
//! nothing.

use std::fs::File;
use std::path::Path;
use std::process::{Command, Output};

use nix::fcntl::{Flock, FlockArg};
use tempfile::TempDir;

const TAPE: &str = "/nonexistent/tapectl-drive-eject-test/nst7";
const SG: &str = "/nonexistent/tapectl-drive-eject-test/sg7";

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

fn backend_add(home: &Path, name: &str, tape: &str, sg: &str) {
    ok(
        &run(
            home,
            &[
                "backend",
                "add",
                "--name",
                name,
                "--device-tape",
                tape,
                "--device-sg",
                sg,
                "--generation",
                "LTO-6",
            ],
        ),
        "backend add",
    );
}

/// A home with one configured (nonexistent) drive. `init` and `backend add`
/// write their own events, so a test compares [`rows_written`] against the
/// count it took here, never against zero.
fn configured_home() -> TempDir {
    let home = TempDir::new().unwrap();
    ok(&run(home.path(), &["init"]), "init");
    backend_add(home.path(), "lto0", TAPE, SG);
    home
}

/// stdout must be exactly one JSON value — the contract a script relies on.
fn json_of(out: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not one JSON value ({e}): {stdout:?}"))
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Everything an eject, a contact or a reading could have written.
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
        "events",
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

/// A drive that is not there: ONE JSON object on stdout with the reason in
/// it, exit 2, nothing recorded. The control for the refusals below: this is
/// what a free drive does, so a refusal that skips it is visible.
#[test]
fn an_absent_drive_prints_one_json_object_exits_2_and_records_nothing() {
    let home = configured_home();
    let baseline = rows_written(home.path());
    let out = run(home.path(), &["drive", "eject", "--json"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let v = json_of(&out);
    assert_eq!(v["device"], TAPE);
    assert_eq!(v["backend"], "lto0");
    assert_eq!(v["outcome"], "node_unreadable");
    assert_eq!(v["dry_run"], false);
    assert!(
        v["message"].as_str().unwrap().contains("no drive answers"),
        "{v}"
    );
    assert!(v["contact_id"].is_null() && v["event_id"].is_null(), "{v}");
    assert_eq!(rows_written(home.path()), baseline, "nothing was ejected");

    // The text form puts the reason on stderr, where a script looks for it.
    let out = run(home.path(), &["drive", "eject"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stdout).is_empty(),
        "{}",
        text(&out)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no drive answers"),
        "{}",
        text(&out)
    );
}

/// A drive a tapectl command holds: exit 75, the holder named with its pid,
/// the node never read (the same eject, free, reaches the node and finds it
/// absent), nothing recorded.
#[test]
fn a_held_drive_is_refused_with_75_naming_the_holder_and_records_nothing() {
    let home = configured_home();
    let baseline = rows_written(home.path());
    let holder = hold(home.path(), "drive-nst7.lock");

    let out = run(home.path(), &["drive", "eject", "--json"]);
    assert_eq!(out.status.code(), Some(75), "{}", text(&out));
    let v = json_of(&out);
    assert_eq!(v["outcome"], "refused_held");
    let holders = v["holders"].as_array().unwrap();
    assert_eq!(holders.len(), 1, "{v}");
    assert_eq!(holders[0]["kind"], "drive");
    assert_eq!(
        holders[0]["pid"],
        std::process::id(),
        "/proc/locks names the holder"
    );

    let out = run(home.path(), &["drive", "eject"]);
    assert_eq!(out.status.code(), Some(75));
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains("refused")
            && stderr.contains(&format!("pid {}", std::process::id()))
            && stderr.contains("Nothing was sent"),
        "{stderr}"
    );
    assert_eq!(rows_written(home.path()), baseline);

    // `--dry-run` refuses a held drive exactly as the real run does.
    let out = run(home.path(), &["drive", "eject", "--dry-run", "--json"]);
    assert_eq!(out.status.code(), Some(75), "{}", text(&out));
    assert_eq!(json_of(&out)["outcome"], "refused_held");
    assert_eq!(json_of(&out)["dry_run"], true);

    // Positive control: with the lock free the same command reaches the node.
    drop(holder);
    let free = run(home.path(), &["drive", "eject", "--json"]);
    assert_eq!(free.status.code(), Some(2));
    assert_eq!(json_of(&free)["outcome"], "node_unreadable");
    assert_eq!(rows_written(home.path()), baseline);
}

/// A live volume session is named by its label (its lock is the volume's),
/// and refuses even though the drive lock is free.
#[test]
fn a_live_volume_session_is_refused_and_named_by_its_label() {
    let home = configured_home();
    let baseline = rows_written(home.path());
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

    let out = run(home.path(), &["drive", "eject", "--json"]);
    assert_eq!(out.status.code(), Some(75), "{}", text(&out));
    let v = json_of(&out);
    assert_eq!(v["outcome"], "refused_held");
    let holders = v["holders"].as_array().unwrap();
    assert_eq!(holders.len(), 1, "{v}");
    assert_eq!(holders[0]["kind"], "volume");
    assert_eq!(holders[0]["name"], "L6-0042");
    assert_eq!(rows_written(home.path()), baseline);
}

/// The drive is resolved STRICTLY — it moves a cartridge, and the record
/// names it by the chip read through `device_sg`. A path no backend claims
/// is an error naming what is configured (`drive status` would look at it
/// anyway), several drives need `--device`, and nothing is recorded.
#[test]
fn the_drive_is_resolved_strictly() {
    let home = configured_home();
    let out = run(
        home.path(),
        &["drive", "eject", "--device", "/nonexistent/other/nst9"],
    );
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("no [[backends.lto]] entry has device_tape"),
        "{}",
        text(&out)
    );

    backend_add(
        home.path(),
        "lto1",
        "/nonexistent/tapectl-drive-eject-test/nst8",
        "/nonexistent/tapectl-drive-eject-test/sg8",
    );
    let baseline = rows_written(home.path());
    let out = run(home.path(), &["drive", "eject"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("pass --device to select one"),
        "{}",
        text(&out)
    );

    // Positive control: naming one of the two reaches its node.
    let out = run(home.path(), &["drive", "eject", "--device", TAPE, "--json"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert_eq!(json_of(&out)["backend"], "lto0");
    assert_eq!(rows_written(home.path()), baseline);
}
