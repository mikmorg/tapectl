//! ADR-0012 2026-10-19 item 107: `--key FILE` on `restore unit`, `restore
//! file`, `restore volume` and `catalog rebuild`, through the real binary
//! against a throwaway home made by `init --escrow-public-key` — the heir's
//! home, which holds the escrow PUBLIC key and nothing that can open a tape.
//!
//! No tape device exists here (never a real one): what the binary can prove
//! without a drive is the half the heir meets first — the flag's spelling and
//! repetition, a key file that is not an identity refused by name BEFORE the
//! drive is touched, and a good key neither stored nor registered nor logged.
//! The restores and the rebuild that use the key are in the library's tests
//! (`volume::restore::heir_tests`, `repair_tests`), over a fake tape.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use age::secrecy::ExposeSecret;
use clap::Parser;
use tapectl::cli::{catalog::CatalogCommands, restore::RestoreCommands, Cli, Commands};
use tempfile::TempDir;

const DEVICE: &str = "/nonexistent/tapectl-restore-key-nst";

fn tapectl(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .arg("--home")
        .arg(home)
        .args(args)
        .env("HOME", home.parent().unwrap())
        .env_remove("TAPECTL_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("spawn tapectl")
}

/// An identity file as `age-keygen -o` writes one: (path, public, secret).
fn identity_file(dir: &Path, name: &str) -> (PathBuf, String, String) {
    let id = age::x25519::Identity::generate();
    let public = id.to_public().to_string();
    let secret = id.to_string().expose_secret().to_string();
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!("# created: 2026-10-19T00:00:00Z\n# public key: {public}\n{secret}\n"),
    )
    .unwrap();
    (path, public, secret)
}

/// The heir's home: `init --operator heir --escrow-public-key <public>`.
fn heir_home(tmp: &Path, escrow_public: &str) -> PathBuf {
    let home = tmp.join("h");
    let out = tapectl(
        &home,
        &[
            "init",
            "--operator",
            "heir",
            "--escrow-public-key",
            escrow_public,
        ],
    );
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    home
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// Files under `root` whose bytes hold `needle`, relative.
fn files_holding(root: &Path, needle: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if std::fs::read(&p)
                .unwrap()
                .windows(needle.len())
                .any(|w| w == needle.as_bytes())
            {
                found.push(p.strip_prefix(root).unwrap().display().to_string());
            }
        }
    }
    found.sort();
    found
}

fn key_listing(home: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(home.join("keys"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// The four command lines, each given `keys` (flag, file) pairs.
fn lines(dest: &str, keys: &[&str]) -> Vec<Vec<String>> {
    let with_keys = |line: Vec<&str>| -> Vec<String> {
        let mut line: Vec<String> = line.into_iter().map(String::from).collect();
        for k in keys {
            line.push("--key".to_string());
            line.push((*k).to_string());
        }
        line
    };
    vec![
        with_keys(vec![
            "catalog",
            "rebuild",
            "--from-volume",
            "--device",
            DEVICE,
        ]),
        with_keys(vec![
            "restore", "unit", "--unit", "keepsake", "--from", "L6-0001", "--device", DEVICE,
            "--to", dest,
        ]),
        with_keys(vec![
            "restore", "file", "--unit", "keepsake", "--file", "a.txt", "--from", "L6-0001",
            "--device", DEVICE, "--to", dest,
        ]),
        with_keys(vec![
            "restore", "volume", "L6-0001", "--device", DEVICE, "--to", dest,
        ]),
    ]
}

fn run(home: &Path, line: &[String]) -> (bool, String) {
    let args: Vec<&str> = line.iter().map(String::as_str).collect();
    let out = tapectl(home, &args);
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A key file that is not an identity is refused by name, with the reason,
/// before the drive is touched: the device never appears in the error, though
/// it does not exist and opening it would have failed. A good key on the same
/// line gets past the key stage (the positive control) and fails further on —
/// at the unit the catalog does not have, or the device.
#[test]
fn a_bad_key_is_refused_by_name_before_the_drive_and_a_good_key_is_not() {
    let tmp = TempDir::new().unwrap();
    let (good, public, secret) = identity_file(tmp.path(), "escrow.key");
    let home = heir_home(tmp.path(), &public);
    let public_only = tmp.path().join("escrow.pub");
    std::fs::write(&public_only, format!("{public}\n")).unwrap();
    let junk = tmp.path().join("junk.key");
    std::fs::write(&junk, "not a key\n").unwrap();
    let missing = tmp.path().join("missing.key");
    let dest = tmp.path().join("out");

    for bad in [&public_only, &junk, &missing] {
        // The good key first, the bad one second: every `--key` is checked.
        for line in lines(s(&dest), &[s(&good), s(bad)]) {
            let (ok, err) = run(&home, &line);
            assert!(!ok, "{line:?} should be refused");
            assert!(err.contains(s(bad)), "{line:?}: names the file: {err}");
            assert!(
                err.contains("is not an age X25519 identity file") || err.contains("cannot read"),
                "{line:?}: {err}"
            );
            assert!(
                !err.contains("nonexistent"),
                "{line:?}: the drive was reached before the key was checked: {err}"
            );
            assert!(!err.contains(&secret), "never echoes a secret: {err}");
        }
    }

    // Positive control: the good key alone passes the key stage.
    for line in lines(s(&dest), &[s(&good)]) {
        let (ok, err) = run(&home, &line);
        assert!(!ok, "no tape and no catalog: {line:?} must fail somewhere");
        assert!(
            !err.contains("identity file") && !err.contains("cannot read"),
            "{line:?}: a good key is not refused as a key: {err}"
        );
    }
    assert!(!dest.exists(), "nothing was restored");
}

/// The key is used and kept nowhere: after commands that took it, the heir's
/// home has no new key file, no registered key and no copy of the secret
/// anywhere — catalog, config, logs, keys. The secret IS in the file passed
/// (the control for the scan), and the scan finds a planted copy.
#[test]
fn a_key_given_is_not_stored_registered_or_logged() {
    let tmp = TempDir::new().unwrap();
    let (good, public, secret) = identity_file(tmp.path(), "escrow.key");
    let home = heir_home(tmp.path(), &public);
    let keys_before = key_listing(&home);
    let rows = |home: &Path| -> i64 {
        rusqlite::Connection::open(home.join("tapectl.db"))
            .unwrap()
            .query_row("SELECT COUNT(*) FROM encryption_keys", [], |r| r.get(0))
            .unwrap()
    };
    let rows_before = rows(&home);
    assert_eq!(rows_before, 3, "heir's primary and backup, and the escrow");

    // Controls for the scan.
    assert_eq!(files_holding(tmp.path(), &secret), ["escrow.key"]);
    assert!(files_holding(&home, &secret).is_empty());
    std::fs::write(home.join("planted.txt"), format!("x{secret}x")).unwrap();
    assert_eq!(files_holding(&home, &secret), ["planted.txt"]);
    std::fs::remove_file(home.join("planted.txt")).unwrap();

    let dest = tmp.path().join("out");
    for line in lines(s(&dest), &[s(&good)]) {
        let (ok, _) = run(&home, &line);
        assert!(!ok);
    }
    // The commands that start a session left a log to scan.
    let logs = std::fs::read_dir(home.join("logs"))
        .map(|d| d.flatten().count())
        .unwrap_or(0);
    assert!(logs > 0, "positive control: a session log exists");

    assert_eq!(key_listing(&home), keys_before, "keys/ is untouched");
    assert_eq!(rows(&home), rows_before, "no key was registered");
    assert_eq!(
        files_holding(&home, &secret),
        Vec::<String>::new(),
        "the secret is in no file of the home"
    );
}

/// The flag's surface, through the real clap tree: repeatable on all four
/// commands, order kept, required of `catalog rebuild`, and not a switch to
/// anything else (`restore raw-volume` reads no key and takes none).
#[test]
fn key_is_repeatable_on_all_four_commands_and_required_of_the_rebuild() {
    let parse = |args: &[&str]| {
        let mut argv = vec!["tapectl"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv)
    };
    let names = |keys: &[PathBuf]| -> Vec<String> {
        keys.iter().map(|k| k.display().to_string()).collect()
    };

    let Commands::Restore {
        command: RestoreCommands::Unit { key, .. },
    } = parse(&[
        "restore", "unit", "--unit", "u", "--to", "d", "--key", "a.key", "--key", "b.key",
    ])
    .unwrap()
    .command
    else {
        panic!("restore unit");
    };
    assert_eq!(names(&key), ["a.key", "b.key"]);

    let Commands::Restore {
        command: RestoreCommands::File { key, .. },
    } = parse(&[
        "restore", "file", "--unit", "u", "--file", "f", "--to", "d", "--key", "a.key",
    ])
    .unwrap()
    .command
    else {
        panic!("restore file");
    };
    assert_eq!(names(&key), ["a.key"]);

    let Commands::Restore {
        command: RestoreCommands::Volume { key, .. },
    } = parse(&[
        "restore", "volume", "L", "--to", "d", "--key", "a.key", "--key", "b.key",
    ])
    .unwrap()
    .command
    else {
        panic!("restore volume");
    };
    assert_eq!(names(&key), ["a.key", "b.key"]);

    let Commands::Catalog {
        command: CatalogCommands::Rebuild { key, .. },
    } = parse(&[
        "catalog",
        "rebuild",
        "--from-volume",
        "--key",
        "a.key",
        "--key",
        "b.key",
    ])
    .unwrap()
    .command
    else {
        panic!("catalog rebuild");
    };
    assert_eq!(names(&key), ["a.key", "b.key"]);

    // No key: a restore needs none (the home's own are used), the rebuild
    // needs at least one.
    let Commands::Restore {
        command: RestoreCommands::Unit { key, .. },
    } = parse(&["restore", "unit", "--unit", "u", "--to", "d"])
        .unwrap()
        .command
    else {
        panic!("restore unit");
    };
    assert!(key.is_empty());
    let err = parse(&["catalog", "rebuild", "--from-volume"])
        .unwrap_err()
        .to_string();
    assert!(err.contains("--key"), "{err}");

    assert!(
        parse(&["restore", "raw-volume", "--to", "d", "--key", "a.key"]).is_err(),
        "a raw dump decrypts nothing and takes no key"
    );
}
