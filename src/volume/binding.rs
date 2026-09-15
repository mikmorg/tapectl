//! Binding a volume to the physical cartridge it is being written to
//! (ADR-0010, "Init binds the cartridge").
//!
//! The `cartridge_volumes` join and the whole `cartridges` lifecycle existed
//! only in tests until now: no production path ever wrote the join, because
//! nothing knew WHICH cartridge it was writing. The MAM medium serial is
//! that knowledge, and `volume init` is where it is read.
//!
//! **This module adds no consent gate, deliberately.** The tempting rule —
//! refuse when the cartridge is still bound to a live volume, and make
//! `--force` or the retire lifecycle the way past — was considered and
//! rejected by ADR-0010 ("Binding adds no second consent gate"). `volume
//! init` has already asked the tape itself: a File 0 naming a sealed volume
//! is refused unless `--force` (ADR-0003, issue #27), and that decision is
//! made against the medium's own evidence rather than the catalog's weaker
//! claim about it. An operator who reached binding either loaded a blank or
//! erased tape — the data is already physically gone — or gave `--force`,
//! which is the consent. Demanding a second override for the same act is the
//! ceremony ADR-0008 warns about, and it would turn `mt erase` + `volume
//! init` — the most ordinary reuse there is — into a two-command catalog
//! dance that teaches operators to reach for `--force` by reflex.
//!
//! So binding RECORDS the displacement instead of relitigating it: the open
//! mount is closed, the displaced volume moves to `erased`, an events row
//! says why, and the caller warns — naming the volume and any unit that just
//! lost its last copy. Nothing is blocked (ADR-0004); the catalog simply
//! stops crediting a copy that no longer physically exists, which is the
//! over-crediting the lifecycle suite's own comment predicted.
//!
//! The one refusal binding DOES introduce lives in
//! [`crate::tape::media_detect::resolve_media`], not here: a registered row
//! whose `media_type` disagrees with the DETECTED generation is a fact
//! error, because either the row is wrong or the wrong tape is loaded and
//! tapectl cannot tell which.

use rusqlite::{params, Connection, OptionalExtension};

use crate::cli::operations::{retire_impacts, RetireImpact};
use crate::db::events;
use crate::error::{Result, TapectlError};
use crate::media::Generation;
use crate::tape::mam::MamInfo;

/// The `cartridges` columns `volume init` needs, for the row it matched.
#[derive(Debug, Clone)]
pub(crate) struct CartridgeRow {
    pub id: i64,
    pub barcode: String,
    pub media_type: String,
    pub nominal_capacity: i64,
    pub status: String,
    pub serial_number: Option<String>,
}

/// The outcome of [`lookup_cartridge`]: which row (if any) this write will
/// bind to, and whether an explicit `--cartridge` was overtaken by a
/// serial match.
#[derive(Debug, Clone, Default)]
pub(crate) struct CartridgeLookup {
    pub row: Option<CartridgeRow>,
    /// `--cartridge B` was given, but the loaded medium's MAM serial matched
    /// a DIFFERENT registered cartridge, which wins — the serial is read off
    /// the medium, the barcode is typed by a human. Carries `B` so the
    /// caller can say the request was superseded.
    pub superseded_request: Option<String>,
}

/// One volume displaced from a cartridge by this binding.
pub(crate) struct Displaced {
    pub label: String,
    /// Units with a completed write on the displaced volume, and how many
    /// ADR-0004-eligible copies each still has ELSEWHERE. Computed with
    /// `cli::operations::retire_impacts` — the one coverage derivation
    /// `volume retire`, `unit mark-tape-only` and `compact-finish` already
    /// share, never re-derived here.
    pub impacts: Vec<RetireImpact>,
}

/// What [`bind_cartridge`] did, in enough detail for the caller to say it.
#[derive(Default)]
pub(crate) struct BindOutcome {
    /// `None` when no medium serial was readable — the volume is written
    /// unbound, which is what keeps serial-less virtual harnesses working.
    pub cartridge_id: Option<i64>,
    pub barcode: Option<String>,
    /// The cartridge row did not exist and was created from MAM, with its
    /// barcode set to the medium serial.
    pub auto_registered: bool,
    /// The medium serial was recorded onto an existing row that lacked one.
    pub serial_recorded: bool,
    pub displaced: Vec<Displaced>,
}

/// Find the cartridge this write is for, WITHOUT mutating anything.
///
/// Run before the tape is opened, so `--cartridge` naming a cartridge that
/// was never registered fails free — before File 0 is read and long before a
/// `volumes` row exists.
///
/// The MAM serial wins over `--cartridge`: it was read off the medium, while
/// the barcode was typed by a human who may have picked up the wrong tape.
/// ADR-0010 names the row-vs-medium *generation* disagreement as the only new
/// refusal binding introduces, so a superseded `--cartridge` is reported, not
/// refused.
pub(crate) fn lookup_cartridge(
    conn: &Connection,
    serial: Option<&str>,
    requested_barcode: Option<&str>,
) -> Result<CartridgeLookup> {
    let by_serial = match serial {
        Some(s) => select_cartridge(conn, "serial_number", s)?,
        None => None,
    };
    if let Some(row) = by_serial {
        let superseded = requested_barcode
            .filter(|b| *b != row.barcode)
            .map(str::to_string);
        return Ok(CartridgeLookup {
            row: Some(row),
            superseded_request: superseded,
        });
    }

    if let Some(barcode) = requested_barcode {
        let row = select_cartridge(conn, "barcode", barcode)?.ok_or_else(|| {
            TapectlError::Other(format!(
                "cartridge \"{barcode}\" is not registered. Register it first \
                 (`tapectl cartridge register --barcode {barcode} --media-type <GEN>`), \
                 or omit --cartridge and let `volume init` auto-register this medium \
                 from its MAM serial."
            ))
        })?;
        // Reaching here means the loaded medium's serial matched no row. If
        // this one already carries a DIFFERENT serial it is a different
        // physical cartridge, and binding anyway would strand the volume:
        // the first `volume write` re-reads the serial and refuses. Silently
        // re-pointing the row at this medium instead would be worse — it
        // would rewrite one cartridge's identity and history as another's.
        if let (Some(recorded), Some(loaded)) = (&row.serial_number, serial) {
            if recorded != loaded {
                return Err(TapectlError::Other(format!(
                    "cartridge \"{barcode}\" is registered with medium serial {recorded}, \
                     but the drive holds {loaded}. That is a different physical cartridge. \
                     Load {barcode}, or omit --cartridge and let this medium register \
                     itself."
                )));
            }
        }
        return Ok(CartridgeLookup {
            row: Some(row),
            superseded_request: None,
        });
    }

    Ok(CartridgeLookup::default())
}

/// Refuse to bind a cartridge the operator has declared unfit (ADR-0011).
///
/// This is the ONE status-based refusal binding has, and it is deliberately
/// not the second consent gate ADR-0010 rejected. The difference is the one
/// ADR-0008 draws between risk and incoherence: `volume init` past File 0
/// means the operator either loaded a blank tape or gave `--force`, and
/// ADR-0010 lets that consent stand for the displacement. But no amount of
/// consent makes a medium you have declared permanently unfit fit again —
/// that is a fact about the plastic, not a risk to be accepted.
///
/// So this takes no `force` parameter AT ALL, structurally like
/// `session.rs`'s `check_tape_contact`/`AlreadySealed`: a caller cannot
/// defeat it even by mistake. The escape is `cartridge mark-erased`, the
/// operator saying they were wrong — which is a different statement from
/// "proceed anyway".
///
/// Every other status still binds silently: `in_use`, `pending_erase` and
/// `available` are all ordinary reuse, and refusing them would be exactly
/// the two-command catalog dance ADR-0010 refused to create.
pub(crate) fn refuse_retired(row: &CartridgeRow) -> Result<()> {
    if row.status == "retired_permanent" {
        return Err(TapectlError::Other(format!(
            "cartridge \"{}\" is retired_permanent and must never be written again \
             (ADR-0011). This is a fact about the medium, not a risk judgement — \
             --force does not override it. If the cartridge is in fact usable, say so \
             with `tapectl cartridge mark-erased {}`, which is the only way back.",
            row.barcode, row.barcode
        )));
    }
    Ok(())
}

fn select_cartridge(conn: &Connection, column: &str, value: &str) -> Result<Option<CartridgeRow>> {
    // `column` is never operator input: both call sites pass a literal.
    let sql = format!(
        "SELECT id, barcode, media_type, nominal_capacity, status, serial_number
         FROM cartridges WHERE {column} = ?1"
    );
    // `.optional()?`, never `.ok()`: a locked database or a malformed query
    // must surface as an error, not silently read as "not registered" — which
    // would auto-register a duplicate cartridge for a tape that already has
    // one.
    let row = conn
        .query_row(&sql, params![value], |r| {
            Ok(CartridgeRow {
                id: r.get(0)?,
                barcode: r.get(1)?,
                media_type: r.get(2)?,
                nominal_capacity: r.get(3)?,
                status: r.get(4)?,
                serial_number: r.get(5)?,
            })
        })
        .optional()?;
    Ok(row)
}

/// Bind `volume_id` to the cartridge in the drive, recording (never
/// refusing) whatever it displaces.
///
/// Call inside the same transaction as the `volumes` INSERT: a volume that
/// exists but is not bound, or a displacement recorded for a volume that was
/// never created, are both worse than either change alone.
///
/// - `row` — the [`lookup_cartridge`] match, if there was one.
/// - `serial` — the MAM medium serial, if readable. With no row AND no
///   serial there is nothing to bind to and the volume is written unbound.
/// - `generation`/`capacity_bytes` — already resolved by the caller; used
///   only when auto-registering.
pub(crate) fn bind_cartridge(
    conn: &Connection,
    volume_id: i64,
    row: Option<&CartridgeRow>,
    serial: Option<&str>,
    generation: Generation,
    capacity_bytes: i64,
    mam: &MamInfo,
) -> Result<BindOutcome> {
    let mut outcome = BindOutcome::default();

    let (cartridge_id, barcode, prior_status) = match row {
        Some(r) => {
            // Record the serial on a row that was pre-registered by hand and
            // has now been seen in a drive for the first time.
            if r.serial_number.is_none() {
                if let Some(s) = serial {
                    conn.execute(
                        "UPDATE cartridges SET serial_number = ?1 WHERE id = ?2",
                        params![s, r.id],
                    )?;
                    events::log_field_change(
                        conn,
                        "cartridge",
                        r.id,
                        &r.barcode,
                        "updated",
                        "serial_number",
                        None,
                        s,
                        None,
                    )?;
                    outcome.serial_recorded = true;
                }
            }
            (r.id, r.barcode.clone(), r.status.clone())
        }
        None => match serial {
            // Auto-register: the barcode IS the medium serial, because that
            // is the only identifier the tape itself carries. An operator who
            // wants a human barcode registers the cartridge first and passes
            // --cartridge.
            Some(s) => {
                // The MAM serial matched no row by `serial_number` (else
                // `lookup_cartridge` would have returned it here already),
                // but `barcode` is `TEXT NOT NULL UNIQUE` and this INSERT is
                // about to bind `s` to BOTH columns. If a row already
                // carries `s` as its BARCODE -- exactly what an operator
                // gets by hand-registering a cartridge using the serial
                // printed on its shell, before it was ever loaded -- the
                // INSERT below collides. Refuse by name instead of
                // surfacing the raw UNIQUE constraint failure: silently
                // adopting that row would rewrite its identity on a guess,
                // the same mistake `lookup_cartridge` already refuses for
                // the sibling case above (a `--cartridge` naming a row with
                // a DIFFERENT recorded serial). This takes no `force` --
                // it is a fact tapectl cannot resolve on its own, not a
                // risk to accept.
                if select_cartridge(conn, "barcode", s)?.is_some() {
                    return Err(TapectlError::Other(format!(
                        "this medium reports serial {s}, which matches no registered \
                         cartridge — but a cartridge is already registered under the \
                         barcode \"{s}\". tapectl cannot tell whether that is this same \
                         physical cartridge, registered by hand before it was ever \
                         loaded, or a different one whose sticker happens to read the \
                         same. It will not guess.\n\
                         \n\
                         If it IS this cartridge, name it so the serial is recorded \
                         onto the existing row:\n    \
                         tapectl volume init <label> --device <dev> --cartridge {s}\n\
                         (`volume write` has no --cartridge, so do this at init.)\n\
                         \n\
                         If it is a DIFFERENT cartridge, give the registered one a \
                         barcode of its own and retry:\n    \
                         tapectl cartridge relabel {s} <new-barcode>"
                    )));
                }
                conn.execute(
                    "INSERT INTO cartridges
                        (barcode, media_type, manufacturer, serial_number,
                         tape_length_meters, nominal_capacity, status)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'in_use')",
                    params![
                        s,
                        generation.as_str(),
                        mam.manufacturer.as_deref(),
                        s,
                        mam.length_meters,
                        capacity_bytes,
                    ],
                )?;
                let id = conn.last_insert_rowid();
                events::log_created(conn, "cartridge", id, s, None)?;
                outcome.auto_registered = true;
                (id, s.to_string(), "in_use".to_string())
            }
            // No row, no serial: nothing to bind to. mhvtl drives that
            // expose no medium serial land here, and lose nothing but the
            // binding itself.
            None => return Ok(outcome),
        },
    };

    outcome.cartridge_id = Some(cartridge_id);
    outcome.barcode = Some(barcode.clone());

    // --- record the displacement (ADR-0010: never refuse it) -------------
    let mut stmt = conn.prepare(
        "SELECT v.id, v.label, v.status FROM cartridge_volumes cv
         JOIN volumes v ON v.id = cv.volume_id
         WHERE cv.cartridge_id = ?1 AND cv.unmounted_at IS NULL AND cv.volume_id != ?2",
    )?;
    let displaced: Vec<(i64, String, String)> = stmt
        .query_map(params![cartridge_id, volume_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);

    for (vol_id, label, prior) in &displaced {
        // Computed BEFORE the status change, so the evidence describes the
        // world the operator is being warned about.
        let impacts = retire_impacts(conn, *vol_id)?;
        conn.execute(
            "UPDATE volumes SET status = 'erased' WHERE id = ?1",
            params![vol_id],
        )?;
        events::log_field_change(
            conn,
            "volume",
            *vol_id,
            label,
            "erased",
            "status",
            Some(prior),
            "erased",
            None,
        )?;
        events::log_event(
            conn,
            "cartridge",
            cartridge_id,
            Some(&barcode),
            "displaced",
            None,
            Some(label),
            None,
            Some(&format!(
                "volume \"{label}\" was displaced by `volume init` writing a new volume \
                 to this cartridge (ADR-0010); its bytes are gone from the medium"
            )),
            None,
        )?;
        outcome.displaced.push(Displaced {
            label: label.clone(),
            impacts,
        });
    }

    conn.execute(
        "UPDATE cartridge_volumes SET unmounted_at = datetime('now')
         WHERE cartridge_id = ?1 AND unmounted_at IS NULL AND volume_id != ?2",
        params![cartridge_id, volume_id],
    )?;

    // --- bind ------------------------------------------------------------
    conn.execute(
        "INSERT OR IGNORE INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
        params![cartridge_id, volume_id],
    )?;

    // ADR-0011 promises that a cartridge's place and its volumes' places
    // "cannot disagree", and reasons about `cartridge move` and `volume move`
    // as the only writers of the pair. Binding is a THIRD writer: it attaches
    // a volume to a cartridge that may already sit somewhere, and a volume
    // written to a cartridge on the offsite shelf would otherwise be recorded
    // nowhere at all -- `catalog locate` would have no answer for the very
    // tape whose place the catalog knows. Inherit it, the same way
    // `move_together` records one, so the guarantee holds for every writer
    // rather than for two of the three.
    let cart_loc: Option<i64> = conn.query_row(
        "SELECT location_id FROM cartridges WHERE id = ?1",
        params![cartridge_id],
        |row| row.get(0),
    )?;
    if let Some(loc_id) = cart_loc {
        let vol_loc: Option<i64> = conn.query_row(
            "SELECT location_id FROM volumes WHERE id = ?1",
            params![volume_id],
            |row| row.get(0),
        )?;
        if vol_loc.is_none() {
            conn.execute(
                "INSERT INTO volume_movements (volume_id, from_location, to_location)
                 VALUES (?1, NULL, ?2)",
                params![volume_id, loc_id],
            )?;
            conn.execute(
                "UPDATE volumes SET location_id = ?1 WHERE id = ?2",
                params![loc_id, volume_id],
            )?;
        }
    }

    // `in_use` unconditionally, from ANY prior status. A precondition here
    // would be a consent gate in disguise: `pending_erase -> mt erase ->
    // volume init` is exactly the ordinary reuse ADR-0010 protects.
    if prior_status != "in_use" {
        conn.execute(
            "UPDATE cartridges SET status = 'in_use', last_use = datetime('now') WHERE id = ?1",
            params![cartridge_id],
        )?;
        events::log_field_change(
            conn,
            "cartridge",
            cartridge_id,
            &barcode,
            "updated",
            "status",
            Some(&prior_status),
            "in_use",
            None,
        )?;
    } else {
        conn.execute(
            "UPDATE cartridges SET last_use = datetime('now') WHERE id = ?1",
            params![cartridge_id],
        )?;
    }

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn register(conn: &Connection, barcode: &str, gen: &str, serial: Option<&str>, status: &str) {
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number, status)
             VALUES (?1, ?2, 2500000000000, ?3, ?4)",
            params![barcode, gen, serial, status],
        )
        .unwrap();
    }

    fn new_volume(conn: &Connection, label: &str) -> i64 {
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
            params![label],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn volume_status(conn: &Connection, id: i64) -> String {
        conn.query_row(
            "SELECT status FROM volumes WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn cartridge_status(conn: &Connection, id: i64) -> String {
        conn.query_row(
            "SELECT status FROM cartridges WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn open_mounts(conn: &Connection, cartridge_id: i64) -> Vec<i64> {
        let mut stmt = conn
            .prepare(
                "SELECT volume_id FROM cartridge_volumes
                 WHERE cartridge_id = ?1 AND unmounted_at IS NULL ORDER BY volume_id",
            )
            .unwrap();
        stmt.query_map(params![cartridge_id], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    }

    // ---- lookup_cartridge ------------------------------------------------

    #[test]
    fn a_serial_match_finds_the_registered_cartridge() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC001", "LTO-6", Some("SER-1"), "available");
        let found = lookup_cartridge(&conn, Some("SER-1"), None).unwrap();
        assert_eq!(found.row.unwrap().barcode, "BC001");
    }

    // ---- ADR-0011: the one status-based refusal binding has -------------

    /// A medium the operator declared permanently unfit cannot be bound.
    /// The message must name the way back, because there is exactly one.
    #[test]
    fn a_retired_permanent_cartridge_is_refused() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC001", "LTO-6", Some("SER-1"), "retired_permanent");
        let found = lookup_cartridge(&conn, Some("SER-1"), None).unwrap();
        let err = refuse_retired(found.row.as_ref().unwrap())
            .expect_err("a retired_permanent cartridge must never be written again");
        let msg = err.to_string();
        assert!(msg.contains("retired_permanent"), "got: {msg}");
        assert!(
            msg.contains("mark-erased"),
            "the refusal must name the only way back; got: {msg}"
        );
    }

    /// The refusal takes no `force` parameter AT ALL — it is structurally
    /// un-overridable, like `session.rs`'s `AlreadySealed`. This test
    /// exists so that adding one later fails to compile here first.
    #[test]
    fn every_other_status_still_binds_silently() {
        // ADR-0010: binding relitigates nothing. `in_use` (a live volume),
        // `pending_erase` (awaiting a bulk erase) and `available` are all
        // ordinary reuse, and File 0 already made the decision.
        for status in ["available", "in_use", "pending_erase"] {
            let conn = db::open_memory().unwrap();
            register(&conn, "BC001", "LTO-6", Some("SER-1"), status);
            let found = lookup_cartridge(&conn, Some("SER-1"), None).unwrap();
            refuse_retired(found.row.as_ref().unwrap())
                .unwrap_or_else(|e| panic!("status {status:?} must bind silently: {e}"));
        }
    }

    #[test]
    fn an_unknown_serial_with_no_cartridge_flag_matches_nothing() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC001", "LTO-6", Some("SER-1"), "available");
        let found = lookup_cartridge(&conn, Some("SER-OTHER"), None).unwrap();
        assert!(found.row.is_none());
        assert!(found.superseded_request.is_none());
    }

    #[test]
    fn cartridge_flag_naming_an_unregistered_barcode_is_an_error() {
        let conn = db::open_memory().unwrap();
        let err = lookup_cartridge(&conn, None, Some("NOPE"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("\"NOPE\" is not registered"), "{err}");
    }

    /// The serial was read off the medium; the barcode was typed by a human.
    /// ADR-0010 makes only the row-vs-medium GENERATION disagreement a
    /// refusal, so this is reported, not refused.
    #[test]
    fn a_serial_match_supersedes_an_explicit_cartridge_flag_without_erroring() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC001", "LTO-6", Some("SER-1"), "available");
        register(&conn, "BC002", "LTO-6", None, "available");
        let found = lookup_cartridge(&conn, Some("SER-1"), Some("BC002")).unwrap();
        assert_eq!(found.row.unwrap().barcode, "BC001");
        assert_eq!(found.superseded_request.as_deref(), Some("BC002"));
    }

    /// `--cartridge B` where B already carries a DIFFERENT medium serial is
    /// a different physical cartridge. Binding anyway would strand the
    /// volume — the first `volume write` re-reads the serial and refuses —
    /// and re-pointing B's row at this medium would rewrite one cartridge's
    /// identity as another's.
    #[test]
    fn cartridge_flag_naming_a_row_with_a_different_serial_is_an_error() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC001", "LTO-6", Some("SER-1"), "available");
        let err = lookup_cartridge(&conn, Some("SER-2"), Some("BC001"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("registered with medium serial SER-1"), "{err}");
        assert!(err.contains("the drive holds SER-2"), "{err}");
    }

    /// ...but re-naming the SAME cartridge is fine, and so is naming one
    /// that has never been in a drive.
    #[test]
    fn cartridge_flag_naming_a_matching_or_serial_less_row_is_accepted() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC001", "LTO-6", Some("SER-1"), "available");
        register(&conn, "BC002", "LTO-6", None, "available");
        assert_eq!(
            lookup_cartridge(&conn, Some("SER-1"), Some("BC001"))
                .unwrap()
                .row
                .unwrap()
                .barcode,
            "BC001"
        );
        assert_eq!(
            lookup_cartridge(&conn, Some("SER-9"), Some("BC002"))
                .unwrap()
                .row
                .unwrap()
                .barcode,
            "BC002"
        );
    }

    // ---- bind_cartridge --------------------------------------------------

    #[test]
    fn binding_an_available_cartridge_marks_it_in_use_and_opens_a_mount() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC001", "LTO-6", Some("SER-1"), "available");
        let found = lookup_cartridge(&conn, Some("SER-1"), None).unwrap();
        let cart_id = found.row.as_ref().unwrap().id;
        let vol = new_volume(&conn, "L6-0001");

        let out = bind_cartridge(
            &conn,
            vol,
            found.row.as_ref(),
            Some("SER-1"),
            Generation::Lto6,
            2_500_000_000_000,
            &MamInfo::default(),
        )
        .unwrap();

        assert_eq!(out.cartridge_id, Some(cart_id));
        assert_eq!(out.barcode.as_deref(), Some("BC001"));
        assert!(!out.auto_registered);
        assert!(out.displaced.is_empty());
        assert_eq!(cartridge_status(&conn, cart_id), "in_use");
        assert_eq!(open_mounts(&conn, cart_id), vec![vol]);
    }

    /// ADR-0011 promises a cartridge's place and its volumes' places "cannot
    /// disagree", reasoning about `cartridge move` and `volume move` as the
    /// only writers of the pair. Binding is the third writer, and it was
    /// silently not one: a volume written to a cartridge already on the
    /// offsite shelf was recorded nowhere, so `catalog locate` had no answer
    /// for the one tape whose place the catalog knew.
    #[test]
    fn a_bound_volume_inherits_its_cartridge_location() {
        let conn = db::open_memory().unwrap();
        conn.execute("INSERT INTO locations (name) VALUES ('bank')", [])
            .unwrap();
        let loc: i64 = conn
            .query_row("SELECT id FROM locations WHERE name = 'bank'", [], |r| {
                r.get(0)
            })
            .unwrap();
        register(&conn, "BC001", "LTO-6", Some("SER-1"), "available");
        let found = lookup_cartridge(&conn, Some("SER-1"), None).unwrap();
        let cart_id = found.row.as_ref().unwrap().id;
        conn.execute(
            "UPDATE cartridges SET location_id = ?1 WHERE id = ?2",
            rusqlite::params![loc, cart_id],
        )
        .unwrap();
        let vol = new_volume(&conn, "L6-0001");

        bind_cartridge(
            &conn,
            vol,
            found.row.as_ref(),
            Some("SER-1"),
            Generation::Lto6,
            2_500_000_000_000,
            &MamInfo::default(),
        )
        .unwrap();

        let vol_loc: Option<i64> = conn
            .query_row(
                "SELECT location_id FROM volumes WHERE id = ?1",
                rusqlite::params![vol],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            vol_loc,
            Some(loc),
            "a volume bound to a located cartridge must inherit its location"
        );
        let movements: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM volume_movements WHERE volume_id = ?1 AND to_location = ?2",
                rusqlite::params![vol, loc],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            movements, 1,
            "the inherited place must leave a movement row"
        );
    }

    /// A cartridge with no location leaves the volume's location alone —
    /// inheriting NULL over NULL must not manufacture a movement row.
    #[test]
    fn binding_an_unlocated_cartridge_records_no_movement() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC001", "LTO-6", Some("SER-1"), "available");
        let found = lookup_cartridge(&conn, Some("SER-1"), None).unwrap();
        let vol = new_volume(&conn, "L6-0001");
        bind_cartridge(
            &conn,
            vol,
            found.row.as_ref(),
            Some("SER-1"),
            Generation::Lto6,
            2_500_000_000_000,
            &MamInfo::default(),
        )
        .unwrap();
        let movements: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM volume_movements WHERE volume_id = ?1",
                rusqlite::params![vol],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(movements, 0);
    }

    /// ADR-0010: no medium serial (some virtual drives expose none) means no
    /// binding, and that must be a clean unbound write rather than an error
    /// — the virtual harnesses lose nothing but the binding.
    #[test]
    fn no_serial_and_no_row_leaves_the_volume_unbound_without_erroring() {
        let conn = db::open_memory().unwrap();
        let vol = new_volume(&conn, "L6-0001");
        let out = bind_cartridge(
            &conn,
            vol,
            None,
            None,
            Generation::Lto6,
            2_500_000_000_000,
            &MamInfo::default(),
        )
        .unwrap();
        assert!(out.cartridge_id.is_none());
        assert!(out.barcode.is_none());
        assert!(!out.auto_registered);
    }

    #[test]
    fn an_unmatched_serial_auto_registers_a_cartridge_whose_barcode_is_that_serial() {
        let conn = db::open_memory().unwrap();
        let vol = new_volume(&conn, "L6-0001");
        let mam = MamInfo {
            manufacturer: Some("HP".into()),
            length_meters: Some(846),
            ..MamInfo::default()
        };
        let out = bind_cartridge(
            &conn,
            vol,
            None,
            Some("E01001L8_1775794348"),
            Generation::Lto8,
            12_000_000_000_000,
            &mam,
        )
        .unwrap();

        assert!(out.auto_registered);
        assert_eq!(out.barcode.as_deref(), Some("E01001L8_1775794348"));
        let (barcode, media_type, cap, manuf, len, serial, status): (
            String,
            String,
            i64,
            Option<String>,
            Option<i64>,
            Option<String>,
            String,
        ) = conn
            .query_row(
                "SELECT barcode, media_type, nominal_capacity, manufacturer,
                        tape_length_meters, serial_number, status
                 FROM cartridges WHERE id = ?1",
                params![out.cartridge_id.unwrap()],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(barcode, "E01001L8_1775794348");
        assert_eq!(serial.as_deref(), Some("E01001L8_1775794348"));
        assert_eq!(media_type, "LTO-8");
        assert_eq!(cap, 12_000_000_000_000);
        assert_eq!(manuf.as_deref(), Some("HP"));
        assert_eq!(len, Some(846));
        assert_eq!(status, "in_use");
    }

    #[test]
    fn a_hand_registered_cartridge_learns_its_serial_on_first_bind() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC001", "LTO-6", None, "available");
        let found = lookup_cartridge(&conn, Some("SER-1"), Some("BC001")).unwrap();
        let vol = new_volume(&conn, "L6-0001");
        let out = bind_cartridge(
            &conn,
            vol,
            found.row.as_ref(),
            Some("SER-1"),
            Generation::Lto6,
            2_500_000_000_000,
            &MamInfo::default(),
        )
        .unwrap();
        assert!(out.serial_recorded);
        let serial: Option<String> = conn
            .query_row(
                "SELECT serial_number FROM cartridges WHERE barcode = 'BC001'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(serial.as_deref(), Some("SER-1"));
    }

    /// The rule ADR-0010's "Binding adds no second consent gate" paragraph
    /// exists for, and the shape `scripts/mhvtl-verify-gate.sh` depends on:
    /// four volumes initialised on ONE cartridge in a single run, each after
    /// an `mt erase`, WITHOUT `--force`. Every one must succeed, closing the
    /// previous mount and marking the previous volume erased.
    #[test]
    fn re_initialising_a_bound_cartridge_displaces_rather_than_refuses() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC001", "LTO-6", Some("SER-1"), "available");
        let cart_id = lookup_cartridge(&conn, Some("SER-1"), None)
            .unwrap()
            .row
            .unwrap()
            .id;

        let mut previous: Option<(i64, String)> = None;
        for label in ["MHVTLG", "MHVTLR1", "MHVTLR2", "MHVTLR3"] {
            let found = lookup_cartridge(&conn, Some("SER-1"), None).unwrap();
            let vol = new_volume(&conn, label);
            let out = bind_cartridge(
                &conn,
                vol,
                found.row.as_ref(),
                Some("SER-1"),
                Generation::Lto6,
                2_500_000_000_000,
                &MamInfo::default(),
            )
            .expect("binding must never refuse (ADR-0010)");

            match &previous {
                None => assert!(out.displaced.is_empty()),
                Some((prev_id, prev_label)) => {
                    assert_eq!(out.displaced.len(), 1, "{label} should displace one volume");
                    assert_eq!(&out.displaced[0].label, prev_label);
                    assert_eq!(volume_status(&conn, *prev_id), "erased");
                }
            }
            // Exactly one open mount at all times: the volume just written.
            assert_eq!(open_mounts(&conn, cart_id), vec![vol]);
            previous = Some((vol, label.to_string()));
        }
    }

    /// The lifecycle ADR-0010 makes live for the first time: a cartridge left
    /// `pending_erase` by `compact-finish`, bulk-erased, then reused. No
    /// precondition, no `--force`.
    #[test]
    fn a_pending_erase_cartridge_is_reusable_with_no_override() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC001", "LTO-6", Some("SER-1"), "pending_erase");
        let found = lookup_cartridge(&conn, Some("SER-1"), None).unwrap();
        let cart_id = found.row.as_ref().unwrap().id;
        let vol = new_volume(&conn, "L6-0002");
        bind_cartridge(
            &conn,
            vol,
            found.row.as_ref(),
            Some("SER-1"),
            Generation::Lto6,
            2_500_000_000_000,
            &MamInfo::default(),
        )
        .unwrap();
        assert_eq!(cartridge_status(&conn, cart_id), "in_use");
    }

    // ---- #160: the barcode collision auto-register can reach -------------

    /// The reachable defect from #160: an operator hand-registers a
    /// cartridge using the serial printed on its shell as the barcode, with
    /// no `--serial` given (the operator guide used to teach exactly this).
    /// When that tape is loaded, `lookup_cartridge` finds nothing by serial
    /// (the row's `serial_number` is NULL), so `bind_cartridge` reaches its
    /// auto-register arm and would collide on the `barcode` UNIQUE
    /// constraint. This must be refused BY NAME, naming both the medium
    /// serial and the conflicting barcode, and must not adopt the existing
    /// row (that would rewrite its identity on a guess) or write anything.
    #[test]
    fn auto_register_refuses_a_barcode_collision_with_no_serial_match() {
        let conn = db::open_memory().unwrap();
        register(&conn, "E01001L8_1775794348", "LTO-8", None, "available");
        let vol = new_volume(&conn, "L6-0001");
        let mam = MamInfo {
            manufacturer: Some("HP".into()),
            ..MamInfo::default()
        };

        // No row matched by serial -- `lookup_cartridge` would have found
        // nothing, exactly like a fresh tape.
        let err = match bind_cartridge(
            &conn,
            vol,
            None,
            Some("E01001L8_1775794348"),
            Generation::Lto8,
            12_000_000_000_000,
            &mam,
        ) {
            Ok(_) => panic!("a barcode collision must be refused, not silently bound"),
            Err(e) => e.to_string(),
        };

        assert!(
            err.contains("E01001L8_1775794348"),
            "must name the colliding serial/barcode: {err}"
        );
        assert!(err.contains("relabel"), "must point at the remedy: {err}");

        let cartridge_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM cartridges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            cartridge_count, 1,
            "the refusal must not have written a second row"
        );
        let event_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            event_count, 0,
            "the refusal is read-only and must log nothing (the test's `register` \
             helper writes straight SQL, unlike the CLI, so there is no prior event)"
        );
    }

    /// #160 defect bullet 1 claimed `serial_number` could be written more
    /// than once. The coordinator's verification found two independent
    /// guards already prevent it: `lookup_cartridge` refuses a `--cartridge`
    /// naming a row with a DIFFERENT recorded serial
    /// (`cartridge_flag_naming_a_row_with_a_different_serial_is_an_error`,
    /// above), and `bind_cartridge`'s own UPDATE only fires
    /// `if r.serial_number.is_none()` (binding.rs). This pins the SECOND,
    /// deeper guard directly against `bind_cartridge` -- bypassing
    /// `lookup_cartridge`'s refusal on purpose -- so a future change that
    /// reaches `bind_cartridge` by any other path cannot silently start
    /// overwriting a recorded serial. Documents shipped behaviour; fixes
    /// nothing.
    #[test]
    fn bind_cartridge_never_overwrites_an_existing_serial() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC001", "LTO-6", Some("SER-1"), "available");
        let row = lookup_cartridge(&conn, None, Some("BC001"))
            .unwrap()
            .row
            .unwrap();
        let cart_id = row.id;
        let vol = new_volume(&conn, "L6-0001");

        let out = bind_cartridge(
            &conn,
            vol,
            Some(&row),
            Some("SER-2"),
            Generation::Lto6,
            2_500_000_000_000,
            &MamInfo::default(),
        )
        .unwrap();

        assert!(
            !out.serial_recorded,
            "a row that already has a serial must not report one as newly recorded"
        );
        let serial: Option<String> = conn
            .query_row(
                "SELECT serial_number FROM cartridges WHERE id = ?1",
                params![cart_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serial.as_deref(),
            Some("SER-1"),
            "bind_cartridge must never overwrite a recorded serial with a different one"
        );
    }

    /// The displacement warning must be able to name the units that just
    /// lost their last copy — that is the whole reason it is a warning and
    /// not a log line. The count comes from `retire_impacts`, unmodified.
    #[test]
    fn displacement_reports_units_that_lose_their_last_copy() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC001", "LTO-6", Some("SER-1"), "available");

        // A unit whose only completed write is on the volume about to be
        // displaced.
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('u1', 'photos', 1, 'mtime_size', 1, 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
             VALUES (1, 1, 'full', 'current', '/srv/photos')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size)
             VALUES (1, 'staged', 524288)",
            [],
        )
        .unwrap();

        let old = new_volume(&conn, "L6-OLD");
        conn.execute(
            "UPDATE volumes SET status = 'sealed' WHERE id = ?1",
            params![old],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (1, 1, ?1, 'completed')",
            params![old],
        )
        .unwrap();
        let found = lookup_cartridge(&conn, Some("SER-1"), None).unwrap();
        bind_cartridge(
            &conn,
            old,
            found.row.as_ref(),
            Some("SER-1"),
            Generation::Lto6,
            2_500_000_000_000,
            &MamInfo::default(),
        )
        .unwrap();

        let new = new_volume(&conn, "L6-NEW");
        let found = lookup_cartridge(&conn, Some("SER-1"), None).unwrap();
        let out = bind_cartridge(
            &conn,
            new,
            found.row.as_ref(),
            Some("SER-1"),
            Generation::Lto6,
            2_500_000_000_000,
            &MamInfo::default(),
        )
        .unwrap();

        assert_eq!(out.displaced.len(), 1);
        let impacts = &out.displaced[0].impacts;
        assert_eq!(impacts.len(), 1);
        assert_eq!(impacts[0].unit_name, "photos");
        assert_eq!(
            impacts[0].other_copies, 0,
            "photos has no other copy — the warning must be able to say so"
        );
    }
}
