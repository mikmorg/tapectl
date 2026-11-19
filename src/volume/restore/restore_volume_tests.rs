//! Tests of tapectl's restores: restore volume and the requires fence (ported onto the tar reader in T4.8).
//!
//! Every unit is staged by the real `stage create` and every tape written by
//! the real `volume write` ([`super::fixture::Home`]); the cartridges are
//! in-memory tapes behind a [`TestBay`]. A tape whose File 0 says something
//! else is a REAL written tape with its File 0 rewritten
//! ([`crate::volume::format::rewrite_file0`]), never a tape made by hand.
//! What the drive was asked to read is seen through [`Spy`], a store that
//! logs each file read, so "File 0 once and nothing else", "one pass, strictly
//! forward" and "ub was never read" are asserted, not assumed.

use std::cell::RefCell;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::rc::Rc;

use rusqlite::Connection;
use tempfile::TempDir;

use super::fixture::{assert_restored, copy_of, noise, Home, Item, What, DEVICE, TENANT};
use super::repair_tests::tests::TestBay;
use super::{
    plan_units, restore_unit_from_loaded_store, restore_units, restore_units_from_bay,
    restore_units_from_loaded_store, unit_requests, units_on_volume, RestoreOptions, RestoreTarget,
    UnitRequest, UnitsReport, SCRATCH_NAME,
};
use crate::cli::restore::{run, units_report_lines, RestoreCommands};
use crate::error::Result;
use crate::store::{CapacityReport, MemStore, Store};
use crate::tape::contact::{ContactSite, Medium, Operation};
use crate::volume::restore_record::rows;

// ── helpers ──

/// The tape files read, in order, through a [`Spy`].
type Reads = Rc<RefCell<Vec<u32>>>;

/// A store that logs every tape file read from it and otherwise is the
/// medium it wraps. (`read_file_head`, File 0's bounded read, goes through
/// `read_file` by the trait's default, so it is logged too.)
struct Spy {
    inner: MemStore,
    reads: Reads,
}

impl Store for Spy {
    fn capacity(&mut self) -> Result<CapacityReport> {
        self.inner.capacity()
    }
    fn execute(&mut self, src: &mut dyn std::io::Read, len: u64, sync: bool) -> Result<u64> {
        self.inner.execute(src, len, sync)
    }
    fn read_file(&mut self, position: u32, sink: &mut dyn Write) -> Result<u64> {
        self.reads.borrow_mut().push(position);
        self.inner.read_file(position, sink)
    }
    fn reposition_for_resume(&mut self, file_index: u32) -> Result<()> {
        self.inner.reposition_for_resume(file_index)
    }
    fn blank_at_bot(&mut self) -> Result<bool> {
        self.inner.blank_at_bot()
    }
    fn eod_at_or_before(&mut self, position: u32) -> Result<bool> {
        self.inner.eod_at_or_before(position)
    }
    fn files_on_medium(&mut self) -> Result<Option<u32>> {
        self.inner.files_on_medium()
    }
}

/// The bay with `start` in the drive, its cartridge `label` replaced by a
/// [`Spy`] over a copy of what that tape holds now.
fn spied_bay(h: &Home, start: &str) -> (TestBay, Reads) {
    let mut bay = h.bay(start, &[]);
    let reads: Reads = Rc::default();
    bay.carts.insert(
        start.to_string(),
        Box::new(Spy {
            inner: copy_of(&h.tapes[start]),
            reads: reads.clone(),
        }),
    );
    (bay, reads)
}

/// `restore volume`'s contact site.
fn volume_site(h: &Home) -> ContactSite<'_> {
    ContactSite::new(
        &h.config,
        Operation::RestoreVolume,
        DEVICE,
        Medium::NoBackend,
    )
}

/// `restore volume`'s set: plan it, then the one pass, as `restore_units`
/// does around the drive.
fn run_set(
    h: &Home,
    label: &str,
    reqs: &[UnitRequest],
    fail_fast: bool,
    bay: &mut TestBay,
) -> Result<UnitsReport> {
    let options = RestoreOptions::default();
    let planned = plan_units(&h.conn, &h.config, label, reqs, &options)?;
    restore_units_from_bay(
        &h.conn,
        &h.paths,
        &h.config,
        label,
        &planned,
        &options,
        fail_fast,
        bay,
        volume_site(h),
    )
}

/// One request for each of `names`, each into `root/<name>`.
fn requests(names: &[&str], root: &Path) -> Vec<UnitRequest> {
    let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
    unit_requests(&names, &root.to_string_lossy()).unwrap()
}

/// Three units on one volume, `label`: ua and ub each hold a file larger than
/// a slice (a run of slices of its own), uc is one small file.
fn three(label: &str) -> Home {
    let mut h = Home::new("64K", None, 1);
    let a = vec![
        Item::file("a.bin", noise(90_000, 1)),
        Item::file("a.txt", b"alpha".to_vec()),
    ];
    let b = vec![
        Item::file("b.bin", noise(120_000, 2)),
        Item::file("d/b.txt", b"bravo".to_vec()),
    ];
    let c = vec![Item::file("c.txt", b"charlie".to_vec())];
    for (name, items) in [("ua", &a), ("ub", &b), ("uc", &c)] {
        h.unit(TENANT, name, items);
        h.stage(name);
    }
    h.volume(label);
    h.write(label);
    h
}

/// Flip a byte inside the data of `unit`'s first slice on `label`, so the
/// members it holds cannot be read from this tape.
fn damage_first_slice(h: &mut Home, label: &str, unit: &str) {
    let pos = h.position(label, unit, 1);
    h.flip(label, pos, 2_000);
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// How many contacts the restores made.
fn restore_contacts(conn: &Connection) -> i64 {
    count(
        conn,
        "SELECT COUNT(*) FROM cartridge_contacts WHERE operation LIKE 'restore%'",
    )
}

/// The contacts the RESTORES made (the fixture's own `volume init` and
/// `volume write` contacts are not in it), oldest first:
/// `(operation, volume_id, outcome, detail)`.
#[allow(clippy::type_complexity)]
fn contact_rows(conn: &Connection) -> Vec<(String, Option<i64>, Option<String>, Option<String>)> {
    conn.prepare("SELECT operation, volume_id, outcome, detail FROM cartridge_contacts WHERE operation LIKE 'restore%' ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// `(operation, outcome)` of the one contact row.
fn only_contact(conn: &Connection) -> (String, Option<String>) {
    let all = contact_rows(conn);
    assert_eq!(all.len(), 1, "one contact: {all:?}");
    (all[0].0.clone(), all[0].2.clone())
}

fn volume_id(conn: &Connection, label: &str) -> i64 {
    conn.query_row("SELECT id FROM volumes WHERE label = ?1", [label], |r| {
        r.get(0)
    })
    .unwrap()
}

/// Every unit of `names` is in `root/<name>` as it was.
fn assert_set_restored(h: &Home, root: &Path, names: &[&str]) {
    for n in names {
        assert_restored(h, n, &root.join(n), &[]);
    }
}

fn assert_ascending(reads: &[u32]) {
    assert!(
        reads.windows(2).all(|w| w[0] < w[1]),
        "strictly ascending: {reads:?}"
    );
}

// ── issue #398: several units from one volume, one tape pass ──

/// THE acceptance test for #398: three units restored in one session read
/// the tape strictly forward in ONE pass -- File 0 once, then every slice of
/// every unit in ascending position -- under one contact, one `restores` row
/// for each unit, and no cartridge change. Asked for in NON-tape order, so
/// the order is the restore's own doing.
#[test]
fn three_units_are_one_rewind_and_one_forward_pass() {
    let h = three("MU-1");
    let root = TempDir::new().unwrap();
    let mut reqs = requests(&["ua", "ub", "uc"], root.path());
    reqs.reverse();
    let (mut bay, reads) = spied_bay(&h, "MU-1");
    let report = run_set(&h, "MU-1", &reqs, false, &mut bay).unwrap();
    assert_eq!(report.failed(), 0, "{report:?}");
    assert_set_restored(&h, root.path(), &["ua", "ub", "uc"]);
    let names: Vec<&str> = report.units.iter().map(|u| u.unit_name.as_str()).collect();
    assert_eq!(names, ["ua", "ub", "uc"], "outcomes in tape order");

    assert_eq!(bay.releases, 0, "the drive was never released and reloaded");
    assert!(bay.asked.is_empty(), "nobody was asked for a cartridge");
    let reads = reads.borrow();
    assert_eq!(reads[0], 0, "File 0 first, once: {reads:?}");
    assert_ascending(&reads);
    let slices: i64 = ["ua", "ub", "uc"].iter().map(|u| h.num_slices(u)).sum();
    assert!(slices > 3, "positive control: units have several slices");
    assert_eq!(
        reads.len() as i64,
        slices + 1,
        "File 0 and each slice once: {reads:?}"
    );

    assert_eq!(restore_contacts(&h.conn), 1);
    let all = rows(&h.conn).unwrap();
    assert_eq!(all.len(), 3);
    assert!(
        all.iter().all(|r| r.outcome == "ok" && r.kind == "unit"),
        "{all:?}"
    );
    let contact: i64 = h
        .conn
        .query_row(
            "SELECT id FROM cartridge_contacts WHERE operation = 'restore volume'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(all.iter().all(|r| r.contact_id == Some(contact)));
}

/// One unit's failure does not stop the others: ub's first slice is damaged
/// on the only copy, ua and uc restore whole, ub is reported (and recorded)
/// failed with the restore's own INCOMPLETE listing, and the pass is still
/// one pass, forward only.
#[test]
fn a_failing_unit_does_not_stop_the_others() {
    let mut h = three("MU-1");
    damage_first_slice(&mut h, "MU-1", "ub");
    let root = TempDir::new().unwrap();
    let (mut bay, reads) = spied_bay(&h, "MU-1");
    let report = run_set(
        &h,
        "MU-1",
        &requests(&["ua", "ub", "uc"], root.path()),
        false,
        &mut bay,
    )
    .unwrap();
    assert_eq!(report.failed(), 1, "{report:?}");
    let b = &report.units[1];
    assert_eq!(b.unit_name, "ub");
    let error = b.error.as_deref().unwrap_or("");
    assert!(error.contains("INCOMPLETE"), "{b:?}");
    // The members the damaged slice holds are named, and every other member
    // is there: the listing is the damage's and nothing else's.
    let table = h.rows("ub");
    let lost: Vec<&str> = table
        .iter()
        .filter(|r| r.slice == 1)
        .map(|r| r.path.as_str())
        .collect();
    assert!(!lost.is_empty(), "positive control: slice 1 holds members");
    assert!(
        lost.len() < table.len(),
        "positive control: others are elsewhere"
    );
    for path in &lost {
        assert!(error.contains(path), "{path} is named: {error}");
    }
    for row in table.iter().filter(|r| r.slice != 1) {
        assert!(
            root.path().join("ub").join(&row.path).exists(),
            "{} is restored",
            row.path
        );
    }
    assert_restored(&h, "ua", &root.path().join("ua"), &[]);
    assert_restored(&h, "uc", &root.path().join("uc"), &[]);
    assert_eq!(bay.releases, 0);
    assert_ascending(&reads.borrow());
    let outcomes: Vec<String> = rows(&h.conn)
        .unwrap()
        .into_iter()
        .map(|r| r.outcome)
        .collect();
    assert_eq!(outcomes, ["ok", "failed", "ok"]);
}

/// `fail_fast`: the first failure ends the session; the units after it are
/// reported not attempted, and have no row.
#[test]
fn fail_fast_stops_at_the_first_failing_unit() {
    let mut h = three("MU-1");
    damage_first_slice(&mut h, "MU-1", "ub");
    let root = TempDir::new().unwrap();
    let mut bay = h.bay("MU-1", &[]);
    let report = run_set(
        &h,
        "MU-1",
        &requests(&["ua", "ub", "uc"], root.path()),
        true,
        &mut bay,
    )
    .unwrap();
    assert_eq!(report.failed(), 2, "{report:?}");
    assert!(report.units[0].error.is_none());
    assert!(report.units[1].error.is_some());
    assert!(!report.units[2].attempted, "{report:?}");
    assert_eq!(rows(&h.conn).unwrap().len(), 2);
    assert!(!root.path().join("uc").exists());
    // The positive control: the same set without the flag does restore uc.
    let root = TempDir::new().unwrap();
    let mut bay = h.bay("MU-1", &[]);
    let report = run_set(
        &h,
        "MU-1",
        &requests(&["ua", "ub", "uc"], root.path()),
        false,
        &mut bay,
    )
    .unwrap();
    assert!(
        report.units[2].attempted && report.units[2].error.is_none(),
        "{report:?}"
    );
    assert!(root.path().join("uc").exists());
}

/// Everything that can be refused without the tape is refused for the WHOLE
/// set before the drive opens: an unknown unit, a unit asked for twice, two
/// units into one directory, a destination that is not empty, and a set that
/// does not fit on the disk though each unit alone would. Through
/// `restore_units` itself, whose drive is a path nothing is at: a refusal
/// that came after the drive would be a different error.
#[test]
fn the_whole_set_is_checked_before_the_drive_opens() {
    let h = three("MU-1");
    let root = TempDir::new().unwrap();
    let options = RestoreOptions::default();
    let refused = |reqs: &[UnitRequest], want: &str| {
        let err = restore_units(
            &h.conn, &h.paths, &h.config, "MU-1", reqs, &options, false, DEVICE, 4096, false,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains(want), "{want}: {err}");
        assert!(!err.contains("open"), "the drive was reached: {err}");
        assert_eq!(restore_contacts(&h.conn), 0, "{err}");
        assert_eq!(count(&h.conn, "SELECT COUNT(*) FROM restores"), 0, "{err}");
    };

    let mut reqs = requests(&["ua", "ub", "uc"], root.path());
    reqs[2].unit = "nope".into();
    refused(&reqs, "nope");

    let mut reqs = requests(&["ua", "ub", "uc"], root.path());
    reqs[2].unit = "ua".into();
    refused(&reqs, "more than once");

    let mut reqs = requests(&["ua", "ub", "uc"], root.path());
    reqs[2].dest_dir = reqs[0].dest_dir.clone();
    refused(&reqs, "same destination");

    let reqs = requests(&["ua", "ub", "uc"], root.path());
    fs::create_dir_all(&reqs[1].dest_dir).unwrap();
    fs::write(Path::new(&reqs[1].dest_dir).join("x"), b"x").unwrap();
    refused(&reqs, "is not empty");
    fs::remove_dir_all(&reqs[1].dest_dir).unwrap();

    // The units restore one after another and every restored unit stays, so
    // the set needs the sum of its units: one byte short of it is refused
    // though each unit alone fits, and exactly the sum is not.
    let planned = plan_units(&h.conn, &h.config, "MU-1", &reqs, &options).unwrap();
    let sizes: Vec<i64> = planned.iter().map(|p| p.plan.bytes).collect();
    let total: i64 = sizes.iter().sum();
    assert!(sizes.iter().all(|s| *s > 0 && *s < total - 1), "{sizes:?}");
    {
        let _free = super::RestoreFreeOverride::set(total - 1);
        refused(&reqs, "not enough disk space");
    }
    let _free = super::RestoreFreeOverride::set(total);
    let mut bay = h.bay("MU-1", &[]);
    let report = run_set(&h, "MU-1", &reqs, false, &mut bay).unwrap();
    assert_eq!(report.failed(), 0, "{report:?}");
}

/// The wrong tape in the drive refuses the whole set at the one
/// corroboration, before any slice is read: one contact, and every unit's row
/// failed, naming the tape.
#[test]
fn the_wrong_tape_refuses_the_set_after_one_file0_read() {
    let mut h = three("MU-1");
    h.volume("MU-OTHER");
    let other = h.tapes["MU-OTHER"].files[0].clone();
    h.tapes.get_mut("MU-1").unwrap().files[0] = other;
    let root = TempDir::new().unwrap();
    let (mut bay, reads) = spied_bay(&h, "MU-1");
    let err = run_set(
        &h,
        "MU-1",
        &requests(&["ua", "ub", "uc"], root.path()),
        false,
        &mut bay,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("wrong tape"), "{err}");
    assert_eq!(*reads.borrow(), vec![0], "only File 0 was read");
    assert_eq!(restore_contacts(&h.conn), 1);
    let all = rows(&h.conn).unwrap();
    assert_eq!(all.len(), 3);
    assert!(all.iter().all(|r| r.outcome == "failed"), "{all:?}");
}

/// A dry run plans the whole set -- the version and slices of each unit, in
/// tape order -- and touches nothing: no drive, no contact, no row.
#[test]
fn a_dry_run_plans_the_set_without_the_drive() {
    let h = three("MU-1");
    let root = TempDir::new().unwrap();
    let report = restore_units(
        &h.conn,
        &h.paths,
        &h.config,
        "MU-1",
        &requests(&["uc", "ub", "ua"], root.path()),
        &RestoreOptions::default(),
        false,
        DEVICE,
        4096,
        true,
    )
    .expect("a dry run opens no drive: the device is a path nothing is at");
    assert!(report.dry_run);
    let names: Vec<&str> = report.units.iter().map(|u| u.unit_name.as_str()).collect();
    assert_eq!(names, ["ua", "ub", "uc"], "tape order");
    let slices: Vec<usize> = report.units.iter().map(|u| u.slices).collect();
    let want: Vec<usize> = ["ua", "ub", "uc"]
        .iter()
        .map(|u| h.num_slices(u) as usize)
        .collect();
    assert_eq!(slices, want);
    assert_eq!(restore_contacts(&h.conn), 0);
    assert_eq!(rows(&h.conn).unwrap().len(), 0);
    assert!(!root.path().join("ua").exists(), "nothing was made");
}

// ── `restore volume` as the command runs it (ADR-0012 2026-10-06 item 23) ──

/// `restore volume <label> --to <to> [--unit ...]`.
fn volume_command(label: &str, to: &Path, units: &[&str]) -> RestoreCommands {
    RestoreCommands::Volume {
        label: Some(label.to_string()),
        to: to.to_string_lossy().into_owned(),
        units: units.iter().map(|u| u.to_string()).collect(),
        device: Some(DEVICE.to_string()),
        scratch: None,
        overwrite: false,
        no_space_check: false,
        no_file_check: false,
        fail_fast: false,
        key: Vec::new(),
        dry_run: false,
    }
}

/// The set `restore volume LABEL --to TO [--unit ...]` restores, as the
/// command builds it from the catalog, then the one pass over `bay`.
fn restore_volume(
    h: &Home,
    label: &str,
    to: &Path,
    units: &[&str],
    fail_fast: bool,
    bay: &mut TestBay,
) -> Result<UnitsReport> {
    let names: Vec<String> = if units.is_empty() {
        units_on_volume(&h.conn, label)?
    } else {
        units.iter().map(|u| u.to_string()).collect()
    };
    let reqs = unit_requests(&names, &to.to_string_lossy())?;
    run_set(h, label, &reqs, fail_fast, bay)
}

/// THE acceptance test for the command half of #398: with no `--unit`, every
/// unit on the volume is restored, each into `--to/<unit>`, in ONE pass --
/// File 0 once, every slice in ascending position -- under ONE contact
/// recorded as `restore volume`, with a `unit` row for each.
#[test]
fn restore_volume_restores_every_unit_into_its_own_directory_in_one_pass() {
    let h = three("RV-1");
    let root = TempDir::new().unwrap();
    let to = root.path().join("out");
    let (mut bay, reads) = spied_bay(&h, "RV-1");
    let report = restore_volume(&h, "RV-1", &to, &[], false, &mut bay).expect("restore volume");
    assert_eq!(report.failed(), 0, "{report:?}");
    assert_set_restored(&h, &to, &["ua", "ub", "uc"]);
    let mut entries: Vec<String> = fs::read_dir(&to)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(entries, ["ua", "ub", "uc"], "one directory per unit");

    assert_eq!(bay.releases, 0);
    let reads = reads.borrow();
    assert_eq!(reads[0], 0, "File 0 first: {reads:?}");
    assert_ascending(&reads);
    let slices: i64 = ["ua", "ub", "uc"].iter().map(|u| h.num_slices(u)).sum();
    assert_eq!(reads.len() as i64, slices + 1, "{reads:?}");

    let ops: Vec<String> = contact_rows(&h.conn).into_iter().map(|c| c.0).collect();
    assert_eq!(ops, ["restore volume"], "one contact, under its own name");
    let all = rows(&h.conn).unwrap();
    let dests: Vec<String> = all.iter().map(|r| r.destination.clone()).collect();
    let want: Vec<String> = ["ua", "ub", "uc"]
        .iter()
        .map(|u| to.join(u).to_string_lossy().into_owned())
        .collect();
    assert_eq!(dests, want);
    assert!(
        all.iter().all(|r| r.kind == "unit" && r.outcome == "ok"),
        "{all:?}"
    );
    assert!(
        all.iter().all(|r| r.files_restored.unwrap_or(0) > 0),
        "each row counts the members restored: {all:?}"
    );
}

/// `--unit` names the set: only those units are read and restored, the
/// others neither read nor created. A dry run of the same set opens no drive.
#[test]
fn restore_volume_unit_restores_only_the_units_named() {
    let h = three("RV-2");
    let root = TempDir::new().unwrap();
    let to = root.path().join("out");

    let mut dry = volume_command("RV-2", &to, &["uc", "ua"]);
    if let RestoreCommands::Volume { dry_run, .. } = &mut dry {
        *dry_run = true;
    }
    run(&h.conn, &h.paths, &h.config, &dry, false, false)
        .expect("a dry run opens no drive: the device is a path nothing is at");
    assert_eq!(restore_contacts(&h.conn), 0);
    assert!(!to.exists(), "a dry run made nothing");

    let (mut bay, reads) = spied_bay(&h, "RV-2");
    let report = restore_volume(&h, "RV-2", &to, &["uc", "ua"], false, &mut bay).unwrap();
    assert_eq!(report.failed(), 0, "{report:?}");
    assert_set_restored(&h, &to, &["ua", "uc"]);
    assert!(!to.join("ub").exists(), "ub was not asked for");
    let b_slices: Vec<u32> = (1..=h.num_slices("ub"))
        .map(|n| h.position("RV-2", "ub", n) as u32)
        .collect();
    assert!(!b_slices.is_empty());
    let reads = reads.borrow();
    assert!(
        reads.iter().all(|n| !b_slices.contains(n)),
        "ub's slices were read: {reads:?}"
    );
    let a_first = h.position("RV-2", "ua", 1) as u32;
    assert!(
        reads.contains(&a_first),
        "positive control: ua's slices are read: {reads:?}"
    );
    let names: Vec<String> = rows(&h.conn)
        .unwrap()
        .into_iter()
        .map(|r| r.unit_name.unwrap())
        .collect();
    assert_eq!(names, ["ua", "uc"], "tape order, not the order asked");
}

/// A unit that fails makes the command fail -- the count it says is the
/// report's -- while the others are still restored; with `--fail-fast` the
/// units after it are not attempted. (The command's sentence "N of M units
/// not restored from LABEL" is made in `cli::restore::run` from
/// `UnitsReport::failed`, behind the real drive; what is asserted here is the
/// count and the report's own summary line.)
#[test]
fn restore_volume_fails_when_any_unit_fails() {
    let mut h = three("RV-3");
    damage_first_slice(&mut h, "RV-3", "ub");
    let root = TempDir::new().unwrap();
    let to = root.path().join("out");
    let mut bay = h.bay("RV-3", &[]);
    let report = restore_volume(&h, "RV-3", &to, &[], false, &mut bay).unwrap();
    assert_eq!(report.failed(), 1, "{report:?}");
    assert_eq!(
        units_report_lines(&report).last().unwrap(),
        "2 of 3 units restored from RV-3"
    );
    assert_restored(&h, "ua", &to.join("ua"), &[]);
    assert_restored(&h, "uc", &to.join("uc"), &[]);

    let to = root.path().join("fast");
    let mut bay = h.bay("RV-3", &[]);
    let report = restore_volume(&h, "RV-3", &to, &[], true, &mut bay).unwrap();
    assert_eq!(report.failed(), 2, "{report:?}");
    assert_eq!(
        units_report_lines(&report).last().unwrap(),
        "1 of 3 units restored from RV-3"
    );
    assert!(!to.join("uc").exists(), "--fail-fast stopped before uc");
}

/// The report prints one line per unit in TAPE order -- asked for backwards
/// here -- saying how each ended, then a summary.
#[test]
fn the_report_is_in_tape_order_and_names_each_outcome() {
    let mut h = three("RV-4");
    damage_first_slice(&mut h, "RV-4", "ub");
    let root = TempDir::new().unwrap();
    let reqs = requests(&["uc", "ub", "ua"], root.path());
    let mut bay = h.bay("RV-4", &[]);
    let report = run_set(&h, "RV-4", &reqs, false, &mut bay).unwrap();
    let lines = units_report_lines(&report);
    let at = |prefix: &str| {
        lines
            .iter()
            .position(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no line starts {prefix:?}: {lines:#?}"))
    };
    let (a, b, c) = (
        at("restored \"ua\" v1"),
        at("FAILED \"ub\" v1"),
        at("restored \"uc\" v1"),
    );
    assert!(a < b && b < c, "tape order: {lines:#?}");
    assert!(lines[b].contains("INCOMPLETE"), "{lines:#?}");
    assert_eq!(lines.last().unwrap(), "2 of 3 units restored from RV-4");
}

/// No volume of that label, or two units of which one would land inside the
/// other's directory: refused before the drive, through the command.
#[test]
fn restore_volume_refuses_before_the_drive() {
    let mut h = Home::new("64K", None, 1);
    h.unit(TENANT, "nest", &[Item::file("a.txt", b"alpha".to_vec())]);
    // A unit NAMED inside another's name, its folder elsewhere (the catalog
    // refuses a folder inside a unit's folder).
    let folder = h.sources.join("inner-folder");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("b.txt"), b"bravo").unwrap();
    crate::unit::init_unit_with_config(
        &h.conn,
        &h.config,
        folder.to_str().unwrap(),
        TENANT,
        Some("nest/inner"),
        &[],
        None,
    )
    .unwrap();
    h.stage("nest");
    h.stage("nest/inner");
    h.volume("RV-5");
    h.write("RV-5");
    let root = TempDir::new().unwrap();
    let go = |label: &str| {
        run(
            &h.conn,
            &h.paths,
            &h.config,
            &volume_command(label, root.path(), &[]),
            false,
            false,
        )
        .unwrap_err()
        .to_string()
    };
    let err = go("NOPE");
    assert!(err.contains("no volume \"NOPE\""), "{err}");
    let err = go("RV-5");
    assert!(err.contains("inside"), "{err}");
    assert!(err.contains("Nothing was read from tape"), "{err}");
    assert_eq!(restore_contacts(&h.conn), 0, "the drive was opened");
    assert_eq!(rows(&h.conn).unwrap().len(), 0);
}

// ── ADR-0012 2026-10-06 item 15: the requires fence ──
//
// Every reader checks File 0's `layout_version`, `magic` and `requires` and
// refuses a tape needing a feature it does not know. `restore raw` and
// `catalog rebuild` always did; restore of slices read File 0 for identity
// only, so a tape fenced with `requires = ["parts"]` (#409) would have
// restored one part of a divided version as the whole of it. The refusal
// rides the contact's one File 0 read, so these tests also pin that nothing
// but that read touched the tape.

/// How a Layout 3 File 0 states what it requires (D1): nothing.
const STATED: &str = "\nrequires = []\n";
const UNKNOWN: &str = "\nrequires = [\"unknown-feature\"]\n";

/// File 0 of `label`'s tape with its first `from` replaced by `to`
/// ([`crate::volume::format::rewrite_file0`], which asserts `from` is there:
/// the proof that the real File 0 says what these tests assume).
fn rewrite(h: &mut Home, label: &str, from: &str, to: &str) {
    let tape = h.tapes.get_mut(label).unwrap();
    tape.files[0] = crate::volume::format::rewrite_file0(&tape.files[0], from, to);
}

/// One unit `rq-unit` on volume `RQ-1`, written by the real write.
fn one() -> Home {
    let mut h = Home::new("64K", None, 1);
    h.unit(
        TENANT,
        "rq-unit",
        &[
            Item::file("a.txt", b"alpha".to_vec()),
            Item::file("b.bin", noise(100_000, 3)),
        ],
    );
    h.stage("rq-unit");
    h.volume("RQ-1");
    h.write("RQ-1");
    h
}

/// `restore unit` / `restore file` of `rq-unit` from `RQ-1` through a spied
/// bay: the result, the destination and what was read.
fn restore_one(h: &Home, as_file: bool) -> (Result<super::RestoreReport>, TempDir, Reads) {
    let (mut bay, reads) = spied_bay(h, "RQ-1");
    let what = if as_file {
        What::File("a.txt")
    } else {
        What::Unit
    };
    let (r, dest) = h.restore(
        "rq-unit",
        what,
        "RQ-1",
        &RestoreOptions::default(),
        &mut bay,
    );
    (r, dest, reads)
}

/// File 0 was read once and nothing else was: no slice, no second look at
/// File 0.
fn assert_only_file0_was_read(reads: &Reads) {
    assert_eq!(*reads.borrow(), vec![0], "File 0, once, and nothing else");
}

/// The refusal is the one `restore raw` and `catalog rebuild` give: the text
/// `check_id_thunk_readable` makes of the same File 0.
fn assert_unknown_feature_refusal(h: &Home, err: &str) {
    assert!(err.contains("\"unknown-feature\""), "{err}");
    assert!(err.contains("newer tapectl"), "{err}");
    assert!(err.contains("RESTORE.sh"), "{err}");
    assert!(err.contains("Nothing has been read past it"), "{err}");
    let file0: Vec<u8> = h.tapes["RQ-1"].files[0]
        .iter()
        .copied()
        .filter(|b| *b != 0)
        .collect();
    let raw = crate::volume::format::check_id_thunk_readable(&String::from_utf8(file0).unwrap())
        .unwrap_err()
        .to_string();
    assert_eq!(err, raw, "the same text raw and rebuild give");
}

/// THE acceptance test for `restore unit` and `restore file` (#476): a tape
/// whose File 0 requires a feature this tapectl does not know is refused by
/// name at the contact, before a key is loaded or a slice read. The contact
/// and the `restores` row record the refusal, as they do a wrong tape.
#[test]
fn restore_unit_and_file_refuse_a_tape_requiring_an_unknown_feature() {
    for as_file in [false, true] {
        let mut h = one();
        rewrite(&mut h, "RQ-1", STATED, UNKNOWN);
        let (r, dest, reads) = restore_one(&h, as_file);
        let err = r.unwrap_err().to_string();
        let what = format!("file={as_file}");
        assert_unknown_feature_refusal(&h, &err);
        assert_only_file0_was_read(&reads);
        assert!(
            !dest.path().join(SCRATCH_NAME).exists(),
            "{what}: no scratch directory"
        );
        assert_eq!(
            fs::read_dir(dest.path()).unwrap().count(),
            0,
            "{what}: nothing was made in the destination"
        );
        let all = rows(&h.conn).unwrap();
        assert_eq!(all.len(), 1, "{what}: {all:?}");
        assert_eq!(all[0].outcome, "failed", "{what}");
        assert!(
            all[0]
                .error
                .as_deref()
                .is_some_and(|e| e.contains("\"unknown-feature\"")),
            "{what}: {all:?}"
        );
        assert_eq!(all[0].slices_read, Some(0), "{what}");
        let (_, outcome) = only_contact(&h.conn);
        assert_eq!(outcome.as_deref(), Some("failed"), "{what}");
    }
}

/// `restore volume`: one corroboration refuses the whole set, so every unit's
/// row is `failed` with the reason and no unit's destination is made, on one
/// contact with one File 0 read.
#[test]
fn restore_volume_refuses_a_tape_requiring_an_unknown_feature() {
    let mut h = three("RQ-1");
    rewrite(&mut h, "RQ-1", STATED, UNKNOWN);
    let root = TempDir::new().unwrap();
    let (mut bay, reads) = spied_bay(&h, "RQ-1");
    let err = run_set(
        &h,
        "RQ-1",
        &requests(&["ua", "ub", "uc"], root.path()),
        false,
        &mut bay,
    )
    .unwrap_err()
    .to_string();
    assert_unknown_feature_refusal(&h, &err);
    assert_only_file0_was_read(&reads);
    let all = rows(&h.conn).unwrap();
    assert_eq!(all.len(), 3, "{all:?}");
    for r in &all {
        assert_eq!(r.outcome, "failed", "{r:?}");
        assert!(
            r.error
                .as_deref()
                .is_some_and(|e| e.contains("\"unknown-feature\"")),
            "{r:?}"
        );
    }
    for u in ["ua", "ub", "uc"] {
        assert!(!root.path().join(u).exists(), "{u} was made");
    }
    let (operation, outcome) = only_contact(&h.conn);
    assert_eq!(operation, "restore volume");
    assert_eq!(outcome.as_deref(), Some("failed"));
}

/// A `requires` that is not a list of strings cannot be shown to be readable
/// (`check_id_thunk_readable`): refused on both paths with the parse error,
/// never restored as if it required nothing.
#[test]
fn a_requires_that_is_not_a_list_is_refused() {
    let mut h = one();
    rewrite(&mut h, "RQ-1", STATED, "\nrequires = \"parts\"\n");
    let (r, dest, reads) = restore_one(&h, false);
    let err = r.unwrap_err().to_string();
    assert!(err.contains("TOML parse failed"), "{err}");
    assert!(err.contains("requires"), "{err}");
    assert_only_file0_was_read(&reads);
    assert!(!dest.path().join(SCRATCH_NAME).exists());

    let mut h = three("RQ-1");
    rewrite(&mut h, "RQ-1", STATED, "\nrequires = \"parts\"\n");
    let root = TempDir::new().unwrap();
    let (mut bay, reads) = spied_bay(&h, "RQ-1");
    let err = run_set(
        &h,
        "RQ-1",
        &requests(&["ua", "ub", "uc"], root.path()),
        false,
        &mut bay,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("TOML parse failed"), "{err}");
    assert_only_file0_was_read(&reads);
    assert!(rows(&h.conn).unwrap().iter().all(|r| r.outcome == "failed"));
}

/// Another layout is the same refusal (the rest of `FormatClaims::refusal`,
/// which the restore path reaches).
#[test]
fn another_layout_is_refused_on_restore() {
    let mut h = one();
    rewrite(
        &mut h,
        "RQ-1",
        "\nlayout_version = 3\n",
        "\nlayout_version = 2\n",
    );
    let (r, dest, reads) = restore_one(&h, false);
    let err = r.unwrap_err().to_string();
    assert!(err.contains("layout_version = 2"), "{err}");
    assert_only_file0_was_read(&reads);
    assert_eq!(fs::read_dir(dest.path()).unwrap().count(), 0);
}

/// ADR-0012 item 258(d): a File 0 that states no `requires` is refused on
/// both paths, naming the field, with only File 0 read.
#[test]
fn a_tape_that_states_no_requires_is_refused() {
    let mut h = one();
    rewrite(&mut h, "RQ-1", STATED, "\n");
    let (r, dest, reads) = restore_one(&h, false);
    let err = r.unwrap_err().to_string();
    assert!(err.contains("states no `requires`"), "{err}");
    assert_only_file0_was_read(&reads);
    assert!(!dest.path().join(SCRATCH_NAME).exists());

    let mut h = three("RQ-1");
    rewrite(&mut h, "RQ-1", STATED, "\n");
    let root = TempDir::new().unwrap();
    let (mut bay, reads) = spied_bay(&h, "RQ-1");
    let err = run_set(
        &h,
        "RQ-1",
        &requests(&["ua", "ub", "uc"], root.path()),
        false,
        &mut bay,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("states no `requires`"), "{err}");
    assert_only_file0_was_read(&reads);
    assert!(rows(&h.conn).unwrap().iter().all(|r| r.outcome == "failed"));
}

/// The positive controls: a File 0 that states `requires = []`, as Layout 3
/// does, or carries a key nobody requires, restores exactly as before -- on
/// the unit path and the volume path -- and the slices ARE read, so the
/// refusals above are the fence and not a tape that cannot be restored at all.
#[test]
fn a_tape_that_requires_nothing_restores_as_before() {
    for (what, to) in [
        ("states requires = [], as Layout 3 does", STATED),
        (
            "carries a key nobody requires",
            "\nrequires = []\nfuture_key = \"x\"\n",
        ),
    ] {
        let mut h = one();
        rewrite(&mut h, "RQ-1", STATED, to);
        let (r, dest, reads) = restore_one(&h, false);
        r.unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_restored(&h, "rq-unit", dest.path(), &[]);
        assert!(
            reads.borrow().iter().any(|p| *p > 0),
            "{what}: the slices were read"
        );
        assert_eq!(rows(&h.conn).unwrap()[0].outcome, "ok", "{what}");

        let mut h = three("RQ-1");
        rewrite(&mut h, "RQ-1", STATED, to);
        let root = TempDir::new().unwrap();
        let (mut bay, reads) = spied_bay(&h, "RQ-1");
        let report = run_set(
            &h,
            "RQ-1",
            &requests(&["ua", "ub", "uc"], root.path()),
            false,
            &mut bay,
        )
        .unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(report.failed(), 0, "{what}: {report:?}");
        assert_set_restored(&h, root.path(), &["ua", "ub", "uc"]);
        assert!(
            reads.borrow().iter().any(|p| *p > 0),
            "{what}: the slices were read"
        );
    }
}

/// A File 0 that cannot be read at all is an absence, and proceeds as it
/// did: the fence turns away a tape that SAYS it needs something, not one
/// that said nothing. (An empty tape fails later, on its own terms -- never
/// on `requires`.)
#[test]
fn an_unreadable_file0_is_not_a_new_refusal() {
    let mut h = one();
    h.tapes.get_mut("RQ-1").unwrap().files.clear();
    let (r, _dest, _reads) = restore_one(&h, false);
    let err = r.unwrap_err().to_string();
    assert!(!err.contains("requires"), "{err}");
    assert!(!err.contains("newer tapectl"), "{err}");
    assert!(!err.is_empty());
    // The positive control: the same tape with its File 0 says it needs
    // something, and is refused by name.
    let mut h = one();
    rewrite(&mut h, "RQ-1", STATED, UNKNOWN);
    let (r, _dest, _reads) = restore_one(&h, false);
    assert!(r.unwrap_err().to_string().contains("requires"));
}

// ── the tape that is loaded, named by no label (#435) ──

/// `restore unit` with no `--from`: File 0 is read ONCE (by `identify`) and
/// the contact check is served from those bytes, so the fence adds no read;
/// it refuses ahead of the key and every slice, on the one contact.
#[test]
fn restore_with_no_from_refuses_a_tape_requiring_an_unknown_feature() {
    let mut h = one();
    rewrite(&mut h, "RQ-1", STATED, UNKNOWN);
    let reads: Reads = Rc::default();
    let mut store = Spy {
        inner: copy_of(&h.tapes["RQ-1"]),
        reads: reads.clone(),
    };
    let dest = TempDir::new().unwrap();
    let dest_dir = dest.path().to_string_lossy().into_owned();

    let err = restore_unit_from_loaded_store(
        &h.conn,
        &h.paths,
        &h.config,
        "rq-unit",
        RestoreTarget::Unit {
            dest_dir: &dest_dir,
        },
        &RestoreOptions::default(),
        None,
        &mut store,
        h.site(),
    )
    .unwrap_err()
    .to_string();

    assert!(err.contains("\"unknown-feature\""), "{err}");
    assert!(err.contains("newer tapectl"), "{err}");
    assert_only_file0_was_read(&reads);
    assert!(!dest.path().join(SCRATCH_NAME).exists());
    let contacts = contact_rows(&h.conn);
    assert_eq!(contacts.len(), 1, "{contacts:?}");
    assert_eq!(contacts[0].1, Some(volume_id(&h.conn, "RQ-1")));
    assert_eq!(contacts[0].2.as_deref(), Some("failed"));
    assert!(
        contacts[0]
            .3
            .as_deref()
            .is_some_and(|d| d.contains("unknown-feature")),
        "{contacts:?}"
    );
}

/// `restore volume` with no label: the same, for the set.
#[test]
fn restore_volume_with_no_label_refuses_a_tape_requiring_an_unknown_feature() {
    let mut h = one();
    rewrite(&mut h, "RQ-1", STATED, UNKNOWN);
    let reads: Reads = Rc::default();
    let mut store = Spy {
        inner: copy_of(&h.tapes["RQ-1"]),
        reads: reads.clone(),
    };
    let root = TempDir::new().unwrap();
    let to = root.path().join("out");

    let err = restore_units_from_loaded_store(
        &h.conn,
        &h.paths,
        &h.config,
        &[],
        &to.to_string_lossy(),
        &RestoreOptions::default(),
        false,
        &mut store,
        volume_site(&h),
    )
    .unwrap_err()
    .to_string();

    assert!(err.contains("\"unknown-feature\""), "{err}");
    assert_only_file0_was_read(&reads);
    assert!(!to.join("rq-unit").exists());
    let (operation, outcome) = only_contact(&h.conn);
    assert_eq!(operation, "restore volume");
    assert_eq!(outcome.as_deref(), Some("failed"));
    let restores = rows(&h.conn).unwrap();
    assert_eq!(restores.len(), 1, "{restores:?}");
    assert_eq!(restores[0].outcome, "failed");
}

/// `restore volume` with no label: the units are listed from the volume File
/// 0 names, planned and restored from it, each as it was; the report names
/// the volume.
#[test]
fn restore_volume_with_no_label_restores_the_units_of_the_loaded_volume() {
    let h = three("RL-MANY");
    let mut store = copy_of(&h.tapes["RL-MANY"]);
    let root = TempDir::new().unwrap();
    let to = root.path().join("out");

    let report = restore_units_from_loaded_store(
        &h.conn,
        &h.paths,
        &h.config,
        &[],
        &to.to_string_lossy(),
        &RestoreOptions::default(),
        false,
        &mut store,
        volume_site(&h),
    )
    .expect("the contact check passes; each unit's own failure is its outcome");

    assert_eq!(report.volume_label, "RL-MANY");
    let names: Vec<&str> = report.units.iter().map(|u| u.unit_name.as_str()).collect();
    assert_eq!(
        names,
        ["ua", "ub", "uc"],
        "every unit written to the loaded volume"
    );
    assert!(
        report
            .units
            .iter()
            .all(|u| u.attempted && u.error.is_none()),
        "{report:?}"
    );
    assert_set_restored(&h, &to, &["ua", "ub", "uc"]);
    let (operation, outcome) = only_contact(&h.conn);
    assert_eq!(operation, "restore volume");
    assert_eq!(outcome.as_deref(), Some("ok"));
    // ONE contact for the whole run, naming the volume (issue #442).
    let contacts = contact_rows(&h.conn);
    assert_eq!(
        contacts[0].1,
        Some(volume_id(&h.conn, "RL-MANY")),
        "{contacts:?}"
    );
}

/// The same for a tape that is another catalog's volume: refused, nothing
/// planned past the volume lookup.
#[test]
fn restore_volume_with_no_label_refuses_a_tape_the_catalog_does_not_know() {
    let mut h = Home::new("64K", None, 1);
    h.unit(TENANT, "rl-unit", &[Item::file("a.txt", b"alpha".to_vec())]);
    h.stage("rl-unit");
    h.volume("RL-OTHER");
    h.write("RL-OTHER");
    // A volume of a different catalog: made in another home.
    let mut stranger = Home::new("64K", None, 1);
    stranger.volume("RL-STRANGER");
    let mut store = copy_of(&stranger.tapes["RL-STRANGER"]);
    let root = TempDir::new().unwrap();
    let to = root.path().join("out");

    let err = restore_units_from_loaded_store(
        &h.conn,
        &h.paths,
        &h.config,
        &[],
        &to.to_string_lossy(),
        &RestoreOptions::default(),
        false,
        &mut store,
        volume_site(&h),
    )
    .unwrap_err()
    .to_string();

    assert!(err.contains("RL-STRANGER"), "{err}");
    assert!(err.contains("no volume with that uuid"), "{err}");
    assert!(!to.exists());
    // The drive was opened and File 0 read: one `failed` contact with the
    // reason, naming no volume (issue #442).
    let contacts = contact_rows(&h.conn);
    assert_eq!(contacts.len(), 1, "{contacts:?}");
    assert_eq!(contacts[0].0, "restore volume");
    assert_eq!(contacts[0].1, None);
    assert_eq!(contacts[0].2.as_deref(), Some("failed"));
    assert!(
        contacts[0]
            .3
            .as_deref()
            .is_some_and(|d| d.contains("no volume with that uuid")),
        "{contacts:?}"
    );
    // The positive control: the catalog's own volume, loaded, is accepted.
    let mut own = copy_of(&h.tapes["RL-OTHER"]);
    let report = restore_units_from_loaded_store(
        &h.conn,
        &h.paths,
        &h.config,
        &[],
        &to.to_string_lossy(),
        &RestoreOptions::default(),
        false,
        &mut own,
        volume_site(&h),
    )
    .unwrap();
    assert_eq!(report.volume_label, "RL-OTHER");
}

/// `restore volume` with no label, asked for a unit the catalog does not
/// have on the volume: refused by the set's planning, after File 0 -- one
/// `failed` contact naming the volume (issue #442).
#[test]
fn restore_volume_with_no_label_records_a_plan_refusal_as_a_failed_contact() {
    let h = three("RL-SETPLAN");
    let mut store = copy_of(&h.tapes["RL-SETPLAN"]);
    let root = TempDir::new().unwrap();
    let to = root.path().join("out");

    let err = restore_units_from_loaded_store(
        &h.conn,
        &h.paths,
        &h.config,
        &["no-such-unit".to_string()],
        &to.to_string_lossy(),
        &RestoreOptions::default(),
        false,
        &mut store,
        volume_site(&h),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("no-such-unit"), "{err}");

    let contacts = contact_rows(&h.conn);
    assert_eq!(contacts.len(), 1, "{contacts:?}");
    assert_eq!(contacts[0].0, "restore volume");
    assert_eq!(contacts[0].1, Some(volume_id(&h.conn, "RL-SETPLAN")));
    assert_eq!(contacts[0].2.as_deref(), Some("failed"));
    assert!(
        contacts[0]
            .3
            .as_deref()
            .is_some_and(|d| d.contains("no-such-unit")),
        "{contacts:?}"
    );
    assert!(!to.exists(), "nothing was restored");
}
