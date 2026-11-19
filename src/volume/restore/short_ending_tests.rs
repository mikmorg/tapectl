//! #420: restore through tapectl from a tape that ended early
//! (`volume-format-v2.md` §4.2 R3-R5; `volume-format-v3.md` §5), on the shared
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
/// (its first slice is there), and a unit planned after the warning has no
/// row, so the tape names no data of it: refused by name before any slice is
/// read, with the tape untouched.
#[test]
fn the_unit_the_tape_ended_in_is_a_range_and_one_after_it_has_no_data_there() {
    let mut t = short_tape::short_tape();
    // bravo: a range of one slice, selected like any other write.
    let selection = select_write_positions(&t.conn, "bravo", LABEL, None).unwrap();
    assert_eq!(selection.positions.len(), 1, "its first slice, at K");
    assert_eq!(selection.positions[0].position, t.ended_after.to_string());

    // charlie (nothing of it on the tape) has no data there.
    let err = select_write_positions(&t.conn, "charlie", LABEL, None)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("no data for unit \"charlie\" on volume"),
        "{err}"
    );

    // Through the store seam: refused, and nothing restored.
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

/// The repair source (#480, §4.2 R5; ADR-0012 item 288): a range's `written`
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
