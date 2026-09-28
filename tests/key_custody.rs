//! Key custody at the command line (issue #350): what `key list`, `key
//! import` and `db backup --include-keys` tell the operator, driven through
//! the real binary against a throwaway `HOME` — never the operator's real
//! `~/.tapectl`.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

/// The operator tenant every test here initialises.
const OP: &str = "op";

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("failed to spawn tapectl binary")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn ok(home: &Path, args: &[&str]) -> Output {
    let o = run(home, args);
    assert!(
        o.status.success(),
        "`tapectl {}` failed\nstdout: {}\nstderr: {}",
        args.join(" "),
        stdout(&o),
        stderr(&o)
    );
    o
}

/// `init` with the default escrow identity (ADR-0005), so the operator
/// tenant carries the escrow row.
fn init_home() -> TempDir {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init", "--operator", OP]);
    home
}

// ── #350(c): the escrow key is labelled escrow, not `primary` ──

/// The escrow row is stored with `key_type = 'primary'`: migration 001's
/// `CHECK(key_type IN ('primary','backup'))` has no room for `'escrow'` and
/// migration 003 added `is_escrow` without touching that CHECK. `key list`
/// printed the stored column verbatim, so the one key that is a recipient of
/// every tape showed up as an ordinary `primary`. The `KeyRow` JSON pin in
/// `src/cli/key.rs` that expects `"escrow"` is a synthetic row — it was never
/// fed a real escrow row, and `key list --json` does not use `KeyRow`.
#[test]
fn key_list_labels_the_escrow_key_as_escrow_in_the_table_and_in_json() {
    let home = init_home();

    let table = stdout(&ok(home.path(), &["key", "list", "--tenant", OP]));
    let escrow_line = table
        .lines()
        .find(|l| l.contains(&format!("{OP}-escrow")))
        .unwrap_or_else(|| panic!("no escrow row in the table:\n{table}"));
    let cells: Vec<&str> = escrow_line.split('|').map(str::trim).collect();
    // | Alias | Type | Active | Escrow | Fingerprint | Created |
    assert_eq!(
        cells.get(2).copied(),
        Some("escrow"),
        "the Type column of the escrow row must say escrow: {escrow_line:?}"
    );
    // Positive control: the operator's own ordinary key keeps its type.
    let primary_line = table
        .lines()
        .find(|l| l.contains(&format!("{OP}-primary")))
        .unwrap_or_else(|| panic!("no primary row in the table:\n{table}"));
    let cells: Vec<&str> = primary_line.split('|').map(str::trim).collect();
    assert_eq!(cells.get(2).copied(), Some("primary"), "{primary_line:?}");

    let json: serde_json::Value = serde_json::from_str(&stdout(&ok(
        home.path(),
        &["--json", "key", "list", "--tenant", OP],
    )))
    .expect("key list --json must print JSON");
    let rows = json.as_array().expect("an array of keys");
    let escrow = rows
        .iter()
        .find(|r| r["is_escrow"] == serde_json::json!(true))
        .unwrap_or_else(|| panic!("no is_escrow row in --json: {json}"));
    assert_eq!(escrow["key_type"], "escrow", "{escrow}");
    let ordinary = rows
        .iter()
        .find(|r| r["alias"] == format!("{OP}-primary"))
        .unwrap_or_else(|| panic!("no {OP}-primary row in --json: {json}"));
    assert_eq!(ordinary["key_type"], "primary", "{ordinary}");
}
