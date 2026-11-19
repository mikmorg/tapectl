//! #420: restore through tapectl from a tape that ended early
//! (`volume-format-v2.md` §4.2 R3-R5; `volume-format-v3.md` §5), on the shared
//! fixture (`short_tape`): `alpha` whole, `bravo` cut with its first slice on
//! the tape, `charlie` cut. The units are staged by the real `stage create`
//! and the tape is written by the real session; a restore reads the unit's
//! file table from the tape's own catalog and feeds tar whole members.

use std::fs;
use std::path::Path;

use rusqlite::{params, Connection};
use tempfile::TempDir;

use super::short_tape::{self, ShortTape, LABEL};
use super::{
    cut_units_on_volume, restore_unit_from_store, restore_units_from_loaded_store,
    select_write_positions, units_on_volume, RestoreOptions, RestoreTarget,
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

/// A second volume in service holding `unit`'s version 1 whole: the
/// place a refusal sends the operator.
fn whole_elsewhere(conn: &Connection, unit: &str, label: &str) {
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                              capacity_bytes, status, sealed_at)
         VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed', datetime('now'))",
        params![label],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
         SELECT ss.id, s.id, (SELECT id FROM volumes WHERE label = ?2), 'completed'
           FROM stage_sets ss JOIN snapshots s ON s.id = ss.snapshot_id
           JOIN units u ON u.id = s.unit_id WHERE u.name = ?1",
        params![unit, label],
    )
    .unwrap();
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

/// R4: a cut unit is refused by name before any slice is read, says
/// where the tape ended and where its version is whole — from the
/// catalog: a volume in service with a completed write of the same
/// version (`coverage::in_service_copy_of_version_excluding`), or,
/// with none, that it is still to be written.
#[test]
fn a_cut_unit_is_refused_by_name_and_points_at_the_volume_that_holds_it_whole() {
    let mut t = short_tape::short_tape();
    let k = t.ended_after;
    let last = t.positions_of("bravo")[1];
    let err = select_write_positions(&t.conn, "bravo", LABEL, None)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(&format!(
            "unit \"bravo\" version 1 is not on volume \"{LABEL}\" whole"
        )),
        "{err}"
    );
    assert!(err.contains(&format!("after tape file {k}")), "{err}");
    assert!(err.contains(&format!("run to tape file {last}")), "{err}");
    assert!(
        err.contains("no volume this catalog knows holds it whole yet"),
        "{err}"
    );
    assert!(err.contains("Nothing was read from tape"), "{err}");
    // --version names it too, and charlie (nothing of it on the
    // tape) is refused the same way.
    let err = select_write_positions(&t.conn, "bravo", LABEL, Some(1))
        .unwrap_err()
        .to_string();
    assert!(err.contains("is not on volume"), "{err}");
    let err = select_write_positions(&t.conn, "charlie", LABEL, None)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("unit \"charlie\" version 1 is not on volume"),
        "{err}"
    );

    whole_elsewhere(&t.conn, "bravo", "WHOLE02");
    let err = select_write_positions(&t.conn, "bravo", LABEL, None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("volume \"WHOLE02\" holds it whole"), "{err}");
    assert!(
        err.contains("tapectl restore unit --unit bravo --from WHOLE02 --version 1"),
        "{err}"
    );

    // Through the store seam: refused, and nothing restored.
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
    assert!(err.contains("is not on volume"), "{err}");
    assert!(fs::read_dir(dest.path()).unwrap().next().is_none());

    // The positive control: the version restores from where it IS
    // whole once the catalog knows nothing cut of it here.
    assert!(select_write_positions(&t.conn, "alpha", LABEL, None).is_ok());
}

/// `restore volume` with no unit named restores every unit the tape
/// holds whole and names the cut ones; a tape that ended inside its
/// first unit holds none whole and says so.
#[test]
fn restore_volume_restores_the_whole_units_and_names_the_cut_ones() {
    let mut t = short_tape::short_tape();
    assert_eq!(units_on_volume(&t.conn, LABEL).unwrap(), vec!["alpha"]);
    let cut = cut_units_on_volume(&t.conn, LABEL).unwrap();
    assert_eq!(
        cut.iter()
            .map(|c| (c.unit_name.as_str(), c.version, c.whole_on.clone()))
            .collect::<Vec<_>>(),
        vec![("bravo", 1, None), ("charlie", 1, None)]
    );
    let lines = crate::cli::restore::cut_units_lines(&cut, LABEL).join("\n");
    assert!(
        lines.contains("not on this tape whole: \"bravo\" v1"),
        "{lines}"
    );
    assert!(lines.contains("2 units left out"), "{lines}");

    let dest = TempDir::new().unwrap();
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

    // Every unit cut: the volume holds none whole, and says which.
    t.conn
        .execute("UPDATE writes SET status = 'cut'", [])
        .unwrap();
    let err = units_on_volume(&t.conn, LABEL).unwrap_err().to_string();
    assert!(err.contains("holds no unit whole"), "{err}");
    assert!(err.contains("\"alpha\" v1"), "{err}");
}

/// The repair source (#480, §4.2 R5): a cut write's slices are never
/// one, even its slice at `ended_after` that is on the tape and hashes
/// right; and a completed write's slice recorded past its own tape's
/// `ended_after` (a catalog `audit` names) is no source either.
#[test]
fn a_cut_writes_slices_are_never_a_repair_source() {
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
    // bravo's first slice is on the tape at K; its write is cut.
    assert_eq!(t.positions_of("bravo")[0], k);
    let bravo_first = sha_at(k);
    assert!(
        crate::volume::slice_holders::holders_of(&t.conn, &bravo_first)
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
