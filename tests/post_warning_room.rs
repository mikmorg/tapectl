//! The post-warning room key and the slice cap (issue #479, ADR-0012
//! 2026-10-17 item 100), end to end through the real binary: a drive's
//! `post_warning_bytes` makes `config check` and `config set` refuse a
//! `slice_size` whose slice plus the seal does not fit in half of the smallest
//! room, and an absent key checks nothing. The same refusal at `stage create`
//! is `staging::tests::stage_create_refuses_a_slice_size_the_post_warning_room_cannot_hold`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("TAPECTL_HOME")
        .env_remove("TAPECTL_CONFIG")
        .output()
        .expect("failed to spawn tapectl binary")
}

fn text(out: &Output) -> String {
    format!(
        "exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn ok(home: &Path, args: &[&str]) -> Output {
    let out = run(home, args);
    assert!(
        out.status.success(),
        "`tapectl {}` failed\n{}",
        args.join(" "),
        text(&out)
    );
    out
}

fn config_path(home: &Path) -> PathBuf {
    home.join(".tapectl").join("config.toml")
}

/// A fresh home with one drive, `hp`, and no measured room.
fn home_with_drive() -> TempDir {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init", "--no-escrow"]);
    ok(
        home.path(),
        &[
            "config",
            "add",
            "backends.lto",
            "name=hp",
            "device_tape=/dev/tapectl-post-warning-nonexistent",
            "device_sg=/dev/tapectl-post-warning-nonexistent-sg",
            "generation=LTO-6",
        ],
    );
    home
}

/// `config set` that must be refused: nonzero exit, the file byte-identical,
/// and the output returned for the caller to read.
fn set_refused(home: &Path, key: &str, value: &str) -> String {
    let before = std::fs::read(config_path(home)).unwrap();
    let out = run(home, &["config", "set", key, value]);
    assert!(
        !out.status.success(),
        "`config set {key} {value}` should have been refused\n{}",
        text(&out)
    );
    assert_eq!(
        before,
        std::fs::read(config_path(home)).unwrap(),
        "a refused edit must leave the file byte-identical"
    );
    text(&out)
}

#[test]
fn the_room_key_is_refused_when_the_default_slice_cannot_be_sealed_in_it() {
    let home = home_with_drive();
    let h = home.path();
    // Absent key: any slice size passes, even an absurd one.
    ok(h, &["config", "set", "defaults.slice_size", "10T"]);
    ok(h, &["config", "check"]);
    ok(h, &["config", "set", "defaults.slice_size", "1G"]);

    // 2 GB of room: half is 1,000,000,000 bytes, under one 1 GiB slice.
    let refused = set_refused(h, "backends.lto[hp].post_warning_bytes", "2G");
    for needle in [
        "defaults.slice_size",
        "backends.lto[\"hp\"].post_warning_bytes = 2G (2000000000 bytes)",
        "1000000000 bytes",
        "ADR-0012 item 100",
    ] {
        assert!(refused.contains(needle), "{needle}: {refused}");
    }

    // 40 GB of room: half is 2e10, a 1 GiB slice and the seal fit.
    ok(
        h,
        &[
            "config",
            "set",
            "backends.lto[hp].post_warning_bytes",
            "40G",
        ],
    );
    ok(h, &["config", "check"]);
    assert!(std::fs::read_to_string(config_path(h))
        .unwrap()
        .contains("post_warning_bytes = \"40G\""));

    // A slice size over half of it is refused as an edit.
    let refused = set_refused(h, "defaults.slice_size", "30G");
    assert!(
        refused.contains("defaults.slice_size = 30.0 GiB")
            && refused.contains("post_warning_bytes = 40G"),
        "{refused}"
    );
    // Zero and nonsense are refused by name.
    let refused = set_refused(h, "backends.lto[hp].post_warning_bytes", "0");
    assert!(refused.contains("post_warning_bytes"), "{refused}");
    let refused = set_refused(h, "backends.lto[hp].post_warning_bytes", "plenty");
    assert!(refused.contains("post_warning_bytes"), "{refused}");
}

/// The file-level cap is `config check`'s and `config set`'s, never a load error: a staging setting must
/// not block reading a tape. Written by hand (the editor would have refused it), the file breaks the cap,
/// `config check` reports an `error:` line and exits non-zero, and every other command loads it and runs.
#[test]
fn config_check_fails_on_a_file_that_breaks_the_cap_but_every_other_command_still_runs() {
    let home = home_with_drive();
    let h = home.path();
    let cfg = config_path(h);
    let original = std::fs::read_to_string(&cfg).unwrap();
    let broken = original.replacen(
        "generation = \"LTO-6\"",
        "generation = \"LTO-6\"\npost_warning_bytes = \"2G\"",
        1,
    );
    assert_ne!(
        broken, original,
        "precondition: the drive's table is in the file"
    );
    std::fs::write(&cfg, broken).unwrap();

    // config check: the file loads (the `config: valid` line), and the cap is an error with a non-zero exit.
    let out = run(h, &["config", "check"]);
    assert!(!out.status.success(), "{}", text(&out));
    let all = text(&out);
    assert!(all.contains("config: valid"), "the file loads: {all}");
    assert!(!all.contains("config: INVALID"), "{all}");
    assert!(
        all.contains("error: ")
            && all.contains("defaults.slice_size")
            && all.contains("post_warning_bytes = 2G"),
        "{all}"
    );
    let json = run(h, &["--json", "config", "check"]);
    assert!(!json.status.success(), "{}", text(&json));
    let v: serde_json::Value = serde_json::from_slice(&json.stdout).expect("--json is JSON");
    assert_eq!(v["valid"], true, "{v}");
    assert_eq!(
        v["slice_cap_problems"].as_array().map(Vec::len),
        Some(1),
        "{v}"
    );

    // Every other command loads the file and runs as before: the ones that read a catalog, and the restore
    // path that never cuts a slice.
    ok(h, &["unit", "list"]);
    ok(h, &["volume", "list"]);
    let missing = run(
        h,
        &[
            "restore",
            "unit",
            "--unit",
            "no-such-unit",
            "--from",
            "L6-0001",
            "--to",
            "/tmp/tapectl-post-warning-restore",
            "--dry-run",
        ],
    );
    let missing_text = text(&missing);
    assert!(
        !missing.status.success() && !missing_text.contains("post_warning_bytes"),
        "restore unit loaded the config and failed for its own reason (no such unit), not the cap: {missing_text}"
    );
    assert!(
        missing_text.contains("no-such-unit") || missing_text.contains("L6-0001"),
        "{missing_text}"
    );

    // An edit that leaves the cap problem as it was goes through, with a warning; one that adds a problem
    // is refused; the repair is accepted and `config check` passes.
    let unrelated = run(h, &["config", "set", "defaults.min_copies", "3"]);
    assert!(unrelated.status.success(), "{}", text(&unrelated));
    assert!(
        String::from_utf8_lossy(&unrelated.stderr).contains("still breaks the slice cap"),
        "{}",
        text(&unrelated)
    );
    let worse = set_refused(h, "backends.lto[hp].post_warning_bytes", "1G");
    assert!(worse.contains("post_warning_bytes = 1G"), "{worse}");
    ok(h, &["config", "set", "defaults.slice_size", "256M"]);
    ok(h, &["config", "check"]);
}
