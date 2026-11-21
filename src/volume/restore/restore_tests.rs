//! Tests of tapectl's restores: the `restores` row, the refusals made before
//! the tape is touched, the space check, the one forward pass over the tape,
//! and what a damaged slice does to a restore (ported onto the tar reader in
//! T4.8).
//!
//! Every unit is staged by the real `stage create` and every tape written by
//! the real `volume write` ([`super::fixture::Home`]). Where a test asks what
//! the drive was made to do, the written medium is replayed into a
//! [`FakeTape`] behind an injected drive, and the real `restore_unit` /
//! `restore_file` run over it.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use rusqlite::Connection;
use tempfile::TempDir;

use super::fixture::{
    assert_restored, copy_of, listing, noise, Home, Item, What, BS, DEVICE, TENANT,
};
use super::{
    check_restore_space, existing, load_identities, plan_restore, restore_file, restore_raw_volume,
    restore_unit, scratch_dir, select_write_positions, space_needs, RestoreFreeOverride,
    RestoreOptions, RestorePlan, RestoreReport, RestoreTarget, SCRATCH_NAME,
};
use crate::error::Result;
use crate::store::injected::InjectedDrive;
use crate::store::{MemStore, OpenMode};
use crate::tape::contact::{ContactSite, Medium, Operation};
use crate::tape::fake::{FakeTape, Op};
use crate::volume::restore_record::{rows, RestoreRow};

const UNIT: &str = "rr-unit";

// ── helpers ──

/// Two small files and a folder: the unit most of these tests restore.
fn two_files() -> Vec<Item> {
    vec![
        Item::file("a.txt", b"alpha".to_vec()),
        Item::file("b.txt", b"bravo".to_vec()),
        Item::Dir("docs".into()),
    ]
}

/// A big file, a mid one and two small ones (one in a folder): several
/// slices at 64K.
fn tree() -> Vec<Item> {
    vec![
        Item::file("big.bin", noise(300_000, 1)),
        Item::file("mid.bin", noise(150_000, 2)),
        Item::file("small.txt", b"small".to_vec()),
        Item::file("sub/deep.txt", b"deep".to_vec()),
    ]
}

/// A home with `UNIT` of `items` staged and written to the volume `label`.
fn written(items: &[Item], slice_size: &str, label: &str) -> Home {
    let mut h = Home::new(slice_size, None, 1);
    h.unit(TENANT, UNIT, items);
    h.stage(UNIT);
    h.volume(label);
    h.write(label);
    h
}

fn only_row(conn: &Connection) -> RestoreRow {
    let mut all = rows(conn).unwrap();
    assert_eq!(all.len(), 1, "one restore is one row: {all:?}");
    all.remove(0)
}

fn id_of(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// `(operation, outcome)` of contact `id`, asserted BY VALUE.
fn contact(conn: &Connection, id: i64) -> (String, Option<String>) {
    conn.query_row(
        "SELECT operation, outcome FROM cartridge_contacts WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn plan_of(h: &Home, label: &str, target: RestoreTarget<'_>) -> RestorePlan {
    let selection = select_write_positions(&h.conn, UNIT, label, None).unwrap();
    plan_restore(&h.conn, &selection, target).unwrap()
}

/// What the catalog holds that a refusal before the tape must leave alone.
#[derive(Debug, PartialEq)]
struct Counts {
    contacts: i64,
    journal: i64,
    restores: i64,
}

fn counts(conn: &Connection) -> Counts {
    Counts {
        contacts: id_of(conn, "SELECT COUNT(*) FROM cartridge_contacts"),
        journal: id_of(conn, "SELECT COUNT(*) FROM mam_journal"),
        restores: id_of(conn, "SELECT COUNT(*) FROM restores"),
    }
}

/// A written home, its tape behind an injected drive, so the REAL
/// `restore_unit` / `restore_file` run end to end and the fake says whether,
/// and how, the drive was opened and what was read.
struct Rig {
    h: Home,
    fake: FakeTape,
    _drive: InjectedDrive,
    before: Counts,
}

impl Rig {
    fn new(h: Home, label: &str) -> Rig {
        let fake = FakeTape::with_files(h.tapes[label].files.clone(), BS);
        let drive = InjectedDrive::install(&fake);
        let before = counts(&h.conn);
        Rig {
            h,
            fake,
            _drive: drive,
            before,
        }
    }

    fn unit(&self, label: &str, dest: &Path, options: &RestoreOptions) -> Result<RestoreReport> {
        restore_unit(
            &self.h.conn,
            &self.h.paths,
            &self.h.config,
            UNIT,
            label,
            &dest.to_string_lossy(),
            DEVICE,
            BS,
            None,
            false,
            options,
        )
    }

    fn dry_unit(&self, label: &str, dest: &Path) -> Result<RestoreReport> {
        restore_unit(
            &self.h.conn,
            &self.h.paths,
            &self.h.config,
            UNIT,
            label,
            &dest.to_string_lossy(),
            DEVICE,
            BS,
            None,
            true,
            &RestoreOptions::default(),
        )
    }

    fn file(
        &self,
        label: &str,
        path: &str,
        dest: &Path,
        options: &RestoreOptions,
    ) -> Result<RestoreReport> {
        restore_file(
            &self.h.conn,
            &self.h.paths,
            &self.h.config,
            UNIT,
            path,
            label,
            &dest.to_string_lossy(),
            DEVICE,
            BS,
            None,
            options,
        )
    }

    /// A refusal before contact: the drive never opened, and so no contact,
    /// MAM journal or `restores` row was made.
    fn assert_no_tape_contact(&self) {
        assert_eq!(self.fake.opens(), vec![], "the drive was opened");
        assert_eq!(counts(&self.h.conn), self.before, "a contact was made");
    }

    /// The slice reads the tape saw, as tape file numbers.
    fn reads(&self) -> Vec<u32> {
        self.fake
            .ops()
            .iter()
            .filter_map(|op| match op {
                Op::Read(n) => Some(*n),
                _ => None,
            })
            .collect()
    }
}

/// The paths an INCOMPLETE restore lists as damaged or missing.
fn lost_paths(err: &str) -> BTreeSet<String> {
    err.lines()
        .skip_while(|l| !l.contains("file(s) damaged or missing:"))
        .skip(1)
        .take_while(|l| l.starts_with("    ") && !l.starts_with("     "))
        .filter_map(|l| l.trim().rsplit_once(" (").map(|(p, _)| p.to_string()))
        .collect()
}

/// The paths of the members the file table says touch slice `n`.
fn members_touching(h: &Home, slice_size: u64, n: u64) -> BTreeSet<String> {
    h.rows(UNIT)
        .into_iter()
        .filter(|r| r.slices(slice_size).contains(&n))
        .map(|r| r.path)
        .collect()
}

/// Every restored file but the `lost` ones is in `dest` byte for byte, and
/// no lost regular file is there at all.
fn assert_all_but(h: &Home, dest: &Path, lost: &BTreeSet<String>) {
    let want = listing(&h.source(UNIT));
    let got = listing(dest);
    for (path, what) in &want {
        if lost.contains(path) {
            if what.starts_with("file ") {
                assert!(
                    !got.contains_key(path),
                    "{path} was listed lost, yet it is in the destination"
                );
            }
        } else {
            assert_eq!(got.get(path), Some(what), "{path} must be restored whole");
        }
    }
}

/// The byte of the age file `position` of `label` that is in the payload
/// nonce: a landmark found in the bytes, never an absolute offset (age puts a
/// grease stanza of random length before the MAC line, so the header's end
/// moves from one encryption to the next, #484).
fn nonce_offset(h: &Home, label: &str, position: usize) -> usize {
    let file = &h.tapes[label].files[position];
    // The MAC line is the only one that starts with `---`.
    let mac = file
        .windows(5)
        .position(|w| w == b"\n--- ")
        .expect("an age header's MAC line")
        + 5;
    // The payload nonce follows the MAC's 43 characters and its newline.
    mac + 43 + 1 + 3
}

/// A character of the MAC line itself: age refuses the header.
fn header_mac_offset(h: &Home, label: &str, position: usize) -> usize {
    let file = &h.tapes[label].files[position];
    file.windows(5)
        .position(|w| w == b"\n--- ")
        .expect("an age header's MAC line")
        + 5
        + 5
}

// ── issue #306: the RESTORE's own record ──

/// The success row, and the POSITIVE CONTROL for every failure test below: a
/// clean restore records a row too, with tar's report present and non-empty
/// and the counts measured, so these tests distinguish "records restores"
/// from "records failures".
#[test]
fn a_clean_restore_unit_records_one_ok_row_with_tar_report_and_counts() {
    let h = written(&two_files(), "64K", "RR-OK");
    let mut bay = h.bay("RR-OK", &[]);
    let (result, dest) = h.restore(
        UNIT,
        What::Unit,
        "RR-OK",
        &RestoreOptions::default(),
        &mut bay,
    );
    let report = result.expect("a clean restore");
    assert_restored(&h, UNIT, dest.path(), &[]);
    let dest_str = dest.path().to_string_lossy().to_string();

    let r = only_row(&h.conn);
    assert_eq!(r.kind, "unit");
    assert_eq!(r.outcome, "ok");
    assert_eq!(r.error, None);
    let contact_id = r.contact_id.expect("the contact is the spine");
    assert_eq!(
        contact(&h.conn, contact_id),
        ("restore unit".to_string(), Some("ok".to_string()))
    );
    assert_eq!(
        r.volume_id,
        Some(id_of(
            &h.conn,
            "SELECT id FROM volumes WHERE label = 'RR-OK'"
        ))
    );
    assert_eq!(r.volume_label.as_deref(), Some("RR-OK"));
    assert_eq!(
        r.unit_id,
        Some(id_of(
            &h.conn,
            "SELECT id FROM units WHERE name = 'rr-unit'"
        ))
    );
    assert_eq!(r.unit_name.as_deref(), Some(UNIT));
    assert_eq!(r.version, Some(1));
    assert_eq!(r.file_path, None);
    assert_eq!(r.destination, dest_str);
    assert!(r.finished_at >= r.started_at);
    assert_eq!(r.slices_read, Some(report.slices as i64));
    assert!(report.slices >= 1, "positive control: slices were read");
    let plain: i64 = id_of(&h.conn, "SELECT SUM(size_bytes) FROM stage_slices");
    assert_eq!(
        r.bytes_restored,
        Some(plain),
        "the slices' plaintext, measured"
    );
    assert_eq!(
        r.files_restored,
        Some(h.rows(UNIT).len() as i64),
        "every member of the file table"
    );
    assert_eq!(r.tar_exit_code, Some(0));
    let argv = r.tar_argv.as_deref().expect("tar ran: its argv is kept");
    assert!(
        argv.starts_with(&format!(r#"["{}""#, h.config.tar.binary)),
        "{argv}"
    );
    assert!(argv.contains(r#""--extract""#), "{argv}");
    assert!(r.tar_stdout.is_some(), "tar's stdout kept, even if empty");
    assert!(r.tar_stderr.is_some(), "tar's stderr kept, even if empty");
    let version = r.tar_version.expect("tar ran, so its version was read");
    assert!(
        version.to_lowercase().contains("tar"),
        "tar's first version line: {version}"
    );
    assert_eq!(r.tapectl_version, crate::build_info::VERSION);
}

/// Issue #350(d) at the restore path: tenant `T` must not decrypt with
/// tenant `T-old`'s key just because `T-` prefixes its file name. Positive
/// control first: with no key rows (the `catalog rebuild` case) the file name
/// cannot say whose it is, so `T` is given it and nothing is withheld that
/// cannot be proven another tenant's. Then, with the catalog's rows, the same
/// loader gives `T` its own and the operator's, and not the old tenant's.
#[test]
fn restore_loads_only_the_keys_the_catalog_says_are_the_tenants() {
    let h = Home::new("64K", None, 1);
    let old = format!("{TENANT}-old");
    crate::tenant::add_tenant(&h.conn, &h.paths, &old, None, false).unwrap();
    let tenant = crate::db::queries::get_tenant_by_name(&h.conn, TENANT)
        .unwrap()
        .unwrap();
    let publics = |tenant_name: &str| -> BTreeSet<String> {
        fs::read_dir(&h.paths.keys_dir)
            .unwrap()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let stem = name.strip_suffix(".age.pub")?.to_string();
                // A public key with no secret beside it (the escrow
                // recipient) opens nothing here.
                if !h.paths.keys_dir.join(format!("{stem}.age.key")).exists() {
                    return None;
                }
                let owner = id_of_owner(&h.conn, &stem)?;
                (owner == tenant_name).then(|| fs::read_to_string(e.path()).unwrap().trim().into())
            })
            .collect()
    };
    let loaded = |h: &Home| -> BTreeSet<String> {
        load_identities(&h.conn, &h.paths, &tenant, &[])
            .unwrap()
            .iter()
            .map(|i| i.to_public().to_string())
            .collect()
    };
    let (own, operator, others) = (
        publics(TENANT),
        publics("quince-operator-main"),
        publics(&old),
    );
    assert!(!own.is_empty() && !operator.is_empty() && !others.is_empty());

    // With the catalog's rows: its own and the operator's, nothing of the
    // old tenant's.
    let got = loaded(&h);
    assert!(
        own.is_subset(&got) && operator.is_subset(&got),
        "own {own:?} operator {operator:?} got {got:?}"
    );
    assert!(
        got.is_disjoint(&others),
        "the old tenant's key was loaded: {got:?}"
    );

    // Control: with no key rows the name cannot say whose a file is, so the
    // prefix rule gives the tenant every file it might own.
    h.conn.execute("DELETE FROM encryption_keys", []).unwrap();
    let got = loaded(&h);
    assert!(
        !got.is_disjoint(&others),
        "control: with no key row t may own t-old's key: {got:?}"
    );
}

/// The tenant that owns the key alias `alias`, in the catalog.
fn id_of_owner(conn: &Connection, alias: &str) -> Option<String> {
    conn.query_row(
        "SELECT t.name FROM encryption_keys k JOIN tenants t ON t.id = k.tenant_id
          WHERE k.alias = ?1",
        [alias],
        |r| r.get(0),
    )
    .ok()
}

/// `restore file` is ONE row of kind `file` under `restore unit`'s one
/// contact, naming the directory the operator gave: never the scratch
/// directory the file was extracted into.
#[test]
fn a_restore_file_records_one_file_row_naming_the_operators_destination() {
    let h = written(&two_files(), "64K", "RF-OK");
    let mut bay = h.bay("RF-OK", &[]);
    let (result, dest) = h.restore(
        UNIT,
        What::File("b.txt"),
        "RF-OK",
        &RestoreOptions::default(),
        &mut bay,
    );
    result.expect("a clean file restore");
    assert_eq!(fs::read(dest.path().join("b.txt")).unwrap(), b"bravo");
    assert_eq!(names_in(dest.path()), ["b.txt"], "only the one file lands");

    let r = only_row(&h.conn);
    assert_eq!(r.kind, "file");
    assert_eq!(r.outcome, "ok");
    assert_eq!(r.file_path.as_deref(), Some("b.txt"));
    assert_eq!(r.destination, dest.path().to_string_lossy());
    assert_eq!(r.files_restored, Some(1));
    assert_eq!(
        contact(&h.conn, r.contact_id.unwrap()),
        ("restore unit".to_string(), Some("ok".to_string()))
    );
    assert!(r.tar_argv.unwrap().contains(r#""--extract""#));
    assert_eq!(r.tar_exit_code, Some(0));
}

/// A file the version did not archive is refused by name from the file
/// table, and a restore that reaches the contact records it as a `failed` row
/// (and a failed contact), not an `ok` row: the bay seam opens the contact
/// before the plan is made. Tar never ran, so its report is NULL, not empty.
/// (Through the drive the same refusal is made before any contact at all:
/// `an_unknown_or_directory_file_is_refused_before_the_drive_is_opened`.)
#[test]
fn a_restore_file_whose_entry_is_missing_records_a_failed_row() {
    let h = written(&two_files(), "64K", "RF-MISS");
    let mut bay = h.bay("RF-MISS", &[]);
    let (result, dest) = h.restore(
        UNIT,
        What::File("nope.txt"),
        "RF-MISS",
        &RestoreOptions::default(),
        &mut bay,
    );
    let err = result.unwrap_err().to_string();
    assert!(err.contains("has no file \"nope.txt\""), "{err}");
    assert!(names_in(dest.path()).is_empty(), "nothing was placed");

    let r = only_row(&h.conn);
    assert_eq!(r.kind, "file");
    assert_eq!(r.outcome, "failed");
    assert_eq!(r.error.as_deref(), Some(err.as_str()));
    assert_eq!(r.file_path.as_deref(), Some("nope.txt"));
    assert_eq!(r.files_restored, None, "nothing was placed");
    assert_eq!(r.slices_read, Some(0), "measured: the tape was not read");
    assert_eq!(r.tar_argv, None, "tar never ran: NULL, not empty");
    assert_eq!(r.tar_stdout, None);
    assert_eq!(r.tar_exit_code, None);
    assert_eq!(
        contact(&h.conn, r.contact_id.unwrap()).1.as_deref(),
        Some("failed")
    );
}

/// A restore that fails AFTER its contact opened (here on the key load: no
/// key file is left) records a `failed` row naming the error, with NULL for
/// tar's report because tar never ran.
#[test]
fn a_restore_failing_after_the_contact_opened_records_a_failed_row() {
    let h = written(&two_files(), "64K", "RR-NOKEY");
    for entry in fs::read_dir(&h.paths.keys_dir).unwrap().flatten() {
        if entry.file_name().to_string_lossy().ends_with(".age.key") {
            fs::remove_file(entry.path()).unwrap();
        }
    }
    let mut bay = h.bay("RR-NOKEY", &[]);
    let (result, _dest) = h.restore(
        UNIT,
        What::Unit,
        "RR-NOKEY",
        &RestoreOptions::default(),
        &mut bay,
    );
    let err = result.unwrap_err().to_string();
    assert!(err.contains("no secret keys"), "{err}");

    let r = only_row(&h.conn);
    assert_eq!(r.kind, "unit");
    assert_eq!(r.outcome, "failed");
    assert_eq!(r.error.as_deref(), Some(err.as_str()));
    assert_eq!(
        contact(&h.conn, r.contact_id.unwrap()).1.as_deref(),
        Some("failed")
    );
    assert_eq!(r.slices_read, Some(0), "measured: none were read");
    assert_eq!(r.tar_argv, None, "tar never ran: NULL, not empty");
    assert_eq!(r.tar_stdout, None);
    assert_eq!(r.tar_stderr, None);
    assert_eq!(r.tar_exit_code, None);
    assert_eq!(r.tar_version, None);
    assert_eq!(r.files_restored, None);
}

/// A wrong-tape refusal happens at the contact, so it is recorded as a
/// failed restore too.
#[test]
fn a_restore_refused_for_the_wrong_tape_records_a_failed_row() {
    let mut h = written(&two_files(), "64K", "RR-WANT");
    h.volume("RR-LOADED");
    let mut bay = h.bay("RR-LOADED", &[]);
    let (result, _dest) = h.restore(
        UNIT,
        What::Unit,
        "RR-WANT",
        &RestoreOptions::default(),
        &mut bay,
    );
    let err = result.unwrap_err().to_string();
    assert!(err.contains("wrong tape"), "{err}");
    let r = only_row(&h.conn);
    assert_eq!(r.outcome, "failed");
    assert!(r.error.as_deref().unwrap().contains("wrong tape"));
    assert_eq!(r.volume_label.as_deref(), Some("RR-WANT"));
    assert_eq!(r.slices_read, Some(0));
}

/// A refusal BEFORE the contact, a dry run here, is not a restore and writes
/// no row. The positive control is every test above.
#[test]
fn a_dry_run_writes_no_row() {
    let rig = Rig::new(written(&two_files(), "64K", "RR-DRY"), "RR-DRY");
    let dest = TempDir::new().unwrap();
    let report = rig.dry_unit("RR-DRY", dest.path()).unwrap();
    assert!(report.dry_run);
    assert_eq!(report.slices as i64, rig.h.num_slices(UNIT));
    assert!(rows(&rig.h.conn).unwrap().is_empty());
    rig.assert_no_tape_contact();
}

fn raw_volume(conn: &Connection, h: &Home, store: &mut MemStore, dest: &Path) -> bool {
    let site = ContactSite::new(
        &h.config,
        Operation::RestoreRawVolume,
        DEVICE,
        Medium::NoBackend,
    );
    restore_raw_volume(conn, store, dest, None, site).is_ok()
}

/// `restore raw-volume`: a clean dump of a tape the real write made is an
/// `ok` row of kind `raw-volume`, carrying the tape's own label and the
/// measured dump, and no tar report (a raw dump runs no tar).
#[test]
fn a_clean_raw_volume_dump_records_an_ok_row() {
    let h = written(&two_files(), "64K", "RAW-OK");
    let mut store = copy_of(&h.tapes["RAW-OK"]);
    let dest = TempDir::new().unwrap();
    assert!(raw_volume(&h.conn, &h, &mut store, dest.path()));

    let r = only_row(&h.conn);
    assert_eq!(r.kind, "raw-volume");
    assert_eq!(r.outcome, "ok");
    assert_eq!(
        r.volume_label.as_deref(),
        Some("RAW-OK"),
        "the tape's own claim"
    );
    assert_eq!(r.volume_id, None, "raw-volume names no catalog row");
    assert_eq!(r.unit_name, None);
    assert!(r.files_restored.unwrap() >= 6, "{:?}", r.files_restored);
    assert!(r.bytes_restored.unwrap() > 0);
    assert_eq!(r.slices_read, None);
    assert_eq!(r.tar_argv, None);
    assert_eq!(r.tar_stdout, None);
    assert_eq!(r.destination, dest.path().to_string_lossy());
    assert_eq!(
        contact(&h.conn, r.contact_id.unwrap()),
        ("restore raw-volume".to_string(), Some("ok".to_string()))
    );
}

/// A raw dump that fails (File 0 only, no front index) records a `failed` row
/// with the error.
#[test]
fn a_failed_raw_volume_dump_records_a_failed_row() {
    let h = written(&two_files(), "64K", "RAW-BAD");
    let mut store = copy_of(&h.tapes["RAW-BAD"]);
    store.files.truncate(1);
    let dest = TempDir::new().unwrap();
    assert!(!raw_volume(&h.conn, &h, &mut store, dest.path()));

    let r = only_row(&h.conn);
    assert_eq!(r.kind, "raw-volume");
    assert_eq!(r.outcome, "failed");
    assert!(r.error.is_some());
    assert_eq!(r.files_restored, None, "not known: the dump never finished");
    assert_eq!(r.bytes_restored, None);
}

// ── issue #406: restore checks before it touches the tape ──

fn rig(label: &str) -> Rig {
    Rig::new(written(&two_files(), "64K", label), label)
}

/// The positive control for every refusal below: the same rig, a clean
/// destination, and the restore runs, read-only. A restore of a unit puts
/// nothing in a scratch directory: no slice is ever written to disk, so the
/// directory does not exist at any read.
#[test]
fn a_restore_unit_into_an_empty_destination_runs_and_makes_no_scratch_directory() {
    let r = rig("PF-OK");
    let dest = TempDir::new().unwrap();
    r.fake.watch(&dest.path().join(SCRATCH_NAME));
    r.unit("PF-OK", dest.path(), &RestoreOptions::default())
        .unwrap();

    assert_restored(&r.h, UNIT, dest.path(), &[]);
    assert_eq!(r.fake.opens(), vec![OpenMode::ReadOnly]);
    let watched = r.fake.watched();
    assert!(!watched.is_empty(), "positive control: slices were read");
    assert!(
        watched.iter().all(|(_, existed)| !existed),
        "a scratch directory existed while a unit was read: {watched:?}"
    );
}

/// Issue #406: `restore file` used to extract the WHOLE unit into
/// `tempfile::tempdir()`: $TMPDIR, the small root filesystem on home2. Now
/// its scratch is inside `--to` while the tape is read, only the file's
/// members are fed to tar, and only that file lands.
#[test]
fn restore_file_extracts_only_its_file_with_scratch_inside_to() {
    let r = rig("PF-FILE");
    let dest = TempDir::new().unwrap();
    r.fake.watch(&dest.path().join(SCRATCH_NAME));
    r.file("PF-FILE", "b.txt", dest.path(), &RestoreOptions::default())
        .unwrap();

    assert_eq!(fs::read(dest.path().join("b.txt")).unwrap(), b"bravo");
    assert_eq!(names_in(dest.path()), ["b.txt"], "no scratch left behind");
    assert!(
        r.fake.watched().iter().any(|(_, existed)| *existed),
        "the scratch directory was inside --to while the slice was read, not in $TMPDIR: {:?}",
        r.fake.watched()
    );
    let row = only_row(&r.h.conn);
    assert!(row.tar_argv.expect("tar ran").contains(r#""--extract""#));
    assert_eq!(
        row.files_restored,
        Some(1),
        "the one member the table names"
    );
}

/// The production half never asks the system for a temp directory: the
/// deterministic form of "never touches $TMPDIR". Every file of the restore
/// path is scanned.
#[test]
fn the_restore_code_never_uses_a_system_temp_directory() {
    const SOURCES: [(&str, &str); 6] = [
        ("restore.rs", include_str!("../restore.rs")),
        ("reader.rs", include_str!("reader.rs")),
        ("supply.rs", include_str!("supply.rs")),
        ("fill.rs", include_str!("fill.rs")),
        ("span.rs", include_str!("span.rs")),
        ("decode.rs", include_str!("decode.rs")),
    ];
    let mut searched = 0;
    for (name, src) in SOURCES {
        let prod = src.split("#[cfg(test)]\nmod tests").next().unwrap();
        let code: Vec<&str> = prod
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect();
        searched += code.len();
        assert!(code.len() > 20, "positive control: {name} was read");
        for needle in ["tempfile::", "temp_dir(", "TMPDIR"] {
            assert!(
                !code.iter().any(|l| l.contains(needle)),
                "{name}: production restore code uses {needle}"
            );
        }
    }
    assert!(
        SOURCES[0].1.contains("fn restore_file("),
        "positive control: the restore module is the one scanned"
    );
    assert!(
        searched > 1000,
        "positive control: {searched} lines scanned"
    );
}

#[test]
fn the_scratch_directory_is_inside_the_destination_or_scratch() {
    assert_eq!(
        scratch_dir(Path::new("/restore/here"), None),
        Path::new("/restore/here/.tapectl-restore-tmp")
    );
    assert_eq!(
        scratch_dir(Path::new("/restore/here"), Some(Path::new("/big/disk"))),
        Path::new("/big/disk/.tapectl-restore-tmp")
    );
}

/// Too little space is refused before the drive is opened: it used to fill
/// the disk hours in.
#[test]
fn a_short_space_destination_is_refused_before_the_drive_is_opened() {
    let r = rig("PF-SPACE");
    let dest = TempDir::new().unwrap();
    {
        let _free = RestoreFreeOverride::set(10);
        let err = r
            .unit("PF-SPACE", dest.path(), &RestoreOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not enough disk space"), "{err}");
        assert!(err.contains("Nothing was read from tape"), "{err}");
        // #594: a unit writes no slice to disk, so --scratch changes nothing
        // for it and the refusal does not send the operator to it.
        assert!(!err.contains("--scratch"), "{err}");
        assert!(err.contains("--to"), "{err}");
        r.assert_no_tape_contact();

        // --no-space-check is the escape, as in RESTORE.sh.
        let options = RestoreOptions {
            no_space_check: true,
            ..RestoreOptions::default()
        };
        r.unit("PF-SPACE", dest.path(), &options).unwrap();
    }
    assert_restored(&r.h, UNIT, dest.path(), &[]);
}

/// A destination that does not exist yet is measured on its nearest existing
/// ancestor, and a RELATIVE one (`--to restored`) on the working directory's,
/// never skipped: its ancestors as written end in "", which exists nowhere.
#[test]
fn a_missing_destination_is_measured_on_its_nearest_existing_ancestor() {
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(
        existing(Path::new("tapectl-no-such-relative-dir/deeper")),
        Some(cwd)
    );
    let tmp = TempDir::new().unwrap();
    assert_eq!(
        existing(&tmp.path().join("not/yet/made")),
        Some(tmp.path().to_path_buf())
    );
    assert_eq!(existing(tmp.path()), Some(tmp.path().to_path_buf()));
}

/// A restore of a unit writes the unit's size once in `--to` and nothing
/// else: its members go to tar as their slice is read. Refused one byte
/// short of it, accepted at it. (It used to be twice the unit: the slices
/// decrypted to scratch, then the extract.)
#[test]
fn the_space_needed_to_restore_a_unit_is_the_unit_once() {
    let r = rig("PF-MATH");
    let plan = plan_of(&r.h, "PF-MATH", RestoreTarget::Unit { dest_dir: "." });
    let members: i64 = r.h.rows(UNIT).iter().map(|m| m.len() as i64).sum();
    assert_eq!(plan.bytes, members, "the members' length in the stream");
    assert_eq!(plan.file_size, None);
    let dest = TempDir::new().unwrap();
    {
        let _free = RestoreFreeOverride::set(plan.bytes - 1);
        let err = r
            .unit("PF-MATH", dest.path(), &RestoreOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not enough disk space"), "{err}");
        assert!(err.contains("the unit's size"), "{err}");
        assert!(!err.contains("twice"), "{err}");
        r.assert_no_tape_contact();
    }
    let _free = RestoreFreeOverride::set(plan.bytes);
    r.unit("PF-MATH", dest.path(), &RestoreOptions::default())
        .unwrap();
    assert_restored(&r.h, UNIT, dest.path(), &[]);
}

/// The figures behind it: a unit keeps its bytes in the destination and has
/// no transient; a file is extracted in scratch and then moved, which on one
/// filesystem costs nothing more than the larger of the two.
#[test]
fn the_space_figures_are_the_units_bytes_and_for_a_file_its_scratch() {
    let dest = TempDir::new().unwrap();
    let scratch = scratch_dir(dest.path(), None);

    let unit = space_needs(dest.path(), &scratch, 5_000, None).expect("one filesystem");
    assert_eq!(unit.len(), 1);
    assert_eq!((unit[0].kept, unit[0].transient), (5_000, 0));

    let file = space_needs(dest.path(), &scratch, 1_536, Some(5)).expect("one filesystem");
    assert_eq!(file.len(), 1, "scratch inside --to is one filesystem");
    assert_eq!(
        (file[0].kept, file[0].transient),
        (0, 1_536),
        "the extract in scratch, then a rename into place"
    );
    // A file larger than its members' length (it cannot be, but the larger
    // figure is the one asked for).
    let file = space_needs(dest.path(), &scratch, 10, Some(4_000)).unwrap();
    assert_eq!(file[0].transient, 4_000);

    // And through the check itself: refused one byte short, accepted at it.
    let h = written(&two_files(), "64K", "PF-FIG");
    let plan = plan_of(
        &h,
        "PF-FIG",
        RestoreTarget::File {
            file_path: "b.txt",
            dest_dir: ".",
        },
    );
    assert_eq!(plan.file_size, Some(5));
    let need = plan.bytes.max(5);
    {
        let _free = RestoreFreeOverride::set(need - 1);
        let err = check_restore_space(dest.path(), &scratch, &plan)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not enough disk space"), "{err}");
        assert!(err.contains("moves it into place"), "{err}");
        // For `restore file` --scratch DIR moves the extraction.
        assert!(err.contains("--scratch DIR"), "{err}");
    }
    let _free = RestoreFreeOverride::set(need);
    check_restore_space(dest.path(), &scratch, &plan).unwrap();
}

/// A destination that is not empty is refused before the drive is opened
/// (tar would keep each colliding file, found only after the whole unit was
/// read); `--overwrite` replaces them and tar is told to.
#[test]
fn a_non_empty_destination_is_refused_unless_overwrite() {
    let r = rig("PF-FULL");
    let dest = TempDir::new().unwrap();
    fs::write(dest.path().join("a.txt"), b"STALE").unwrap();
    let err = r
        .unit("PF-FULL", dest.path(), &RestoreOptions::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("is not empty"), "{err}");
    assert!(err.contains("--overwrite"), "{err}");
    r.assert_no_tape_contact();

    // A dry run that says "would restore" means it.
    let dry = r.dry_unit("PF-FULL", dest.path());
    assert!(dry.is_err(), "a dry run takes the same refusal");
    r.assert_no_tape_contact();

    let options = RestoreOptions {
        overwrite: true,
        ..RestoreOptions::default()
    };
    r.unit("PF-FULL", dest.path(), &options).unwrap();
    assert_eq!(fs::read(dest.path().join("a.txt")).unwrap(), b"alpha");
    let argv = only_row(&r.h.conn).tar_argv.expect("tar ran");
    assert!(argv.contains(r#""--overwrite""#), "{argv}");
}

/// `restore file` places one name, so only that name collides.
#[test]
fn restore_file_refuses_an_existing_file_of_that_name_unless_overwrite() {
    let r = rig("PF-FCOL");
    let dest = TempDir::new().unwrap();
    fs::write(dest.path().join("unrelated.txt"), b"mine").unwrap();
    fs::write(dest.path().join("b.txt"), b"STALE").unwrap();
    let err = r
        .file("PF-FCOL", "b.txt", dest.path(), &RestoreOptions::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("already exists"), "{err}");
    r.assert_no_tape_contact();

    let options = RestoreOptions {
        overwrite: true,
        ..RestoreOptions::default()
    };
    r.file("PF-FCOL", "b.txt", dest.path(), &options).unwrap();
    assert_eq!(fs::read(dest.path().join("b.txt")).unwrap(), b"bravo");
    assert_eq!(
        fs::read(dest.path().join("unrelated.txt")).unwrap(),
        b"mine"
    );
}

/// `--file` is checked against the version's file table before the drive is
/// opened: a typo used to read every slice (~5 h on a large unit) and then
/// fail. A directory is refused too.
#[test]
fn an_unknown_or_directory_file_is_refused_before_the_drive_is_opened() {
    let r = rig("PF-NOFILE");
    let dest = TempDir::new().unwrap();
    let err = r
        .file(
            "PF-NOFILE",
            "nope.txt",
            dest.path(),
            &RestoreOptions::default(),
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("has no file \"nope.txt\""), "{err}");
    assert!(err.contains("tape was not touched"), "{err}");
    assert!(err.contains("tapectl catalog search"), "{err}");
    // `catalog search` looks in each unit's NEWEST version only; the version
    // restored here may be an older one.
    assert!(
        err.contains("tapectl catalog search \"<words of the name>\" --all-versions"),
        "{err}"
    );
    r.assert_no_tape_contact();

    // Positive control: the folder IS in the file table, and is refused for
    // being one, not for being absent.
    assert!(r.h.rows(UNIT).iter().any(|m| m.path == "docs"));
    let err = r
        .file("PF-NOFILE", "docs", dest.path(), &RestoreOptions::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("is a directory"), "{err}");
    r.assert_no_tape_contact();
}

/// A staging whose catalog holds no file table cannot be read by tapectl: a
/// unit and a file are both refused by name before the tape is touched (a
/// restore is cut from the tape's slices by the table, and there is no
/// scanning or spooling fallback).
#[test]
fn a_staging_with_no_file_table_is_refused_before_the_drive_is_opened() {
    let r = rig("PF-NOTBL");
    let dest = TempDir::new().unwrap();
    assert!(!r.h.rows(UNIT).is_empty(), "positive control: it had one");
    r.h.conn.execute("DELETE FROM stage_files", []).unwrap();
    let err = r
        .unit("PF-NOTBL", dest.path(), &RestoreOptions::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("holds no file table"), "{err}");
    assert!(err.contains("Nothing was read from tape"), "{err}");
    r.assert_no_tape_contact();
    let err = r
        .file("PF-NOTBL", "b.txt", dest.path(), &RestoreOptions::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("holds no file table"), "{err}");
    r.assert_no_tape_contact();
}

/// A scratch directory a killed `restore file` left (its guard never ran)
/// may hold a restored file: refused, named, never reused, inside `--to` and
/// inside `--scratch`. A restore of a unit makes none, so a leftover under
/// `--scratch` is neither read nor touched by one.
#[test]
fn a_leftover_scratch_directory_is_refused_before_the_drive_is_opened() {
    let r = rig("PF-LEFT");
    let dest = TempDir::new().unwrap();
    fs::create_dir(dest.path().join(SCRATCH_NAME)).unwrap();
    let err = r
        .file("PF-LEFT", "b.txt", dest.path(), &RestoreOptions::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("already exists"), "{err}");
    assert!(err.contains("may hold the restored file"), "{err}");
    assert!(err.contains(SCRATCH_NAME), "{err}");
    assert!(err.contains("Nothing was read from tape"), "{err}");

    let other = TempDir::new().unwrap();
    fs::create_dir(other.path().join(SCRATCH_NAME)).unwrap();
    let fresh = TempDir::new().unwrap();
    let options = RestoreOptions {
        scratch: Some(other.path().to_path_buf()),
        ..RestoreOptions::default()
    };
    let err = r
        .file("PF-LEFT", "b.txt", fresh.path(), &options)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(&other.path().join(SCRATCH_NAME).display().to_string()),
        "{err}"
    );
    r.assert_no_tape_contact();

    // A unit restore uses no scratch: the same leftover is no reason to
    // refuse it, and it is left exactly as it was.
    r.unit("PF-LEFT", fresh.path(), &options).unwrap();
    assert_restored(&r.h, UNIT, fresh.path(), &[]);
    assert!(other.path().join(SCRATCH_NAME).is_dir());
    assert!(names_in(&other.path().join(SCRATCH_NAME)).is_empty());
}

/// `--scratch DIR`: `restore file`'s extract waits there while the tape is
/// read, not in `--to`, and nothing is left in either. A unit restore with
/// the same option never makes the directory.
#[test]
fn scratch_dir_puts_restore_files_extraction_on_another_disk() {
    let r = rig("PF-SCR");
    let dest = TempDir::new().unwrap();
    let other = TempDir::new().unwrap();
    r.fake.watch(&other.path().join(SCRATCH_NAME));
    let options = RestoreOptions {
        scratch: Some(other.path().to_path_buf()),
        ..RestoreOptions::default()
    };
    r.file("PF-SCR", "b.txt", dest.path(), &options).unwrap();
    assert!(
        r.fake.watched().iter().any(|(_, existed)| *existed),
        "{:?}",
        r.fake.watched()
    );
    assert_eq!(names_in(dest.path()), ["b.txt"]);
    assert!(names_in(other.path()).is_empty(), "scratch removed");

    // Positive control's other side: a unit restore never makes it.
    let again = TempDir::new().unwrap();
    r.fake.watch(&other.path().join(SCRATCH_NAME));
    let seen = r.fake.watched().len();
    r.unit("PF-SCR", again.path(), &options).unwrap();
    let watched = r.fake.watched();
    assert!(watched.len() > seen, "positive control: slices were read");
    assert!(
        watched[seen..].iter().all(|(_, existed)| !existed),
        "{watched:?}"
    );
    assert!(names_in(other.path()).is_empty());
}

// ── issue #389: restore reads the tape forward ──

/// THE acceptance test for restore: a unit of K contiguous slices comes off a
/// tape with ONE rewind (the open's), then File 0 (the contact check), one
/// forward space to the first slice and every slice in turn. Before #389 it
/// was a rewind and a locate per slice. Driven through the real
/// `restore_unit` over a `TapeStore` on the in-memory tape, written by the
/// real `volume write`, so the restored files prove the slices that came back
/// were the right ones.
#[test]
fn a_multi_slice_restore_is_one_rewind_and_one_forward_pass() {
    let h = written(&tree(), "64K", "FWD-1");
    let slices = h.num_slices(UNIT);
    assert!(slices >= 3, "want a multi-slice unit, got {slices}");
    let positions: Vec<u32> = (1..=slices)
        .map(|n| h.position("FWD-1", UNIT, n) as u32)
        .collect();
    let r = Rig::new(h, "FWD-1");
    let dest = TempDir::new().unwrap();
    let report = r
        .unit("FWD-1", dest.path(), &RestoreOptions::default())
        .expect("a clean multi-slice restore");
    assert_eq!(report.slices as i64, slices);
    assert_restored(&r.h, UNIT, dest.path(), &[]);

    assert!(
        positions.windows(2).all(|w| w[0] < w[1]),
        "the slices lie in tape order: {positions:?}"
    );
    // File 0 is a bounded head read (issue #400), then one space to the
    // first slice and the slices in turn.
    assert_eq!(r.reads(), positions, "every slice once, forward");
    assert_eq!(r.fake.rewinds(), 1, "{:?}", r.fake.ops());
    assert_eq!(r.fake.ops()[..2], [Op::Rewind, Op::ReadHead(0)]);
}

// ── issue #411: members are cut by the file table; read only what a file needs ──

/// THE acceptance test for #411's unit half: a multi-slice unit restores with
/// free space for the DESTINATION ONLY: each slice is held in memory while
/// its members are fed to tar, so nothing of it is ever on disk in scratch,
/// in one forward pass.
#[test]
fn a_unit_restores_with_space_for_the_destination_only() {
    let h = written(&tree(), "64K", "ST-1");
    assert!(h.num_slices(UNIT) >= 3, "want a multi-slice unit");
    let plan = plan_of(&h, "ST-1", RestoreTarget::Unit { dest_dir: "." });
    let r = Rig::new(h, "ST-1");
    let dest = TempDir::new().unwrap();
    let _free = RestoreFreeOverride::set(plan.bytes);
    r.fake.watch(&dest.path().join(SCRATCH_NAME));

    r.unit("ST-1", dest.path(), &RestoreOptions::default())
        .expect("a streamed restore needs only the destination's space");
    assert_restored(&r.h, UNIT, dest.path(), &[]);

    let argv = only_row(&r.h.conn).tar_argv.expect("tar ran");
    assert!(argv.contains(r#""--extract""#), "{argv}");
    let bytes = r.fake.watched_bytes();
    assert!(
        bytes.len() as i64 > r.h.num_slices(UNIT),
        "positive control: every read was watched: {bytes:?}"
    );
    assert!(
        bytes.iter().all(|(_, b)| *b == 0),
        "a decrypted slice was on disk while the tape was read: {bytes:?}"
    );
    assert_eq!(r.fake.rewinds(), 1);
}

/// A unit of many files with long names has a file table far bigger than a
/// pipe's buffer (#497's shape, which made a restore of a unit of many files
/// fail once an unread tail outgrew a pipe): it restores whole and every
/// slice is read once, forward.
#[test]
fn a_unit_of_many_long_names_restores_whole_reading_each_slice_once() {
    let items: Vec<Item> = (0..1500)
        .map(|i| Item::File(format!("{i:04}-{}.bin", "n".repeat(150)), noise(100, i + 7)))
        .collect();
    let h = written(&items, "256K", "BC-1");
    let slices = h.num_slices(UNIT);
    assert!(slices >= 2, "want several slices, got {slices}");
    let table: usize = h.rows(UNIT).iter().map(|m| m.path.len()).sum();
    assert!(table > 200_000, "the table's own names are {table} bytes");
    let positions: Vec<u32> = (1..=slices)
        .map(|n| h.position("BC-1", UNIT, n) as u32)
        .collect();
    let r = Rig::new(h, "BC-1");
    let dest = TempDir::new().unwrap();
    r.unit("BC-1", dest.path(), &RestoreOptions::default())
        .expect("the unit is whole");
    assert_restored(&r.h, UNIT, dest.path(), &[]);
    assert_eq!(r.reads(), positions, "every slice read once, forward");
}

/// A slice whose age header does not open cannot be read around: no chunk of
/// it can be told from another. With no other copy the slice is bad as a
/// whole: the members cut from it are left out and listed, the restore goes
/// on with the next slice, and no byte that did not authenticate is written
/// (ADR-0012 item 264, `volume-format-v3.md` §7.1; #583). It was a failure of
/// the whole restore in 1.x (#484), when a dar slice could not be skipped; a
/// Layout 3 slice holds whole members, so it can.
///
/// The byte is a landmark of the age file, not an offset: age's grease stanza
/// makes the header's length random (#484).
#[test]
fn a_corrupt_slice_header_leaves_its_members_out_and_the_restore_goes_on() {
    const SLICE: u64 = 128 * 1024;
    let mut h = written(&tree(), "128K", "ST-3");
    let slices = h.num_slices(UNIT);
    assert!(slices >= 3);
    let pos = h.position("ST-3", UNIT, 2);
    let at = header_mac_offset(&h, "ST-3", pos);
    h.flip("ST-3", pos, at);
    let lost = members_touching(&h, SLICE, 2);
    assert!(
        lost.len() < h.rows(UNIT).len(),
        "positive control: members outside slice 2 exist"
    );
    let all: Vec<u32> = (1..=slices)
        .map(|n| h.position("ST-3", UNIT, n) as u32)
        .collect();
    let r = Rig::new(h, "ST-3");
    let dest = TempDir::new().unwrap();
    let err = r
        .unit("ST-3", dest.path(), &RestoreOptions::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("INCOMPLETE"), "{err}");
    assert!(err.contains("slice 2"), "{err}");
    assert!(err.contains("cannot be read around"), "{err}");
    assert_eq!(
        lost_paths(&err),
        lost,
        "exactly the members the table puts in slice 2: {err}"
    );
    assert_all_but(&r.h, dest.path(), &lost);
    assert_eq!(r.reads(), all, "every slice is read, in one forward pass");
    assert_eq!(r.fake.rewinds(), 1);
    assert!(
        !dest.path().join(SCRATCH_NAME).exists(),
        "no scratch is left behind"
    );
    let row = only_row(&r.h.conn);
    assert_eq!(row.outcome, "failed");
    assert_eq!(row.error.as_deref(), Some(err.as_str()));
    assert!(row.tar_argv.is_some(), "tar ran for the other slices");
}

/// The other half of #484: a byte of the payload nonce, outside the header's
/// MAC. The header opens and the key unwraps, but no chunk of the slice
/// authenticates, and the only copy has nothing to substitute, so the whole of
/// slice 2 is a hole. The members cut from it are left out and listed, which
/// are exactly the members of the file table that touch slice 2; every other
/// file is whole and no byte that did not authenticate was written.
#[test]
fn a_slice_whose_every_chunk_fails_is_a_hole_and_the_restore_is_incomplete() {
    const SLICE: u64 = 128 * 1024;
    let mut h = written(&tree(), "128K", "ST-4");
    let pos = h.position("ST-4", UNIT, 2);
    let at = nonce_offset(&h, "ST-4", pos);
    h.flip("ST-4", pos, at);
    let lost = members_touching(&h, SLICE, 2);
    assert!(
        lost.contains("big.bin"),
        "the long member spans slice 2: {lost:?}"
    );
    assert!(
        lost.len() < h.rows(UNIT).len(),
        "positive control: members outside slice 2 exist"
    );
    let r = Rig::new(h, "ST-4");
    let dest = TempDir::new().unwrap();
    let err = r
        .unit("ST-4", dest.path(), &RestoreOptions::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("INCOMPLETE"), "{err}");
    assert!(err.contains("slice 2 on ST-4"), "{err}");
    assert!(err.contains("hole(s)"), "{err}");
    assert!(
        err.contains("0..65536"),
        "every chunk, from the first: {err}"
    );
    assert_eq!(
        lost_paths(&err),
        lost,
        "exactly the members the table puts in slice 2: {err}"
    );
    assert!(
        !err.contains("partial restore"),
        "a hole is not a stream that failed: {err}"
    );
    // No unauthenticated byte: a lost file is not there, every other file is
    // whole.
    assert_all_but(&r.h, dest.path(), &lost);
    assert!(
        fs::read(dest.path().join("small.txt")).is_ok() || lost.contains("small.txt"),
        "positive control: the files outside the slice are there"
    );
    assert!(!dest.path().join("big.bin").exists());
    assert_eq!(r.fake.rewinds(), 1);
    assert!(!dest.path().join(SCRATCH_NAME).exists());
    let row = only_row(&r.h.conn);
    assert_eq!(row.outcome, "failed");
    assert_eq!(
        row.tar_exit_code,
        Some(0),
        "tar's exit is never the verdict"
    );
}

/// THE acceptance test for #411's file half: `restore file` reads only the
/// slices that hold the one file, in one forward pass, never the whole unit.
#[test]
fn restore_file_reads_only_the_slices_that_hold_it() {
    const SLICE: u64 = 64 * 1024;
    let h = written(&tree(), "64K", "SF-1");
    let all = h.num_slices(UNIT);
    assert!(all >= 5, "want a many-slice unit, got {all}");
    let rows = h.rows(UNIT);
    let row = rows.iter().find(|m| m.path == "small.txt").unwrap();
    let wanted: Vec<u32> = row
        .slices(SLICE)
        .map(|n| h.position("SF-1", UNIT, n as i64) as u32)
        .collect();
    let r = Rig::new(h, "SF-1");
    let dest = TempDir::new().unwrap();
    let report = r
        .file("SF-1", "small.txt", dest.path(), &RestoreOptions::default())
        .unwrap();
    assert_eq!(fs::read(dest.path().join("small.txt")).unwrap(), b"small");
    assert_eq!(names_in(dest.path()), ["small.txt"]);

    let reads = r.reads();
    assert_eq!(reads, wanted, "exactly the slices that hold the file");
    assert!(
        (reads.len() as i64) < all,
        "positive control: the unit has more slices than the file needs: {reads:?} of {all}"
    );
    assert!(
        reads.windows(2).all(|w| w[0] < w[1]),
        "ascending: {reads:?}"
    );
    assert_eq!(r.fake.rewinds(), 1);
    assert_eq!(report.slices, reads.len());
    assert_eq!(only_row(&r.h.conn).slices_read, Some(reads.len() as i64));
}

/// A file below directories needs nothing of its ancestors' slices: tar
/// makes the missing parents itself, and the file comes back identical. The
/// unit is laid out so the folders lie in slice 1 and the file in a later
/// slice (positive control: the ancestors' slices are not the file's, and
/// are not read).
#[test]
fn restore_file_of_a_nested_file_reads_only_its_own_slices_and_makes_its_parents() {
    const SLICE: u64 = 64 * 1024;
    let items = vec![
        Item::file("00-sub/deep/a-big.bin", noise(150_000, 3)),
        Item::file("00-sub/deep/x", b"x, nested two folders deep\n".to_vec()),
        Item::file("zz-tail.bin", noise(100_000, 4)),
    ];
    let h = written(&items, "64K", "SF-2");
    let all = h.num_slices(UNIT);
    let rows = h.rows(UNIT);
    let file = rows.iter().find(|m| m.path == "00-sub/deep/x").unwrap();
    let ancestors: Vec<&_> = rows
        .iter()
        .filter(|m| m.path == "00-sub" || m.path == "00-sub/deep")
        .collect();
    assert_eq!(ancestors.len(), 2, "the folders are members of the table");
    let own: Vec<u64> = file.slices(SLICE).collect();
    for folder in &ancestors {
        assert!(
            folder.slices(SLICE).all(|n| !own.contains(&n)),
            "shape control: {} lies in other slices than the file ({own:?})",
            folder.path
        );
    }
    let wanted: Vec<u32> = own
        .iter()
        .map(|n| h.position("SF-2", UNIT, *n as i64) as u32)
        .collect();
    let r = Rig::new(h, "SF-2");
    let dest = TempDir::new().unwrap();
    r.file(
        "SF-2",
        "00-sub/deep/x",
        dest.path(),
        &RestoreOptions::default(),
    )
    .unwrap();
    assert_eq!(
        fs::read(dest.path().join("x")).unwrap(),
        b"x, nested two folders deep\n"
    );
    assert_eq!(names_in(dest.path()), ["x"]);
    let reads = r.reads();
    assert_eq!(reads, wanted, "only the file's own slices");
    assert!((reads.len() as i64) < all, "{reads:?} of {all}");
    assert_eq!(only_row(&r.h.conn).files_restored, Some(1));
}

// ── ADR-0012 items 303, 306 (#594 C-1): the unit's root gets its recorded metadata ──

/// The recorded mtime of the root in these tests: long ago, so no restore's
/// own writes could match it.
const ROOT_MTIME_SECS: i64 = 981_173_106; // 2001-02-03 04:05:06 UTC

fn mode_of(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

fn mtime_of(p: &Path) -> i64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(p).unwrap().mtime()
}

fn xattr_of(p: &Path) -> Option<String> {
    let o = std::process::Command::new("getfattr")
        .args(["-n", "user.root-mark", "--only-values"])
        .arg(p)
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

fn acl_of(p: &Path) -> String {
    let o = std::process::Command::new("getfacl")
        .arg("-c")
        .arg(p)
        .output()
        .expect("getfacl");
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// What the host let the test mark the root with.
#[derive(Clone, Copy)]
struct Marks {
    xattr: bool,
    acl: bool,
    default_acl: bool,
}

/// A home whose unit folder carries a mode, an mtime, and (where the host
/// allows) an extended attribute and an access and a default ACL of its own,
/// staged and written. A mark the host refuses is said loudly and not
/// checked.
fn written_with_a_marked_root(label: &str) -> (Home, Marks) {
    use std::os::unix::fs::PermissionsExt;
    let mut h = Home::new("64K", None, 1);
    h.unit(TENANT, UNIT, &two_files());
    let src = h.source(UNIT);
    let ok = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd)
            .args(args)
            .arg(&src)
            .status()
            .is_ok_and(|s| s.success())
    };
    let marks = Marks {
        xattr: ok("setfattr", &["-n", "user.root-mark", "-v", "kept"]),
        acl: ok("setfacl", &["-m", "u:12345:r-x"]),
        default_acl: ok("setfacl", &["-d", "-m", "u:12346:r-x"]),
    };
    for (what, set) in [
        ("xattr", marks.xattr),
        ("ACL", marks.acl),
        ("default ACL", marks.default_acl),
    ] {
        if !set {
            eprintln!("SKIPPING the root's {what} half of item 303's test: the host refuses it");
        }
    }
    // chmod after setfacl: an ACL's mask rides on the group bits.
    fs::set_permissions(&src, fs::Permissions::from_mode(0o750)).unwrap();
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(ROOT_MTIME_SECS as u64);
    fs::File::open(&src)
        .unwrap()
        .set_modified(t)
        .expect("the source root's mtime");
    h.stage(UNIT);
    h.volume(label);
    h.write(label);
    (h, marks)
}

fn assert_root_is_recorded(dest: &Path, marks: Marks) {
    assert_eq!(
        mode_of(dest),
        0o750,
        "the destination's mode is the root member's"
    );
    assert_eq!(
        mtime_of(dest),
        ROOT_MTIME_SECS,
        "the destination's mtime is the root member's, not the restore's own"
    );
    if marks.xattr {
        assert_eq!(xattr_of(dest).as_deref(), Some("kept"));
    }
    let acl = acl_of(dest);
    if marks.acl {
        assert!(acl.contains("user:12345:r-x"), "{acl}");
    }
    if marks.default_acl {
        assert!(acl.contains("default:user:12346:r-x"), "{acl}");
    }
}

#[test]
fn an_empty_destinations_root_gets_the_recorded_mode_mtime_xattr_and_acls() {
    let (h, marks) = written_with_a_marked_root("R3-EMPTY");
    let r = Rig::new(h, "R3-EMPTY");
    // TempDir makes it 0700, now: nothing like the recorded root.
    let dest = TempDir::new().unwrap();
    assert_ne!(mode_of(dest.path()), 0o750, "positive control");
    r.unit("R3-EMPTY", dest.path(), &RestoreOptions::default())
        .unwrap();
    assert_restored(&r.h, UNIT, dest.path(), &[]);
    assert_root_is_recorded(dest.path(), marks);
}

#[test]
fn a_destination_tapectl_makes_gets_the_recorded_root_too() {
    let (h, marks) = written_with_a_marked_root("R3-NEW");
    let r = Rig::new(h, "R3-NEW");
    let parent = TempDir::new().unwrap();
    let dest = parent.path().join("made-by-tapectl");
    assert!(!dest.exists());
    r.unit("R3-NEW", &dest, &RestoreOptions::default()).unwrap();
    assert_root_is_recorded(&dest, marks);
}

#[test]
fn a_non_empty_destinations_root_keeps_its_own() {
    let (h, _) = written_with_a_marked_root("R3-FULL");
    let r = Rig::new(h, "R3-FULL");
    let dest = TempDir::new().unwrap();
    fs::write(dest.path().join("a.txt"), b"STALE").unwrap();
    let (mode, mtime) = (mode_of(dest.path()), mtime_of(dest.path()));
    let err = r
        .unit("R3-FULL", dest.path(), &RestoreOptions::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("it is not empty"), "{err}");
    assert_eq!(mode_of(dest.path()), mode);
    assert_eq!(mtime_of(dest.path()), mtime);
    assert_eq!(fs::read(dest.path().join("a.txt")).unwrap(), b"STALE");
}

/// `--overwrite` is tar's own: it sets the root itself, and tapectl runs no
/// second tar for it.
#[test]
fn an_overwrite_restore_sets_the_root_by_tars_own_run_alone() {
    let (h, marks) = written_with_a_marked_root("R3-OW");
    let r = Rig::new(h, "R3-OW");
    let dest = TempDir::new().unwrap();
    let options = RestoreOptions {
        overwrite: true,
        ..RestoreOptions::default()
    };
    r.unit("R3-OW", dest.path(), &options).unwrap();
    assert_root_is_recorded(dest.path(), marks);
    let argv = only_row(&r.h.conn).tar_argv.expect("tar ran");
    assert!(argv.contains(r#""--overwrite""#), "{argv}");
}

/// `restore file` places one file: the destination's own mode and times are
/// never given the unit root's.
#[test]
fn a_restore_file_does_not_touch_the_destinations_root() {
    let (h, _) = written_with_a_marked_root("R3-FILE");
    let r = Rig::new(h, "R3-FILE");
    let dest = TempDir::new().unwrap();
    let mode = mode_of(dest.path());
    r.file("R3-FILE", "b.txt", dest.path(), &RestoreOptions::default())
        .unwrap();
    assert_eq!(fs::read(dest.path().join("b.txt")).unwrap(), b"bravo");
    assert_eq!(mode_of(dest.path()), mode, "its own mode");
    assert_ne!(mtime_of(dest.path()), ROOT_MTIME_SECS);
    assert_ne!(mode, 0o750, "positive control: the recorded root differs");
    // Asking for the root by name is refused, whichever way: it is the
    // destination itself, or a folder and not a file.
    let err = r
        .file("R3-FILE", ".", dest.path(), &RestoreOptions::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("already exists"), "{err}");
    let overwrite = RestoreOptions {
        overwrite: true,
        ..RestoreOptions::default()
    };
    let err = r
        .file("R3-FILE", ".", dest.path(), &overwrite)
        .unwrap_err()
        .to_string();
    assert!(err.contains("is a directory"), "{err}");
    assert_eq!(mode_of(dest.path()), mode);
}
