//! `config set` / `config add` / `config remove` (issue #143, ADR-0012
//! amendment 2026-10-07 item 27), end to end through the real binary.
//!
//! The ruling's four promises, each asserted on the FILE, never on what was
//! printed: the edit keeps the file's comments and layout; an unknown key is
//! refused by name, as every reader refuses it; the result is validated as
//! `config check` would before anything is written; and a refused edit
//! leaves the file byte-identical.

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

/// Runs `args`, asserts it failed, and asserts the config file's bytes did
/// not move by one.
fn refused(home: &Path, args: &[&str]) -> String {
    let cfg = config_path(home);
    let before = std::fs::read(&cfg).unwrap();
    let out = run(home, args);
    let after = std::fs::read(&cfg).unwrap();
    assert!(
        !out.status.success(),
        "`tapectl {}` should have been refused\n{}",
        args.join(" "),
        text(&out)
    );
    assert!(
        before == after,
        "a refused `tapectl {}` changed the file\n--- before\n{}\n--- after\n{}",
        args.join(" "),
        String::from_utf8_lossy(&before),
        String::from_utf8_lossy(&after)
    );
    text(&out)
}

fn config_path(home: &Path) -> PathBuf {
    home.join(".tapectl").join("config.toml")
}

fn read(home: &Path) -> String {
    std::fs::read_to_string(config_path(home)).unwrap()
}

/// A fresh home with an operator's comment above `[defaults]`.
fn home() -> TempDir {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init", "--no-escrow"]);
    let cfg = config_path(home.path());
    let original = std::fs::read_to_string(&cfg).unwrap();
    let marked = original.replacen(
        "[defaults]",
        "# operator note: slices sized for the basement drive\n[defaults]",
        1,
    );
    assert_ne!(marked, original, "precondition: init writes [defaults]");
    std::fs::write(&cfg, marked).unwrap();
    home
}

fn config_check_passes(home: &Path) {
    ok(home, &["config", "check"]);
}

#[test]
fn set_changes_one_value_and_keeps_every_comment() {
    let home = home();
    let before = read(home.path());
    ok(home.path(), &["config", "set", "defaults.slice_size", "2G"]);
    let after = read(home.path());

    assert!(after.contains("slice_size = \"2G\""), "{after}");
    assert!(after.contains("# operator note: slices sized for the basement drive"));
    // init's commented examples survive too.
    assert!(after.contains("# [[backends.lto]]"), "{after}");
    // Exactly one line differs.
    let changed: Vec<_> = before
        .lines()
        .zip(after.lines())
        .filter(|(a, b)| a != b)
        .collect();
    assert_eq!(before.lines().count(), after.lines().count());
    assert_eq!(changed.len(), 1, "{changed:?}");
    config_check_passes(home.path());
}

#[test]
fn set_types_a_number_and_keeps_a_size_as_a_string() {
    let home = home();
    ok(home.path(), &["config", "set", "staging.jobs", "3"]);
    let after = read(home.path());
    assert!(after.contains("jobs = 3\n"), "{after}");
    config_check_passes(home.path());
}

#[test]
fn set_of_an_unknown_key_is_refused_by_name_and_the_file_is_untouched() {
    let home = home();
    let out = refused(
        home.path(),
        &["config", "set", "defaults.slice_sizee", "2G"],
    );
    assert!(out.contains("slice_sizee"), "{out}");
    let out = refused(home.path(), &["config", "set", "nonsense.key", "1"]);
    assert!(out.contains("nonsense"), "{out}");
}

#[test]
fn set_of_a_renamed_key_gets_the_rename_readers_give() {
    let home = home();
    let out = refused(
        home.path(),
        &["config", "set", "defaults.min_copies_for_tape_only", "3"],
    );
    assert!(out.contains("min_copies"), "{out}");
}

#[test]
fn set_of_a_bad_value_is_refused_and_the_file_is_untouched() {
    let home = home();
    let out = refused(
        home.path(),
        &["config", "set", "defaults.compression", "zipzap"],
    );
    assert!(out.contains("zipzap"), "{out}");
    refused(
        home.path(),
        &["config", "set", "defaults.min_copies", "lots"],
    );
    refused(
        home.path(),
        &["config", "set", "defaults.slice_size", "huge"],
    );
}

#[test]
fn set_of_a_table_is_refused() {
    let home = home();
    refused(home.path(), &["config", "set", "defaults", "1"]);
}

#[test]
fn set_creates_a_missing_table_as_a_real_table() {
    let home = home();
    ok(
        home.path(),
        &["config", "set", "host_check.max_load_per_cpu", "2.5"],
    );
    let after = read(home.path());
    assert!(
        after.contains("\n[host_check]\nmax_load_per_cpu = 2.5\n"),
        "{after}"
    );
    config_check_passes(home.path());
}

#[test]
fn dry_run_writes_nothing() {
    let home = home();
    let before = std::fs::read(config_path(home.path())).unwrap();
    let out = ok(
        home.path(),
        &["config", "set", "defaults.slice_size", "2G", "--dry-run"],
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("DRY RUN"));
    assert_eq!(before, std::fs::read(config_path(home.path())).unwrap());
    // ... and still refuses what the real run refuses.
    refused(
        home.path(),
        &["config", "set", "defaults.nope", "2G", "--dry-run"],
    );
    for args in [
        &[
            "config",
            "add",
            "defaults.global_excludes",
            "*.bak",
            "--dry-run",
        ][..],
        &["config", "remove", "defaults.slice_size", "--dry-run"][..],
        &[
            "config",
            "add",
            "collections",
            "name=m",
            "root=/m",
            "tenant=t",
            "--dry-run",
        ][..],
    ] {
        ok(home.path(), args);
        assert_eq!(
            before,
            std::fs::read(config_path(home.path())).unwrap(),
            "`tapectl {}` wrote",
            args.join(" ")
        );
    }
}

#[test]
fn a_collection_is_added_edited_and_removed_by_name() {
    let home = home();
    ok(
        home.path(),
        &[
            "config",
            "add",
            "collections",
            "name=movies",
            "root=/srv/media/movies",
            "tenant=family",
        ],
    );
    let after = read(home.path());
    assert!(after.contains("[[collections]]\nname = \"movies\"\nroot = \"/srv/media/movies\"\ntenant = \"family\"\n"), "{after}");
    assert!(after.contains("# operator note"), "{after}");
    config_check_passes(home.path());

    ok(
        home.path(),
        &["config", "set", "collections[movies].unit_depth", "2"],
    );
    assert!(read(home.path()).contains("unit_depth = 2\n"));

    // A second collection with the same name could not be addressed.
    refused(
        home.path(),
        &[
            "config",
            "add",
            "collections",
            "name=movies",
            "root=/x",
            "tenant=t",
        ],
    );
    // An unknown field in a new table is refused by name.
    let out = refused(
        home.path(),
        &[
            "config",
            "add",
            "collections",
            "name=tv",
            "root=/x",
            "tenant=t",
            "depth=2",
        ],
    );
    assert!(out.contains("depth"), "{out}");
    // A required field cannot be removed.
    let out = refused(
        home.path(),
        &["config", "remove", "collections[movies].root"],
    );
    assert!(out.contains("root"), "{out}");
    // A name that looks like a number stays a string.
    ok(
        home.path(),
        &[
            "config",
            "add",
            "collections",
            "name=2024",
            "root=/srv/2024",
            "tenant=family",
        ],
    );
    assert!(read(home.path()).contains("name = \"2024\""));

    ok(home.path(), &["config", "remove", "collections[movies]"]);
    ok(home.path(), &["config", "remove", "collections[0]"]);
    let after = read(home.path());
    assert!(!after.contains("[[collections]]"), "{after}");
    assert!(after.contains("# operator note"), "{after}");
    config_check_passes(home.path());
}

#[test]
fn a_list_value_takes_and_gives_up_elements() {
    let home = home();
    ok(
        home.path(),
        &["config", "add", "defaults.global_excludes", "*.bak"],
    );
    let after = read(home.path());
    assert!(after.contains("\"*.bak\""), "{after}");
    config_check_passes(home.path());
    ok(
        home.path(),
        &["config", "remove", "defaults.global_excludes", "*.bak"],
    );
    assert!(!read(home.path()).contains("*.bak"));
    // Removing what is not there is refused.
    refused(
        home.path(),
        &["config", "remove", "defaults.global_excludes", "*.bak"],
    );
}

#[test]
fn removing_a_key_that_is_not_in_the_file_is_refused() {
    let home = home();
    refused(home.path(), &["config", "remove", "defaults.no_such_key"]);
    refused(home.path(), &["config", "remove", "collections[nope]"]);
}

#[test]
fn a_backend_added_by_config_add_is_what_backend_add_writes() {
    let home = home();
    ok(
        home.path(),
        &[
            "config",
            "add",
            "backends.lto",
            "name=drive-a",
            "device_tape=/dev/tapectl-config-edit-nonexistent-a",
            "device_sg=/dev/tapectl-config-edit-nonexistent-sg-a",
            "generation=lto6",
        ],
    );
    let after = read(home.path());
    // The canonical spelling, as `backend add` writes it.
    assert!(after.contains("generation = \"LTO-6\""), "{after}");
    config_check_passes(home.path());

    // Same name, and same drive under a new name: both refused, as
    // `backend add` refuses them.
    refused(
        home.path(),
        &[
            "config",
            "add",
            "backends.lto",
            "name=drive-a",
            "device_tape=/dev/tapectl-config-edit-nonexistent-b",
            "device_sg=/dev/sg9",
            "generation=LTO-6",
        ],
    );
    refused(
        home.path(),
        &[
            "backend",
            "add",
            "--name",
            "drive-b",
            "--device-tape",
            "/dev/tapectl-config-edit-nonexistent-a",
            "--device-sg",
            "/dev/sg9",
            "--generation",
            "LTO-6",
        ],
    );
    refused(
        home.path(),
        &[
            "config",
            "add",
            "backends.lto",
            "name=bad name",
            "device_tape=/dev/tapectl-config-edit-nonexistent-c",
            "device_sg=/dev/sg9",
            "generation=LTO-6",
        ],
    );
    ok(home.path(), &["config", "remove", "backends.lto[drive-a]"]);
    config_check_passes(home.path());
}

#[test]
fn backend_add_still_writes_a_table_that_loads() {
    let home = home();
    ok(
        home.path(),
        &[
            "backend",
            "add",
            "--name",
            "drive-a",
            "--device-tape",
            "/dev/tapectl-config-edit-nonexistent-a",
            "--device-sg",
            "/dev/tapectl-config-edit-nonexistent-sg",
            "--generation",
            "LTO-6",
        ],
    );
    let after = read(home.path());
    assert!(
        after.contains("[[backends.lto]]\nname = \"drive-a\""),
        "{after}"
    );
    assert!(after.contains("# operator note"), "{after}");
    config_check_passes(home.path());
    ok(
        home.path(),
        &[
            "config",
            "set",
            "backends.lto[drive-a].enospc_buffer",
            "64M",
        ],
    );
    assert!(read(home.path()).contains("enospc_buffer = \"64M\""));
}

/// The stub an older `init` wrote (`lto = []` under `[backends]`) is the
/// shape that made `backend add` fail once; a table added over it must load.
#[test]
fn an_empty_inline_list_becomes_the_first_table() {
    let home = home();
    let cfg = config_path(home.path());
    let original = std::fs::read_to_string(&cfg).unwrap();
    let stubbed = original.replacen("[backends]\n", "[backends]\nlto = []\n", 1);
    assert_ne!(stubbed, original, "precondition: init writes [backends]");
    std::fs::write(&cfg, stubbed).unwrap();
    config_check_passes(home.path());
    ok(
        home.path(),
        &[
            "backend",
            "add",
            "--name",
            "drive-a",
            "--device-tape",
            "/dev/tapectl-config-edit-nonexistent-a",
            "--device-sg",
            "/dev/tapectl-config-edit-nonexistent-sg",
            "--generation",
            "LTO-6",
        ],
    );
    let after = read(home.path());
    assert!(!after.contains("lto = []"), "{after}");
    config_check_passes(home.path());
}

/// A file that already fails to load can be repaired one key at a time: an
/// edit that adds no problem goes through, and one that adds a problem is
/// refused even there.
#[test]
fn a_broken_config_is_repaired_one_key_at_a_time() {
    let home = home();
    let cfg = config_path(home.path());
    let original = std::fs::read_to_string(&cfg).unwrap();
    let broken = original.replacen("[defaults]\n", "[defaults]\nbogus1 = 1\nbogus2 = 2\n", 1);
    std::fs::write(&cfg, &broken).unwrap();
    assert!(!run(home.path(), &["config", "check"]).status.success());

    // Adds a third problem: refused.
    refused(home.path(), &["config", "set", "defaults.bogus3", "1"]);
    refused(
        home.path(),
        &["config", "set", "defaults.compression", "zipzap"],
    );

    let out = ok(home.path(), &["config", "remove", "defaults.bogus1"]);
    let said = text(&out);
    assert!(
        said.contains("bogus2"),
        "the remaining problem is named: {said}"
    );
    ok(home.path(), &["config", "remove", "defaults.bogus2"]);
    assert_eq!(read(home.path()), original);
    config_check_passes(home.path());
}

/// Atomic replace writes through a symlinked config to its target rather
/// than replacing the link (the mode is kept too: `config_edit`'s own
/// `replace_file` test, since every command tightens the home's modes
/// before it runs).
#[test]
fn the_replacement_writes_through_a_symlink() {
    let home = home();
    let cfg = config_path(home.path());
    let real = home.path().join("real-config.toml");
    std::fs::rename(&cfg, &real).unwrap();
    std::os::unix::fs::symlink(&real, &cfg).unwrap();

    ok(home.path(), &["config", "set", "defaults.slice_size", "2G"]);

    assert!(std::fs::symlink_metadata(&cfg)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(std::fs::read_to_string(&real)
        .unwrap()
        .contains("slice_size = \"2G\""));
}

#[test]
fn json_output_names_the_change() {
    let home = home();
    let out = ok(
        home.path(),
        &["--json", "config", "set", "defaults.slice_size", "2G"],
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON object");
    assert_eq!(v["action"], "set");
    assert_eq!(v["key"], "defaults.slice_size");
    assert_eq!(v["value"], "\"2G\"");
    assert_eq!(v["previous"], "\"1G\"");
}
