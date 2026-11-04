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
            "archive_sets",
            "name=m",
            "min_copies=2",
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

/// A collection's table, as `collection add` writes it, appended by hand
/// so the tests of `config`'s per-key edits need no real root.
fn with_collection(home: &Path, name: &str, root: &str) {
    let cfg = config_path(home);
    let original = read(home);
    std::fs::write(
        &cfg,
        format!("{original}\n[[collections]]\nname = \"{name}\"\nroot = \"{root}\"\ntenant = \"family\"\n"),
    )
    .unwrap();
}

/// ADR-0012 item 206: a whole collection is added by `collection add` and
/// removed by `collection remove`; `config add collections` and `config
/// remove 'collections[NAME]'` are refused by name (exit 2), pointing at
/// them, and the file does not move. Every key of one collection is still
/// `config`'s to edit.
#[test]
fn a_whole_collection_is_refused_by_name_and_its_keys_are_config_s() {
    let home = home();
    let out = refused(
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
    assert!(out.contains("exit=Some(2)"), "{out}");
    assert!(
        out.contains("use `tapectl collection add NAME ROOT --tenant TENANT`"),
        "{out}"
    );

    with_collection(home.path(), "movies", "/srv/media/movies");
    config_check_passes(home.path());
    for key in ["collections[movies]", "collections[0]"] {
        let out = refused(home.path(), &["config", "remove", key]);
        assert!(out.contains("exit=Some(2)"), "{out}");
        assert!(
            out.contains("use `tapectl collection remove NAME`"),
            "{out}"
        );
    }

    ok(
        home.path(),
        &["config", "set", "collections[movies].unit_depth", "2"],
    );
    assert!(read(home.path()).contains("unit_depth = 2\n"));
    ok(
        home.path(),
        &["config", "add", "collections[movies].exclude", "loose.txt"],
    );
    assert!(read(home.path()).contains("exclude = [\"loose.txt\"]"));
    ok(
        home.path(),
        &[
            "config",
            "remove",
            "collections[movies].exclude",
            "loose.txt",
        ],
    );
    // An unknown key in a collection is refused by name.
    let out = refused(
        home.path(),
        &["config", "set", "collections[movies].depth", "2"],
    );
    assert!(out.contains("depth"), "{out}");
    // A required field cannot be removed.
    let out = refused(
        home.path(),
        &["config", "remove", "collections[movies].root"],
    );
    assert!(out.contains("root"), "{out}");
    let after = read(home.path());
    assert!(after.contains("# operator note"), "{after}");
    config_check_passes(home.path());
}

/// `collection add` writes the table with the comments kept and its keys
/// typed (a name as a string, a depth as a number), and `collection remove`
/// takes it out again, the comments still kept.
#[test]
fn collection_add_and_remove_edit_the_file_in_place() {
    let home = home();
    ok(home.path(), &["tenant", "add", "family"]);
    let root = TempDir::new().unwrap();
    let root_str = root.path().canonicalize().unwrap().display().to_string();
    ok(
        home.path(),
        &[
            "collection",
            "add",
            "photos-2024",
            &root_str,
            "--tenant",
            "family",
            "--unit-depth",
            "2",
            "--exclude",
            "*.partial",
        ],
    );
    let after = read(home.path());
    assert!(
        after.contains(&format!(
            "[[collections]]\nname = \"photos-2024\"\nroot = \"{root_str}\"\ntenant = \"family\"\n\
             unit_depth = 2\nexclude = [\"*.partial\"]\n"
        )),
        "{after}"
    );
    assert!(after.contains("# operator note"), "{after}");
    config_check_passes(home.path());

    ok(home.path(), &["collection", "remove", "photos-2024"]);
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
    refused(home.path(), &["config", "remove", "collections[nope].root"]);
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
    // The canonical spelling, as `drive add` writes it.
    assert!(after.contains("generation = \"LTO-6\""), "{after}");
    config_check_passes(home.path());

    // Same name, and same drive under a new name: both refused, as
    // `drive add` refuses them.
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
            "drive",
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
            "drive",
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

/// Issue #431: `device_sg` is optional. A drive added with only its tape
/// node (by `drive add` or `config add`) is written without the key, the
/// file loads, and `config check` — which stays advisory, exit 0 — names
/// what it could not derive rather than printing an empty node. The tape
/// node is a path that cannot exist here, so nothing real is derived.
#[test]
fn a_backend_without_device_sg_loads_and_config_check_names_what_it_lacks() {
    let home = home();
    ok(
        home.path(),
        &[
            "drive",
            "add",
            "--name",
            "drive-a",
            "--device-tape",
            "/dev/tapectl-config-edit-nonexistent-a",
            "--generation",
            "LTO-6",
        ],
    );
    ok(
        home.path(),
        &[
            "config",
            "add",
            "backends.lto",
            "name=drive-b",
            "device_tape=/dev/tapectl-config-edit-nonexistent-b",
            "generation=LTO-6",
        ],
    );
    let after = read(home.path());
    assert!(
        !after.lines().any(|l| l.starts_with("device_sg")),
        "no device_sg line is written when none is given:\n{after}"
    );

    let human = String::from_utf8_lossy(&ok(home.path(), &["config", "check"]).stdout).to_string();
    assert!(human.contains("config: valid"), "{human}");
    for name in ["drive-a", "drive-b"] {
        assert!(
            human.contains(&format!("drive \"{name}\": device_sg is not set")),
            "{name}: {human}"
        );
    }
    assert!(human.contains("cannot be derived"), "{human}");

    let json: serde_json::Value =
        serde_json::from_slice(&ok(home.path(), &["config", "check", "--json"]).stdout).unwrap();
    let devices = json["tape_devices"].as_array().unwrap();
    assert_eq!(devices.len(), 2, "{json}");
    for d in devices {
        assert_eq!(d["device_sg"], "", "{d}");
        assert_eq!(d["device_sg_derived"], false, "{d}");
        assert!(
            d["device_sg_pairing_problem"]
                .as_str()
                .is_some_and(|p| p.contains("device_sg is not set")),
            "{d}"
        );
    }

    // An explicit empty value is not a way to spell "absent".
    refused(
        home.path(),
        &["config", "set", "backends.lto[drive-a].device_sg", ""],
    );
}

/// The stub an older `init` wrote (`lto = []` under `[backends]`) is the
/// shape that made `drive add` fail once; a table added over it must load.
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
            "drive",
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

/// The review's reproduction (ADR-0012 2026-10-07 item 27: comments and
/// layout are kept): with init's `[host_check]` uncommented, the comments
/// above the header — init's commented drive example among them — were the
/// table's prefix, and removing the table deleted them.
#[test]
fn removing_a_table_keeps_init_s_drive_example() {
    let home = home();
    let cfg = config_path(home.path());
    let original = read(home.path());
    let uncommented = original.replacen("# [host_check]\n", "[host_check]\n", 1);
    assert_ne!(
        uncommented, original,
        "precondition: init writes # [host_check]"
    );
    std::fs::write(&cfg, &uncommented).unwrap();
    config_check_passes(home.path());

    ok(home.path(), &["config", "remove", "host_check"]);
    let after = read(home.path());
    assert!(after.contains("Tape drive. Uncomment and edit"), "{after}");
    assert!(after.contains("# [[backends.lto]]"), "{after}");
    assert!(after.contains("# operator note"), "{after}");
    assert_eq!(after, uncommented.replacen("[host_check]\n", "", 1));
    config_check_passes(home.path());
}

/// The same for the last entry of a list of tables written at the end of
/// the file, below init's commented examples, as `collection remove` takes
/// it out (ADR-0012 item 206).
#[test]
fn removing_the_last_collection_keeps_the_comments_above_it() {
    let home = home();
    let cfg = config_path(home.path());
    let original = read(home.path());
    let entry = "\n# the media shelf\n[[collections]]\nname = \"movies\"\n\
                 root = \"/srv/media/movies\"\ntenant = \"family\"\n";
    std::fs::write(&cfg, format!("{original}{entry}")).unwrap();
    config_check_passes(home.path());

    ok(home.path(), &["collection", "remove", "movies"]);
    let after = read(home.path());
    assert!(after.contains("Tape drive. Uncomment and edit"), "{after}");
    assert!(after.contains("# [host_check]"), "{after}");
    assert!(after.contains("# the media shelf"), "{after}");
    assert!(!after.contains("[[collections]]"), "{after}");
    config_check_passes(home.path());
}

/// An unknown key is refused by name even on a file that already has it:
/// the broken-file allowance is for repairing a file, not for writing more
/// under a key no reader accepts.
#[test]
fn an_edit_under_an_unknown_key_already_in_the_file_is_refused() {
    let home = home();
    let cfg = config_path(home.path());
    let original = read(home.path());
    std::fs::write(&cfg, format!("{original}\n[typo]\na = 1\n")).unwrap();
    let out = refused(home.path(), &["config", "set", "typo.x", "1"]);
    assert!(out.contains("typo"), "{out}");

    let broken = original.replacen("[defaults]\n", "[defaults]\ntypo = 1\n", 1);
    assert_ne!(broken, original, "precondition");
    std::fs::write(&cfg, format!("{broken}\n[typo]\na = 1\n")).unwrap();
    let out = refused(home.path(), &["config", "set", "defaults.typo", "2"]);
    assert!(out.contains("defaults.typo"), "{out}");

    ok(home.path(), &["config", "remove", "typo"]);
    ok(home.path(), &["config", "remove", "defaults.typo"]);
    config_check_passes(home.path());
}

/// Issue #453 (ADR-0012 2026-10-15 later, item 66; 2026-10-16 item 75):
/// `[defaults] verify_interval_days` is a key `config set` writes (as an
/// integer, with the comments kept), refuses at 0 by name with the file
/// untouched, and `config check --json` prints the value in force: 365 until
/// the file says otherwise, which is also what `init` writes.
#[test]
fn set_of_defaults_verify_interval_days_takes_a_day_count_and_refuses_zero() {
    let home = home();
    let json_default = |home: &Path| -> serde_json::Value {
        let out = ok(home, &["--json", "config", "check"]);
        let v: serde_json::Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("config check --json is not one document: {e}"));
        v["defaults"]["verify_interval_days"].clone()
    };
    assert_eq!(json_default(home.path()), serde_json::json!(365));
    // The human report states it too.
    let human = ok(home.path(), &["config", "check"]);
    assert!(
        String::from_utf8_lossy(&human.stdout).contains("defaults: verify_interval_days = 365"),
        "{}",
        String::from_utf8_lossy(&human.stdout)
    );

    let out = refused(
        home.path(),
        &["config", "set", "defaults.verify_interval_days", "0"],
    );
    assert!(out.contains("defaults.verify_interval_days = 0"), "{out}");
    assert!(out.contains(">= 1"), "{out}");
    // Not a day count at all.
    refused(
        home.path(),
        &["config", "set", "defaults.verify_interval_days", "often"],
    );

    ok(
        home.path(),
        &["config", "set", "defaults.verify_interval_days", "180"],
    );
    let after = read(home.path());
    assert!(after.contains("verify_interval_days = 180\n"), "{after}");
    assert!(after.contains("# operator note: slices sized for the basement drive"));
    config_check_passes(home.path());
    assert_eq!(json_default(home.path()), serde_json::json!(180));
    // `config show` prints the file, so the key is there too.
    let shown = ok(home.path(), &["config", "show"]);
    assert!(String::from_utf8_lossy(&shown.stdout).contains("verify_interval_days = 180"));

    // A set value cannot be walked back below one day either.
    refused(
        home.path(),
        &["config", "set", "defaults.verify_interval_days", "0"],
    );
}

/// ADR-0012 2026-10-16 item 75 (#455): `config set` writes a collection's
/// `snapshot_command` and `release_command` as strings (shell syntax intact),
/// `config check` accepts them and runs neither, and the two refusals
/// (`release_command` alone, an empty command) leave the file untouched.
#[test]
fn set_of_a_collections_snapshot_hook_commands() {
    let home = home();
    with_collection(home.path(), "movies", "/srv/media/movies");
    let out = refused(
        home.path(),
        &[
            "config",
            "set",
            "collections[movies].release_command",
            "zfs destroy tank/media@tapectl",
        ],
    );
    assert!(
        out.contains("release_command is set but snapshot_command is not"),
        "{out}"
    );
    let out = refused(
        home.path(),
        &["config", "set", "collections[movies].snapshot_command", " "],
    );
    assert!(out.contains("snapshot_command is empty"), "{out}");

    let take = "zfs snapshot tank/media@tapectl && echo /tank/media/.zfs/snapshot/tapectl";
    ok(
        home.path(),
        &[
            "config",
            "set",
            "collections[movies].snapshot_command",
            take,
        ],
    );
    ok(
        home.path(),
        &[
            "config",
            "set",
            "collections[movies].release_command",
            "zfs destroy tank/media@tapectl",
        ],
    );
    let after = read(home.path());
    assert!(
        after.contains(&format!("snapshot_command = \"{take}\"\n")),
        "{after}"
    );
    assert!(after.contains("release_command = \"zfs destroy tank/media@tapectl\"\n"));
    assert!(after.contains("# operator note"), "{after}");
    config_check_passes(home.path());
}

/// Issue #453: `config set` refuses a copy floor below 1 and a place floor
/// below 0, at the defaults level and on an archive set, by name, with the
/// file untouched; the smallest legitimate values go through.
#[test]
fn set_of_a_copy_floor_below_one_or_a_place_floor_below_zero_is_refused() {
    let home = home();
    let out = refused(home.path(), &["config", "set", "defaults.min_copies", "0"]);
    assert!(out.contains("defaults.min_copies = 0"), "{out}");
    assert!(out.contains(">= 1"), "{out}");
    // `--` so that clap reads -1 as the value, not as a flag.
    let out = refused(
        home.path(),
        &["config", "set", "--", "defaults.min_locations", "-1"],
    );
    assert!(out.contains("defaults.min_locations = -1"), "{out}");
    assert!(out.contains(">= 0"), "{out}");

    ok(home.path(), &["config", "add", "archive_sets", "name=cold"]);
    let out = refused(
        home.path(),
        &["config", "set", "archive_sets[cold].min_copies", "0"],
    );
    assert!(
        out.contains("archive_sets[0] (\"cold\").min_copies = 0"),
        "{out}"
    );

    ok(home.path(), &["config", "set", "defaults.min_copies", "1"]);
    ok(
        home.path(),
        &["config", "set", "defaults.min_locations", "0"],
    );
    ok(
        home.path(),
        &["config", "set", "archive_sets[cold].min_copies", "1"],
    );
    let after = read(home.path());
    assert!(after.contains("min_copies = 1\n"), "{after}");
    assert!(after.contains("min_locations = 0\n"), "{after}");
    config_check_passes(home.path());
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn get_json(home: &Path, key: &str) -> serde_json::Value {
    let out = ok(home, &["--json", "config", "get", key]);
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("config get --json is not one document: {e}\n{}", text(&out)))
}

fn add_drive(home: &Path, name: &str, node: &str) {
    ok(
        home,
        &[
            "drive",
            "add",
            "--name",
            name,
            "--device-tape",
            &format!("/dev/tapectl-config-edit-nonexistent-{node}"),
            "--generation",
            "LTO-6",
        ],
    );
}

/// ADR-0012 item 214: `config get KEY` gives the value in force and where it
/// comes from — the file, or a default (a key the file does not set, an
/// optional one nothing sets) — and `--json` is `{key, value, source}`. A
/// bare key resolves to its full key, and the line names that key.
#[test]
fn config_get_names_the_value_in_force_and_where_it_comes_from() {
    let home = home();
    let h = home.path();
    let cfg = config_path(h);

    // init writes slice_size into the file.
    let line = stdout(&ok(h, &["config", "get", "defaults.slice_size"]));
    assert_eq!(
        line.trim_end(),
        format!("defaults.slice_size = \"1G\"  # set in {}", cfg.display())
    );
    let v = get_json(h, "defaults.slice_size");
    assert_eq!(
        v,
        serde_json::json!({"key": "defaults.slice_size", "value": "1G", "source": "config.toml"})
    );

    // Not in the file: its default, by its full key or its bare one.
    for key in ["host_check.max_load_per_cpu", "max_load_per_cpu"] {
        let v = get_json(h, key);
        assert_eq!(v["key"], "host_check.max_load_per_cpu", "{key}: {v}");
        assert_eq!(v["value"], serde_json::json!(1.0), "{key}: {v}");
        assert_eq!(v["source"], "default", "{key}: {v}");
    }
    ok(h, &["config", "remove", "defaults.slice_size"]);
    let line = stdout(&ok(h, &["config", "get", "slice_size"]));
    assert_eq!(line.trim_end(), "defaults.slice_size = \"1G\"  # default");

    // A drive's optional key nothing sets, then set by its bare name.
    add_drive(h, "drive-a", "a");
    let line = stdout(&ok(h, &["config", "get", "post_warning_bytes"]));
    assert_eq!(
        line.trim_end(),
        "backends.lto[drive-a].post_warning_bytes is not set  # default"
    );
    let v = get_json(h, "post_warning_bytes");
    assert_eq!(v["value"], serde_json::Value::Null, "{v}");
    assert_eq!(v["source"], "default", "{v}");

    let out = ok(h, &["config", "set", "post_warning_bytes", "99008M"]);
    assert!(
        stdout(&out).contains("set backends.lto[drive-a].post_warning_bytes = \"99008M\""),
        "{}",
        text(&out)
    );
    assert!(read(h).contains("post_warning_bytes = \"99008M\"\n"));
    let v = get_json(h, "backends.lto.post_warning_bytes");
    assert_eq!(
        v,
        serde_json::json!({
            "key": "backends.lto[drive-a].post_warning_bytes",
            "value": "99008M",
            "source": "config.toml",
        })
    );
    config_check_passes(h);
}

/// The key forms scripts already use keep working exactly (first-run.sh
/// step 8 sets `backends.lto[NAME].post_warning_bytes` and reads it back
/// from `config show --json`'s `backends.lto[]`), beside the new bare and
/// one-drive forms, which refuse naming each drive once there are two.
#[test]
fn the_existing_key_forms_work_beside_the_new_ones() {
    let home = home();
    let h = home.path();
    add_drive(h, "drive-a", "a");
    ok(
        h,
        &[
            "config",
            "set",
            "backends.lto[drive-a].post_warning_bytes",
            "99008M",
        ],
    );
    ok(
        h,
        &["config", "set", "backends.lto[0].enospc_buffer", "64M"],
    );
    let shown: serde_json::Value =
        serde_json::from_slice(&ok(h, &["--json", "config", "show"]).stdout).unwrap();
    let drive = &shown["backends"]["lto"][0];
    assert_eq!(drive["name"], "drive-a", "{shown}");
    assert_eq!(
        drive["device_tape"], "/dev/tapectl-config-edit-nonexistent-a",
        "{shown}"
    );
    assert_eq!(drive["post_warning_bytes"], "99008M", "{shown}");
    assert_eq!(drive["enospc_buffer"], "64M", "{shown}");

    // A second drive: the bare and one-drive forms name both and change
    // nothing; the selected form still works.
    add_drive(h, "drive-b", "b");
    let out = refused(h, &["config", "set", "backends.lto.enospc_buffer", "32M"]);
    assert!(out.contains("backends.lto[drive-a].enospc_buffer"), "{out}");
    assert!(out.contains("backends.lto[drive-b].enospc_buffer"), "{out}");
    let out = run(h, &["config", "get", "post_warning_bytes"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let said = text(&out);
    assert!(
        said.contains("backends.lto[drive-a].post_warning_bytes"),
        "{said}"
    );
    assert!(
        said.contains("backends.lto[drive-b].post_warning_bytes"),
        "{said}"
    );
    ok(
        h,
        &[
            "config",
            "set",
            "backends.lto[drive-b].post_warning_bytes",
            "40G",
        ],
    );
    assert_eq!(
        get_json(h, "backends.lto[drive-b].post_warning_bytes")["value"],
        "40G"
    );
    config_check_passes(h);
}

/// Item 214: a misspelt key names the key meant, on get, set, add and
/// remove; a refused edit is one line naming the key and what was wrong,
/// with no excerpt of the file and with types named plainly.
#[test]
fn a_misspelt_key_is_named_and_a_refusal_is_one_line() {
    let home = home();
    let h = home.path();
    let out = run(h, &["config", "get", "defaults.slice_sizee"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(
        text(&out).contains("did you mean defaults.slice_size?"),
        "{}",
        text(&out)
    );
    for args in [
        &["config", "set", "defaults.slice_sizee", "2G"][..],
        &["config", "add", "defaults.global_exclude", "*.bak"][..],
        &["config", "remove", "defaults.slice_sizee"][..],
    ] {
        let out = refused(h, args);
        assert!(out.contains("did you mean defaults."), "{args:?}: {out}");
    }

    let cfg = config_path(h);
    for (key, value, want) in [
        (
            "defaults.min_copies",
            "lots",
            "defaults.min_copies takes a whole number, not \"lots\"",
        ),
        (
            "defaults.encrypt",
            "maybe",
            "defaults.encrypt takes true or false, not \"maybe\"",
        ),
        (
            "defaults.slice_sizee",
            "2G",
            "defaults.slice_sizee is not a config key — did you mean defaults.slice_size?",
        ),
    ] {
        let out = run(h, &["config", "set", key, value]);
        assert!(!out.status.success(), "{}", text(&out));
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        let lines: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "{key}: one line: {err}");
        assert!(lines[0].contains(want), "{key}: {err}");
        assert!(
            lines[0].contains(&format!("{} is unchanged", cfg.display())),
            "{key}: {err}"
        );
        assert!(!err.contains("TOML parse error"), "{key}: {err}");
        assert!(!err.contains(" | "), "{key}: no excerpt: {err}");
    }
}

fn notes(out: &Output) -> Vec<String> {
    stdout(out)
        .lines()
        .filter_map(|l| l.strip_prefix("note: "))
        .map(str::to_string)
        .collect()
}

/// Item 214: `config check` still prints `config: valid` and exits 0 for a
/// config that loads, and notes what it lacks: no drive, a drive without
/// its measured post_warning_bytes, a collection whose root or tenant is
/// not there. `--json` keeps its keys and adds `notes`.
#[test]
fn config_check_notes_what_a_loadable_config_lacks_at_exit_0() {
    let home = home();
    let h = home.path();
    let out = ok(h, &["config", "check"]);
    assert!(
        stdout(&out).starts_with("config: valid\n"),
        "{}",
        text(&out)
    );
    let n = notes(&out);
    assert!(
        n.iter()
            .any(|l| l.contains("no drive is configured") && l.contains("`tapectl drive add")),
        "{n:?}"
    );
    let json: serde_json::Value =
        serde_json::from_slice(&ok(h, &["--json", "config", "check"]).stdout).unwrap();
    assert_eq!(json["valid"], true, "{json}");
    for key in [
        "problems",
        "tape_devices",
        "staging",
        "dar",
        "defaults",
        "ops",
    ] {
        assert!(json.get(key).is_some(), "{key} is kept: {json}");
    }
    assert!(
        json["notes"]
            .as_array()
            .is_some_and(|a| a.iter().any(|l| l.as_str().unwrap().contains("no drive"))),
        "{json}"
    );

    add_drive(h, "drive-a", "a");
    let n = notes(&ok(h, &["config", "check"]));
    assert!(!n.iter().any(|l| l.contains("no drive")), "{n:?}");
    let room = n
        .iter()
        .find(|l| l.contains("drive \"drive-a\" has no post_warning_bytes"))
        .unwrap_or_else(|| panic!("{n:?}"));
    for want in ["fill ceiling", "slice cap", "scripts/lto-profile.sh"] {
        assert!(room.contains(want), "{want}: {room}");
    }
    ok(h, &["config", "set", "post_warning_bytes", "99008M"]);
    let n = notes(&ok(h, &["config", "check"]));
    assert!(!n.iter().any(|l| l.contains("post_warning_bytes")), "{n:?}");

    // A collection whose root does not exist, for a tenant that does not.
    with_collection(h, "movies", "/nonexistent/tapectl-config-edit/movies");
    let out = ok(h, &["config", "check"]);
    assert!(
        stdout(&out).starts_with("config: valid\n"),
        "{}",
        text(&out)
    );
    let n = notes(&out);
    assert!(
        n.iter()
            .any(|l| l.starts_with("collection \"movies\" cannot run: root")),
        "{n:?}"
    );
    assert!(
        n.iter()
            .any(|l| l.starts_with("collection \"movies\" cannot run: tenant \"family\"")),
        "{n:?}"
    );
}
