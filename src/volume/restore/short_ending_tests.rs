//! #420: restore through tapectl from a tape that ended early
//! (`volume-format-v3.md` §5.1 R3-R5; `volume-format-v3.md` §5), on the shared
//! fixture (`short_tape`): `alpha` whole, `bravo` a range with its first slice
//! on the tape, `charlie` planned and not written (no row). The units are staged by the real `stage create`
//! and the tape is written by the real session; a restore reads the unit's
//! file table from the tape's own catalog and feeds tar whole members.

use std::fs;
use std::path::Path;

use rusqlite::params;
use tempfile::TempDir;

use super::short_tape::{self, ShortTape, LABEL};
use super::{
    restore_unit_from_store, restore_units_from_loaded_store, select_write_positions,
    units_on_volume, RestoreOptions, RestoreTarget,
};
use crate::config::Config;
use crate::tape::contact::{ContactSite, Medium, Operation};

/// The `ContactSite` of a `MemStore` test: no configured backend, as a
/// machine with no drive at all.
fn site(operation: Operation) -> ContactSite<'static> {
    static CFG: std::sync::OnceLock<Config> = std::sync::OnceLock::new();
    ContactSite::new(
        CFG.get_or_init(Config::default),
        operation,
        "/nonexistent/tapectl-contact-test-nst",
        Medium::NoBackend,
    )
}

/// The unit's own dotfile: it is a member of every stream.
const DOTFILE: &str = ".tapectl-unit.toml";

/// The regular files directly under `root`, the unit's dotfile left out
/// (its presence is asserted by the caller).
fn files_under(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = fs::read_dir(root)
        .unwrap()
        .flatten()
        .filter(|e| e.path().is_file() && e.file_name() != DOTFILE)
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                fs::read(e.path()).unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

fn expected(t: &ShortTape, unit: &str) -> Vec<(String, Vec<u8>)> {
    let mut files = t.files.iter().find(|(u, _)| u == unit).unwrap().1.clone();
    files.sort();
    files
}

/// A whole unit restores from a short tape as from any other, every file
/// back as it was.
#[test]
fn a_whole_unit_restores_from_a_short_tape() {
    let mut t = short_tape::short_tape();
    let dest = TempDir::new().unwrap();
    let dest_str = dest.path().to_string_lossy().to_string();
    let report = restore_unit_from_store(
        &t.conn,
        &t.paths,
        &Config::default(),
        "alpha",
        LABEL,
        1,
        RestoreTarget::Unit {
            dest_dir: &dest_str,
        },
        &RestoreOptions::default(),
        &mut t.store,
        site(Operation::RestoreUnit),
    )
    .expect("a whole unit restores");
    assert_eq!(report.slices, 2);
    assert_eq!(files_under(dest.path()), expected(&t, "alpha"));
    assert!(
        dest.path().join(DOTFILE).is_file(),
        "positive control: the unit's own dotfile is in the stream, so the filter above hid one"
    );
}

/// Item 288: the unit the early warning interrupted is a range on the tape
/// (its first slice is there). A restore of it takes the files the tape holds
/// whole and lists the rest, naming the slice no live volume holds and where
/// the catalog saw it (D17, item 266; #572): it is not refused before the
/// tape. A unit planned after the warning has no row, so the tape names no
/// data of it: refused by name before any slice is read, with the tape
/// untouched.
#[test]
fn the_unit_the_tape_ended_in_is_a_range_and_one_after_it_has_no_data_there() {
    let mut t = short_tape::short_tape();
    // bravo: a range of one slice, and slice 2 of it is on no live volume. It is
    // selected, with the one slice the tape holds.
    let selection = select_write_positions(&t.conn, "bravo", LABEL, None).unwrap();
    assert_eq!(
        selection
            .positions
            .iter()
            .map(|wp| wp.slice_number)
            .collect::<Vec<_>>(),
        vec![1],
        "only the slice a live volume holds is planned"
    );

    assert_eq!(
        selection.unheld.keys().copied().collect::<Vec<_>>(),
        vec![2],
        "the slice on no live volume is listed, with its reason"
    );

    // Through the store seam: the files slice 1 holds whole are restored and
    // the rest are listed with where the catalog saw them.
    let dest = TempDir::new().unwrap();
    let dest_str = dest.path().to_string_lossy().to_string();
    let err = restore_unit_from_store(
        &t.conn,
        &t.paths,
        &Config::default(),
        "bravo",
        LABEL,
        1,
        RestoreTarget::Unit {
            dest_dir: &dest_str,
        },
        &RestoreOptions::default(),
        &mut t.store,
        site(Operation::RestoreUnit),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("INCOMPLETE"), "{err}");
    assert!(err.contains("slice 2 of it is on no live volume"), "{err}");
    let got = files_under(dest.path());
    let want = expected(&t, "bravo");
    assert!(
        !got.is_empty() && got.len() < want.len(),
        "some files, not all: {} of {}",
        got.len(),
        want.len()
    );
    for g in &got {
        assert!(want.contains(g), "{} is restored byte for byte", g.0);
    }

    // charlie (nothing of it on the tape) has no data there.
    let err = select_write_positions(&t.conn, "charlie", LABEL, None)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("no data for unit \"charlie\" on volume"),
        "{err}"
    );
    let dest = TempDir::new().unwrap();
    let dest_str = dest.path().to_string_lossy().to_string();
    let err = restore_unit_from_store(
        &t.conn,
        &t.paths,
        &Config::default(),
        "charlie",
        LABEL,
        1,
        RestoreTarget::Unit {
            dest_dir: &dest_str,
        },
        &RestoreOptions::default(),
        &mut t.store,
        site(Operation::RestoreUnit),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("no data for unit"), "{err}");
    assert!(fs::read_dir(dest.path()).unwrap().next().is_none());

    // The positive control: a whole unit selects.
    assert!(select_write_positions(&t.conn, "alpha", LABEL, None).is_ok());
}

/// #572 and #573: `restore volume` with no unit named is the disaster
/// recovery command. The default set holds the range unit `bravo`; it must
/// restore `alpha` whole and `bravo`'s whole files, and list what it could
/// not, per unit. And a unit that cannot be planned at all (here, `alpha`'s
/// directory is not empty) is listed as failed without refusing the others.
#[test]
fn restore_volume_with_no_unit_named_restores_what_it_can_and_lists_the_rest() {
    let mut t = short_tape::short_tape();
    let dest = TempDir::new().unwrap();
    // `alpha` cannot be planned: its destination is not empty.
    fs::create_dir_all(dest.path().join("alpha")).unwrap();
    fs::write(dest.path().join("alpha").join("stray"), b"x").unwrap();
    let report = restore_units_from_loaded_store(
        &t.conn,
        &t.paths,
        &Config::default(),
        &[],
        &dest.path().to_string_lossy(),
        &RestoreOptions::default(),
        false,
        &mut t.store,
        site(Operation::RestoreVolume),
    )
    .expect("the units it can restore are restored");
    let by_name = |n: &str| {
        report
            .units
            .iter()
            .find(|u| u.unit_name == n)
            .unwrap_or_else(|| panic!("{n} is in the report: {report:?}"))
    };
    let alpha = by_name("alpha");
    assert!(!alpha.attempted, "{alpha:?}");
    assert!(
        alpha.error.as_deref().unwrap().contains("it is not empty"),
        "{alpha:?}"
    );
    let bravo = by_name("bravo");
    assert!(bravo.attempted, "{bravo:?}");
    let why = bravo.error.as_deref().unwrap();
    assert!(why.contains("INCOMPLETE"), "{why}");
    assert!(why.contains("slice 2 of it is on no live volume"), "{why}");
    assert_eq!(report.failed(), 2, "{report:?}");
    let got = files_under(&dest.path().join("bravo"));
    assert!(!got.is_empty(), "bravo's whole files are restored");
    for g in &got {
        assert!(expected(&t, "bravo").contains(g));
    }

    // With the stray file gone, alpha is restored whole beside bravo.
    fs::remove_dir_all(dest.path().join("alpha")).unwrap();
    fs::remove_dir_all(dest.path().join("bravo")).unwrap();
    let report = restore_units_from_loaded_store(
        &t.conn,
        &t.paths,
        &Config::default(),
        &[],
        &dest.path().to_string_lossy(),
        &RestoreOptions::default(),
        false,
        &mut t.store,
        site(Operation::RestoreVolume),
    )
    .unwrap();
    assert_eq!(report.failed(), 1, "only bravo is short: {report:?}");
    assert_eq!(
        files_under(&dest.path().join("alpha")),
        expected(&t, "alpha")
    );
}

/// `restore volume` with no unit named takes every unit the tape holds a
/// written slice of, a range included (it is read across cartridges, item
/// 266), and restores the whole ones; a tape with nothing written says so.
#[test]
fn restore_volume_takes_every_unit_the_tape_has_a_written_slice_of() {
    let mut t = short_tape::short_tape();
    assert_eq!(
        units_on_volume(&t.conn, LABEL).unwrap(),
        vec!["alpha", "bravo"],
        "charlie has no row, so the tape names nothing of it"
    );

    // Naming alpha restores it whole, as from any tape.
    let dest = TempDir::new().unwrap();
    let report = restore_units_from_loaded_store(
        &t.conn,
        &t.paths,
        &Config::default(),
        &["alpha".to_string()],
        &dest.path().to_string_lossy(),
        &RestoreOptions::default(),
        false,
        &mut t.store,
        site(Operation::RestoreVolume),
    )
    .unwrap();
    assert_eq!(report.failed(), 0);
    assert_eq!(
        report
            .units
            .iter()
            .map(|u| u.unit_name.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha"]
    );
    assert_eq!(
        files_under(&dest.path().join("alpha")),
        expected(&t, "alpha")
    );
    assert!(dest.path().join("alpha").join(DOTFILE).is_file());
    assert!(!dest.path().join("bravo").exists());

    // A volume with no write left holds nothing to restore.
    t.conn.execute("DELETE FROM write_positions", []).unwrap();
    t.conn.execute("DELETE FROM writes", []).unwrap();
    let err = units_on_volume(&t.conn, LABEL).unwrap_err().to_string();
    assert!(err.contains("holds no unit with written slices"), "{err}");
}

/// The repair source (#480, §5.1 R5; ADR-0012 item 288): a range's `written`
/// slices are a source, the one at `ended_after` included (it is on the tape
/// and hashes right); a unit planned after the warning has no write, so
/// nothing of it is one; and a completed write's slice recorded past its own
/// tape's `ended_after` (a catalog `audit` names) is no source either.
#[test]
fn a_ranges_written_slices_are_a_repair_source_and_an_unwritten_units_are_none() {
    let t = short_tape::short_tape();
    let k = t.ended_after;
    let sha_at = |position: u32| -> String {
        t.conn
            .query_row(
                "SELECT sl.sha256_encrypted FROM write_positions wp
                   JOIN stage_slices sl ON sl.id = wp.stage_slice_id
                  WHERE CAST(wp.position AS INTEGER) = ?1",
                params![position],
                |r| r.get(0),
            )
            .unwrap()
    };
    // bravo's first slice is on the tape at K; its write is a range, and the
    // slice is `written` there.
    assert_eq!(t.positions_of("bravo")[0], k);
    let bravo_first = sha_at(k);
    let holders = crate::volume::slice_holders::holders_of(&t.conn, &bravo_first).unwrap();
    assert_eq!(holders.len(), 1);
    assert_eq!(holders[0].label, LABEL);
    // charlie has no write: none of its positions exist, and no slice of it is
    // held anywhere.
    let charlie_first: String = t
        .conn
        .query_row(
            "SELECT sl.sha256_encrypted FROM stage_slices sl
               JOIN stage_sets ss ON ss.id = sl.stage_set_id
               JOIN snapshots s ON s.id = ss.snapshot_id
               JOIN units u ON u.id = s.unit_id
              WHERE u.name = 'charlie' AND sl.slice_number = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        crate::volume::slice_holders::holders_of(&t.conn, &charlie_first)
            .unwrap()
            .is_empty()
    );
    // alpha's slices are a source on this tape (the positive control).
    let alpha_first = sha_at(t.positions_of("alpha")[0]);
    let holders = crate::volume::slice_holders::holders_of(&t.conn, &alpha_first).unwrap();
    assert_eq!(holders.len(), 1);
    assert_eq!(holders[0].label, LABEL);

    // A completed row whose position the catalog records past its
    // volume's end: not a source.
    t.conn
        .execute(
            "UPDATE volumes SET ended_after = ?1 WHERE label = ?2",
            params![t.positions_of("alpha")[0] - 1, LABEL],
        )
        .unwrap();
    assert!(
        crate::volume::slice_holders::holders_of(&t.conn, &alpha_first)
            .unwrap()
            .is_empty()
    );
}

/// restore-4: a unit that cannot be planned is known before the drive opens.
/// It heads the report, and with `--fail-fast` the pass stops before any
/// unit is read: the others are reported not attempted.
#[test]
fn a_unit_that_cannot_be_planned_heads_the_report_and_stops_a_fail_fast_pass() {
    let mut t = short_tape::short_tape();
    let dest = TempDir::new().unwrap();
    fs::create_dir_all(dest.path().join("alpha")).unwrap();
    fs::write(dest.path().join("alpha").join("stray"), b"x").unwrap();
    let run = |t: &mut ShortTape, fail_fast: bool| {
        restore_units_from_loaded_store(
            &t.conn,
            &t.paths,
            &Config::default(),
            &[],
            &dest.path().to_string_lossy(),
            &RestoreOptions::default(),
            fail_fast,
            &mut t.store,
            site(Operation::RestoreVolume),
        )
        .unwrap()
    };
    let names = |r: &super::UnitsReport| -> Vec<String> {
        r.units.iter().map(|u| u.unit_name.clone()).collect()
    };

    let report = run(&mut t, false);
    assert_eq!(names(&report), ["alpha", "bravo"], "the refused unit first");
    assert!(report.units[1].attempted, "{report:?}");

    // `--fail-fast`: nothing is read.
    let _ = fs::remove_dir_all(dest.path().join("bravo"));
    let report = run(&mut t, true);
    assert_eq!(names(&report), ["alpha", "bravo"]);
    assert!(report.units[0].error.is_some(), "{report:?}");
    assert!(!report.units[1].attempted, "bravo was not read: {report:?}");
    assert!(
        !dest.path().join("bravo").exists(),
        "nothing was extracted before the stop"
    );
    assert_eq!(report.failed(), 2);
}

/// C3: only a refusal of a unit's plan is listed. A catalog error (here a
/// table that is gone, as a failed read would be) is the command's own.
#[test]
fn a_catalog_error_is_not_listed_as_a_unit_that_cannot_be_planned() {
    let t = short_tape::short_tape();
    let dest = TempDir::new().unwrap();
    let names = units_on_volume(&t.conn, LABEL).unwrap();
    let requests = super::unit_requests(&names, &dest.path().to_string_lossy()).unwrap();
    t.conn
        .execute_batch("ALTER TABLE write_positions RENAME TO write_positions_gone")
        .unwrap();
    let err = super::plan_set(&t.conn, LABEL, &requests, &RestoreOptions::default(), true)
        .err()
        .expect("the catalog error stops the command");
    assert!(
        matches!(err.unwrapped(), crate::error::TapectlError::Database(_)),
        "{err}"
    );
}

/// A store that answers File 0 and the envelopes and then loses the drive:
/// every read at or after `from` fails as the kernel's ENODEV does.
struct Gone<'a> {
    inner: &'a mut crate::store::MemStore,
    from: u32,
}

impl crate::store::Store for Gone<'_> {
    fn capacity(&mut self) -> crate::error::Result<crate::store::CapacityReport> {
        self.inner.capacity()
    }
    fn execute(
        &mut self,
        src: &mut dyn std::io::Read,
        len: u64,
        sync: bool,
    ) -> crate::error::Result<u64> {
        self.inner.execute(src, len, sync)
    }
    fn read_file(
        &mut self,
        position: u32,
        sink: &mut dyn std::io::Write,
    ) -> crate::error::Result<u64> {
        if position >= self.from {
            return Err(crate::error::TapectlError::TapeIo(
                "read: No such device (os error 19)".into(),
            ));
        }
        self.inner.read_file(position, sink)
    }
    fn reposition_for_resume(&mut self, file_index: u32) -> crate::error::Result<()> {
        self.inner.reposition_for_resume(file_index)
    }
    fn blank_at_bot(&mut self) -> crate::error::Result<bool> {
        self.inner.blank_at_bot()
    }
    fn eod_at_or_before(&mut self, position: u32) -> crate::error::Result<bool> {
        self.inner.eod_at_or_before(position)
    }
    fn files_on_medium(&mut self) -> crate::error::Result<Option<u32>> {
        self.inner.files_on_medium()
    }
}

/// C1: a drive that stops answering is a stop, not a stream of bad slices:
/// the restore ends at the first slice it cannot read for that reason, with no
/// list of "damaged" files for a repair from another copy.
#[test]
fn a_drive_that_stops_answering_ends_the_restore() {
    let mut t = short_tape::short_tape();
    let first = t.positions_of("alpha")[0];
    let dest = TempDir::new().unwrap();
    let dest_str = dest.path().to_string_lossy().to_string();
    let mut gone = Gone {
        inner: &mut t.store,
        from: first,
    };
    let err = restore_unit_from_store(
        &t.conn,
        &t.paths,
        &Config::default(),
        "alpha",
        LABEL,
        1,
        RestoreTarget::Unit {
            dest_dir: &dest_str,
        },
        &RestoreOptions::default(),
        &mut gone,
        site(Operation::RestoreUnit),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("the drive is not answering"), "{err}");
    assert!(!err.contains("INCOMPLETE"), "{err}");
}
