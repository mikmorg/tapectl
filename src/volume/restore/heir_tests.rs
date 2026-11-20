//! ADR-0012 2026-10-19 item 107: the heir's path, end to end, with only the
//! escrow secret.
//!
//! A tape is written by the real `volume write` (every unit staged by the
//! real `stage create`: GNU tar's stream cut at member starts, each slice
//! age-encrypted to the tenant, the operator and the escrow recipient, the
//! file table kept) into a `MemStore`. The heir's home is then made the way
//! `tapectl init --escrow-public-key` makes one (a fresh operator whose keys
//! did not write the tape, and the escrow PUBLIC key registered; `init`
//! itself lives in the binary, so the library calls it makes stand in), and
//! the heir runs, in library form, exactly the commands the ADR names:
//!
//! ```text
//! tapectl catalog rebuild --from-volume --key escrow.key
//! tapectl restore unit --unit keepsake --key escrow.key --to DIR
//! ```
//!
//! Nothing here is faked but the drive: the rebuild is `rebuild_from_store`
//! and the restore `restore_unit_from_store`, the functions the commands call
//! over a `TapeStore`.
//!
//! A restore plans from the staging's file table, which a rebuilt catalog
//! has not got until `catalog rebuild` restores it (T4.9): the tests that
//! restore after a rebuild carry that ignore, and the proofs of the rebuild
//! and of the key's custody run now.

use std::path::Path;

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use super::fixture::{self, Home as Writer, Item};
use super::{restore_unit_from_store, RestoreOptions, RestoreReport, RestoreTarget};
use crate::config::{Config, TapectlPaths};
use crate::crypto::keys::{generate_keypair, KeyFile};
use crate::error::Result;
use crate::progress::{self, Display};
use crate::store::injected::InjectedDrive;
use crate::store::MemStore;
use crate::tape::contact::{ContactSite, Medium, Operation};
use crate::tape::fake::FakeTape;
use crate::volume::rebuild;

const LABEL: &str = "HEIR01";
const UNIT: &str = "keepsake";

fn sha_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn cfg() -> &'static Config {
    static CFG: std::sync::OnceLock<Config> = std::sync::OnceLock::new();
    CFG.get_or_init(Config::default)
}

/// What the writer's machine leaves behind: the sealed tape, the files
/// it holds, and the escrow keypair — whose SECRET is the only thing the
/// heir has.
struct Tape {
    store: MemStore,
    files: Vec<(String, Vec<u8>)>,
    escrow_public: String,
    escrow_secret: String,
    operator_public: String,
    _writer: Writer,
}

/// Four 100 KB files in one unit: several slices at a 150 KiB cut.
fn written_tape() -> Tape {
    let mut w = Writer::new("150KiB", None, 1);
    // The fixture's home keeps no escrow secret (it is shown once, stored
    // nowhere): register a recipient whose secret this test holds, before
    // anything is staged to it.
    w.conn
        .execute("DELETE FROM encryption_keys WHERE is_escrow = 1", [])
        .unwrap();
    for entry in std::fs::read_dir(&w.paths.keys_dir).unwrap().flatten() {
        if entry.file_name().to_string_lossy().contains("-escrow") {
            std::fs::remove_file(entry.path()).unwrap();
        }
    }
    let escrow = crate::cli::key::create_escrow_recipient(&w.conn, &w.paths, None).unwrap();
    let items = fixture::files("f", 4, 100_000, 3);
    let files: Vec<(String, Vec<u8>)> = items
        .iter()
        .map(|i| match i {
            Item::File(p, b) => (p.clone(), b.clone()),
            other => panic!("not a file: {other:?}"),
        })
        .collect();
    w.unit(fixture::TENANT, UNIT, &items);
    w.stage(UNIT);
    assert!(w.num_slices(UNIT) >= 3, "want a many-slice staging");
    w.volume(LABEL);
    w.write(LABEL);
    let store = w.tapes.remove(LABEL).expect("the tape was written");
    let operator_public: String = w
        .conn
        .query_row(
            "SELECT k.public_key FROM encryption_keys k JOIN tenants t ON t.id = k.tenant_id
              WHERE t.name = ?1 AND k.is_active = 1 AND k.is_escrow = 0",
            params![fixture::OPERATOR],
            |r| r.get(0),
        )
        .unwrap();
    Tape {
        store,
        files,
        escrow_public: escrow.keypair.public_key,
        escrow_secret: escrow.keypair.secret_key,
        operator_public,
        _writer: w,
    }
}

/// The heir's machine: `tapectl init --operator heir --escrow-public-key
/// <the cover sheet's>` — a fresh operator whose keys did not write the
/// tape, and the escrow PUBLIC key registered. The secret is in no file
/// of this home.
struct Heir {
    paths: TapectlPaths,
    conn: Connection,
    _dir: TempDir,
}

fn heir_home(escrow_public: &str) -> Heir {
    let dir = TempDir::new().unwrap();
    let paths = TapectlPaths::new(dir.path().join(".tapectl"));
    paths.ensure_dirs().unwrap();
    std::fs::create_dir_all(&paths.logs_dir).unwrap();
    let conn = crate::db::open(&paths.db_file).unwrap();
    crate::tenant::add_tenant(&conn, &paths, "heir", Some("System operator"), true).unwrap();
    crate::cli::key::adopt_escrow_recipient(&conn, &paths, escrow_public).unwrap();
    Heir {
        paths,
        conn,
        _dir: dir,
    }
}

/// Every file under `root`, relative, with its sha256.
fn tree_listing(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push((
                    p.strip_prefix(root).unwrap().display().to_string(),
                    sha_hex(&std::fs::read(&p).unwrap()),
                ));
            }
        }
    }
    out.sort();
    out
}

/// Files under `root` whose bytes hold `needle`.
fn files_holding(root: &Path, needle: &str) -> Vec<String> {
    let mut out = Vec::new();
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
                out.push(p.strip_prefix(root).unwrap().display().to_string());
            }
        }
    }
    out.sort();
    out
}

fn rebuild_with(home: &Heir, tape: &mut Tape, given: &[KeyFile]) -> Result<rebuild::RebuildReport> {
    let scratch = TempDir::new().unwrap();
    rebuild::rebuild_from_store(
        &home.conn,
        &mut tape.store,
        &KeyFile::all_identities(given),
        Some(LABEL),
        "recovered",
        Some("lto0"),
        scratch.path(),
        "memstore",
        ContactSite::new(
            cfg(),
            Operation::CatalogRebuild,
            "memstore",
            Medium::NoBackend,
        ),
    )
}

fn restore_with(
    home: &Heir,
    tape: &mut Tape,
    dest: &Path,
    options: &RestoreOptions,
) -> Result<RestoreReport> {
    let dest = dest.to_string_lossy().into_owned();
    restore_unit_from_store(
        &home.conn,
        &home.paths,
        cfg(),
        UNIT,
        LABEL,
        1,
        RestoreTarget::Unit { dest_dir: &dest },
        options,
        &mut tape.store,
        ContactSite::new(cfg(), Operation::RestoreUnit, "memstore", Medium::NoBackend),
    )
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn key_file(dir: &Path, name: &str, tape: &Tape) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(
        &p,
        format!(
            "# created: 2026-10-19T00:00:00Z\n# public key: {}\n{}\n",
            tape.escrow_public, tape.escrow_secret
        ),
    )
    .unwrap();
    p
}

/// The rebuild half of item 107 and the key's custody: the heir with only
/// the escrow secret rebuilds the catalog from the tape, files the unit
/// under the tenant that owns it, and the secret goes nowhere. (The restore
/// that follows is `an_heir_with_only_the_escrow_secret_rebuilds_and_restores_through_tapectl`.)
#[test]
fn the_escrow_key_rebuilds_the_catalog_and_is_kept_nowhere() {
    let mut tape = written_tape();
    let home = heir_home(&tape.escrow_public);
    let keydir = TempDir::new().unwrap();
    let escrow_key = key_file(keydir.path(), "escrow.key", &tape);
    let given = KeyFile::read_all(std::slice::from_ref(&escrow_key)).unwrap();

    let homes_publics: Vec<String> = home
        .conn
        .prepare("SELECT public_key FROM encryption_keys")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert!(
        !homes_publics.contains(&tape.operator_public),
        "the home's operator did not write the tape"
    );
    let key_rows_before = count(&home.conn, "SELECT COUNT(*) FROM encryption_keys");
    let keys_before = tree_listing(&home.paths.keys_dir);

    // Positive controls for the scans below.
    assert_eq!(
        files_holding(keydir.path(), &tape.escrow_secret),
        ["escrow.key"]
    );
    assert!(files_holding(&home.paths.home, &tape.escrow_secret).is_empty());

    let session = progress::start_session(
        Some(&home.paths.logs_dir),
        "catalog rebuild",
        Display::Off,
        false,
    );
    let report = rebuild_with(&home, &mut tape, &given).expect("the escrow key rebuilds");
    drop(session);
    assert!(report.volume_inserted, "{report:?}");
    assert!(
        report.key_is_escrow,
        "the key given is the registered escrow"
    );
    assert_eq!(report.units, 1);
    assert_eq!(
        home.conn
            .query_row(
                "SELECT t.name FROM units u JOIN tenants t ON t.id = u.tenant_id
                  WHERE u.name = 'keepsake'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        fixture::TENANT,
        "the unit is filed under the tenant that owns it"
    );

    assert_eq!(tree_listing(&home.paths.keys_dir), keys_before);
    assert_eq!(
        count(&home.conn, "SELECT COUNT(*) FROM encryption_keys"),
        key_rows_before,
        "no key was registered by the rebuild"
    );
    assert_eq!(
        home.conn
            .query_row(
                "SELECT public_key FROM encryption_keys WHERE is_escrow = 1",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        tape.escrow_public,
        "the registered escrow identity is the one init adopted, unreplaced (ADR-0005)"
    );
    home.conn
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    assert!(
        !tree_listing(&home.paths.logs_dir).is_empty(),
        "positive control: the session wrote a log to scan"
    );
    assert_eq!(
        files_holding(&home.paths.home, &tape.escrow_secret),
        Vec::<String>::new(),
        "the escrow secret is in no file of the home — catalog, logs, keys, staging"
    );
}

/// THE acceptance test of item 107: an heir with only the escrow secret
/// rebuilds the catalog and restores through tapectl, and the secret
/// goes nowhere.
///
/// ```text
/// tapectl init --operator heir --escrow-public-key age1...
/// tapectl catalog rebuild --from-volume --key escrow.key
/// tapectl restore unit --unit keepsake --key escrow.key --to DIR
/// ```
#[test]
fn an_heir_with_only_the_escrow_secret_rebuilds_and_restores_through_tapectl() {
    let mut tape = written_tape();
    let home = heir_home(&tape.escrow_public);
    let keydir = TempDir::new().unwrap();
    let escrow_key = key_file(keydir.path(), "escrow.key", &tape);
    let given = KeyFile::read_all(std::slice::from_ref(&escrow_key)).unwrap();

    let homes_publics: Vec<String> = home
        .conn
        .prepare("SELECT public_key FROM encryption_keys")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert!(
        !homes_publics.contains(&tape.operator_public),
        "the home's operator did not write the tape"
    );
    let key_rows_before = count(&home.conn, "SELECT COUNT(*) FROM encryption_keys");
    let keys_before = tree_listing(&home.paths.keys_dir);

    // Positive controls for the scans below: the secret IS in the key
    // file the heir passes, and the scan finds a secret that is planted.
    assert_eq!(
        files_holding(keydir.path(), &tape.escrow_secret),
        ["escrow.key"]
    );
    assert!(
        files_holding(&home.paths.home, &tape.escrow_secret).is_empty(),
        "the escrow secret is in no file of the heir's home before the commands"
    );

    let session = progress::start_session(
        Some(&home.paths.logs_dir),
        "catalog rebuild",
        Display::Off,
        false,
    );

    // `catalog rebuild --from-volume --key escrow.key`
    let report = rebuild_with(&home, &mut tape, &given).expect("the escrow key rebuilds");
    assert!(report.volume_inserted, "{report:?}");
    assert!(
        report.key_is_escrow,
        "the key given is the registered escrow"
    );
    assert_eq!(report.units, 1);
    assert_eq!(
        count(
            &home.conn,
            "SELECT COUNT(*) FROM units WHERE name = 'keepsake'"
        ),
        1
    );

    // Without `--key` the same restore fails with the wrong-key refusal
    // as it stands: the home holds keys, and none of them opens the tape.
    let none = TempDir::new().unwrap();
    let err = restore_with(&home, &mut tape, none.path(), &RestoreOptions::default())
        .expect_err("the home's keys open nothing on this tape")
        .to_string();
    assert!(
        err.contains(&age::DecryptError::NoMatchingKeys.to_string()),
        "{err}"
    );
    assert!(!err.contains("--key"), "no --key was given: {err}");
    assert!(
        !err.contains("INCOMPLETE") && err.contains("cannot be read around"),
        "a wrong key is refused at the first slice, not listed as damage: {err}"
    );

    // `restore unit --unit keepsake --key escrow.key --to DIR`
    let dest = TempDir::new().unwrap();
    let options = RestoreOptions {
        keys: given.clone(),
        ..RestoreOptions::default()
    };
    let restored = restore_with(&home, &mut tape, dest.path(), &options)
        .expect("the escrow key restores through tapectl");
    assert_eq!(restored.unit_name, UNIT);
    assert!(restored.slices >= 3);
    for (path, bytes) in &tape.files {
        assert_eq!(
            &std::fs::read(dest.path().join(path)).unwrap_or_default(),
            bytes,
            "{path} restored wrong"
        );
    }
    drop(session);

    // The key was used and kept nowhere: not in the home's keys/, not
    // registered, not in the catalog, not in the session log.
    assert_eq!(
        tree_listing(&home.paths.keys_dir),
        keys_before,
        "keys/ is untouched: nothing copied in, nothing rewritten"
    );
    assert_eq!(
        count(&home.conn, "SELECT COUNT(*) FROM encryption_keys"),
        key_rows_before,
        "no key was registered by either command"
    );
    assert_eq!(
        count(
            &home.conn,
            "SELECT COUNT(*) FROM encryption_keys WHERE is_escrow = 1"
        ),
        1
    );
    home.conn
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let logs = tree_listing(&home.paths.logs_dir);
    assert!(
        !logs.is_empty(),
        "positive control: the session wrote a log to scan"
    );
    assert_eq!(
        files_holding(&home.paths.home, &tape.escrow_secret),
        Vec::<String>::new(),
        "the escrow secret is in no file of the home — catalog, logs, keys, staging"
    );
}

/// `--key` repeated: the identities of every file are tried, in any
/// order, and a file that opens nothing beside the right one is no
/// obstacle — for the rebuild as for the restore.
#[test]
fn repeated_key_flags_all_count_for_the_rebuild_and_the_restore() {
    let mut tape = written_tape();
    let home = heir_home(&tape.escrow_public);
    let dir = TempDir::new().unwrap();
    let write = |name: &str, secret: &str| {
        let p = dir.path().join(name);
        std::fs::write(&p, format!("{secret}\n")).unwrap();
        p
    };
    let wrong = write("wrong.key", &generate_keypair().secret_key);
    let right = write("escrow.key", &tape.escrow_secret);
    let given = KeyFile::read_all(&[wrong, right]).unwrap();

    let report = rebuild_with(&home, &mut tape, &given).expect("wrong first, right second");
    assert!(report.key_is_escrow);
    let dest = TempDir::new().unwrap();
    let options = RestoreOptions {
        keys: given,
        ..RestoreOptions::default()
    };
    restore_with(&home, &mut tape, dest.path(), &options).expect("restore");
    for (path, bytes) in &tape.files {
        assert_eq!(&std::fs::read(dest.path().join(path)).unwrap(), bytes);
    }
}

/// The rebuild half of the test above, which needs no file table: a wrong
/// key file before the right one, and the right one first, both rebuild.
#[test]
fn repeated_key_flags_all_count_for_the_rebuild() {
    for wrong_first in [true, false] {
        let mut tape = written_tape();
        let home = heir_home(&tape.escrow_public);
        let dir = TempDir::new().unwrap();
        let write = |name: &str, secret: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, format!("{secret}\n")).unwrap();
            p
        };
        let wrong = write("wrong.key", &generate_keypair().secret_key);
        let right = write("escrow.key", &tape.escrow_secret);
        let order = if wrong_first {
            [wrong, right]
        } else {
            [right, wrong]
        };
        let given = KeyFile::read_all(&order).unwrap();
        let report = rebuild_with(&home, &mut tape, &given).expect("both orders rebuild");
        assert!(report.key_is_escrow, "wrong_first={wrong_first}");
        assert_eq!(report.units, 1);
    }
    // The control: a key file that opens nothing alone does not rebuild.
    let mut tape = written_tape();
    let home = heir_home(&tape.escrow_public);
    let dir = TempDir::new().unwrap();
    let only_wrong = dir.path().join("wrong.key");
    std::fs::write(&only_wrong, format!("{}\n", generate_keypair().secret_key)).unwrap();
    let given = KeyFile::read_all(&[only_wrong]).unwrap();
    assert!(
        rebuild_with(&home, &mut tape, &given).is_err(),
        "a key that opens nothing alone does not rebuild"
    );
    assert_eq!(count(&home.conn, "SELECT COUNT(*) FROM units"), 0);
}

const DEVICE: &str = "/nonexistent/tapectl-heir-nst";

/// The command line, parsed by the real clap tree.
fn parse(args: &[&str]) -> crate::cli::Commands {
    use clap::Parser;
    let mut argv = vec!["tapectl"];
    argv.extend_from_slice(args);
    crate::cli::Cli::try_parse_from(argv)
        .unwrap_or_else(|e| panic!("{args:?}: {e}"))
        .command
}

/// Run one parsed `catalog rebuild` or `restore ...` line against the
/// home, as `main` dispatches it.
fn dispatch(home: &Heir, line: &[&str]) -> Result<()> {
    match parse(line) {
        crate::cli::Commands::Catalog { command } => crate::cli::catalog::run_with_paths(
            &home.conn,
            &home.paths,
            cfg(),
            &command,
            false,
            false,
        ),
        crate::cli::Commands::Restore { command } => {
            crate::cli::restore::run(&home.conn, &home.paths, cfg(), &command, false, false)
        }
        other => panic!("not a command under test: {other:?}"),
    }
}

/// The heir's commands as the ADR types them, parsed by the real clap
/// tree and dispatched as `main` does, over a fake drive holding the
/// sealed tape: the flag, its spelling and its repetition reach the
/// restore. `restore unit`, `restore file` and `restore volume` each
/// restore with the escrow secret alone.
#[test]
fn the_commands_as_the_heir_types_them_restore_with_the_escrow_key_alone() {
    let tape = written_tape();
    let home = heir_home(&tape.escrow_public);
    let keydir = TempDir::new().unwrap();
    let escrow = keydir.path().join("escrow.key");
    std::fs::write(&escrow, format!("{}\n", tape.escrow_secret)).unwrap();
    let escrow = escrow.to_string_lossy().into_owned();
    let stranger = keydir.path().join("stranger.key");
    std::fs::write(&stranger, format!("{}\n", generate_keypair().secret_key)).unwrap();
    let stranger = stranger.to_string_lossy().into_owned();
    let fake = FakeTape::with_files(tape.store.files.clone(), fixture::BS);
    let _drive = InjectedDrive::install(&fake);

    // `--key` is required of the rebuild: a rebuild with no key is a
    // usage error naming it.
    {
        use clap::Parser;
        let err =
            crate::cli::Cli::try_parse_from(["tapectl", "catalog", "rebuild", "--from-volume"])
                .unwrap_err()
                .to_string();
        assert!(err.contains("--key"), "{err}");
    }

    dispatch(
        &home,
        &[
            "catalog",
            "rebuild",
            "--from-volume",
            "--device",
            DEVICE,
            "--key",
            &stranger,
            "--key",
            &escrow,
        ],
    )
    .expect("two --key flags, one of them the escrow secret, rebuild");
    assert_eq!(
        count(
            &home.conn,
            "SELECT COUNT(*) FROM units WHERE name = 'keepsake'"
        ),
        1
    );

    let check = |dest: &Path, files: &[&str]| {
        for (path, bytes) in tape
            .files
            .iter()
            .filter(|(p, _)| files.contains(&p.as_str()))
        {
            assert_eq!(
                &std::fs::read(dest.join(path)).unwrap_or_default(),
                bytes,
                "{path} restored wrong"
            );
        }
    };
    let all: Vec<&str> = tape.files.iter().map(|(p, _)| p.as_str()).collect();

    // Without `--key` each command is refused with the wrong-key
    // refusal as it stands; with it, each restores.
    let unit_dest = TempDir::new().unwrap();
    let unit_dest_s = unit_dest.path().to_string_lossy().into_owned();
    let unit_line = |extra: &[&str]| -> Vec<String> {
        let mut line: Vec<String> = [
            "restore", "unit", "--unit", UNIT, "--from", LABEL, "--device", DEVICE, "--to",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        line.push(unit_dest_s.clone());
        line.extend(extra.iter().map(|s| s.to_string()));
        line
    };
    let run_line = |line: Vec<String>| {
        let line: Vec<&str> = line.iter().map(String::as_str).collect();
        dispatch(&home, &line)
    };
    let err = run_line(unit_line(&[])).unwrap_err().to_string();
    assert!(
        err.contains(&age::DecryptError::NoMatchingKeys.to_string()),
        "{err}"
    );
    assert!(
        !err.contains("INCOMPLETE") && err.contains("cannot be read around"),
        "a wrong key is refused at the first slice, not listed as damage: {err}"
    );
    let err = run_line(unit_line(&["--key", &stranger]))
        .unwrap_err()
        .to_string();
    assert!(err.contains(&stranger), "the file given is named: {err}");
    // The refused tries left no residue in the destination.
    assert!(unit_dest.path().read_dir().unwrap().next().is_none());
    run_line(unit_line(&["--key", &stranger, "--key", &escrow]))
        .expect("restore unit --key stranger.key --key escrow.key");
    check(unit_dest.path(), &all);

    let file_dest = TempDir::new().unwrap();
    let file_dest_s = file_dest.path().to_string_lossy().into_owned();
    let file_line = |key: &[&str]| -> Vec<String> {
        let mut line: Vec<String> = [
            "restore", "file", "--unit", UNIT, "--file", "f-03.bin", "--from", LABEL, "--device",
            DEVICE, "--to",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        line.push(file_dest_s.clone());
        for k in key {
            line.push("--key".into());
            line.push((*k).into());
        }
        line
    };
    assert!(run_line(file_line(&[])).is_err(), "no key, no file");
    run_line(file_line(&[&escrow])).expect("restore file --key escrow.key");
    check(file_dest.path(), &["f-03.bin"]);

    let volume_dest = TempDir::new().unwrap();
    let volume_dest_s = volume_dest.path().to_string_lossy().into_owned();
    let volume_line = |key: &[&str]| -> Vec<String> {
        let mut line: Vec<String> = ["restore", "volume", LABEL, "--device", DEVICE, "--to"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        line.push(volume_dest_s.clone());
        for k in key {
            line.push("--key".into());
            line.push((*k).into());
        }
        line
    };
    assert!(run_line(volume_line(&[])).is_err(), "no key, no volume");
    // A failed `restore volume` leaves its unit directory behind; the
    // retry names it empty again.
    let _ = std::fs::remove_dir_all(volume_dest.path().join(UNIT));
    run_line(volume_line(&[&escrow])).expect("restore volume --key escrow.key");
    check(&volume_dest.path().join(UNIT), &all);
}

/// A `--key` that is not an identity file is refused by name, with the
/// reason, before anything else — before the device is resolved and the
/// drive opened — by each of the four commands.
#[test]
fn a_key_that_is_not_an_identity_is_refused_by_name_before_the_drive_opens() {
    let tape = written_tape();
    let home = heir_home(&tape.escrow_public);
    let dir = TempDir::new().unwrap();
    let public_only = dir.path().join("escrow.pub");
    std::fs::write(&public_only, format!("{}\n", tape.escrow_public)).unwrap();
    let junk = dir.path().join("junk.key");
    std::fs::write(&junk, "this is not a key\n").unwrap();
    let good = dir.path().join("escrow.key");
    std::fs::write(&good, format!("{}\n", tape.escrow_secret)).unwrap();
    let fake = FakeTape::with_files(tape.store.files.clone(), fixture::BS);
    let _drive = InjectedDrive::install(&fake);
    let dest = TempDir::new().unwrap();
    let dest_s = dest.path().join("out").to_string_lossy().into_owned();

    for bad in [&public_only, &junk, &dir.path().join("missing.key")] {
        let bad_s = bad.to_string_lossy().into_owned();
        let good_s = good.to_string_lossy().into_owned();
        let lines: Vec<Vec<&str>> = vec![
            vec![
                "catalog",
                "rebuild",
                "--from-volume",
                "--device",
                DEVICE,
                "--key",
                &good_s,
                "--key",
                &bad_s,
            ],
            vec![
                "restore", "unit", "--unit", UNIT, "--from", LABEL, "--device", DEVICE, "--to",
                &dest_s, "--key", &good_s, "--key", &bad_s,
            ],
            vec![
                "restore", "file", "--unit", UNIT, "--file", "f-01.bin", "--from", LABEL,
                "--device", DEVICE, "--to", &dest_s, "--key", &bad_s, "--key", &good_s,
            ],
            vec![
                "restore", "volume", LABEL, "--device", DEVICE, "--to", &dest_s, "--key", &bad_s,
            ],
        ];
        for line in lines {
            let err = dispatch(&home, &line)
                .expect_err("a bad key file is refused")
                .to_string();
            assert!(err.contains(&bad_s), "{line:?}: the file is named: {err}");
            assert!(
                err.contains("is not an age X25519 identity file") || err.contains("cannot read"),
                "{line:?}: {err}"
            );
            assert!(
                !err.contains(&tape.escrow_secret),
                "a refusal never echoes a secret: {err}"
            );
            assert!(
                fake.opens().is_empty(),
                "{line:?}: the drive was opened before the key was checked"
            );
        }
    }
    assert_eq!(
        count(&home.conn, "SELECT COUNT(*) FROM volumes"),
        0,
        "nothing was rebuilt"
    );
    assert!(!dest.path().join("out").exists());
}
