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
//! is refused absolutely and one naming another unsealed volume is refused
//! unless `--force` (ADR-0003, issue #27), and that decision is
//! made against the medium's own evidence rather than the catalog's weaker
//! claim about it. An operator who reached binding either loaded a blank or
//! erased tape — the data is already physically gone — or gave `--force`,
//! which is the consent. Demanding a second override for the same act is the
//! ceremony ADR-0008 warns about, and it would turn `mt erase` + `volume
//! init` — the most ordinary reuse there is — into a two-command catalog
//! dance that teaches operators to reach for `--force` by reflex.
//!
//! **That rule holds only on a serial MATCH** (ADR-0010's "Correction
//! 2026-09-14", ADR-0012, issue #155). It rests on two facts, not one: File 0
//! decided consent, AND the MAM serial proves the blank tape in the drive is
//! the cartridge being displaced. A match needs a serial on both sides, so the
//! second fact is missing whenever the DRIVE reports none or the named ROW has
//! none recorded. Then the case is refused rather than gated — see
//! [`refuse_unwitnessed_displacement`], which is a FACT refusal like the rest
//! below, not the consent gate ADR-0010 rejected.
//!
//! So binding RECORDS the displacement instead of relitigating it: the open
//! mount is closed, the displaced volume moves to `erased`, an events row
//! says why, and the caller warns — naming the volume and any unit that just
//! lost its last copy. Nothing is blocked (ADR-0004); the catalog simply
//! stops crediting a copy that no longer physically exists, which is the
//! over-crediting the lifecycle suite's own comment predicted.
//!
//! The refusals binding DOES introduce are all fact errors rather than
//! consent gates — tapectl refuses because it cannot tell what is true, not
//! because it wants the operator to insist:
//!
//! - [`crate::tape::media_detect::resolve_media`] (not in this module): a
//!   registered row whose `media_type` disagrees with the DETECTED generation
//!   means either the row is wrong or the wrong tape is loaded.
//! - [`refuse_retired`]: no amount of consent makes a medium declared
//!   permanently unfit fit again (ADR-0011).
//! - [`require_named_cartridge`]: no serial and no `--cartridge` leaves
//!   nothing to bind to at all (ADR-0012, issue #192).
//! - [`refuse_unwitnessed_displacement`]: no serial MATCH and a `--cartridge`
//!   naming a row still bound to a LIVE volume is undecidable — that
//!   cartridge erased, or a different tape wearing its sticker (ADR-0012,
//!   issue #155).
//! - [`corroborate_contact`]: at every LATER contact, two KNOWN identities
//!   that disagree — the medium's, the bound row's, File 0's, the volume
//!   named on the command line — mean the wrong tape is loaded (ADR-0012,
//!   issue #193). This is the only one of the five that also WRITES: a bound
//!   row with no serial learns one, once.
//!
//! None of the five takes a `force`, structurally.
//!
//! The last one is where this module stops being only about `volume init`.
//! Everything above `corroborate_contact` ESTABLISHES a binding; everything
//! from it down CHECKS one, at `volume write`, `volume resume`, `volume
//! verify`, `volume read-slices`, `volume compact-read`, `restore`, `catalog
//! rebuild` and `volume identify` alike — one implementation, because six
//! copies of a four-branch rule is how they drift.

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
    /// `None` when nothing could be bound. Since ADR-0012 (issue #192)
    /// `volume init` refuses before reaching that state
    /// ([`require_named_cartridge`]), so at init this is always `Some`; the
    /// `None` case survives for [`bind_late`], whose no-op on a serial-less
    /// drive is what keeps `volume write` working on volumes initialised
    /// before that rule existed.
    ///
    /// [`bind_late`]: crate::volume::write
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
/// the barcode was typed by a human who may have picked up the wrong tape. A
/// superseded `--cartridge` is not among the refusals binding introduces, so
/// it is reported, not refused.
///
/// ADR-0010's original text called the row-vs-medium *generation* disagreement
/// "the only new refusal"; its 2026-09-14 correction retracts that. There are
/// now four, listed in this module's header — two of them this function's own
/// (an unregistered barcode, and a row carrying a DIFFERENT serial).
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
                 (`tapectl cartridge register --barcode {barcode} --generation <GEN>`), \
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

/// Refuse a `volume init` that can identify no cartridge at all (ADR-0012).
///
/// When the drive reports no medium serial AND no `--cartridge` named a
/// registered row, there is nothing to bind to. ADR-0010 wrote such a volume
/// unbound with a warning; ADR-0012 removed that outcome **at init**, because
/// a volume no cartridge claims is a copy the catalog cannot place: copy
/// counting cannot tell this cartridge from another, `check_loaded_cartridge`
/// has nothing to compare against, and File 0 can attest no identity — the
/// three things the rest of this module exists to provide.
///
/// Like [`refuse_retired`] this takes no `force`: it is a fact tapectl cannot
/// resolve on its own, not a risk to accept. The operator knows which
/// cartridge is in the drive and says so; nothing about `--force` would tell
/// tapectl.
///
/// **This lives here but is called only from `volume_init`, deliberately.**
/// [`bind_cartridge`] must keep accepting `(None, None)` as a clean no-op:
/// `bind_late` also calls it, has no `--cartridge` to offer, and that no-op
/// is what keeps `volume write` working on volumes initialised before this
/// rule existed. A refusal inside `bind_cartridge` would break them. Pure and
/// separate so it can be drilled directly, and so the refusal can run with
/// the other FACT checks — before the tape device is opened and before the
/// transaction, so a refused init leaves nothing behind.
pub(crate) fn require_named_cartridge(
    serial: Option<&str>,
    row: Option<&CartridgeRow>,
) -> Result<()> {
    if serial.is_some() || row.is_some() {
        return Ok(());
    }
    Err(TapectlError::Other(
        "this drive reports no medium serial, so tapectl cannot tell which physical \
         cartridge is loaded. Name it:\n    \
         tapectl volume init <label> --device <dev> --cartridge <barcode>\n\
         \n\
         A volume no cartridge claims is a copy the catalog cannot place: copy counting \
         cannot tell it from another tape, `volume write` cannot check you reloaded the \
         same one, and the tape itself can record no identity. Register the cartridge \
         first if it is new (`tapectl cartridge register --barcode <barcode> \
         --generation <GEN>`). There is no --force for this — it is a fact tapectl \
         cannot resolve on its own, not a risk to accept."
            .to_string(),
    ))
}

/// Refuse a `--cartridge` with NO SERIAL MATCH that would displace a LIVE
/// volume (ADR-0012; ADR-0010's "Correction 2026-09-14"; issue #155).
///
/// ADR-0010's "Binding adds no second consent gate" rests on TWO facts, not
/// one: File 0 has already decided consent, **and** the MAM serial proves the
/// blank tape in the drive is the same cartridge whose volume is being
/// displaced. ADR-0010's correction states the restriction in exactly those
/// terms — the rule "holds only when the cartridge was **matched by MAM
/// serial**" — and a match needs a serial on BOTH sides:
///
/// - The drive reports none. The operator named the row by typed barcode, so
///   the medium that consented via File 0 is a different physical object from
///   the row being erased.
/// - The drive reports one but the named row has none recorded. There is
///   nothing to have matched it against, so the serial proves nothing about
///   *this* row. This case is worse than the first: binding would also write
///   that serial onto the row permanently ([`bind_cartridge`] records a serial
///   onto a row that lacks one), so the real cartridge could never be bound
///   again — data loss with a dead end attached.
///
/// Either way `--cartridge B` over a blank tape is B erased or a different
/// tape wearing B's sticker, and tapectl cannot tell which.
///
/// `lookup_cartridge` has already refused the remaining shape — both serials
/// present and DIFFERENT — so by the time this runs, two `Some`s mean they
/// match. "Witnessed" therefore reduces to `serial.is_some() &&
/// row.serial_number.is_some()`.
///
/// **A non-displacing init is untouched.** ADR-0012's "a bound row that has no
/// serial yet learns it at that contact" governs corroboration, not assertion:
/// where no live volume is displaced this returns `Ok` and the row learns its
/// serial exactly as before.
///
/// So this is refused rather than gated — incoherence, not risk (ADR-0008).
/// Like [`refuse_retired`] and [`require_named_cartridge`] it takes no `force`
/// AT ALL, structurally, so a caller cannot defeat it by mistake. The way past
/// is the operator saying the bytes are gone, in the vocabulary that already
/// exists for exactly that statement: `volume retire` or `cartridge
/// mark-erased`. Where the serial DOES match, ADR-0010's original reasoning
/// still governs and the displacement is recorded, not relitigated.
///
/// "Live" is [`crate::policy::coverage::in_service`], the named predicate,
/// never an inlined status list (issue #96). An `initialized` volume is
/// deliberately NOT live: it holds no bytes, and refusing there would block
/// re-initialising a cartridge whose first init was abandoned — an ordinary
/// workflow precisely on the drives this refusal is about.
///
/// **This lives here but is called only from `volume_init`**, for the same
/// reason as [`require_named_cartridge`]: [`bind_cartridge`] must keep
/// accepting a bind with no serial match, because `bind_late` calls it with no
/// `--cartridge` to offer and that no-op is what keeps `volume write` working
/// on volumes initialised before these rules. Pure of the tape and of the
/// transaction, so a refused init leaves nothing behind.
pub(crate) fn refuse_unwitnessed_displacement(
    conn: &Connection,
    serial: Option<&str>,
    row: Option<&CartridgeRow>,
) -> Result<()> {
    let Some(row) = row else { return Ok(()) };
    // Witnessed: the chip named the tape in the drive AND the row has a serial
    // it was named BY. `lookup_cartridge` already refused two Somes that
    // differ, so reaching here with both present means they agree.
    if serial.is_some() && row.serial_number.is_some() {
        return Ok(());
    }

    // No `cv.volume_id != ?` exclusion, unlike `bind_cartridge`'s own
    // displacement query and `free_cartridge_if_last_live`: this runs before
    // the transaction that INSERTs the new `volumes` row, so there is no
    // self-mount to exclude. Every open mount found here belongs to some
    // OTHER volume by construction.
    let sql = format!(
        "SELECT v.label FROM cartridge_volumes cv
         JOIN volumes v ON v.id = cv.volume_id
         WHERE cv.cartridge_id = ?1 AND cv.unmounted_at IS NULL AND {}
         ORDER BY v.id",
        crate::policy::coverage::in_service("v")
    );
    let mut stmt = conn.prepare(&sql)?;
    let live: Vec<String> = stmt
        .query_map(params![row.id], |r| r.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);
    if live.is_empty() {
        return Ok(());
    }

    let barcode = &row.barcode;
    let labels = live
        .iter()
        .map(|l| format!("\"{l}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let (is_are, volume_s) = if live.len() == 1 {
        ("is", "volume")
    } else {
        ("are", "volumes")
    };
    let retires = live
        .iter()
        .map(|l| format!("    tapectl volume retire {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    // Why there is no serial MATCH — the two shapes that reach here. Both end
    // in the same place, but an operator staring at a drive that plainly did
    // read a serial needs to be told the gap is in the catalog, not the drive.
    let why = match serial {
        None => "this drive reports no medium serial".to_string(),
        Some(s) => format!(
            "the drive reports medium serial {s}, but \"{barcode}\" has none recorded to \
             compare it against — and binding would write {s} onto \"{barcode}\" \
             permanently, so the real \"{barcode}\" could never be bound again"
        ),
    };
    Err(TapectlError::Other(format!(
        "{why}, so tapectl cannot confirm the tape in the drive is cartridge \
         \"{barcode}\" — and \"{barcode}\" is bound to {volume_s} \
         {labels}, which {is_are} still live.\n\
         \n\
         That makes this undecidable: either \"{barcode}\" has been erased, in which case \
         those bytes are already gone, or a DIFFERENT tape is wearing \"{barcode}\"'s \
         sticker and the real one is still on the shelf, holding whatever copies the \
         catalog credits it with. File 0 consented for the tape in the DRIVE; it says \
         nothing about which cartridge that is. Only a matching medium serial proves \
         that, and there is none here.\n\
         \n\
         Say the bytes are gone first, then re-run this init:\n\
         {retires}\n\
         or, if the whole cartridge is wiped:\n    \
         tapectl cartridge mark-erased {barcode}\n\
         \n\
         There is no --force for this — it is a fact tapectl cannot resolve on its own, \
         not a risk to accept."
    )))
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
/// refusing) whatever it displaces — the one case ADR-0010's rule does not
/// cover having already been refused upstream by
/// [`refuse_unwitnessed_displacement`] (ADR-0012, issue #155).
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
                    // One writer, shared with `corroborate_contact`'s learn
                    // branch (ADR-0012, issue #193): write-once is the
                    // caller's `serial_number IS NULL` guard, here and
                    // there, rather than a rule spelled twice.
                    record_medium_serial(conn, r.id, &r.barcode, s)?;
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
    // Still never refused HERE, and the one case ADR-0010's rule does not
    // cover was already refused upstream: `volume_init` calls
    // `refuse_unwitnessed_displacement` before the transaction, so a
    // `--cartridge` with no serial MATCH naming a row bound to a live volume
    // never reaches this loop (ADR-0012, issue #155). What does reach it is a
    // serial match — where the medium itself proves which cartridge this is —
    // or a displacement of something not live.
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
    // `identity_source` records which identity THIS BINDING was established
    // under (ADR-0012, migration 014, issue #192), and the rule has no
    // inference in it: `serial` IS the MAM read, so `Some` means the chip
    // identified this cartridge and `None` means the operator named it with
    // `volume init --cartridge <barcode>`. `volume write` reads this back to
    // fill File 0's `[media].cartridge_identity_source`, taking
    // `cartridges.serial_number` for 'mam' and `.barcode` for 'operator', so
    // the tape and the catalog cannot disagree about which string identifies
    // the cartridge.
    //
    // `bind_late` always reaches here with `Some` (it early-returns when no
    // serial is readable), so a late binding is always 'mam' — which is
    // exactly right: it exists only because a serial became readable.
    //
    // The `OR IGNORE` is left alone deliberately. That a re-bind of the same
    // pair is silently dropped is a real defect, but it is issue #162's, not
    // this change's.
    let identity_source = if serial.is_some() { "mam" } else { "operator" };
    conn.execute(
        "INSERT OR IGNORE INTO cartridge_volumes (cartridge_id, volume_id, identity_source)
         VALUES (?1, ?2, ?3)",
        params![cartridge_id, volume_id, identity_source],
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

// ── Corroboration at contact (ADR-0012, issue #193) ─────────────────────
//
// Everything above this line is about ESTABLISHING a binding at `volume
// init`. Everything below is about CHECKING it at every later contact —
// the other half of the same ADR-0012 ruling, and deliberately one
// implementation rather than one per call site.

/// What the catalog claims about the volume a contact names.
///
/// Built by [`claim_for_volume`]. The absence of the whole struct (the
/// caller passing `None`) is the DR case: a tape whose volume row does not
/// exist yet, which corroborates against nothing and proceeds.
#[derive(Debug, Clone)]
pub(crate) struct CatalogClaim {
    pub volume_label: String,
    /// `volumes.uuid`, read PLAINLY. Never via `volume::write::volume_uuid`,
    /// which self-heals a NULL by WRITING one — corroboration on a read path
    /// must not mutate the catalog just to have something to compare.
    pub volume_uuid: Option<String>,
    /// The open `cartridge_volumes` mount, when the volume has one. `None`
    /// is an absence (an unbound volume), never a contradiction.
    pub bound: Option<BoundCartridge>,
}

/// The cartridge a volume is currently mounted on, as the catalog has it.
#[derive(Debug, Clone)]
pub(crate) struct BoundCartridge {
    pub cartridge_id: i64,
    pub barcode: String,
    pub serial_number: Option<String>,
    /// The BINDING's identity source (migration 014): `"mam"`, `"operator"`,
    /// or `None` for a binding predating #192. Recorded for completeness —
    /// the `operator` case is corroborated by the EXISTENCE of this binding,
    /// not by anything compared against this field.
    pub identity_source: Option<String>,
}

/// What File 0 says, when File 0 was readable and parseable.
///
/// Every field is independently optional because every one of them is an
/// ABSENCE when missing: a tape that could not be read, a legacy thunk with
/// no `[media]` table, a pre-#192 `[media]` with no identity source. None of
/// those is a contradiction (issue #193: "Corroboration refuses a
/// contradiction, never an absence").
#[derive(Debug, Clone, Default)]
pub(crate) struct File0Facts {
    pub label: Option<String>,
    pub uuid: Option<String>,
    pub media: Option<crate::volume::format::IdThunkMedia>,
}

impl File0Facts {
    /// File 0's `cartridge_serial` **if and only if it is a chip serial**.
    ///
    /// `Some("operator")` means the string is a BARCODE, and ADR-0012 says
    /// in terms that it is "corroborated through the catalog binding, not by
    /// comparing its recorded barcode to the row's — the row may since have
    /// been relabelled" (`cartridge relabel`, issue #160, is a legitimate
    /// act). So it contributes nothing here, and the corroboration for that
    /// case is the binding itself.
    ///
    /// `None` means UNKNOWN and MUST NOT be read as `"mam"`: every tape
    /// written before the field existed omits it, and defaulting would make
    /// all of them falsely attest a chip-verified serial
    /// (`volume-format-v2.md` §1.1).
    fn chip_serial(&self) -> Option<&str> {
        let media = self.media.as_ref()?;
        match media.cartridge_identity_source.as_deref() {
            Some("mam") => Some(media.cartridge_serial.as_str()),
            _ => None,
        }
    }
}

/// Everything this contact can see about the medium in the drive.
///
/// `serial` is the MAM chip read (`None` = the drive cannot see one, as on
/// some virtual drives); `file0` is what the tape's own File 0 attests
/// (`File0Facts::default()` = not read, or unparseable).
#[derive(Debug, Clone, Default)]
pub(crate) struct MediumFacts {
    pub serial: Option<String>,
    pub file0: File0Facts,
}

impl MediumFacts {
    /// The MAM read alone — the shape the two WRITE contacts use.
    ///
    /// `volume write` and `volume resume` deliberately offer no File 0 here:
    /// their File-0 discipline is the ADR-0003 CONSENT gate
    /// (`check_fresh_write_contact` for a fresh write, `check_tape_contact`
    /// → divergence → quarantine for a resume), and a fact refusal running
    /// first would pre-empt both — including the quarantine that is how a
    /// resume is supposed to record a divergent tape.
    pub(crate) fn from_serial(serial: Option<String>) -> Self {
        Self {
            serial,
            file0: File0Facts::default(),
        }
    }

    /// The MAM read plus File 0 — the shape every READ contact uses.
    pub(crate) fn new(serial: Option<String>, file0: File0Facts) -> Self {
        Self { serial, file0 }
    }
}

/// What [`corroborate_contact`] concluded. There is no "refused" variant:
/// a contradiction is an `Err`, like every other fact refusal in this
/// module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Corroboration {
    /// Every fact both sides know agrees, or one side does not know it.
    Agreed,
    /// The bound row had no serial recorded and the medium reported one, so
    /// the row has now learnt it — once.
    SerialLearned { barcode: String, serial: String },
}

/// Read File 0 off `store` and parse what it says about identity, LENIENTLY.
///
/// Never errors. A tape read failure, a File 0 that is not v2, a missing
/// `[media]` table — all are absences, and an absence proceeds (ADR-0010's
/// read-path leniency, issue #193's constraint 1). The refusals this feeds
/// are for two KNOWN facts that disagree.
pub(crate) fn read_file0_facts(store: &mut dyn crate::store::Store) -> File0Facts {
    let mut raw = Vec::new();
    if let Err(e) = store.read_file(0, &mut raw) {
        tracing::warn!(err = %e, "could not read File 0 at contact; corroborating without it");
        return File0Facts::default();
    }
    file0_facts_from_text(&String::from_utf8_lossy(&raw))
}

/// [`read_file0_facts`] over File 0 text already in hand — the shape
/// `volume identify` needs, having just read and printed those bytes.
pub(crate) fn file0_facts_from_text(text: &str) -> File0Facts {
    use crate::volume::format;
    let identity = format::parse_id_thunk_identity(text).ok();
    File0Facts {
        label: identity.as_ref().map(|i| i.label.clone()),
        uuid: identity.as_ref().map(|i| i.uuid.clone()),
        media: format::parse_id_thunk_media(text).ok(),
    }
}

/// The MAM medium serial the drive can report for `device`, or `None`.
///
/// LENIENT by construction (ADR-0010, "Read paths stay usable without a
/// configured drive"): the serial lives behind the drive's SCSI generic
/// node, which is only known from a configured backend. A rebuilt machine
/// with keys and no `backend add` gets `None` — an absence, so every read
/// path still works, which is exactly ADR-0005's DR path.
pub(crate) fn loaded_medium_serial(config: &crate::config::Config, device: &str) -> Option<String> {
    let backend = config
        .backends
        .lto
        .iter()
        .find(|b| b.device_tape == device)?;
    crate::tape::media_detect::detect(device, &backend.device_sg)
        .mam
        .serial
}

/// The catalog's claim about one volume, for [`corroborate_contact`].
pub(crate) fn claim_for_volume(
    conn: &Connection,
    volume_id: i64,
    label: &str,
) -> Result<CatalogClaim> {
    let volume_uuid: Option<String> = conn
        .query_row(
            "SELECT uuid FROM volumes WHERE id = ?1",
            params![volume_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    // `.optional()?`, never `.ok()` — the same rule `select_cartridge`
    // states: a locked or malformed database must surface, not silently read
    // as "unbound" and wave a wrong cartridge through.
    let bound = conn
        .query_row(
            "SELECT c.id, c.barcode, c.serial_number, cv.identity_source
             FROM cartridge_volumes cv
             JOIN cartridges c ON c.id = cv.cartridge_id
             WHERE cv.volume_id = ?1 AND cv.unmounted_at IS NULL",
            params![volume_id],
            |r| {
                Ok(BoundCartridge {
                    cartridge_id: r.get(0)?,
                    barcode: r.get(1)?,
                    serial_number: r.get(2)?,
                    identity_source: r.get(3)?,
                })
            },
        )
        .optional()?;
    Ok(CatalogClaim {
        volume_label: label.to_string(),
        volume_uuid,
        bound,
    })
}

/// Corroborate one volume at contact: load the catalog's claim about it and
/// run [`corroborate_contact`] against it.
///
/// **The entry point every contact uses** — `volume write`, `volume resume`,
/// `volume verify`, `volume read-slices`, `volume compact-read`, `restore`,
/// `catalog rebuild` and `volume identify`. It exists so a call site is one
/// line and cannot get the claim wrong; the RULE is still the one function
/// below.
///
/// A learnt serial is announced here rather than inside the rule, so every
/// contact reports it identically and none has to remember to.
pub(crate) fn corroborate_volume(
    conn: &Connection,
    volume_id: i64,
    label: &str,
    medium: &MediumFacts,
) -> Result<Corroboration> {
    let claim = claim_for_volume(conn, volume_id, label)?;
    let outcome = corroborate_contact(conn, Some(&claim), medium, None)?;
    if let Corroboration::SerialLearned { barcode, serial } = &outcome {
        eprintln!(
            "note: cartridge \"{barcode}\" had no medium serial recorded; learnt {serial} \
             from the tape in the drive at this contact."
        );
    }
    Ok(outcome)
}

/// Corroborate the loaded medium against the catalog, at ANY contact
/// (ADR-0012, issue #193). **The one implementation; never re-write it per
/// call site.**
///
/// ADR-0012, the corroboration half of "A cartridge is known by the serial
/// its chip reports; a barcode is a label":
///
/// > when the medium and the bound row both have a serial they must agree,
/// > and a different serial is a different cartridge, refused; a bound row
/// > that has no serial yet learns it at that contact, once (`NULL` → value,
/// > never value → value) — unless another row already holds that serial, in
/// > which case the loaded tape *is* that other cartridge and the write is
/// > refused; when the medium reports no serial, `--cartridge` must name the
/// > bound row. A File 0 whose identity source is `operator` is corroborated
/// > through the catalog binding, not by comparing its recorded barcode to
/// > the row's — the row may since have been relabelled.
///
/// CONTEXT.md **Contact**: "the only moment the tape is authoritative, and
/// therefore the reconciliation event".
///
/// **Absence is not contradiction.** This is the rule the call sites get
/// wrong and the one to re-read before adding any refusal here. A drive that
/// reports no serial, a row with none recorded, a volume with no binding, a
/// missing or unparseable File 0, a `cartridge_identity_source` that is not
/// there — every one of those is a fact this contact cannot see, and a check
/// that cannot see cannot refuse (the rule [`check_loaded_cartridge`]'s doc
/// comment introduced, preserved verbatim here). Only two KNOWN facts that
/// differ are a contradiction. `claim = None` — the rebuilt machine, the DR
/// path, a `catalog rebuild` on a tape whose row does not exist yet —
/// proceeds unconditionally.
///
/// [`check_loaded_cartridge`]: crate::volume::write
///
/// **`identity_source = "operator"` is corroborated THROUGH THE BINDING.**
/// File 0 names the volume; the volume's open `cartridge_volumes` mount
/// names the cartridge. That chain IS the corroboration. File 0's recorded
/// barcode is never string-matched against `cartridges.barcode`, because
/// `cartridge relabel` (issue #160) is a legitimate act and the sticker may
/// since have changed — see [`File0Facts::chip_serial`].
///
/// Like [`refuse_retired`], [`require_named_cartridge`] and
/// [`refuse_unwitnessed_displacement`], this takes **no `force` parameter at
/// all**, structurally, so no caller can defeat it by mistake. Its refusals
/// are facts tapectl cannot resolve on its own, not risks to accept
/// (ADR-0008).
///
/// `requested_barcode` is the ruling's `--cartridge` clause. Today only
/// `volume init` has that flag, and init is the ESTABLISHING event
/// ([`lookup_cartridge`] + [`require_named_cartridge`] +
/// [`refuse_unwitnessed_displacement`]), so every production contact passes
/// `None`. The branch lives here anyway: the whole point of this function is
/// that the rule exists once, so a contact that later gains the flag wires
/// it in rather than writing a fifth copy.
pub(crate) fn corroborate_contact(
    conn: &Connection,
    claim: Option<&CatalogClaim>,
    medium: &MediumFacts,
    requested_barcode: Option<&str>,
) -> Result<Corroboration> {
    // ABSENCE: no catalog claim at all. The DR path — a tape whose volume
    // row does not exist yet — has nothing to contradict, so it proceeds.
    let Some(claim) = claim else {
        return Ok(Corroboration::Agreed);
    };
    let label = &claim.volume_label;

    // CONTRADICTION: File 0 names a DIFFERENT volume than the one asked
    // about (issue #164). Checked before the cartridge, because it is the
    // cheaper and more legible statement of the same wrong-tape fact.
    if let Some(found) = &claim_mismatch_label(claim, medium) {
        return Err(TapectlError::Other(format!(
            "wrong tape: this command names volume \"{label}\", but the loaded cartridge's \
             File 0 identifies volume \"{found}\". Load \"{label}\"'s cartridge, or re-run \
             naming \"{found}\". There is no --force for this — it is a fact tapectl cannot \
             resolve on its own, not a risk to accept."
        )));
    }
    if let (Some(found), Some(expected)) = (&medium.file0.uuid, &claim.volume_uuid) {
        if !found.is_empty() && !expected.is_empty() && found != expected {
            return Err(TapectlError::Other(format!(
                "wrong tape: volume \"{label}\" is uuid {expected} in this catalog, but the \
                 loaded cartridge's File 0 carries uuid {found} under the same label. A label \
                 can be reused after a retire; the uuid cannot, so this is a different volume \
                 (`layout-session.md`). There is no --force for this."
            )));
        }
    }

    // ABSENCE: the volume has no open binding, so there is no cartridge
    // claim to corroborate. `bind_late` exists precisely for volumes
    // initialised before ADR-0010 and must keep working.
    let Some(bound) = &claim.bound else {
        return Ok(Corroboration::Agreed);
    };
    let barcode = &bound.barcode;

    // The ruling's first clause: the medium and the bound row both have a
    // serial, so they must agree. Wording preserved from the
    // `check_loaded_cartridge` this replaces, so an operator who has seen
    // this refusal before still recognises it.
    if let (Some(recorded), Some(loaded)) = (&bound.serial_number, &medium.serial) {
        if recorded != loaded {
            return Err(TapectlError::Other(format!(
                "wrong cartridge: volume \"{label}\" was initialised on {recorded} \
                 (cartridge {barcode}), the drive holds {loaded}. Load that cartridge, or \
                 `volume init` a new label on this one."
            )));
        }
    }

    // File 0's own claim, but ONLY where it says the string is a chip serial
    // — see `File0Facts::chip_serial` for why `operator` and `None` are not.
    if let Some(file0_chip) = medium.file0.chip_serial() {
        if let Some(loaded) = &medium.serial {
            if file0_chip != loaded {
                return Err(TapectlError::Other(format!(
                    "wrong cartridge: the tape's own File 0 records that volume \"{label}\" was \
                     written to medium serial {file0_chip}, but the drive reports {loaded}. \
                     Those are two different cartridges. There is no --force for this."
                )));
            }
        }
        if let Some(recorded) = &bound.serial_number {
            if file0_chip != recorded {
                return Err(TapectlError::Other(format!(
                    "wrong cartridge: the tape's own File 0 records that volume \"{label}\" was \
                     written to medium serial {file0_chip}, but this catalog binds it to \
                     cartridge {barcode} (serial {recorded}). There is no --force for this."
                )));
            }
        }
    }

    // The ruling's third clause: with no serial off the medium, the bound
    // row is the only thing identifying the cartridge, so an explicit
    // `--cartridge` naming a different row is a contradiction.
    if medium.serial.is_none() {
        if let Some(requested) = requested_barcode {
            if requested != barcode {
                return Err(TapectlError::Other(format!(
                    "wrong cartridge: --cartridge names \"{requested}\", but volume \"{label}\" \
                     is bound to cartridge \"{barcode}\" — and this drive reports no medium \
                     serial, so tapectl cannot tell which tape is actually loaded. Name \
                     \"{barcode}\", or load the cartridge you did name. There is no --force \
                     for this."
                )));
            }
        }
        return Ok(Corroboration::Agreed);
    }

    // The ruling's second clause: a bound row that has no serial yet LEARNS
    // it at this contact, once. `value -> value` is impossible by
    // construction — this arm is only reached when `serial_number IS NULL`.
    let Some(loaded) = medium.serial.as_deref() else {
        return Ok(Corroboration::Agreed);
    };
    if bound.serial_number.is_some() {
        return Ok(Corroboration::Agreed);
    }

    // ...but ONLY for a binding that never had one. `bind_cartridge` records
    // `identity_source = 'mam'` exactly when its `serial` argument was
    // `Some` (migration 014), so `'mam'` with a NULL `serial_number` is not
    // ADR-0012's "a bound row that has no serial YET" — it is a row that HAD
    // a chip serial and has since lost it. That is an inconsistent catalog,
    // and `volume::write::resolve_cartridge_identity` already refuses it by
    // name, pointing at `db backup`.
    //
    // Learning here would silently repair it from THIS CONTACT'S MAM read
    // and make that guard unreachable — sealing File 0 with a serial taken
    // from whichever drive happened to write the tape, which is precisely
    // the source #192 rejected ("the identity comes from the binding, never
    // from a fresh MAM read"). A damaged catalog is not a fact this contact
    // can establish, so it is left exactly as it is: an ABSENCE on the read
    // paths, and the write path's own refusal where it matters.
    if bound.identity_source.as_deref() == Some("mam") {
        tracing::warn!(
            cartridge = %barcode,
            "binding claims a chip-reported identity but the cartridge row records no \
             serial; not learning one from this contact (inconsistent catalog)"
        );
        return Ok(Corroboration::Agreed);
    }

    // ...unless another row already holds that serial, in which case the
    // loaded tape IS that other cartridge. Refused BEFORE the UPDATE and by
    // name: `idx_cartridges_serial_number` would otherwise surface this as a
    // bare UNIQUE constraint error, which names neither cartridge.
    if let Some(other) = row_holding_serial(conn, loaded, bound.cartridge_id)? {
        return Err(TapectlError::Other(format!(
            "wrong cartridge: the drive reports medium serial {loaded}, which this catalog \
             already records for cartridge \"{other}\" — so the loaded tape IS \"{other}\", \
             not \"{barcode}\", which volume \"{label}\" is bound to. Load \"{barcode}\", or \
             if \"{other}\" and \"{barcode}\" are the same physical cartridge registered \
             twice, resolve that first. There is no --force for this — it is a fact tapectl \
             cannot resolve on its own, not a risk to accept."
        )));
    }

    // BEST-EFFORT, deliberately (issue #193's judgement point). The serial is
    // a fact the chip reported; it is true whether or not this contact's
    // operation succeeds, so it is recorded rather than discarded. But
    // learning is BOOKKEEPING, not consent: every refusal above has already
    // run, and nothing downstream reads this row again. So a catalog that
    // cannot be written — read-only media, a locked database, the DR machine
    // — warns and proceeds, because an unwritable catalog must never turn a
    // working READ into a failure (ADR-0010's read-path leniency). On a
    // write path a locked database fails at the next statement anyway.
    tracing::info!(
        cartridge = %barcode,
        serial = %loaded,
        // How the binding was established (migration 014). Not compared
        // against anything — an `operator` binding is corroborated by its own
        // existence — but it is the fact that EXPLAINS the missing serial, so
        // it belongs beside the repair in the log.
        bound_as = bound.identity_source.as_deref().unwrap_or("unknown"),
        "recording the medium serial for a binding that had none"
    );
    if let Err(e) = record_medium_serial(conn, bound.cartridge_id, barcode, loaded) {
        tracing::warn!(
            err = %e,
            cartridge = %barcode,
            serial = %loaded,
            "could not record the medium serial learnt at this contact (continuing)"
        );
        return Ok(Corroboration::Agreed);
    }
    Ok(Corroboration::SerialLearned {
        barcode: barcode.clone(),
        serial: loaded.to_string(),
    })
}

/// File 0's label when it DISAGREES with the claim's, else `None`.
///
/// Split out so the absence rule is legible at the call site: an
/// unparseable File 0 has no label, and no label is no contradiction.
fn claim_mismatch_label(claim: &CatalogClaim, medium: &MediumFacts) -> Option<String> {
    let found = medium.file0.label.as_ref()?;
    (found != &claim.volume_label).then(|| found.clone())
}

/// The barcode of a DIFFERENT cartridge row already recording `serial`.
fn row_holding_serial(conn: &Connection, serial: &str, excluding: i64) -> Result<Option<String>> {
    let found = conn
        .query_row(
            "SELECT barcode FROM cartridges WHERE serial_number = ?1 AND id != ?2",
            params![serial, excluding],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    Ok(found)
}

/// Record a medium serial onto a cartridge row that has none, with the
/// events row that says where it came from.
///
/// The ONE writer of `cartridges.serial_number` on the binding paths, shared
/// by [`bind_cartridge`] (establishing) and [`corroborate_contact`]
/// (learning at a later contact). Write-once is the CALLER's guarantee —
/// both call it only under `serial_number IS NULL` — which is what makes
/// ADR-0012's "`NULL` → value, never value → value" structural rather than a
/// rule repeated in two places.
fn record_medium_serial(
    conn: &Connection,
    cartridge_id: i64,
    barcode: &str,
    serial: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE cartridges SET serial_number = ?1 WHERE id = ?2",
        params![serial, cartridge_id],
    )?;
    events::log_field_change(
        conn,
        "cartridge",
        cartridge_id,
        barcode,
        "updated",
        "serial_number",
        None,
        serial,
        None,
    )?;
    Ok(())
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

    fn binding_identity_source(conn: &Connection, volume_id: i64) -> Option<String> {
        conn.query_row(
            "SELECT identity_source FROM cartridge_volumes
             WHERE volume_id = ?1 AND unmounted_at IS NULL",
            params![volume_id],
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
    /// A superseded `--cartridge` is not among the refusals ADR-0010 and
    /// ADR-0012 introduce, so this is reported, not refused. (ADR-0010's
    /// original text called the row-vs-medium GENERATION disagreement "the
    /// only new refusal"; its 2026-09-14 correction retracts that — there are
    /// now four, listed in this module's header.)
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

    // ---- ADR-0012: init must be able to name the cartridge --------------

    /// The `(None, None)` input — no medium serial, no `--cartridge` — is the
    /// one case `volume init` refuses (ADR-0012). Everything else passes
    /// through untouched, including the barcode-only case that IS the
    /// operator naming the cartridge.
    #[test]
    fn init_refuses_only_when_nothing_can_name_the_cartridge() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC001", "LTO-6", Some("SER-1"), "available");
        let row = lookup_cartridge(&conn, Some("SER-1"), None).unwrap().row;

        // A chip serial names it.
        require_named_cartridge(Some("SER-1"), None).unwrap();
        // A `--cartridge` that matched a registered row names it.
        require_named_cartridge(None, row.as_ref()).unwrap();
        // Both is fine too.
        require_named_cartridge(Some("SER-1"), row.as_ref()).unwrap();

        let err = require_named_cartridge(None, None)
            .expect_err("nothing can name the loaded cartridge; init must refuse")
            .to_string();
        assert!(
            err.contains("--cartridge"),
            "the refusal must name the flag; got: {err}"
        );
        assert!(
            err.contains("no medium serial"),
            "the refusal must say why; got: {err}"
        );
        // `--force` is named only to say it does not exist here, exactly as
        // `refuse_retired` does — never offered as a way past.
        assert!(
            err.contains("no --force for this"),
            "the refusal must close the --force door explicitly; got: {err}"
        );
    }

    // ---- #155: the serial-less displacement that has no witness ----------

    /// Bind `label` to `barcode` with the serial `serial`, then force the
    /// resulting volume to `status`. Produces the open `cartridge_volumes`
    /// mount that [`refuse_unwitnessed_displacement`] is asked about, built
    /// through the real binding path rather than by hand-inserting the join
    /// row — a fabricated mount would not prove the refusal sees what
    /// `bind_cartridge` actually writes.
    fn bind_then_set_status(
        conn: &Connection,
        barcode: &str,
        serial: Option<&str>,
        label: &str,
        status: &str,
    ) -> i64 {
        let found = lookup_cartridge(conn, serial, Some(barcode)).unwrap();
        let vol = new_volume(conn, label);
        bind_cartridge(
            conn,
            vol,
            found.row.as_ref(),
            serial,
            Generation::Lto6,
            2_500_000_000_000,
            &MamInfo::default(),
        )
        .unwrap();
        conn.execute(
            "UPDATE volumes SET status = ?1 WHERE id = ?2",
            params![status, vol],
        )
        .unwrap();
        vol
    }

    /// The #155 defect itself. An operator whose drive reports no medium
    /// serial loads blank BC003 and types `--cartridge BC002`, one digit
    /// wrong. BC002 is the sealed tape on the shelf holding the only copy of
    /// a unit. File 0 on the loaded tape is blank, so the contact check
    /// consents — but it consented for the tape in the DRIVE, and nothing
    /// proves that is BC002.
    ///
    /// ADR-0012 refuses this: "a blank tape plus `--cartridge B`, where B is
    /// bound to a live volume, is either B erased or a different tape wearing
    /// B's sticker, and tapectl cannot tell which."
    #[test]
    fn a_serial_less_cartridge_flag_will_not_displace_a_live_volume() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC002", "LTO-6", None, "available");
        bind_then_set_status(&conn, "BC002", None, "L6-SEALED", "sealed");

        let found = lookup_cartridge(&conn, None, Some("BC002")).unwrap();
        let err = refuse_unwitnessed_displacement(&conn, None, found.row.as_ref())
            .expect_err("no serial witnesses this displacement; init must refuse")
            .to_string();

        assert!(
            err.contains("no medium serial"),
            "the refusal must say why it cannot tell; got: {err}"
        );
        assert!(
            err.contains("BC002") && err.contains("L6-SEALED"),
            "the refusal must name the cartridge AND the volume at stake; got: {err}"
        );
        assert!(
            err.contains("wearing"),
            "the refusal must name the ambiguity — a different tape wearing that \
             sticker; got: {err}"
        );
        assert!(
            err.contains("tapectl volume retire L6-SEALED")
                && err.contains("tapectl cartridge mark-erased BC002"),
            "the refusal must name both ways past, ready to paste; got: {err}"
        );
        // `--force` is named only to close the door, exactly as
        // `refuse_retired` and `require_named_cartridge` do.
        assert!(
            err.contains("no --force for this"),
            "the refusal must close the --force door explicitly; got: {err}"
        );
    }

    /// The other half of "matched by MAM serial" (ADR-0010's Correction
    /// 2026-09-14): a match needs a serial on BOTH sides, and here the DRIVE
    /// has one but the named ROW does not. The serial proves the tape in the
    /// drive is *some* cartridge; it proves nothing about BC002, which has
    /// nothing recorded to have matched.
    ///
    /// Strictly worse than the serial-less case above, which is why the
    /// condition covers both: binding would ALSO record SER-9 onto BC002
    /// permanently (`bind_cartridge` writes a serial onto a row that lacks
    /// one), so the real BC002 could never be bound again — data loss with a
    /// dead end attached.
    ///
    /// The non-displacing version of this exact input stays legal and is
    /// covered by `a_hand_registered_cartridge_learns_its_serial_on_first_bind`
    /// below: ADR-0012's "a bound row that has no serial yet learns it at that
    /// contact" is about corroboration, not about asserting over a live volume.
    #[test]
    fn a_cartridge_flag_naming_a_serial_less_row_will_not_displace_a_live_volume() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC002", "LTO-6", None, "available");
        bind_then_set_status(&conn, "BC002", None, "L6-SEALED", "sealed");

        // The drive DOES read a serial this time; it just matches nothing.
        let found = lookup_cartridge(&conn, Some("SER-9"), Some("BC002")).unwrap();
        assert_eq!(found.row.as_ref().unwrap().barcode, "BC002");
        assert!(
            found.row.as_ref().unwrap().serial_number.is_none(),
            "fixture must give BC002 no recorded serial — that is the whole point"
        );

        let err = refuse_unwitnessed_displacement(&conn, Some("SER-9"), found.row.as_ref())
            .expect_err("a serial the named row cannot match witnesses nothing; init must refuse")
            .to_string();

        assert!(
            err.contains("SER-9") && err.contains("none recorded to compare it against"),
            "the refusal must say the gap is in the catalog, not the drive — an operator \
             watching the drive read a serial needs that; got: {err}"
        );
        assert!(
            err.contains("could never be bound again"),
            "the refusal must name the dead end binding would create; got: {err}"
        );
        assert!(
            err.contains("BC002") && err.contains("L6-SEALED"),
            "the refusal must name the cartridge AND the volume at stake; got: {err}"
        );
        assert!(
            err.contains("tapectl volume retire L6-SEALED")
                && err.contains("tapectl cartridge mark-erased BC002"),
            "the refusal must name both ways past, ready to paste; got: {err}"
        );
        assert!(
            err.contains("no --force for this"),
            "the refusal must close the --force door explicitly; got: {err}"
        );
    }

    /// The deliberate line, pinned so it stays where ADR-0012 put it: an
    /// `initialized` volume holds no bytes, so it is NOT live and must still
    /// displace. Refusing here would block re-initialising a cartridge whose
    /// first init was abandoned — an ordinary workflow precisely on the
    /// serial-less drives this refusal is about. Same reading of "live" as
    /// `free_cartridge_if_last_live`, and the same named predicate.
    ///
    /// Both layers are pinned: the refusal passes, AND the displacement it
    /// declines to block actually happens.
    #[test]
    fn a_serial_less_cartridge_flag_still_displaces_an_initialized_volume() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC002", "LTO-6", None, "available");
        // `new_volume` already creates `initialized`; set it explicitly so
        // the fixture's default cannot silently change what this test means.
        let abandoned = bind_then_set_status(&conn, "BC002", None, "L6-ABANDONED", "initialized");

        let found = lookup_cartridge(&conn, None, Some("BC002")).unwrap();
        refuse_unwitnessed_displacement(&conn, None, found.row.as_ref())
            .expect("an initialized volume is not live; re-init must not be blocked");

        let fresh = new_volume(&conn, "L6-RETRY");
        let out = bind_cartridge(
            &conn,
            fresh,
            found.row.as_ref(),
            None,
            Generation::Lto6,
            2_500_000_000_000,
            &MamInfo::default(),
        )
        .unwrap();
        assert_eq!(out.displaced.len(), 1);
        assert_eq!(out.displaced[0].label, "L6-ABANDONED");
        assert_eq!(volume_status(&conn, abandoned), "erased");
        assert_eq!(open_mounts(&conn, found.row.unwrap().id), vec![fresh]);
    }

    /// A mount that is already closed displaces nothing, so there is nothing
    /// to be undecidable about. The refusal asks about OPEN mounts only —
    /// `unmounted_at IS NULL` — exactly like `bind_cartridge`'s own
    /// displacement query.
    #[test]
    fn a_serial_less_cartridge_flag_is_fine_when_the_mount_is_already_closed() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC002", "LTO-6", None, "available");
        let vol = bind_then_set_status(&conn, "BC002", None, "L6-SEALED", "sealed");
        let found = lookup_cartridge(&conn, None, Some("BC002")).unwrap();
        let cart_id = found.row.as_ref().unwrap().id;
        conn.execute(
            "UPDATE cartridge_volumes SET unmounted_at = datetime('now') WHERE volume_id = ?1",
            params![vol],
        )
        .unwrap();
        assert!(open_mounts(&conn, cart_id).is_empty());

        refuse_unwitnessed_displacement(&conn, None, found.row.as_ref())
            .expect("a closed mount displaces nothing; there is nothing to refuse");
    }

    /// An `erased` volume's bytes are already gone — the operator has said
    /// so, which is the exact statement the refusal's message asks for. It is
    /// not `in_service`, so re-init proceeds without a second ceremony.
    #[test]
    fn a_serial_less_cartridge_flag_is_fine_over_an_already_erased_volume() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC002", "LTO-6", None, "available");
        bind_then_set_status(&conn, "BC002", None, "L6-GONE", "erased");
        let found = lookup_cartridge(&conn, None, Some("BC002")).unwrap();
        refuse_unwitnessed_displacement(&conn, None, found.row.as_ref())
            .expect("an erased volume is not live; re-init must not be blocked");
    }

    /// The restriction is a RESTRICTION, not a replacement, and this is its
    /// precise boundary: a serial on BOTH sides that agree. The medium itself
    /// then proves which cartridge is loaded, the second fact ADR-0010 rests
    /// on is present, and its original reasoning still governs — the
    /// displacement is recorded, never refused. Identical setup to the two
    /// refusal tests above apart from the serials, so what separates them is
    /// exactly the match and nothing else.
    #[test]
    fn a_witnessed_displacement_of_a_live_volume_still_proceeds() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC002", "LTO-6", Some("SER-1"), "available");
        let sealed = bind_then_set_status(&conn, "BC002", Some("SER-1"), "L6-SEALED", "sealed");

        let found = lookup_cartridge(&conn, Some("SER-1"), Some("BC002")).unwrap();
        // What makes this "witnessed" is the RECORDED serial, not just the one
        // the drive read — assert the fixture really provides it, or this test
        // would silently become a third copy of the refusal case.
        assert_eq!(
            found.row.as_ref().unwrap().serial_number.as_deref(),
            Some("SER-1")
        );
        refuse_unwitnessed_displacement(&conn, Some("SER-1"), found.row.as_ref())
            .expect("the serial proves which cartridge this is (ADR-0010); never refuse");

        let fresh = new_volume(&conn, "L6-NEW");
        let out = bind_cartridge(
            &conn,
            fresh,
            found.row.as_ref(),
            Some("SER-1"),
            Generation::Lto6,
            2_500_000_000_000,
            &MamInfo::default(),
        )
        .expect("a witnessed binding must never refuse (ADR-0010)");
        assert_eq!(out.displaced.len(), 1);
        assert_eq!(out.displaced[0].label, "L6-SEALED");
        assert_eq!(volume_status(&conn, sealed), "erased");
        assert_eq!(open_mounts(&conn, found.row.unwrap().id), vec![fresh]);
    }

    /// ADR-0010: no medium serial (some virtual drives expose none) means no
    /// binding, and that must be a clean unbound write rather than an error.
    ///
    /// **This is now the `bind_late` path specifically.** ADR-0012 made
    /// `volume_init` refuse `(None, None)` before it ever reaches here
    /// ([`require_named_cartridge`], drilled just above), but `bind_late` —
    /// which has no `--cartridge` to offer — still calls `bind_cartridge`
    /// with this shape, and its silent no-op is what keeps `volume write`
    /// working on volumes initialised before that rule existed. This test is
    /// the only coverage of that input, so it stays.
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

    /// Migration 014 / ADR-0012: the binding records which identity it was
    /// established under, and the rule is exactly "did the chip say so".
    /// `volume write` reads this back to fill File 0's
    /// `[media].cartridge_identity_source`, so getting it wrong here seals a
    /// provenance claim the catalog does not support.
    #[test]
    fn the_binding_records_whether_the_chip_or_the_operator_named_the_cartridge() {
        // A MAM serial was read: the chip identified this cartridge.
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
        assert_eq!(binding_identity_source(&conn, vol).as_deref(), Some("mam"));

        // No serial was readable, so the operator named the cartridge with
        // `--cartridge BC002`. The identity on tape will be that barcode.
        let conn = db::open_memory().unwrap();
        register(&conn, "BC002", "LTO-6", None, "available");
        let found = lookup_cartridge(&conn, None, Some("BC002")).unwrap();
        let vol = new_volume(&conn, "L6-0002");
        bind_cartridge(
            &conn,
            vol,
            found.row.as_ref(),
            None,
            Generation::Lto6,
            2_500_000_000_000,
            &MamInfo::default(),
        )
        .unwrap();
        assert_eq!(
            binding_identity_source(&conn, vol).as_deref(),
            Some("operator")
        );
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

    // ---- MAM load count recording (issue #184) ----------------------------
    //
    // `total_load_count` was SELECTed by `cartridge list`/`info` and written
    // by nothing: every cartridge showed "Loads: 0" forever, a wrong number
    // presented as a fact about wear. The MAM load count is already read at
    // both contacts `bind_cartridge` serves (`volume init` and `volume
    // write`'s late binding, both of which pass `&MamInfo` in), so the fix
    // lives entirely inside `bind_cartridge`: no new MAM read.

    #[test]
    fn bind_cartridge_records_the_mam_load_count_on_an_existing_row() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC001", "LTO-6", Some("SER-1"), "available");
        let found = lookup_cartridge(&conn, Some("SER-1"), None).unwrap();
        let cart_id = found.row.as_ref().unwrap().id;
        let vol = new_volume(&conn, "L6-0001");
        let mam = MamInfo {
            load_count: Some(7),
            ..MamInfo::default()
        };

        bind_cartridge(
            &conn,
            vol,
            found.row.as_ref(),
            Some("SER-1"),
            Generation::Lto6,
            2_500_000_000_000,
            &mam,
        )
        .unwrap();

        let loads: Option<i64> = conn
            .query_row(
                "SELECT total_load_count FROM cartridges WHERE id = ?1",
                params![cart_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            loads,
            Some(7),
            "a MAM-reported load count must be recorded onto the bound cartridge row"
        );
    }

    #[test]
    fn bind_cartridge_leaves_an_unreadable_load_count_null_on_auto_register() {
        let conn = db::open_memory().unwrap();
        let vol = new_volume(&conn, "L6-0001");

        let out = bind_cartridge(
            &conn,
            vol,
            None,
            Some("E01001L8_1775794348"),
            Generation::Lto8,
            12_000_000_000_000,
            &MamInfo::default(), // no load_count -- this drive/read did not report one
        )
        .unwrap();

        let loads: Option<i64> = conn
            .query_row(
                "SELECT total_load_count FROM cartridges WHERE id = ?1",
                params![out.cartridge_id.unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            loads, None,
            "a newly auto-registered cartridge with no readable MAM load count must stay \
             NULL (unknown), never fall back to the schema's DEFAULT 0"
        );
    }

    /// It is a monotone counter the drive itself maintains -- `bind_cartridge`
    /// must SET the latest observed value, never accumulate across contacts.
    #[test]
    fn bind_cartridge_writes_the_observed_load_count_not_an_increment() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC001", "LTO-6", Some("SER-1"), "available");
        let cart_id = lookup_cartridge(&conn, Some("SER-1"), None)
            .unwrap()
            .row
            .unwrap()
            .id;

        let vol1 = new_volume(&conn, "L6-0001");
        let row1 = lookup_cartridge(&conn, Some("SER-1"), None).unwrap();
        bind_cartridge(
            &conn,
            vol1,
            row1.row.as_ref(),
            Some("SER-1"),
            Generation::Lto6,
            2_500_000_000_000,
            &MamInfo {
                load_count: Some(3),
                ..MamInfo::default()
            },
        )
        .unwrap();

        // A later contact (e.g. re-initialising the same cartridge for a new
        // volume, ADR-0010's displacement) sees the drive's counter having
        // advanced.
        let vol2 = new_volume(&conn, "L6-0002");
        let row2 = lookup_cartridge(&conn, Some("SER-1"), None).unwrap();
        bind_cartridge(
            &conn,
            vol2,
            row2.row.as_ref(),
            Some("SER-1"),
            Generation::Lto6,
            2_500_000_000_000,
            &MamInfo {
                load_count: Some(5),
                ..MamInfo::default()
            },
        )
        .unwrap();

        let loads: Option<i64> = conn
            .query_row(
                "SELECT total_load_count FROM cartridges WHERE id = ?1",
                params![cart_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            loads,
            Some(5),
            "the column must be SET to the drive's latest observed count, never incremented \
             (3 + 5 must never appear)"
        );
    }

    /// A contact whose MAM read has no load count (e.g. a drive that does not
    /// expose the attribute) must not erase a value a PRIOR contact already
    /// established -- "unknown" is for never-observed, not for
    /// observed-then-forgotten.
    #[test]
    fn bind_cartridge_preserves_a_known_load_count_when_a_later_contact_reports_none() {
        let conn = db::open_memory().unwrap();
        register(&conn, "BC001", "LTO-6", Some("SER-1"), "available");
        let cart_id = lookup_cartridge(&conn, Some("SER-1"), None)
            .unwrap()
            .row
            .unwrap()
            .id;

        let vol1 = new_volume(&conn, "L6-0001");
        let row1 = lookup_cartridge(&conn, Some("SER-1"), None).unwrap();
        bind_cartridge(
            &conn,
            vol1,
            row1.row.as_ref(),
            Some("SER-1"),
            Generation::Lto6,
            2_500_000_000_000,
            &MamInfo {
                load_count: Some(9),
                ..MamInfo::default()
            },
        )
        .unwrap();

        let vol2 = new_volume(&conn, "L6-0002");
        let row2 = lookup_cartridge(&conn, Some("SER-1"), None).unwrap();
        bind_cartridge(
            &conn,
            vol2,
            row2.row.as_ref(),
            Some("SER-1"),
            Generation::Lto6,
            2_500_000_000_000,
            &MamInfo::default(), // this contact's read reported none
        )
        .unwrap();

        let loads: Option<i64> = conn
            .query_row(
                "SELECT total_load_count FROM cartridges WHERE id = ?1",
                params![cart_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            loads,
            Some(9),
            "a later contact with no readable load count must not erase a previously \
             observed value"
        );
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

    /// ── ADR-0012 corroboration, one test per branch (issue #193) ────────
    ///
    /// These drill [`corroborate_contact`] itself. The per-call-site tests
    /// live with their call sites and prove only that the contact CALLS
    /// this — "a contact that skips corroboration is the defect returning".
    mod corroboration {
        use super::*;
        use crate::volume::format::IdThunkMedia;

        /// A volume bound to one cartridge, with `serial` recorded on the
        /// row (or not) and `source` on the binding.
        fn bound(
            barcode: &str,
            serial: Option<&str>,
            source: Option<&str>,
        ) -> (Connection, i64, i64) {
            let conn = db::open_memory().unwrap();
            register(&conn, barcode, "LTO-6", serial, "in_use");
            let cart_id = conn.last_insert_rowid();
            let vol_id = new_volume(&conn, "L6-0001");
            conn.execute(
                "INSERT INTO cartridge_volumes (cartridge_id, volume_id, identity_source)
                 VALUES (?1, ?2, ?3)",
                params![cart_id, vol_id, source],
            )
            .unwrap();
            (conn, vol_id, cart_id)
        }

        fn claim(conn: &Connection, vol_id: i64) -> CatalogClaim {
            claim_for_volume(conn, vol_id, "L6-0001").unwrap()
        }

        fn row_serial(conn: &Connection, cart_id: i64) -> Option<String> {
            conn.query_row(
                "SELECT serial_number FROM cartridges WHERE id = ?1",
                params![cart_id],
                |r| r.get(0),
            )
            .unwrap()
        }

        /// File 0 attesting `serial` with the given identity source.
        fn file0(label: &str, serial: &str, source: Option<&str>) -> File0Facts {
            File0Facts {
                label: Some(label.to_string()),
                uuid: None,
                media: Some(IdThunkMedia {
                    cartridge_serial: serial.to_string(),
                    cartridge_identity_source: source.map(str::to_string),
                }),
            }
        }

        // ── Branch 1: both sides have a serial ──

        #[test]
        fn serials_that_agree_proceed() {
            let (conn, vol, _) = bound("BC001", Some("SER-1"), Some("mam"));
            let c = claim(&conn, vol);
            let out = corroborate_contact(
                &conn,
                Some(&c),
                &MediumFacts::from_serial(Some("SER-1".into())),
                None,
            )
            .unwrap();
            assert_eq!(out, Corroboration::Agreed);
        }

        #[test]
        fn serials_that_differ_are_refused_naming_both() {
            let (conn, vol, _) = bound("BC001", Some("SER-1"), Some("mam"));
            let c = claim(&conn, vol);
            let err = corroborate_contact(
                &conn,
                Some(&c),
                &MediumFacts::from_serial(Some("SER-2".into())),
                None,
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("wrong cartridge"), "{err}");
            assert!(
                err.contains("SER-1"),
                "must name what the catalog claims: {err}"
            );
            assert!(
                err.contains("SER-2"),
                "must name what the drive holds: {err}"
            );
            assert!(err.contains("BC001"), "must name the cartridge: {err}");
        }

        // ── Branch 2: the row learns a serial, ONCE ──

        #[test]
        fn a_row_with_no_serial_learns_it_once() {
            let (conn, vol, cart) = bound("BC001", None, Some("operator"));
            let out = corroborate_contact(
                &conn,
                Some(&claim(&conn, vol)),
                &MediumFacts::from_serial(Some("SER-1".into())),
                None,
            )
            .unwrap();
            assert_eq!(
                out,
                Corroboration::SerialLearned {
                    barcode: "BC001".into(),
                    serial: "SER-1".into()
                }
            );
            assert_eq!(row_serial(&conn, cart).as_deref(), Some("SER-1"));

            // NULL -> value happened; value -> value must not. A second
            // contact with the SAME serial is a plain agreement, and the
            // row is not rewritten (nothing to rewrite it to).
            let out2 = corroborate_contact(
                &conn,
                Some(&claim(&conn, vol)),
                &MediumFacts::from_serial(Some("SER-1".into())),
                None,
            )
            .unwrap();
            assert_eq!(out2, Corroboration::Agreed, "learning must happen once");
            assert_eq!(row_serial(&conn, cart).as_deref(), Some("SER-1"));

            // And a second contact with a DIFFERENT serial is now branch 1:
            // refused, never a silent value -> value rewrite.
            let err = corroborate_contact(
                &conn,
                Some(&claim(&conn, vol)),
                &MediumFacts::from_serial(Some("SER-9".into())),
                None,
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("wrong cartridge"), "{err}");
            assert_eq!(
                row_serial(&conn, cart).as_deref(),
                Some("SER-1"),
                "a refused contact must not have rewritten the serial"
            );
        }

        /// `bind_cartridge` writes `identity_source = 'mam'` exactly when
        /// it had a serial to record (migration 014), so `'mam'` beside a
        /// NULL `serial_number` is a row that HAD a chip serial and lost it
        /// — an inconsistent catalog, which
        /// `volume::write::resolve_cartridge_identity` refuses by name.
        ///
        /// Learning here would repair it from THIS contact's MAM read and
        /// make that guard unreachable, sealing File 0 with a serial taken
        /// from whichever drive wrote the tape — the source #192 rejected.
        #[test]
        fn a_mam_binding_that_lost_its_serial_is_not_repaired_from_the_drive() {
            let (conn, vol, cart) = bound("BC001", None, Some("mam"));
            let out = corroborate_contact(
                &conn,
                Some(&claim(&conn, vol)),
                &MediumFacts::from_serial(Some("SER-1".into())),
                None,
            )
            .unwrap();
            assert_eq!(out, Corroboration::Agreed, "an absence, not a repair");
            assert!(
                row_serial(&conn, cart).is_none(),
                "the write path's inconsistent-catalog refusal must stay reachable"
            );
        }

        // ── Branch 3: another row already holds that serial ──

        #[test]
        fn a_serial_another_row_already_holds_is_refused() {
            let (conn, vol, cart) = bound("BC001", None, Some("operator"));
            register(&conn, "BC002", "LTO-6", Some("SER-1"), "in_use");
            let err = corroborate_contact(
                &conn,
                Some(&claim(&conn, vol)),
                &MediumFacts::from_serial(Some("SER-1".into())),
                None,
            )
            .unwrap_err()
            .to_string();
            assert!(
                err.contains("BC002"),
                "must name the cartridge it really is: {err}"
            );
            assert!(
                err.contains("BC001"),
                "must name the one it was expected to be: {err}"
            );
            assert!(err.contains("SER-1"), "{err}");
            assert!(
                row_serial(&conn, cart).is_none(),
                "a refused contact must learn nothing"
            );
        }

        // ── Branch 4: the medium reports no serial ──

        #[test]
        fn no_medium_serial_with_a_cartridge_naming_the_bound_row_proceeds() {
            let (conn, vol, _) = bound("BC001", None, Some("operator"));
            let out = corroborate_contact(
                &conn,
                Some(&claim(&conn, vol)),
                &MediumFacts::from_serial(None),
                Some("BC001"),
            )
            .unwrap();
            assert_eq!(out, Corroboration::Agreed);
        }

        #[test]
        fn no_medium_serial_with_a_cartridge_naming_another_row_is_refused() {
            let (conn, vol, _) = bound("BC001", None, Some("operator"));
            let err = corroborate_contact(
                &conn,
                Some(&claim(&conn, vol)),
                &MediumFacts::from_serial(None),
                Some("BC002"),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("BC002"), "{err}");
            assert!(err.contains("BC001"), "{err}");
        }

        // ── The branch most likely to be got wrong ──

        /// ADR-0012: "A File 0 whose identity source is `operator` is
        /// corroborated through the catalog binding, not by comparing its
        /// recorded barcode to the row's — the row may since have been
        /// relabelled." `cartridge relabel` (issue #160) is legitimate, so a
        /// File 0 recording the OLD sticker must still corroborate.
        #[test]
        fn an_operator_file0_corroborates_through_the_binding_after_a_relabel() {
            let (conn, vol, cart) = bound("OLD-STICKER", None, Some("operator"));
            conn.execute(
                "UPDATE cartridges SET barcode = 'NEW-STICKER' WHERE id = ?1",
                params![cart],
            )
            .unwrap();

            // File 0 still records the barcode it was sealed with.
            let medium = MediumFacts::new(None, file0("L6-0001", "OLD-STICKER", Some("operator")));
            let out = corroborate_contact(&conn, Some(&claim(&conn, vol)), &medium, None).unwrap();
            assert_eq!(
                out,
                Corroboration::Agreed,
                "a relabelled cartridge must still corroborate: File 0 names the volume, \
                 the volume's binding names the cartridge"
            );
        }

        /// The same rule stated from the other side: an `operator` File 0 is
        /// never string-matched against anything, so even a barcode that
        /// matches NOTHING in the catalog is not a contradiction.
        #[test]
        fn an_operator_file0_barcode_is_never_string_matched() {
            let (conn, vol, _) = bound("BC001", Some("SER-1"), Some("mam"));
            let medium = MediumFacts::new(
                Some("SER-1".into()),
                file0("L6-0001", "SOME-OTHER-STICKER", Some("operator")),
            );
            corroborate_contact(&conn, Some(&claim(&conn, vol)), &medium, None)
                .expect("an operator-sourced barcode is corroborated through the binding");
        }

        /// A `mam` File 0 IS a chip serial, and contradicts a drive that
        /// reports a different one.
        #[test]
        fn a_mam_file0_contradicting_the_drive_is_refused() {
            let (conn, vol, _) = bound("BC001", None, None);
            let medium =
                MediumFacts::new(Some("SER-2".into()), file0("L6-0001", "SER-1", Some("mam")));
            let err = corroborate_contact(&conn, Some(&claim(&conn, vol)), &medium, None)
                .unwrap_err()
                .to_string();
            assert!(err.contains("SER-1"), "{err}");
            assert!(err.contains("SER-2"), "{err}");
        }

        /// `volume-format-v2.md` §1.1: an absent `cartridge_identity_source`
        /// means UNKNOWN and must never be read as `"mam"`. Every tape
        /// written before #192 omits it, and defaulting would make all of
        /// them falsely attest a chip-verified serial — and here, refuse.
        #[test]
        fn a_file0_with_no_identity_source_is_unknown_not_mam() {
            let (conn, vol, _) = bound("BC001", None, None);
            let medium = MediumFacts::new(Some("SER-2".into()), file0("L6-0001", "SER-1", None));
            corroborate_contact(&conn, Some(&claim(&conn, vol)), &medium, None)
                .expect("an unknown identity source contributes nothing, and proceeds");
        }

        // ── Absence is not contradiction ──

        /// The DR path: a tape whose volume row does not exist at all. This
        /// is `catalog rebuild` on a rebuilt machine, and it must proceed.
        #[test]
        fn no_catalog_claim_at_all_proceeds() {
            let conn = db::open_memory().unwrap();
            let medium = MediumFacts::new(
                Some("SER-1".into()),
                file0("SOME-TAPE", "SER-9", Some("mam")),
            );
            let out = corroborate_contact(&conn, None, &medium, Some("BC-ANY")).unwrap();
            assert_eq!(out, Corroboration::Agreed);
        }

        /// A check that cannot see cannot refuse — the rule
        /// `check_loaded_cartridge` introduced, preserved. Virtual drives
        /// that expose no medium serial keep working unchanged.
        #[test]
        fn an_unreadable_medium_serial_is_silent_rather_than_a_refusal() {
            let (conn, vol, _) = bound("BC001", Some("SER-1"), Some("mam"));
            corroborate_contact(
                &conn,
                Some(&claim(&conn, vol)),
                &MediumFacts::from_serial(None),
                None,
            )
            .unwrap();
        }

        #[test]
        fn an_unbound_volume_is_silent() {
            let conn = db::open_memory().unwrap();
            let vol = new_volume(&conn, "L6-0001");
            let c = claim_for_volume(&conn, vol, "L6-0001").unwrap();
            assert!(c.bound.is_none());
            corroborate_contact(
                &conn,
                Some(&c),
                &MediumFacts::from_serial(Some("SER-2".into())),
                None,
            )
            .unwrap();
        }

        #[test]
        fn an_unparseable_file0_is_an_absence() {
            let (conn, vol, _) = bound("BC001", Some("SER-1"), Some("mam"));
            let medium = MediumFacts::new(Some("SER-1".into()), File0Facts::default());
            corroborate_contact(&conn, Some(&claim(&conn, vol)), &medium, None).unwrap();
        }

        // ── File 0 names a different volume (issue #164) ──

        #[test]
        fn a_file0_naming_a_different_volume_is_refused() {
            let (conn, vol, _) = bound("BC001", Some("SER-1"), Some("mam"));
            let medium =
                MediumFacts::new(Some("SER-1".into()), file0("L6-0002", "SER-1", Some("mam")));
            let err = corroborate_contact(&conn, Some(&claim(&conn, vol)), &medium, None)
                .unwrap_err()
                .to_string();
            assert!(err.contains("wrong tape"), "{err}");
            assert!(
                err.contains("L6-0001"),
                "must name what was asked for: {err}"
            );
            assert!(err.contains("L6-0002"), "must name what was found: {err}");
        }

        /// `layout-session.md`: label AND uuid must match, so a label reused
        /// after a retire reads as a different volume.
        #[test]
        fn a_file0_with_the_same_label_but_a_different_uuid_is_refused() {
            let (conn, vol, _) = bound("BC001", Some("SER-1"), Some("mam"));
            conn.execute(
                "UPDATE volumes SET uuid = 'uuid-catalog' WHERE id = ?1",
                params![vol],
            )
            .unwrap();
            let mut f = file0("L6-0001", "SER-1", Some("mam"));
            f.uuid = Some("uuid-on-tape".into());
            let medium = MediumFacts::new(Some("SER-1".into()), f);
            let err = corroborate_contact(&conn, Some(&claim(&conn, vol)), &medium, None)
                .unwrap_err()
                .to_string();
            assert!(err.contains("uuid-catalog"), "{err}");
            assert!(err.contains("uuid-on-tape"), "{err}");
        }

        /// Structural, not asserted at runtime: the refusal takes no `force`
        /// of any kind, like its four siblings. Adding one fails to compile
        /// here first.
        #[test]
        fn corroboration_takes_no_force() {
            let f: fn(
                &Connection,
                Option<&CatalogClaim>,
                &MediumFacts,
                Option<&str>,
            ) -> Result<Corroboration> = corroborate_contact;
            let _ = f;
        }
    }
}
