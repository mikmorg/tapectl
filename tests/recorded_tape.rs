//! The recorded tape (#552, plan T6.4): a small tape written by tapectl 2.0.0 on an mhvtl
//! drive, kept under `tests/fixtures/recorded-2.0.0/`, which every later build must read.
//!
//! WHAT IT HOLDS. Sixteen tape files, File 0 to the seal, each as the heir reads it (block
//! padded to 524,288 bytes): two tenants, the operator and the escrow recipient; a unit of
//! three slices whose root has mode 0750 and which has a nested folder, an empty file, an
//! empty folder, a name with a non-ASCII letter, a hard link and a symbolic link; and a zstd
//! unit and a gzip unit (each unit's own `.tapectl-unit.toml` is a member too). It was written through the real staging path (`stage create`: GNU
//! tar, the cut at member starts, each slice compressed alone, age) and `volume write`.
//! `RECORDED.md` names the tapectl commit, the tar that made it (GNU tar 1.35), the drive
//! emulation and the command line; `EXPECTED.toml` says what the tape holds, made from the
//! source trees alone; `keys/` holds test keys made for this fixture alone.
//!
//! WHAT IT PROVES, with no device and no environment variable: the candidate's catalog
//! rebuild from the tape matches EXPECTED; `restore unit` of every unit gives every path
//! with its recorded kind, size and sha256 and the root's mode; `volume verify` passes;
//! and the tape's OWN RESTORE.sh (File 2 of the fixture, never the tree's) restores every
//! unit under the heir harness, `mt` and `dd` standing in for the drive and serving the
//! fixture's tape files.
//!
//! NOT COVERED HERE: `restore file` (issue #552 names it). tapectl's one-file restore has no
//! seam over a store: `restore_file` opens the drive and `RestoreTarget` is crate-private,
//! so an integration test cannot hand it the fixture's files. The seam would be a
//! `restore_file_from_store_for_tests` beside `restore_unit_from_store_for_tests` in
//! `src/volume/restore.rs`, outside this task's fence; until it lands, the one-file
//! restore is covered here only by the tape's own RESTORE.sh for whole units.
//!
//! THE RULE. This fixture is never re-recorded to make a test pass. A later build that
//! cannot read it has broken the format, and that is the CTO's decision, as a change to a
//! byte pin (`tests/on_tape_golden.rs`) is. A failure here names the fixture file it
//! concerns; the answer is to find what the build changed, never to record again.

#[path = "common/heir/mod.rs"]
mod heir;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use tapectl::config::{Config, TapectlPaths};
use tapectl::crypto::keys::KeyFile;
use tapectl::db;
use tapectl::store::{MemStore, Tier};
use tapectl::tape::contact::{ContactSite, Medium, Operation};
use tapectl::volume::rebuild;
use tapectl::volume::restore::{restore_unit_from_store_for_tests, RestoreOptions};
use tapectl::volume::write::volume_verify_with_store;

use heir::Heir;

/// The block the tape was written and read at (File 1's recipe: `mt setblk 524288`).
const BLOCK: usize = 524_288;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/recorded-2.0.0")
}

#[derive(Deserialize)]
struct Expected {
    tape: TapeE,
    unit: Vec<UnitE>,
}

#[derive(Deserialize)]
struct TapeE {
    label: String,
    tenants: Vec<String>,
}

#[derive(Deserialize)]
struct UnitE {
    name: String,
    tenant: String,
    version: i64,
    compression: String,
    root_mode: u32,
    path: Vec<PathE>,
}

#[derive(Deserialize)]
struct PathE {
    path: String,
    kind: String,
    size: u64,
    sha256: Option<String>,
    target: Option<String>,
    hardlink_of: Option<String>,
}

fn expected() -> Expected {
    let p = fixture_dir().join("EXPECTED.toml");
    let text = fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// Every tape file of the fixture, in order, as recorded.
fn tape_files() -> Vec<Vec<u8>> {
    let mut files = Vec::new();
    loop {
        let p = fixture_dir().join(format!("file-{:03}", files.len()));
        if !p.exists() {
            break;
        }
        let bytes = fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        assert!(
            !bytes.is_empty() && bytes.len().is_multiple_of(BLOCK),
            "{}: a tape file as the heir reads it is whole {BLOCK}-byte blocks, not {} bytes",
            p.display(),
            bytes.len()
        );
        files.push(bytes);
    }
    assert!(
        files.len() >= 5,
        "{}: the recorded tape's files are missing (found {})",
        fixture_dir().display(),
        files.len()
    );
    files
}

/// The fixture's tape files in a store, as a drive at the beginning of the medium.
fn store() -> MemStore {
    let mut store = MemStore::new(BLOCK);
    let files = tape_files();
    store.syncs = vec![true; files.len()];
    store.files = files;
    store
}

fn key_path(name: &str) -> PathBuf {
    let p = fixture_dir().join("keys").join(format!("{name}.key"));
    assert!(p.is_file(), "{}: the fixture's key is missing", p.display());
    p
}

fn key_file(name: &str) -> KeyFile {
    KeyFile::read(&key_path(name)).unwrap_or_else(|e| panic!("{}: {e}", key_path(name).display()))
}

/// A fresh home with an open catalog.
struct Home {
    _tmp: tempfile::TempDir,
    paths: TapectlPaths,
    config: Config,
    conn: rusqlite::Connection,
}

fn fresh_home() -> Home {
    let tmp = tempfile::tempdir().unwrap();
    let paths = TapectlPaths::new(tmp.path().join("home"));
    paths.ensure_dirs().unwrap();
    let conn = db::open(&paths.db_file).unwrap();
    let mut config = Config::default();
    let staging = tmp.path().join("staging");
    fs::create_dir_all(&staging).unwrap();
    config.staging.directory = staging.to_string_lossy().into_owned();
    Home {
        _tmp: tmp,
        paths,
        config,
        conn,
    }
}

/// The catalog rebuilt from the recorded tape with the operator's key, as `catalog rebuild`
/// does it, over the fixture's files.
fn rebuilt() -> (Home, MemStore, rebuild::RebuildReport) {
    let home = fresh_home();
    let mut tape = store();
    let identities = KeyFile::all_identities(&[key_file("operator")]);
    let scratch = tempfile::tempdir().unwrap();
    let want = expected();
    let report = rebuild::rebuild_from_store(
        &home.conn,
        &mut tape,
        &identities,
        Some(&want.tape.label),
        "recovered",
        Some("recorded"),
        scratch.path(),
        "recorded-2.0.0",
        ContactSite::new(
            &home.config,
            Operation::CatalogRebuild,
            "recorded-2.0.0",
            Medium::NoBackend,
        ),
    )
    .unwrap_or_else(|e| {
        panic!(
            "{}: catalog rebuild from the recorded tape failed: {e}",
            fixture_dir().display()
        )
    });
    (home, tape, report)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Every entry under `root` (not the root itself): relative path, as lstat sees it.
fn walk(root: &Path) -> BTreeMap<String, PathBuf> {
    fn go(base: &Path, dir: &Path, out: &mut BTreeMap<String, PathBuf>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let rel = p
                .strip_prefix(base)
                .unwrap()
                .to_str()
                .expect("a restored name is UTF-8 here")
                .to_string();
            out.insert(rel, p.clone());
            if fs::symlink_metadata(&p).unwrap().is_dir() {
                go(base, &p, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    go(root, root, &mut out);
    out
}

/// What a restore of `unit` into `dir` must hold: every path with its recorded kind, size,
/// sha256 and target, a hard link's second name the same inode as the first, and the
/// root's mode. `what` names the source of the restore in a failure.
fn assert_unit_restored(unit: &UnitE, dir: &Path, what: &str) {
    let fixture = fixture_dir().join("EXPECTED.toml");
    // Every entry, the unit's `.tapectl-unit.toml` (a member of the tape) among them.
    let on_disk = walk(dir);
    assert!(
        !on_disk.contains_key(".tapectl-damaged-files.txt"),
        "{} ({what}): unit {} came back with a damaged-files list",
        fixture.display(),
        unit.name
    );
    let want: Vec<&str> = unit.path.iter().map(|p| p.path.as_str()).collect();
    let got: Vec<&str> = on_disk.keys().map(String::as_str).collect();
    let mut want_sorted = want.clone();
    want_sorted.sort_unstable();
    assert_eq!(
        got,
        want_sorted,
        "{} ({what}): unit {} restored the wrong set of paths",
        fixture.display(),
        unit.name
    );
    for e in &unit.path {
        let p = &on_disk[&e.path];
        let meta = fs::symlink_metadata(p).unwrap();
        let ctx = format!("{} ({what}): {} {}", fixture.display(), unit.name, e.path);
        match e.kind.as_str() {
            "dir" => assert!(meta.is_dir(), "{ctx}: expected a folder"),
            "symlink" => {
                assert!(
                    meta.file_type().is_symlink(),
                    "{ctx}: expected a symbolic link"
                );
                let target = fs::read_link(p).unwrap();
                assert_eq!(
                    Some(target.to_str().unwrap()),
                    e.target.as_deref(),
                    "{ctx}: link target"
                );
            }
            "file" => {
                assert!(meta.is_file(), "{ctx}: expected a regular file");
                assert_eq!(meta.len(), e.size, "{ctx}: size");
                let bytes = fs::read(p).unwrap();
                assert_eq!(
                    Some(sha256_hex(&bytes).as_str()),
                    e.sha256.as_deref(),
                    "{ctx}: sha256"
                );
                if let Some(first) = &e.hardlink_of {
                    let other = fs::symlink_metadata(&on_disk[first]).unwrap();
                    assert_eq!(
                        (meta.dev(), meta.ino()),
                        (other.dev(), other.ino()),
                        "{ctx}: a hard link of {first}"
                    );
                }
            }
            other => panic!("{ctx}: EXPECTED.toml names the kind {other:?}"),
        }
    }
    let root = fs::symlink_metadata(dir).unwrap();
    assert_eq!(
        root.permissions().mode() & 0o7777,
        unit.root_mode,
        "{} ({what}): unit {}'s root mode",
        fixture.display(),
        unit.name
    );
}

/// A unit's rebuilt file rows at `version`: path -> (kind name, size, sha256 hex).
fn file_rows(
    conn: &rusqlite::Connection,
    unit: &str,
    version: i64,
) -> BTreeMap<String, (String, u64, Option<String>)> {
    let mut stmt = conn
        .prepare(
            "SELECT p.path, fv.kind, fv.size_bytes, fv.sha256
               FROM file_versions fv
               JOIN paths p ON p.id = fv.path_id
               JOIN snapshots s ON s.id = fv.snapshot_id
               JOIN units u ON u.id = s.unit_id
              WHERE u.name = ?1 AND s.version = ?2",
        )
        .unwrap();
    stmt.query_map(rusqlite::params![unit, version], |r| {
        let kind: i64 = r.get(1)?;
        let sha: Option<Vec<u8>> = r.get(3)?;
        Ok((
            r.get::<_, String>(0)?,
            (
                tapectl::db::files::FileKind::from_code(kind)
                    .unwrap()
                    .as_str()
                    .to_string(),
                r.get::<_, i64>(2)? as u64,
                sha.map(|b| b.iter().map(|x| format!("{x:02x}")).collect()),
            ),
        ))
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

fn key_name_of(tenant: &str) -> &'static str {
    match tenant {
        "rec-alpha" => "tenant-alpha",
        "rec-bravo" => "tenant-bravo",
        other => panic!("{}: no key for tenant {other}", fixture_dir().display()),
    }
}

// ---- the fixture itself ----

/// The fixture says what made it: the tapectl, the GNU tar (the test names it), the drive.
#[test]
fn recorded_md_names_the_tar_that_made_the_tape() {
    let p = fixture_dir().join("RECORDED.md");
    let text = fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let tar = text
        .lines()
        .find_map(|l| l.strip_prefix("- GNU tar that made the tape: "))
        .unwrap_or_else(|| panic!("{}: no tar version line", p.display()));
    assert!(
        tar.starts_with("tar (GNU tar) "),
        "{}: the tar that made the tape is not GNU tar: {tar}",
        p.display()
    );
    eprintln!("the recorded tape was made with {tar}");
    for need in ["- tapectl: tapectl 2.0.0", "- drive: ", "- command line: "] {
        assert!(
            text.lines().any(|l| l.starts_with(need)),
            "{}: no line {need:?}",
            p.display()
        );
    }
}

// ---- tapectl reads it ----

/// `catalog rebuild` from the tape gives the catalog EXPECTED describes: the volume, the
/// tenants, every unit under its tenant at its version and with its compression, and each
/// unit's file rows (every path with its kind, size and sha256, and no other).
#[test]
fn catalog_rebuild_from_the_recorded_tape_matches_expected() {
    let want = expected();
    let (home, _tape, report) = rebuilt();
    let ctx = fixture_dir().display().to_string();
    assert_eq!(report.label, want.tape.label, "{ctx}");
    assert_eq!(report.units, want.unit.len(), "{ctx}: units rebuilt");
    assert_eq!(
        report.positions,
        // the slices the tape holds: three, one, one
        5,
        "{ctx}: slice positions rebuilt"
    );

    // Tenants: the two the tape names, and no others.
    let tenants: Vec<String> = home
        .conn
        .prepare("SELECT name FROM tenants ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let mut names = want.tape.tenants.clone();
    names.sort();
    assert_eq!(tenants, names, "{ctx}: the tenants rebuilt");

    // Units: each under its tenant, at its version.
    for u in &want.unit {
        let (tenant, version): (String, i64) = home
            .conn
            .query_row(
                "SELECT t.name, MAX(s.version) FROM units u
                   JOIN tenants t ON t.id = u.tenant_id
                   JOIN snapshots s ON s.unit_id = u.id
                  WHERE u.name = ?1",
                [&u.name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or_else(|e| panic!("{ctx}: unit {} not rebuilt: {e}", u.name));
        assert_eq!(tenant, u.tenant, "{ctx}: unit {}'s tenant", u.name);
        assert_eq!(version, u.version, "{ctx}: unit {}'s version", u.name);
        let compression: Option<String> = home
            .conn
            .query_row(
                "SELECT ss.compression FROM stage_sets ss
                   JOIN snapshots s ON s.id = ss.snapshot_id
                   JOIN units u ON u.id = s.unit_id
                  WHERE u.name = ?1 AND s.version = ?2",
                rusqlite::params![&u.name, u.version],
                |r| r.get(0),
            )
            .unwrap_or_else(|e| panic!("{ctx}: unit {}'s stage set not rebuilt: {e}", u.name));
        let rows = file_rows(&home.conn, &u.name, u.version);
        let want_rows: BTreeMap<String, (String, u64, Option<String>)> = u
            .path
            .iter()
            .map(|e| {
                let kind = match e.kind.as_str() {
                    "file" => "regular",
                    other => other,
                };
                (e.path.clone(), (kind.to_string(), e.size, e.sha256.clone()))
            })
            .collect();
        assert_eq!(
            rows.keys().collect::<Vec<_>>(),
            want_rows.keys().collect::<Vec<_>>(),
            "{ctx}: unit {}'s rebuilt file rows name other paths than EXPECTED",
            u.name
        );
        for (path, got) in &rows {
            assert_eq!(got, &want_rows[path], "{ctx}: unit {} row {path}", u.name);
        }
        assert_eq!(
            compression.as_deref().unwrap_or("none"),
            u.compression,
            "{ctx}: unit {}'s compression",
            u.name
        );
    }
}

/// `restore unit` of every unit, over the fixture's files with the catalog rebuilt from them,
/// gives every path with its recorded kind, size and sha256, and the root's mode.
#[test]
fn restore_unit_of_every_recorded_unit_gives_what_expected_says() {
    let want = expected();
    let (home, mut tape, _report) = rebuilt();
    for u in &want.unit {
        let dest = tempfile::tempdir().unwrap();
        let dir = dest.path().join(&u.name);
        fs::create_dir_all(&dir).unwrap();
        let options = RestoreOptions {
            keys: vec![key_file(key_name_of(&u.tenant))],
            ..RestoreOptions::default()
        };
        let report = restore_unit_from_store_for_tests(
            &home.conn,
            &home.paths,
            &home.config,
            &u.name,
            &want.tape.label,
            u.version,
            &dir,
            &options,
            &mut tape,
        )
        .unwrap_or_else(|e| {
            panic!(
                "{}: restore unit {} failed: {e}",
                fixture_dir().display(),
                u.name
            )
        });
        assert!(
            report.success,
            "{}: restore unit {} did not succeed: {:?}",
            fixture_dir().display(),
            u.name,
            report.tar_said
        );
        assert_unit_restored(u, &dir, "tapectl restore unit");
    }
}

/// `volume verify` (the full tier) passes on the recorded tape: every file reads and matches.
#[test]
fn volume_verify_passes_on_the_recorded_tape() {
    let want = expected();
    let (home, mut tape, _report) = rebuilt();
    let volume_id: i64 = home
        .conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            [&want.tape.label],
            |r| r.get(0),
        )
        .unwrap_or_else(|e| {
            panic!(
                "{}: the volume was not rebuilt: {e}",
                fixture_dir().display()
            )
        });
    let report = volume_verify_with_store(
        &home.conn,
        &mut tape,
        &want.tape.label,
        volume_id,
        BLOCK,
        Tier::Integrity,
        ContactSite::new(
            &home.config,
            Operation::VolumeVerify,
            "recorded-2.0.0",
            Medium::NoBackend,
        ),
    )
    .unwrap_or_else(|e| panic!("{}: volume verify failed: {e}", fixture_dir().display()));
    assert_eq!(
        report.failed,
        0,
        "{}: volume verify found mismatches: {:?}",
        fixture_dir().display(),
        report.mismatches
    );
    assert!(
        report.checked > 0 && report.passed == report.checked,
        "{}: verify checked {} and passed {}",
        fixture_dir().display(),
        report.checked,
        report.passed
    );
}

// ---- the heir reads it ----

/// The tape's OWN RESTORE.sh (File 2 of the fixture, not the tree's), run under the heir
/// harness with the fixture's tape files served as `FAKE_FILES`, restores every unit; the
/// result is checked against EXPECTED as tapectl's restore is.
#[test]
fn the_recorded_tapes_own_restore_sh_restores_every_unit() {
    let want = expected();
    let files = tape_files();
    let script_file = fixture_dir().join("file-002");
    // The script as the heir reads it: the padding after its last byte is zeros.
    let mut script = files[2].clone();
    while script.last() == Some(&0) {
        script.pop();
    }
    assert!(
        script.starts_with(b"#!"),
        "{}: File 2 of the recorded tape is not the script",
        script_file.display()
    );
    let keys: Vec<(&'static str, PathBuf)> = ["tenant-alpha", "tenant-bravo", "operator", "escrow"]
        .iter()
        .map(|n| (*n, key_path(n)))
        .collect();
    let key_refs: Vec<(&'static str, &Path)> =
        keys.iter().map(|(n, p)| (*n, p.as_path())).collect();
    let h = Heir::recorded(&files, &script, &want.tape.label, &key_refs);
    for u in &want.unit {
        let dest = h.dest(&format!("out-{}", u.name));
        let key = h.keys[key_name_of(&u.tenant)]
            .to_string_lossy()
            .into_owned();
        let out = h.run(&[
            "--restore",
            "--key",
            &key,
            "--to",
            dest.to_str().unwrap(),
            "--unit",
            &u.name,
        ]);
        assert_eq!(
            out.code,
            0,
            "{}: the tape's RESTORE.sh failed for {}: {}",
            script_file.display(),
            u.name,
            out.all()
        );
        assert_unit_restored(u, &dest, "the tape's own RESTORE.sh");
    }
}
