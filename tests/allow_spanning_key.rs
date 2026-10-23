//! The spanning policy key `allow_spanning` through the real binary (#478,
//! ADR-0012 item 82: "a policy key resolved like the others (dotfile, then
//! archive set, then `[defaults]`), off by default").
//!
//! What only the binary shows: `init` writes the key, `config check` accepts
//! it in each of its three places and refuses a misspelling of it by name in
//! each, `archive-set sync` carries an `[[archive_sets]]` value to the
//! catalog column (and leaves the column NULL, "not set", when the table does
//! not name it), and `unit tag` and `unit rename`, which rewrite the dotfile,
//! keep it. How it resolves is `policy::resolve`'s own unit tests. A drive is
//! never touched: nothing here reads or writes a tape.

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

fn append_config(home: &Path, extra: &str) {
    let mut cfg = std::fs::read_to_string(config_path(home)).unwrap();
    cfg.push_str(extra);
    std::fs::write(config_path(home), cfg).unwrap();
}

/// A fresh home with tenant `alice` and the unit `photos` (its dotfile in
/// the returned directory).
fn home_with_a_unit() -> (TempDir, TempDir) {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init", "--no-escrow"]);
    ok(home.path(), &["tenant", "add", "alice"]);
    let src = TempDir::new().unwrap();
    std::fs::write(src.path().join("f.txt"), b"hello").unwrap();
    ok(
        home.path(),
        &[
            "unit",
            "init",
            src.path().to_str().unwrap(),
            "--tenant",
            "alice",
            "--name",
            "photos",
        ],
    );
    (home, src)
}

fn dotfile(src: &Path) -> PathBuf {
    src.join(".tapectl-unit.toml")
}

fn add_dotfile_policy(src: &Path, policy: &str) {
    let mut raw = std::fs::read_to_string(dotfile(src)).unwrap();
    raw.push_str(&format!("\n[policy]\n{policy}\n"));
    std::fs::write(dotfile(src), raw).unwrap();
}

fn archive_set_column(home: &Path, set: &str) -> Option<i64> {
    let conn = tapectl::db::open(&home.join(".tapectl").join("tapectl.db")).expect("open catalog");
    conn.query_row(
        "SELECT allow_spanning FROM archive_sets WHERE name = ?1",
        [set],
        |r| r.get(0),
    )
    .unwrap()
}

/// `init` writes `allow_spanning = false`, and `config check` says the
/// config is valid with the key set in `[defaults]`, in an
/// `[[archive_sets]]` table and in a unit's dotfile `[policy]`, and warns
/// about none of them.
#[test]
fn config_check_accepts_allow_spanning_in_each_place() {
    let (home, src) = home_with_a_unit();
    let written = std::fs::read_to_string(config_path(home.path())).unwrap();
    assert!(written.contains("allow_spanning = false"), "{written}");

    ok(
        home.path(),
        &["config", "set", "defaults.allow_spanning", "true"],
    );
    append_config(
        home.path(),
        "\n[[archive_sets]]\nname = \"whole\"\nallow_spanning = false\n",
    );
    add_dotfile_policy(src.path(), "allow_spanning = true");

    let out = ok(home.path(), &["config", "check"]);
    let all = text(&out);
    assert!(all.contains("config: valid"), "{all}");
    assert!(!all.contains("cannot be read"), "{all}");
    assert!(
        !all.contains("allow_spanning"),
        "nothing to say about it: {all}"
    );
}

/// A misspelled spanning key is refused by name in `[defaults]` and in an
/// `[[archive_sets]]` table (the config does not load, and `config check`
/// exits non-zero naming it), and named by `config check` in a dotfile,
/// whose `[policy]` every command that resolves the unit's policy then
/// refuses.
#[test]
fn a_misspelled_allow_spanning_is_refused_by_name_in_each_place() {
    for (bad, extra) in [
        ("allow_span", None),
        ("allowSpanning", None),
        (
            "allow_span",
            Some("\n[[archive_sets]]\nname = \"typo\"\nallow_span = true\n"),
        ),
        (
            "allowSpanning",
            Some("\n[[archive_sets]]\nname = \"typo\"\nallowSpanning = true\n"),
        ),
    ] {
        let home = TempDir::new().unwrap();
        ok(home.path(), &["init", "--no-escrow"]);
        match extra {
            Some(table) => append_config(home.path(), table),
            None => {
                let cfg = std::fs::read_to_string(config_path(home.path())).unwrap();
                let typo = cfg.replacen("[defaults]\n", &format!("[defaults]\n{bad} = true\n"), 1);
                assert_ne!(typo, cfg, "precondition: init writes [defaults]");
                std::fs::write(config_path(home.path()), typo).unwrap();
            }
        }
        let check = run(home.path(), &["config", "check"]);
        assert!(!check.status.success(), "{bad}: {}", text(&check));
        assert!(text(&check).contains(bad), "{bad}: {}", text(&check));
        // Every other command refuses the file too.
        let list = run(home.path(), &["archive-set", "list"]);
        assert!(!list.status.success(), "{bad}: {}", text(&list));
        assert!(text(&list).contains(bad), "{bad}: {}", text(&list));
    }

    for bad in ["allow_span", "allowSpanning"] {
        let (home, src) = home_with_a_unit();
        add_dotfile_policy(src.path(), &format!("{bad} = true"));
        let all = text(&ok(home.path(), &["config", "check"]));
        assert!(all.contains("cannot be read"), "{bad}: {all}");
        assert!(all.contains(bad), "{bad}: {all}");
    }
}

/// `unit tag` and `unit rename` rewrite the whole dotfile; the spanning key
/// survives both, at either value.
#[test]
fn unit_tag_and_unit_rename_keep_allow_spanning() {
    for value in ["true", "false"] {
        let (home, src) = home_with_a_unit();
        add_dotfile_policy(src.path(), &format!("allow_spanning = {value}"));
        let line = format!("allow_spanning = {value}");

        ok(home.path(), &["unit", "tag", "photos", "--add", "family"]);
        let raw = std::fs::read_to_string(dotfile(src.path())).unwrap();
        assert!(raw.contains("family"), "the tag landed: {raw}");
        assert!(raw.contains(&line), "unit tag dropped it: {raw}");

        ok(home.path(), &["unit", "rename", "photos", "pictures"]);
        let raw = std::fs::read_to_string(dotfile(src.path())).unwrap();
        assert!(
            raw.contains("name = \"pictures\""),
            "the rename landed: {raw}"
        );
        assert!(raw.contains(&line), "unit rename dropped it: {raw}");
    }
}

/// `archive-set sync` writes an `[[archive_sets]]` table's `allow_spanning`
/// to the catalog column; a table that does not name it leaves the column
/// NULL ("not set", deferring to `[defaults]`), and a second sync of the same
/// file is a no-op for both sets.
#[test]
fn archive_set_sync_carries_allow_spanning_and_leaves_it_null_when_unnamed() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init", "--no-escrow"]);
    append_config(
        home.path(),
        "\n[[archive_sets]]\nname = \"span\"\nallow_spanning = true\n\
         \n[[archive_sets]]\nname = \"silent\"\nmin_copies = 3\n",
    );
    let first =
        String::from_utf8_lossy(&ok(home.path(), &["archive-set", "sync"]).stdout).to_string();
    assert!(first.contains("2 created"), "{first}");
    assert_eq!(archive_set_column(home.path(), "span"), Some(1));
    assert_eq!(archive_set_column(home.path(), "silent"), None);

    let second =
        String::from_utf8_lossy(&ok(home.path(), &["archive-set", "sync"]).stdout).to_string();
    assert!(
        second.contains("0 created, 0 updated, 2 unchanged"),
        "{second}"
    );
    assert_eq!(archive_set_column(home.path(), "silent"), None);

    let info: serde_json::Value =
        serde_json::from_slice(&ok(home.path(), &["--json", "archive-set", "info", "span"]).stdout)
            .unwrap();
    assert_eq!(info["allow_spanning"], serde_json::json!(true), "{info}");
    let info: serde_json::Value = serde_json::from_slice(
        &ok(home.path(), &["--json", "archive-set", "info", "silent"]).stdout,
    )
    .unwrap();
    assert_eq!(info["allow_spanning"], serde_json::Value::Null, "{info}");
}
