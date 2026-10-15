//! Issue #309 (ADR-0012, 2026-10-07 item 29): `tapectl drive poll` takes
//! the drive lock WITHOUT waiting and, when a tapectl command holds the
//! drive, exits 75 having read and written nothing; with the lock free it
//! runs, records its contact, and — with no readable sg node — exits 2
//! rather than a quiet 0.
//!
//! Ungated: the configured nodes are paths that do not exist, so no device
//! is opened; the "other command" is this test holding the lockfile.

use std::fs::File;
use std::path::Path;
use std::process::{Command, Output};

use nix::fcntl::{Flock, FlockArg};
use tempfile::TempDir;

const TAPE: &str = "/nonexistent/tapectl-drive-poll-test/nst7";
const SG: &str = "/nonexistent/tapectl-drive-poll-test/sg7";

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

fn contacts(home: &Path) -> Vec<(String, Option<String>)> {
    let conn = rusqlite::Connection::open(home.join(".tapectl/tapectl.db")).unwrap();
    let mut stmt = conn
        .prepare("SELECT operation, outcome FROM cartridge_contacts ORDER BY id")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn a_held_drive_is_skipped_with_75_and_a_free_one_is_polled() {
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

    // Another tapectl command has the drive: hold its lockfile.
    let locks = home.path().join(".tapectl/locks");
    std::fs::create_dir_all(&locks).unwrap();
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(locks.join("drive-nst7.lock"))
        .unwrap();
    let holder = Flock::lock(file, FlockArg::LockExclusiveNonblock).unwrap();

    let busy = run(home.path(), &["drive", "poll"]);
    assert_eq!(
        busy.status.code(),
        Some(75),
        "held: {}",
        String::from_utf8_lossy(&busy.stderr)
    );
    assert!(
        String::from_utf8_lossy(&busy.stderr).contains("nothing was read"),
        "{}",
        String::from_utf8_lossy(&busy.stderr)
    );
    assert!(
        contacts(home.path()).is_empty(),
        "a skipped poll records no contact"
    );

    // Positive control: the same poll with the lock free runs and records
    // its contact — here with no readable node, so no page was read: an
    // error, never a quiet 0.
    drop(holder);
    let ran = run(home.path(), &["drive", "poll"]);
    assert_eq!(
        ran.status.code(),
        Some(2),
        "{}{}",
        String::from_utf8_lossy(&ran.stdout),
        String::from_utf8_lossy(&ran.stderr)
    );
    assert!(
        String::from_utf8_lossy(&ran.stderr).contains("read no log page"),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert_eq!(
        contacts(home.path()),
        vec![("drive poll".to_string(), Some("failed".to_string()))]
    );
}

/// Issue #431: a drive configured with no `device_sg` whose tape node the
/// kernel cannot bind (here, a path that does not exist) is refused BY NAME
/// when something needs the sg node — not read through an empty path, not
/// guessed. The poll reads and records nothing.
#[test]
fn a_poll_of_a_drive_with_no_derivable_sg_node_refuses_by_name() {
    // A basename no host's /sys has: a derivation must not borrow a real
    // drive's sg node for a tape node that does not exist.
    const NO_SG_TAPE: &str = "/nonexistent/tapectl-drive-poll-test/nst-fake-431";
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
                NO_SG_TAPE,
                "--generation",
                "LTO-6",
            ],
        ),
        "backend add without --device-sg",
    );

    let out = run(home.path(), &["drive", "poll"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("lto0"), "names the backend: {stderr}");
    assert!(
        stderr.contains("device_sg is not set") && stderr.contains("cannot be derived"),
        "says why: {stderr}"
    );
    assert!(
        stderr.contains("Set device_sg"),
        "says how to fix it: {stderr}"
    );
    assert!(
        contacts(home.path()).is_empty(),
        "a refused poll records no contact"
    );
}
