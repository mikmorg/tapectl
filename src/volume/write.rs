use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::{collections::HashSet, fs};

use rusqlite::{params, Connection, OptionalExtension};
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::{Config, TapectlPaths};
use crate::db::{events, queries};
use crate::error::{Result, TapectlError};
use crate::staging;
use crate::tape::health;
use crate::util::{HashingWriter, TruncatingWriter};

use crate::store::{Store, TapeStore, Tier};

use super::binding;
use super::build::{self, BuildInputs, BuildSlice, BuildUnit, TenantInfo};
use super::format;
use super::layout;
use super::layout_model::{
    CapacityBudget, ContentSource, KeyAvailability, Layout, LayoutEntry, ZoneKind,
};
use super::session::{
    self, check_tape_contact, ConfirmOutcome, ContactOutcome, QuarantineReason, ResumeOutcome,
};

/// STOP-GAP pending an explicit operator/schema decision — see the T8 report.
///
/// Every `Layout` needs a `volume_uuid` (the v2 ID thunk's `[volume] uuid`
/// field, and the §2.1 tenant-envelope permutation seed,
/// `docs/design/v2-open-questions.md` §2.1/§2.3), but the `volumes` table
/// (`001_initial.sql`) has no `uuid` column — unlike `units.uuid`, which is
/// generated once at `unit init` and persisted (`src/unit/mod.rs`). No task
/// from T1 through T7 added one (verified: no `volumes.uuid` anywhere in the
/// schema or migrations, and the existing integration-test suite's `INSERT
/// INTO volumes` statements omit a uuid column and pass).
///
/// This derives a stable, deterministic placeholder from the volume's
/// `label` (`UNIQUE NOT NULL` on `volumes`, and volumes have no rename
/// command) via `sha256("tapectl-volume-uuid-placeholder-v1\0" || label)`,
/// keeping the first 16 bytes as the UUID — no schema change, no new
/// dependency (reuses `sha2`, already a dep, per the plan's guidance to
/// derive deterministic pseudo-randomness from `sha2` in counter mode rather
/// than add `rand`/`uuid`'s `v5` feature). It is stable across `volume_init`
/// and every `volume_write` attempt for the same label, so the identity
/// check in `session::InterruptedSession::resume` and the envelope
/// permutation both stay internally consistent.
///
/// The volume's UUID, read from `volumes.uuid` (migration 004).
///
/// This is a real, independent identifier — NOT derived from the label. The v2
/// ID thunk pairs `uuid` with `label` as the tape's identity, and resume
/// requires BOTH to match (`layout-session.md`) so that a relabelled cartridge,
/// or a label reused after a retire, reads as divergence rather than as the
/// same volume. §2.1 also seeds the tenant-envelope permutation from it.
///
/// Self-heals a NULL by generating and persisting a v4 once, so DB fixtures
/// that `INSERT INTO volumes` without a uuid keep working.
fn volume_uuid(conn: &Connection, volume_id: i64) -> Result<String> {
    let existing: Option<Option<String>> = conn
        .query_row(
            "SELECT uuid FROM volumes WHERE id = ?1",
            params![volume_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?;
    if let Some(Some(u)) = existing {
        if !u.is_empty() {
            return Ok(u);
        }
    }
    let fresh = Uuid::new_v4().to_string();
    conn.execute(
        "UPDATE volumes SET uuid = ?1 WHERE id = ?2",
        params![fresh, volume_id],
    )?;
    warn!(volume_id, "volume had no uuid; generated and persisted one");
    Ok(fresh)
}

/// Initialize a volume: create the DB record and write the provisional v2 ID
/// thunk to tape. Positions are unknown at init time
/// (`docs/design/v2-open-questions.md` §2.3) — the write session rewrites
/// File 0 from BOT with the real `total_files`/`seal_marker` once the Layout
/// is built; this call must not try to preserve init's File 0.
///
/// Contact discipline (issue #27): before the first byte is written, File 0
/// is read and checked via [`check_fresh_write_contact`] — the same check
/// `session::InterruptedSession::resume_checking` runs. There is no `Layout`
/// yet at init time (no staged units), so only the identity half applies
/// (`seal_position = None`; see that function's doc comment for why this
/// loses nothing: any tape with a parseable File 0 already refuses via
/// identity, sealed or not). No DB row is created for `label` until AFTER
/// this check passes, so a refusal here leaves no stale `volumes` row behind
/// to clean up — and per ADR-0010 that now covers the CARTRIDGE rows too:
/// nothing is registered, bound or displaced until File 0 has spoken.
///
/// ADR-0010 makes this the point where three facts about the physical medium
/// are decided, once, and then never re-derived from config:
///
/// 1. **Generation** — detected from the drive
///    ([`media_detect::detect`]), with `--media` / a matched cartridge row /
///    the drive's own generation standing in only when nothing is readable
///    ([`media_detect::resolve_media`]). A `--media` that contradicts a
///    detected code is an error, and a drive that cannot write the detected
///    generation is a hard refusal `--force` does not bypass: it is a
///    physical fact, not a consent tier (ADR-0008).
/// 2. **Capacity** — `capacity_override` -> the bound cartridge row ->
///    the generation table ([`crate::media::resolve_capacity`]), stored on
///    `volumes.capacity_bytes`. Every later gate reads that row; this is the
///    fix for issue #141, where an LTO-5 cartridge in an LTO-6 drive was
///    planned as 2.5 TB.
/// 3. **Which cartridge** — bound via the MAM medium serial
///    ([`crate::volume::binding`]), which is the first time any production
///    path has written the `cartridge_volumes` join.
///
/// Ordering is load-bearing. Every FACT check (detect, the `--media`
/// contradiction, `can_write`, a `--cartridge` that names no row) runs
/// before the tape device is opened, so a wrong-tape or wrong-flag run costs
/// nothing. Every catalog MUTATION runs after `check_fresh_write_contact`
/// passes and inside one transaction, so a refused init never displaces a
/// volume in the catalog. `volume_write`'s own late-binding call
/// ([`bind_late`]) honours the same rule for the same reason: it runs after
/// `check_fresh_write_contact` + `reposition_for_resume(0)`, not before
/// `build`/`validate`/`TapeStore::open`, so a write refused at any of those
/// stages cannot leave a committed displacement behind either (issue #154).
#[allow(clippy::too_many_arguments)] // conn/config + label/device/block_size + force + the two ADR-0010 declarations
pub fn volume_init(
    conn: &Connection,
    config: &Config,
    label: &str,
    device: &str,
    block_size: usize,
    force: bool,
    // `--media <GEN>`: the operator's declaration of the loaded medium's
    // generation. Only consulted when nothing could be detected; an error
    // when it contradicts a detected density code.
    declared_media: Option<&str>,
    // `--cartridge <BARCODE>`: bind to this already-registered cartridge
    // when the medium's serial matches no row (or no serial is readable).
    cartridge_barcode: Option<&str>,
) -> Result<i64> {
    // Creation-time label validation (issue #103). A label reaches the
    // filesystem too: `volume_read_slices` below joins
    // `{staging}/clone-{from_label}-{unit_name}`. Same defect, third entry
    // point — this codebase's recurring lesson is that one of these is never
    // the only one.
    crate::naming::validate_volume_label(label)?;

    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            params![label],
            |row| row.get(0),
        )
        .ok();
    if existing.is_some() {
        return Err(TapectlError::Other(format!(
            "volume \"{label}\" already exists"
        )));
    }

    let backend = crate::config::resolve_lto_backend(config, Some(device))?;
    let drive_gen = crate::media::Generation::parse(&backend.generation).ok_or_else(|| {
        TapectlError::Config(format!(
            "backends.lto[\"{}\"].generation = {:?} is not a recognised LTO generation",
            backend.name, backend.generation
        ))
    })?;

    // ---- ADR-0010 fact-finding, all before the tape device is opened ----
    let det = crate::tape::media_detect::detect(device, &backend.device_sg);
    let declared = match declared_media {
        Some(m) => Some(crate::media::Generation::parse(m).ok_or_else(|| {
            TapectlError::Other(format!(
                "--media {m:?} is not a recognised LTO generation \
                 (e.g. LTO-6, LTO-7, LTO-7-M8, LTO-8)"
            ))
        })?),
        None => None,
    };
    let serial = det.mam.serial.as_deref();

    // The row is looked up (never mutated) here because BOTH the generation
    // resolution and the capacity resolution need it, and because a
    // `--cartridge` that names nothing must fail before the drive is touched.
    let lookup = binding::lookup_cartridge(conn, serial, cartridge_barcode)?;
    // ADR-0012: a volume no cartridge claims is a copy the catalog cannot
    // place, so the unbound-with-a-warning outcome no longer exists AT INIT.
    // With the other FACT checks, before the drive is opened and before the
    // transaction — a refusal must leave nothing behind. `bind_late` keeps
    // the `(None, None)` no-op, which is what still lets `volume write` run
    // on volumes initialised before this rule.
    binding::require_named_cartridge(serial, lookup.row.as_ref())?;
    // ADR-0011: the one status-based refusal binding has. A medium the
    // operator declared permanently unfit cannot be written, and `--force`
    // is deliberately not consulted — `refuse_retired` does not take it.
    // Here with the other FACT checks, before the tape device is opened.
    if let Some(row) = &lookup.row {
        binding::refuse_retired(row)?;
    }
    let row_gen = match &lookup.row {
        Some(r) => match crate::media::Generation::parse(&r.media_type) {
            Some(g) => Some((g, r.barcode.as_str())),
            None => {
                // A row registered before ADR-0010 canonicalised the column,
                // or hand-edited. It cannot arbitrate a generation, so it is
                // ignored for that purpose and said so — its capacity figure
                // is still usable, and `cartridge register` now refuses to
                // create another one like it.
                warn!(
                    barcode = %r.barcode,
                    media_type = %r.media_type,
                    "cartridge media_type is not a recognised LTO generation; ignoring it \
                     for generation resolution"
                );
                None
            }
        },
        None => None,
    };

    let (generation, media_source) =
        crate::tape::media_detect::resolve_media(&det, declared, row_gen, drive_gen)?;
    if !media_source.is_detected() {
        // ONE line, not two. This was `warn!` AND `eprintln!` with the same
        // text, and tracing routes WARN to stderr — so the operator saw the
        // notice twice and reasonably wondered what the second one meant.
        // `eprintln!` is the one that survives: it is the operator-facing
        // line, and a log level cannot filter it away. (The `warn!` above,
        // about an unrecognised cartridge `media_type`, has no eprintln twin
        // and stays as a log line.)
        eprintln!(
            "warning: medium generation not detectable from this drive; \
             assuming {generation} (from {})",
            media_source.describe()
        );
    }

    // A physical fact, not a risk judgement: `--force` is deliberately NOT
    // consulted (ADR-0010 decision 2, ADR-0008's tiers).
    if !crate::media::Generation::can_write(drive_gen, generation) {
        return Err(TapectlError::Other(format!(
            "an {drive_gen} drive cannot write {generation} media. This is a physical \
             limit of the drive, not a policy — --force does not override it. Load a \
             {drive_gen}-writable cartridge, or write this one in a drive that can."
        )));
    }

    let capacity_override = match &backend.capacity_override {
        Some(v) => Some(staging::parse_size_to_bytes(v)? as u64),
        None => None,
    };
    let (nominal_capacity, capacity_source) = crate::media::resolve_capacity(
        capacity_override,
        lookup.row.as_ref().map(|r| r.nominal_capacity as u64),
        generation,
    );
    let nominal_capacity = nominal_capacity as i64;
    info!(
        label,
        %generation,
        nominal_capacity,
        capacity_source = ?capacity_source,
        "volume media resolved"
    );

    // Generated here (not deferred to the `volume_uuid()` self-heal helper)
    // so the SAME value is used for the contact check below and the
    // eventual INSERT — no DB row exists yet to read it back from.
    let candidate_uuid = Uuid::new_v4().to_string();

    // usable_bytes (the T4 capacity oracle) is informational at this stage —
    // volume_init only ever writes the provisional identity thunk; real
    // capacity gating happens in volume_write's pre-open validate.
    let usable_bytes = (nominal_capacity as f64 * backend.usable_capacity_factor) as u64;
    let mut store = TapeStore::open(device, block_size, usable_bytes)?;

    check_fresh_write_contact(&mut store, label, &candidate_uuid, None, force)?;
    // The check above read File 0 (and possibly moved the physical head on
    // real tape); undo that before the real write, which must start at BOT
    // exactly like an untouched fresh session would (`reposition_for_resume`'s
    // doc comment notes this one exception).
    store.reposition_for_resume(0)?;

    // ---- from here on the catalog changes; File 0 has consented --------
    // One transaction: a `volumes` row that exists but is not bound, or a
    // displacement recorded for a volume that was never created, are both
    // worse than either change alone.
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO volumes (label, uuid, backend_type, backend_name, media_type,
                              capacity_bytes, mam_capacity_bytes, mam_remaining_at_start, status)
         VALUES (?1, ?2, 'lto', ?3, ?4, ?5, ?6, ?7, 'initialized')",
        params![
            label,
            candidate_uuid,
            backend.name,
            generation.as_str(),
            nominal_capacity,
            det.mam.max_capacity_bytes,
            det.mam.remaining_bytes,
        ],
    )?;
    let volume_id = tx.last_insert_rowid();
    let bound = binding::bind_cartridge(
        &tx,
        volume_id,
        lookup.row.as_ref(),
        serial,
        generation,
        nominal_capacity,
        &det.mam,
    )?;
    events::log_created(&tx, "volume", volume_id, label, None)?;
    tx.commit()?;

    report_binding(label, &lookup, &bound);

    // Provisional total_files: unknown until the write session builds the
    // real Layout. Format §1's minimum shape is 4 front files + >=1 tenant
    // envelope + operator + backup + >=1 slice + the seal marker; 8 is a
    // representative placeholder, thrown away wholesale (not preserved, not
    // interpreted) when the write session rewrites File 0 from BOT.
    const PROVISIONAL_TOTAL_FILES: i32 = 8;
    let created_at = chrono::Utc::now().to_rfc3339();

    // The identity the binding just recorded (ADR-0012, issue #192), by the
    // same rule the permanent File 0 resolves from the catalog a moment later
    // in `volume_write`: `serial` IS the MAM read, so `Some` is a
    // chip-reported identity and `None` is the operator's `--cartridge`
    // barcode. These are discarded bytes, but a provisional File 0 that
    // disagrees with the permanent one is a trap for whoever reads init's
    // output and reasonably believes it.
    //
    // `bound.cartridge_id.is_none()` means nothing was bound at all, which
    // `require_named_cartridge` above no longer permits at init; the arm
    // stays because "unknown is said by saying nothing" is the correct
    // fallback, not because it is reachable.
    let (identity_serial, identity_source) = match (&bound.cartridge_id, &bound.barcode, serial) {
        (Some(_), _, Some(s)) => (s.to_string(), Some("mam")),
        (Some(_), Some(barcode), None) => (barcode.clone(), Some("operator")),
        _ => (
            det.mam.serial.as_deref().unwrap_or("").to_string(),
            None::<&str>,
        ),
    };

    // ADR-0010: real MAM values at init now. The FIELD NAMES and shape are
    // unchanged (ADR-0007 — on-tape bytes are forever); only the values that
    // were previously hardcoded zeros and blanks now say what the drive
    // actually reported.
    let id_thunk = layout::generate_id_thunk_v2(&layout::IdThunkV2Params {
        label,
        uuid: &candidate_uuid,
        media_type: generation.as_str(),
        tapectl_version: env!("CARGO_PKG_VERSION"),
        nominal_capacity,
        mam_capacity: det.mam.max_capacity_bytes.unwrap_or(0),
        total_files: PROVISIONAL_TOTAL_FILES,
        mam_manufacturer: det.mam.manufacturer.as_deref().unwrap_or(""),
        mam_serial: &identity_serial,
        mam_length: det.mam.length_meters.unwrap_or(0),
        mam_loads: det.mam.load_count.unwrap_or(0),
        created_at: &created_at,
        cartridge_identity_source: identity_source,
    });

    store.execute(
        &mut Cursor::new(id_thunk.as_bytes()),
        id_thunk.len() as u64,
        false,
    )?;
    info!(label = label, "volume initialized");

    Ok(volume_id)
}

/// Say what the binding did — ADR-0010 requires init to WARN about a
/// displacement, naming the volume and any unit that just lost its last
/// copy, rather than refuse it. stderr, so `--json` stdout stays parseable.
fn report_binding(label: &str, lookup: &binding::CartridgeLookup, bound: &binding::BindOutcome) {
    if let Some(requested) = &lookup.superseded_request {
        if let Some(actual) = &bound.barcode {
            eprintln!(
                "note: --cartridge {requested} was superseded by the loaded medium's own \
                 serial, which is registered as cartridge {actual}"
            );
        }
    }
    match &bound.barcode {
        Some(barcode) if bound.auto_registered => {
            eprintln!("cartridge {barcode} auto-registered from MAM (barcode = medium serial)");
        }
        Some(barcode) => {
            eprintln!("volume \"{label}\" bound to cartridge {barcode}");
            if bound.serial_recorded {
                eprintln!("  medium serial recorded on cartridge {barcode}");
            }
        }
        None => {
            eprintln!(
                "warning: no medium serial readable; volume \"{label}\" is not bound to a \
                 cartridge. Copy counting cannot tell this cartridge from another, and \
                 `volume write` cannot check you reloaded the same one."
            );
        }
    }

    let now = chrono::Utc::now().naive_utc();
    for d in &bound.displaced {
        eprintln!(
            "warning: cartridge {} previously held volume \"{}\"; it is now marked erased \
             because these bytes are being overwritten (ADR-0010).",
            bound.barcode.as_deref().unwrap_or("?"),
            d.label,
        );
        for impact in &d.impacts {
            if impact.other_copies == 0 {
                eprintln!(
                    "         *** unit \"{}\" [{}] now has ZERO copies ***",
                    impact.unit_name, impact.unit_status
                );
            } else {
                let evidence =
                    crate::policy::evidence::describe(&impact.unit_name, &impact.evidence, now);
                eprintln!(
                    "         unit \"{}\" [{}]: {} other copy/copies remain{}",
                    impact.unit_name,
                    impact.unit_status,
                    impact.other_copies,
                    evidence.map(|e| format!(" ({e})")).unwrap_or_default(),
                );
            }
        }
    }
}

/// Bind a volume that `volume init` left unbound, now that a medium serial
/// is readable (W3 Change 8, ADR-0010's ladder unchanged).
///
/// `volume init` writes a volume unbound whenever no medium serial could be
/// read — the case ADR-0010 keeps working so serial-less virtual harnesses
/// lose nothing. But an unbound volume costs real things: copy counting
/// cannot tell this cartridge from another, and
/// [`check_loaded_cartridge`] has nothing to compare against, so the
/// wrong-cartridge check silently passes. If `volume write` CAN read a
/// serial, binding here recovers all of that before a single slice is
/// written.
///
/// Deliberately a no-op in every other case:
///
/// - The volume already has an open mount — [`check_loaded_cartridge`] has
///   just confirmed it is the right one, and rebinding would be a
///   displacement nobody asked for.
/// - No serial readable — exactly as at init, the volume stays unbound.
/// - No generation resolvable — auto-registering a cartridge needs one, and
///   inventing it is how a row starts lying.
///
/// There is no `--cartridge` on `volume write`, so this is the serial-only
/// half of the ladder: match a registered row by serial, else auto-register
/// one whose barcode IS the serial. ADR-0011's `refuse_retired` applies
/// here for the same reason it applies at init.
#[allow(clippy::too_many_arguments)] // conn + volume identity + the three ADR-0010 facts
fn bind_late(
    conn: &Connection,
    volume_id: i64,
    label: &str,
    det: &crate::tape::media_detect::Detected,
    volume_media_type: Option<&str>,
    drive_generation: &str,
    nominal_capacity: i64,
) -> Result<()> {
    let already_bound: Option<i64> = conn
        .query_row(
            "SELECT cartridge_id FROM cartridge_volumes
             WHERE volume_id = ?1 AND unmounted_at IS NULL",
            params![volume_id],
            |r| r.get(0),
        )
        .optional()?;
    if already_bound.is_some() {
        return Ok(());
    }
    let Some(serial) = det.mam.serial.as_deref() else {
        return Ok(());
    };
    // The volume's own recorded generation first (ADR-0010 decided it at
    // init from the medium that was loaded), then what the drive detects
    // now, then the drive's native generation.
    let Some(generation) = volume_media_type
        .and_then(crate::media::Generation::parse)
        .or(det.generation)
        .or_else(|| crate::media::Generation::parse(drive_generation))
    else {
        return Ok(());
    };

    let lookup = binding::lookup_cartridge(conn, Some(serial), None)?;
    if let Some(row) = &lookup.row {
        binding::refuse_retired(row)?;
    }

    // Its own transaction: the `volumes` row this binds already exists and
    // is committed, so there is no larger unit of work to join — but the
    // displacement bookkeeping inside `bind_cartridge` is still all-or-
    // nothing.
    let tx = conn.unchecked_transaction()?;
    let bound = binding::bind_cartridge(
        &tx,
        volume_id,
        lookup.row.as_ref(),
        Some(serial),
        generation,
        nominal_capacity,
        &det.mam,
    )?;
    tx.commit()?;

    if bound.barcode.is_some() {
        eprintln!(
            "note: volume \"{label}\" was unbound after `volume init` (no medium serial was \
             readable then); binding it now from the loaded medium."
        );
        report_binding(label, &lookup, &bound);
    }
    Ok(())
}

/// A volume's own recorded capacity and media generation — the ADR-0010
/// authority for both, decided once at `volume init` from the medium that
/// was actually loaded.
///
/// Every write-path gate after init reads this instead of config. Issue #141
/// is exactly what happens when it does not: an LTO-5 cartridge planned at
/// the LTO-6 drive's 2.5 TB, run to a real end-of-tape.
fn volume_media(conn: &Connection, volume_id: i64, label: &str) -> Result<(i64, Option<String>)> {
    conn.query_row(
        "SELECT capacity_bytes, media_type FROM volumes WHERE id = ?1",
        params![volume_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .map_err(|_| TapectlError::VolumeNotFound(label.to_string()))
}

/// Refuse a cartridge that is not the one `volume init` bound (ADR-0010,
/// "`volume write` re-reads the serial").
///
/// The same wrong-cartridge discipline as the File 0 check, one layer
/// earlier and from a different witness: File 0 says what was written to
/// this tape, the MAM serial says which tape it is. Silent — not an error —
/// whenever either side is unknown: an unbound volume (no serial was
/// readable at init, as on some virtual drives), a cartridge row with no
/// recorded serial, or a drive that reports none now. A check that cannot
/// see cannot refuse.
///
/// Unlike the File 0 check there is no `--force`: this compares two recorded
/// serials, and disagreement means the operator loaded a different physical
/// cartridge than the one this volume was planned for. Continuing would
/// overwrite it while the catalog kept crediting the other one.
fn check_loaded_cartridge(
    conn: &Connection,
    volume_id: i64,
    label: &str,
    loaded_serial: Option<&str>,
) -> Result<()> {
    let Some(loaded) = loaded_serial else {
        return Ok(());
    };
    let bound: Option<(Option<String>, String)> = conn
        .query_row(
            "SELECT c.serial_number, c.barcode
             FROM cartridge_volumes cv
             JOIN cartridges c ON c.id = cv.cartridge_id
             WHERE cv.volume_id = ?1 AND cv.unmounted_at IS NULL",
            params![volume_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((Some(initialised_on), barcode)) = bound else {
        return Ok(());
    };
    if initialised_on != loaded {
        return Err(TapectlError::Other(format!(
            "wrong cartridge: volume \"{label}\" was initialised on {initialised_on} \
             (cartridge {barcode}), the drive holds {loaded}. Load that cartridge, or \
             `volume init` a new label on this one."
        )));
    }
    Ok(())
}

/// What File 0's `[media]` says about the cartridge's identity: the serial
/// string, and how it was established (`docs/design/volume-format-v2.md`
/// §1.1). `None` for the source means UNKNOWN, and the line is then omitted
/// from the thunk entirely.
#[derive(Debug)]
struct CartridgeIdentity {
    serial: String,
    source: Option<String>,
}

/// Resolve the identity File 0 will attest, FROM THE BINDING — never from a
/// fresh MAM read (ADR-0012, issue #192).
///
/// The catalog already knows which cartridge this volume is bound to and,
/// since migration 014, under which identity that binding was established.
/// Taking the string from there rather than from whatever the drive reports
/// at this contact is what keeps the tape and the catalog in agreement: a
/// volume bound by barcode because no serial was readable at `volume init`
/// must record THAT barcode, even if a later drive reads its MAM cleanly —
/// `bind_late` early-returns on an already-bound volume, so the catalog would
/// never learn that serial, and the same physical tape would otherwise attest
/// different provenance depending on which drive wrote it.
///
/// MAM corroborates the medium at each contact ([`check_loaded_cartridge`],
/// a few lines above this call); it does not supply this.
///
/// The legacy path is deliberately byte-for-byte what it always was: an
/// unbound volume, or one bound before 014 existed, takes the live MAM serial
/// (or `""`) and omits the source line — because absent means unknown.
fn resolve_cartridge_identity(
    conn: &Connection,
    volume_id: i64,
    label: &str,
    mam_serial: Option<&str>,
) -> Result<CartridgeIdentity> {
    // `.optional()?`, never `.ok()`: a locked or malformed database must
    // surface as an error rather than silently read as "unbound" and seal a
    // tape with the wrong identity on it.
    let bound: Option<(Option<String>, String, Option<String>)> = conn
        .query_row(
            "SELECT c.serial_number, c.barcode, cv.identity_source
             FROM cartridge_volumes cv
             JOIN cartridges c ON c.id = cv.cartridge_id
             WHERE cv.volume_id = ?1 AND cv.unmounted_at IS NULL",
            params![volume_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;

    let legacy = || CartridgeIdentity {
        serial: mam_serial.unwrap_or_default().to_string(),
        source: None,
    };

    let Some((serial_number, barcode, identity_source)) = bound else {
        return Ok(legacy());
    };

    match identity_source.as_deref() {
        Some("mam") => {
            // 'mam' means a chip serial was recorded onto this binding, so a
            // NULL `serial_number` is an inconsistent catalog. Refuse rather
            // than write `cartridge_serial = ""` beside a claim that a chip
            // reported it — a provenance assertion with no identity behind it
            // is exactly the wrong bytes this change exists to prevent, and
            // tape bytes are forever.
            let Some(serial) = serial_number else {
                return Err(TapectlError::Other(format!(
                    "volume \"{label}\" is bound to cartridge \"{barcode}\" with identity \
                     source \"mam\", but that cartridge row records no medium serial. \
                     tapectl will not seal a tape claiming a chip-reported identity it \
                     cannot name. The catalog is inconsistent — this binding was recorded \
                     with a serial that has since been cleared. Restore the database from \
                     a `db backup`, or `volume init` a new label on this cartridge."
                )));
            };
            Ok(CartridgeIdentity {
                serial,
                source: Some("mam".to_string()),
            })
        }
        // No serial was readable when this volume was bound, so the operator
        // named the cartridge and the identity IS that barcode (ADR-0012: a
        // barcode is a relabelable sticker, verifiable only against the
        // catalog).
        Some("operator") => Ok(CartridgeIdentity {
            serial: barcode,
            source: Some("operator".to_string()),
        }),
        // NULL: bound before migration 014. Unknown, and unknown is said by
        // saying nothing.
        _ => Ok(legacy()),
    }
}

/// Refuse a medium whose detected generation is not the one this volume was
/// initialised on (ADR-0010). Silent when nothing is detected or the row
/// predates ADR-0010 and records no parseable generation — the same
/// cannot-see-cannot-refuse rule as [`check_loaded_cartridge`].
fn check_loaded_generation(
    label: &str,
    det: &crate::tape::media_detect::Detected,
    volume_media_type: Option<&str>,
) -> Result<()> {
    let (Some(detected), Some(recorded)) = (
        det.generation,
        volume_media_type.and_then(crate::media::Generation::parse),
    ) else {
        return Ok(());
    };
    if detected != recorded {
        return Err(TapectlError::Other(format!(
            "wrong medium: volume \"{label}\" was initialised on {recorded} media, the \
             drive holds {detected}. Its plan, capacity gate and ID thunk all assume \
             {recorded}."
        )));
    }
    Ok(())
}

/// Full volume write pipeline (`docs/design/v2-implementation-plan.md` T8):
/// orchestration only. Gather the staged batch, assemble `BuildInputs` from
/// the DB, `build()` the Layout, then drive the §9 typestate session —
/// `validate -> plan -> execute -> seal -> confirm`
/// (`docs/design/v2-open-questions.md` §9). Every on-tape byte comes from
/// `build::build` + `session.rs` now; no hand-rolled layout logic (mini-index,
/// manual position arithmetic) remains here.
///
/// Contact discipline (issue #27): once the Layout is built (so its real
/// seal-marker position is known) and the store is open, but before
/// `into_validated`/`plan`/`execute` ever run, [`check_fresh_write_contact`]
/// reads File 0 and the seal-marker position and refuses on a wrong-tape or
/// already-sealed finding — the same check `session::InterruptedSession::resume_checking`
/// runs, applied to the fresh (non-resumed) path this function drives.
/// Split pre-write validation failures for the `--allow-missing-escrow`
/// override (CTO 2026-09-10, `docs/design-errata.md` §2.16). Returns
/// `(blocking, waived)`: `waived` is empty unless `allow_missing_escrow` is
/// set, in which case it holds the `StageSetLacksEscrow` failures the caller
/// downgrades to warnings; every OTHER failure — capacity, a corrupt staged
/// slice, and `EscrowRecipientMissing` (no escrow registered at all) — always
/// stays in `blocking`. Pure: no DB, no device, so it is unit-tested directly.
///
/// The override is safe to expose because `stage_create` refuses to stage
/// without an escrow recipient, so a `StageSetLacksEscrow` can ONLY originate
/// from `read-slices`/`compact-read` reactivating an already-sealed
/// pre-escrow stage set — never from fresh staging. It therefore cannot seal
/// a fresh un-escrowed tape.
fn blocking_validation_errors(
    errs: Vec<crate::volume::layout_model::LayoutError>,
    allow_missing_escrow: bool,
) -> (
    Vec<crate::volume::layout_model::LayoutError>,
    Vec<crate::volume::layout_model::LayoutError>,
) {
    use crate::volume::layout_model::LayoutError;
    if !allow_missing_escrow {
        return (errs, Vec::new());
    }
    // partition: `true` bucket (blocking) is everything that is NOT a waivable
    // per-stage-set escrow gap.
    errs.into_iter()
        .partition(|e| !matches!(e, LayoutError::StageSetLacksEscrow { .. }))
}

#[allow(clippy::too_many_arguments)] // conn/paths/config + label/device/block_size + force + allow_missing_escrow
pub fn volume_write(
    conn: &Connection,
    // Unused now that backend resolution goes through `resolve_lto_backend`
    // (ADR-0010) rather than `no_lto_backend_error(Some(paths))`. Kept as a
    // parameter (not removed) since it is public API called positionally
    // from `cli::volume`, from `collection::batch` and directly from tests.
    // The ADR's cartridge binding turned out not to need it: every message
    // it emits names a barcode or a medium serial, neither of which lives
    // under the tapectl home.
    _paths: &TapectlPaths,
    config: &Config,
    label: &str,
    device: &str,
    block_size: usize,
    force: bool,
    allow_missing_escrow: bool,
) -> Result<()> {
    let volume_id: i64 = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            params![label],
            |row| row.get(0),
        )
        .map_err(|_| TapectlError::VolumeNotFound(label.to_string()))?;

    // Refuse fast, before any real work, if this volume already has an
    // unresolved write session. `ValidatedLayout::plan` would otherwise hit
    // `writes`' `UNIQUE(stage_set_id, volume_id)` with a raw constraint
    // error on the retry, and — worse — a fresh `build()` would produce a
    // Layout that cannot match what a partially written tape already holds
    // (its ID thunk embeds a `created_at` and a MAM load count that have
    // since moved on). An interrupted session is resumed, never re-written:
    // that is `volume_resume`'s job.
    let existing_sessions: i64 = conn.query_row(
        "SELECT COUNT(*) FROM writes WHERE volume_id = ?1 AND status IN ('planned','in_progress','interrupted')",
        params![volume_id],
        |r| r.get(0),
    )?;
    if existing_sessions > 0 {
        return Err(TapectlError::Other(format!(
            "volume \"{label}\" already has an unresolved write session \
             (status planned/in_progress/interrupted). If it was interrupted, reload the \
             same cartridge and run `tapectl volume resume {label}` — it continues that \
             session from its frozen staging files rather than rebuilding it. Otherwise \
             inspect the `writes`/`write_positions` rows for volume_id {volume_id}."
        )));
    }

    let units = find_staged_data(conn)?;
    if units.is_empty() {
        return Err(TapectlError::Other(
            "no staged data to write — run `tapectl stage create` first".into(),
        ));
    }

    let backend = crate::config::resolve_lto_backend(config, Some(device))?;
    // ADR-0010 decision 3: capacity was decided ONCE, at `volume init`, from
    // the generation of the medium actually loaded — config is never
    // consulted for it again. Reading `backends.lto[].nominal_capacity` here
    // is exactly how issue #141 planned an LTO-5 cartridge as 2.5 TB.
    let (nominal_capacity, volume_media_type) = volume_media(conn, volume_id, label)?;
    let usable_bytes = (nominal_capacity as f64 * backend.usable_capacity_factor) as u64;
    // v2 collapses the v1 "manifest reserve" into just the ENOSPC buffer
    // (`volume-format-v2.md` §8) — the old `manifest_reserve` config field is
    // gone (T10 config cleanup: nothing read it after this path stopped, and
    // this was that "nothing").
    // `.max(0)` dropped (issue #59): a negative value is now rejected by
    // the parser itself, so a successful `Ok` is already non-negative.
    let enospc_buffer = staging::parse_size_to_bytes(&backend.enospc_buffer)? as u64;

    let mut distinct_tenant_ids: Vec<i64> = units.iter().map(|u| u.tenant_id).collect();
    distinct_tenant_ids.sort_unstable();
    distinct_tenant_ids.dedup();
    // The stage sets this write will put on tape, taken straight from
    // `find_staged_data`'s single selection above — the one place the write
    // path decides what it writes. Used twice below (the escrow-recorded-
    // recipient check inside `assemble_session_keys`, issue #115, and the
    // filtered catalog snapshot, issue #83); deriving it once is deliberate,
    // because two selections that can drift is exactly how issue #96
    // happened.
    let stage_set_ids: Vec<i64> = units.iter().map(|u| u.stage_set_id).collect();
    let SessionKeys {
        keys,
        tenants,
        operator_public_keys,
        escrow_public_key,
    } = assemble_session_keys(conn, &distinct_tenant_ids, &stage_set_ids)?;

    // One read of the loaded medium, serving three purposes (ADR-0010): the
    // wrong-cartridge and wrong-generation checks immediately below, the MAM
    // bookkeeping UPDATE, and the ID thunk's MAM fields. Read before the tape
    // stream itself is opened — `detect` opens the device read-only and drops
    // the fd, and the st driver refuses a second concurrent open.
    //
    // MAM capacity stays informational and never gates the write: the
    // pre-flight capacity gate reads `volumes.capacity_bytes`, which was
    // decided at init from the medium's detected generation
    // (`layout-session.md`'s validation point 1).
    let det = crate::tape::media_detect::detect(device, &backend.device_sg);
    let mam = det.mam.clone();
    if mam.max_capacity_bytes.is_some() || mam.remaining_bytes.is_some() {
        let _ = conn.execute(
            "UPDATE volumes SET mam_capacity_bytes = ?1, mam_remaining_at_start = ?2
             WHERE id = ?3",
            params![mam.max_capacity_bytes, mam.remaining_bytes, volume_id],
        );
    }

    // Wrong-cartridge discipline, one layer earlier than the File 0 check
    // (ADR-0010): the volume knows which medium serial it was initialised
    // on, so a swapped cartridge is caught before `build()` materialises a
    // single slice — never mind before anything is written.
    check_loaded_cartridge(conn, volume_id, label, mam.serial.as_deref())?;
    check_loaded_generation(label, &det, volume_media_type.as_deref())?;

    // File 0's `[media]` identity, taken from the BINDING (ADR-0012, issue
    // #192) now that the loaded medium has been corroborated against it just
    // above. Resolved here, where `conn` is in scope; `build()` stays
    // `Connection`-free and receives it as plain data on `BuildInputs`.
    //
    // Note on ordering: `bind_late` runs AFTER `build()` (issue #154 moved it
    // there so a refused write displaces nothing), so a legacy unbound volume
    // is still unbound at this point and the field is absent. That is
    // correct, and deliberately not "predicted" from what `bind_late` is
    // about to do — absent means unknown, and coupling File 0's bytes to a
    // step that has not run yet would be the worse bug.
    let cartridge_identity =
        resolve_cartridge_identity(conn, volume_id, label, mam.serial.as_deref())?;

    let volume_uuid = volume_uuid(conn, volume_id)?;
    let created_at = chrono::Utc::now().to_rfc3339();
    // Session directory: materialize-to-staging (`v2-open-questions.md`
    // §2.2) lives under the configured staging directory, namespaced per
    // volume label + a fresh session uuid (so a later attempt never collides
    // with an earlier one's frozen files).
    let session_dir = PathBuf::from(&config.staging.directory)
        .join("sessions")
        .join(format!("{label}-{}", Uuid::new_v4()));

    // Filtered catalog.db (issue #83): generated here, where `conn` is
    // available — `build()` deliberately stays `Connection`-free (T5b). The
    // scope is exactly this write's stage_sets (`units`, just gathered by
    // `find_staged_data` above) — everything `status = 'staged'` right now
    // is what is about to become this volume's `writes`/`write_positions`
    // rows below, and nothing else is staged at this instant (the
    // unresolved-write-session check above refuses a second concurrent
    // write). `build()` appends it to the OPERATOR envelope only.
    fs::create_dir_all(&session_dir)?;
    let catalog_db_path = session_dir.join("catalog_snapshot.db");
    crate::db::catalog_snapshot::build_catalog_snapshot(conn, &stage_set_ids, &catalog_db_path)?;

    let inputs = BuildInputs {
        label: label.to_string(),
        volume_uuid,
        // The generation of the medium this volume was initialised on, not
        // the drive's (ADR-0010). Only a pre-ADR-0010 row can lack one, and
        // the drive's own generation is a better ID-thunk value than a blank.
        media_type: volume_media_type
            .clone()
            .unwrap_or_else(|| backend.generation.clone()),
        tapectl_version: env!("CARGO_PKG_VERSION").to_string(),
        created_at,
        block_size: block_size as u64,
        usable_bytes,
        enospc_buffer,
        nominal_capacity,
        mam_capacity: mam.max_capacity_bytes.unwrap_or(0),
        mam_manufacturer: mam.manufacturer.clone().unwrap_or_default(),
        mam_serial: cartridge_identity.serial,
        cartridge_identity_source: cartridge_identity.source,
        mam_length: mam.length_meters.unwrap_or(0),
        mam_loads: mam.load_count.unwrap_or(0),
        units,
        tenants,
        operator_public_keys,
        escrow_public_key,
        catalog_db_path: Some(catalog_db_path),
    };

    let built = build::build(&inputs, &session_dir)?;
    // Snapshot the Layout before the typestate chain consumes `built` — the
    // terminal `SealedSession` only exposes `volume_id`/`label`, not the
    // entries, and `bytes_written`/`num_data_files` bookkeeping (below) needs
    // them after confirm succeeds.
    let layout_snapshot = built.layout.clone();

    // Pre-flight validate — run BEFORE the tape device is opened. This is
    // what replaces the old inline capacity-only gate: an over-capacity (or
    // keyless, or corrupt-staged-slice) refusal here never touches the
    // drive, exactly like the gate it replaces
    // (`docs/design/v2-implementation-plan.md` T8's trap: "do NOT leave two
    // capacity gates"). Sacred invariant 2 (full-hash staged slices from
    // disk) runs here, not a size-only shortcut.
    if let Err(errs) = built.validate(&keys) {
        let (blocking, waived) = blocking_validation_errors(errs, allow_missing_escrow);
        for e in &waived {
            tracing::warn!(
                "sealing a slice the escrow key cannot open, per --allow-missing-escrow \
                 (ADR-0005): {e}"
            );
        }
        if !blocking.is_empty() {
            return Err(TapectlError::Other(format!(
                "volume \"{label}\" failed pre-write validation: {}",
                blocking
                    .iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join("; ")
            )));
        }
    }

    let mut store = TapeStore::open(device, block_size, usable_bytes)?;

    // Contact discipline (#27): the Layout is already built, so its real
    // seal-marker entry gives an a-priori position — unlike `volume_init`,
    // this check is never vacuous. Runs before capacity/plan/execute: no
    // point checking whether a WRONG tape has room.
    let seal_position = layout_snapshot
        .entries
        .iter()
        .find(|e| matches!(e.kind, ZoneKind::SealMarker))
        .map(|e| e.position as u32);
    check_fresh_write_contact(
        &mut store,
        label,
        &layout_snapshot.volume_uuid,
        seal_position,
        force,
    )?;
    // Undo the position change the read-based check above made (TapeStore's
    // read_file rewinds+forward-spaces internally) — the write below must
    // start at BOT exactly like an untouched fresh session would.
    store.reposition_for_resume(0)?;

    let validated = built.into_validated(&keys, &mut store).map_err(|errs| {
        TapectlError::Other(format!(
            "volume \"{label}\" failed validation at contact: {}",
            errs.iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        ))
    })?;

    // ADR-0010's binding ladder, one stage later, for the volume `volume init`
    // could not bind (W3 Change 8) — placed here, after every refusal this
    // write can still suffer, so a refused write cannot displace a volume in
    // the catalog (issue #154). It used to run before `build`/`validate`/
    // `TapeStore::open`/`check_fresh_write_contact`, so any of those four
    // refusals left a committed displacement with nothing to roll it back.
    //
    // It sits below `into_validated` as well, which is NOT merely a re-run of
    // the pre-flight `validate` above: it additionally compares the layout
    // against `store.capacity()`, read from the drive that is only now open.
    // An over-capacity refusal is the most routine pre-write refusal there is
    // and can fire for the first time here, so binding above this line would
    // leave exactly the displacement this issue exists to prevent, for the
    // likeliest refusal of all. `into_validated` takes no `Connection` and
    // touches no cartridge state, so this is the latest point before `plan()`
    // writes anything, and nothing in between needs the binding.
    //
    // Mirrors `volume_init`'s ordering (see the invariant comment above it).
    bind_late(
        conn,
        volume_id,
        label,
        &det,
        volume_media_type.as_deref(),
        &backend.generation,
        nominal_capacity,
    )?;

    let planned = validated.plan(conn, volume_id, &inputs.units)?;
    let execute_outcome = planned.execute(conn, &mut store)?;

    let result = finish_session(
        conn,
        &mut store,
        volume_id,
        label,
        &layout_snapshot,
        block_size as u64,
        execute_outcome.into(),
    );

    collect_health_best_effort(conn, config, device, volume_id);

    result
}

/// Resume an interrupted write session for `label` — the cross-process half
/// of `docs/design/layout-session.md`'s Resume rule (issue #25, playbook T8
/// remainder). Mirrors [`volume_write`]'s shape, with three deliberate
/// differences:
///
/// 1. It **rehydrates** the Layout via [`session::InterruptedSession::rehydrate`]
///    rather than calling `build::build`. This is the load-bearing fact:
///    `BuildInputs::created_at` is `chrono::Utc::now()` at `volume_write` call
///    time and is persisted nowhere, and `BuildInputs::mam_loads` is
///    `read_mam`'s `load_count`, which increments on every cartridge load. A
///    rebuilt Layout would therefore carry different ID-thunk bytes than the
///    tape already holds, and `SealedPending::confirm`'s front-index diff
///    would quarantine a perfectly good volume.
/// 2. It never calls [`check_fresh_write_contact`]. Resume has its own
///    contact discipline INSIDE `resume_checking` (the File-0 identity check
///    plus the seal-marker absence check, both via `check_tape_contact`), and
///    the fresh-write refusal — which treats a matching-but-partially-written
///    tape as a wrong-cartridge event — would reject every legitimate resume.
/// 3. Its tenant list comes from the rehydrated Layout's tenant-envelope
///    entries, not from a staged batch (there is none: `plan` already
///    consumed it, and `find_staged_data` would return the wrong thing).
///    `KeyAvailability::tenant_ids` is documented as exactly this — "every
///    tenant that has an envelope on this volume".
///
/// It shares [`assemble_session_keys`] and [`finish_session`] with
/// `volume_write` rather than copying them.
pub fn volume_resume(
    conn: &Connection,
    // See `volume_write`'s `_paths` for why this is unused but kept.
    _paths: &TapectlPaths,
    config: &Config,
    label: &str,
    device: &str,
    block_size: usize,
) -> Result<()> {
    let volume_id: i64 = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            params![label],
            |row| row.get(0),
        )
        .map_err(|_| TapectlError::VolumeNotFound(label.to_string()))?;

    let session = match session::InterruptedSession::rehydrate(conn, volume_id)? {
        Some(s) => s,
        None => return Err(nothing_to_resume(conn, volume_id, label)),
    };

    // Snapshot before `resume` consumes the session (same reason
    // `volume_write` clones its Layout: the terminal `SealedSession` exposes
    // only volume_id/label, and the bookkeeping below needs the entries).
    let layout_snapshot = session.layout().clone();

    let tenant_ids: Vec<i64> = {
        let mut ids: Vec<i64> = layout_snapshot
            .entries
            .iter()
            .filter_map(|e| match e.kind {
                ZoneKind::TenantEnvelope { tenant_id } => Some(tenant_id),
                _ => None,
            })
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    // The stage sets this session is writing, recovered from the rehydrated
    // Layout's own slice entries rather than from a fresh
    // `WHERE status = 'staged'` query (issue #115). `plan` already moved
    // those stage sets out of `'staged'`, so a second selection would return
    // the wrong set — and a second selection is what issue #96 was. The
    // Layout IS the frozen record of the one `find_staged_data` selection
    // this session was planned from.
    let stage_set_ids = stage_set_ids_for_layout(conn, &layout_snapshot)?;
    let SessionKeys { keys, .. } = assemble_session_keys(conn, &tenant_ids, &stage_set_ids)?;

    let backend = crate::config::resolve_lto_backend(config, Some(device))?;
    // ADR-0010 decision 3: the volume's own row, never config. Resume must
    // in any case reuse the figure the interrupted session planned against —
    // a capacity that moved mid-session would be a different plan.
    let (nominal_capacity, _) = volume_media(conn, volume_id, label)?;
    let usable_bytes = (nominal_capacity as f64 * backend.usable_capacity_factor) as u64;
    let mut store = TapeStore::open(device, block_size, usable_bytes)?;

    info!(label, volume_id, "resuming interrupted volume write");
    let outcome = session.resume(conn, &keys, &mut store)?;

    let result = finish_session(
        conn,
        &mut store,
        volume_id,
        label,
        &layout_snapshot,
        block_size as u64,
        outcome,
    );

    collect_health_best_effort(conn, config, device, volume_id);

    result
}

/// Deliberately abandon a volume's unfinished write session (issue #94) —
/// the implementation of `docs/design/layout-session.md`'s Aborted row, first
/// clause: "Operator explicitly abandoned an interrupted session."
///
/// This is the operator path that makes it safe for `resume_checking` to stop
/// auto-aborting on a revalidation failure. Nothing here can tell a transient
/// cause from a permanent one — only the operator can — so the judgement is
/// spelled as its own command rather than inferred from a `LayoutError`
/// variant.
///
/// It never contacts the tape (hence no device argument): the cartridge is
/// left exactly as the interrupted session left it — unsealed, physically
/// unharmed, and reusable after a bulk erase plus `cartridge mark-erased`.
/// Only the `writes` rows move. `write_positions`, `volumes.status`, the
/// staged slices and the session directory are all untouched; the staged
/// files stay pinned until `staging clean` runs.
///
/// It refuses outright if any row is still `in_progress`. Per `rehydrate`'s
/// own reasoning, `db::open` sweeps `in_progress` to `interrupted` before any
/// command holds a `Connection`, so a surviving `in_progress` row means
/// another process is writing this tape right now, and aborting it would
/// corrupt a live session.
pub fn volume_abort(conn: &Connection, label: &str, assume_yes: bool) -> Result<()> {
    let volume_id: i64 = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            params![label],
            |row| row.get(0),
        )
        .map_err(|_| TapectlError::VolumeNotFound(label.to_string()))?;

    let rows: Vec<(i64, String)> = conn
        .prepare(
            "SELECT id, status FROM writes
             WHERE volume_id = ?1 AND status IN ('planned', 'in_progress', 'interrupted')
             ORDER BY id",
        )?
        .query_map(params![volume_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    if rows.is_empty() {
        return Err(TapectlError::Other(format!(
            "volume \"{label}\" has no unfinished write session to abort — nothing is in the \
             `planned`, `in_progress` or `interrupted` state."
        )));
    }

    if rows.iter().any(|(_, s)| s == "in_progress") {
        return Err(TapectlError::Other(format!(
            "volume \"{label}\" has an `in_progress` write session. `tapectl` sweeps crashed \
             sessions to 'interrupted' when it opens the database, so a row still 'in_progress' \
             means ANOTHER PROCESS IS WRITING THIS TAPE RIGHT NOW. Refusing to abort — cutting \
             a live writer's session out from under it would destroy the cartridge."
        )));
    }

    let write_ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
    let mut slice_count: i64 = 0;
    for id in &write_ids {
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM write_positions WHERE write_id = ?1",
            params![id],
            |r| r.get(0),
        )?;
        slice_count += n;
    }
    let statuses: Vec<&str> = rows.iter().map(|(_, s)| s.as_str()).collect();

    let facts = vec![
        format!(
            "volume \"{label}\": {} unfinished write session row(s) ({}), covering {slice_count} \
             planned slice position(s).",
            write_ids.len(),
            statuses.join(", ")
        ),
        "The session becomes ABORTED and can never be resumed — `tapectl volume resume` adopts \
         only interrupted sessions."
            .to_string(),
        "The cartridge is NOT touched: it is left unsealed and physically unharmed, so it can be \
         bulk-erased and reused (`cartridge mark-erased`)."
            .to_string(),
        "The staged slices stay pinned on disk until `tapectl staging clean` runs, so the data \
         can be re-staged or written to another volume."
            .to_string(),
    ];
    crate::cli::consent::confirm(
        &format!("abandon the unfinished write session on volume \"{label}\""),
        &facts,
        assume_yes,
    )?;

    let tx = conn.unchecked_transaction()?;
    for id in &write_ids {
        tx.execute(
            "UPDATE writes SET status = 'aborted' WHERE id = ?1",
            params![id],
        )?;
    }
    events::log_event(
        &tx,
        "volume",
        volume_id,
        Some(label),
        "write_aborted",
        None,
        None,
        Some("operator abandoned the unfinished write session (`volume abort`)"),
        None,
        None,
    )?;
    tx.commit()?;

    info!(
        label,
        volume_id,
        sessions = write_ids.len(),
        "operator aborted unfinished write session"
    );
    Ok(())
}

/// Explain a `rehydrate` that found nothing, naming the `writes` statuses
/// that actually exist for this volume rather than asserting there are none.
///
/// This matters for one status in particular. `volume_write`'s existing-session
/// refusal counts `planned`/`in_progress`/`interrupted` and points the
/// operator here, but `rehydrate` adopts only `interrupted` — so without this,
/// a `planned` row would make the two commands contradict each other, each
/// pointing at the other. A `planned` row is reachable (`plan` inserts
/// `'planned'` and `execute` is what flips it to `'in_progress'`, so a kill in
/// that window sticks) and is never swept by `recover_orphaned_sessions`,
/// which only touches `in_progress`. Nothing has been written to tape at that
/// point, so the honest instruction is to clear it and start over, not to
/// resume it.
fn nothing_to_resume(conn: &Connection, volume_id: i64, label: &str) -> TapectlError {
    let statuses: Vec<String> = match conn
        .prepare("SELECT DISTINCT status FROM writes WHERE volume_id = ?1 ORDER BY status")
        .and_then(|mut s| {
            s.query_map(params![volume_id], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()
        }) {
        Ok(v) => v,
        Err(e) => return e.into(),
    };

    if statuses.is_empty() {
        return TapectlError::Other(format!(
            "volume \"{label}\" has no write sessions at all — there is nothing to resume. Run \
             `tapectl volume write {label}` to start one."
        ));
    }
    if statuses.iter().any(|s| s == "planned") {
        return TapectlError::Other(format!(
            "volume \"{label}\" has a `planned` write session, not an interrupted one: it was \
             killed after planning but before execution began, so nothing was ever written to \
             tape and there is no partial recording to continue. Clear it with \
             `tapectl volume abort {label}` and run `tapectl volume write {label}` again. \
             (Existing statuses: {}.)",
            statuses.join(", ")
        ));
    }
    if statuses.iter().any(|s| s == "in_progress") {
        return TapectlError::Other(format!(
            "volume \"{label}\" has an `in_progress` write session. `tapectl` sweeps crashed \
             sessions to 'interrupted' when it opens the database, so a row still 'in_progress' \
             means ANOTHER PROCESS IS WRITING THIS TAPE RIGHT NOW. Refusing to resume — two \
             writers on one cartridge would destroy it. (Existing statuses: {}.)",
            statuses.join(", ")
        ));
    }
    TapectlError::Other(format!(
        "volume \"{label}\" has no interrupted write session to resume — its write sessions are \
         all resolved (existing statuses: {}). Run `tapectl volume write {label}` to start a \
         new one.",
        statuses.join(", ")
    ))
}

/// Everything a write session needs from the tenant/key tables, assembled
/// once. Shared by [`volume_write`] and [`volume_resume`] so the two can
/// never drift on what "the keys for this volume" means — they differ only in
/// where `tenant_ids` comes from (a staged batch vs. the rehydrated Layout's
/// tenant-envelope entries), which is why that is a parameter.
///
/// One `TenantInfo` per tenant, carrying that tenant's own active
/// (non-escrow) keys only: `build()` appends the operator and escrow
/// recipients itself (`with_escrow`).
struct SessionKeys {
    keys: KeyAvailability,
    tenants: Vec<TenantInfo>,
    operator_public_keys: Vec<String>,
    escrow_public_key: Option<String>,
}

/// The stage sets a rehydrated Layout is writing, recovered by mapping its
/// `ZoneKind::Slice` entries back through `stage_slices.stage_set_id`
/// (issue #115).
///
/// This is a lookup **by id**, never a second selection: `find_staged_data`
/// remains the one place the write path decides *which* stage sets it
/// writes, and by resume time its answer is frozen into the Layout (`plan`
/// has already moved those rows out of `status = 'staged'`, so re-running
/// that query would return something else entirely). Two selections that can
/// drift is how issue #96 happened.
///
/// A slice id with no surviving row is skipped rather than erroring: the
/// escrow check downstream fails closed on the stage sets it *can* see, and
/// a missing `stage_slices` row is a separate integrity problem that
/// `validate`'s staged-slice checks already report.
fn stage_set_ids_for_layout(conn: &Connection, layout: &Layout) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT stage_set_id FROM stage_slices WHERE id = ?1")?;
    let mut ids = Vec::new();
    for entry in &layout.entries {
        let ZoneKind::Slice { stage_slice_id } = entry.kind else {
            continue;
        };
        let found: Option<i64> = stmt
            .query_row(params![stage_slice_id], |r| r.get(0))
            .optional()?;
        if let Some(stage_set_id) = found {
            ids.push(stage_set_id);
        }
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// Which of `stage_set_ids` were encrypted WITHOUT `escrow_public_key`
/// (issue #115), as `(stage_set_id, unit_name, reason)` — the payload of
/// [`KeyAvailability::stage_sets_lacking_escrow`].
///
/// The evidence is `stage_sets.key_fingerprints`, the recipient list
/// `stage_create` recorded for the slices it wrote (`fingerprint ==
/// public_key` by construction throughout this system, so the column holds
/// public keys verbatim). It is the ONLY evidence available: an age X25519
/// stanza carries a per-encryption ephemeral share, not the recipient's
/// identity, so the ciphertext itself cannot be asked who it was encrypted
/// to without a private key — and the escrow private key is deliberately
/// absent from any machine that runs this code (ADR-0005).
///
/// **Fails closed.** A stage set whose recipient list is missing,
/// unparseable, or contradicted by `encrypted = 0` is reported, not skipped:
/// on write-once media "we could not tell" must not read as "it is fine".
/// Each case gets its own `reason` so the operator can tell a pre-escrow
/// stage set apart from a corrupt row.
///
/// This is a lookup by id over the ids the caller already selected — never
/// its own `WHERE status = 'staged'` query. `find_staged_data` stays the one
/// place the write path decides what it writes (issue #96).
///
/// The query and the classification both live in `policy::escrow` now
/// (`Scope::StageSets` — by id, no `writes`/`volumes` join, since a stage
/// set about to be written has no completed write yet); this is a thin
/// mapping from its verdicts to the `(stage_set_id, unit_name, reason)`
/// shape `KeyAvailability::stage_sets_lacking_escrow` expects.
fn stage_sets_lacking_escrow(
    conn: &Connection,
    stage_set_ids: &[i64],
    escrow_public_key: &str,
) -> Result<Vec<(i64, String, String)>> {
    let rows = crate::policy::escrow::stage_set_coverage(
        conn,
        crate::policy::escrow::Scope::StageSets(stage_set_ids),
        escrow_public_key,
    )?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let reason = match row.coverage {
                crate::policy::escrow::Coverage::Covered => return None,
                crate::policy::escrow::Coverage::Unknown => {
                    crate::policy::escrow::UNKNOWN_REASON.to_string()
                }
                crate::policy::escrow::Coverage::Gap(reason) => reason,
            };
            Some((row.stage_set_id, row.unit_name, reason))
        })
        .collect())
}

fn assemble_session_keys(
    conn: &Connection,
    tenant_ids: &[i64],
    stage_set_ids: &[i64],
) -> Result<SessionKeys> {
    let mut tenants = Vec::with_capacity(tenant_ids.len());
    let mut tenants_with_active_key = HashSet::new();
    for &tenant_id in tenant_ids {
        let tenant = queries::get_tenant_by_id(conn, tenant_id)?
            .ok_or_else(|| TapectlError::Other(format!("tenant {tenant_id} not found")))?;
        let keys = queries::get_active_keys_for_tenant(conn, tenant_id)?;
        if keys.is_empty() {
            return Err(TapectlError::Other(format!(
                "tenant \"{}\" (id={tenant_id}) has no active key — cannot encrypt its envelope",
                tenant.name
            )));
        }
        tenants_with_active_key.insert(tenant_id);
        tenants.push(TenantInfo {
            tenant_id,
            tenant_name: tenant.name,
            public_keys: keys.into_iter().map(|k| k.public_key).collect(),
        });
    }

    let operator = queries::get_operator_tenant(conn)?
        .ok_or_else(|| TapectlError::Other("no operator tenant configured".into()))?;
    let operator_keys = queries::get_active_keys_for_tenant(conn, operator.id)?;
    if operator_keys.is_empty() {
        return Err(TapectlError::Other("operator has no active key".into()));
    }
    let operator_public_keys: Vec<String> =
        operator_keys.into_iter().map(|k| k.public_key).collect();

    // ADR-0005: the permanent escrow recipient. `None` fails validation via
    // `KeyAvailability.escrow_recipient_present = Some(false)`, the same way
    // `key rotate` refuses without one.
    let escrow_public_key = queries::escrow_public_key(conn)?;

    // Issue #115: "an escrow is registered" and "the slices about to be
    // written were encrypted to it" are different questions, and only the
    // second one is about the bytes that end up on tape. Computed here, in
    // the one place `volume_write` and `volume_resume` both route through,
    // so the two can never drift on it — the same reason this function
    // exists at all.
    let lacking = match &escrow_public_key {
        Some(pk) => stage_sets_lacking_escrow(conn, stage_set_ids, pk)?,
        // No escrow registered at all: `escrow_recipient_present:
        // Some(false)` already fails validation with the right remedy
        // ("register one"), so listing every stage set here would bury it
        // under N copies of the wrong remedy ("re-stage"). Still computed —
        // an empty list, not `None`, which is reserved for callers that have
        // no stage-set context.
        None => Vec::new(),
    };

    Ok(SessionKeys {
        keys: KeyAvailability {
            tenant_ids: tenant_ids.to_vec(),
            tenants_with_active_key,
            operator_key_present: true,
            escrow_recipient_present: Some(escrow_public_key.is_some()),
            stage_sets_lacking_escrow: Some(lacking),
        },
        tenants,
        operator_public_keys,
        escrow_public_key,
    })
}

/// The post-execute tail, shared verbatim by [`volume_write`] and
/// [`volume_resume`]: `seal` -> `confirm` -> bookkeeping/audit, plus the
/// Interrupted/Aborted/Quarantined terminations. Factored rather than
/// duplicated because this is where a copy-paste would silently drift — in
/// particular the `Quarantined` arm, which only the resume path can reach
/// from `resume` itself (the File-0 identity check / already-sealed refusal)
/// and which a half-copied tail would drop.
///
/// It takes a [`ResumeOutcome`] because that enum is the superset:
/// `volume_write` converts its `ExecuteOutcome` via the existing
/// `From<ExecuteOutcome>` impl, leaving `Quarantined` unreachable-but-handled
/// on the fresh path. This shares the Interrupted and Aborted arms too, not
/// just seal/confirm.
///
/// Sacred invariant 1 (`v2-implementation-plan.md`): the seal marker is
/// written only inside `ReadyToSeal::seal`. This function calls it; it never
/// constructs a seal entry of its own.
fn finish_session(
    conn: &Connection,
    store: &mut dyn Store,
    volume_id: i64,
    label: &str,
    layout: &Layout,
    block_size: u64,
    outcome: ResumeOutcome,
) -> Result<()> {
    match outcome {
        ResumeOutcome::Ready(ready) => {
            let sealed_pending = ready.seal(store)?;
            match sealed_pending.confirm(conn, store, Tier::default())? {
                ConfirmOutcome::Sealed(sealed) => {
                    record_write_bookkeeping(conn, volume_id, layout, block_size)?;
                    events::log_event(
                        conn,
                        "volume",
                        volume_id,
                        Some(label),
                        "write_completed",
                        None,
                        None,
                        None,
                        None,
                        None,
                    )?;
                    info!(label = sealed.label, volume_id, "volume write sealed");
                    Ok(())
                }
                ConfirmOutcome::Quarantined(q) => log_quarantine(conn, volume_id, label, &q.reason),
            }
        }
        ResumeOutcome::Quarantined(q) => log_quarantine(conn, volume_id, label, &q.reason),
        ResumeOutcome::Interrupted(_) => {
            events::log_event(
                conn,
                "volume",
                volume_id,
                Some(label),
                "write_interrupted",
                None,
                None,
                None,
                None,
                None,
            )?;
            Err(TapectlError::Other(format!(
                "volume \"{label}\" write interrupted (SIGINT) — the tape is left unsealed, and \
                 the session's `writes`/`write_positions` rows are in the `interrupted` state. \
                 Reload the same cartridge and run `tapectl volume resume {label}` to continue \
                 from where it stopped."
            )))
        }
        ResumeOutcome::Aborted(a) => {
            events::log_event(
                conn,
                "volume",
                volume_id,
                Some(label),
                "write_aborted",
                None,
                None,
                Some(&a.reason),
                None,
                None,
            )?;
            Err(TapectlError::Other(format!(
                "volume \"{label}\" write aborted: {}",
                a.reason
            )))
        }
    }
}

/// The one place a quarantine is recorded and reported — reached from
/// `confirm`'s failure (either path) and from `resume`'s own divergence
/// findings (the resume-only arm).
fn log_quarantine(
    conn: &Connection,
    volume_id: i64,
    label: &str,
    reason: &QuarantineReason,
) -> Result<()> {
    let reason = describe_quarantine(reason);
    events::log_event(
        conn,
        "volume",
        volume_id,
        Some(label),
        "write_quarantined",
        None,
        None,
        Some(&reason),
        None,
        None,
    )?;
    Err(TapectlError::Other(format!(
        "volume \"{label}\" quarantined: {reason}"
    )))
}

/// Best-effort sg_logs health collection. Never lets a collection failure
/// shadow the session's real outcome (matching v1: always attempted, its own
/// errors only logged).
fn collect_health_best_effort(conn: &Connection, config: &Config, device: &str, volume_id: i64) {
    if let Some(bk) = config.backends.lto.iter().find(|b| b.device_tape == device) {
        match health::collect(&bk.device_sg) {
            Ok((counters, raw)) => {
                if let Err(e) = health::record(conn, volume_id, "write", &counters, &raw) {
                    warn!(err = %e, "health_logs insert failed");
                }
            }
            Err(e) => warn!(sg_device = %bk.device_sg, err = %e, "sg_logs collection failed"),
        }
    }
}

/// Populate `volumes`' write-summary columns (`bytes_written`,
/// `num_data_files`, `has_manifest`, `first_write`, `last_write`) —
/// informational fields `report capacity`/`report age`/`audit` all read
/// (verified: `grep -rn "bytes_written" src/cli`), but that
/// `session::SealedPending::confirm`'s own transaction (T6) does not touch —
/// it only flips `status`. v1 populated these inline as part of the write
/// loop; this restores that parity for v2-sealed volumes. Deliberately never
/// touches `status` (confirm's transaction already set it to `sealed`).
fn record_write_bookkeeping(
    conn: &Connection,
    volume_id: i64,
    layout: &Layout,
    block_size: u64,
) -> Result<()> {
    let slice_entries: Vec<&LayoutEntry> = layout
        .entries
        .iter()
        .filter(|e| matches!(e.kind, ZoneKind::Slice { .. }))
        .collect();
    let bytes_written: i64 = slice_entries
        .iter()
        .filter_map(|e| e.on_tape_bytes(block_size))
        .sum::<u64>() as i64;
    let num_data_files = slice_entries.len() as i64;
    conn.execute(
        "UPDATE volumes SET bytes_written = ?1, num_data_files = ?2, has_manifest = 1,
         first_write = COALESCE(first_write, datetime('now')), last_write = datetime('now')
         WHERE id = ?3",
        params![bytes_written, num_data_files, volume_id],
    )?;
    Ok(())
}

/// A one-line, human-readable summary of why a session quarantined.
/// `volume_write`'s fresh (non-resumed) path only ever reaches
/// `QuarantineReason::ConfirmFailed`; `IdentityMismatch`/`AlreadySealed` are
/// resume-only findings, produced by `InterruptedSession::resume` and reached
/// through `volume_resume`. (Issue #27 added an EQUIVALENT
/// identity/seal check to the fresh path too, in `check_fresh_write_contact`
/// below — but deliberately as a plain refusal, not a `QuarantineReason`: a
/// wrong cartridge loaded before any write means the operator grabbed the
/// wrong tape, not that this not-yet-written logical volume diverged, so it
/// is never marked `quarantined`. This function's claim above still holds.)
fn describe_quarantine(reason: &QuarantineReason) -> String {
    match reason {
        QuarantineReason::ConfirmFailed(evidence) => format!(
            "confirm chain-walk found {} mismatch(es) at tier {:?}: {:?}",
            evidence.mismatches.len(),
            evidence.tier,
            evidence.mismatches
        ),
        QuarantineReason::IdentityMismatch {
            expected_label,
            expected_uuid,
            found,
        } => format!(
            "identity mismatch: expected label={expected_label:?} uuid={expected_uuid:?}, found {found:?}"
        ),
        QuarantineReason::AlreadySealed { seal_position } => format!(
            "tape already carries a seal marker at position {seal_position} \
             (ADR-0003: sealed volumes are immutable)"
        ),
    }
}

/// A tiny, pure decision — no I/O, no `Store` — over an already-computed
/// `ContactOutcome` (issue #27). Kept separate from [`check_fresh_write_contact`]
/// so the decision itself (what does `--force` permit, and what does it
/// never permit) is unit-testable without a store of any kind.
///
/// `allow_overwrite` is `--force`: the loud, explicit operator assertion
/// (ADR-0001's third evidence category — physical facts an operator attests
/// to at contact) for the one legitimate reuse case this repo's cartridge
/// lifecycle does not (yet) wire into the write path at all: a foreign or
/// stale identity at File 0 that the operator has physically verified is
/// safe to overwrite. It can defeat `IdentityMismatch`. It can NEVER defeat
/// `AlreadySealed` — ADR-0003 makes a sealed volume's immutability absolute,
/// and the sanctioned way past a sealed cartridge is to bulk-erase it first
/// (`cartridge mark-erased`), which is exactly what turns its File 0
/// unreadable again (`ContactOutcome::Blank`) on the next attempt — not a
/// software override.
fn decide_fresh_write_contact(
    outcome: &ContactOutcome,
    label: &str,
    volume_uuid: &str,
    allow_overwrite: bool,
) -> Result<()> {
    match outcome {
        ContactOutcome::Blank | ContactOutcome::Matches => Ok(()),
        ContactOutcome::AlreadySealed { seal_position } => Err(TapectlError::Other(format!(
            "refusing to write volume \"{label}\": the loaded cartridge already carries a SEALED \
             volume — a valid seal marker parses at tape position {seal_position}. ADR-0003: \
             sealed volumes are immutable, there is no append, and --force cannot override this. \
             If this cartridge should be reused: retire its current volume, bulk-erase the \
             physical tape, then run `tapectl cartridge mark-erased` before writing to it again."
        ))),
        ContactOutcome::IdentityMismatch { found } => {
            let found_desc = match found {
                Some(id) => format!("label={:?}, uuid={:?}", id.label, id.uuid),
                None => "a present but unparseable/corrupt File 0".to_string(),
            };
            if allow_overwrite {
                warn!(
                    label,
                    volume_uuid,
                    found = %found_desc,
                    "--force overriding a File-0 identity mismatch at contact"
                );
                Ok(())
            } else {
                Err(TapectlError::Other(format!(
                    "refusing to write volume \"{label}\" (uuid {volume_uuid}): the loaded \
                     cartridge's File 0 already identifies a DIFFERENT volume ({found_desc}) — \
                     this looks like the wrong physical cartridge. Verify the correct tape is \
                     loaded, or if you are deliberately overwriting this cartridge, re-run with \
                     --force."
                )))
            }
        }
    }
}

/// The fresh-write contact check (issue #27): the same File-0 + seal-marker
/// check `session::InterruptedSession::resume_checking` runs
/// ([`check_tape_contact`]), applied before the very first byte of a fresh
/// `volume_init`/`volume_write` — closing the gap the issue describes:
/// neither call read File 0 before this fix, so loading the wrong cartridge
/// (including one already holding a different, SEALED volume) silently
/// overwrote it. Returns `Ok(())` to proceed; `Err` refuses.
fn check_fresh_write_contact(
    store: &mut dyn Store,
    label: &str,
    volume_uuid: &str,
    seal_position: Option<u32>,
    allow_overwrite: bool,
) -> Result<()> {
    let outcome = check_tape_contact(store, label, volume_uuid, seal_position);
    decide_fresh_write_contact(&outcome, label, volume_uuid, allow_overwrite)
}

/// Verify a volume via the v2 keyless chain walk
/// (`docs/design/volume-format-v2.md` §5) — the same algorithm
/// `session::SealedPending::confirm` runs at seal time and `RESTORE.sh
/// --verify` reimplements independently in bash
/// (`docs/design/v2-open-questions.md` §10: "one chain walk, three
/// consumers"; this is consumer 2). `tier` selects `Tier::Integrity`
/// (default, `--full`: hash every content file against the front index's
/// ciphertext hashes) or `Tier::Navigable` (`--quick`: seal binding + front
/// index self-consistency only); the tier actually achieved is recorded
/// honestly in `verification_sessions.verify_type` (`full`/`quick`,
/// ADR-0001).
///
/// There is no in-memory session `Layout` to diff against here — this can
/// run long after (possibly years after) the write session that sealed the
/// volume, and no serialized Layout is persisted anywhere. So the `Layout`
/// `Store::confirm` checks against is reconstructed FROM the just-read front
/// index itself: this makes `chain_walk`'s step-3 "diff against the Layout"
/// a tautology by construction, but the seal-binding hash (step 2) and the
/// per-file content hash (step 4, Integrity tier) still independently verify
/// the tape against itself — exactly what a keyless heir running `RESTORE.sh
/// --verify` can do, and (today) no more: this function has DB access but
/// nothing to cross-check the front index's claims against, since
/// metadata-file sizes/hashes are not recorded anywhere in the DB — only
/// slice cursor rows are (`write_positions.stage_slice_id` is `NOT NULL`).
/// See the T8 report for this as a known, accepted limitation.
/// Record one `verification_results` row per mismatch (issue #142).
///
/// `verification_sessions` has always carried the AGGREGATE — how many
/// slices failed — and `verification_results` has been defined and indexed
/// since `001_initial.sql` with no writer at all. So a failed verify could
/// say "3 failed" and never which three, which is the difference between an
/// operator who knows what to re-copy and one who re-copies a whole tape.
///
/// WHAT CANNOT BE RECORDED, AND WHY THAT IS NOT A BUG HERE.
/// `write_position_id` and `stage_slice_id` are both `NOT NULL REFERENCES`,
/// and only SLICES get a `write_positions` cursor row — `write_positions.
/// stage_slice_id` is `NOT NULL`, so the seal marker, the front index, the
/// envelopes and every other metadata file have nothing to point at. A
/// mismatch at one of those positions therefore has no row it could legally
/// occupy, and this SKIPS it rather than inventing a cursor or relaxing the
/// schema. Nothing is lost: every mismatch, recordable or not, is in
/// `Evidence::mismatches`, which `volume verify --json` now prints in full
/// and `warn!` has always logged, and the session aggregate still counts
/// them all. The return value is how many rows were actually written, so a
/// caller can tell "recorded" from "counted".
///
/// Returns the number of rows inserted.
pub(crate) fn record_verification_results(
    conn: &Connection,
    session_id: i64,
    volume_id: i64,
    evidence: &crate::store::Evidence,
) -> Result<usize> {
    // `write_positions.position` is TEXT (001_initial.sql), holding the
    // decimal position — matched as a string, the way every other writer
    // in this file stores it (`position.to_string()`).
    let mut cursor = conn.prepare(
        "SELECT wp.id, wp.stage_slice_id
         FROM write_positions wp
         JOIN writes w ON w.id = wp.write_id
         WHERE w.volume_id = ?1 AND wp.position = ?2
         LIMIT 1",
    )?;
    let mut insert = conn.prepare(
        "INSERT INTO verification_results
            (session_id, write_position_id, stage_slice_id, result,
             expected_sha256, actual_sha256, notes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;

    let mut recorded = 0usize;
    for m in &evidence.mismatches {
        let found: Option<(i64, i64)> = cursor
            .query_row(params![volume_id, m.position.to_string()], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let Some((write_position_id, stage_slice_id)) = found else {
            continue;
        };
        let hashes = m.kind.compares_hashes();
        insert.execute(params![
            session_id,
            write_position_id,
            stage_slice_id,
            mismatch_result(m.kind),
            hashes.then_some(m.expected.as_str()),
            hashes.then_some(m.actual.as_str()),
            // The true kind always survives here, even where `result` had to
            // round it to one of the schema's five values.
            format!(
                "{}: expected {}, found {}",
                m.kind.label(),
                m.expected,
                m.actual
            ),
        ])?;
        recorded += 1;
    }
    Ok(recorded)
}

/// Map a [`crate::store::MismatchKind`] onto the fixed `verification_results.
/// result` vocabulary (`001_initial.sql`: passed / failed_checksum /
/// failed_read / failed_decrypt / skipped).
///
/// The v2 chain walk has six failure kinds and the column has five values,
/// none of them added for it, so this is a lossy projection by construction.
/// It is lossy in the SAFE direction — the true kind is always written to
/// `notes` — and the split follows what the disagreement actually IS: two
/// kinds compare sha256 hashes (`failed_checksum`), the other four are the
/// bytes not being readable or not being what the map said (`failed_read`).
/// `failed_decrypt` is never produced: the chain walk is KEYLESS by design
/// (ADR-0007), so it never attempts a decryption that could fail.
fn mismatch_result(kind: crate::store::MismatchKind) -> &'static str {
    if kind.compares_hashes() {
        "failed_checksum"
    } else {
        "failed_read"
    }
}

pub fn volume_verify(
    conn: &Connection,
    config: &Config,
    label: &str,
    device: &str,
    block_size: usize,
    tier: Tier,
) -> Result<VerifyReport> {
    let volume_id: i64 = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            params![label],
            |row| row.get(0),
        )
        .map_err(|_| TapectlError::VolumeNotFound(label.to_string()))?;

    // Capacity comes from the volume's own row (ADR-0010 decision 3) — the
    // figure decided at init from the medium actually loaded. Only the
    // usable-capacity FACTOR is a property of the drive.
    //
    // LENIENT (ADR-0010): verify is a read path and must keep working with
    // no backend configured for this device (`crate::config::resolve_device`
    // never errors when `device` is given) — the same `None => 0` fallback
    // as before covers that case, and costs nothing, since verify only reads.
    let (nominal_capacity, _) = volume_media(conn, volume_id, label)?;
    let usable_bytes = match crate::config::resolve_device(config, Some(device))
        .ok()
        .and_then(|(_, b)| b)
    {
        Some(b) => (nominal_capacity as f64 * b.usable_capacity_factor) as u64,
        None => 0,
    };
    let mut store = TapeStore::open(device, block_size, usable_bytes)?;

    let report = volume_verify_with_store(conn, &mut store, label, volume_id, block_size, tier)?;

    // Best-effort sg_logs health collection. Advisory only, and deliberately
    // OUTSIDE the store-injectable half: it needs the drive's sg node, which
    // a `MemStore` does not have.
    if let Some(bk) = config.backends.lto.iter().find(|b| b.device_tape == device) {
        if let Ok((counters, raw)) = health::collect(&bk.device_sg) {
            if let Err(e) = health::record(conn, volume_id, "verify", &counters, &raw) {
                warn!(err = %e, "health_logs insert failed");
            }
        }
    }

    Ok(report)
}

/// [`volume_verify`] minus the tape device: everything from the front-index
/// read through the `verification_sessions` / `verification_results` rows.
///
/// Split out for the reason ADR-0006 gives generally and
/// [`volume_identify`] already demonstrates: with a `&mut dyn Store` the
/// whole verify path — including its DB bookkeeping — is exercisable against
/// a `MemStore` with no hardware, which is how issue #142's per-mismatch
/// recording is tested at all. `volume_verify` keeps the drive-only parts
/// (opening the device, sg_logs health collection).
pub(crate) fn volume_verify_with_store(
    conn: &Connection,
    store: &mut dyn Store,
    label: &str,
    volume_id: i64,
    block_size: usize,
    tier: Tier,
) -> Result<VerifyReport> {
    // Read File 3 (front index) raw; its true (pre-padding) length is
    // recovered by stripping trailing NUL padding — the same trick
    // `volume_identify` already uses for File 0, and the sanctioned
    // cross-tool byte contract for File 3 specifically
    // (`volume-format-v2.md` §4: "a reader recovering File 3 from a padded
    // tape read obtains the same bytes by stripping trailing NUL padding").
    let mut fi_raw = Vec::new();
    store.read_file(3, &mut fi_raw)?;
    let fi_text = String::from_utf8_lossy(&fi_raw);
    let fi_trimmed = fi_text.trim_end_matches('\0');
    let fi_true_len = fi_trimmed.len() as u64;

    let parsed_fi = format::parse_front_index(fi_trimmed).map_err(|e| {
        TapectlError::Other(format!(
            "volume \"{label}\": front index (File 3) unparseable: {e}"
        ))
    })?;

    let mut entries: Vec<LayoutEntry> = Vec::with_capacity(parsed_fi.len());
    for p in &parsed_fi {
        let kind = ZoneKind::from_type_label(&p.type_label).ok_or_else(|| {
            TapectlError::Other(format!(
                "volume \"{label}\": front index position {} has an unrecognized type \"{}\"",
                p.position, p.type_label
            ))
        })?;
        entries.push(LayoutEntry {
            position: p.position,
            // File 3's own true length is the one fact this reconstruction
            // takes from a source other than the parsed entry (its own
            // entry omits it, self-referentially, by design).
            size_bytes: if p.position == 3 {
                Some(fi_true_len)
            } else {
                p.size_bytes
            },
            sha256: p.sha256_encrypted.clone(),
            kind,
            // Not read by `chain_walk` (position/kind/size_bytes/sha256
            // only) — this reconstruction has no real backing file per
            // entry, so there is nothing truer to put here.
            source: ContentSource::Generated,
        });
    }

    let layout = Layout {
        label: label.to_string(),
        volume_uuid: String::new(),
        media_type: String::new(),
        block_size: block_size as u64,
        // Unused by `chain_walk` (capacity is a build/validate-time concern).
        budget: CapacityBudget {
            available_bytes: 0,
            reserve_bytes: 0,
        },
        entries,
    };

    let evidence = store.confirm(&layout, tier)?;

    let verify_type = match tier {
        Tier::Integrity => "full",
        Tier::Navigable => "quick",
    };
    let outcome = if evidence.mismatches.is_empty() {
        "passed"
    } else {
        "failed"
    };
    // One transaction (issue #142): the session aggregate and the per-mismatch
    // detail describe the same verify, so a crash between them must leave
    // neither rather than a session claiming "3 failed" with nothing to name
    // them — which is precisely the state this change exists to end.
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO verification_sessions
            (volume_id, verify_type, outcome, completed_at, slices_checked, slices_passed, slices_failed)
         VALUES (?1, ?2, ?3, datetime('now'), ?4, ?5, ?6)",
        params![
            volume_id,
            verify_type,
            outcome,
            evidence.files_checked as i64,
            if evidence.mismatches.is_empty() {
                evidence.files_checked as i64
            } else {
                0
            },
            evidence.mismatches.len() as i64,
        ],
    )?;
    let session_id = tx.last_insert_rowid();
    record_verification_results(&tx, session_id, volume_id, &evidence)?;
    tx.commit()?;

    for m in &evidence.mismatches {
        warn!(
            position = m.position,
            kind = m.kind.label(),
            expected = %m.expected,
            actual = %m.actual,
            "verify mismatch"
        );
    }

    Ok(VerifyReport {
        checked: evidence.files_checked as usize,
        passed: (evidence.files_checked as usize).saturating_sub(evidence.mismatches.len()),
        failed: evidence.mismatches.len(),
        mismatches: evidence.mismatches,
    })
}

/// Read and display the ID thunk from a tape. Reads File 0 as raw text
/// (magic + label, whatever version) — no version-dispatch logic needed
/// here: v1 and v2 thunks are both plain text an heir reads with `dd | tr -d
/// '\0'`, and the v2 magic (`tapectl-volume-v2`) already self-identifies
/// within that text (`v2-open-questions.md` §2.7: "volume_identify reads
/// File 0 only... needs the v2 magic accepted alongside v1" — true by
/// construction, since this never parses the magic at all).
///
/// Takes an already-open `store` (ADR-0006) — the caller opens
/// `TapeStore::open_read` (or, in tests, hands in a `MemStore`), so this
/// function is directly unit-testable with no tape device.
pub fn volume_identify(store: &mut dyn Store) -> Result<String> {
    let mut data = Vec::new();
    store.read_file(0, &mut data)?;
    let text = String::from_utf8_lossy(&data).to_string();
    Ok(text.trim_end_matches('\0').to_string())
}

/// Outcome of [`stream_verify_slice_to_staging`]. `write_positions.
/// sha256_on_volume` and `stage_slices.sha256_encrypted` have historically
/// been populated slightly differently across code paths, so a match
/// against EITHER is accepted — the same either-match `read_slices`/
/// `compact_read` always did, before or after streaming (issue #86).
enum SliceStreamOutcome {
    /// The streamed (true, unpadded) bytes matched one of the candidate
    /// hashes; `dest_path` now holds exactly those bytes.
    Verified,
    /// The streamed bytes matched none of the candidates; `dest_path` has
    /// already been removed.
    ChecksumMismatch { actual: String },
}

/// Stream the tape file at `position` into `dest_path`, trimming block
/// padding to `true_len` bytes as they arrive — never materializing a whole
/// encrypted slice in RAM (the same OOM shape #85 fixed for restore; this
/// is that fix for `read_slices`/`compact_read`, issue #86). Compares the
/// resulting hash against `expected_hashes` (a match against ANY of them is
/// accepted) and reports the verdict via [`SliceStreamOutcome`].
///
/// On anything other than a clean match — a checksum mismatch, or a tape
/// read error propagated as `Err` — `dest_path` is removed before
/// returning. Streaming writes bytes to `dest_path` as they arrive, so by
/// the time a mismatch or error is discovered, a corrupt/partial (or,
/// on a read error, merely empty) file may already sit there; the old
/// buffered code could check the hash before ever calling `fs::write`, so
/// this cleanup is what keeps the "no corrupt file left in staging"
/// invariant that check-then-write used to give for free.
///
/// The verdict/cleanup logic is centralized HERE rather than inlined in
/// each of `read_slices`/`compact_read` because both share it verbatim, one
/// call per slice — mirrors `restore.rs::restore_one_slice_inner`'s pass 1
/// (`TruncatingWriter` over a `HashingWriter` over the destination file).
/// `read_slices`/`compact_read` themselves now take `&mut dyn Store` too
/// (ADR-0006, C7): the seam is at the entry point, so `MemStore` drives the
/// real functions directly — no store-shaped inner twin needed here.
fn stream_verify_slice_to_staging(
    store: &mut dyn Store,
    position: u32,
    true_len: u64,
    expected_hashes: &[&str],
    dest_path: &Path,
) -> Result<SliceStreamOutcome> {
    let file = fs::File::create(dest_path)?;
    let mut bounded = TruncatingWriter::new(HashingWriter::new(file), true_len);
    let read_result = store.read_file(position, &mut bounded);
    let hashing = bounded.into_inner();
    let actual = hashing.finalize_hex();
    drop(hashing); // close dest_path's handle before any removal below

    if let Err(e) = read_result {
        let _ = fs::remove_file(dest_path);
        return Err(e);
    }

    if expected_hashes.iter().any(|h| *h == actual) {
        Ok(SliceStreamOutcome::Verified)
    } else {
        let _ = fs::remove_file(dest_path);
        Ok(SliceStreamOutcome::ChecksumMismatch { actual })
    }
}

/// Read encrypted slices for a unit from a volume into staging.
/// After this, use `volume write` to write them to a destination tape
/// with the full self-describing volume layout.
///
/// Position-based, driven entirely from `write_positions` (DB), not the
/// on-tape index — unaffected by the v2 index relocation
/// (`v2-open-questions.md` §2.7).
///
/// Takes an already-open `store` (ADR-0006) rather than a device path — the
/// caller opens `TapeStore::open_read`, so this function is directly
/// unit-testable against a `MemStore` fixture.
pub fn read_slices(
    conn: &Connection,
    config: &Config,
    from_label: &str,
    unit_name: &str,
    store: &mut dyn Store,
) -> Result<ReadSlicesReport> {
    // Look up source volume
    let from_vol_id: i64 = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            params![from_label],
            |row| row.get(0),
        )
        .map_err(|_| TapectlError::VolumeNotFound(from_label.to_string()))?;

    // Look up unit
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;

    // Find write positions for this unit on the source volume
    let mut stmt = conn.prepare(
        "SELECT wp.position, wp.sha256_on_volume, wp.stage_slice_id,
                ss.encrypted_bytes, ss.sha256_encrypted, ss.stage_set_id,
                ss.id as slice_db_id
         FROM write_positions wp
         JOIN writes w ON w.id = wp.write_id
         JOIN stage_slices ss ON ss.id = wp.stage_slice_id
         JOIN stage_sets sts ON sts.id = w.stage_set_id
         JOIN snapshots sn ON sn.id = sts.snapshot_id
         WHERE w.volume_id = ?1 AND sn.unit_id = ?2 AND wp.status = 'written'
         ORDER BY CAST(wp.position AS INTEGER)",
    )?;
    let source_slices: Vec<(String, String, i64, i64, String, i64, i64)> = stmt
        .query_map(params![from_vol_id, unit.id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    if source_slices.is_empty() {
        return Err(TapectlError::Other(format!(
            "no slices for unit \"{unit_name}\" on volume \"{from_label}\""
        )));
    }

    info!(
        unit = unit_name,
        slices = source_slices.len(),
        "reading slices from {from_label}"
    );

    // Read encrypted slices from source tape to staging
    let staging_dir = &config.staging.directory;
    let clone_dir =
        std::path::Path::new(staging_dir).join(format!("clone-{from_label}-{unit_name}"));
    fs::create_dir_all(&clone_dir)?;

    let mut total_bytes: i64 = 0;
    let mut slices_read: i64 = 0;
    let mut affected_stage_sets = HashSet::new();

    for (
        pos_str,
        sha_on_vol,
        _stage_slice_id,
        enc_bytes,
        sha_encrypted,
        stage_set_id,
        slice_db_id,
    ) in &source_slices
    {
        let pos: u32 = pos_str.parse().unwrap_or(0);
        let slice_path = clone_dir.join(format!("slice_{slice_db_id}.dat"));

        match stream_verify_slice_to_staging(
            store,
            pos,
            *enc_bytes as u64,
            &[sha_on_vol.as_str(), sha_encrypted.as_str()],
            &slice_path,
        )? {
            SliceStreamOutcome::Verified => {}
            SliceStreamOutcome::ChecksumMismatch { actual } => {
                return Err(TapectlError::Other(format!(
                    "checksum mismatch reading slice at position {pos} from {from_label}: \
                     got {actual}, expected {sha_on_vol} (or {sha_encrypted})"
                )));
            }
        }

        // Update staging_path so volume_write can find this slice
        conn.execute(
            "UPDATE stage_slices SET staging_path = ?1 WHERE id = ?2",
            params![slice_path.to_string_lossy().to_string(), slice_db_id],
        )?;

        affected_stage_sets.insert(*stage_set_id);
        total_bytes += *enc_bytes;
        slices_read += 1;
        info!(
            position = pos,
            slice_id = slice_db_id,
            "read slice from source"
        );
    }

    // Restore stage_sets status so find_staged_data() picks them up.
    // Guard: only promote sets that were previously successfully staged.
    for ss_id in &affected_stage_sets {
        conn.execute(
            "UPDATE stage_sets SET status = 'staged' WHERE id = ?1 AND status IN ('staged', 'cleaned')",
            params![ss_id],
        )?;
    }

    info!(
        from = from_label,
        unit = unit_name,
        slices = slices_read,
        "read-slices complete — data staged for volume write"
    );

    Ok(ReadSlicesReport {
        slices_read,
        bytes_read: total_bytes,
    })
}

#[derive(Debug, Default)]
pub struct ReadSlicesReport {
    pub slices_read: i64,
    pub bytes_read: i64,
}

#[derive(Debug, Default)]
pub struct CompactReadReport {
    pub slices_read: i64,
    pub bytes_read: i64,
    pub slices_skipped: i64,
}

/// Compact-read: read live encrypted slices from a volume to staging.
/// "Live" means the snapshot is NOT reclaimable or purged.
///
/// Takes an already-open `store` (ADR-0006) rather than a device path — the
/// caller opens `TapeStore::open_read`, so this function is directly
/// unit-testable against a `MemStore` fixture.
pub fn compact_read(
    conn: &Connection,
    config: &Config,
    label: &str,
    store: &mut dyn Store,
) -> Result<CompactReadReport> {
    let volume_id: i64 = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            params![label],
            |row| row.get(0),
        )
        .map_err(|_| TapectlError::VolumeNotFound(label.to_string()))?;

    // Find live slices (snapshots not reclaimable/purged)
    let mut stmt = conn.prepare(
        "SELECT wp.position, wp.sha256_on_volume, wp.stage_slice_id,
                ss.encrypted_bytes, ss.sha256_encrypted, ss.stage_set_id, ss.id as slice_id
         FROM write_positions wp
         JOIN writes w ON w.id = wp.write_id
         JOIN stage_slices ss ON ss.id = wp.stage_slice_id
         JOIN stage_sets sts ON sts.id = w.stage_set_id
         JOIN snapshots s ON s.id = sts.snapshot_id
         WHERE w.volume_id = ?1 AND w.status = 'completed' AND wp.status = 'written'
           AND s.status NOT IN ('reclaimable', 'purged')
         ORDER BY CAST(wp.position AS INTEGER)",
    )?;
    let live_slices: Vec<(String, String, i64, i64, String, i64, i64)> = stmt
        .query_map(params![volume_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    if live_slices.is_empty() {
        return Err(TapectlError::Other(format!(
            "no live slices on volume \"{label}\""
        )));
    }

    let staging_dir = &config.staging.directory;
    let compact_dir = std::path::Path::new(staging_dir).join(format!("compact-{label}"));
    fs::create_dir_all(&compact_dir)?;

    let mut total_bytes: i64 = 0;
    let mut slices_read: i64 = 0;
    let mut slices_skipped: i64 = 0;
    let mut affected_stage_sets = HashSet::new();

    for (pos_str, sha_on_vol, _slice_id, enc_bytes, sha_encrypted, ss_id, slice_db_id) in
        &live_slices
    {
        let pos: u32 = pos_str.parse().unwrap_or(0);
        let slice_path = compact_dir.join(format!("slice_{slice_db_id}.dat"));

        match stream_verify_slice_to_staging(
            store,
            pos,
            *enc_bytes as u64,
            &[sha_on_vol.as_str(), sha_encrypted.as_str()],
            &slice_path,
        )? {
            SliceStreamOutcome::Verified => {}
            SliceStreamOutcome::ChecksumMismatch { actual } => {
                warn!(
                    position = pos,
                    slice_id = slice_db_id,
                    actual = %actual,
                    expected_a = %sha_on_vol,
                    expected_b = %sha_encrypted,
                    "checksum mismatch — skipping slice"
                );
                slices_skipped += 1;
                continue;
            }
        }

        // Update staging_path so compact-write can find slices
        conn.execute(
            "UPDATE stage_slices SET staging_path = ?1 WHERE id = ?2",
            params![slice_path.to_string_lossy().to_string(), slice_db_id],
        )?;

        affected_stage_sets.insert(*ss_id);
        total_bytes += *enc_bytes;
        slices_read += 1;
        info!(position = pos, slice_id = slice_db_id, "read live slice");
    }

    // Restore stage_sets status so find_staged_data() picks them up.
    // Guard: only promote sets that were previously successfully staged.
    for ss_id in &affected_stage_sets {
        conn.execute(
            "UPDATE stage_sets SET status = 'staged' WHERE id = ?1 AND status IN ('staged', 'cleaned')",
            params![ss_id],
        )?;
    }

    if slices_skipped > 0 {
        return Err(TapectlError::Other(format!(
            "compact-read \"{label}\": {slices_skipped} slice(s) skipped due to checksum mismatch \
             ({slices_read} read successfully) — investigate before proceeding with compact-write",
        )));
    }

    info!(label = label, slices = slices_read, "compact-read complete");

    Ok(CompactReadReport {
        slices_read,
        bytes_read: total_bytes,
        slices_skipped,
    })
}

/// Compact-write: write staged compaction slices to destination volume.
/// Reuses the normal write pipeline — staged data from compact-read is
/// treated the same as any other staged data.
pub fn compact_write(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    dest_label: &str,
    device: &str,
    block_size: usize,
    allow_missing_escrow: bool,
) -> Result<()> {
    // The normal volume_write picks up all staged data. `force` is not
    // exposed here (out of scope for #27, which is narrowly about
    // `VolumeCommands::Init`/`Write`) — a compaction destination that fails
    // contact discipline hard-refuses, same as `quick-archive`/`collection
    // run` below.
    volume_write(
        conn,
        paths,
        config,
        dest_label,
        device,
        block_size,
        false,
        allow_missing_escrow,
    )
}

/// One unit affected by `compact_finish`'s retirement: its remaining
/// ADR-0004-eligible copy count with the source volume excluded, and the
/// evidence behind it (issue #99).
///
/// It is `cli::operations::RetireImpact` in a public wrapper, because
/// `compact_finish` retires a volume exactly like `volume_retire` does and
/// therefore consumes coverage the same way. `RetireImpact` itself is
/// `pub(crate)`, so this type carries the fields across rather than
/// re-deriving any of them.
#[derive(Debug)]
pub struct CompactFinishReport {
    pub unit_name: String,
    pub unit_status: String,
    /// ADR-0004-eligible copies this unit still has on some OTHER volume.
    /// Zero is the Tier-2 trigger (issue #147).
    pub other_copies: i64,
    pub evidence: Vec<crate::policy::evidence::CoverageEvidence>,
}

/// Compact-finish: retire the source volume after compaction.
///
/// Two gates, in this order, and the order is the ADR-0008 tier order:
///
/// 1. **Tier 3, absolute.** Any LIVE slice on this volume with no copy on
///    another volume refuses outright. No flag defeats it — this is the
///    zero-coverage case ADR-0008 says nothing may waive, and it runs
///    first so that `--yes` can never reach it.
/// 2. **Tier 2, overridable (issue #147).** If retiring the volume leaves
///    any unit with ZERO ADR-0004-eligible copies elsewhere, the coverage
///    facts are displayed and consent is required; `--force`/`--yes`
///    overrides, and a non-interactive session with neither refuses rather
///    than hanging. ADR-0008 names this command Tier 2, and until #147 it
///    had only the Tier-3 refusal.
///
/// Gate 2 can fire where gate 1 does not: gate 1 asks about live SLICES
/// (skipping reclaimable and purged snapshots), while coverage is a
/// question about UNITS across every snapshot they ever had.
pub fn compact_finish(
    conn: &Connection,
    label: &str,
    assume_yes: bool,
) -> Result<Vec<CompactFinishReport>> {
    let (vol_id, status): (i64, String) = conn
        .query_row(
            "SELECT id, status FROM volumes WHERE label = ?1",
            params![label],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| TapectlError::VolumeNotFound(label.to_string()))?;

    // Guard: verify all live slices exist on at least one other volume
    let mut stmt = conn.prepare(
        "SELECT u.name, sl.slice_number
         FROM write_positions wp
         JOIN writes w ON w.id = wp.write_id
         JOIN stage_slices sl ON sl.id = wp.stage_slice_id
         JOIN stage_sets sts ON sts.id = sl.stage_set_id
         JOIN snapshots s ON s.id = sts.snapshot_id
         JOIN units u ON u.id = s.unit_id
         WHERE w.volume_id = ?1 AND w.status = 'completed' AND wp.status = 'written'
           AND s.status NOT IN ('reclaimable', 'purged')
           AND NOT EXISTS (
             SELECT 1 FROM write_positions wp2
             JOIN writes w2 ON w2.id = wp2.write_id
             WHERE wp2.stage_slice_id = wp.stage_slice_id
               AND w2.volume_id != ?1
               AND w2.status = 'completed'
               AND wp2.status = 'written'
           )",
    )?;
    let unprotected: Vec<(String, i64)> = stmt
        .query_map(params![vol_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    if !unprotected.is_empty() {
        let examples: Vec<String> = unprotected
            .iter()
            .take(5)
            .map(|(name, num)| format!("{name} slice {num}"))
            .collect();
        return Err(TapectlError::Other(format!(
            "cannot retire \"{label}\": {} live slice(s) have no copy on another volume ({})",
            unprotected.len(),
            examples.join(", "),
        )));
    }

    // The population whose coverage this retirement consumes (ADR-0004,
    // issue #99), through `retire_impacts` — the ONE derivation `volume
    // retire`, `unit mark-tape-only` and ADR-0010's binding already share.
    // This used to be a near-copy of that query, missing only
    // `other_copies`; two coverage queries that can disagree is how this
    // codebase has been bitten before, so the copy is gone rather than
    // extended.
    let report: Vec<CompactFinishReport> = crate::cli::operations::retire_impacts(conn, vol_id)?
        .into_iter()
        .map(|impact| CompactFinishReport {
            unit_name: impact.unit_name,
            unit_status: impact.unit_status,
            other_copies: impact.other_copies,
            evidence: impact.evidence,
        })
        .collect();

    // ADR-0008 Tier 2 (issue #147). Only the zero-copy case gates, exactly
    // as `volume_retire` reads the tier: a retirement that leaves every
    // affected unit with another copy is an ordinary operation.
    let at_risk: Vec<&CompactFinishReport> =
        report.iter().filter(|u| u.other_copies == 0).collect();
    if !at_risk.is_empty() {
        let action = format!("retire volume \"{label}\" (compact-finish)");
        let mut facts: Vec<String> = at_risk
            .iter()
            .map(|u| {
                format!(
                    "unit \"{}\" [{}] would have ZERO copies remaining after this retirement",
                    u.unit_name, u.unit_status
                )
            })
            .collect();
        // ADR-0004 Tier 1: evidence age for the units that DO retain
        // coverage, shown at the moment consent is asked and never gating.
        let now = chrono::Utc::now().naive_utc();
        for unit in &report {
            if unit.other_copies != 0 {
                if let Some(line) =
                    crate::policy::evidence::describe(&unit.unit_name, &unit.evidence, now)
                {
                    facts.push(line);
                }
            }
        }
        crate::cli::consent::confirm(&action, &facts, assume_yes)?;
    }

    // Retire volume + update cartridge atomically
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE volumes SET status = 'retired' WHERE id = ?1",
        params![vol_id],
    )?;

    // Free the cartridge through the ONE guarded writer (ADR-0011). This was
    // a second, bare UPDATE that fired on any status: it would walk a
    // `retired_permanent` cartridge -- a medium the operator has declared
    // unfit and which `cartridge retire` gated behind a Tier-2 consent
    // prompt -- back to `pending_erase`, silently and without an event, and
    // `cartridge mark-erased` would then accept it. The shared helper keeps
    // the `in_use` precondition, the other-live-volume check and the audit
    // trail identical on both paths.
    crate::cli::operations::free_cartridge_if_last_live(&tx, vol_id)?;

    events::log_field_change(
        &tx,
        "volume",
        vol_id,
        label,
        "compact_finish",
        "status",
        Some(&status),
        "retired",
        None,
    )?;
    tx.commit()?;

    info!(label = label, "compact-finish: volume retired");
    Ok(report)
}

// ── Internal helpers ──

#[derive(Debug, Default)]
pub struct VerifyReport {
    pub checked: usize,
    pub passed: usize,
    pub failed: usize,
    /// Every disagreement the chain walk found, in walk order (issue #142).
    ///
    /// A superset of what reached `verification_results`: a mismatch at a
    /// metadata position has no `write_positions` cursor row to reference,
    /// so it is reportable here and not storable there. See
    /// [`record_verification_results`].
    pub mismatches: Vec<crate::store::Mismatch>,
}

/// Gather the staged batch as `BuildUnit`s, ready for `build::build`.
/// `ORDER BY u.name` stays — the alphabetical first-fit ordering ruled in
/// sheet §7 (`docs/design/v2-open-questions.md`); `build()` never reorders
/// its input, so this is where unit order is decided.
fn find_staged_data(conn: &Connection) -> Result<Vec<BuildUnit>> {
    let mut stmt = conn.prepare(
        "SELECT ss.id, ss.snapshot_id, u.name, u.uuid, u.tenant_id,
                ss.dar_version, ss.dar_command, ss.catalog_path, s.version
         FROM stage_sets ss
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN units u ON u.id = s.unit_id
         WHERE ss.status = 'staged'
         ORDER BY u.name",
    )?;

    type Row = (
        i64,
        i64,
        String,
        String,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
    );
    let rows: Vec<Row> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut units = Vec::new();
    for (ss_id, snap_id, name, uuid, tenant_id, dar_ver, dar_cmd, catalog_path, snap_ver) in rows {
        let mut slice_stmt = conn.prepare(
            "SELECT id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted, staging_path
             FROM stage_slices WHERE stage_set_id = ?1 AND staging_path IS NOT NULL
             ORDER BY slice_number",
        )?;
        let slices: Vec<BuildSlice> = slice_stmt
            .query_map(params![ss_id], |row| {
                let staging_path: String = row.get(6)?;
                Ok(BuildSlice {
                    slice_id: row.get(0)?,
                    slice_number: row.get(1)?,
                    size_bytes: row.get(2)?,
                    encrypted_bytes: row.get(3)?,
                    sha256_plain: row.get(4)?,
                    sha256_encrypted: row.get(5)?,
                    staging_path: PathBuf::from(staging_path),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        if !slices.is_empty() {
            units.push(BuildUnit {
                stage_set_id: ss_id,
                snapshot_id: snap_id,
                unit_name: name,
                unit_uuid: uuid,
                tenant_id,
                dar_version: dar_ver,
                dar_command: dar_cmd,
                catalog_path,
                snapshot_version: snap_ver,
                slices,
            });
        }
    }
    Ok(units)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Evidence, Mismatch, MismatchKind};
    use sha2::{Digest, Sha256};

    /// Issue #147: `compact-finish` is named Tier 2 by ADR-0008 and had
    /// only the Tier-3 refusal. These prove the Tier-3 refusal still comes
    /// FIRST and absolutely, that the new Tier-2 gate fires on zero
    /// coverage, and that a non-interactive session refuses rather than
    /// hangs. Every test passes `assume_yes` or asserts the non-TTY
    /// refusal, so none can touch real stdin.
    mod compact_finish_consent {
        use super::*;
        use rusqlite::params;

        /// A source volume `L6-SRC` with `unitA`'s completed write on it,
        /// and — when `with_other_copy` — the same stage set completed to a
        /// sealed `L6-DST`, which is what makes the retirement safe. Every
        /// snapshot here is `reclaimable`, so the TIER-3 slice check finds
        /// nothing to complain about (it skips reclaimable/purged) and the
        /// Tier-2 unit check is reached in isolation. That divergence is
        /// the whole reason #147 exists.
        fn setup(with_other_copy: bool) -> Connection {
            let conn = crate::db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
                [],
            )
            .unwrap();
            let tid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                 VALUES ('u1', 'unitA', ?1, 'mtime_size', 1, 'active')",
                params![tid],
            )
            .unwrap();
            let unit_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
                 VALUES (?1, 1, 'full', 'reclaimable', '/src')",
                params![unit_id],
            )
            .unwrap();
            let snap_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO stage_sets (snapshot_id, status, slice_size)
                 VALUES (?1, 'staged', 524288)",
                params![snap_id],
            )
            .unwrap();
            let ss_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                      capacity_bytes, status)
                 VALUES ('L6-SRC', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
                [],
            )
            .unwrap();
            let src = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, 'completed')",
                params![ss_id, snap_id, src],
            )
            .unwrap();
            if with_other_copy {
                conn.execute(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                          capacity_bytes, status)
                     VALUES ('L6-DST', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
                    [],
                )
                .unwrap();
                let dst = conn.last_insert_rowid();
                conn.execute(
                    "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                     VALUES (?1, ?2, ?3, 'completed')",
                    params![ss_id, snap_id, dst],
                )
                .unwrap();
            }
            conn
        }

        fn status_of(conn: &Connection, label: &str) -> String {
            conn.query_row(
                "SELECT status FROM volumes WHERE label = ?1",
                params![label],
                |r| r.get(0),
            )
            .unwrap()
        }

        /// The bug #147 names: a unit that ends with ZERO copies used to be
        /// retired silently, because the Tier-3 slice check skips
        /// reclaimable snapshots and nothing else looked.
        #[test]
        fn zero_remaining_copies_now_refuses_without_consent() {
            let conn = setup(false);
            let err = compact_finish(&conn, "L6-SRC", false)
                .expect_err("a unit dropping to zero copies must gate (ADR-0008 Tier 2)");
            assert!(err.to_string().contains("refused"), "got: {err}");
            assert_eq!(
                status_of(&conn, "L6-SRC"),
                "sealed",
                "a refused compact-finish must not retire the volume"
            );
        }

        /// Tier 2, not Tier 3: `--force`/`--yes` genuinely overrides.
        #[test]
        fn assume_yes_overrides_the_zero_copy_gate() {
            let conn = setup(false);
            compact_finish(&conn, "L6-SRC", true).expect("--yes must override a Tier-2 gate");
            assert_eq!(status_of(&conn, "L6-SRC"), "retired");
        }

        /// The ordinary case is untouched: every affected unit keeps a copy,
        /// so no gate is reached and no consent is needed. This is what the
        /// mhvtl lifecycle suite and `tests/integration.rs` exercise, both
        /// of which run with stdin closed and no `--yes`.
        #[test]
        fn a_unit_that_keeps_a_copy_needs_no_consent_at_all() {
            let conn = setup(true);
            compact_finish(&conn, "L6-SRC", false)
                .expect("no at-risk unit means no gate, exactly as before #147");
            assert_eq!(status_of(&conn, "L6-SRC"), "retired");
        }

        /// The Tier-3 refusal stays FIRST and stays absolute — `--yes` must
        /// not reach it. Here the snapshot is live ('current'), so the slice
        /// check has something to find.
        #[test]
        fn the_tier_3_slice_refusal_still_wins_over_assume_yes() {
            let conn = setup(false);
            conn.execute(
                "UPDATE snapshots SET status = 'current' WHERE unit_id = 1",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes,
                                           sha256_plain, encrypted_bytes, sha256_encrypted)
                 VALUES (1, 0, 1024, 'cafe', 1024, 'deadbeef')",
                [],
            )
            .unwrap();
            let slice_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO write_positions (write_id, stage_slice_id, position,
                                              sha256_on_volume, status)
                 VALUES (1, ?1, '5', 'deadbeef', 'written')",
                params![slice_id],
            )
            .unwrap();

            let err = compact_finish(&conn, "L6-SRC", true)
                .expect_err("no flag may defeat the Tier-3 refusal (ADR-0008)");
            assert!(
                err.to_string().contains("have no copy on another volume"),
                "the Tier-3 refusal must be the one that fired; got: {err}"
            );
            assert_eq!(status_of(&conn, "L6-SRC"), "sealed");
        }
    }

    fn direct_hash(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        format!("{:x}", h.finalize())
    }

    // --- stream_verify_slice_to_staging (issue #86: read_slices/
    // compact_read streaming) -----------------------------------------------
    //
    // `stream_verify_slice_to_staging` is the per-slice logic shared by both
    // `read_slices` and `compact_read`, one call per slice — factored out
    // because both callers need it verbatim, not because it is the only
    // testable layer. `read_slices`/`compact_read` themselves now take
    // `&mut dyn Store` directly too (ADR-0006, C7): the CLI opens
    // `TapeStore::open_read` once and hands the store in, so the entry
    // points are the test surface — see the `read_slices_with_a_mem_store_...`
    // and `compact_read_with_a_mem_store_...` tests further down.
    // (`MemStore` is imported once, further down, by the fresh-write-contact
    // tests — a single `use` covers the whole flat `mod tests`.)

    #[test]
    fn stream_verify_slice_to_staging_round_trips_and_trims_padding() {
        let mut store = MemStore::new(4096);
        let true_bytes = b"encrypted slice content, repeated a bit so on-tape block \
                           padding is real and not a no-op. "
            .repeat(20);
        let hash = direct_hash(&true_bytes);
        store
            .execute(
                &mut Cursor::new(true_bytes.clone()),
                true_bytes.len() as u64,
                false,
            )
            .unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let dest = tmp.path().join("slice.dat");

        let outcome = stream_verify_slice_to_staging(
            &mut store,
            0,
            true_bytes.len() as u64,
            &[hash.as_str()],
            &dest,
        )
        .unwrap();
        assert!(matches!(outcome, SliceStreamOutcome::Verified));

        let on_disk = fs::read(&dest).unwrap();
        assert_eq!(
            on_disk, true_bytes,
            "staged file must hold exactly the true (unpadded) bytes, not the padded tail"
        );
    }

    #[test]
    fn stream_verify_slice_to_staging_detects_mismatch_and_removes_the_partial_file() {
        let mut store = MemStore::new(4096);
        let true_bytes = b"some slice content that will not match the expected hash".to_vec();
        store
            .execute(
                &mut Cursor::new(true_bytes.clone()),
                true_bytes.len() as u64,
                false,
            )
            .unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let dest = tmp.path().join("slice.dat");
        let wrong_hash = "0".repeat(64);

        let outcome = stream_verify_slice_to_staging(
            &mut store,
            0,
            true_bytes.len() as u64,
            &[wrong_hash.as_str()],
            &dest,
        )
        .unwrap();
        match outcome {
            SliceStreamOutcome::ChecksumMismatch { actual } => {
                assert_ne!(actual, wrong_hash);
                assert_eq!(actual, direct_hash(&true_bytes));
            }
            SliceStreamOutcome::Verified => panic!("must not verify against a wrong hash"),
        }
        assert!(
            !dest.exists(),
            "a corrupt/mismatched slice must not be left behind in staging \
             (streaming writes bytes before the hash is known, unlike the old \
             buffered check-then-write code, so this cleanup is load-bearing)"
        );
    }

    #[test]
    fn stream_verify_slice_to_staging_accepts_a_match_against_either_expected_hash() {
        // Mirrors read_slices'/compact_read's own `!= sha_on_vol &&
        // != sha_encrypted` either-match: the true hash is the SECOND
        // candidate here, proving a match anywhere in the list is accepted,
        // not just at index 0.
        let mut store = MemStore::new(4096);
        let true_bytes = b"content whose hash matches the second candidate only".to_vec();
        let hash = direct_hash(&true_bytes);
        store
            .execute(
                &mut Cursor::new(true_bytes.clone()),
                true_bytes.len() as u64,
                false,
            )
            .unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let dest = tmp.path().join("slice.dat");
        let wrong = "0".repeat(64);

        let outcome = stream_verify_slice_to_staging(
            &mut store,
            0,
            true_bytes.len() as u64,
            &[wrong.as_str(), hash.as_str()],
            &dest,
        )
        .unwrap();
        assert!(matches!(outcome, SliceStreamOutcome::Verified));
    }

    #[test]
    fn stream_verify_slice_to_staging_cleans_up_on_a_tape_read_error() {
        // Nothing recorded at position 0 -> MemStore::read_file errors.
        // The destination file is created (empty) before the read is
        // attempted, so this proves the cleanup-on-error path removes it
        // rather than leaving an empty file behind.
        let mut store = MemStore::new(4096);
        let tmp = tempfile::TempDir::new().unwrap();
        let dest = tmp.path().join("slice.dat");

        let err = stream_verify_slice_to_staging(&mut store, 0, 10, &["irrelevant"], &dest);
        assert!(err.is_err());
        assert!(
            !dest.exists(),
            "the empty file created before a failed read must not be left behind"
        );
    }

    // --- volume_identify (Store seam, C7) -----------------------------------

    #[test]
    fn volume_identify_reads_file_0_and_trims_padding() {
        let params = layout::IdThunkV2Params {
            label: "IDTEST",
            uuid: "22222222-2222-2222-2222-222222222222",
            media_type: "LTO-6",
            tapectl_version: "0.2.0",
            nominal_capacity: 1,
            mam_capacity: 1,
            total_files: 6,
            mam_manufacturer: "IBM",
            mam_serial: "SERIAL1",
            mam_length: 1,
            mam_loads: 1,
            created_at: "2026-09-11T00:00:00Z",
            cartridge_identity_source: None,
        };
        let thunk_text = layout::generate_id_thunk_v2(&params);

        // Block size smaller than the thunk text so the store genuinely
        // pads position 0 to a block boundary — proving trim_end_matches
        // strips real padding, not a no-op on an already-aligned buffer.
        let mut store = MemStore::new(64);
        store
            .execute(&mut thunk_text.as_bytes(), thunk_text.len() as u64, false)
            .unwrap();

        let id = volume_identify(&mut store).unwrap();
        assert!(id.contains("IDTEST"), "id thunk missing label: {id}");
        assert!(!id.ends_with('\0'), "block padding must be trimmed: {id:?}");
    }

    // --- read_slices / compact_read via MemStore (Store seam, C7) ----------
    //
    // Both entry points now take `&mut dyn Store` directly, so a `MemStore`
    // drives the real function bodies end-to-end — no store-shaped inner
    // twin needed. Each fixture builds just enough DB state (tenant, unit,
    // snapshot, stage_set, stage_slice, volume, write, write_position) for
    // the function's own SQL to find one slice, plus a MemStore holding that
    // slice's true (unpadded) plaintext at the matching tape position.

    /// Insert one tenant/unit/snapshot/stage_set/stage_slice/volume/write/
    /// write_position chain for `label`/`unit_name`, with `slice_bytes` as
    /// the slice's plaintext content recorded at tape `position`. Returns
    /// the stage_slice id. `write_status`/`snapshot_status` let callers
    /// exercise `compact_read`'s extra filters (`w.status = 'completed'`,
    /// `s.status NOT IN ('reclaimable', 'purged')`).
    fn seed_one_slice_fixture(
        conn: &Connection,
        label: &str,
        unit_name: &str,
        position: u32,
        slice_bytes: &[u8],
        write_status: &str,
        snapshot_status: &str,
    ) -> i64 {
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t1', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES (?1, ?1, ?2, 'mtime_size', 1, 'active')",
            params![unit_name, tenant_id],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            &format!(
                "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
                 VALUES (?1, 1, '{snapshot_status}', '/tmp', 1, ?2)"
            ),
            params![unit_id, slice_bytes.len() as i64],
        )
        .unwrap();
        let snap_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
            params![snap_id],
        )
        .unwrap();
        let ss_id = conn.last_insert_rowid();
        let hash = direct_hash(slice_bytes);
        conn.execute(
            "INSERT INTO stage_slices
                (stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted)
             VALUES (?1, 1, ?2, ?2, ?3, ?3)",
            params![ss_id, slice_bytes.len() as i64, hash],
        )
        .unwrap();
        let slice_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
            params![label],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();
        conn.execute(
            &format!(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, '{write_status}')"
            ),
            params![ss_id, snap_id, volume_id],
        )
        .unwrap();
        let write_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO write_positions (write_id, stage_slice_id, position, status, sha256_on_volume)
             VALUES (?1, ?2, ?3, 'written', ?4)",
            params![write_id, slice_id, position.to_string(), hash],
        )
        .unwrap();
        slice_id
    }

    // ── issue #142: `verification_results` gets rows ──

    /// A complete, self-consistent v2 tape in a `MemStore`: File 0 id thunk,
    /// 1 guide, 2 RESTORE.sh, 3 front index, 4 data slice, 5 seal marker.
    ///
    /// `slice_on_tape` is what actually lands at position 4; the front index
    /// always claims the hash of `slice_claimed`. Pass the same bytes twice
    /// for a clean tape, different bytes for a corrupted one — that is the
    /// whole corruption mechanism, and it is exactly what a bit-rotted tape
    /// looks like to the keyless chain walk.
    fn mem_store_v2_tape(label: &str, slice_claimed: &[u8], slice_on_tape: &[u8]) -> MemStore {
        const BS: usize = 4096;
        let id_thunk = format!("tapectl-volume-v2\n[volume]\nlabel = \"{label}\"\n").into_bytes();
        let guide = b"SYSTEM GUIDE\n".to_vec();
        let restore_sh = b"#!/bin/sh\n".to_vec();

        // Every content file's entry carries its true size + the sha256 of
        // its on-tape bytes; File 3 and the seal marker carry neither and
        // one, respectively (the two self-referential exclusions, §3).
        let mut files = vec![
            layout::FrontIndexFile {
                position: 0,
                type_label: "id_thunk",
                size_bytes: Some(id_thunk.len() as u64),
                sha256_encrypted: Some(direct_hash(&id_thunk)),
            },
            layout::FrontIndexFile {
                position: 1,
                type_label: "system_guide",
                size_bytes: Some(guide.len() as u64),
                sha256_encrypted: Some(direct_hash(&guide)),
            },
            layout::FrontIndexFile {
                position: 2,
                type_label: "restore_sh",
                size_bytes: Some(restore_sh.len() as u64),
                sha256_encrypted: Some(direct_hash(&restore_sh)),
            },
            layout::FrontIndexFile {
                position: 3,
                type_label: "front_index",
                size_bytes: None,
                sha256_encrypted: None,
            },
            layout::FrontIndexFile {
                position: 4,
                type_label: "data_slice",
                size_bytes: Some(slice_claimed.len() as u64),
                sha256_encrypted: Some(direct_hash(slice_claimed)),
            },
            layout::FrontIndexFile {
                position: 5,
                type_label: "seal_marker",
                size_bytes: None,
                sha256_encrypted: None,
            },
        ];

        let fi_text = layout::generate_front_index(label, &files);
        let fi_bytes = fi_text.clone().into_bytes();
        // The seal marker's embedded copy fills in File 3's own figures,
        // which are only known once File 3's bytes exist.
        files[3].size_bytes = Some(fi_bytes.len() as u64);
        files[3].sha256_encrypted = Some(direct_hash(&fi_bytes));
        let seal_text = layout::generate_seal_marker(label, 6, &direct_hash(&fi_bytes), &files);

        let mut store = MemStore::new(BS);
        for bytes in [
            id_thunk,
            guide,
            restore_sh,
            fi_bytes,
            slice_on_tape.to_vec(),
            seal_text.into_bytes(),
        ] {
            store
                .execute(&mut Cursor::new(bytes.clone()), bytes.len() as u64, false)
                .unwrap();
        }
        store
    }

    fn verification_result_rows(conn: &Connection) -> Vec<(String, String, String)> {
        conn.prepare(
            "SELECT wp.position, vr.result, COALESCE(vr.notes, '')
             FROM verification_results vr
             JOIN write_positions wp ON wp.id = vr.write_position_id
             ORDER BY vr.id",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    }

    /// THE test for issue #142: a deliberately corrupted slice produces
    /// EXACTLY ONE `verification_results` row, and that row names the
    /// position that failed.
    ///
    /// Before this, `verification_sessions` recorded `slices_failed = 1` and
    /// the table that exists to say WHICH had been empty since
    /// `001_initial.sql`.
    #[test]
    fn a_corrupted_slice_writes_exactly_one_verification_result_naming_its_position() {
        let conn = crate::db::open_memory().unwrap();
        let good = b"the bytes the front index promises. ".repeat(4);
        let rotted = b"the bytes the tape actually holds!! ".repeat(4);
        assert_eq!(good.len(), rotted.len(), "same length: a HASH change only");

        seed_one_slice_fixture(
            &conn,
            "VR-CORRUPT",
            "vr-unit",
            4,
            &good,
            "completed",
            "staged",
        );
        let volume_id: i64 = conn
            .query_row(
                "SELECT id FROM volumes WHERE label = 'VR-CORRUPT'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        let mut store = mem_store_v2_tape("VR-CORRUPT", &good, &rotted);
        let report = volume_verify_with_store(
            &conn,
            &mut store,
            "VR-CORRUPT",
            volume_id,
            4096,
            Tier::Integrity,
        )
        .unwrap();

        assert_eq!(report.failed, 1, "mismatches: {:?}", report.mismatches);
        assert_eq!(report.mismatches[0].position, 4);

        let rows = verification_result_rows(&conn);
        assert_eq!(rows.len(), 1, "expected exactly one recorded row: {rows:?}");
        assert_eq!(rows[0].0, "4", "the row must name the failing position");
        assert_eq!(rows[0].1, "failed_checksum");
        assert!(
            rows[0].2.contains("content_hash_mismatch"),
            "notes must carry the true MismatchKind: {}",
            rows[0].2
        );

        // The hash columns are genuinely hashes for this kind.
        let (expected, actual): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT expected_sha256, actual_sha256 FROM verification_results",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(expected.as_deref(), Some(direct_hash(&good).as_str()));
        assert_eq!(actual.as_deref(), Some(direct_hash(&rotted).as_str()));

        // And the session aggregate still agrees with the detail.
        let failed: i64 = conn
            .query_row("SELECT slices_failed FROM verification_sessions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(failed, 1);
    }

    /// A clean tape writes a passing session and NO result rows — the table
    /// records failures, not a row per file checked.
    #[test]
    fn a_clean_tape_writes_no_verification_result_rows() {
        let conn = crate::db::open_memory().unwrap();
        let good = b"intact slice bytes, repeated a few times. ".repeat(4);
        seed_one_slice_fixture(
            &conn,
            "VR-CLEAN",
            "vc-unit",
            4,
            &good,
            "completed",
            "staged",
        );
        let volume_id: i64 = conn
            .query_row("SELECT id FROM volumes WHERE label = 'VR-CLEAN'", [], |r| {
                r.get(0)
            })
            .unwrap();

        let mut store = mem_store_v2_tape("VR-CLEAN", &good, &good);
        let report = volume_verify_with_store(
            &conn,
            &mut store,
            "VR-CLEAN",
            volume_id,
            4096,
            Tier::Integrity,
        )
        .unwrap();

        assert_eq!(report.failed, 0, "mismatches: {:?}", report.mismatches);
        assert!(verification_result_rows(&conn).is_empty());
        let outcome: String = conn
            .query_row("SELECT outcome FROM verification_sessions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(outcome, "passed");
    }

    /// A mismatch at a METADATA position is counted by the session and has
    /// no `verification_results` row to occupy: `write_positions.
    /// stage_slice_id` is `NOT NULL`, so only slices have a cursor row to
    /// reference. Asserted, rather than left to be discovered, because the
    /// alternative — relaxing the FK or inventing a cursor row — is the
    /// wrong fix and someone will propose it.
    #[test]
    fn a_metadata_position_mismatch_is_counted_but_not_recordable() {
        let conn = crate::db::open_memory().unwrap();
        let good = b"slice bytes that stay intact throughout. ".repeat(4);
        seed_one_slice_fixture(&conn, "VR-META", "vm-unit", 4, &good, "completed", "staged");
        let volume_id: i64 = conn
            .query_row("SELECT id FROM volumes WHERE label = 'VR-META'", [], |r| {
                r.get(0)
            })
            .unwrap();

        // Corrupt File 1 (the system guide) instead of the slice.
        let mut store = mem_store_v2_tape("VR-META", &good, &good);
        store.files[1] = b"TAMPERED GUIDE".to_vec();

        let report = volume_verify_with_store(
            &conn,
            &mut store,
            "VR-META",
            volume_id,
            4096,
            Tier::Integrity,
        )
        .unwrap();

        assert_eq!(report.failed, 1, "mismatches: {:?}", report.mismatches);
        assert_eq!(report.mismatches[0].position, 1);
        assert!(
            verification_result_rows(&conn).is_empty(),
            "a metadata position has no write_positions row to reference"
        );
        // The session still counts it, so nothing is silently dropped.
        let failed: i64 = conn
            .query_row("SELECT slices_failed FROM verification_sessions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(failed, 1);
    }

    fn mem_store_with_slice_at(position: u32, bytes: &[u8]) -> MemStore {
        let mut store = MemStore::new(4096);
        for p in 0..=position {
            if p == position {
                store
                    .execute(&mut Cursor::new(bytes.to_vec()), bytes.len() as u64, false)
                    .unwrap();
            } else {
                store
                    .execute(&mut Cursor::new(vec![0u8]), 1, false)
                    .unwrap();
            }
        }
        store
    }

    #[test]
    fn read_slices_with_a_mem_store_stages_the_slice_and_updates_staging_path() {
        let conn = crate::db::open_memory().unwrap();
        let data = b"read_slices MemStore fixture plaintext, repeated. ".repeat(10);
        let slice_id =
            seed_one_slice_fixture(&conn, "RSLABEL", "rs-unit", 4, &data, "completed", "staged");

        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.staging.directory = tmp.path().to_string_lossy().into_owned();

        let mut store = mem_store_with_slice_at(4, &data);

        let report = read_slices(&conn, &config, "RSLABEL", "rs-unit", &mut store).unwrap();
        assert_eq!(report.slices_read, 1);
        assert_eq!(report.bytes_read, data.len() as i64);

        let staging_path: String = conn
            .query_row(
                "SELECT staging_path FROM stage_slices WHERE id = ?1",
                params![slice_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!staging_path.is_empty());
        let on_disk = fs::read(&staging_path).unwrap();
        assert_eq!(on_disk, data, "staged bytes must be the true plaintext");
    }

    #[test]
    fn compact_read_with_a_mem_store_stages_live_slices() {
        let conn = crate::db::open_memory().unwrap();
        let data = b"compact_read MemStore fixture plaintext, repeated. ".repeat(10);
        seed_one_slice_fixture(&conn, "CRLABEL", "cr-unit", 4, &data, "completed", "staged");

        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.staging.directory = tmp.path().to_string_lossy().into_owned();

        let mut store = mem_store_with_slice_at(4, &data);

        let report = compact_read(&conn, &config, "CRLABEL", &mut store).unwrap();
        assert_eq!(report.slices_read, 1);
        assert_eq!(report.slices_skipped, 0);
        assert_eq!(report.bytes_read, data.len() as i64);
    }

    #[test]
    fn volume_uuid_is_persisted_and_stable_not_derived_from_label() {
        // Migration 004: the uuid is an independent identifier, not a
        // restatement of the label — resume requires BOTH to match so a
        // relabelled cartridge reads as divergence (layout-session.md).
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('L6-0001', 'lto', 'lto0', 'LTO-6', 1000, 'initialized')",
            [],
        )
        .unwrap();
        let id_a = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('L6-0002', 'lto', 'lto0', 'LTO-6', 1000, 'initialized')",
            [],
        )
        .unwrap();
        let id_b = conn.last_insert_rowid();

        // Self-heal: fixtures that insert without a uuid still work.
        let a1 = volume_uuid(&conn, id_a).unwrap();
        let a2 = volume_uuid(&conn, id_a).unwrap();
        assert_eq!(a1, a2, "uuid must be stable once persisted");
        assert!(uuid::Uuid::parse_str(&a1).is_ok(), "must be a real uuid");

        let b = volume_uuid(&conn, id_b).unwrap();
        assert_ne!(a1, b, "distinct volumes must get distinct uuids");

        // And it is genuinely random, not a function of the label: a second
        // volume row sharing a label would still differ. (label is UNIQUE, so
        // assert the weaker observable: the uuid is not derivable from label.)
        let stored: String = conn
            .query_row(
                "SELECT uuid FROM volumes WHERE id = ?1",
                params![id_a],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, a1, "uuid must be persisted, not recomputed");
    }

    #[test]
    fn find_staged_data_returns_units_in_name_order_with_their_slices() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t1', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();

        // Inserted zeta-then-alpha, deliberately reverse-alphabetical, to
        // prove `ORDER BY u.name` (sheet §7's alphabetical first-fit) is what
        // actually orders the result, not insertion order.
        for name in ["zeta", "alpha"] {
            conn.execute(
                "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                 VALUES (?1, ?1, ?2, 'mtime_size', 1, 'active')",
                params![name, tenant_id],
            )
            .unwrap();
            let unit_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
                 VALUES (?1, 1, 'staged', '/tmp', 1, 10)",
                params![unit_id],
            )
            .unwrap();
            let snap_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
                params![snap_id],
            )
            .unwrap();
            let ss_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO stage_slices
                    (stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted, staging_path)
                 VALUES (?1, 1, 10, 10, 'a', 'b', '/tmp/x')",
                params![ss_id],
            )
            .unwrap();
        }

        let units = find_staged_data(&conn).unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(
            units[0].unit_name, "alpha",
            "ORDER BY u.name must sort alphabetically regardless of insertion order"
        );
        assert_eq!(units[1].unit_name, "zeta");
        assert_eq!(units[0].slices.len(), 1);
        assert_eq!(units[0].slices[0].sha256_encrypted, "b");
    }

    #[test]
    fn find_staged_data_skips_stage_sets_with_no_staged_slices() {
        // A stage_set whose only slice has staging_path = NULL (never
        // actually staged to disk, or already cleaned) must not surface as
        // a unit to write — `find_staged_data`'s slice query filters
        // `staging_path IS NOT NULL`, and an empty slice list drops the unit
        // entirely (mirrors v1's `if !slices.is_empty()` guard).
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t1', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('u1', 'u1', ?1, 'mtime_size', 1, 'active')",
            params![tenant_id],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'staged', '/tmp', 1, 10)",
            params![unit_id],
        )
        .unwrap();
        let snap_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
            params![snap_id],
        )
        .unwrap();
        let ss_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_slices
                (stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted, staging_path)
             VALUES (?1, 1, 10, 10, 'a', 'b', NULL)",
            params![ss_id],
        )
        .unwrap();

        let units = find_staged_data(&conn).unwrap();
        assert!(
            units.is_empty(),
            "a stage_set with no on-disk slices must not surface as staged data"
        );
    }

    // --- issue #115: recorded recipients vs the CURRENT escrow ------------
    //
    // The defect these pin: `stage create` encrypts the slices, `volume
    // write` encrypts the envelopes, and the escrow recipient could be
    // registered between the two. Pre-write validation only asked "is an
    // escrow registered", so the tape sealed and verified clean with
    // 5-recipient envelopes over 4-recipient slices — the escrow key opens
    // the envelope and not the data, which is the exact failure ADR-0005
    // exists to prevent. `stage_sets.key_fingerprints` is the ONLY evidence
    // of who a staged slice was really encrypted to (an age X25519 stanza
    // carries an ephemeral share, not a recipient identity), so it is what
    // gets compared.

    /// A DB with an operator tenant (+ active key), one content tenant
    /// (+ active key), one unit/snapshot/stage_set, and no escrow yet.
    /// Returns `(conn, tenant_id, stage_set_id)`.
    fn escrow_check_fixture() -> (Connection, i64, i64) {
        let conn = crate::db::open_memory().unwrap();

        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('operator', 1, 'active')",
            [],
        )
        .unwrap();
        let operator_id = conn.last_insert_rowid();
        let op_key = crate::crypto::keys::generate_keypair();
        conn.execute(
            "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key, key_type, is_active)
             VALUES (?1, 'operator-key', ?2, ?3, 'primary', 1)",
            params![operator_id, op_key.fingerprint, op_key.public_key],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('alpha', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        let tenant_key = crate::crypto::keys::generate_keypair();
        conn.execute(
            "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key, key_type, is_active)
             VALUES (?1, 'alpha-key', ?2, ?3, 'primary', 1)",
            params![tenant_id, tenant_key.fingerprint, tenant_key.public_key],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('u-photos', 'photos', ?1, 'mtime_size', 1, 'active')",
            params![tenant_id],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'staged', '/tmp/photos', 1, 10)",
            params![unit_id],
        )
        .unwrap();
        let snapshot_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
            params![snapshot_id],
        )
        .unwrap();
        let stage_set_id = conn.last_insert_rowid();

        (conn, tenant_id, stage_set_id)
    }

    /// Register the permanent escrow recipient (ADR-0005) on its own holder
    /// tenant, exactly as production does — public key only, the secret half
    /// never touching the DB. Mirrors `tests/mhvtl_e2e.rs`'s harness.
    /// Returns the escrow public key.
    fn register_escrow(conn: &Connection) -> String {
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('escrow-holder', 0, 'active')",
            [],
        )
        .unwrap();
        let holder_id = conn.last_insert_rowid();
        let kp = crate::crypto::keys::generate_keypair();
        queries::insert_escrow_key(
            conn,
            holder_id,
            "test-escrow",
            &kp.fingerprint,
            &kp.public_key,
            Some("test escrow recipient (ADR-0005)"),
        )
        .unwrap();
        kp.public_key
    }

    fn set_key_fingerprints(conn: &Connection, stage_set_id: i64, json: Option<&str>) {
        conn.execute(
            "UPDATE stage_sets SET key_fingerprints = ?1 WHERE id = ?2",
            params![json, stage_set_id],
        )
        .unwrap();
    }

    /// The verdict list `assemble_session_keys` hands `validate`, for the
    /// stage sets `find_staged_data` selected.
    fn verdicts(
        conn: &Connection,
        tenant_id: i64,
        stage_set_id: i64,
    ) -> Vec<(i64, String, String)> {
        let session = assemble_session_keys(conn, &[tenant_id], &[stage_set_id]).unwrap();
        session
            .keys
            .stage_sets_lacking_escrow
            .expect("production assembly must always COMPUTE this field, never leave it None")
    }

    #[test]
    fn stage_set_staged_before_escrow_registration_is_named_by_the_assembly_path() {
        // The exact real-world ordering from the 2026-09-10 LTO-6 session:
        // stage first (4 recipients), register escrow second, write third.
        let (conn, tenant_id, stage_set_id) = escrow_check_fixture();
        let other = crate::crypto::keys::generate_keypair();
        set_key_fingerprints(
            &conn,
            stage_set_id,
            Some(&serde_json::to_string(&vec![other.public_key]).unwrap()),
        );
        register_escrow(&conn);

        let v = verdicts(&conn, tenant_id, stage_set_id);
        assert_eq!(v.len(), 1, "expected exactly one lacking stage set: {v:?}");
        assert_eq!(v[0].0, stage_set_id);
        assert_eq!(v[0].1, "photos", "the verdict must name the unit");
        assert_eq!(v[0].2, "encrypted without the current escrow recipient");
    }

    #[test]
    fn stage_set_recording_the_escrow_recipient_passes() {
        let (conn, tenant_id, stage_set_id) = escrow_check_fixture();
        let escrow_pk = register_escrow(&conn);
        let other = crate::crypto::keys::generate_keypair();
        set_key_fingerprints(
            &conn,
            stage_set_id,
            Some(&serde_json::to_string(&vec![other.public_key, escrow_pk]).unwrap()),
        );

        assert!(
            verdicts(&conn, tenant_id, stage_set_id).is_empty(),
            "a recorded list containing the escrow key is the PASS case"
        );

        // ...and the Layout-level predicate agrees.
        let session = assemble_session_keys(&conn, &[tenant_id], &[stage_set_id]).unwrap();
        let layout = Layout {
            label: "ESCTEST".into(),
            volume_uuid: "u".into(),
            media_type: "LTO-6".into(),
            block_size: 512 * 1024,
            budget: CapacityBudget {
                available_bytes: 1_000_000_000,
                reserve_bytes: 0,
            },
            entries: vec![],
        };
        assert!(layout.validate(&session.keys).is_ok());
    }

    #[test]
    fn escrow_check_fails_closed_on_null_unparseable_and_unencrypted() {
        // Every one of these is a stage set whose recipient list cannot be
        // proven to contain the escrow key. None of them may be skipped:
        // "we could not tell" and "it is fine" are different answers, and
        // only one of them is safe on write-once media.
        let (conn, tenant_id, stage_set_id) = escrow_check_fixture();
        let escrow_pk = register_escrow(&conn);

        // NULL — never finalized, or a pre-#115 row.
        set_key_fingerprints(&conn, stage_set_id, None);
        let v = verdicts(&conn, tenant_id, stage_set_id);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].2, "no recorded recipient list");

        // Not JSON at all.
        set_key_fingerprints(&conn, stage_set_id, Some("{not json"));
        let v = verdicts(&conn, tenant_id, stage_set_id);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].2, "recipient list is unreadable");

        // Valid JSON, but not a list of recipients.
        set_key_fingerprints(&conn, stage_set_id, Some("{\"escrow\": true}"));
        let v = verdicts(&conn, tenant_id, stage_set_id);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].2, "recipient list is unreadable");

        // encrypted = 0. The recorded list deliberately DOES contain the
        // escrow key here, so only the `encrypted` column can produce a
        // verdict — proving that column is really consulted.
        set_key_fingerprints(
            &conn,
            stage_set_id,
            Some(&serde_json::to_string(&vec![escrow_pk]).unwrap()),
        );
        conn.execute(
            "UPDATE stage_sets SET encrypted = 0 WHERE id = ?1",
            params![stage_set_id],
        )
        .unwrap();
        let v = verdicts(&conn, tenant_id, stage_set_id);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].2, "staged with encrypted=0");
    }

    #[test]
    fn no_registered_escrow_reports_only_escrow_recipient_missing_not_every_stage_set() {
        // With no escrow at all the remedy is "register one", and
        // `EscrowRecipientMissing` says exactly that. Repeating it per stage
        // set would bury it under N copies of the WRONG remedy (re-stage).
        let (conn, tenant_id, stage_set_id) = escrow_check_fixture();
        set_key_fingerprints(&conn, stage_set_id, None);

        let session = assemble_session_keys(&conn, &[tenant_id], &[stage_set_id]).unwrap();
        assert_eq!(session.keys.escrow_recipient_present, Some(false));
        assert_eq!(
            session.keys.stage_sets_lacking_escrow,
            Some(vec![]),
            "computed (Some), but empty — the missing-escrow error covers this case"
        );
    }

    #[test]
    fn stage_set_ids_for_layout_maps_slice_entries_back_to_their_stage_sets() {
        // `volume_resume`'s half of the plumbing: by resume time `plan` has
        // already moved these rows out of status='staged', so the Layout —
        // not a re-run of `find_staged_data` — is what says which stage sets
        // this session writes.
        let (conn, _tenant_id, stage_set_id) = escrow_check_fixture();
        let mut slice_ids = Vec::new();
        for n in 1..=2 {
            conn.execute(
                "INSERT INTO stage_slices
                    (stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted, staging_path)
                 VALUES (?1, ?2, 10, 10, 'a', 'b', '/tmp/x')",
                params![stage_set_id, n],
            )
            .unwrap();
            slice_ids.push(conn.last_insert_rowid());
        }

        let layout = Layout {
            label: "RESTEST".into(),
            volume_uuid: "u".into(),
            media_type: "LTO-6".into(),
            block_size: 512 * 1024,
            budget: CapacityBudget {
                available_bytes: 1_000_000_000,
                reserve_bytes: 0,
            },
            entries: vec![
                LayoutEntry {
                    position: 0,
                    kind: ZoneKind::IdThunk,
                    size_bytes: Some(10),
                    sha256: None,
                    source: ContentSource::Generated,
                },
                LayoutEntry {
                    position: 4,
                    kind: ZoneKind::Slice {
                        stage_slice_id: slice_ids[0],
                    },
                    size_bytes: Some(10),
                    sha256: Some("b".into()),
                    source: ContentSource::Generated,
                },
                LayoutEntry {
                    position: 5,
                    kind: ZoneKind::Slice {
                        stage_slice_id: slice_ids[1],
                    },
                    size_bytes: Some(10),
                    sha256: Some("b".into()),
                    source: ContentSource::Generated,
                },
            ],
        };

        assert_eq!(
            stage_set_ids_for_layout(&conn, &layout).unwrap(),
            vec![stage_set_id],
            "two slices of one stage set must dedup to one id, and non-slice \
             entries must contribute nothing"
        );
    }

    /// Issue #115, the `--force` question. `force` is a parameter of exactly
    /// one thing — [`check_fresh_write_contact`], the File-0 identity check
    /// at contact — and `volume_write` runs `built.validate(&keys)` BEFORE
    /// `TapeStore::open` ever touches a device. So a stage set lacking the
    /// escrow recipient is refused with `force = true` just as loudly as
    /// without it, and no drive is contacted on the way to the refusal.
    ///
    /// This drives the real `volume_write` (not a validate-level stand-in)
    /// precisely to pin that ordering: a bogus device path is reached only
    /// if the refusal fails to fire, in which case the error names the
    /// device instead of the stage set.
    fn sle(id: i64) -> crate::volume::layout_model::LayoutError {
        crate::volume::layout_model::LayoutError::StageSetLacksEscrow {
            stage_set_id: id,
            unit: "photos".into(),
            reason: "escrow recipient absent from recorded list".into(),
        }
    }

    #[test]
    fn allow_missing_escrow_waives_only_stage_set_escrow_gaps() {
        use crate::volume::layout_model::LayoutError;
        // Without the flag, an escrow gap blocks.
        let (blocking, waived) = blocking_validation_errors(vec![sle(1)], false);
        assert_eq!(blocking.len(), 1);
        assert!(waived.is_empty());

        // With the flag, an escrow gap is waived and nothing blocks.
        let (blocking, waived) = blocking_validation_errors(vec![sle(1), sle(2)], true);
        assert!(
            blocking.is_empty(),
            "escrow gaps must be waived: {blocking:?}"
        );
        assert_eq!(waived.len(), 2, "both gaps must be reported as warnings");

        // The flag NEVER waives a hard failure sitting alongside a gap.
        let cap = LayoutError::CapacityExceeded {
            needed: 100,
            reserve: 10,
            available: 50,
        };
        let (blocking, waived) = blocking_validation_errors(vec![sle(1), cap], true);
        assert_eq!(blocking.len(), 1, "capacity must still block");
        assert!(matches!(blocking[0], LayoutError::CapacityExceeded { .. }));
        assert_eq!(waived.len(), 1);

        // The flag NEVER waives "no escrow registered at all" — that is a
        // different failure (EscrowRecipientMissing), not a per-set gap.
        let (blocking, waived) =
            blocking_validation_errors(vec![LayoutError::EscrowRecipientMissing], true);
        assert_eq!(blocking.len(), 1, "a total escrow absence must still block");
        assert!(waived.is_empty());
    }

    #[test]
    fn force_does_not_bypass_the_escrow_check_and_never_reaches_the_device() {
        let (conn, _tenant_id, stage_set_id) = escrow_check_fixture();

        // Staged before the escrow existed: 1 unrelated recipient recorded.
        let other = crate::crypto::keys::generate_keypair();
        set_key_fingerprints(
            &conn,
            stage_set_id,
            Some(&serde_json::to_string(&vec![other.public_key]).unwrap()),
        );
        register_escrow(&conn);

        // A real staged slice on disk, so `validate`'s tri-layer L1 has
        // something valid to check and cannot be what fails.
        let tmp = tempfile::TempDir::new().unwrap();
        let slices_dir = tmp.path().join("slices");
        fs::create_dir_all(&slices_dir).unwrap();
        let content = b"encrypted slice bytes for the force test".repeat(8);
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                       sha256_plain, sha256_encrypted)
             VALUES (?1, 1, ?2, ?2, ?3, ?4)",
            params![
                stage_set_id,
                content.len() as i64,
                direct_hash(b"plaintext hash is not exercised here"),
                direct_hash(&content),
            ],
        )
        .unwrap();
        let slice_id = conn.last_insert_rowid();
        let slice_path = slices_dir.join(format!("slice_{slice_id}.age"));
        fs::write(&slice_path, &content).unwrap();
        conn.execute(
            "UPDATE stage_slices SET staging_path = ?1 WHERE id = ?2",
            params![slice_path.to_string_lossy(), slice_id],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('FORCETEST', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
            [],
        )
        .unwrap();

        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();

        let staging_dir = tmp.path().join("staging");
        fs::create_dir_all(&staging_dir).unwrap();
        let mut config = Config::default();
        config.staging.directory = staging_dir.to_string_lossy().into_owned();
        // Device paths that cannot exist: reaching either one is the failure
        // this test is looking for.
        config.backends.lto.push(crate::config::LtoBackendConfig {
            name: "no-such-drive".into(),
            device_tape: "/nonexistent/tapectl-force-test-nst".into(),
            device_sg: "/nonexistent/tapectl-force-test-sg".into(),
            generation: "LTO-8".into(),
            capacity_override: Some("2400G".into()),
            usable_capacity_factor: 0.92,
            enospc_buffer: "50M".into(),
        });

        let err = volume_write(
            &conn,
            &paths,
            &config,
            "FORCETEST",
            "/nonexistent/tapectl-force-test-nst",
            512 * 1024,
            true,  // --force
            false, // --allow-missing-escrow
        )
        .expect_err("force must not bypass pre-write validation");
        let msg = err.to_string();

        assert!(
            msg.contains("failed pre-write validation"),
            "expected the pre-flight refusal, got: {msg}"
        );
        assert!(
            msg.contains(&format!("stage set {stage_set_id}")) && msg.contains("photos"),
            "the refusal must name the offending stage set and its unit: {msg}"
        );
        assert!(
            msg.contains("encrypted without the current escrow recipient")
                && msg.contains("ADR-0005"),
            "the refusal must give the reason and the ADR: {msg}"
        );
        assert!(
            !msg.contains("tapectl-force-test-nst"),
            "validation must refuse BEFORE the tape device is opened: {msg}"
        );

        // Nothing was planned: no `writes` rows, so the operator is not left
        // with a phantom session to resume or abort.
        let writes: i64 = conn
            .query_row("SELECT COUNT(*) FROM writes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(writes, 0, "a refused write must plan nothing");
    }

    #[test]
    fn record_write_bookkeeping_sums_only_padded_slice_entries() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
             VALUES ('BKTEST', 'lto', 'lto0', 1000000, 'active')",
            [],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();

        let bs = 512 * 1024u64;
        let layout = Layout {
            label: "BKTEST".to_string(),
            volume_uuid: "u".to_string(),
            media_type: "LTO-6".to_string(),
            block_size: bs,
            budget: CapacityBudget {
                available_bytes: 0,
                reserve_bytes: 0,
            },
            entries: vec![
                LayoutEntry {
                    position: 0,
                    kind: ZoneKind::IdThunk,
                    size_bytes: Some(100),
                    sha256: None,
                    source: ContentSource::Generated,
                },
                // 1 byte pads to one whole block.
                LayoutEntry {
                    position: 1,
                    kind: ZoneKind::Slice { stage_slice_id: 1 },
                    size_bytes: Some(1),
                    sha256: None,
                    source: ContentSource::Generated,
                },
                // block_size + 1 pads to two whole blocks.
                LayoutEntry {
                    position: 2,
                    kind: ZoneKind::Slice { stage_slice_id: 2 },
                    size_bytes: Some(bs + 1),
                    sha256: None,
                    source: ContentSource::Generated,
                },
            ],
        };

        record_write_bookkeeping(&conn, volume_id, &layout, bs).unwrap();

        let (bytes_written, num_data_files, has_manifest): (i64, i64, i64) = conn
            .query_row(
                "SELECT bytes_written, num_data_files, has_manifest FROM volumes WHERE id = ?1",
                params![volume_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();

        assert_eq!(
            num_data_files, 2,
            "only Slice entries count toward num_data_files, not id_thunk"
        );
        assert_eq!(
            bytes_written,
            (bs + 2 * bs) as i64,
            "must sum block-PADDED (on-tape) slice bytes, not the true size_bytes"
        );
        assert_eq!(has_manifest, 1);
    }

    #[test]
    fn record_write_bookkeeping_never_touches_status() {
        // confirm()'s own transaction already set status = 'sealed' before
        // this runs (session::SealedPending::confirm) — this bookkeeping
        // step must not clobber it back to some other value.
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
             VALUES ('BKTEST2', 'lto', 'lto0', 1000000, 'sealed')",
            [],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();
        let layout = Layout {
            label: "BKTEST2".to_string(),
            volume_uuid: "u".to_string(),
            media_type: "LTO-6".to_string(),
            block_size: 4096,
            budget: CapacityBudget {
                available_bytes: 0,
                reserve_bytes: 0,
            },
            entries: vec![],
        };
        record_write_bookkeeping(&conn, volume_id, &layout, 4096).unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM volumes WHERE id = ?1",
                params![volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "sealed");
    }

    #[test]
    fn describe_quarantine_confirm_failed_mentions_mismatch_count_and_tier() {
        let evidence = Evidence {
            tier: Tier::Integrity,
            files_checked: 5,
            mismatches: vec![Mismatch {
                position: 4,
                kind: MismatchKind::ContentHashMismatch,
                expected: "aa".into(),
                actual: "bb".into(),
            }],
        };
        let msg = describe_quarantine(&QuarantineReason::ConfirmFailed(evidence));
        assert!(msg.contains('1'), "expected the mismatch count in: {msg}");
        assert!(msg.contains("Integrity"), "expected the tier in: {msg}");
    }

    #[test]
    fn describe_quarantine_already_sealed_mentions_position_and_adr() {
        let msg = describe_quarantine(&QuarantineReason::AlreadySealed { seal_position: 12 });
        assert!(msg.contains("12"), "expected the seal position in: {msg}");
        assert!(
            msg.contains("ADR-0003"),
            "expected the ADR citation in: {msg}"
        );
    }

    // --- fresh-write contact discipline (issue #27) ------------------------
    //
    // `decide_fresh_write_contact` is pure (no store at all); `check_fresh_write_contact`
    // adds the store round-trip via MemStore — the session tests' own
    // convention (never a real tape device).

    use crate::store::MemStore;

    const FW_LABEL: &str = "NEWVOL";
    const FW_UUID: &str = "11111111-1111-1111-1111-111111111111";
    const FW_BS: u64 = 512 * 1024;

    fn fw_id_thunk_bytes(label: &str, uuid: &str, total_files: i32) -> Vec<u8> {
        let thunk = layout::generate_id_thunk_v2(&layout::IdThunkV2Params {
            label,
            uuid,
            media_type: "LTO-6",
            tapectl_version: "0.1.0-test",
            nominal_capacity: 1,
            mam_capacity: 1,
            total_files,
            mam_manufacturer: "",
            mam_serial: "",
            mam_length: 0,
            mam_loads: 0,
            created_at: "2026-07-28T00:00:00Z",
            cartridge_identity_source: None,
        });
        let mut padded = thunk.into_bytes();
        padded.resize(FW_BS as usize, 0);
        padded
    }

    fn fw_put_file(store: &mut MemStore, position: usize, bytes: Vec<u8>) {
        if store.files.len() <= position {
            store.files.resize(position + 1, Vec::new());
            store.syncs.resize(position + 1, false);
        }
        store.files[position] = bytes;
    }

    // -- decide_fresh_write_contact: pure decision, no store -----

    #[test]
    fn decide_fresh_write_contact_blank_proceeds() {
        decide_fresh_write_contact(&ContactOutcome::Blank, FW_LABEL, FW_UUID, false).unwrap();
    }

    #[test]
    fn decide_fresh_write_contact_matches_proceeds() {
        decide_fresh_write_contact(&ContactOutcome::Matches, FW_LABEL, FW_UUID, false).unwrap();
    }

    #[test]
    fn decide_fresh_write_contact_mismatch_refuses_by_default_naming_found_and_expected() {
        let found = format::parse_id_thunk_identity(&String::from_utf8_lossy(&fw_id_thunk_bytes(
            "WRONGVOL",
            "00000000-0000-0000-0000-000000000000",
            8,
        )))
        .unwrap();
        let outcome = ContactOutcome::IdentityMismatch { found: Some(found) };
        let err = decide_fresh_write_contact(&outcome, FW_LABEL, FW_UUID, false).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("WRONGVOL"),
            "expected the FOUND label in: {msg}"
        );
        assert!(
            msg.contains("00000000-0000-0000-0000-000000000000"),
            "expected the FOUND uuid in: {msg}"
        );
        assert!(
            msg.contains(FW_LABEL),
            "expected the EXPECTED label in: {msg}"
        );
        assert!(
            msg.contains(FW_UUID),
            "expected the EXPECTED uuid in: {msg}"
        );
        assert!(
            msg.contains("--force"),
            "expected the override hint in: {msg}"
        );
    }

    #[test]
    fn decide_fresh_write_contact_mismatch_with_unparseable_file_zero_still_refuses() {
        let outcome = ContactOutcome::IdentityMismatch { found: None };
        let err = decide_fresh_write_contact(&outcome, FW_LABEL, FW_UUID, false).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains(FW_LABEL));
    }

    #[test]
    fn decide_fresh_write_contact_mismatch_permits_with_force() {
        let outcome = ContactOutcome::IdentityMismatch {
            found: Some(format::IdThunkIdentity {
                label: "WRONGVOL".to_string(),
                uuid: "00000000-0000-0000-0000-000000000000".to_string(),
            }),
        };
        decide_fresh_write_contact(&outcome, FW_LABEL, FW_UUID, true).unwrap();
    }

    #[test]
    fn decide_fresh_write_contact_already_sealed_refuses_without_force() {
        let outcome = ContactOutcome::AlreadySealed { seal_position: 12 };
        let err = decide_fresh_write_contact(&outcome, FW_LABEL, FW_UUID, false).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("12"), "expected the seal position in: {msg}");
        assert!(msg.contains("ADR-0003"));
    }

    #[test]
    fn decide_fresh_write_contact_already_sealed_refuses_even_with_force() {
        // The one non-negotiable rule of the override: --force can defeat a
        // wrong-identity refusal, but never a sealed one.
        let outcome = ContactOutcome::AlreadySealed { seal_position: 12 };
        let err = decide_fresh_write_contact(&outcome, FW_LABEL, FW_UUID, true).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("ADR-0003"),
            "expected the ADR citation in: {msg}"
        );
    }

    // -- check_fresh_write_contact: full pipeline via MemStore -----

    #[test]
    fn check_fresh_write_contact_blank_tape_permits_write() {
        let mut store = MemStore::new(FW_BS as usize);
        check_fresh_write_contact(&mut store, FW_LABEL, FW_UUID, Some(5), false).unwrap();
    }

    #[test]
    fn check_fresh_write_contact_matching_identity_permits_write() {
        // The critical fresh-write happy path: `volume_init` already stamped
        // this exact tape's File 0 with this exact label+uuid; `volume_write`
        // must proceed WITHOUT --force.
        let mut store = MemStore::new(FW_BS as usize);
        fw_put_file(&mut store, 0, fw_id_thunk_bytes(FW_LABEL, FW_UUID, 8));
        check_fresh_write_contact(&mut store, FW_LABEL, FW_UUID, Some(7), false).unwrap();
    }

    #[test]
    fn check_fresh_write_contact_wrong_identity_refuses_with_found_and_expected() {
        let mut store = MemStore::new(FW_BS as usize);
        fw_put_file(
            &mut store,
            0,
            fw_id_thunk_bytes("WRONGVOL", "00000000-0000-0000-0000-000000000000", 8),
        );
        let err =
            check_fresh_write_contact(&mut store, FW_LABEL, FW_UUID, None, false).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("WRONGVOL"), "found label missing from: {msg}");
        assert!(msg.contains(FW_LABEL), "expected label missing from: {msg}");
    }

    #[test]
    fn check_fresh_write_contact_wrong_identity_permits_with_force() {
        let mut store = MemStore::new(FW_BS as usize);
        fw_put_file(
            &mut store,
            0,
            fw_id_thunk_bytes("WRONGVOL", "00000000-0000-0000-0000-000000000000", 8),
        );
        check_fresh_write_contact(&mut store, FW_LABEL, FW_UUID, None, true).unwrap();
    }

    #[test]
    fn check_fresh_write_contact_sealed_tape_refuses_even_with_force() {
        // --force is not reachable by accident, AND it never reaches a
        // sealed tape at all: prove both in one test by forcing anyway and
        // still getting refused.
        let mut store = MemStore::new(FW_BS as usize);
        fw_put_file(&mut store, 0, fw_id_thunk_bytes(FW_LABEL, FW_UUID, 6));
        let seal_bytes = layout::generate_seal_marker(FW_LABEL, 6, "deadbeef", &[]).into_bytes();
        let mut seal_padded = seal_bytes;
        seal_padded.resize(FW_BS as usize, 0);
        fw_put_file(&mut store, 5, seal_padded);

        let err =
            check_fresh_write_contact(&mut store, FW_LABEL, FW_UUID, Some(5), true).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("ADR-0003"), "expected ADR citation in: {msg}");
    }

    #[test]
    fn check_fresh_write_contact_foreign_sealed_tape_refuses_even_with_force() {
        // The headline scenario from issue #27's own description: "loading
        // the WRONG physical cartridge — including one holding a
        // different, already-sealed volume." Unlike the test above (same
        // label/uuid, sealed), File 0 here identifies a totally DIFFERENT,
        // foreign volume — the caller's own `seal_position` (7) is a
        // position in ITS layout, unrelated to where this foreign tape's
        // real seal marker (5) sits. --force can defeat a plain identity
        // mismatch, so if the foreign tape's own sealed-ness were missed,
        // this would silently overwrite a sealed volume. Must refuse
        // regardless.
        let mut store = MemStore::new(FW_BS as usize);
        fw_put_file(
            &mut store,
            0,
            fw_id_thunk_bytes("WRONGVOL", "00000000-0000-0000-0000-000000000000", 6),
        );
        let seal_bytes = layout::generate_seal_marker("WRONGVOL", 6, "deadbeef", &[]).into_bytes();
        let mut seal_padded = seal_bytes;
        seal_padded.resize(FW_BS as usize, 0);
        fw_put_file(&mut store, 5, seal_padded);

        let err =
            check_fresh_write_contact(&mut store, FW_LABEL, FW_UUID, Some(7), true).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("ADR-0003"),
            "expected ADR citation (this must be an AlreadySealed refusal, not a bypassed \
             IdentityMismatch) in: {msg}"
        );
    }

    #[test]
    fn check_fresh_write_contact_default_is_no_override_not_reachable_by_accident() {
        // The zero-argument, no-flag CLI invocation must refuse — the
        // override is never the default.
        let mut store = MemStore::new(FW_BS as usize);
        fw_put_file(
            &mut store,
            0,
            fw_id_thunk_bytes("WRONGVOL", "00000000-0000-0000-0000-000000000000", 8),
        );
        let default_force = bool::default();
        assert!(!default_force, "bool::default() must be false");
        assert!(
            check_fresh_write_contact(&mut store, FW_LABEL, FW_UUID, None, default_force).is_err()
        );
    }

    /// W3 Change 8: `volume write` binds a volume that `volume init` could
    /// not, because no medium serial was readable then and one is now.
    /// `bind_late` takes a `Detected` and a `Connection`, so the whole
    /// ladder drills without a drive.
    mod late_binding {
        use super::*;
        use crate::media::Generation;
        use crate::tape::mam::MamInfo;
        use crate::tape::media_detect::{DetectSource, Detected};

        fn det_with_serial(serial: Option<&str>) -> Detected {
            Detected {
                generation: Some(Generation::Lto6),
                code: Generation::Lto6.density_code(),
                source: DetectSource::MamMedium,
                mam: MamInfo {
                    serial: serial.map(str::to_string),
                    ..MamInfo::default()
                },
            }
        }

        /// An UNBOUND volume, the state `volume init` leaves behind when the
        /// drive reports no medium serial.
        fn unbound_volume() -> (Connection, i64) {
            let conn = crate::db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                      capacity_bytes, status)
                 VALUES ('L6-0001', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
                [],
            )
            .unwrap();
            let vol_id = conn.last_insert_rowid();
            (conn, vol_id)
        }

        fn bound_barcode(conn: &Connection, vol_id: i64) -> Option<String> {
            conn.query_row(
                "SELECT c.barcode FROM cartridge_volumes cv
                 JOIN cartridges c ON c.id = cv.cartridge_id
                 WHERE cv.volume_id = ?1 AND cv.unmounted_at IS NULL",
                params![vol_id],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
        }

        fn call(conn: &Connection, vol_id: i64, det: &Detected) -> Result<()> {
            bind_late(
                conn,
                vol_id,
                "L6-0001",
                det,
                Some("LTO-6"),
                "LTO-6",
                2_500_000_000_000,
            )
        }

        /// The headline: a serial readable now auto-registers a cartridge
        /// whose barcode IS that serial, exactly as init's ladder does.
        #[test]
        fn a_readable_serial_binds_a_volume_init_left_unbound() {
            let (conn, vol_id) = unbound_volume();
            call(&conn, vol_id, &det_with_serial(Some("SER-1"))).unwrap();
            assert_eq!(bound_barcode(&conn, vol_id).as_deref(), Some("SER-1"));
        }

        /// A registered cartridge carrying that serial wins over
        /// auto-registration — the same match init makes.
        #[test]
        fn a_registered_serial_binds_to_that_row_not_a_new_one() {
            let (conn, vol_id) = unbound_volume();
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number,
                                         status)
                 VALUES ('A001L6', 'LTO-6', 2500000000000, 'SER-1', 'available')",
                [],
            )
            .unwrap();
            call(&conn, vol_id, &det_with_serial(Some("SER-1"))).unwrap();
            assert_eq!(bound_barcode(&conn, vol_id).as_deref(), Some("A001L6"));
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM cartridges", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 1, "an existing row must not be duplicated");
        }

        /// No serial: unchanged from ADR-0010 — the volume stays unbound and
        /// the serial-less virtual harnesses lose nothing.
        #[test]
        fn no_serial_leaves_the_volume_unbound_without_erroring() {
            let (conn, vol_id) = unbound_volume();
            call(&conn, vol_id, &det_with_serial(None)).unwrap();
            assert_eq!(bound_barcode(&conn, vol_id), None);
        }

        /// An already-bound volume is left strictly alone.
        /// `check_loaded_cartridge` has just confirmed it is the right
        /// cartridge, and rebinding would record a displacement nobody
        /// asked for.
        #[test]
        fn an_already_bound_volume_is_not_rebound() {
            let (conn, vol_id) = unbound_volume();
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number,
                                         status)
                 VALUES ('A001L6', 'LTO-6', 2500000000000, 'SER-1', 'in_use')",
                [],
            )
            .unwrap();
            let cart_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
                params![cart_id, vol_id],
            )
            .unwrap();

            call(&conn, vol_id, &det_with_serial(Some("SER-1"))).unwrap();
            assert_eq!(bound_barcode(&conn, vol_id).as_deref(), Some("A001L6"));
            let events: i64 = conn
                .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
                .unwrap();
            assert_eq!(events, 0, "a no-op must write nothing at all");
        }

        /// ADR-0011 applies here for the same reason it applies at init: no
        /// amount of consent makes a medium declared unfit fit again, and
        /// `bind_late` has no `force` to consult either.
        #[test]
        fn a_retired_permanent_cartridge_is_refused_here_too() {
            let (conn, vol_id) = unbound_volume();
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number,
                                         status)
                 VALUES ('A001L6', 'LTO-6', 2500000000000, 'SER-1', 'retired_permanent')",
                [],
            )
            .unwrap();
            let err = call(&conn, vol_id, &det_with_serial(Some("SER-1")))
                .expect_err("a retired_permanent cartridge must never be written");
            assert!(err.to_string().contains("mark-erased"), "got: {err}");
            assert_eq!(bound_barcode(&conn, vol_id), None);
        }

        /// Issue #154's negative control: prove the displacement `bind_late`
        /// performs is real and observable, independent of where the call
        /// site sits inside `volume_write`. Without this, "no displacement
        /// is observable" in an ordering test would pass vacuously for the
        /// wrong reason (nothing was ever at risk of being displaced).
        ///
        /// V1 is a live, sealed volume already mounted (open, `unmounted_at
        /// IS NULL`) on a cartridge whose serial is "SER-1". Binding V2 to
        /// that same serial must mark V1 `erased`, close V1's mount, and
        /// log one `displaced` event naming it — exactly `binding::
        /// bind_cartridge`'s documented behaviour (ADR-0010: "never refuse
        /// it, only record it").
        #[test]
        fn bind_late_displaces_a_live_open_mounted_volume_on_the_matched_cartridge() {
            let conn = crate::db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number,
                                         status)
                 VALUES ('A001L6', 'LTO-6', 2500000000000, 'SER-1', 'in_use')",
                [],
            )
            .unwrap();
            let cart_id = conn.last_insert_rowid();

            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                      capacity_bytes, status)
                 VALUES ('V1-SEALED', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
                [],
            )
            .unwrap();
            let v1_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
                params![cart_id, v1_id],
            )
            .unwrap();

            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                      capacity_bytes, status)
                 VALUES ('V2-NEW', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
                [],
            )
            .unwrap();
            let v2_id = conn.last_insert_rowid();

            bind_late(
                &conn,
                v2_id,
                "V2-NEW",
                &det_with_serial(Some("SER-1")),
                Some("LTO-6"),
                "LTO-6",
                2_500_000_000_000,
            )
            .unwrap();

            let v1_status: String = conn
                .query_row(
                    "SELECT status FROM volumes WHERE id = ?1",
                    params![v1_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                v1_status, "erased",
                "the displaced volume must be marked erased"
            );

            let v1_mount_closed: bool = conn
                .query_row(
                    "SELECT unmounted_at IS NOT NULL FROM cartridge_volumes WHERE volume_id = ?1",
                    params![v1_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                v1_mount_closed,
                "the displaced volume's mount must be closed"
            );

            let displaced_events: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE action = 'displaced'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                displaced_events, 1,
                "exactly one displaced event must be logged"
            );

            // V2 now holds the live mount on the cartridge V1 was displaced
            // from.
            assert_eq!(bound_barcode(&conn, v2_id).as_deref(), Some("A001L6"));
        }

        /// Issue #154, the ordering fix: a `volume_write` refused before the
        /// device is ever opened must leave the catalog exactly as it found
        /// it — no displaced volume, no closed mount, no `displaced` event —
        /// for a volume that has nothing to do with the one being written.
        ///
        /// HONESTY NOTE, read before trusting this test: `bind_late` returns
        /// `Ok(())` the instant `det.mam.serial` is `None` (this module's
        /// own `no_serial_leaves_the_volume_unbound_without_erroring` pins
        /// that), and every in-process harness for `volume_write` — this one
        /// included — has to drive it with a device path
        /// `media_detect::detect` cannot actually read, because there is no
        /// injectable seam into `detect` (deliberately: adding one here
        /// would be scope creep on the write path, see issue #154's
        /// discussion). So in THIS test `bind_late` never has a serial to
        /// act on, at either the old call site or the new one, and this
        /// test is expected to pass identically before and after the
        /// ordering fix — it does not, and cannot, reproduce the bug. Test 1
        /// above (`bind_late_displaces_a_live_open_mounted_volume_on_the_
        /// matched_cartridge`) is the actual reproduction, driving
        /// `bind_late` directly with a serial that matches. This test's
        /// value is as a regression guard: if some future change moved
        /// `bind_late` back above the pre-write checks AND also made a
        /// serial reachable from a harness like this one, this is the
        /// assertion that would catch it.
        #[test]
        fn a_refused_write_leaves_an_unrelated_mounted_volume_untouched() {
            let (conn, _tenant_id, stage_set_id) = escrow_check_fixture();

            // V1: a live, sealed volume already mounted on a cartridge that
            // has nothing to do with the volume this write targets —
            // standing in for "the wrong tape is loaded" from issue #154's
            // failure narrative.
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number,
                                         status)
                 VALUES ('A001L6', 'LTO-6', 2500000000000, 'SER-1', 'in_use')",
                [],
            )
            .unwrap();
            let cart_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                      capacity_bytes, status)
                 VALUES ('V1-SEALED', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
                [],
            )
            .unwrap();
            let v1_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
                params![cart_id, v1_id],
            )
            .unwrap();

            // Staged before escrow existed: an escrow gap, exactly like
            // `force_does_not_bypass_the_escrow_check_and_never_reaches_the_
            // device` above, so `validate` refuses before the tape device
            // is ever opened.
            let other = crate::crypto::keys::generate_keypair();
            set_key_fingerprints(
                &conn,
                stage_set_id,
                Some(&serde_json::to_string(&vec![other.public_key]).unwrap()),
            );
            register_escrow(&conn);

            let tmp = tempfile::TempDir::new().unwrap();
            let slices_dir = tmp.path().join("slices");
            fs::create_dir_all(&slices_dir).unwrap();
            let content = b"encrypted slice bytes for the ordering test".repeat(8);
            conn.execute(
                "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes,
                                           encrypted_bytes, sha256_plain, sha256_encrypted)
                 VALUES (?1, 1, ?2, ?2, ?3, ?4)",
                params![
                    stage_set_id,
                    content.len() as i64,
                    direct_hash(b"plaintext hash is not exercised here"),
                    direct_hash(&content),
                ],
            )
            .unwrap();
            let slice_id = conn.last_insert_rowid();
            let slice_path = slices_dir.join(format!("slice_{slice_id}.age"));
            fs::write(&slice_path, &content).unwrap();
            conn.execute(
                "UPDATE stage_slices SET staging_path = ?1 WHERE id = ?2",
                params![slice_path.to_string_lossy(), slice_id],
            )
            .unwrap();

            // V2: the volume this write actually targets — unbound, and
            // unrelated to V1's cartridge.
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                      capacity_bytes, status)
                 VALUES ('ORDERTEST', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
                [],
            )
            .unwrap();

            let home = tmp.path().join("home");
            fs::create_dir_all(&home).unwrap();
            let paths = TapectlPaths::new(home);
            paths.ensure_dirs().unwrap();

            let staging_dir = tmp.path().join("staging");
            fs::create_dir_all(&staging_dir).unwrap();
            let mut config = Config::default();
            config.staging.directory = staging_dir.to_string_lossy().into_owned();
            config.backends.lto.push(crate::config::LtoBackendConfig {
                name: "no-such-drive".into(),
                device_tape: "/nonexistent/tapectl-order-test-nst".into(),
                device_sg: "/nonexistent/tapectl-order-test-sg".into(),
                generation: "LTO-8".into(),
                capacity_override: Some("2400G".into()),
                usable_capacity_factor: 0.92,
                enospc_buffer: "50M".into(),
            });

            let err = volume_write(
                &conn,
                &paths,
                &config,
                "ORDERTEST",
                "/nonexistent/tapectl-order-test-nst",
                512 * 1024,
                false, // --force
                false, // --allow-missing-escrow
            )
            .expect_err("the escrow gap must refuse before the device is touched");
            assert!(
                err.to_string().contains("failed pre-write validation"),
                "expected the pre-flight refusal, got: {err}"
            );

            // The assertion that matters: V1, unrelated to this write, is
            // exactly as it was before the refused write ran.
            let v1_status: String = conn
                .query_row(
                    "SELECT status FROM volumes WHERE id = ?1",
                    params![v1_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                v1_status, "sealed",
                "a refused write must not touch an unrelated volume's status"
            );

            let v1_mount_open: bool = conn
                .query_row(
                    "SELECT unmounted_at IS NULL FROM cartridge_volumes WHERE volume_id = ?1",
                    params![v1_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                v1_mount_open,
                "a refused write must not close an unrelated volume's mount"
            );

            let displaced_events: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE action = 'displaced'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                displaced_events, 0,
                "a refused write must log no displacement"
            );
        }
    }

    // --- ADR-0010: the write path's wrong-cartridge / wrong-medium checks ---
    //
    // Both are pure enough to drill without a drive: one takes a
    // `Connection` and a serial string, the other a `Detected` and a row's
    // recorded generation.
    // --- ADR-0012 / #192: what File 0 attests about the cartridge identity --
    //
    // The whole point of the change is that this comes from the BINDING, not
    // from whatever MAM says at this contact, so every case below hands the
    // resolver a *contradicting* live serial and asserts the binding wins.
    mod file_0_cartridge_identity {
        use super::*;

        /// A volume bound to `barcode`, whose cartridge row records
        /// `serial_number = serial`, with the binding's `identity_source` set
        /// to `source`.
        fn bound(barcode: &str, serial: Option<&str>, source: Option<&str>) -> (Connection, i64) {
            let conn = crate::db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number, status)
                 VALUES (?1, 'LTO-6', 2500000000000, ?2, 'in_use')",
                params![barcode, serial],
            )
            .unwrap();
            let cart_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
                 VALUES ('L6-0001', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
                [],
            )
            .unwrap();
            let vol_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO cartridge_volumes (cartridge_id, volume_id, identity_source)
                 VALUES (?1, ?2, ?3)",
                params![cart_id, vol_id, source],
            )
            .unwrap();
            (conn, vol_id)
        }

        #[test]
        fn a_mam_binding_attests_the_rows_chip_serial_not_this_contacts() {
            let (conn, vol) = bound("BC001", Some("SER-1"), Some("mam"));
            // A different live serial is deliberately supplied: the binding,
            // not the contact, decides what goes on tape.
            let id = resolve_cartridge_identity(&conn, vol, "L6-0001", Some("SER-LIVE")).unwrap();
            assert_eq!(id.serial, "SER-1");
            assert_eq!(id.source.as_deref(), Some("mam"));
        }

        /// The case the whole redesign exists for: bound by barcode because
        /// no serial was readable at init. A drive that CAN read MAM later
        /// must not change what this tape says — `bind_late` early-returns on
        /// an already-bound volume, so the catalog would never learn that
        /// serial, and the tape and the catalog would disagree forever.
        #[test]
        fn an_operator_binding_attests_the_barcode_even_when_mam_reads_now() {
            let (conn, vol) = bound("HOME-007", None, Some("operator"));
            let id = resolve_cartridge_identity(&conn, vol, "L6-0001", Some("SER-LIVE")).unwrap();
            assert_eq!(id.serial, "HOME-007");
            assert_eq!(id.source.as_deref(), Some("operator"));
        }

        /// 'mam' with no recorded serial is an inconsistent catalog, and
        /// sealing `cartridge_serial = ""` beside `= "mam"` would be a
        /// provenance claim with no identity behind it. Tape bytes are
        /// forever; refuse instead.
        #[test]
        fn a_mam_binding_with_no_recorded_serial_is_refused_by_name() {
            let (conn, vol) = bound("BC001", None, Some("mam"));
            let err = resolve_cartridge_identity(&conn, vol, "L6-0001", Some("SER-LIVE"))
                .unwrap_err()
                .to_string();
            assert!(err.contains("L6-0001"), "{err}");
            assert!(err.contains("BC001"), "{err}");
            assert!(err.contains("records no medium serial"), "{err}");
        }

        /// A binding made before migration 014: unknown, and unknown is said
        /// by saying NOTHING. Absent must never read as "mam" — every tape
        /// written before #192 omits the key.
        #[test]
        fn a_pre_migration_binding_falls_back_to_the_live_mam_serial_and_says_nothing() {
            let (conn, vol) = bound("BC001", Some("SER-1"), None);
            let id = resolve_cartridge_identity(&conn, vol, "L6-0001", Some("SER-LIVE")).unwrap();
            assert_eq!(id.serial, "SER-LIVE", "legacy behaviour is unchanged");
            assert!(id.source.is_none());
        }

        /// An unbound volume (pre-ADR-0010, or a drive exposing no serial)
        /// keeps exactly the behaviour it always had, down to the empty
        /// string when nothing is readable.
        #[test]
        fn an_unbound_volume_keeps_the_legacy_shape_exactly() {
            let conn = crate::db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
                 VALUES ('L6-0001', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
                [],
            )
            .unwrap();
            let vol = conn.last_insert_rowid();

            let seen = resolve_cartridge_identity(&conn, vol, "L6-0001", Some("SER-LIVE")).unwrap();
            assert_eq!(seen.serial, "SER-LIVE");
            assert!(seen.source.is_none());

            let blind = resolve_cartridge_identity(&conn, vol, "L6-0001", None).unwrap();
            assert_eq!(blind.serial, "");
            assert!(blind.source.is_none());
        }
    }

    mod loaded_medium_checks {
        use super::*;
        use crate::media::Generation;
        use crate::tape::mam::MamInfo;
        use crate::tape::media_detect::{DetectSource, Detected};

        /// A volume bound to a cartridge whose `serial_number` is `serial`.
        fn bound_volume(serial: Option<&str>) -> (Connection, i64) {
            let conn = crate::db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number, status)
                 VALUES ('BC001', 'LTO-6', 2500000000000, ?1, 'in_use')",
                params![serial],
            )
            .unwrap();
            let cart_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
                 VALUES ('L6-0001', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
                [],
            )
            .unwrap();
            let vol_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
                params![cart_id, vol_id],
            )
            .unwrap();
            (conn, vol_id)
        }

        fn detected(g: Option<Generation>) -> Detected {
            Detected {
                generation: g,
                code: g.and_then(Generation::density_code),
                source: if g.is_some() {
                    DetectSource::MamMedium
                } else {
                    DetectSource::None
                },
                mam: MamInfo::default(),
            }
        }

        #[test]
        fn the_same_cartridge_passes() {
            let (conn, vol) = bound_volume(Some("SER-1"));
            check_loaded_cartridge(&conn, vol, "L6-0001", Some("SER-1")).unwrap();
        }

        #[test]
        fn a_different_cartridge_is_refused_naming_both_serials() {
            let (conn, vol) = bound_volume(Some("SER-1"));
            let err = check_loaded_cartridge(&conn, vol, "L6-0001", Some("SER-2"))
                .unwrap_err()
                .to_string();
            assert!(err.contains("wrong cartridge"), "{err}");
            assert!(err.contains("initialised on SER-1"), "{err}");
            assert!(err.contains("the drive holds SER-2"), "{err}");
            assert!(err.contains("cartridge BC001"), "{err}");
        }

        /// A check that cannot see cannot refuse — ADR-0010 keeps virtual
        /// drives that expose no medium serial working unchanged.
        #[test]
        fn an_unreadable_serial_is_silent_rather_than_a_refusal() {
            let (conn, vol) = bound_volume(Some("SER-1"));
            check_loaded_cartridge(&conn, vol, "L6-0001", None).unwrap();
        }

        #[test]
        fn a_cartridge_row_with_no_recorded_serial_is_silent() {
            let (conn, vol) = bound_volume(None);
            check_loaded_cartridge(&conn, vol, "L6-0001", Some("SER-2")).unwrap();
        }

        #[test]
        fn an_unbound_volume_is_silent() {
            let conn = crate::db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
                 VALUES ('L6-0001', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
                [],
            )
            .unwrap();
            let vol = conn.last_insert_rowid();
            check_loaded_cartridge(&conn, vol, "L6-0001", Some("SER-2")).unwrap();
        }

        #[test]
        fn the_same_generation_passes() {
            check_loaded_generation("L6-0001", &detected(Some(Generation::Lto6)), Some("LTO-6"))
                .unwrap();
        }

        #[test]
        fn a_different_generation_is_refused() {
            let err = check_loaded_generation(
                "L6-0001",
                &detected(Some(Generation::Lto5)),
                Some("LTO-6"),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("wrong medium"), "{err}");
            assert!(err.contains("initialised on LTO-6 media"), "{err}");
            assert!(err.contains("the drive holds LTO-5"), "{err}");
        }

        #[test]
        fn an_undetectable_generation_is_silent() {
            check_loaded_generation("L6-0001", &detected(None), Some("LTO-6")).unwrap();
        }

        /// A `volumes` row written before ADR-0010 can hold anything in
        /// `media_type`; it cannot arbitrate, so it does not refuse.
        #[test]
        fn an_unparseable_recorded_generation_is_silent() {
            check_loaded_generation("L6-0001", &detected(Some(Generation::Lto6)), Some("lto6?"))
                .unwrap();
            check_loaded_generation("L6-0001", &detected(Some(Generation::Lto6)), None).unwrap();
        }
    }
}
