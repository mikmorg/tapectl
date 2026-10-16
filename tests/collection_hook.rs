//! `collection run`'s snapshot hook through the real binary (ADR-0012
//! 2026-10-16 item 75, issue #455).
//!
//! The staging half of the hook is exercised in `collection::batch`'s unit
//! tests (a real stage from a fake snapshot, without a drive). What only the
//! binary shows is the command line around it: a dry run prints the command
//! and runs nothing, and a snapshot command that fails refuses the real run
//! with a non-zero exit before any unit is read. A drive is never touched:
//! the configured device is a path that is not a tape, and the run is refused
//! before it would be opened.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn run_tapectl(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("failed to spawn tapectl binary")
}

fn ok(home: &Path, args: &[&str]) -> Output {
    let out = run_tapectl(home, args);
    assert!(
        out.status.success(),
        "`tapectl {}` failed ({:?})\nstdout={}\nstderr={}",
        args.join(" "),
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out
}

fn count(home: &Path, sql: &str) -> i64 {
    let conn = tapectl::db::open(&home.join(".tapectl").join("tapectl.db")).expect("open catalog");
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// A home with a two-unit collection whose `snapshot_command` is `command`
/// (TOML-escaped by the caller), one configured drive that is not a tape,
/// and an `initialized` destination volume `L1`.
fn home_with_a_hooked_collection(command: &str) -> (TempDir, TempDir) {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);
    ok(home.path(), &["tenant", "add", "media"]);

    let root = TempDir::new().unwrap();
    for name in ["alpha", "beta"] {
        let dir = root.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.dat"), vec![0u8; 64 * 1024]).unwrap();
    }

    let cfg = home.path().join(".tapectl").join("config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!(
        r#"
[[backends.lto]]
name = "p"
device_tape = "{dev}"
device_sg = "{dev}"
generation = "LTO-6"
capacity_override = "10M"
fill_ceiling = 1.0
enospc_buffer = "0"

[[collections]]
name = "microlib"
root = "{root}"
tenant = "media"
unit_depth = 1
snapshot_command = '''{command}'''
"#,
        dev = home.path().join("not-a-tape").display(),
        root = root.path().display(),
    ));
    std::fs::write(&cfg, &text).unwrap();

    // `config check` validates the keys and runs neither command.
    ok(home.path(), &["config", "check"]);
    ok(home.path(), &["collection", "sync"]);

    let conn = tapectl::db::open(&home.path().join(".tapectl").join("tapectl.db")).unwrap();
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                              capacity_bytes, status)
         VALUES ('L1', 'lto', 'p', 'LTO-6', 10000000, 'initialized')",
        [],
    )
    .unwrap();
    (home, root)
}

const RUN: [&str; 6] = [
    "collection",
    "run",
    "--collection",
    "microlib",
    "--label",
    "L1",
];

/// A dry run says which command it would run and runs it not: the marker
/// the command would have left is absent, and the JSON carries the command.
#[test]
fn a_dry_run_names_the_snapshot_command_and_runs_nothing() {
    let marker = TempDir::new().unwrap();
    let marker_file = marker.path().join("hook-ran");
    let command = format!("touch '{}'; exit 9", marker_file.display());
    let (home, _root) = home_with_a_hooked_collection(&command);

    let mut args = RUN.to_vec();
    args.push("--dry-run");
    let out = ok(home.path(), &args);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("snapshot_command"), "{text}");
    assert!(text.contains(&command), "{text}");
    assert!(!marker_file.exists(), "a dry run ran the snapshot command");

    let mut json_args = vec!["--json"];
    json_args.extend(args);
    let out = ok(home.path(), &json_args);
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(parsed["snapshot_command"], serde_json::json!(command));
    assert!(!marker_file.exists());
    assert_eq!(count(home.path(), "SELECT COUNT(*) FROM snapshots"), 0);
}

/// A real run whose snapshot command fails is refused with a non-zero exit,
/// the error names the command and what it said, and no unit was read: no
/// snapshot, no stage set.
#[test]
fn a_failing_snapshot_command_refuses_the_real_run_before_any_unit_is_read() {
    let marker = TempDir::new().unwrap();
    let marker_file = marker.path().join("hook-ran");
    let command = format!(
        "touch '{}'; echo 'cannot take a snapshot: pool is degraded' >&2; exit 9",
        marker_file.display()
    );
    let (home, _root) = home_with_a_hooked_collection(&command);

    let out = run_tapectl(home.path(), &RUN);
    assert!(!out.status.success(), "the run should have been refused");
    let stderr = String::from_utf8_lossy(&out.stderr);
    for part in [
        "snapshot_command",
        "exit status: 9",
        "cannot take a snapshot: pool is degraded",
        "nothing was snapshotted or staged",
    ] {
        assert!(stderr.contains(part), "{part}: {stderr}");
    }
    assert!(marker_file.exists(), "the snapshot command ran");
    assert_eq!(count(home.path(), "SELECT COUNT(*) FROM snapshots"), 0);
    assert_eq!(count(home.path(), "SELECT COUNT(*) FROM stage_sets"), 0);
}
