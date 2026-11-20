//! Tests of tapectl's restores of a version held across cartridges (#478;
//! `volume-format-v3.md` §5.3, ADR-0012 items 260, 266 and 288), ported onto
//! the tar reader in T4.8.
//!
//! The ranges are made by the real write: a unit whose policy lets it span, a
//! drive whose early warning comes where a test arms it ([`Warn`], as the
//! span proofs arm it), and `volume write` over an in-memory medium
//! ([`super::fixture::Home`] makes everything else). No catalog row, slice
//! or tape file is made by hand, and no tape is damaged but by flipping a
//! byte of its ciphertext.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use rusqlite::{params, Connection};
use tempfile::TempDir;

use super::fixture::{
    assert_restored, files, listing, noise, Home, Item, What, BS, DEVICE, TENANT,
};
use super::repair_tests::tests::{Reply, TestBay};
use super::{
    plan_restore, plan_units, restore_unit, restore_units_from_bay, select_write_positions, span,
    unit_requests, units_on_volume, RestoreOptions, RestoreTarget, SCRATCH_NAME,
};
use crate::error::Result;
use crate::store::{CapacityReport, EarlyWarning, MemStore, Store, TapeStore};
use crate::tape::contact::ContactSlot;
use crate::tape::fake::{FakeTape, Op};
use crate::volume::filetable::{self, Kind, Row};
use crate::volume::write::{self, ContactStore};

/// The slice size of every unit here: a member longer than it is a run.
const SLICE: u64 = 64 * 1024;
const A: &str = "SPR-A";
const B: &str = "SPR-B";
const C: &str = "SPR-C";
const UNIT: &str = "span-unit";

// ── the drive: an early warning armed where a test says ──

/// When the drive's early warning comes on a write.
#[derive(Debug, Clone, Copy)]
enum Warn {
    /// Never: the tape seals full.
    Never,
    /// One byte into the tape file of the unit's slice `n`, wherever the plan
    /// puts it: its first block reaches the warning, so the tape ends after
    /// that slice (the short seal).
    InSlice(i64),
}

/// The drive a write sees: the volume's medium, its early warning armed as
/// [`Warn`] says.
struct Drive<'c> {
    medium: MemStore,
    conn: &'c Connection,
    label: String,
    warn: Warn,
}

impl Store for Drive<'_> {
    fn capacity(&mut self) -> Result<CapacityReport> {
        self.medium.capacity()
    }
    fn execute(&mut self, src: &mut dyn Read, len: u64, sync: bool) -> Result<u64> {
        if let Warn::InSlice(n) = self.warn {
            let at = planned_position(self.conn, &self.label, UNIT, n);
            if at == Some(self.medium.files.len() as i64) {
                let before = self
                    .medium
                    .files
                    .iter()
                    .map(|f| f.len() as u64)
                    .sum::<u64>();
                let old = std::mem::replace(&mut self.medium, MemStore::new(BS));
                let mut armed = MemStore::new(BS).with_early_warning_after(before + 1);
                armed.files = old.files;
                armed.syncs = old.syncs;
                self.medium = armed;
                self.warn = Warn::Never;
            }
        }
        self.medium.execute(src, len, sync)
    }
    fn early_warning(&self) -> Option<EarlyWarning> {
        self.medium.early_warning()
    }
    fn read_file(&mut self, position: u32, sink: &mut dyn Write) -> Result<u64> {
        self.medium.read_file(position, sink)
    }
    fn reposition_for_resume(&mut self, file_index: u32) -> Result<()> {
        self.medium.reposition_for_resume(file_index)
    }
    fn reposition_at_most(&mut self, file_index: u32) -> Result<u32> {
        self.medium.reposition_at_most(file_index)
    }
    fn blank_at_bot(&mut self) -> Result<bool> {
        self.medium.blank_at_bot()
    }
    fn eod_at_or_before(&mut self, position: u32) -> Result<bool> {
        self.medium.eod_at_or_before(position)
    }
    fn files_on_medium(&mut self) -> Result<Option<u32>> {
        self.medium.files_on_medium()
    }
}

/// Where the plan put `unit`'s slice `n` on `label`, once it is planned.
fn planned_position(conn: &Connection, label: &str, unit: &str, n: i64) -> Option<i64> {
    conn.query_row(
        "SELECT CAST(wp.position AS INTEGER) FROM write_positions wp
           JOIN writes w ON w.id = wp.write_id
           JOIN volumes v ON v.id = w.volume_id
           JOIN stage_slices sl ON sl.id = wp.stage_slice_id
           JOIN snapshots s ON s.id = w.snapshot_id
           JOIN units u ON u.id = s.unit_id
          WHERE v.label = ?1 AND u.name = ?2 AND sl.slice_number = ?3",
        params![label, unit, n],
        |r| r.get(0),
    )
    .ok()
}

/// `volume write` of `label` with the drive's early warning as `warn`, then
/// the release `volume write` runs after a write that stood.
fn write_warned(h: &mut Home, label: &str, warn: Warn) {
    let medium = h.tapes.remove(label).expect("the volume was made");
    let mut drive = Drive {
        medium,
        conn: &h.conn,
        label: label.to_string(),
        warn,
    };
    let mut slot = ContactSlot::empty();
    let r = write::volume_write_contacted(
        &h.conn,
        &h.paths,
        &h.config,
        label,
        DEVICE,
        BS,
        false,
        false,
        false,
        false,
        true,
        &mut slot,
        ContactStore::Injected(&mut drive),
        None,
    );
    let r = slot.finish_result(r);
    h.tapes.insert(label.to_string(), drive.medium);
    r.unwrap_or_else(|e| panic!("volume write {label}: {e}"));
    crate::cli::volume::release_staging_after_write(&h.conn, &h.config, label, false);
}

// ── the units and the tapes ──

/// What the unit holds: files of about 40 KB (one to a slice at 64K), a long
/// one that is a run of slices of its own, and one in a folder.
fn tree() -> Vec<Item> {
    let mut items = files("part", 5, 40_000, 3);
    items.push(Item::file("zz-long.bin", noise(150_000, 77)));
    items.push(Item::file("sub/deep.txt", b"deep in a folder\n".to_vec()));
    items
}

/// A home with the unit staged (spannable, 64K slices) and `labels` made as
/// volumes in that order; nothing written. `min_copies` is the copy floor.
fn staged(labels: &[&str], items: &[Item], min_copies: i32) -> Home {
    let mut h = Home::new("64K", None, min_copies);
    h.unit(TENANT, UNIT, items);
    h.stage(UNIT);
    for label in labels {
        h.volume(label);
    }
    h
}

/// The unit's slice count, the least a span needs.
fn slices_of(h: &Home) -> i64 {
    let n = h.num_slices(UNIT);
    assert!(n >= 6, "want a many-slice unit: {n}");
    n
}

/// One staging of the tree held across `labels` as ranges by the real write:
/// each tape but the last ends at its early warning after the slice `cuts`
/// names, and the next tape holds the slices owed. Returns the home and the
/// slice count.
fn spanned(labels: &[&str], cuts: &[i64]) -> (Home, i64) {
    let mut h = staged(labels, &tree(), 1);
    let n = slices_of(&h);
    write_ranges(&mut h, labels, cuts);
    assert_eq!(
        range_cuts(&h, labels),
        cuts_with_end(cuts, n),
        "the real write held the unit as the ranges the test wants"
    );
    (h, n)
}

fn write_ranges(h: &mut Home, labels: &[&str], cuts: &[i64]) {
    for (i, label) in labels.iter().enumerate() {
        let warn = cuts.get(i).map_or(Warn::Never, |&k| Warn::InSlice(k));
        write_warned(h, label, warn);
    }
}

fn cuts_with_end(cuts: &[i64], n: i64) -> Vec<(i64, i64)> {
    let mut start = 1;
    let mut out = Vec::new();
    for &end in cuts.iter().chain(std::iter::once(&n)) {
        out.push((start, end));
        start = end + 1;
    }
    out
}

/// The first and last slice of the unit each of `labels` holds as a written
/// range, in the order given.
fn range_cuts(h: &Home, labels: &[&str]) -> Vec<(i64, i64)> {
    labels
        .iter()
        .map(|label| {
            h.conn
                .query_row(
                    "SELECT MIN(sl.slice_number), MAX(sl.slice_number)
                       FROM write_positions wp
                       JOIN writes w ON w.id = wp.write_id
                       JOIN volumes v ON v.id = w.volume_id
                       JOIN stage_slices sl ON sl.id = wp.stage_slice_id
                       JOIN snapshots s ON s.id = w.snapshot_id
                       JOIN units u ON u.id = s.unit_id
                      WHERE v.label = ?1 AND u.name = ?2 AND wp.status = 'written'",
                    params![label, UNIT],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                )
                .unwrap_or_else(|e| panic!("{label} holds no slice of the unit: {e}"))
        })
        .collect()
}

/// The status of the unit's write on each volume: `(label, status)`.
fn write_statuses(h: &Home) -> Vec<(String, String)> {
    let mut stmt = h
        .conn
        .prepare(
            "SELECT v.label, w.status FROM writes w
               JOIN volumes v ON v.id = w.volume_id
               JOIN snapshots s ON s.id = w.snapshot_id
               JOIN units u ON u.id = s.unit_id
              WHERE u.name = ?1 ORDER BY w.id",
        )
        .unwrap();
    stmt.query_map(params![UNIT], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap()
}

fn volume_id(h: &Home, label: &str) -> i64 {
    h.conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            params![label],
            |r| r.get(0),
        )
        .unwrap()
}

/// `(label, outcome, detail)` of every contact, in order.
fn contacts(h: &Home) -> Vec<(String, String, Option<String>)> {
    let mut stmt = h
        .conn
        .prepare(
            "SELECT COALESCE(v.label, '-'), COALESCE(c.outcome, '-'), c.detail
               FROM cartridge_contacts c LEFT JOIN volumes v ON v.id = c.volume_id
              ORDER BY c.id",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap()
}

/// The paths an INCOMPLETE restore lists as damaged or missing.
fn lost_paths(err: &str) -> BTreeSet<String> {
    err.lines()
        .skip_while(|l| !l.contains("file(s) damaged or missing:"))
        .skip(1)
        .take_while(|l| l.starts_with("    ") && !l.starts_with("     "))
        .filter_map(|l| l.trim().split_once(" (").map(|(p, _)| p.to_string()))
        .collect()
}

/// The slices a restore of the whole unit reads when started on `from`: the
/// ones that hold a member (a slice holding only the stream's tail is not
/// read).
fn read_slices(h: &Home, from: &str) -> Vec<i64> {
    let selection = select_write_positions(&h.conn, UNIT, from, None).unwrap();
    plan_restore(&h.conn, &selection, RestoreTarget::Unit { dest_dir: "/x" })
        .unwrap()
        .positions
        .iter()
        .map(|wp| wp.slice_number)
        .collect()
}

/// The regular files of `unit`'s file table whose every slice is among
/// `slices`: what a cartridge holding those slices holds whole.
fn whole_files(
    rows: &[Row],
    slice_size: u64,
    slices: std::ops::RangeInclusive<i64>,
) -> BTreeSet<String> {
    let present: BTreeSet<u64> = slices.map(|n| n as u64).collect();
    filetable::held_whole(rows, slice_size, &present)
        .into_iter()
        .filter(|r| r.kind == Kind::File)
        .map(|r| r.path.clone())
        .collect()
}

/// Every regular file of the unit's source, as `(path, bytes)`.
fn source_files(h: &Home) -> Vec<(String, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            let meta = fs::symlink_metadata(&p).unwrap();
            if meta.is_dir() {
                walk(root, &p, out);
            } else if meta.is_file() {
                out.push((
                    p.strip_prefix(root).unwrap().to_string_lossy().into_owned(),
                    fs::read(&p).unwrap(),
                ));
            }
        }
    }
    let root = h.source(UNIT);
    let mut out = Vec::new();
    walk(&root, &root, &mut out);
    out
}

/// Every entry directly under `dir`: empty for a restore that wrote nothing.
fn entries_in(dir: &Path) -> Vec<std::path::PathBuf> {
    fs::read_dir(dir)
        .map(|it| it.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// `restore unit` of the unit, version 1, from the cartridge in the drive,
/// named by no `--from`: File 0 says which volume it is.
fn restore_loaded(
    h: &Home,
    unit: &str,
    what: What<'_>,
    options: &RestoreOptions,
    bay: &mut TestBay,
) -> (Result<super::RestoreReport>, TempDir) {
    let dest = TempDir::new().unwrap();
    let dest_dir = dest.path().to_string_lossy().into_owned();
    let target = match what {
        What::Unit => RestoreTarget::Unit {
            dest_dir: &dest_dir,
        },
        What::File(file_path) => RestoreTarget::File {
            file_path,
            dest_dir: &dest_dir,
        },
    };
    let r = super::restore_unit_from_loaded_bay(
        &h.conn,
        &h.paths,
        &h.config,
        unit,
        target,
        options,
        None,
        bay,
        h.site(),
    );
    (r, dest)
}

// ── the proofs ──

/// THE acceptance test of item 131 on the tar reader: a version held as two
/// ranges restores byte for byte from the cartridge holding slice 1, with the
/// second asked for once, by label and sticker, when its first slice is next.
/// It is read in one tar run across the swap; no slice is a substitution, and
/// each cartridge is a contact of its own.
#[test]
fn a_two_volume_span_restores_byte_identical() {
    let (h, n) = spanned(&[A, B], &[3]);
    let k = 3;
    let last = *read_slices(&h, A).last().unwrap();
    assert!(last > k, "the second cartridge is read");

    // The plan: every slice, in slice order, from the cartridge that holds it.
    let selection = select_write_positions(&h.conn, UNIT, A, None).unwrap();
    assert_eq!(
        selection
            .positions
            .iter()
            .map(|wp| (wp.slice_number, wp.volume_id))
            .collect::<Vec<_>>(),
        (1..=n)
            .map(|s| (s, volume_id(&h, if s <= k { A } else { B })))
            .collect::<Vec<_>>(),
        "every slice, in slice order, from the cartridge that holds it"
    );
    let plan = plan_restore(&h.conn, &selection, RestoreTarget::Unit { dest_dir: "/x" }).unwrap();
    assert_eq!(
        span::runs(&plan.positions)
            .iter()
            .map(|r| r.volume_id)
            .collect::<Vec<_>>(),
        [volume_id(&h, A), volume_id(&h, B)],
        "the restore reads two cartridges"
    );

    let before = contacts(&h).len();
    let mut bay = h.bay(A, &[Reply::Load(B)]);
    let (r, dest) = h.restore(UNIT, What::Unit, A, &RestoreOptions::default(), &mut bay);
    let report = r.unwrap_or_else(|e| panic!("{e}"));
    assert_restored(&h, UNIT, dest.path(), &[]);
    assert_eq!(report.slices as i64, plan.positions.len() as i64);
    assert!(report.substitutions.is_empty(), "{report:?}");
    assert_eq!(bay.asked_labels(), [B]);
    let prompt = &bay.asked[0].1;
    assert!(
        prompt.contains("held across cartridges")
            && prompt.contains(&format!("slice {} is next", k + 1))
            && prompt.contains(&format!("{B} holds slices {} to {last} of unit", k + 1))
            && prompt.contains(&format!("cartridge {B}-STK")),
        "{prompt}"
    );
    let contacts = contacts(&h).split_off(before);
    assert_eq!(contacts.len(), 2, "{contacts:?}");
    assert_eq!((contacts[0].0.as_str(), contacts[0].1.as_str()), (A, "ok"));
    assert_eq!((contacts[1].0.as_str(), contacts[1].1.as_str()), (B, "ok"));
    assert!(
        contacts[0]
            .2
            .as_deref()
            .unwrap_or("")
            .contains(&format!("continues on {B} at slice {}", k + 1)),
        "{contacts:?}"
    );
    assert!(!dest.path().join(SCRATCH_NAME).exists());
    let rows = crate::volume::restore_record::rows(&h.conn).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, "ok");
}

/// Three cartridges: asked for in slice order, each by its own label,
/// sticker and the slices it holds, and the version comes back byte for byte.
#[test]
fn a_three_volume_span_asks_in_slice_order_naming_each_cartridge() {
    let (h, _n) = spanned(&[A, B, C], &[1, 3]);
    let last = *read_slices(&h, A).last().unwrap();
    let before = contacts(&h).len();
    let mut bay = h.bay(A, &[Reply::Load(B), Reply::Load(C)]);
    let (r, dest) = h.restore(UNIT, What::Unit, A, &RestoreOptions::default(), &mut bay);
    r.unwrap();
    assert_restored(&h, UNIT, dest.path(), &[]);
    assert_eq!(bay.asked_labels(), [B, C]);
    let (b, c) = (&bay.asked[0].1, &bay.asked[1].1);
    assert!(
        b.contains("slice 2 is next")
            && b.contains(&format!("{B} holds slices 2 to 3"))
            && b.contains(&format!("cartridge {B}-STK")),
        "{b}"
    );
    assert!(
        c.contains("slice 4 is next")
            && c.contains(&format!("{C} holds {}", span::slices_phrase(4, last)))
            && c.contains(&format!("cartridge {C}-STK"))
            && c.contains(&format!("it is not on {B}")),
        "{c}"
    );
    assert_eq!(contacts(&h).len() - before, 3);
}

/// Item 131's "a live whole copy first": tape A holds the start of the
/// version, and both a range of the rest (on the volume with the LOWER id)
/// and a whole copy hold the slices after it. The whole copy is asked for.
/// Started on the whole copy, the version restores from it alone.
#[test]
fn a_live_whole_copy_is_preferred_over_a_span() {
    // Ids: A 1, C 2, B 3; written A (a range), B (the whole set, as the
    // second tape of item 93 holds it), C (the owed rest, a range).
    let mut h = staged(&[A, C, B], &tree(), 2);
    let n = slices_of(&h);
    write_warned(&mut h, A, Warn::InSlice(2));
    write_warned(&mut h, B, Warn::Never);
    write_warned(&mut h, C, Warn::Never);
    assert_eq!(
        write_statuses(&h),
        [
            (A.to_string(), "range".to_string()),
            (B.to_string(), "completed".to_string()),
            (C.to_string(), "range".to_string()),
        ]
    );
    assert_eq!(range_cuts(&h, &[A, B, C]), [(1, 2), (1, n), (3, n)]);
    assert!(
        volume_id(&h, C) < volume_id(&h, B),
        "the range would win on id"
    );

    let selection = select_write_positions(&h.conn, UNIT, A, None).unwrap();
    let runs: Vec<i64> = span::runs(&selection.positions)
        .iter()
        .map(|r| r.volume_id)
        .collect();
    assert_eq!(runs, [volume_id(&h, A), volume_id(&h, B)]);
    let mut bay = h.bay(A, &[Reply::Load(B)]);
    let (r, dest) = h.restore(UNIT, What::Unit, A, &RestoreOptions::default(), &mut bay);
    r.unwrap();
    assert_restored(&h, UNIT, dest.path(), &[]);
    assert_eq!(bay.asked_labels(), [B]);

    // From the whole copy: one cartridge, no prompt.
    let mut bay = h.bay(B, &[]);
    let (r, dest) = h.restore(UNIT, What::Unit, B, &RestoreOptions::default(), &mut bay);
    r.unwrap();
    assert_restored(&h, UNIT, dest.path(), &[]);
    assert!(bay.asked.is_empty());
}

/// Item 266: a restore may start on any tape of a span. Started on the
/// cartridge that holds only the later slices (named by `--from`, or found
/// loaded), it asks for the holder of slice 1 as the next cartridge, then for
/// this one again when its slices come, and restores the version whole. (The
/// refusal of 1.7.0, naming the holders of slice 1, is gone.)
#[test]
fn a_restore_may_start_on_a_tape_without_slice_1_and_asks_for_the_holder_of_slice_1() {
    let (h, _n) = spanned(&[A, B], &[3]);
    let last = *read_slices(&h, B).last().unwrap();

    let mut bay = h.bay(B, &[Reply::Load(A), Reply::Load(B)]);
    let (r, dest) = h.restore(UNIT, What::Unit, B, &RestoreOptions::default(), &mut bay);
    r.unwrap_or_else(|e| panic!("--from {B}: {e}"));
    assert_restored(&h, UNIT, dest.path(), &[]);
    assert_eq!(bay.asked_labels(), [A, B], "slice order");
    let first = &bay.asked[0].1;
    assert!(
        first.contains("slice 1 is next")
            && first.contains(&format!("it is not on {B}"))
            && first.contains(&format!("{A} holds slices 1 to 3 of unit"))
            && first.contains(&format!("cartridge {A}-STK")),
        "{first}"
    );
    assert!(
        bay.asked[1].1.contains("slice 4 is next")
            && bay.asked[1]
                .1
                .contains(&format!("{B} holds slices 4 to {last}")),
        "{}",
        bay.asked[1].1
    );

    // The loaded tape, named by no --from, by the same rule.
    let mut bay = h.bay(B, &[Reply::Load(A), Reply::Load(B)]);
    let (r, dest) = restore_loaded(&h, UNIT, What::Unit, &RestoreOptions::default(), &mut bay);
    r.unwrap_or_else(|e| panic!("loaded {B}: {e}"));
    assert_restored(&h, UNIT, dest.path(), &[]);
    assert_eq!(bay.asked_labels(), [A, B]);
}

/// A version with a slice on no live volume is refused before any prompt,
/// naming the slices and where the catalog saw them; the dry run says the
/// same. (The refusal that still holds in the new design: every slice must
/// be somewhere before a cartridge is asked for.)
#[test]
fn a_slice_on_no_live_volume_is_refused_before_any_prompt() {
    let (h, n) = spanned(&[A, B], &[3]);
    // Positive control: with B in service the same restore goes through.
    let mut bay = h.bay(A, &[Reply::Load(B)]);
    let (r, _dest) = h.restore(UNIT, What::Unit, A, &RestoreOptions::default(), &mut bay);
    r.expect("the span restores while B is in service");

    h.conn
        .execute(
            "UPDATE volumes SET status = 'retired' WHERE label = ?1",
            params![B],
        )
        .unwrap();
    let mut bay = h.bay(A, &[Reply::Load(B)]);
    let (r, dest) = h.restore(UNIT, What::Unit, A, &RestoreOptions::default(), &mut bay);
    let err = r.unwrap_err().to_string();
    let slices = format!("slices 4 to {n} of it are on no live volume");
    for needle in [
        slices.as_str(),
        &format!("\"{A}\" planned it, but its tape ended before it"),
        &format!("\"{B}\" is given up (retired or erased)"),
        "Nothing was read from tape",
    ] {
        assert!(err.contains(needle), "{needle}: {err}");
    }
    assert!(
        err.contains("catalog rebuild --from-volume") || err.contains("still staged"),
        "what would bring them back is said: {err}"
    );
    assert!(bay.asked.is_empty(), "refused before any prompt");
    assert!(entries_in(dest.path()).is_empty());

    let dest_str = dest.path().to_string_lossy().into_owned();
    let dry = restore_unit(
        &h.conn,
        &h.paths,
        &h.config,
        UNIT,
        A,
        &dest_str,
        "/nonexistent/tapectl-478-nst",
        4096,
        None,
        true,
        &RestoreOptions::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(dry.contains(&slices), "{dry}");
}

/// What a restore of the span, started on A, leaves when B is not given: the
/// files A holds whole are there byte for byte, and every other file is
/// absent and named in the error. Returns the error text.
fn assert_what_tape_a_holds_whole(h: &Home, k: i64, dest: &Path, err: &str) {
    assert!(err.contains("INCOMPLETE"), "{err}");
    let rows = h.rows(UNIT);
    let on_a = whole_files(&rows, SLICE, 1..=k);
    let on_other: BTreeSet<String> = rows
        .iter()
        .filter(|r| r.kind == Kind::File && !on_a.contains(&r.path))
        .map(|r| r.path.clone())
        .collect();
    assert!(
        on_a.iter().any(|p| p != ".tapectl-unit.toml") && !on_other.is_empty(),
        "positive control: the tape holds some files whole and not all: {on_a:?} / {on_other:?}"
    );
    let lost = lost_paths(err);
    for (path, bytes) in source_files(h) {
        let there = fs::read(dest.join(&path)).ok();
        if on_a.contains(&path) {
            assert_eq!(there.as_ref(), Some(&bytes), "{path} is on tape A whole");
            assert!(!lost.contains(&path), "{path} is not lost: {err}");
        } else {
            assert!(there.is_none(), "{path} is not on tape A whole: {err}");
            assert!(lost.contains(&path), "{path} is named: {err}");
        }
    }
}

/// With nobody at the terminal a restore that needs a second cartridge does
/// not ask for it (tapectl swaps no cartridge without a person): it restores
/// the files its own tape holds whole and lists the rest, naming nothing
/// that is lost but what is not on the tape.
#[test]
fn with_nobody_at_the_terminal_the_restore_takes_what_its_tape_holds_whole_and_lists_the_rest() {
    let (h, _n) = spanned(&[A, B], &[3]);
    let mut bay = h.bay(A, &[Reply::Load(B)]);
    bay.attended = false;
    let (r, dest) = h.restore(UNIT, What::Unit, A, &RestoreOptions::default(), &mut bay);
    let err = r
        .expect_err("the files on the other cartridge are listed")
        .to_string();
    assert_what_tape_a_holds_whole(&h, 3, dest.path(), &err);
    assert!(bay.asked.is_empty(), "nobody is asked");
    assert!(!dest.path().join(SCRATCH_NAME).exists());
}

/// "I do not have it" at the prompt for the next cartridge (item 266, R7)
/// is allowed: the restore goes on with the files its tape holds whole and
/// lists the rest, and does not ask for the cartridge again.
#[test]
fn skip_at_the_prompt_for_the_next_cartridge_restores_what_is_held_whole_and_lists_the_rest() {
    let (h, _n) = spanned(&[A, B], &[3]);
    let mut bay = h.bay(A, &[Reply::Skip]);
    let (r, dest) = h.restore(UNIT, What::Unit, A, &RestoreOptions::default(), &mut bay);
    let err = r
        .expect_err("the files on the other cartridge are listed")
        .to_string();
    assert_what_tape_a_holds_whole(&h, 3, dest.path(), &err);
    assert_eq!(bay.asked_labels(), [B], "asked once, not again");
    assert!(!dest.path().join(SCRATCH_NAME).exists());

    // "stop" is still a stop: it ends the restore by name.
    let mut bay = h.bay(A, &[Reply::Stop]);
    let (r, _dest) = h.restore(UNIT, What::Unit, A, &RestoreOptions::default(), &mut bay);
    let err = r.unwrap_err().to_string();
    assert!(err.contains("stopped at the operator's request"), "{err}");
}

/// `restore file` across a swap reads only the slices that hold the file: a
/// file whose slices are all on the first cartridge asks for nobody, a file
/// on the second asks for it once and is placed whole, and a long file whose
/// run crosses the end of the first tape is read across the swap.
#[test]
fn restore_file_reads_across_a_swap() {
    let (h, n) = spanned(&[A, B], &[3]);
    let k = 3;
    let rows = h.rows(UNIT);
    let on_a = whole_files(&rows, SLICE, 1..=k);
    let on_b = whole_files(&rows, SLICE, k + 1..=n);
    let early = on_a
        .iter()
        .find(|p| p.starts_with("part-"))
        .expect("a file on A");
    let late = on_b
        .iter()
        .find(|p| p.starts_with("part-"))
        .expect("a file on B");
    let want = |h: &Home, path: &str| {
        source_files(h)
            .into_iter()
            .find(|(p, _)| p == path)
            .unwrap()
            .1
    };
    let placed =
        |dest: &Path, path: &str| fs::read(dest.join(Path::new(path).file_name().unwrap()));

    // On the first cartridge only: nobody is asked.
    let mut bay = h.bay(A, &[Reply::Load(B)]);
    let (r, dest) = h.restore(
        UNIT,
        What::File(early),
        A,
        &RestoreOptions::default(),
        &mut bay,
    );
    let report = r.unwrap_or_else(|e| panic!("{early}: {e}"));
    assert_eq!(placed(dest.path(), early).unwrap(), want(&h, early));
    assert!(bay.asked.is_empty(), "{:?}", bay.asked);
    let slices_of_early = rows
        .iter()
        .find(|r| &r.path == early)
        .unwrap()
        .slices(SLICE)
        .count();
    assert_eq!(
        report.slices, slices_of_early,
        "only the file's slices are read"
    );

    // On the second only: it is asked for, by label, once.
    let mut bay = h.bay(A, &[Reply::Load(B)]);
    let (r, dest) = h.restore(
        UNIT,
        What::File(late),
        A,
        &RestoreOptions::default(),
        &mut bay,
    );
    r.unwrap_or_else(|e| panic!("{late}: {e}"));
    assert_eq!(placed(dest.path(), late).unwrap(), want(&h, late));
    assert_eq!(bay.asked_labels(), [B]);

    // A long file whose run crosses the end of the first tape: the second is
    // asked for when the run's next slice is.
    let mut h = staged(&[A, B], &tree(), 1);
    let long = h
        .rows(UNIT)
        .into_iter()
        .find(|r| r.path == "zz-long.bin")
        .unwrap();
    assert!(long.is_run(SLICE), "the long file is a run");
    let k = long.slice as i64;
    write_ranges(&mut h, &[A, B], &[k]);
    assert!(
        long.slices(SLICE).last().unwrap() as i64 > k,
        "the run crosses the tape's end"
    );
    let mut bay = h.bay(A, &[Reply::Load(B)]);
    let (r, dest) = h.restore(
        UNIT,
        What::File("zz-long.bin"),
        A,
        &RestoreOptions::default(),
        &mut bay,
    );
    r.unwrap_or_else(|e| panic!("zz-long.bin: {e}"));
    assert_eq!(
        placed(dest.path(), "zz-long.bin").unwrap(),
        want(&h, "zz-long.bin")
    );
    assert_eq!(bay.asked_labels(), [B]);
    assert!(
        bay.asked[0].1.contains(&format!("slice {} is next", k + 1)),
        "{}",
        bay.asked[0].1
    );
}

/// Item 101 sees ranges: a slice damaged on the whole copy is read from a
/// range that holds it, that cartridge serves the next slice it holds too,
/// and the restore goes back to the whole copy for the rest.
#[test]
fn a_damaged_slice_is_replaced_from_a_range_holder() {
    let mut h = staged(&[A, B], &tree(), 2);
    let n = slices_of(&h);
    write_warned(&mut h, A, Warn::Never);
    write_warned(&mut h, B, Warn::InSlice(3));
    assert_eq!(
        write_statuses(&h),
        [
            (A.to_string(), "completed".to_string()),
            (B.to_string(), "range".to_string())
        ]
    );
    assert_eq!(range_cuts(&h, &[A, B]), [(1, n), (1, 3)]);
    let pos = h.position(A, UNIT, 2);
    h.flip(A, pos, 2_000);
    let mut bay = h.bay(A, &[Reply::Load(B), Reply::Load(A)]);
    let (r, dest) = h.restore(UNIT, What::Unit, A, &RestoreOptions::default(), &mut bay);
    let report = r.unwrap_or_else(|e| panic!("{e}"));
    assert_restored(&h, UNIT, dest.path(), &[]);
    assert_eq!(bay.asked_labels(), [B, A]);
    assert!(
        bay.asked[0].1.contains("Slice 2") && bay.asked[0].1.contains("checksum mismatch"),
        "{:?}",
        bay.asked
    );
    assert!(
        bay.asked[1].1.contains("Slice 4 of unit") && bay.asked[1].1.contains("is next"),
        "{:?}",
        bay.asked
    );
    assert_eq!(
        report
            .substitutions
            .iter()
            .map(|s| (s.slice_number, s.supplied_by.as_str()))
            .collect::<Vec<_>>(),
        vec![(2, B)]
    );
}

/// Item 260 (which replaces item 143): a range IS a source for the files
/// whose slices it holds whole, and for no other. The version's other
/// staging (cut at another size) is held only as a range, on B, which ends
/// inside the long file's run; the whole copy on A has two damaged slices. The
/// lost files that lie whole in the range are filled from it, asking for it by
/// label; the long file, whose run goes on past the range's end, lies whole in
/// no range and is listed.
#[test]
fn a_range_is_a_source_for_the_files_it_holds_whole_and_for_no_other() {
    const TWO: u64 = 40 * 1024;
    let mut h = staged(&[A, B], &tree(), 2);
    let one = h.stage_set(UNIT, 1);
    write_warned(&mut h, A, Warn::Never);
    // The first staging is released (its copy floor, for the moment, is 1),
    // and the version is staged again, cut at another size.
    h.config.defaults.min_copies = 1;
    crate::cli::volume::release_staging_after_write(&h.conn, &h.config, A, false);
    h.config.defaults.min_copies = 2;
    h.config.defaults.slice_size = "40K".into();
    let snapshot: i64 = h
        .conn
        .query_row(
            "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
            params![one],
            |r| r.get(0),
        )
        .unwrap();
    let two = crate::staging::stage_create_admitting(
        &h.conn,
        &h.paths,
        &h.config,
        snapshot,
        true,
        crate::staging::Admission::Restage,
        None,
        &mut std::io::sink(),
    )
    .unwrap_or_else(|e| panic!("the second staging: {e}"));
    assert_ne!(one, two);
    let rows1 = crate::db::stage_files::load(&h.conn, one).unwrap();
    let rows2 = crate::db::stage_files::load(&h.conn, two).unwrap();
    let n2: i64 = h
        .conn
        .query_row(
            "SELECT COUNT(*) FROM stage_slices WHERE stage_set_id = ?1",
            params![two],
            |r| r.get(0),
        )
        .unwrap();
    let n1: i64 = h
        .conn
        .query_row(
            "SELECT COUNT(*) FROM stage_slices WHERE stage_set_id = ?1",
            params![one],
            |r| r.get(0),
        )
        .unwrap();
    let long2 = rows2.iter().find(|r| r.path == "zz-long.bin").unwrap();
    let k = long2.slice as i64;
    assert!(
        long2.slices(TWO).last().unwrap() as i64 > k && k < n2,
        "the long file's run goes on past the range's end"
    );
    write_warned(&mut h, B, Warn::InSlice(k));
    assert_eq!(range_cuts(&h, &[A, B]), [(1, n1), (1, k)]);

    // Damage the whole copy: the slice holding the first part file and the
    // first slice of the long file's run.
    let first_part = rows1.iter().find(|r| r.path.starts_with("part-")).unwrap();
    let hit = [
        first_part.slice as i64,
        rows1
            .iter()
            .find(|r| r.path == "zz-long.bin")
            .unwrap()
            .slice as i64,
    ];
    let mut lost: BTreeSet<String> = BTreeSet::new();
    for s in hit {
        let pos = h.position_in(A, one, s);
        h.flip(A, pos, 2_000);
        lost.extend(
            filetable::members_of(&rows1, SLICE, s as u64)
                .into_iter()
                .filter(|r| r.kind == Kind::File)
                .map(|r| r.path.clone()),
        );
    }
    let in_b = whole_files(&rows2, TWO, 1..=k);
    let from_b: BTreeSet<String> = lost.intersection(&in_b).cloned().collect();
    let stuck: BTreeSet<String> = lost.difference(&in_b).cloned().collect();
    assert!(
        !from_b.is_empty(),
        "positive control: B holds a lost file whole: {lost:?}"
    );
    assert!(stuck.contains("zz-long.bin"), "{stuck:?}");

    let mut bay = h.bay(A, &[Reply::Load(B), Reply::Load(A)]);
    let (r, dest) = h.restore(UNIT, What::Unit, A, &RestoreOptions::default(), &mut bay);
    let err = r
        .expect_err("the long file is lost to every copy")
        .to_string();
    assert!(err.contains("INCOMPLETE"), "{err}");
    let listed: BTreeSet<String> = lost_paths(&err)
        .into_iter()
        .filter(|p| source_files(&h).iter().any(|(f, _)| f == p))
        .collect();
    assert_eq!(listed, stuck, "{err}");
    for (path, bytes) in source_files(&h) {
        let there = fs::read(dest.path().join(&path)).ok();
        if stuck.contains(&path) {
            assert!(there.is_none(), "{path} is lost to every copy");
        } else if from_b.contains(&path) {
            assert_eq!(
                there.as_ref(),
                Some(&bytes),
                "{path} is filled from a range"
            );
        }
    }
    assert_eq!(bay.asked_labels(), [B], "only the range is asked for");
}

/// A version held whole restores as in 1.6.0 though ranges of it exist
/// elsewhere: no prompt, and the tape moves as a clean restore's does (one
/// rewind, File 0, a space to the first slice, each slice read once, in
/// order); the cartridges that hold ranges are not touched.
#[test]
fn a_version_held_whole_restores_as_in_1_6_0_beside_its_ranges() {
    let mut h = staged(&[A, B, C], &tree(), 2);
    let n = slices_of(&h);
    write_warned(&mut h, A, Warn::Never);
    write_warned(&mut h, B, Warn::InSlice(2));
    write_warned(&mut h, C, Warn::Never);
    assert_eq!(range_cuts(&h, &[A, B, C]), [(1, n), (1, 2), (3, n)]);
    let selection = select_write_positions(&h.conn, UNIT, A, None).unwrap();
    assert!(selection
        .positions
        .iter()
        .all(|wp| wp.volume_id == volume_id(&h, A)));

    let mut fakes = BTreeMap::new();
    let carts = h
        .tapes
        .iter()
        .map(|(label, mem)| {
            let fake = FakeTape::with_files(mem.files.clone(), BS);
            let store = TapeStore::from_ops(fake.boxed(), 0).unwrap();
            fakes.insert(label.clone(), fake);
            (label.clone(), Box::new(store) as Box<dyn Store>)
        })
        .collect();
    let mut bay = TestBay {
        carts,
        open: Some(A.to_string()),
        physical: Some(A.to_string()),
        attended: true,
        script: VecDeque::new(),
        asked: Vec::new(),
        releases: 0,
    };
    let (r, dest) = h.restore(UNIT, What::Unit, A, &RestoreOptions::default(), &mut bay);
    r.unwrap_or_else(|e| panic!("{e}"));
    assert_restored(&h, UNIT, dest.path(), &[]);
    assert!(bay.asked.is_empty());
    let first = h.position(A, UNIT, 1) as u32;
    let mut want = vec![Op::Rewind, Op::ReadHead(0), Op::Space(first - 1)];
    want.extend(
        read_slices(&h, A)
            .into_iter()
            .map(|s| Op::Read(h.position(A, UNIT, s) as u32)),
    );
    assert_eq!(fakes[A].ops(), want);
    for label in [B, C] {
        assert!(
            !fakes[label]
                .ops()
                .iter()
                .any(|op| matches!(op, Op::Read(_))),
            "{label} is never read: {:?}",
            fakes[label].ops()
        );
    }
}

/// `restore volume` (item 89, D17): the whole units on the tape are restored
/// and so is the unit of which it holds only a range, across cartridges as
/// `restore unit` restores it. With the next cartridge not given, the whole
/// unit is whole and the unit held across cartridges is listed as incomplete
/// by name; naming it alone is no longer refused.
#[test]
fn restore_volume_restores_the_whole_units_and_the_unit_held_across_cartridges() {
    const OTHER: &str = "other-478";
    let mut h = Home::new("64K", None, 1);
    h.unit(TENANT, OTHER, &files("o", 2, 30_000, 5));
    h.unit(TENANT, UNIT, &tree());
    h.stage(OTHER);
    h.stage(UNIT);
    h.volume(A);
    h.volume(B);
    let n = slices_of(&h);
    write_warned(&mut h, A, Warn::InSlice(3));
    write_warned(&mut h, B, Warn::Never);
    assert_eq!(range_cuts(&h, &[A, B]), [(1, 3), (4, n)]);

    // The plan order of a tape: the whole unit first, the set held across
    // cartridges last, where the tape ended. Both are on the volume.
    assert_eq!(units_on_volume(&h.conn, A).unwrap(), [OTHER, UNIT]);
    assert_eq!(units_on_volume(&h.conn, B).unwrap(), [UNIT]);

    let options = RestoreOptions::default();
    let names = vec![OTHER.to_string(), UNIT.to_string()];
    let alone = TempDir::new().unwrap();
    let named = unit_requests(&[UNIT.to_string()], &alone.path().to_string_lossy()).unwrap();
    plan_units(&h.conn, &h.config, A, &named, &options)
        .unwrap_or_else(|e| panic!("a range unit named alone is planned: {e}"));

    // The next cartridge given.
    let root = TempDir::new().unwrap();
    let requests = unit_requests(&names, &root.path().to_string_lossy()).unwrap();
    let planned = plan_units(&h.conn, &h.config, A, &requests, &options).unwrap();
    let mut bay = h.bay(A, &[Reply::Load(B)]);
    let report = restore_units_from_bay(
        &h.conn,
        &h.paths,
        &h.config,
        A,
        &planned,
        &options,
        false,
        &mut bay,
        h.site(),
    )
    .unwrap();
    assert_eq!(report.failed(), 0, "{:?}", report.units);
    assert_restored(&h, OTHER, &root.path().join(OTHER), &[]);
    assert_restored(&h, UNIT, &root.path().join(UNIT), &[]);
    assert_eq!(bay.asked_labels(), [B]);

    // "I do not have it": the whole unit is whole, the other lists the rest.
    let root = TempDir::new().unwrap();
    let requests = unit_requests(&names, &root.path().to_string_lossy()).unwrap();
    let planned = plan_units(&h.conn, &h.config, A, &requests, &options).unwrap();
    let mut bay = h.bay(A, &[Reply::Skip]);
    let report = restore_units_from_bay(
        &h.conn,
        &h.paths,
        &h.config,
        A,
        &planned,
        &options,
        false,
        &mut bay,
        h.site(),
    )
    .unwrap();
    assert_eq!(report.failed(), 1, "{:?}", report.units);
    assert_restored(&h, OTHER, &root.path().join(OTHER), &[]);
    let span_unit = report.units.iter().find(|u| u.unit_name == UNIT).unwrap();
    let err = span_unit.error.clone().expect("the unit is incomplete");
    assert_what_tape_a_holds_whole(&h, 3, &root.path().join(UNIT), &err);
    assert_eq!(bay.asked_labels(), [B]);
    let _ = listing;
}
