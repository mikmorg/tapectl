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
    /// When the early warning comes on the destination's write.
    dest_warn: Cell<Warn>,
    /// `(step, slices planned)` at the rehearsal and at the write: what
    /// `volume write` would plan for every staged set, `(unit, slice)`.
    planned: RefCell<Vec<Planned>>,
}

/// `(step, slices planned)`: `(unit, slice number)` in plan order.
type Planned = (String, Vec<(String, i64)>);

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
            dest_warn: Cell::new(Warn::Never),
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
        let mut drive = Drive::new(&self.h.conn, label, medium, self.dest_warn.get());
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
    try_write_full(h, label, warn).unwrap_or_else(|e| panic!("volume write {label}: {e}"));
    crate::cli::volume::release_staging_after_write(&h.conn, &h.config, label, false);
}

/// [`write_full`]'s write, its result given back (the medium is put back
/// either way) and no staging release.
fn try_write_full(h: &mut Home, label: &str, warn: Warn) -> Result<()> {
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
    r
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
    planned: Vec<Planned>,
}

/// `volume rewrite SOURCE --label DESTINATION` of `h`, the typed consent
/// given on the command line. The destination's medium is the source's
/// cartridge, written again. `fail_write`: the first write step fails.
fn rewrite_with(h: &mut Home, source: &str, destination: &str, fail_write: bool) -> Ran {
    rewrite_warning(h, source, destination, fail_write, Warn::Never)
}

/// [`rewrite_with`], the early warning coming on the destination's write as
/// `warn` says.
fn rewrite_warning(
    h: &mut Home,
    source: &str,
    destination: &str,
    fail_write: bool,
    warn: Warn,
) -> Ran {
    let steps = MemSteps::new(h, source);
    steps.fail_write.set(fail_write);
    steps.dest_warn.set(warn);
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

    // A quarantined holder holds nothing a reader may rely on: it is not
    // named as holding the rest.
    h.conn
        .execute(
            "UPDATE volumes SET observed_condition = 'quarantined' WHERE label = ?1",
            params![R1[1]],
        )
        .unwrap();
    let plan = rewrite::plan(&h.conn, &h.config, &options(R1[0], "RNG-T1R")).unwrap();
    let facts = plan.typed_facts(R1[0], "RNG-T1R").join("\n");
    assert!(facts.contains("the rest is on no other volume"), "{facts}");
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
/// and writes the same run, the erase is reached, and the rewrite CLOSES: the
/// hold and the close are tested for each slice the rewrite staged (#579;
/// `volume-format-v3.md` §5.4), and slices 1 and 2, the only ones it staged,
/// are on the destination and read back in full. Slice 3 was never the
/// rewrite's: its holder's quick confirm is not the rewrite's to wait on.
/// Nothing is refused after the erase that the rehearsal did not refuse
/// before it.
///
/// This test pinned "stays open" (the hold asked every slice of the stage
/// set); it changes with #579's fix to the per-slice test the normative text
/// states.
#[test]
fn another_holder_without_a_full_readback_does_not_hold_the_rewrite_open() {
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
    ran.result
        .unwrap_or_else(|e| panic!("the rewrite closes: {e}\n{:?}", ran.log));
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
    assert_eq!(rewrite::holds(&h.conn).unwrap().len(), 0, "no hold is left");
}

/// The rewrite's stop is reached when a slice IT staged has no holder read
/// back in full: the destination's tape ends at the early warning inside the
/// run. The stop names those staged slices, and the remedy it names (an
/// ordinary `volume write --full-confirm` onto another cartridge, then
/// `--resume`) closes the rewrite (#579's loop). A source that holds a range,
/// and one that holds the whole staging, are both in this state.
#[test]
fn the_stops_remedy_followed_by_resume_closes_the_rewrite_in_every_state_it_is_reached() {
    // A source that holds only a range (slices 1-2 of 3; tape 2 holds 3).
    let mut h = range_only_home();
    h.volume("RNG-X");
    let stopped = |h: &mut Home, source: &str, dest: &str, warn: Warn, staged: &str| {
        let ran = rewrite_warning(h, source, dest, false, warn);
        let err = ran
            .result
            .expect_err("the destination took only part")
            .to_string();
        assert!(err.contains("took only part of this rewrite"), "{err}");
        assert!(err.contains(staged), "names the staged slices left: {err}");
        assert!(
            !err.contains("every staged set") && !err.contains("their only copy"),
            "says only what is true: {err}"
        );
        assert!(
            err.contains("tapectl volume write <LABEL> --init --full-confirm")
                && err.contains(&format!("tapectl volume rewrite --resume {dest}")),
            "{err}"
        );
        assert_eq!(
            rewrite::holds(&h.conn).unwrap().len(),
            1,
            "the staged slice with no holder keeps the set held"
        );
    };
    stopped(
        &mut h,
        R1[0],
        "RNG-T1R",
        Warn::InSlice(B1, 1),
        "staged slice 2",
    );
    // The remedy, then the resume.
    write_full(&mut h, "RNG-X", Warn::Never);
    let steps = MemSteps::new(&mut h, R1[0]);
    let r = rewrite::resume(&steps.h.conn, &steps.h.config, &steps, "RNG-T1R", false);
    let log = steps.log.borrow().clone();
    h.tapes = steps.tapes.into_inner();
    r.unwrap_or_else(|e| panic!("resume closes the rewrite: {e}\n{log:?}"));
    assert_eq!(rewrite::holds(&h.conn).unwrap().len(), 0);
    let remedy = write::recorded_ranges(&h.conn, "RNG-X").unwrap();
    assert!(
        remedy.len() == 1 && remedy[0].last_slice == 2 && remedy[0].first_slice <= 2,
        "the remedy wrote the slice the destination did not take (a carried run is \
         re-planned whole, which costs tape and is not wrong): {remedy:?}"
    );

    // A source that holds the staging whole.
    let mut h = Home::new(None, 1, "64K");
    h.unit(
        TENANT_ONE,
        B1,
        files("bravo-reel", &[50_000, 50_000, 50_000], 2),
    );
    h.stage(B1);
    h.volume("WHL-T1");
    h.volume("WHL-X");
    write_full(&mut h, "WHL-T1", Warn::Never);
    stopped(
        &mut h,
        "WHL-T1",
        "WHL-T1R",
        Warn::InSlice(B1, 2),
        "staged slice 3",
    );
    write_full(&mut h, "WHL-X", Warn::Never);
    let steps = MemSteps::new(&mut h, "WHL-T1");
    let r = rewrite::resume(&steps.h.conn, &steps.h.config, &steps, "WHL-T1R", false);
    let log = steps.log.borrow().clone();
    h.tapes = steps.tapes.into_inner();
    r.unwrap_or_else(|e| panic!("resume closes the rewrite: {e}\n{log:?}"));
    assert_eq!(rewrite::holds(&h.conn).unwrap().len(), 0);
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

// ── compaction of a range ──

/// `volume compact-read` of `label`, from a copy of its recorded medium.
fn compact_read_from(h: &Home, label: &str) -> Result<CompactReadReport> {
    let mut store = copy_of(&h.tapes[label]);
    write::compact_read(
        &h.conn,
        &h.config,
        label,
        &mut store,
        ContactSite::new(
            &h.config,
            Operation::VolumeCompactRead,
            DEVICE,
            Medium::NoBackend,
        ),
    )
}

/// `volume compact-write` to `label`: an ordinary write of everything staged,
/// with the full readback.
fn compact_write_to(h: &mut Home, label: &str) {
    write_full(h, label, Warn::Never);
}

fn compact_finish(h: &Home, label: &str) -> Result<()> {
    write::compact_finish(&h.conn, &h.config, label, true).map(|_| ())
}

/// A range-only source is compacted: read, written as the run it was onto a
/// new volume, and retired by `compact-finish`, which first asks that every
/// slice it holds is held elsewhere. The destination holds a range of 1-2,
/// the unit restores whole from it and the cartridge holding the rest, and
/// `compact-finish` refuses before the write while the range's slices have no
/// other holder.
#[test]
fn a_range_only_source_is_compacted_and_compact_finish_waits_for_the_other_holder() {
    let mut h = range_only_home();
    h.volume("RNG-C");
    let read = compact_read_from(&h, R1[0]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(read.slices_read, 2);
    let err = compact_finish(&h, R1[0])
        .expect_err("the range's slices are held by no other volume yet")
        .to_string();
    assert!(
        err.contains("2 live slice(s) have no copy on another volume"),
        "{err}"
    );
    assert_eq!(volume_status(&h.conn, R1[0]), "sealed");

    compact_write_to(&mut h, "RNG-C");
    assert_eq!(
        write::recorded_ranges(&h.conn, "RNG-C").unwrap(),
        [range(B1, 1, 2)],
        "the carried run stays a range"
    );
    compact_finish(&h, R1[0]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(volume_status(&h.conn, R1[0]), "retired");
    assert_eq!(snapshot_status(&h.conn, B1), "current");
    assert_eq!(slice_copies(&h.conn, B1), [1, 1, 1]);
    let mut b = bay(&h, "RNG-C", &[Reply::Load(R1[1])]);
    let (r, dest) = restore(
        &h,
        B1,
        What::Unit,
        Some("RNG-C"),
        &RestoreOptions::default(),
        &mut b,
    );
    r.unwrap_or_else(|e| panic!("restore: {e}"));
    assert_restored(&h, B1, dest.path());
}

/// The volume holding the tail of one unit and the head of another is
/// compacted, and each unit restores whole from its cartridges.
#[test]
fn a_volume_holding_a_tail_and_a_head_is_compacted_and_each_unit_restores_whole() {
    let mut h = tail_and_head_home();
    h.volume("TLH-C");
    compact_read_from(&h, T2[1]).unwrap_or_else(|e| panic!("{e}"));
    compact_write_to(&mut h, "TLH-C");
    assert_eq!(
        write::recorded_ranges(&h.conn, "TLH-C").unwrap(),
        [range(X2, 3, 3), range(Y2, 1, 2)]
    );
    compact_finish(&h, T2[1]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(volume_status(&h.conn, T2[1]), "retired");
    assert_both_restore(&h, (T2[0], "TLH-C"), ("TLH-C", T2[2]));
}

/// A volume is sized, as a compaction candidate, by what it holds: a range
/// counts its own slices, not its set's.
#[test]
fn a_volume_holding_a_range_is_a_compaction_candidate_sized_by_its_slices() {
    let h = range_only_home();
    let rows = crate::cli::report::compaction_candidate_rows(&h.conn).unwrap();
    let held = |label: &str| -> i64 {
        h.conn
            .query_row(
                "SELECT SUM(sl.encrypted_bytes) FROM write_positions wp
                   JOIN writes w ON w.id = wp.write_id
                   JOIN volumes v ON v.id = w.volume_id
                   JOIN stage_slices sl ON sl.id = wp.stage_slice_id
                  WHERE v.label = ?1 AND wp.status = 'written'",
                params![label],
                |r| r.get(0),
            )
            .unwrap()
    };
    for label in R1 {
        let row = rows
            .iter()
            .find(|r| r.label == label)
            .unwrap_or_else(|| panic!("{label} is a row: {rows:?}"));
        assert_eq!(row.live_bytes, held(label), "{label}");
    }
}

/// Two sources whose runs of one staging do not join are refused onto one
/// destination, by hand, before the second is read: tape 1 holds slices 1-2
/// of five, tape 2 slice 3, tape 3 slices 4-5; reading tape 1 and then
/// tape 3 would stage 1-2 and 4-5.
#[test]
fn two_sources_whose_runs_do_not_join_are_refused_naming_the_staging() {
    let mut h = Home::new(None, 1, "64K");
    let unit = "kumquat-zspan-delta";
    h.unit(TENANT_ONE, unit, files("delta-reel", &[50_000; 5], 9));
    h.stage(unit);
    assert_eq!(num_slices(&h.conn, unit), 5);
    for label in ["JN-T1", "JN-T2", "JN-T3"] {
        h.volume(label);
    }
    write_full(&mut h, "JN-T1", Warn::InSlice(unit, 2));
    write_full(&mut h, "JN-T2", Warn::InSlice(unit, 3));
    write_full(&mut h, "JN-T3", Warn::Never);
    assert_eq!(
        write::recorded_ranges(&h.conn, "JN-T3").unwrap(),
        [range(unit, 4, 5)]
    );
    compact_read_from(&h, "JN-T1").unwrap_or_else(|e| panic!("{e}"));
    let err = compact_read_from(&h, "JN-T3")
        .expect_err("1-2 and 4-5 do not join")
        .to_string();
    assert!(
        err.contains(unit) && err.contains("1, 2, 4, 5") && err.contains("unbroken"),
        "{err}"
    );
    assert!(err.contains("Nothing was read or staged"), "{err}");
    // The joining neighbour is taken.
    compact_read_from(&h, "JN-T2").unwrap_or_else(|e| panic!("{e}"));
}

/// A rewritten volume and a compacted one each rebuild, from the tape's own
/// envelope, to the rows the writing catalog holds for it: its writes, its
/// positions and its ending, and the slice rows the manifest carries.
#[test]
fn a_rewritten_and_a_compacted_volume_rebuild_to_the_same_rows() {
    let mut h = range_only_home();
    rewrite_volume(&mut h, R1[0], "RNG-T1R")
        .0
        .unwrap_or_else(|e| panic!("{e}"));
    h.volume("RNG-C");
    // Compact tape 2 (slice 3) as well.
    compact_read_from(&h, R1[1]).unwrap_or_else(|e| panic!("{e}"));
    compact_write_to(&mut h, "RNG-C");
    compact_finish(&h, R1[1]).unwrap_or_else(|e| panic!("{e}"));
    let labels = ["RNG-C", "RNG-T1R"];
    let mine = |rows: Vec<WriteRow>| -> Vec<WriteRow> {
        rows.into_iter()
            .filter(|r| labels.contains(&r.0.as_str()))
            .collect()
    };
    let dir = TempDir::new().unwrap();
    let fresh = crate::db::open(&dir.path().join("rebuilt.db")).unwrap();
    for label in labels {
        rebuild(&h, &fresh, label);
    }
    assert_eq!(mine(write_rows(&fresh)), mine(write_rows(&h.conn)));
    let pos = |rows: Vec<PositionRow>| -> Vec<PositionRow> {
        rows.into_iter()
            .filter(|r| labels.contains(&r.0.as_str()))
            .collect()
    };
    assert_eq!(pos(position_rows(&fresh)), pos(position_rows(&h.conn)));
    assert_eq!(slice_rows(&fresh), slice_rows(&h.conn));
    let vols = |rows: Vec<(String, bool, Option<i64>)>| -> Vec<(String, bool, Option<i64>)> {
        rows.into_iter()
            .filter(|r| labels.contains(&r.0.as_str()))
            .collect()
    };
    assert_eq!(vols(volume_rows(&fresh)), vols(volume_rows(&h.conn)));
    assert_eq!(
        rewrite::holds(&h.conn).unwrap().len(),
        0,
        "no hold is left open"
    );
}

// ── item 300: a carried run binds its generation in both directions (#590) ──

/// `volume` is made on an LTO-8 cartridge (the fixtures' cartridges are
/// LTO-6): its generation and capacity are the catalog's, as `volume init`
/// decides them from the medium.
fn make_lto8(h: &Home, volume: &str) {
    h.conn
        .execute(
            "UPDATE volumes SET media_type = 'LTO-8', capacity_bytes = 12000000000000
             WHERE label = ?1",
            params![volume],
        )
        .unwrap();
}

/// The drive is an LTO-8 now: it is the drive of the destination's write (a
/// drive writes its own generation and the one before it, so an LTO-6 drive
/// is no drive for an LTO-8 cartridge). The tapes already written stay what
/// they were, LTO-6.
fn lto8_drive(h: &mut Home) {
    h.config.backends.lto[0].generation = "LTO-8".into();
}

/// What the catalog and the cartridge hold of `volume`: nothing was written
/// since `files_before` (what `volume init` left on it).
fn assert_nothing_written(h: &Home, volume: &str, files_before: usize) {
    assert!(
        write::recorded_ranges(&h.conn, volume).unwrap().is_empty(),
        "no range was recorded on {volume}"
    );
    assert_eq!(
        h.tapes[volume].files.len(),
        files_before,
        "no file reached {volume}'s cartridge"
    );
}

/// #590's trace: tape 1 (LTO-6) holds a version's head 1-2, tape 2 (LTO-6)
/// the rest. Compacting tape 1 onto an LTO-8 destination would leave the
/// version on two generations, which no one drive reads: the write is
/// refused, before the drive is touched, naming tape 2 and LTO-6, and the
/// source is not retired. Positive control: both tapes carried into the one
/// LTO-8 destination pass, and `compact-finish` retires both.
#[test]
fn a_head_compacted_to_another_generation_than_the_range_after_it_is_refused() {
    let mut h = range_only_home();
    h.volume("RNG-C");
    make_lto8(&h, "RNG-C");
    lto8_drive(&mut h);
    compact_read_from(&h, R1[0]).unwrap_or_else(|e| panic!("{e}"));
    let on_tape = h.tapes["RNG-C"].files.len();
    let err = try_write_full(&mut h, "RNG-C", Warn::Never)
        .expect_err("the head's destination is another generation than the range after it")
        .to_string();
    assert!(
        err.contains("volume \"RNG-C\" is LTO-8")
            && err.contains(&format!("volume \"{}\", which is LTO-6", R1[1]))
            && err.contains("ADR-0012 items 86 and 300")
            && err.contains("Nothing was written and the drive was not touched"),
        "{err}"
    );
    assert_nothing_written(&h, "RNG-C", on_tape);
    let finish = compact_finish(&h, R1[0])
        .expect_err("the source holds the only copy")
        .to_string();
    assert!(
        finish.contains("have no copy on another volume"),
        "{finish}"
    );
    assert_eq!(
        volume_status(&h.conn, R1[0]),
        "sealed",
        "nothing was retired"
    );

    // Both tapes of the version into the same destination: slices 1-2 and 3
    // join into the whole staging, which adjoins no range.
    compact_read_from(&h, R1[1]).unwrap_or_else(|e| panic!("{e}"));
    compact_write_to(&mut h, "RNG-C");
    assert_eq!(
        write::recorded_ranges(&h.conn, "RNG-C").unwrap(),
        Vec::<_>::new(),
        "the destination holds the whole staging, not a range"
    );
    compact_finish(&h, R1[0]).unwrap_or_else(|e| panic!("{e}"));
    compact_finish(&h, R1[1]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(volume_status(&h.conn, R1[0]), "retired");
    assert_eq!(volume_status(&h.conn, R1[1]), "retired");
    assert_eq!(snapshot_status(&h.conn, B1), "current");
    assert_eq!(slice_copies(&h.conn, B1), [1, 1, 1]);
}

/// A carried middle run adjoins a range on each side. Tape 1 holds 1-2, tape
/// 2 slice 3 and tape 3 slices 4-5. Tape 1 is of the destination's
/// generation, so the direction item 86 had passes; tape 3 is not, and the
/// range that begins just after the run refuses it, naming tape 3.
#[test]
fn a_middle_run_compacted_to_another_generation_than_the_range_after_it_is_refused() {
    let mut h = Home::new(None, 1, "64K");
    let unit = "kumquat-zspan-delta";
    h.unit(TENANT_ONE, unit, files("delta-reel", &[50_000; 5], 9));
    h.stage(unit);
    assert_eq!(num_slices(&h.conn, unit), 5);
    for label in ["MD-T1", "MD-T2", "MD-T3", "MD-C"] {
        h.volume(label);
    }
    write_full(&mut h, "MD-T1", Warn::InSlice(unit, 2));
    write_full(&mut h, "MD-T2", Warn::InSlice(unit, 3));
    write_full(&mut h, "MD-T3", Warn::Never);
    assert_eq!(
        write::recorded_ranges(&h.conn, "MD-T2").unwrap(),
        [range(unit, 3, 3)]
    );
    make_lto8(&h, "MD-T1");
    make_lto8(&h, "MD-C");
    lto8_drive(&mut h);
    compact_read_from(&h, "MD-T2").unwrap_or_else(|e| panic!("{e}"));
    let on_tape = h.tapes["MD-C"].files.len();
    let err = try_write_full(&mut h, "MD-C", Warn::Never)
        .expect_err("the range after the run is of another generation")
        .to_string();
    assert!(
        err.contains("volume \"MD-C\" is LTO-8")
            && err.contains("volume \"MD-T3\", which is LTO-6")
            && err.contains("ADR-0012 items 86 and 300"),
        "{err}"
    );
    assert_nothing_written(&h, "MD-C", on_tape);
    assert_eq!(volume_status(&h.conn, "MD-T2"), "sealed");

    // Positive control: once the range after it is of the destination's
    // generation too, the middle run is carried.
    make_lto8(&h, "MD-T3");
    compact_write_to(&mut h, "MD-C");
    assert_eq!(
        write::recorded_ranges(&h.conn, "MD-C").unwrap(),
        [range(unit, 3, 3)]
    );
}

/// `volume rewrite` writes onto the source's own cartridge, so it keeps the
/// source's generation; it reaches item 300 only where the version already
/// stands on two generations (a catalog from before the rule). It refuses
/// that at the rehearsal, before it erases or retires anything, in the
/// direction item 86 had (the range before the tail) and the one item 300
/// adds (the range after the head), naming the volume and its generation.
#[test]
fn a_rewrite_that_would_leave_a_version_on_two_generations_is_refused_before_the_erase() {
    // The head (tape 1) rewritten, the range after it on LTO-8.
    let mut h = range_only_home();
    make_lto8(&h, R1[1]);
    let ran = rewrite_with(&mut h, R1[0], "RNG-T1R", false);
    let err = ran
        .result
        .expect_err("the range after the head is of another generation")
        .to_string();
    assert!(
        err.contains(&format!("volume \"{}\", which is LTO-8", R1[1]))
            && err.contains("ADR-0012 items 86 and 300"),
        "{err}"
    );
    assert_eq!(ran.log, ["read", "write_preflight"], "no erase");
    assert_eq!(volume_status(&h.conn, R1[0]), "sealed");
    assert_eq!(rewrite::holds(&h.conn).unwrap().len(), 0, "no hold is left");

    // The tail (tape 2) rewritten, the range before it on LTO-8.
    let mut h = range_only_home();
    make_lto8(&h, R1[0]);
    let ran = rewrite_with(&mut h, R1[1], "RNG-T2R", false);
    let err = ran
        .result
        .expect_err("the range before the tail is of another generation")
        .to_string();
    assert!(
        err.contains(&format!("volume \"{}\", which is LTO-8", R1[0])),
        "{err}"
    );
    assert_eq!(ran.log, ["read", "write_preflight"], "no erase");
    assert_eq!(volume_status(&h.conn, R1[1]), "sealed");
}

// ── #598: the low findings of area 8 ──

const P8: &str = "kumquat-zspan-papa";
const P8_TAPES: [&str; 3] = ["OVL-A", "OVL-B", "OVL-C"];

/// Four slices at a copy target of 2 over three cartridges, every tape fully
/// read back: tape A holds a range 1-3, tape B a range 1-2, tape C the owed
/// 3-4. Slice 3 is on A and C, slice 4 only on C.
fn overlapping_copies_home() -> Home {
    let mut h = Home::new(None, 2, "64K");
    h.unit(TENANT_ONE, P8, files("papa-reel", &[50_000; 4], 11));
    h.stage(P8);
    assert_eq!(num_slices(&h.conn, P8), 4);
    for label in P8_TAPES {
        h.volume(label);
    }
    write_full(&mut h, P8_TAPES[0], Warn::InSlice(P8, 3));
    write_full(&mut h, P8_TAPES[1], Warn::InSlice(P8, 2));
    write_full(&mut h, P8_TAPES[2], Warn::Never);
    h
}

/// 8-4: the consent says which slices the source holds, not only the ones it
/// holds alone (§5.4: "which slices ... and the volumes that hold the rest").
/// Tape C holds slices 3 and 4, and tape A also holds slice 3: the sentence
/// names 3, 4 as C's, and says 4 is the one no other volume holds.
#[test]
fn the_consent_names_every_slice_the_source_holds_and_the_one_it_holds_alone() {
    let h = overlapping_copies_home();
    assert_eq!(
        write::recorded_ranges(&h.conn, P8_TAPES[2]).unwrap(),
        [range(P8, 3, 4)],
        "tape C holds the owed 3-4"
    );
    let plan = rewrite::plan(&h.conn, &h.config, &options(P8_TAPES[2], "OVL-CR")).unwrap();
    let facts = plan.typed_facts(P8_TAPES[2], "OVL-CR").join("\n");
    assert!(
        facts.contains(&format!("\"{}\" holds of this version", P8_TAPES[2])),
        "{facts}"
    );
    assert!(facts.contains("slice(s) 3, 4 of 4, which"), "{facts}");
    assert!(
        facts.contains("and slice(s) 4 are on no other volume"),
        "{facts}"
    );
}

const S8: &str = "kumquat-zspan-sierra";
const S8_TAPES: [&str; 3] = ["SPL-1", "SPL-2", "SPL-3"];

/// 8-C2: a version of six slices at a copy target of 1 over three tapes, each
/// fully read back: 1-2, 3-4 and 5-6.
fn three_tape_span_home() -> Home {
    let mut h = Home::new(None, 1, "64K");
    h.unit(TENANT_ONE, S8, files("sierra-reel", &[50_000; 6], 13));
    h.stage(S8);
    assert_eq!(num_slices(&h.conn, S8), 6);
    for label in S8_TAPES {
        h.volume(label);
    }
    write_full(&mut h, S8_TAPES[0], Warn::InSlice(S8, 2));
    write_full(&mut h, S8_TAPES[1], Warn::InSlice(S8, 4));
    write_full(&mut h, S8_TAPES[2], Warn::Never);
    assert_eq!(
        write::recorded_ranges(&h.conn, S8_TAPES[2]).unwrap(),
        [range(S8, 5, 6)]
    );
    h
}

/// 8-C2: slices already staged that the write would not plan (their copies
/// meet the floor and no source carries them) cannot make the run the
/// rewrite writes break, so they do not refuse the read. Tape 1's slices
/// 1-2 are staged by `read-slices`, then tape 3 (5-6) is rewritten: the write
/// plans only 5-6, one unbroken run, and the join refusal ("compact the
/// volume that holds the slices between them") is wrong for it. A slice that
/// WOULD be planned (below the floor) still refuses, naming the staging.
#[test]
fn staged_slices_the_write_would_not_plan_do_not_refuse_a_rewrite() {
    let mut h = three_tape_span_home();
    let mut store = copy_of(&h.tapes[S8_TAPES[0]]);
    write::read_slices(
        &h.conn,
        &h.config,
        S8_TAPES[0],
        S8,
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
    let ran = rewrite_with(&mut h, S8_TAPES[2], "SPL-3R", false);
    let report = ran
        .result
        .unwrap_or_else(|e| panic!("rewrite: {e}\nsteps: {:?}", ran.log));
    assert_eq!(report.slices, 2, "the two slices the source held");
    assert_eq!(
        write::recorded_ranges(&h.conn, "SPL-3R").unwrap(),
        [range(S8, 5, 6)],
        "the run the source held, and only it"
    );
}

const HEAD8: &str = "kumquat-alpha-hhh";
const TAIL8: &str = "kumquat-bravo-ttt";
const ORDER_TAPES: [&str; 3] = ["ORD-T1", "ORD-T2", "ORD-T3"];

/// 8-3: a rewrite lays the stagings in the order the source held them
/// (`volume-format-v3.md` §5.4: "the stagings keep the source's order"). Tape
/// 2 holds `TAIL8`'s tail (3), then `HEAD8`'s head (1-2), though `HEAD8`
/// sorts first by name: the destination is laid out the same, owed runs both,
/// in the source's order and not in unit-name order.
#[test]
fn a_rewrite_lays_the_carried_stagings_in_the_order_the_source_held_them() {
    let mut h = Home::new(None, 1, "64K");
    h.unit(TENANT_ONE, TAIL8, files("tail-reel", &[50_000; 3], 21));
    h.stage(TAIL8);
    for label in ORDER_TAPES {
        h.volume(label);
    }
    write_full(&mut h, ORDER_TAPES[0], Warn::InSlice(TAIL8, 2));
    h.unit(TENANT_TWO, HEAD8, files("head-reel", &[50_000; 3], 22));
    h.stage(HEAD8);
    write_full(&mut h, ORDER_TAPES[1], Warn::InSlice(HEAD8, 2));
    write_full(&mut h, ORDER_TAPES[2], Warn::Never);
    assert_eq!(
        write::recorded_ranges(&h.conn, ORDER_TAPES[1]).unwrap(),
        [range(TAIL8, 3, 3), range(HEAD8, 1, 2)],
        "tape 2 holds the tail first, then the head"
    );
    let ran = rewrite_with(&mut h, ORDER_TAPES[1], "ORD-T2R", false);
    ran.result.unwrap_or_else(|e| panic!("{e}\n{:?}", ran.log));
    let [(_, rehearsal), (_, written)] = &ran.planned[..] else {
        panic!("{:?}", ran.planned)
    };
    assert_eq!(rehearsal, written);
    assert_eq!(
        rehearsal,
        &[
            (TAIL8.to_string(), 3),
            (HEAD8.to_string(), 1),
            (HEAD8.to_string(), 2)
        ],
        "the source's order, not the unit names'"
    );
    assert_eq!(
        write::recorded_ranges(&h.conn, "ORD-T2R").unwrap(),
        [range(TAIL8, 3, 3), range(HEAD8, 1, 2)]
    );
}

const MID9: &str = "kumquat-mmm-whole";
const HEAD9: &str = "kumquat-zzz-head";
const TAIL9: &str = "kumquat-aaa-tail";
const ADDED9: &str = "kumquat-000-added";
const OWED9: &str = "kumquat-yyy-owed";
const ABC_TAPES: [&str; 3] = ["ABC-T1", "ABC-T2", "ABC-T3"];

/// A source holding A's tail (owed), C whole (queued) and B's head at its end
/// (owed): tape 2 of three. By name the order is A (`TAIL9`), C (`MID9`), B
/// (`HEAD9`), so a plain write laid tape 2 so; a rewrite that took the
/// carried owed runs first would lay A, B, C.
fn tail_whole_head_home() -> Home {
    let mut h = Home::new(None, 1, "64K");
    h.unit(TENANT_ONE, TAIL9, files("tail-nine", &[50_000; 3], 31));
    h.stage(TAIL9);
    for label in ABC_TAPES {
        h.volume(label);
    }
    write_full(&mut h, ABC_TAPES[0], Warn::InSlice(TAIL9, 2));
    h.unit(TENANT_ONE, MID9, files("mid-nine", &[20_000], 32));
    h.stage(MID9);
    h.unit(TENANT_TWO, HEAD9, files("head-nine", &[50_000; 3], 33));
    h.stage(HEAD9);
    write_full(&mut h, ABC_TAPES[1], Warn::InSlice(HEAD9, 2));
    write_full(&mut h, ABC_TAPES[2], Warn::Never);
    assert_eq!(
        write::recorded_ranges(&h.conn, ABC_TAPES[1]).unwrap(),
        [range(TAIL9, 3, 3), range(HEAD9, 1, 2)],
        "tape 2 holds A's tail, C whole, B's head"
    );
    h
}

/// The slices a volume holds in tape order: `(unit, slice number)`.
fn laid_on(h: &Home, label: &str) -> Vec<(String, i64)> {
    let mut stmt = h
        .conn
        .prepare(
            "SELECT u.name, sl.slice_number
               FROM write_positions wp
               JOIN writes w ON w.id = wp.write_id
               JOIN volumes v ON v.id = w.volume_id
               JOIN stage_slices sl ON sl.id = wp.stage_slice_id
               JOIN stage_sets ss ON ss.id = sl.stage_set_id
               JOIN snapshots s ON s.id = ss.snapshot_id
               JOIN units u ON u.id = s.unit_id
              WHERE v.label = ?1 AND wp.status = 'written'
                AND w.status IN ('completed', 'range')
              ORDER BY CAST(wp.position AS INTEGER)",
        )
        .unwrap();
    let rows = stmt
        .query_map(params![label], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    rows
}

fn a_c_b() -> Vec<(String, i64)> {
    vec![
        (TAIL9.to_string(), 3),
        (MID9.to_string(), 1),
        (HEAD9.to_string(), 1),
        (HEAD9.to_string(), 2),
    ]
}

/// Item 305: a rewrite keeps the source's order for every set it carries,
/// owed or queued alike. The source holds A's tail, C whole and B's head;
/// the destination is laid A, C, B, not A, B, C.
#[test]
fn a_rewrite_lays_an_owed_and_a_queued_carried_set_in_the_sources_order() {
    let mut h = tail_whole_head_home();
    assert_eq!(laid_on(&h, ABC_TAPES[1]), a_c_b(), "the source");
    let ran = rewrite_with(&mut h, ABC_TAPES[1], "ABC-T2R", false);
    ran.result.unwrap_or_else(|e| panic!("{e}\n{:?}", ran.log));
    assert_eq!(laid_on(&h, "ABC-T2R"), a_c_b(), "the destination");
}

/// Item 305 for a compaction: the same source, `compact-read` and
/// `compact-write`.
#[test]
fn a_compaction_lays_an_owed_and_a_queued_carried_set_in_the_sources_order() {
    let mut h = tail_whole_head_home();
    h.volume("ABC-C");
    compact_read_from(&h, ABC_TAPES[1]).unwrap_or_else(|e| panic!("{e}"));
    compact_write_to(&mut h, "ABC-C");
    assert_eq!(laid_on(&h, "ABC-C"), a_c_b());
}

/// Item 305: the sets `--with-staged` adds follow every carried set, owed
/// first (item 289's order among them): an added set with its tail owed
/// (`OWED9`, a range on another tape) before an added set queued (`ADDED9`,
/// first by name).
#[test]
fn the_sets_a_rewrite_adds_follow_every_carried_set_owed_first() {
    let mut h = tail_whole_head_home();
    h.volume("ABC-T4");
    h.unit(TENANT_ONE, OWED9, files("owed-nine", &[50_000; 3], 34));
    h.stage(OWED9);
    write_full(&mut h, "ABC-T4", Warn::InSlice(OWED9, 2));
    h.unit(TENANT_TWO, ADDED9, files("added-nine", &[20_000], 35));
    h.stage(ADDED9);
    let steps = MemSteps::new(&mut h, ABC_TAPES[1]);
    let mut o = options(ABC_TAPES[1], "ABC-T2R");
    o.with_staged = true;
    let result = rewrite::rewrite(&steps.h.conn, &steps.h.config, &steps, &o);
    let log = steps.log.borrow().clone();
    h.tapes = steps.tapes.into_inner();
    result.unwrap_or_else(|e| panic!("{e}\n{log:?}"));
    let mut expected = a_c_b();
    expected.push((OWED9.to_string(), 3));
    expected.push((ADDED9.to_string(), 1));
    assert_eq!(laid_on(&h, "ABC-T2R"), expected);
}
