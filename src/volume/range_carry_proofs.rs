//! A range carried (T5.3; ADR-0012 items 106, 122, 125, 127, 133 as item 288
//! supersedes it, and 142's second half): `volume rewrite` and compaction
//! carry a volume that holds a range, by ciphertext, in slice order, and give
//! up nothing the volume alone holds without the typed consent that names it.
//!
//! A child of [`super`] (`span_proofs`) so that it builds on the same real
//! pieces: each tape is written by the real `volume write` over a
//! [`MemStore`], each stage set by the real `stage create`. The rewrite runs
//! the real [`rewrite::rewrite`] over [`MemSteps`], whose read, init and write
//! are the real `compact_read`, `volume_init_contacted` and
//! `volume_write_contacted` on a medium in memory, and whose erase leaves in
//! the catalog what `cartridge erase` leaves (the real erase needs a drive).
//! No catalog row is made by hand.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use rusqlite::{params, Connection};

use super::*;
use crate::volume::erase::{EraseOutcome, EraseReport, MediumKind, Recorded};
use crate::volume::rewrite::{self, ExpectedVolume, RewriteOptions, RewriteReport, RewriteSteps};
use crate::volume::write::{CapacityNumbers, CompactReadReport, Initialized};

const CHIP: &str = "CHIP-RANGE-PROOFS";

/// The real steps of a rewrite on tapes in memory. `tapes` is [`Home::tapes`]
/// for the length of the command.
struct MemSteps<'a> {
    h: &'a Home,
    tapes: RefCell<BTreeMap<String, MemStore>>,
    /// The volume being rewritten, and so the cartridge whose medium the
    /// erase blanks and the init writes again.
    source: String,
    /// The steps taken, in order.
    log: RefCell<Vec<String>>,
    /// The write step fails once, touching nothing (a window left open).
    fail_write: Cell<bool>,
    /// `(step, slices planned)` at the rehearsal and at the write: what
    /// `volume write` would plan for every staged set, `(unit, slice)`.
    planned: RefCell<Vec<(String, Vec<(String, i64)>)>>,
}

/// What a write would plan now, `(unit, slice number)` in plan order.
fn planned_now(h: &Home) -> Vec<(String, i64)> {
    write::staged_build_units_spanning(&h.conn, &h.config)
        .map(|units| {
            units
                .iter()
                .flat_map(|u| {
                    u.slices
                        .iter()
                        .map(|s| (u.unit_name.clone(), s.slice_number))
                        .collect::<Vec<_>>()
                })
                .collect()
        })
        .unwrap_or_default()
}

impl<'a> MemSteps<'a> {
    fn new(h: &'a mut Home, source: &str) -> Self {
        MemSteps {
            tapes: RefCell::new(std::mem::take(&mut h.tapes)),
            h,
            source: source.to_string(),
            log: RefCell::new(Vec::new()),
            fail_write: Cell::new(false),
            planned: RefCell::new(Vec::new()),
        }
    }

    fn barcode(&self) -> String {
        format!("{}-STK", self.source)
    }
}

impl RewriteSteps for MemSteps<'_> {
    fn host_preflight(&self, _what: &str) -> Result<()> {
        Ok(())
    }

    fn write_preflight(
        &self,
        expected: &ExpectedVolume,
        only_stage_sets: Option<&[i64]>,
    ) -> Result<Option<CapacityNumbers>> {
        self.log.borrow_mut().push("write_preflight".into());
        self.planned
            .borrow_mut()
            .push(("rehearsal".into(), planned_now(self.h)));
        write::rehearse_write_capacity(
            &self.h.conn,
            &self.h.paths,
            &self.h.config,
            DEVICE,
            BS,
            &write::RehearsedVolume {
                label: &expected.label,
                media_type: &expected.media_type,
                capacity_bytes: expected.capacity_bytes,
                cartridge_serial: &expected.cartridge_serial,
                manufacturer: &expected.manufacturer,
            },
            only_stage_sets,
            false,
        )
        .map(Some)
    }

    fn read(&self, source: &str) -> Result<CompactReadReport> {
        self.log.borrow_mut().push("read".into());
        let mut store = copy_of(&self.tapes.borrow()[source]);
        write::compact_read(
            &self.h.conn,
            &self.h.config,
            source,
            &mut store,
            ContactSite::new(
                &self.h.config,
                Operation::VolumeRewrite,
                DEVICE,
                Medium::NoBackend,
            ),
        )
    }

    fn erase(&self, _confirm_label: &str, _cartridge_serial: Option<&str>) -> Result<EraseReport> {
        self.log.borrow_mut().push("erase".into());
        let c = &self.h.conn;
        assert!(
            crate::policy::coverage::is_given_up(c, volume_id(c, &self.source))?,
            "the erase is of a volume the catalog has given up"
        );
        let cartridge: i64 = c.query_row(
            "SELECT id FROM cartridges WHERE barcode = ?1",
            params![self.barcode()],
            |r| r.get(0),
        )?;
        let tx = c.unchecked_transaction()?;
        crate::cli::operations::record_cartridge_erased(
            &tx,
            cartridge,
            &self.barcode(),
            "pending_erase",
            &[volume_id(c, &self.source)],
            "available",
            None,
        )?;
        tx.commit()?;
        // The cartridge is blank: the source's recorded files are gone.
        self.tapes
            .borrow_mut()
            .insert(self.source.clone(), MemStore::new(BS));
        Ok(EraseReport {
            device: DEVICE.into(),
            mode: "short",
            chip_serial: Some(CHIP.into()),
            medium: MediumKind::Tapectl,
            file0: None,
            sealed: None,
            detail: None,
            catalog_volumes: Vec::new(),
            catalog_cartridge: None,
            notes: Vec::new(),
            confirm_label: None,
            outcome: EraseOutcome::Erased,
            medium_after: Some(MediumKind::Empty),
            chip_serial_after: Some(CHIP.into()),
            recorded: Some(Recorded::default()),
            warnings: Vec::new(),
        })
    }

    fn init(&self, label: &str) -> Result<Initialized> {
        self.log.borrow_mut().push(format!("init {label}"));
        let mut tapes = self.tapes.borrow_mut();
        let mut medium = tapes.remove(&self.source).expect("the cartridge's medium");
        let mut slot = ContactSlot::empty();
        let r = write::volume_init_contacted(
            &self.h.conn,
            &self.h.config,
            label,
            DEVICE,
            BS,
            false,
            None,
            Some(&self.barcode()),
            &mut slot,
            ContactStore::Injected(&mut medium),
        );
        let r = slot.finish_result(r);
        tapes.insert(label.to_string(), medium);
        r
    }

    fn write(&self, label: &str, only_stage_sets: Option<&[i64]>) -> Result<()> {
        self.log.borrow_mut().push(format!("write {label}"));
        self.planned
            .borrow_mut()
            .push(("write".into(), planned_now(self.h)));
        if self.fail_write.replace(false) {
            return Err(crate::error::TapectlError::WriteInterrupted(
                "the write stopped".into(),
            ));
        }
        let mut tapes = self.tapes.borrow_mut();
        let medium = tapes.remove(label).expect("the destination was made");
        let mut drive = Drive::new(&self.h.conn, label, medium, Warn::Never);
        let mut slot = ContactSlot::empty();
        let r = write::volume_write_contacted(
            &self.h.conn,
            &self.h.paths,
            &self.h.config,
            label,
            DEVICE,
            BS,
            false,
            false,
            false,
            true,
            true,
            &mut slot,
            ContactStore::Injected(&mut drive),
            only_stage_sets,
        );
        let r = slot.finish_result(r);
        tapes.insert(label.to_string(), drive.medium);
        r
    }

    fn resume_write(&self, _label: &str) -> Result<()> {
        panic!("a proof of the carry resumes nothing");
    }

    fn chip_serial(&self) -> Option<String> {
        Some(CHIP.into())
    }
}

/// [`Home::write`] with the full readback (`volume write --full-confirm`),
/// the fact that makes a write a copy a rewrite may lean on
/// ([`crate::policy::coverage::fully_read_back_copy`]).
fn write_full(h: &mut Home, label: &str, warn: Warn) {
    let medium = h.tapes.remove(label).expect("the volume was made");
    let mut drive = Drive::new(&h.conn, label, medium, warn);
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
        true,
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

fn volume_id(conn: &Connection, label: &str) -> i64 {
    conn.query_row(
        "SELECT id FROM volumes WHERE label = ?1",
        params![label],
        |r| r.get(0),
    )
    .unwrap()
}

fn volume_status(conn: &Connection, label: &str) -> String {
    conn.query_row(
        "SELECT status FROM volumes WHERE label = ?1",
        params![label],
        |r| r.get(0),
    )
    .unwrap()
}

fn options<'a>(source: &'a str, destination: &'a str) -> RewriteOptions<'a> {
    RewriteOptions {
        source,
        destination,
        device: DEVICE,
        only_copy_on_staging: Some(source),
        assume_yes: true,
        with_staged: false,
        keep_staging: false,
    }
}

/// What a rewrite left: the command's result, the steps it took, and the
/// plans it saw at the rehearsal and at the write.
struct Ran {
    result: Result<RewriteReport>,
    log: Vec<String>,
    planned: Vec<(String, Vec<(String, i64)>)>,
}

/// `volume rewrite SOURCE --label DESTINATION` of `h`, the typed consent
/// given on the command line. The destination's medium is the source's
/// cartridge, written again. `fail_write`: the first write step fails.
fn rewrite_with(h: &mut Home, source: &str, destination: &str, fail_write: bool) -> Ran {
    let steps = MemSteps::new(h, source);
    steps.fail_write.set(fail_write);
    let result = rewrite::rewrite(
        &steps.h.conn,
        &steps.h.config,
        &steps,
        &options(source, destination),
    );
    let ran = Ran {
        result,
        log: steps.log.borrow().clone(),
        planned: steps.planned.borrow().clone(),
    };
    h.tapes = steps.tapes.into_inner();
    ran
}

fn rewrite_volume(
    h: &mut Home,
    source: &str,
    destination: &str,
) -> (Result<RewriteReport>, Vec<String>) {
    let ran = rewrite_with(h, source, destination, false);
    (ran.result, ran.log)
}

// ── a source that holds only a range ──

const B1: &str = "kumquat-zspan-bravo";
const R1: [&str; 2] = ["RNG-T1", "RNG-T2"];

/// One unit of three slices at a copy target of 1, over two cartridges: the
/// early warning one byte into slice 2 ends tape 1, a range of 1-2 and
/// nothing else; tape 2 takes the owed slice 3, a range of 3-3. Every slice
/// then has its one copy and the version is current; tape 1 alone holds
/// slices 1 and 2.
fn range_only_home() -> Home {
    let mut h = Home::new(None, 1, "64K");
    h.unit(
        TENANT_ONE,
        B1,
        files("bravo-reel", &[50_000, 50_000, 50_000], 2),
    );
    h.stage(B1);
    assert_eq!(num_slices(&h.conn, B1), 3, "the unit is three slices");
    for label in R1 {
        h.volume(label);
    }
    write_full(&mut h, R1[0], Warn::InSlice(B1, 2));
    write_full(&mut h, R1[1], Warn::Never);
    assert_eq!(
        write::recorded_ranges(&h.conn, R1[0]).unwrap(),
        [range(B1, 1, 2)],
        "tape 1 holds a range and nothing else"
    );
    assert_eq!(
        write::recorded_ranges(&h.conn, R1[1]).unwrap(),
        [range(B1, 3, 3)]
    );
    assert_eq!(snapshot_status(&h.conn, B1), "current");
    assert_eq!(slice_copies(&h.conn, B1), [1, 1, 1]);
    h
}

/// A source holding only a range is rewritten: the typed consent names the
/// version, the slices and the volume that holds the rest; the range is read
/// into staging, the cartridge is erased and written again, and the new
/// volume holds the range as it was, in slice order, with every slice of the
/// staging in its block. The unit then restores whole from the rewritten
/// cartridge and the one that holds the rest.
#[test]
fn a_source_holding_only_a_range_is_rewritten_and_the_unit_restores_whole() {
    let mut h = range_only_home();
    let (r, log) = rewrite_volume(&mut h, R1[0], "RNG-T1R");
    let report = r.unwrap_or_else(|e| panic!("rewrite: {e}\nsteps: {log:?}"));
    assert_eq!(report.slices, 2, "the two slices the source held");
    assert_eq!(
        log,
        [
            "read",
            "write_preflight",
            "erase",
            "init RNG-T1R",
            "write RNG-T1R"
        ]
    );
    assert_eq!(volume_status(&h.conn, R1[0]), "erased");
    assert_eq!(volume_status(&h.conn, "RNG-T1R"), "sealed");
    assert_eq!(
        write::recorded_ranges(&h.conn, "RNG-T1R").unwrap(),
        [range(B1, 1, 2)],
        "a carried range stays a range"
    );
    assert_eq!(snapshot_status(&h.conn, B1), "current");
    assert_eq!(slice_copies(&h.conn, B1), [1, 1, 1]);

    // The unit restores whole from the cartridges as they are now.
    let mut b = bay(&h, "RNG-T1R", &[Reply::Load(R1[1])]);
    let (r, dest) = restore(
        &h,
        B1,
        What::Unit,
        Some("RNG-T1R"),
        &RestoreOptions::default(),
        &mut b,
    );
    r.unwrap_or_else(|e| panic!("restore: {e}"));
    assert_restored(&h, B1, dest.path());
}

/// The consent for a source holding only a range names the version, the
/// slices the source alone holds, and the volume that holds the rest.
#[test]
fn the_typed_consent_for_a_range_names_the_version_its_slices_and_the_volumes_holding_the_rest() {
    let h = range_only_home();
    let plan = rewrite::plan(&h.conn, &h.config, &options(R1[0], "RNG-T1R")).unwrap();
    assert!(plan.needs_typed_consent());
    let facts = plan.typed_facts(R1[0], "RNG-T1R").join("\n");
    assert!(
        facts.contains(&format!("unit \"{B1}\" v1: 2 slice(s)")),
        "{facts}"
    );
    assert!(facts.contains("slice(s) 1, 2 of 3"), "{facts}");
    assert!(
        facts.contains(&format!("the rest is on \"{}\"", R1[1])),
        "{facts}"
    );
}

/// The rehearsal (the source still sealed) and the write after the erase
/// (the source retired) plan the same slices: the range the source held. The
/// steps are the whole run, and nothing is refused after the erase.
#[test]
fn the_rehearsal_and_the_post_erase_write_plan_the_same_slices() {
    let mut h = range_only_home();
    let ran = rewrite_with(&mut h, R1[0], "RNG-T1R", false);
    ran.result.unwrap_or_else(|e| panic!("{e}\n{:?}", ran.log));
    assert_eq!(
        ran.log,
        [
            "read",
            "write_preflight",
            "erase",
            "init RNG-T1R",
            "write RNG-T1R"
        ]
    );
    let [(_, rehearsal), (_, write)] = &ran.planned[..] else {
        panic!("{:?}", ran.planned)
    };
    assert_eq!(rehearsal, write);
    assert_eq!(
        rehearsal,
        &[(B1.to_string(), 1), (B1.to_string(), 2)],
        "the run the source held, and not slice 3 (tape 2 holds it)"
    );
}

/// The other holder has no full readback (a quick confirm): the rewrite plans
/// and writes the same run, the erase is reached, and the rewrite stays open
/// naming what is left, with the slices held. Nothing is refused after the
/// erase that the rehearsal did not refuse before it.
#[test]
fn another_holder_without_a_full_readback_leaves_the_rewrite_open_not_refused_after_the_erase() {
    let mut h = Home::new(None, 1, "64K");
    h.unit(
        TENANT_ONE,
        B1,
        files("bravo-reel", &[50_000, 50_000, 50_000], 2),
    );
    h.stage(B1);
    for label in R1 {
        h.volume(label);
    }
    write_full(&mut h, R1[0], Warn::InSlice(B1, 2));
    h.write(R1[1], Warn::Never); // a quick confirm: write_verified = 0
    let ran = rewrite_with(&mut h, R1[0], "RNG-T1R", false);
    let err = ran.result.expect_err("held open").to_string();
    assert!(err.contains("took only part of this rewrite"), "{err}");
    assert!(
        err.contains("the rest of the staging is on no volume read back in full"),
        "a range destination did not 'end': {err}"
    );
    assert_eq!(
        ran.log,
        [
            "read",
            "write_preflight",
            "erase",
            "init RNG-T1R",
            "write RNG-T1R"
        ]
    );
    let [(_, rehearsal), (_, write)] = &ran.planned[..] else {
        panic!("{:?}", ran.planned)
    };
    assert_eq!(rehearsal, write);
    assert_eq!(
        write::recorded_ranges(&h.conn, "RNG-T1R").unwrap(),
        [range(B1, 1, 2)]
    );
}

/// The other holder is quarantined: slice 3 then has no live holder and its
/// staged file was released, so the plan owes a slice nothing can write. That
/// is refused by the rehearsal, before the erase: no step past the
/// rehearsal, the source untouched.
#[test]
fn a_quarantined_other_holder_is_refused_before_the_erase() {
    let mut h = range_only_home();
    h.conn
        .execute(
            "UPDATE volumes SET observed_condition = 'quarantined' WHERE label = ?1",
            params![R1[1]],
        )
        .unwrap();
    let ran = rewrite_with(&mut h, R1[0], "RNG-T1R", false);
    let err = ran.result.expect_err("refused").to_string();
    assert!(err.contains("Nothing destructive was done"), "{err}");
    assert_eq!(ran.log, ["read", "write_preflight"], "no erase");
    assert_eq!(volume_status(&h.conn, R1[0]), "sealed");
}

/// `volume rewrite --resume` after the erase finishes a range-only source:
/// the first write stops with the cartridge erased and the destination
/// initialised, and the resume writes the run it was staged as.
#[test]
fn resume_after_the_erase_completes_for_a_range_only_source() {
    let mut h = range_only_home();
    let ran = rewrite_with(&mut h, R1[0], "RNG-T1R", true);
    ran.result.expect_err("the write stopped");
    assert_eq!(
        ran.log,
        [
            "read",
            "write_preflight",
            "erase",
            "init RNG-T1R",
            "write RNG-T1R"
        ]
    );
    assert_eq!(volume_status(&h.conn, "RNG-T1R"), "initialized");
    let steps = MemSteps::new(&mut h, R1[0]);
    let r = rewrite::resume(&steps.h.conn, &steps.h.config, &steps, "RNG-T1R", false);
    let log = steps.log.borrow().clone();
    h.tapes = steps.tapes.into_inner();
    r.unwrap_or_else(|e| panic!("resume: {e}\n{log:?}"));
    assert_eq!(log, ["write RNG-T1R"]);
    assert_eq!(
        write::recorded_ranges(&h.conn, "RNG-T1R").unwrap(),
        [range(B1, 1, 2)]
    );
    let c = &h.conn;
    assert_eq!(
        rewrite::holds(c).unwrap().len(),
        0,
        "the hold is lifted by the full readback"
    );
}

/// An ordinary `volume write` with no carrying source still refuses a
/// half-finished two-cartridge read-back: slices 1-2 are read back from tape
/// 1 and slice 3 stays released, the set is staged, and the write names the
/// slice and the `volume read-slices --from` remedy. (T5.2's case.)
#[test]
fn an_ordinary_write_still_refuses_a_half_finished_two_cartridge_read_back() {
    let mut h = range_only_home();
    h.volume("RNG-N");
    let mut store = copy_of(&h.tapes[R1[0]]);
    write::read_slices(
        &h.conn,
        &h.config,
        R1[0],
        B1,
        None,
        &mut store,
        ContactSite::new(
            &h.config,
            Operation::VolumeReadSlices,
            DEVICE,
            Medium::NoBackend,
        ),
    )
    .unwrap();
    let err = h
        .try_write("RNG-N", Warn::Never)
        .map(|_| ())
        .expect_err("slice 3 is released and not owed")
        .to_string();
    assert!(
        err.contains("slice 3") && err.contains("read-slices"),
        "{err}"
    );
    assert!(err.contains(&format!("--from {}", R1[1])), "{err}");
}

// ── a volume holding the tail of one unit and the head of another ──

const X2: &str = "kumquat-alpha-xray";
const Y2: &str = "kumquat-bravo-yankee";
const T2: [&str; 3] = ["TLH-T1", "TLH-T2", "TLH-T3"];

/// Two units of three slices, copy target 1, over three cartridges, every
/// tape fully read back: tape 1 holds x's head (1-2), tape 2 x's tail (3) and
/// y's head (1-2), tape 3 y's tail (3).
fn tail_and_head_home() -> Home {
    let mut h = Home::new(None, 1, "64K");
    h.unit(TENANT_ONE, X2, files("xray", &[50_000, 50_000, 50_000], 5));
    h.unit(
        TENANT_TWO,
        Y2,
        files("yankee", &[50_000, 50_000, 50_000], 6),
    );
    h.stage(X2);
    h.stage(Y2);
    for label in T2 {
        h.volume(label);
    }
    write_full(&mut h, T2[0], Warn::InSlice(X2, 2));
    write_full(&mut h, T2[1], Warn::InSlice(Y2, 2));
    write_full(&mut h, T2[2], Warn::Never);
    assert_eq!(
        write::recorded_ranges(&h.conn, T2[1]).unwrap(),
        [range(X2, 3, 3), range(Y2, 1, 2)],
        "tape 2 holds the tail of x and the head of y"
    );
    assert_eq!(snapshot_status(&h.conn, X2), "current");
    assert_eq!(snapshot_status(&h.conn, Y2), "current");
    h
}

/// Both units restore whole from the cartridges as they are.
fn assert_both_restore(
    h: &Home,
    x_from: (&'static str, &'static str),
    y_from: (&'static str, &'static str),
) {
    for (unit, (start, next)) in [(X2, x_from), (Y2, y_from)] {
        let mut b = bay(h, start, &[Reply::Load(next)]);
        let (r, dest) = restore(
            h,
            unit,
            What::Unit,
            Some(start),
            &RestoreOptions::default(),
            &mut b,
        );
        r.unwrap_or_else(|e| panic!("restore {unit}: {e}"));
        assert_restored(h, unit, dest.path());
    }
}

/// The volume holding the tail of one unit and the head of another is
/// rewritten, and each unit restores whole from its cartridges. The new
/// volume holds the same two runs.
#[test]
fn a_volume_holding_a_tail_and_a_head_is_rewritten_and_each_unit_restores_whole() {
    let mut h = tail_and_head_home();
    let ran = rewrite_with(&mut h, T2[1], "TLH-T2R", false);
    ran.result.unwrap_or_else(|e| panic!("{e}\n{:?}", ran.log));
    assert_eq!(
        write::recorded_ranges(&h.conn, "TLH-T2R").unwrap(),
        [range(X2, 3, 3), range(Y2, 1, 2)]
    );
    let [(_, rehearsal), (_, write)] = &ran.planned[..] else {
        panic!("{:?}", ran.planned)
    };
    assert_eq!(rehearsal, write);
    assert_both_restore(&h, (T2[0], "TLH-T2R"), ("TLH-T2R", T2[2]));
}
