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

// ── #350(b): `key import` of a key the catalog already has ──

/// `init` plus a tenant `family` with its two auto-generated keys.
fn init_home_with_family() -> TempDir {
    let home = init_home();
    ok(home.path(), &["tenant", "add", "family"]);
    home
}

fn pub_file(home: &Path, alias: &str) -> String {
    home.join(".tapectl")
        .join("keys")
        .join(format!("{alias}.age.pub"))
        .to_str()
        .unwrap()
        .to_string()
}

/// A public key the catalog has never seen, in a file of its own.
fn fresh_pub_file(dir: &Path, name: &str) -> String {
    let kp = tapectl::crypto::keys::generate_keypair();
    let path = dir.join(name);
    std::fs::write(&path, format!("{}\n", kp.public_key)).unwrap();
    path.to_str().unwrap().to_string()
}

fn refused(home: &Path, args: &[&str]) -> String {
    let o = run(home, args);
    assert!(
        !o.status.success(),
        "`tapectl {}` should have been refused\nstdout: {}",
        args.join(" "),
        stdout(&o)
    );
    let err = stderr(&o);
    assert!(
        !err.contains("UNIQUE constraint"),
        "`tapectl {}` leaked a raw SQLite error: {err}",
        args.join(" ")
    );
    err
}

fn key_row(home: &Path, tenant: &str, alias: &str) -> serde_json::Value {
    let json: serde_json::Value = serde_json::from_str(&stdout(&ok(
        home,
        &["--json", "key", "list", "--tenant", tenant],
    )))
    .unwrap();
    json.as_array()
        .unwrap()
        .iter()
        .find(|r| r["alias"] == alias)
        .unwrap_or_else(|| panic!("no key {alias} in {json}"))
        .clone()
}

/// Re-importing a key that is already registered and active used to fail
/// with `UNIQUE constraint failed: encryption_keys.fingerprint`. Now it says
/// which key it already is, dry run included; and an alias that is taken by
/// a DIFFERENT key says so too, instead of the other raw UNIQUE error.
#[test]
fn importing_an_already_registered_key_says_so_plainly() {
    let home = init_home_with_family();
    let h = home.path();
    let family_pub = pub_file(h, "family-primary");

    for dry in [false, true] {
        let mut args = vec![];
        if dry {
            args.push("--dry-run");
        }
        args.extend([
            "key",
            "import",
            "--tenant",
            "family",
            "--alias",
            "again",
            &family_pub,
        ]);
        let err = refused(h, &args);
        assert!(
            err.contains("already in the catalog as \"family-primary\"") && err.contains("active"),
            "dry_run={dry}: the refusal must name the key and its state: {err}"
        );
    }

    // Positive control: a key the catalog does not have imports fine.
    let new_pub = fresh_pub_file(h, "new.pub");
    ok(
        h,
        &[
            "key", "import", "--tenant", "family", "--alias", "laptop", &new_pub,
        ],
    );
    assert_eq!(key_row(h, "family", "family-laptop")["is_active"], true);

    // A different key under a taken alias.
    let other_pub = fresh_pub_file(h, "other.pub");
    let err = refused(
        h,
        &[
            "key", "import", "--tenant", "family", "--alias", "laptop", &other_pub,
        ],
    );
    assert!(err.contains("family-laptop"), "{err}");
}

/// `key rotate` deactivates every ordinary key of the tenant — including a
/// recipient someone else holds the secret for. Re-importing that recipient
/// used to be a raw UNIQUE error. Now the refusal says it is deactivated.
#[test]
fn a_deactivated_key_is_named_as_deactivated() {
    let home = init_home_with_family();
    let h = home.path();
    let family_pub = pub_file(h, "family-primary");
    ok(h, &["key", "rotate", "--tenant", "family"]);
    assert_eq!(key_row(h, "family", "family-primary")["is_active"], false);

    let err = refused(
        h,
        &[
            "key",
            "import",
            "--tenant",
            "family",
            "--alias",
            "readd",
            &family_pub,
        ],
    );
    assert!(
        err.contains("already in the catalog as \"family-primary\"") && err.contains("deactivated"),
        "the refusal must say the key is deactivated: {err}"
    );
}

/// The escrow identity (a recipient of every write already, ADR-0005) and
/// another tenant's key (one key belongs to one tenant) are refused by name.
#[test]
fn import_refuses_the_escrow_key_and_another_tenants_key() {
    let home = init_home_with_family();
    let h = home.path();
    let escrow_pub = pub_file(h, &format!("{OP}-escrow"));
    let family_pub = pub_file(h, "family-primary");

    let err = refused(
        h,
        &[
            "key",
            "import",
            "--tenant",
            "family",
            "--alias",
            "esc",
            &escrow_pub,
        ],
    );
    assert!(err.contains("escrow identity"), "{err}");

    let err = refused(
        h,
        &[
            "key",
            "import",
            "--tenant",
            OP,
            "--alias",
            "fam",
            &family_pub,
        ],
    );
    assert!(
        err.contains("tenant \"family\""),
        "must name the tenant that owns the key: {err}"
    );
}
