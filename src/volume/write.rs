use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::{collections::HashSet, fs};

use rusqlite::{params, Connection, OptionalExtension};
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::{Config, TapectlPaths};
use crate::db::{events, queries};
use crate::error::{Result, TapectlError};
use crate::policy::coverage;
use crate::staging;
use crate::tape::contact::{self, ContactGuard, ContactSite, ContactSlot, Medium, Operation};
use crate::tape::drive_identity;
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
///    ([`media_detect::detect`]), with `--generation` / a matched cartridge row /
///    the drive's own generation standing in only when nothing is readable
///    ([`media_detect::resolve_media`]). A `--generation` that contradicts a
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
/// Ordering is load-bearing. Every FACT check (detect, the `--generation`
/// contradiction, `can_write`, a `--cartridge` that names no row, a
/// `--cartridge` with no serial match that would displace a live volume) runs
/// before the tape device is opened, so a wrong-tape or wrong-flag run costs
/// nothing. Every catalog MUTATION runs after `check_fresh_write_contact`
/// passes and inside one transaction, so a refused init never displaces a
/// volume in the catalog. `volume_write`'s own late-binding call
/// ([`bind_late`]) honours the same rule for the same reason: it runs after
/// `check_fresh_write_contact` + `reposition_for_resume(0)`, not before
/// `build`/`validate`/`TapeStore::open`, so a write refused at any of those
/// stages cannot leave a committed displacement behind either (issue #154).
/// The refusal when this drive cannot write the medium that is loaded
/// (ADR-0010 decision 2; ADR-0008 Tier 3 — `--force` is not consulted) is
/// [`media_detect::check_drive_can_write`] — moved there (issue #166) so
/// `volume_init`, `volume_write` and `volume_resume` all reach the exact
/// same message rather than three copies that could drift.
#[allow(clippy::too_many_arguments)] // conn/config + label/device/block_size + force + the two ADR-0010 declarations
pub fn volume_init(
    conn: &Connection,
    config: &Config,
    label: &str,
    device: &str,
    block_size: usize,
    force: bool,
    declared_media: Option<&str>,
    cartridge_barcode: Option<&str>,
) -> Result<i64> {
    // The contact cannot be opened here: every refusal above the MAM read
    // below — a label that already exists, no backend, an empty drive — is a
    // command that never reached a cartridge. See [`ContactSlot`].
    let mut contact = ContactSlot::empty();
    let r = volume_init_contacted(
        conn,
        config,
        label,
        device,
        block_size,
        force,
        declared_media,
        cartridge_barcode,
        &mut contact,
    );
    contact.finish_result(r)
}

#[allow(clippy::too_many_arguments)]
fn volume_init_contacted<'c>(
    conn: &'c Connection,
    config: &Config,
    label: &str,
    device: &str,
    block_size: usize,
    force: bool,
    // `--generation <GEN>`: the operator's declaration of the loaded medium's
    // generation. Only consulted when nothing could be detected; an error
    // when it contradicts a detected density code.
    declared_media: Option<&str>,
    // `--cartridge <BARCODE>`: bind to this already-registered cartridge
    // when the medium's serial matches no row (or no serial is readable).
    cartridge_barcode: Option<&str>,
    contact: &mut ContactSlot<'c>,
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
    // `native_generation()` parses the same field with the identical error
    // text `check_drive_can_write` itself uses below, so this and the
    // helper it calls can never drift on what "not a recognised generation"
    // means.
    let drive_gen = backend.native_generation()?;

    // ---- ADR-0010 fact-finding, all before the tape device is opened ----
    //
    // A fast, non-blocking pre-check (issue #152): `detect`'s driver-density
    // fallback opens the tape node with a plain blocking `open()`, which on a
    // drive with no cartridge loaded stalls until the st driver's no-medium
    // timeout expires (~2m05s observed against an empty mhvtl drive) before
    // reporting exactly the "nothing here" that this check reports in well
    // under a second. Loading no cartridge, or loading the wrong drive, is an
    // ordinary operator slip and should be refused immediately, by name,
    // rather than after a silent multi-minute hang. This does not change
    // `detect`'s own ladder for a LOADED medium at all — it only short-
    // circuits the empty-drive case before `detect` is even called.
    //
    // A physical fact, not a risk judgement, same as `can_write` below:
    // `--force` is deliberately NOT consulted. An empty drive has nothing
    // for `--force` to override.
    if crate::tape::media_detect::probe_no_medium(device) {
        return Err(TapectlError::Other(format!(
            "no cartridge loaded in {device}"
        )));
    }
    let det = crate::tape::media_detect::detect(device, &backend.device_sg);
    // THE CONTACT BEGINS HERE: the MAM read above is the first moment this
    // command and a cartridge were in the same drive, and `volume_id` is
    // NULL because no `volumes` row exists yet — `init` creates it below.
    // Everything from here down is inside the contact, including the six
    // ADR-0010/ADR-0012 fact refusals, each of which really did happen with
    // a tape loaded.
    let contact = contact.fill(ContactGuard::open(
        conn,
        config,
        Operation::VolumeInit,
        device,
        None,
        Medium::Observed {
            backend,
            mam: &det.mam,
        },
    ));
    let declared = match declared_media {
        Some(m) => Some(crate::media::Generation::parse(m).ok_or_else(|| {
            TapectlError::Other(format!(
                "--generation {m:?} is not a recognised LTO generation \
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
    // ADR-0012 / ADR-0010's "Correction 2026-09-14": ADR-0010's no-second-gate
    // rule holds only when the cartridge was matched by MAM serial, which
    // needs one on BOTH sides. With none from the drive, or none recorded on
    // the row the typed `--cartridge` named, a row still bound to a live
    // volume is undecidable — that cartridge erased, or a different tape
    // wearing its sticker — so it is refused (issue #155). Here with the other
    // FACT checks, before the drive is opened and before the transaction: a
    // refusal must displace nothing.
    binding::refuse_unwitnessed_displacement(conn, serial, lookup.row.as_ref())?;
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
    // consulted (ADR-0010 decision 2, ADR-0008's tiers). One refusal,
    // written once (issue #166) — `volume_write` and `volume_resume` call
    // the same helper.
    crate::tape::media_detect::check_drive_can_write(backend, generation)?;

    // Decimal, not binary (ADR-0012: "Cartridge capacities are decimal; data
    // sizes are binary; the two are named apart"). This is a capacity, so it
    // means what the box says. It parsed BINARY until issue #200 — which made
    // it the last site disagreeing with `Config::validate_sizes` and
    // `LtoBackendConfig::planning_capacity_bytes`, both decimal since #168.
    // The consequence was not a rounding difference: a config that LOADED
    // cleanly, validated as 2 500 000 000 000, was re-read here as
    // 2 748 779 069 440 and stored on the volume row that ADR-0010 decision 3
    // makes authoritative for every later capacity gate — a value reinterpreted
    // after its own validation passed, over-crediting the tape by ~10%.
    let capacity_override = match &backend.capacity_override {
        Some(v) => Some(crate::media::parse_capacity_to_bytes(v)? as u64),
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
        &det.mam,
    )?;
    events::log_created(&tx, "volume", volume_id, label, None)?;
    tx.commit()?;

    // AFTER the commit, and ONLY when the CHIP named the cartridge.
    // `cartridge_contacts` has no `identity_source` column, so a
    // `--cartridge <barcode>` bind written here would make an operator's
    // typed assertion read as an observation off the medium — exactly what
    // `REASON_SERIAL_UNREGISTERED` forbids, and the same `serial.is_some()`
    // discriminator the File 0 `[media]` block just below uses to choose
    // between `"mam"` and `"operator"`.
    //
    // The contact was opened before this cartridge row existed, so on the
    // auto-registration path it still carries `REASON_SERIAL_UNREGISTERED`,
    // which was true then and is a lie now; `record_cartridge` replaces both
    // in one statement.
    if serial.is_some() {
        if let Some(cartridge_id) = bound.cartridge_id {
            contact.record_cartridge(cartridge_id);
        }
    }

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
        // Unreachable from `volume_init` since ADR-0012 (issue #192):
        // `require_named_cartridge` refuses that case before the transaction
        // opens, so there is no longer an unbound-with-a-warning outcome to
        // report. Kept rather than made an `unreachable!()` — `report_binding`
        // describes a `BindOutcome`, and `bind_late` can still legitimately
        // produce an unbound one on a legacy volume. A warning is the right
        // thing to print if that ever reaches here; a panic is not.
        None => {
            eprintln!(
                "warning: no medium serial readable; volume \"{label}\" is not bound to a \
                 cartridge. Copy counting cannot tell this cartridge from another, and \
                 `volume write` cannot check you reloaded the same one."
            );
        }
    }

    // Issue #235: the lines themselves come from `binding::render_displacement`,
    // the ONE renderer both displacement callers share. This function used to
    // own them, and `catalog rebuild` — the other caller of
    // `mount_and_record` — rendered the same `Displaced` independently and
    // dropped the zero-copy half of it. Rendering here and printing there is
    // what made that divergence possible; there is now nothing to diverge.
    let now = chrono::Utc::now().naive_utc();
    let barcode = bound.barcode.as_deref().unwrap_or("?");
    for d in &bound.displaced {
        for line in binding::render_displacement(barcode, d, now) {
            eprintln!("{line}");
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
/// - The volume already has an open mount naming the SAME cartridge this
///   contact resolves to — the ordinary, overwhelmingly common path
///   (`volume init` already bound it), and rebinding would be a displacement
///   nobody asked for.
/// - No serial readable — exactly as at init, the volume stays unbound.
/// - No generation resolvable — auto-registering a cartridge needs one, and
///   inventing it is how a row starts lying.
///
/// An open mount naming a DIFFERENT, already-registered cartridge is **not**
/// a no-op (ADR-0012, issue #162): resolving BEFORE deciding, rather than
/// deciding on `already_bound` alone, is what catches a write to the wrong
/// cartridge instead of reporting it as bound to the right one. See
/// [`binding::refuse_rebind`], which this calls — the same refusal every
/// other writer of `cartridge_volumes` gets.
///
/// There is no `--cartridge` on `volume write`, so this is the serial-only
/// half of the ladder: match a registered row by serial, else auto-register
/// one whose barcode IS the serial. ADR-0011's `refuse_retired` applies
/// here for the same reason it applies at init.
///
/// Returns **the cartridge the CHIP'S OWN SERIAL identified**, or `None`
/// (issue #296). `None` is not "nothing was bound": an already-bound volume
/// whose medium serial matches no registered row stays bound and still
/// returns `None`, because nothing this chip said established that binding.
/// The caller records it on the contact, and `cartridge_contacts` has no
/// `identity_source` column — so a value returned here that the medium did
/// not name would make an operator's typed barcode read as an observation
/// off the tape (ADR-0012: a cartridge's identity is its chip serial).
fn bind_late(
    conn: &Connection,
    volume_id: i64,
    label: &str,
    det: &crate::tape::media_detect::Detected,
    volume_media_type: Option<&str>,
    drive_generation: &str,
) -> Result<Option<i64>> {
    let Some(serial) = det.mam.serial.as_deref() else {
        return Ok(None);
    };
    // The volume's own recorded generation first (ADR-0010 decided it at
    // init from the medium that was loaded), then what the drive detects
    // now, then the drive's native generation.
    let Some(generation) = volume_media_type
        .and_then(crate::media::Generation::parse)
        .or(det.generation)
        .or_else(|| crate::media::Generation::parse(drive_generation))
    else {
        return Ok(None);
    };

    // Resolve the cartridge this contact's medium identifies BEFORE
    // deciding anything from `already_bound` (issue #162): the old order
    // read `already_bound` first and returned on `is_some()` alone, so a
    // write to the WRONG cartridge — one already registered under a
    // different row — was silently reported as bound to the right one.
    let lookup = binding::lookup_cartridge(conn, Some(serial), None)?;

    let already_bound: Option<i64> = conn
        .query_row(
            "SELECT cartridge_id FROM cartridge_volumes
             WHERE volume_id = ?1 AND unmounted_at IS NULL",
            params![volume_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(bound_id) = already_bound {
        return match &lookup.row {
            // Same cartridge: today's no-op, unchanged.
            Some(row) if row.id == bound_id => Ok(Some(row.id)),
            // A DIFFERENT, KNOWN cartridge: the Change-2 refusal, so the
            // message is identical to every other writer of
            // `cartridge_volumes`. Never duplicated here.
            Some(row) => binding::refuse_rebind(conn, volume_id, row.id).map(|_| None),
            // The medium's serial matches no registered row at all. Absence
            // is not contradiction (ADR-0012) — tapectl has no SPECIFIC
            // other cartridge to name, so this stays the no-op it always
            // was rather than a guess. The volume IS bound, but not by
            // anything this chip said, so the contact learns nothing.
            None => Ok(None),
        };
    }

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
    Ok(bound.cartridge_id)
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
    paths: &TapectlPaths,
    config: &Config,
    label: &str,
    device: &str,
    block_size: usize,
    force: bool,
    allow_missing_escrow: bool,
) -> Result<()> {
    // ONE contact for the whole write, and the one `volume compact-write`,
    // `collection run` and `quick-archive` inherit — all three reach the
    // drive only through this function, so a guard of their own would
    // record one physical contact twice. See [`ContactSlot`] for why it
    // cannot simply be opened here.
    let mut contact = ContactSlot::empty();
    let r = volume_write_contacted(
        conn,
        paths,
        config,
        label,
        device,
        block_size,
        force,
        allow_missing_escrow,
        &mut contact,
    );
    contact.finish_result(r)
}

#[allow(clippy::too_many_arguments)]
fn volume_write_contacted<'c>(
    conn: &'c Connection,
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
    contact: &mut ContactSlot<'c>,
) -> Result<()> {
    let (volume_id, volume_status, observed_condition): (i64, String, String) = conn
        .query_row(
            "SELECT id, status, observed_condition FROM volumes WHERE label = ?1",
            params![label],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|_| TapectlError::VolumeNotFound(label.to_string()))?;

    // ADR-0012 (issue #161): the catalog's own statement that this volume is
    // finished, gone, or untrusted is a fact, not a risk judgement -- refuse
    // it before ANY other check, including the unresolved-session check
    // just below. A sealed volume's writes are all `completed`, so that
    // check would never fire on it and this status refusal is the truer
    // message; refusing here also means nothing after this point --
    // `find_staged_data`, the MAM `UPDATE`, `check_loaded_generation`,
    // `TapeStore::open`, `check_fresh_write_contact`, `bind_late` -- ever
    // runs for a non-target row, which is what keeps a closed binding
    // permanent (#154).
    //
    // ADR-0012's 2026-09-17 amendment (issue #242): `is_write_target` now
    // also consults `observed_condition`, so a genuine refusal can be
    // either half. Re-checked separately here (a cheap string compare, not
    // a second query) purely so the two halves get DISTINGUISHABLE
    // messages -- `VolumeNotWriteTarget`'s wording asserts the status is
    // the problem, which would be actively misleading for a volume that
    // reads `initialized` (the one legal write-target status) and was
    // refused on its condition instead.
    if !coverage::is_write_target(&volume_status, &observed_condition) {
        if volume_status != "initialized" {
            return Err(TapectlError::VolumeNotWriteTarget {
                label: label.to_string(),
                status: volume_status,
            });
        }
        return Err(TapectlError::VolumeQuarantined {
            label: label.to_string(),
        });
    }

    // ADR-0012 amendment, issue #199: status alone is only a proxy for
    // "does this volume hold bytes we know about?" -- `catalog rebuild
    // --from-volume` can attach a rebuilt row's contents to a stale
    // `initialized` row without ever moving its status (#158). Same
    // ordering rule as the status check just above: this is a read-only
    // query, still ahead of `find_staged_data`, the MAM `UPDATE`, and
    // `TapeStore::open`.
    if coverage::has_completed_write(conn, volume_id)? {
        return Err(TapectlError::VolumeHasRecordedWrite {
            label: label.to_string(),
        });
    }

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
    // Issue #201: say what is about to happen before anything below touches
    // a backend, a MAM, or the drive. See `announce_staged_selection`'s doc
    // comment for why this is stderr and why it changes no selection logic.
    announce_staged_selection(label, &units);

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
    // THE CONTACT BEGINS HERE, at the same one read of the medium the three
    // refusals below consult — the `st` driver refuses a second concurrent
    // open, so there is no second reading to be had and none is taken.
    let contact = contact.fill(ContactGuard::open(
        conn,
        config,
        Operation::VolumeWrite,
        device,
        Some(volume_id),
        Medium::Observed {
            backend,
            mam: &det.mam,
        },
    ));

    // Corroborate at contact (ADR-0012, issue #193) — wrong-cartridge
    // discipline one layer earlier than the File 0 check (ADR-0010): the
    // volume knows which medium serial it was initialised on, so a swapped
    // cartridge is caught before `build()` materialises a single slice —
    // never mind before anything is written.
    //
    // MAM ONLY, deliberately: File 0's discipline on the write path is the
    // ADR-0003 CONSENT gate below (`check_fresh_write_contact`, which
    // `--force` may override and which must also handle a BLANK tape).
    // Feeding File 0 to the fact refusal would pre-empt it.
    binding::corroborate_volume(
        conn,
        volume_id,
        label,
        &binding::MediumFacts::from_serial(mam.serial.clone()),
    )?;
    check_loaded_generation(label, &det, volume_media_type.as_deref())?;

    // ADR-0010 decision 2, issue #166: the drive/medium refusal is checked
    // at every write contact, not only at `volume_init` — a volume
    // initialised on one drive can be written (or resumed) from a different
    // one. Same cannot-see-cannot-refuse rule as `check_loaded_generation`
    // just above: detection wins, and only when NOTHING is detected does the
    // volume's own recorded generation stand in. Before `bind_late` (issue
    // #154: a refused write must displace nothing) and before the session
    // directory / `build()`.
    let medium_for_write_check = det.generation.or_else(|| {
        volume_media_type
            .as_deref()
            .and_then(crate::media::Generation::parse)
    });
    if let Some(m) = medium_for_write_check {
        crate::tape::media_detect::check_drive_can_write(backend, m)?;
    }

    // MAM bookkeeping, moved BELOW the three refusals above (issue #220).
    //
    // It used to sit immediately after `detect`, which meant a write refused
    // by corroboration, by `check_loaded_generation` or by
    // `check_drive_can_write` had already written that cartridge's MAM facts
    // onto this volume's row. On the corroboration path those are facts about
    // a DIFFERENT physical cartridge -- the refusal's whole point -- so the
    // row was left claiming capacity and remaining-space figures read from a
    // tape it is not on.
    //
    // #161 put the catalog guard ahead of everything for exactly this reason:
    // a refused write must touch nothing. These columns are informational and
    // never gate the write (the pre-flight reads `volumes.capacity_bytes`,
    // decided at init), which is why this is low severity -- but informational
    // and wrong is still wrong, and `report capacity` reads them.
    if mam.max_capacity_bytes.is_some() || mam.remaining_bytes.is_some() {
        let _ = conn.execute(
            "UPDATE volumes SET mam_capacity_bytes = ?1, mam_remaining_at_start = ?2
             WHERE id = ?3",
            params![mam.max_capacity_bytes, mam.remaining_bytes, volume_id],
        );
    }

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
    // The cartridge id comes back so the contact can name it (issue #296):
    // a legacy unbound volume auto-registers its cartridge HERE, long after
    // the contact opened carrying `REASON_SERIAL_UNREGISTERED`, which was
    // true then and is a lie now. `bind_late` returns `None` unless the
    // CHIP's own serial established the identity, so an operator-established
    // binding never reads as an observation off the medium.
    if let Some(cartridge_id) = bind_late(
        conn,
        volume_id,
        label,
        &det,
        volume_media_type.as_deref(),
        &backend.generation,
    )? {
        contact.record_cartridge(cartridge_id);
    }

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

    collect_health_best_effort(
        conn,
        config,
        device,
        volume_id,
        contact.id(),
        health::Reading::Write,
    );

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
    paths: &TapectlPaths,
    config: &Config,
    label: &str,
    device: &str,
    block_size: usize,
) -> Result<()> {
    // See [`ContactSlot`]: resume refuses on the volume's status, on its
    // `writes` rows and on a missing backend long before it reads the MAM,
    // and none of those refusals is a contact.
    let mut contact = ContactSlot::empty();
    let r = volume_resume_contacted(conn, paths, config, label, device, block_size, &mut contact);
    contact.finish_result(r)
}

fn volume_resume_contacted<'c>(
    conn: &'c Connection,
    // See `volume_write`'s `_paths` for why this is unused but kept.
    _paths: &TapectlPaths,
    config: &Config,
    label: &str,
    device: &str,
    block_size: usize,
    contact: &mut ContactSlot<'c>,
) -> Result<()> {
    let (volume_id, volume_status, observed_condition): (i64, String, String) = conn
        .query_row(
            "SELECT id, status, observed_condition FROM volumes WHERE label = ?1",
            params![label],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|_| TapectlError::VolumeNotFound(label.to_string()))?;

    // ADR-0012 (issue #161): same fact refusal as `volume_write`, and for
    // the same reason it must come before anything else -- a `quarantined`
    // volume must be named by its status, not answered with
    // `nothing_to_resume`'s message about `writes` rows.
    //
    // ADR-0012's 2026-09-17 amendment (issue #242): same distinguishable
    // two-part refusal as `volume_write` -- see its comment for why.
    if !coverage::is_write_target(&volume_status, &observed_condition) {
        if volume_status != "initialized" {
            return Err(TapectlError::VolumeNotWriteTarget {
                label: label.to_string(),
                status: volume_status,
            });
        }
        return Err(TapectlError::VolumeQuarantined {
            label: label.to_string(),
        });
    }

    // ADR-0012 amendment, issue #199: same additional fact refusal as
    // `volume_write` -- an `initialized` row that a rebuild attached
    // completed contents to must be named by that fact, not fall through
    // to `nothing_to_resume`'s message about unresolved `writes` rows
    // (there may be none at all in the rebuild case). Deliberately does
    // NOT disqualify `planned`/`in_progress`/`interrupted` rows -- see
    // `has_completed_write`'s doc comment; those are exactly what
    // `rehydrate`, called next, exists to continue.
    if coverage::has_completed_write(conn, volume_id)? {
        return Err(TapectlError::VolumeHasRecordedWrite {
            label: label.to_string(),
        });
    }

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
    // `WHERE status = 'staged'` query (issue #115). The Layout IS the frozen
    // record of the one `find_staged_data` selection this session was planned
    // from, and a second selection would return a DIFFERENT set: `'staged'` is
    // a standing state, not a transient one, so by the time a resume runs it
    // also matches every stage set created since this session was planned. A
    // second selection is what issue #96 was.
    //
    // This comment used to justify itself with "`plan` already moved those
    // stage sets out of `'staged'`" (issue #245). Nothing does. Every writer of
    // `stage_sets.status` in the tree: `staging::stage_create` -> `'staged'`,
    // `staging::clean` -> `'cleaned'`, `db::open`'s crash sweep -> `'failed'`,
    // and `read_slices` restoring a set to `'staged'`. `plan` inserts `writes`
    // rows and touches the status not at all. A set stays `'staged'` from
    // `stage create` until `staging clean` — which is precisely what makes
    // ADR-0012's per-copy recipe work (`collection run --label A`, swap, then
    // `volume write B` against the same staged bytes), proved on media by the
    // `collection-second-copy` lifecycle scenario (issue #226).
    let stage_set_ids = stage_set_ids_for_layout(conn, &layout_snapshot)?;
    let SessionKeys { keys, .. } = assemble_session_keys(conn, &tenant_ids, &stage_set_ids)?;

    let backend = crate::config::resolve_lto_backend(config, Some(device))?;
    // ADR-0010 decision 3: the volume's own row, never config. Resume must
    // in any case reuse the figure the interrupted session planned against —
    // a capacity that moved mid-session would be a different plan.
    let (nominal_capacity, volume_media_type) = volume_media(conn, volume_id, label)?;
    let usable_bytes = (nominal_capacity as f64 * backend.usable_capacity_factor) as u64;

    // Corroborate at contact (ADR-0012, issue #193). Resume is a contact
    // under CONTEXT.md's definition and corroborated nothing until now: an
    // interrupted session could be continued onto a DIFFERENT cartridge.
    //
    // Before `TapeStore::open`, because `detect` opens the device read-only
    // and drops the fd and the st driver refuses a second concurrent open —
    // the same ordering `volume_write` states at its own MAM read.
    //
    // MAM ONLY, deliberately, and for a sharper reason than on the write
    // path: `session.resume` runs `check_tape_contact`, which maps a File 0
    // identity mismatch onto DIVERGENCE → quarantine (`layout-session.md`).
    // A fact refusal on File 0 here would pre-empt the quarantine that is
    // how a resume is supposed to record a divergent tape.
    let det = crate::tape::media_detect::detect(device, &backend.device_sg);
    // THE CONTACT BEGINS HERE — resume's `det` is this contact's own reading
    // of the tape, exactly like `volume_write`'s, and the contact records
    // the same one.
    let contact = contact.fill(ContactGuard::open(
        conn,
        config,
        Operation::VolumeResume,
        device,
        Some(volume_id),
        Medium::Observed {
            backend,
            mam: &det.mam,
        },
    ));
    binding::corroborate_volume(
        conn,
        volume_id,
        label,
        &binding::MediumFacts::from_serial(det.mam.serial.clone()),
    )?;

    // ADR-0010 decision 2, issue #166: resume never checked the drive
    // against the medium at all — this is the gap that let an interrupted
    // session be continued on a drive that cannot write the loaded medium,
    // failing loudly on the first physical write instead of refusing here,
    // free, before the store is opened. Must call `detect` itself (just
    // above) rather than reuse one from elsewhere — resume's `det` is
    // this contact's own reading of the tape, exactly like `volume_write`'s.
    let medium_for_write_check = det.generation.or_else(|| {
        volume_media_type
            .as_deref()
            .and_then(crate::media::Generation::parse)
    });
    if let Some(m) = medium_for_write_check {
        crate::tape::media_detect::check_drive_can_write(backend, m)?;
    }

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

    // `'resume'` — the honest word, and the record says it from migration 021
    // (issue #296) onward. It could not before: `001_initial.sql`'s
    // `CHECK(operation IN ('write','read','verify','clean'))` was in force,
    // SQLite enforces a CHECK unconditionally, and
    // `collect_health_best_effort` only warns on an insert failure — so
    // writing `'resume'` would have silently DROPPED the health row on every
    // resume, strictly worse than the mislabel. #295 threaded the word
    // through as a parameter so this became one literal; 021 dropped the
    // CHECK, and this is that literal flipped. "Free TEXT so a new value
    // costs no migration" was true only AFTER the migration that made it
    // free (the #295 hazard note).
    collect_health_best_effort(
        conn,
        config,
        device,
        volume_id,
        contact.id(),
        health::Reading::Resume,
    );

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
        "The staged slices stay pinned on disk; because this session's `writes` row becomes \
         ABORTED, plain `tapectl staging clean` will not release them — use `tapectl staging \
         clean --force`, or write them to another volume first."
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
/// Interrupted/Aborted/Quarantined/Confirming terminations. Factored rather
/// than duplicated because this is where a copy-paste would silently drift —
/// in particular the `Quarantined` arm, which only the resume path can reach
/// from `resume` itself (the File-0 identity check / already-sealed refusal)
/// and which a half-copied tail would drop.
///
/// It takes a [`ResumeOutcome`] because that enum is the superset:
/// `volume_write` converts its `ExecuteOutcome` via the existing
/// `From<ExecuteOutcome>` impl, leaving `Quarantined`/`Confirming`
/// unreachable-but-handled on the fresh path. This shares the Interrupted and
/// Aborted arms too, not just seal/confirm.
///
/// Sacred invariant 1 (`v2-implementation-plan.md`): the seal marker is
/// written only inside `ReadyToSeal::seal`. This function calls it only for
/// `ResumeOutcome::Ready`; it never constructs a seal entry of its own, and
/// the `Confirming` arm below deliberately does NOT call it — the tape is
/// already sealed (ADR-0012's 2026-09-21 amendment, issues #260/#267).
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
            // ADR-0012's 2026-09-21 correction "the seal is RECORDED, not
            // inferred" (issue #277, migration 018): the moment `seal()`
            // returns `Ok`, that fact must become durable state, because
            // `seal_marker_parses_at`'s read-error/no-marker conflation
            // (correct and deliberate for a fresh write to a blank tape)
            // cannot be un-inferred later on resume -- an unreadable seal
            // position is exactly what an `Inconclusive` confirm leaves
            // behind (`session.rs`'s `SealedPending::confirm`). Write-once
            // (`COALESCE`, the same idiom `execute_checking` uses for
            // `started_at`) and never cleared by any confirm outcome
            // afterward, including `Inconclusive`'s own
            // `mark_writes(..., "interrupted")` -- that survival is the
            // entire point. This is the single production call site for
            // `ReadyToSeal::seal` (`ReadyToSeal::seal` itself takes no
            // `Connection`), serving both a fresh write and a resumed one,
            // so recording it here covers both.
            conn.execute(
                "UPDATE volumes SET sealed_at = COALESCE(sealed_at, datetime('now')) \
                 WHERE id = ?1",
                params![volume_id],
            )?;

            // Issue #276: sibling to `session`'s `TAPECTL_TEST_PAUSE_AFTER_PLAN`
            // hook, for a state that one cannot reach -- it parks BEFORE any
            // entry is written, so `seal()` never runs. This one parks AFTER
            // `sealed_at` above and BEFORE `confirm`, which is the only way to
            // hold a real cartridge in exactly the "sealed but unconfirmed"
            // state migration 018 exists to name, long enough for an operator
            // or test harness to interrupt it. See `park_after_seal`'s own doc
            // for why this is a PAUSE, never a forced outcome.
            if let Some(marker) = pause_after_seal_marker_from_env() {
                if park_after_seal(&marker) {
                    sealed_pending.mark_interrupted(conn)?;
                    events::log_event(
                        conn,
                        "volume",
                        volume_id,
                        Some(label),
                        "write_confirm_interrupted",
                        None,
                        None,
                        None,
                        None,
                        None,
                    )?;
                    return Err(TapectlError::Other(format!(
                        "volume \"{label}\": interrupted after seal, before confirm -- the \
                         tape IS sealed but its readback never ran, so the catalog cannot \
                         yet count it as a copy. Reload the same cartridge and run `tapectl \
                         volume resume {label}` to re-enter confirm."
                    )));
                }
            }

            finish_confirm(
                conn,
                store,
                volume_id,
                label,
                layout,
                block_size,
                sealed_pending,
            )
        }
        // ADR-0012's 2026-09-21 amendment (issues #260/#267): resume found
        // the tape already sealed exactly where THIS session left it, all
        // three of `resume_checking`'s (via `resume_reconfirm_eligible`)
        // conjunctive conditions verified. `seal()` must NEVER be called
        // here (sacred invariant 1) — the seal marker is already on the
        // tape; re-enter confirm directly on the `SealedPending` resume
        // already produced.
        ResumeOutcome::Confirming(sealed_pending) => finish_confirm(
            conn,
            store,
            volume_id,
            label,
            layout,
            block_size,
            sealed_pending,
        ),
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

/// The ONE place `TAPECTL_TEST_PAUSE_AFTER_SEAL`'s environment variable is
/// read (issue #276), mirroring `session::park_marker_from_env`'s own
/// reasoning: environment variables are process-global, so an in-process
/// test that set one would leak into every other test running in parallel
/// in the same binary. No test ever touches the environment — this is the
/// single boundary function that would need to.
fn pause_after_seal_marker_from_env() -> Option<String> {
    std::env::var("TAPECTL_TEST_PAUSE_AFTER_SEAL").ok()
}

/// Parks in [`finish_session`], immediately after `sealed_at` is recorded
/// and before `confirm` is called, when `marker` names a readiness-marker
/// path (issue #276). Sibling to `session::run_entries`'s
/// `TAPECTL_TEST_PAUSE_AFTER_PLAN` hook, whose design this follows
/// exactly: writes the marker file so a harness knows parking has begun,
/// then polls [`crate::signal::is_interrupted`] -- the same process-global
/// flag `PlannedSession::execute`/`InterruptedSession::resume` default to
/// -- on a 120-second ceiling, after which it warns and gives up so a
/// harness bug fails on its own assertion rather than hanging.
///
/// **It is a PAUSE, not a failure injection.** This function only decides
/// how long to wait; it never touches `writes`/`volumes` and never
/// constructs a `ConfirmOutcome` -- the caller decides what an interruption
/// means (`finish_session` marks the session interrupted and returns
/// without calling confirm; letting the ceiling expire instead falls
/// through to a perfectly normal confirm). A SIGKILL during the pause ends
/// the process outright, same as anywhere else in a write session; a
/// SIGINT sets the same flag this function polls, so it is noticed here
/// exactly as fast as it would be between two `run_entries` entries.
///
/// Returns `true` iff the pause ended because of an interruption (so the
/// caller should treat this as "interrupted before confirm"), `false` if
/// it ended because the marker could not be created or the ceiling
/// expired (so the caller should proceed as if this hook were absent).
fn park_after_seal(marker: &str) -> bool {
    tracing::warn!(
        marker = %marker,
        "TAPECTL_TEST_PAUSE_AFTER_SEAL is set — parking after seal() with confirm not yet \
         called. This is a TEST hook; it must never be set for a real write."
    );
    if let Err(e) = std::fs::write(marker, "parked\n") {
        tracing::warn!(
            marker = %marker,
            error = %e,
            "TAPECTL_TEST_PAUSE_AFTER_SEAL: cannot create readiness marker; proceeding \
             without parking"
        );
        return false;
    }

    let parked_at = std::time::Instant::now();
    let limit = std::time::Duration::from_secs(120);
    loop {
        if crate::signal::is_interrupted() {
            return true;
        }
        if parked_at.elapsed() >= limit {
            tracing::warn!(
                "TAPECTL_TEST_PAUSE_AFTER_SEAL: no interrupt within 120s — proceeding with \
                 a normal confirm so the caller fails on its own assertion rather than \
                 hanging"
            );
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// The seal->confirm tail shared by a fresh [`ReadyToSeal`] (which calls
/// `seal()` first, in [`finish_session`]) and a resume that met
/// [`ResumeOutcome::Confirming`] (which never does — the tape is already
/// sealed). Factored out so the [`ConfirmOutcome`] three-way match exists in
/// exactly one place — ADR-0012's 2026-09-18 amendment, issues #260/#267.
fn finish_confirm(
    conn: &Connection,
    store: &mut dyn Store,
    volume_id: i64,
    label: &str,
    layout: &Layout,
    block_size: u64,
    sealed_pending: session::SealedPending,
) -> Result<()> {
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
        // ADR-0012's 2026-09-18 amendment (issues #260/#267): nothing here
        // proves the medium bad — confirm's readback simply did not
        // succeed. The tape is physically unharmed and unchanged;
        // `SealedPending::confirm` has already left the `writes` rows
        // `interrupted` (never `aborted`), so `tapectl volume resume` can
        // pick this session back up and re-enter confirm.
        ConfirmOutcome::Inconclusive(inc) => {
            let detail = format!(
                "{} mismatch(es) during readback, none proving the medium itself is bad \
                 (drive/transport evidence only)",
                inc.evidence.mismatches.len()
            );
            events::log_event(
                conn,
                "volume",
                volume_id,
                Some(label),
                "write_confirm_inconclusive",
                None,
                None,
                Some(&detail),
                None,
                None,
            )?;
            Err(TapectlError::Other(format!(
                "volume \"{label}\": confirm could not complete — {detail}. The tape is \
                 physically unharmed and the write is not lost; run `tapectl volume resume \
                 {label}` to retry the confirm readback."
            )))
        }
    }
}

/// The one place a WRITE-path quarantine is recorded and reported — reached
/// from `confirm`'s `Quarantined` outcome (either path: a fresh seal or a
/// resume's re-confirm) and from `resume`'s own divergence findings (the
/// resume-only arm). Confirm's OTHER non-Sealed outcome, `Inconclusive`
/// (ADR-0012's 2026-09-18 amendment, issues #260/#267), is deliberately NOT
/// routed here — it never touches `observed_condition`, so there is no
/// quarantine fact to report; see `finish_confirm`'s own arm. ADR-0001
/// contact-time divergence;
/// `session.rs` has already written `volumes.observed_condition =
/// 'quarantined'` (ADR-0012's 2026-09-17 amendment, issue #242 — `status` is
/// left untouched) by the time this runs, so this only reports it and turns
/// the outcome into the `Err` the command exits on.
///
/// **Deliberately not factored together with
/// [`quarantine_on_medium_evidence`]'s event**, issue #234's peer under
/// ADR-0012. The two rows are different SHAPES, not just different action
/// names: this one carries its reason in `new_value` with no `field` (the
/// status write is not its own), and the verify one is a genuine field
/// change (`field = 'status'`, old -> new) with its reason in `details`.
/// Routing both through one helper silently moved this reason from
/// `new_value` to `details` — an operator-visible change to `report events`
/// on a path issue #234 was explicitly not supposed to touch. The two acts
/// answer to different ADRs and record different facts; sharing a writer
/// bought nothing and cost a regression.
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

/// What a failed verify did to `volumes.observed_condition`, when it did
/// anything (ADR-0012's 2026-09-17 amendment "the status column is the
/// operator's; a medium's condition is its own fact", issue #242; the
/// verify path's OWN quarantine ruling is the earlier 2026-09-17 amendment,
/// issue #234). Before issue #242, this recorded `volumes.status`; a verify
/// never writes `status` at all now.
///
/// Carries the condition it replaced, not just "quarantined happened": a
/// volume whose medium was ALREADY known bad is a different fact from one
/// this verify took out of service, and a report that said `quarantined:
/// true` for both would be lying about one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineEffect {
    /// `volumes.observed_condition` immediately before this verify wrote
    /// `quarantined` — always `"ok"` or `"quarantined"` (the column's own
    /// closed set, migration 017).
    pub previous_condition: String,
    /// The mismatches that proved the medium is bad — a subset of
    /// [`VerifyReport::mismatches`], so an operator reads the reason next to
    /// the consequence rather than having to re-derive it.
    pub proof: Vec<crate::store::Mismatch>,
}

impl QuarantineEffect {
    /// Whether this verify actually changed the volume's condition. `false`
    /// when it was already `quarantined` — the evidence is fresh, the
    /// condition is not new.
    pub fn condition_changed(&self) -> bool {
        self.previous_condition != "quarantined"
    }
}

/// One sentence naming the failures that prove the medium is bad, for the
/// `events` row and the operator-facing message.
fn describe_medium_evidence(proof: &[&crate::store::Mismatch]) -> String {
    let named = proof
        .iter()
        .map(|m| format!("position {} {}", m.position, m.kind.label()))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "verify found {} failure(s) proving the medium is bad: {named}",
        proof.len()
    )
}

/// **ADR-0012, amendment of 2026-09-17 (issue #234): quarantine a volume a
/// verify PROVED unreadable — and only then.**
///
/// The ADR names a failed verify twice as the escape from its own Tier-3
/// refusal (`cli::operations::refuse_last_eligible_copy`, which takes no
/// flag by construction), and from the rule that an operator's `status` is
/// never overwritten by anything tapectl merely observes. No verify path
/// wrote a status at all before issue #234, so the escape did not exist: an
/// operator holding the only copy of a tape they had proved unreadable met
/// a refusal that named no flag and had no command that changed anything.
///
/// The predicate is [`crate::store::Evidence::medium_evidence`], never
/// "the verify failed": a dirty drive, a wrong block size, a transient SCSI
/// error or a tape not loaded must leave the volume exactly as it was,
/// because `quarantined` is precisely what makes a volume stop counting as
/// a copy and a false one silently takes real coverage to zero.
///
/// **Never returns `Err` for the quarantine itself.** A propagated error
/// from `volume_verify` means verify did not complete, which is not
/// evidence about the medium; and `volume verify` reports failure through
/// [`VerifyReport`], never through `Err` (the mhvtl corruption-parity test
/// pins that). Only a database failure can error here.
///
/// **The column, since ADR-0012's later 2026-09-17 amendment ("the status
/// column is the operator's; a medium's condition is its own fact", issue
/// #242): `observed_condition`, never `status`.** Verifying a `retired`
/// volume is a reasonable thing to do (before disposal, before trusting a
/// warehouse deposit, or simply to learn whether a condemned tape is still
/// readable), and under ADR-0011 `retired` means unfit-to-write, not
/// unreadable — so a verify that overwrote it would destroy the operator's
/// own fact for one tapectl merely observed. The `UPDATE` is unconditional
/// on the volume's current CONDITION (matching `session.rs`'s three
/// quarantine writers, which target the same column for the same reason),
/// but `status` itself is never touched here. The condition it replaced is
/// recorded in the `events` row and returned, so nothing is lost: the
/// catalog carries "the operator tried to read this and it failed", which
/// is the fact the amendment says must exist.
/// What a clean full verify did to `volumes.observed_condition`
/// (ADR-0012's 2026-09-18 amendment, issue #268).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionCleared {
    /// The condition this verify replaced — always `"quarantined"`, since a
    /// volume already `"ok"` is left alone and reported as no change.
    pub previous_condition: String,
}

/// **Return a volume to service when a FULL verify finds nothing wrong**
/// (ADR-0012's 2026-09-18 amendment, issue #268) — the inverse of
/// [`quarantine_on_medium_evidence`], and until that amendment it did not
/// exist. Every one of the twelve `SET observed_condition` sites wrote
/// `'quarantined'`; none wrote `'ok'`, and `catalog rebuild` only records a
/// mismatch rather than clearing it. So a quarantine was permanent whatever
/// produced it, which is exactly what made a FALSE one worth arguing about.
///
/// The justification is definitional rather than a policy preference: the
/// column holds what tapectl *observed* about the medium, so a later and
/// better observation is the thing entitled to update it. That is also why
/// this is not an operator override — a `clear-condition` command was
/// considered and rejected in the same ruling, because the condition is
/// evidence and evidence is replaced by gathering more of it.
///
/// **Only [`Tier::Integrity`] clears it.** A navigable verify checks the
/// map, not the bytes, so it cannot license the claim that the medium is
/// sound; a lesser tier leaves the condition exactly as it found it. That
/// asymmetry is the whole reason this takes `tier` rather than inferring
/// "clean" from an empty mismatch list, which a quick verify also produces.
///
/// `status` is never touched here, for the same reason the quarantine half
/// does not touch it: it is the operator's column.
pub(crate) fn clear_condition_on_clean_full_verify(
    conn: &Connection,
    volume_id: i64,
    label: &str,
    tier: Tier,
    evidence: &crate::store::Evidence,
) -> Result<Option<ConditionCleared>> {
    if tier != Tier::Integrity || !evidence.mismatches.is_empty() {
        return Ok(None);
    }
    let previous_condition: String = conn.query_row(
        "SELECT observed_condition FROM volumes WHERE id = ?1",
        params![volume_id],
        |r| r.get(0),
    )?;
    if previous_condition == "ok" {
        // Nothing to report: a clean verify of a healthy volume is the
        // ordinary case and must not write an events row on every run.
        return Ok(None);
    }
    conn.execute(
        "UPDATE volumes SET observed_condition = 'ok' WHERE id = ?1",
        params![volume_id],
    )?;
    events::log_event(
        conn,
        "volume",
        volume_id,
        Some(label),
        "verify_cleared",
        Some("observed_condition"),
        Some(previous_condition.as_str()),
        Some("ok"),
        Some("a full verify read every file back and found no mismatch"),
        None,
    )?;
    warn!(
        label = label,
        previous_condition = %previous_condition,
        "volume returned to service by a clean full verify"
    );
    Ok(Some(ConditionCleared { previous_condition }))
}

pub(crate) fn quarantine_on_medium_evidence(
    conn: &Connection,
    volume_id: i64,
    label: &str,
    evidence: &crate::store::Evidence,
) -> Result<Option<QuarantineEffect>> {
    let proof = evidence.medium_evidence();
    if proof.is_empty() {
        return Ok(None);
    }
    let previous_condition: String = conn.query_row(
        "SELECT observed_condition FROM volumes WHERE id = ?1",
        params![volume_id],
        |r| r.get(0),
    )?;
    conn.execute(
        "UPDATE volumes SET observed_condition = 'quarantined' WHERE id = ?1",
        params![volume_id],
    )?;
    let reason = describe_medium_evidence(&proof);
    // A genuine field change, recorded as one: this function owns BOTH the
    // condition write and the row that explains it, so `old_value` ->
    // `new_value` is the transition it just made and `details` carries why.
    // That is a different row shape from [`log_quarantine`]'s, on purpose —
    // see its doc.
    events::log_event(
        conn,
        "volume",
        volume_id,
        Some(label),
        "verify_quarantined",
        Some("observed_condition"),
        Some(previous_condition.as_str()),
        Some("quarantined"),
        Some(&reason),
        None,
    )?;
    warn!(
        label = label,
        previous_condition = %previous_condition,
        reason = %reason,
        "volume quarantined by a failed verify"
    );
    Ok(Some(QuarantineEffect {
        previous_condition,
        proof: proof.into_iter().cloned().collect(),
    }))
}

/// Best-effort sg_logs health collection. Never lets a collection failure
/// shadow the session's real outcome (matching v1: always attempted, its own
/// errors only logged).
///
/// `operation` is the caller's own word for what it was doing, threaded
/// through rather than hardcoded because two different commands land here.
/// Issue #295 threaded it; migration 021 (issue #296) dropped the CHECK that
/// had been forcing `volume resume` to call itself `write`, and the resume
/// call site now passes [`health::Reading::Resume`].
///
/// `contact_id` is the contact the caller's guard opened (issue #296): the
/// reading names it, and the drive this collection identifies is attached to
/// it — see [`record_health_and_drive`].
fn collect_health_best_effort(
    conn: &Connection,
    config: &Config,
    device: &str,
    volume_id: i64,
    contact_id: Option<i64>,
    operation: health::Reading,
) {
    if let Some(bk) = config.backends.lto.iter().find(|b| b.device_tape == device) {
        collect_and_record_health(conn, bk, Some(volume_id), contact_id, None, operation);
    }
}

/// The hardware half of a health reading: run `sg_logs` and read the drive's
/// identity from the backend the caller ALREADY resolved (no second lookup
/// to disagree with the first, #187), then hand both to
/// [`record_health_and_drive`], which does every database write.
///
/// Each log page is read exactly once here (ADR-0013's read-to-clear
/// hazard): the drive identity comes from sysfs / VPD 0x80 and from the
/// header of the text this ONE collection already returned — never from a
/// second `sg_logs` run.
fn collect_and_record_health(
    conn: &Connection,
    bk: &crate::config::LtoBackendConfig,
    volume_id: Option<i64>,
    contact_id: Option<i64>,
    session_id: Option<i64>,
    reading: health::Reading,
) {
    let collected = match health::collect(&bk.device_sg) {
        Ok(c) => Some(c),
        Err(e) => {
            warn!(sg_device = %bk.device_sg, err = %e, "sg_logs collection failed");
            None
        }
    };
    let identity = drive_identity::read_identity(bk);
    record_health_and_drive(
        conn,
        volume_id,
        contact_id,
        session_id,
        reading,
        collected.as_ref().map(|(c, raw)| (c, raw.as_str())),
        identity,
        &bk.device_tape,
    );
}

/// Every database write a health reading makes, with no hardware in it —
/// split from [`collect_and_record_health`] so the attribution this issue
/// exists for is testable by value (issue #296).
///
/// 1. The `health_logs` row, naming its contact (ADR-0013 §2) and, on the
///    verify path, its `verification_sessions` row (§3). Skipped only when
///    `sg_logs` itself failed: there is no reading to record.
/// 2. The drive (ADR-0013 §1, issue #295) — AFTER the health row,
///    deliberately: identity capture is an addition to the record, never a
///    precondition for it. The `sg_logs` identity header fills any field
///    sysfs did not yield. A drive with no serial records NO row — unknown
///    by absence, never a guess.
/// 3. The contact's `drive_id`, by id rather than through a guard, because
///    on the verify path the guard has already closed
///    ([`contact::record_drive_for`]). Attempted on the sg_logs-failed path
///    too: a drive whose counters could not be read is still the drive that
///    was contacted.
///
/// Returns the `drives.id` attached, if any. Best-effort throughout: every
/// failure is a warning, never an error.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_health_and_drive(
    conn: &Connection,
    volume_id: Option<i64>,
    contact_id: Option<i64>,
    session_id: Option<i64>,
    reading: health::Reading,
    collected: Option<(&health::HealthCounters, &str)>,
    mut identity: drive_identity::DriveIdentity,
    device_for_log: &str,
) -> Option<i64> {
    if let Some((counters, raw)) = collected {
        if let Err(e) = health::record(
            conn, volume_id, contact_id, session_id, reading, counters, raw,
        ) {
            warn!(err = %e, "health_logs insert failed");
        }
        identity.backfill_from_sg_logs_header(raw);
    }
    match drive_identity::upsert(conn, &identity) {
        Ok(Some(drive_id)) => {
            if let Some(cid) = contact_id {
                contact::record_drive_for(conn, cid, drive_id);
            }
            Some(drive_id)
        }
        Ok(None) => {
            warn!(
                device = %device_for_log,
                "drive identity unavailable (no serial); this contact is recorded without a drive"
            );
            None
        }
        Err(e) => {
            warn!(err = %e, "drives upsert failed");
            None
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
/// The v2 chain walk has seven failure kinds and the column has five values,
/// none of them added for it, so this is a lossy projection by construction.
/// It is lossy in the SAFE direction — the true kind is always written to
/// `notes` — and the split follows what the disagreement actually IS: two
/// kinds compare sha256 hashes (`failed_checksum`), the other five are the
/// bytes not being readable or not being what the map said (`failed_read`).
///
/// Issue #239 moved a case across this line on BOTH paths, deliberately: a
/// content read error or short read used to arrive as `ContentHashMismatch`
/// and so recorded `failed_checksum` with `"read failed: …"` sitting in
/// `expected_sha256`. As `ContentUnreadable` it records `failed_read` with
/// both hash columns NULL, which is what actually happened.
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
    //
    // Resolved ONCE (issue #187): a second, raw-string lookup used to find
    // the sg node for `sg_logs` health collection below, and a by-id
    // `--device` — the RECOMMENDED form, per the device-numbering hazard —
    // matched the canonicalising resolver here but missed that one, so
    // health collection was silently skipped with no word said. Both the
    // capacity factor and the sg node now come from this one resolution.
    let (nominal_capacity, _) = volume_media(conn, volume_id, label)?;
    let (_, backend) = crate::config::resolve_device(config, Some(device))?;
    let usable_bytes = match backend {
        Some(b) => (nominal_capacity as f64 * b.usable_capacity_factor) as u64,
        None => 0,
    };

    // Issue #166: refuse before the store is opened if this drive cannot
    // read the loaded medium — the same fact check every other read path
    // now applies. LENIENT the same way the rest of this function is: no
    // backend, or nothing detected, and this proceeds (ADR-0010's read-path
    // leniency / the DR path, ADR-0005).
    crate::tape::media_detect::check_read_contact(config, device)?;

    // Before `TapeStore::open`: reading the MAM opens the device read-only
    // and drops the fd, and the st driver refuses a second concurrent open.
    // LENIENT — an unconfigured backend yields `None`, which is an absence
    // and proceeds (ADR-0010's read-path leniency).
    let observed = binding::loaded_medium(config, device);
    let mut store = TapeStore::open(device, block_size, usable_bytes)?;

    let mut report = volume_verify_with_store(
        conn,
        &mut store,
        label,
        volume_id,
        block_size,
        tier,
        ContactSite::new(
            config,
            Operation::VolumeVerify,
            device,
            Medium::from_read(observed.as_ref().map(|(b, m)| (*b, m))),
        ),
    )?;

    // Best-effort sg_logs health collection. Advisory only, and deliberately
    // OUTSIDE the store-injectable half: it needs the drive's sg node, which
    // a `MemStore` does not have.
    //
    // Issue #187: the SAME resolved `backend` as above, not a second
    // raw-string lookup — and when none resolves, SAY so rather than
    // silently recording nothing.
    match backend {
        // `session_id` (issue #295) and `contact_id` (issue #296) are both
        // carried OUT of `volume_verify_with_store` on the report, because
        // the session is created and the contact closed inside it — before
        // this collection runs. The reading names both; the drive attaches
        // to the (already closed) contact by id.
        Some(bk) => collect_and_record_health(
            conn,
            bk,
            Some(volume_id),
            report.contact_id,
            report.session_id,
            health::Reading::Verify,
        ),
        None => {
            report.drive_health_note = Some(format!(
                "no [[backends.lto]] entry configured for device {device}; drive health \
                 (sg_logs) was not collected. The verification above is unaffected."
            ));
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
    site: ContactSite<'_>,
) -> Result<VerifyReport> {
    let guard = site.open(conn, Some(volume_id));
    let contact_id = guard.id();
    let mut r = verify_contacted(
        conn,
        store,
        label,
        volume_id,
        block_size,
        tier,
        site.medium_serial(),
    );
    // NOT `finish_result` (issue #296). A verify that found mismatches
    // returns `Ok(report)` with `report.failed > 0` — the non-zero exit is
    // `verify_exit_code`'s job, one layer up — and `finish_result` would
    // read that `Ok` as OUTCOME_OK. Migration 020 says this table exists for
    // "the 2031 operator holding a tape with two uncorrected read errors";
    // a contact that found errors and recorded `ok` is precisely the
    // confident wrong answer that migration's own prose objects to.
    match &r {
        Ok(report) if report.failed > 0 => guard.finish(
            contact::OUTCOME_FAILED,
            Some(&format!(
                "{} of {} files mismatched",
                report.failed, report.checked
            )),
        ),
        Ok(_) => guard.finish(contact::OUTCOME_OK, None),
        Err(e) => guard.finish(contact::OUTCOME_FAILED, Some(&e.to_string())),
    }
    // Carried out on the report (issue #296): the guard is gone after this,
    // and `volume_verify`'s health collection still has to name the contact.
    if let Ok(report) = &mut r {
        report.contact_id = contact_id;
    }
    r
}

/// [`volume_verify_with_store`] minus the contact bookkeeping — the verify
/// itself, unchanged.
fn verify_contacted(
    conn: &Connection,
    store: &mut dyn Store,
    label: &str,
    volume_id: i64,
    block_size: usize,
    tier: Tier,
    medium_serial: Option<&str>,
) -> Result<VerifyReport> {
    // CORROBORATE FIRST, before reading anything else (issue #164). A
    // verification session is CONTEXT.md's *Evidence*, and evidence recorded
    // against the wrong volume is worse than no evidence, because it
    // refreshes a staleness clock that gates nothing else. This ran on
    // whatever tape was in the drive and wrote a `passed` row for the volume
    // whose label was typed.
    //
    // The refusal returns HERE — before the front-index read, before
    // `confirm`, and long before the transaction below — so "no
    // `verification_sessions` row" holds structurally rather than by a
    // cleanup step that could be forgotten. Quarantine is for the volume
    // whose claims were contradicted, never the innocent one whose label was
    // typed, so nothing is recorded against either.
    //
    // Unlike the write path this DOES offer File 0: verify has no consent
    // gate to pre-empt, and File 0's label/uuid is the cheapest statement
    // there is of "this is not the tape you named".
    let medium = binding::MediumFacts::new(
        medium_serial.map(str::to_string),
        binding::read_file0_facts(store),
    );
    binding::corroborate_volume(conn, volume_id, label, &medium)?;

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
    // Issue #234, in the SAME transaction and for the same reason the
    // comment above gives: a `quarantined` status with no
    // `verification_sessions` row justifying it — or a session row claiming
    // a proven-bad medium with the volume still counting as a copy — is the
    // same defect, one level up. They describe one verify; a crash must
    // leave neither.
    let quarantine = quarantine_on_medium_evidence(&tx, volume_id, label, &evidence)?;
    // The inverse half (ADR-0012's 2026-09-18 amendment, issue #268), in the
    // SAME transaction and for the same reason the comment above gives: the
    // session row and the condition it justifies describe one verify, so a
    // crash must leave neither. Exactly one of these two can do anything on
    // any given run -- `quarantine_on_medium_evidence` needs a mismatch that
    // proves the medium bad, this one needs no mismatches at all.
    let cleared = clear_condition_on_clean_full_verify(&tx, volume_id, label, tier, &evidence)?;
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
        session_id: Some(session_id),
        // Filled by `volume_verify_with_store`, which holds the guard.
        contact_id: None,
        checked: evidence.files_checked as usize,
        passed: (evidence.files_checked as usize).saturating_sub(evidence.mismatches.len()),
        failed: evidence.mismatches.len(),
        mismatches: evidence.mismatches,
        quarantine,
        cleared,
        // Set by `volume_verify`, which is the only caller with a device and
        // a backend to resolve; this store-injectable half has neither.
        drive_health_note: None,
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

/// [`volume_identify`], corroborated against the catalog **where there is a
/// catalog row to compare against** (ADR-0012, issue #193).
///
/// A separate function rather than parameters on [`volume_identify`], for
/// the reason that function's own doc gives: it is the DB-less File 0
/// reader, mirrored by `volume::raw`, and an heir path that needs no
/// `Connection` is the point of it. This wraps it for the ordinary operator,
/// who does have a catalog and would rather be told the tape in the drive is
/// bound to a different cartridge than read it off the screen themselves.
///
/// Absence-tolerant throughout, and more so than any other contact, since
/// `identify` is what an operator runs precisely when they do not know what
/// is loaded: a File 0 naming a volume this catalog has never heard of is
/// not a contradiction — it is the answer — so it prints and returns `Ok`.
pub fn volume_identify_corroborated(
    conn: &Connection,
    store: &mut dyn Store,
    site: ContactSite<'_>,
) -> Result<Identified> {
    // `volume_id` is NULL here, deliberately: `identify` takes no label and
    // runs against whatever tape is loaded, which is one of the two cases
    // migration 020 names for the column being nullable. The volume this
    // tape claims to be is discovered BELOW, from File 0 — a claim, not the
    // command's subject, and recording it in the contact's `volume_id`
    // would turn the tape's own assertion into the catalog's.
    let guard = site.open(conn, None);
    let r = identify_contacted(conn, store, site.medium_serial());
    // A contradiction is what the CLI turns into a non-zero exit, so it is
    // how this contact ENDED even though the function returns `Ok` — the
    // same split `volume_verify_with_store` makes for a failed verify.
    match &r {
        Ok(Identified {
            contradiction: Some(why),
            ..
        }) => guard.finish(contact::OUTCOME_FAILED, Some(why)),
        Ok(_) => guard.finish(contact::OUTCOME_OK, None),
        Err(e) => guard.finish(contact::OUTCOME_FAILED, Some(&e.to_string())),
    }
    r
}

/// [`volume_identify_corroborated`] minus the contact bookkeeping.
fn identify_contacted(
    conn: &Connection,
    store: &mut dyn Store,
    medium_serial: Option<&str>,
) -> Result<Identified> {
    let text = volume_identify(store)?;
    let file0 = binding::file0_facts_from_text(&text);
    // The volume this tape says it is — not one named on the command line,
    // because `identify` takes no label. No File 0 label, or a label no row
    // matches, is an absence: nothing to corroborate, so nothing refused.
    let Some(label) = file0.label.clone() else {
        return Ok(Identified::agreed(text));
    };
    let volume_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            params![label],
            |r| r.get(0),
        )
        .optional()?;
    let Some(volume_id) = volume_id else {
        return Ok(Identified::agreed(text));
    };
    let medium = binding::MediumFacts::new(medium_serial.map(str::to_string), file0);
    // `identify` REPORTS a contradiction, it does not withhold the answer.
    // Every other contact refuses, because every other contact is about to
    // act on the tape. This one exists to tell the operator what is loaded,
    // and it is what they run precisely WHEN the catalog and the shelf have
    // stopped agreeing — so swallowing File 0 here denies them the one fact
    // that resolves it. ADR-0004's advisory rule, and `catalog locate`'s
    // precedent of listing rows it disapproves of rather than hiding them.
    match binding::corroborate_volume(conn, volume_id, &label, &medium) {
        Ok(_) => Ok(Identified::agreed(text)),
        Err(e) => Ok(Identified {
            text,
            contradiction: Some(e.to_string()),
        }),
    }
}

/// What `volume identify` found: the tape's own File 0 text, and whatever the
/// catalog says that contradicts it (issue #193).
///
/// Two fields rather than a `Result` because they are not alternatives — a
/// contradicted tape still has an identity, and printing it is the whole job.
/// The caller prints `text` either way and treats `contradiction` as the exit
/// status, so a human sees the answer and a script can still detect the
/// disagreement.
#[derive(Debug)]
pub struct Identified {
    pub text: String,
    pub contradiction: Option<String>,
}

impl Identified {
    fn agreed(text: String) -> Self {
        Self {
            text,
            contradiction: None,
        }
    }
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
    site: ContactSite<'_>,
) -> Result<ReadSlicesReport> {
    // The source volume, looked up twice — once here only so the contact
    // can name it, and once inside where the refusal it produces is the
    // documented `VolumeNotFound`. An absent row is not an absent contact:
    // by the time this seam runs, the caller has already opened the device
    // and read the MAM, so a tape was in a drive whether or not the label
    // resolves — which is exactly why `volume_id` is nullable.
    let from_vol_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            params![from_label],
            |row| row.get(0),
        )
        .optional()?;
    let guard = site.open(conn, from_vol_id);
    guard.finish_result(read_slices_contacted(
        conn,
        config,
        from_label,
        unit_name,
        store,
        site.medium_serial(),
    ))
}

/// [`read_slices`] minus the contact bookkeeping.
fn read_slices_contacted(
    conn: &Connection,
    config: &Config,
    from_label: &str,
    unit_name: &str,
    store: &mut dyn Store,
    medium_serial: Option<&str>,
) -> Result<ReadSlicesReport> {
    // Look up source volume
    let from_vol_id: i64 = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            params![from_label],
            |row| row.get(0),
        )
        .map_err(|_| TapectlError::VolumeNotFound(from_label.to_string()))?;

    // Corroborate at contact, before a single slice is read (ADR-0012,
    // issue #193). Slices read off the wrong tape would fail their sha256
    // one at a time with no word about WHY; this says it once, up front.
    let medium = binding::MediumFacts::new(
        medium_serial.map(str::to_string),
        binding::read_file0_facts(store),
    );
    binding::corroborate_volume(conn, from_vol_id, from_label, &medium)?;

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
    site: ContactSite<'_>,
) -> Result<CompactReadReport> {
    // The operation travels with the SITE, not as a constant here: this one
    // function serves two commands. `volume compact-read` records
    // `Operation::VolumeCompactRead`; the interactive `volume compact`
    // records `Operation::VolumeCompact` for its step 1, and makes a second,
    // separate contact through `volume_write` for step 2 — which is
    // physically what happens, because the read-only store must close before
    // the same device can be opened for writing.
    let volume_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            params![label],
            |row| row.get(0),
        )
        .optional()?;
    let guard = site.open(conn, volume_id);
    guard.finish_result(compact_read_contacted(
        conn,
        config,
        label,
        store,
        site.medium_serial(),
    ))
}

/// [`compact_read`] minus the contact bookkeeping.
fn compact_read_contacted(
    conn: &Connection,
    config: &Config,
    label: &str,
    store: &mut dyn Store,
    medium_serial: Option<&str>,
) -> Result<CompactReadReport> {
    let volume_id: i64 = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            params![label],
            |row| row.get(0),
        )
        .map_err(|_| TapectlError::VolumeNotFound(label.to_string()))?;

    // Corroborate at contact (ADR-0012, issue #193). `compact-read` is a
    // contact issue #193 did not name — it reads slices off a tape by
    // position exactly as `read-slices` does, and is wired with it.
    let medium = binding::MediumFacts::new(
        medium_serial.map(str::to_string),
        binding::read_file0_facts(store),
    );
    binding::corroborate_volume(conn, volume_id, label, &medium)?;

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
    /// ADR-0004-eligible copies this unit still has on some OTHER volume,
    /// across ANY snapshot. Display only — it is deliberately NOT what
    /// either consent tier reads (issue #147): see `RetireImpact::at_stake`
    /// for why a per-unit count cannot answer a per-version question.
    pub other_copies: i64,
    pub evidence: Vec<crate::policy::evidence::CoverageEvidence>,
}

/// Compact-finish: retire the source volume after compaction.
///
/// **Three gates, in this order, and the order is the ADR-0008 tier
/// order** (ADR-0012, issue #147 — this command shipped the tiers
/// inverted, prompting at zero and letting `--force` through):
///
/// 1. **Tier 3, absolute — unprotected live SLICES.** Any live slice on
///    this volume with no copy on another volume refuses outright. No flag
///    defeats it, and it runs first so that `--yes` can never reach it.
/// 2. **Tier 3, absolute — the last eligible COPY of a live version**
///    (`cli::operations::refuse_last_eligible_copy`, which takes no
///    `force` parameter at all). A different fact from gate 1 and neither
///    subsumes the other: gate 1 asks whether these BYTES exist on another
///    cartridge, gate 2 asks whether the CATALOG will still credit a copy
///    of that version afterwards. A slice can be safely duplicated while
///    the volume carrying it is the last eligible copy of some other
///    unit's version — and gate 1, which reads `write_positions`, sees
///    nothing of a unit whose stage set has no position rows at all.
/// 3. **Tier 2, overridable.** If the retirement leaves a live version
///    below its RESOLVED policy but above zero
///    (`cli::operations::below_policy_facts`), or leaves a unit the impact
///    analysis reads as zero-copy without gate 2 having fired, the
///    coverage facts are displayed and consent is required;
///    `--force`/`--yes` overrides, and a non-interactive session with
///    neither refuses rather than hanging.
///
/// Tier 1 (ADR-0004) is unchanged and never gates: evidence age for the
/// units that DO retain coverage is shown at the moment consent is asked.
pub fn compact_finish(
    conn: &Connection,
    config: &crate::config::Config,
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
    let impacts = crate::cli::operations::retire_impacts(conn, vol_id)?;
    let action = format!("retire volume \"{label}\" (compact-finish)");

    // ADR-0008 TIER 3, gate 2 (ADR-0012, issue #147). Still before any
    // consent, and still with no `force` in scope to defeat it -- including
    // through the `compact` wrapper, which calls this as its step 3 with
    // `*force || yes` and must not be able to buy past the floor with it.
    crate::cli::operations::refuse_last_eligible_copy(conn, &action, label, &impacts)?;

    // ADR-0008 TIER 2 (issue #147).
    let below_policy = crate::cli::operations::below_policy_facts(conn, config, &impacts)?;

    let report: Vec<CompactFinishReport> = impacts
        .into_iter()
        .map(|impact| CompactFinishReport {
            unit_name: impact.unit_name,
            unit_status: impact.unit_status,
            other_copies: impact.other_copies,
            evidence: impact.evidence,
        })
        .collect();

    let at_risk: Vec<&CompactFinishReport> =
        report.iter().filter(|u| u.other_copies == 0).collect();
    if !at_risk.is_empty() || !below_policy.is_empty() {
        let mut facts: Vec<String> = at_risk
            .iter()
            .map(|u| {
                format!(
                    "unit \"{}\" [{}] would have ZERO copies remaining after this retirement",
                    u.unit_name, u.unit_status
                )
            })
            .collect();
        facts.extend(below_policy);
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
    /// The `verification_sessions` row this verify wrote (issue #295).
    ///
    /// The session is created inside
    /// [`volume_verify_with_store`] and was not carried out of it, which is
    /// why `health_logs.session_id` — a column with a foreign key to that
    /// table since `001_initial.sql` — had no writer for the whole life of
    /// the project. `volume_verify` is the only caller that then collects
    /// drive health, so this is the one route by which the reading and the
    /// session that produced it can be joined.
    ///
    /// `None` only on a `VerifyReport` no verify produced (`Default`), never
    /// on a real one.
    pub session_id: Option<i64>,
    /// The `cartridge_contacts` row this verify opened (issue #296) — the
    /// same shape, and for the same reason, as `session_id` above.
    ///
    /// The contact is opened AND closed inside [`volume_verify_with_store`],
    /// and `volume_verify` collects drive health only after that, so without
    /// this field the reading could not name its contact
    /// (`health_logs.contact_id`, ADR-0013 §2) and the drive `sg_logs`
    /// identified could not be attached to it (`cartridge_contacts.drive_id`
    /// — NULL on every real row before this, which is what left "is it the
    /// drive or the tape?" unanswerable).
    ///
    /// `None` on a `Default` report, and on a real one only when the
    /// contact INSERT itself failed (an inert guard).
    pub contact_id: Option<i64>,
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
    /// What this verify did to `volumes.observed_condition` (issue #234),
    /// and why.
    ///
    /// **Not `status`** — issue #242 moved quarantine to its own column and
    /// this doc said `status` until issue #266 caught it. The effect's field
    /// is `previous_condition`, and `volumes.status` is never written here;
    /// `--json`'s `previous_status`/`status_changed` are kept by the
    /// additive-key rule and honestly report a status that does not move.
    ///
    /// `Some` exactly when at least one mismatch PROVES the medium is bad
    /// (ADR-0012's 2026-09-17 amendment,
    /// [`crate::store::Evidence::medium_evidence`]) — so it is the report's
    /// answer to "is this tape bad, or could this drive just not read it
    /// today?", which the amendment requires be visible in both the human
    /// output and `--json`.
    ///
    /// `None` on a clean verify AND on a failed one whose every mismatch is
    /// a read or transport failure. The second case is a failure — `failed`
    /// is non-zero and the exit code is still `EXIT_ERROR` — that left the
    /// volume exactly as it was.
    pub quarantine: Option<QuarantineEffect>,
    /// What a clean FULL verify did to `volumes.observed_condition` — the
    /// inverse of [`VerifyReport::quarantine`] (ADR-0012's 2026-09-18
    /// amendment, issue #268).
    ///
    /// `Some` only when this verify found no mismatches at all, ran at
    /// [`Tier::Integrity`], AND the volume was quarantined beforehand — so
    /// it reports a real return to service and is `None` on the ordinary
    /// clean verify of a healthy volume, which must not look like an event.
    pub cleared: Option<ConditionCleared>,
    /// Set when drive-health (`sg_logs`) collection was SKIPPED rather than
    /// attempted (issue #187): no backend resolves for the device verify was
    /// given, so there is no sg node to read. `None` means collection was
    /// attempted (it may still have failed silently, as it always could —
    /// this field is only about the "skipped, and said nothing" defect).
    /// Verify still runs and still verifies either way; this is advisory
    /// only, never a reason to fail the command.
    pub drive_health_note: Option<String>,
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

/// Render the selection `volume write` is about to put on tape (issue #201):
/// one line per unit and version, plus a total. This is write-once media, so
/// an operator who did not mean to write everything still `stage create`-d
/// needs to be told BEFORE contact, not after — but announcing is display,
/// never a gate (ADR-0004 Tier 1: displayed, never blocks; ADR-0006 owns the
/// stage-once / write-N-copies / release design this is a window onto, and
/// that design is not changed here).
///
/// Deliberately follows `volume plan`'s existing wording
/// (`cli::volume::VolumeCommands::Plan`, the "`{name} v{ver}: N slices, <size>`"
/// row and the "`total: N slices, <size>`" summary) rather than inventing a
/// second vocabulary. It drops Plan's trailing "x {copies}" term: `volume
/// write` always writes exactly the one physical volume already named on the
/// command line, never a copy count. Matches Plan's row shape exactly,
/// including printing "N slices" for N == 1 — Plan does not pluralize either
/// (`src/cli/volume.rs`'s `Plan` arm), and inventing agreement here alone
/// would be exactly the second vocabulary this is meant to avoid.
///
/// A pure function on purpose (issue #201's second trap): its exact text is
/// pinned by a test without a tape, a database, or capturing this process's
/// own stderr.
fn render_staged_selection(label: &str, units: &[BuildUnit]) -> String {
    let mut out = format!("about to write to volume \"{label}\":\n");
    let mut total_slices: i64 = 0;
    let mut total_bytes: i64 = 0;
    for u in units {
        let slices = u.slices.len() as i64;
        let bytes: i64 = u.slices.iter().map(|s| s.encrypted_bytes).sum();
        total_slices += slices;
        total_bytes += bytes;
        out.push_str(&format!(
            "  {} v{}: {slices} slices, {}\n",
            u.unit_name,
            u.snapshot_version,
            crate::util::format_bytes_binary(bytes),
        ));
    }
    out.push_str(&format!(
        "\ntotal: {total_slices} slices, {}\n",
        crate::util::format_bytes_binary(total_bytes),
    ));
    out
}

/// Print the announcement to stderr, so `--json` stdout stays parseable —
/// the same rule `report_binding`'s doc comment already states (it is
/// `volume_init`'s post-bind reporting helper, defined just after it), not
/// a new one. Called from `volume_write` right after the staged selection
/// is gathered and confirmed non-empty: before backend resolution, before
/// any MAM read, before `TapeStore::open` — before anything that touches
/// tape or even names a drive.
fn announce_staged_selection(label: &str, units: &[BuildUnit]) {
    eprint!("{}", render_staged_selection(label, units));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Evidence, Mismatch, MismatchKind};
    use crate::tape::mam::MamInfo;
    use sha2::{Digest, Sha256};

    /// A device path for tests. NOT `/dev/null` and not `/dev/nstN`: no
    /// ungated test may open a device node, and `detect`'s ladder would try.
    /// Nothing here opens it — the contact seam never reads the drive.
    const TEST_DEVICE: &str = "/nonexistent/tapectl-contact-test-nst";

    /// The `ContactSite` a `MemStore` test has: no configured backend, which
    /// is the honest description of a machine with no drive at all (ADR-0005's
    /// DR shape), so nothing about a drive that is not there is invented.
    ///
    /// Replaces the bare `None` these call sites used to pass for
    /// `medium_serial` — the same absence, now carrying the reason for it.
    fn site(operation: Operation) -> ContactSite<'static> {
        static CFG: std::sync::OnceLock<Config> = std::sync::OnceLock::new();
        ContactSite::new(
            CFG.get_or_init(Config::default),
            operation,
            TEST_DEVICE,
            Medium::NoBackend,
        )
    }

    /// A site whose drive DID report a medium serial — the corroboration
    /// path's `Some(serial)`, now inseparable from the MAM reading the
    /// contact records.
    fn site_observed(operation: Operation, mam: &MamInfo) -> ContactSite<'_> {
        static CFG: std::sync::OnceLock<Config> = std::sync::OnceLock::new();
        static BK: std::sync::OnceLock<crate::config::LtoBackendConfig> =
            std::sync::OnceLock::new();
        ContactSite::new(
            CFG.get_or_init(Config::default),
            operation,
            TEST_DEVICE,
            Medium::Observed {
                backend: BK.get_or_init(|| crate::config::LtoBackendConfig {
                    name: "lto0".to_string(),
                    device_tape: TEST_DEVICE.to_string(),
                    device_sg: "/nonexistent/tapectl-contact-test-sg".to_string(),
                    generation: "LTO-6".to_string(),
                    capacity_override: None,
                    usable_capacity_factor: 0.95,
                    enospc_buffer: "1GiB".to_string(),
                }),
                mam,
            },
        )
    }

    /// A MAM reading carrying just a serial — the only field every
    /// corroboration call site here ever used.
    fn mam_serial(serial: &str) -> MamInfo {
        MamInfo {
            serial: Some(serial.to_string()),
            ..MamInfo::default()
        }
    }

    /// `(operation, outcome, detail)` of the one contact row, asserted BY
    /// VALUE — `is_some()` cannot tell `ok` from `failed`, which is the
    /// whole question these tests ask.
    fn only_contact(conn: &Connection) -> (String, Option<String>, Option<String>) {
        conn.query_row(
            "SELECT operation, outcome, detail FROM cartridge_contacts",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }

    fn contact_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM cartridge_contacts", [], |r| r.get(0))
            .unwrap()
    }

    /// `pause_after_seal_marker_from_env` is the single boundary where the
    /// variable is read (never inside `park_after_seal` or `finish_session`),
    /// mirroring `session::park_marker_is_absent_by_default`'s own reasoning:
    /// no test should have to mutate process-global environment state to
    /// exercise the hook, since that would leak into every other test
    /// running in parallel in this binary.
    #[test]
    fn pause_after_seal_marker_is_absent_by_default() {
        assert!(
            pause_after_seal_marker_from_env().is_none(),
            "TAPECTL_TEST_PAUSE_AFTER_SEAL must not be set in the test environment; if \
             this fails, something is exporting it and every write is parking"
        );
    }

    /// Issue #147 / ADR-0012: `compact-finish` shipped ADR-0008's tiers
    /// inverted — it prompted at zero coverage and let `--force` through,
    /// and it did not gate below-policy coverage at all. These prove the
    /// unprotected-SLICE refusal still comes first and absolutely, that the
    /// last-eligible-COPY floor is a second absolute refusal no flag
    /// reaches, that below-policy coverage now gates at Tier 2 where a flag
    /// does waive it, and that a non-interactive session refuses rather
    /// than hangs. Every test passes `assume_yes` or asserts the non-TTY
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
        /// reclaimable snapshots and nothing else looked. Tier 2, not the
        /// floor: every version in this fixture is RELEASED, so retiring
        /// the volume removes nothing ADR-0008 protects.
        #[test]
        fn zero_remaining_copies_now_refuses_without_consent() {
            let conn = setup(false);
            let err = compact_finish(&conn, &crate::config::Config::default(), "L6-SRC", false)
                .expect_err("a unit dropping to zero copies must gate (ADR-0008 Tier 2)");
            assert!(err.to_string().contains("refused"), "got: {err}");
            assert_eq!(
                status_of(&conn, "L6-SRC"),
                "sealed",
                "a refused compact-finish must not retire the volume"
            );
        }

        /// Tier 2, and `--force`/`--yes` genuinely overrides it — because
        /// this fixture's versions are RELEASED (`reclaimable`), which is
        /// the operator having already given them up. Change that one word
        /// to `current` and the same call is refused outright:
        /// `tier3_refuses_the_last_eligible_copy_of_a_live_version` below
        /// is exactly that test.
        #[test]
        fn assume_yes_overrides_the_gate_when_every_version_is_released() {
            let conn = setup(false);
            compact_finish(&conn, &crate::config::Config::default(), "L6-SRC", true)
                .expect("--yes must override a Tier-2 gate");
            assert_eq!(status_of(&conn, "L6-SRC"), "retired");
        }

        /// The ordinary case is untouched: every affected unit keeps a copy,
        /// so no gate is reached and no consent is needed. This is what the
        /// mhvtl lifecycle suite and `tests/integration.rs` exercise, both
        /// of which run with stdin closed and no `--yes`.
        #[test]
        fn a_unit_that_keeps_a_copy_needs_no_consent_at_all() {
            let conn = setup(true);
            compact_finish(&conn, &crate::config::Config::default(), "L6-SRC", false)
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

            let err = compact_finish(&conn, &crate::config::Config::default(), "L6-SRC", true)
                .expect_err("no flag may defeat the Tier-3 refusal (ADR-0008)");
            assert!(
                err.to_string().contains("have no copy on another volume"),
                "the Tier-3 refusal must be the one that fired; got: {err}"
            );
            assert_eq!(status_of(&conn, "L6-SRC"), "sealed");
        }

        // ── The second Tier-3 floor (ADR-0012, issue #147) ──

        /// The same fixture with the version LIVE rather than released.
        /// Deliberately no `stage_slices`/`write_positions`, so gate 1 (the
        /// unprotected-slice check, which reads `write_positions`) finds
        /// nothing and the COPY floor is exercised in isolation. That
        /// divergence is not hypothetical: a unit whose stage set has no
        /// position rows is invisible to gate 1 entirely.
        fn setup_live(with_other_copy: bool) -> Connection {
            let conn = setup(with_other_copy);
            conn.execute("UPDATE snapshots SET status = 'current'", [])
                .unwrap();
            conn
        }

        #[test]
        fn tier3_refuses_the_last_eligible_copy_of_a_live_version() {
            let conn = setup_live(false);
            let err = compact_finish(&conn, &crate::config::Config::default(), "L6-SRC", false)
                .expect_err("the last eligible copy of a live version must be refused");
            assert!(err.to_string().contains("LAST eligible copy"), "got: {err}");
            assert_eq!(status_of(&conn, "L6-SRC"), "sealed");
        }

        /// The trap issue #147 names explicitly: the `compact` wrapper
        /// calls this as step 3 with `*force || yes`, so the floor has to
        /// be undefeatable by that argument and not merely un-prompted.
        #[test]
        fn tier3_is_not_defeated_by_assume_yes() {
            let conn = setup_live(false);
            let err = compact_finish(&conn, &crate::config::Config::default(), "L6-SRC", true)
                .expect_err("no flag may defeat ADR-0008 Tier 3");
            let msg = err.to_string();
            assert!(msg.contains("no --force for this"), "got: {msg}");
            assert!(
                !msg.contains("refused: non-interactive session")
                    && !msg.contains("aborted, not confirmed"),
                "the floor's text must not look like a consent refusal, or the \
                 `compact` wrapper will offer --force as the way out: {msg}"
            );
            assert_eq!(status_of(&conn, "L6-SRC"), "sealed");
        }

        /// ADR-0008 Tier 2, the limb that did not exist: one copy left
        /// against the shipped `min_copies = 2` is below policy and above
        /// zero, so it gates — and a flag waives it.
        #[test]
        fn tier2_gates_a_below_policy_version_and_a_flag_waives_it() {
            let conn = setup_live(true);
            let err = compact_finish(&conn, &crate::config::Config::default(), "L6-SRC", false)
                .expect_err("one copy against min_copies = 2 must gate");
            assert!(err.to_string().contains("refused"), "got: {err}");
            assert_eq!(status_of(&conn, "L6-SRC"), "sealed");

            let conn = setup_live(true);
            compact_finish(&conn, &crate::config::Config::default(), "L6-SRC", true)
                .expect("--yes waives Tier 2, which is the whole distinction");
            assert_eq!(status_of(&conn, "L6-SRC"), "retired");
        }

        /// No gate at all when the surviving coverage meets policy: the
        /// ordinary compaction must not become a ceremony.
        #[test]
        fn no_gate_at_all_when_every_version_stays_at_or_above_policy() {
            let conn = setup_live(true);
            let mut config = crate::config::Config::default();
            config.defaults.min_copies_for_tape_only = 1;
            compact_finish(&conn, &config, "L6-SRC", false)
                .expect("one surviving copy meets a min_copies of 1");
            assert_eq!(status_of(&conn, "L6-SRC"), "retired");
        }

        /// Trap named in issue #147: the unprotected-live-slices refusal is
        /// a DIFFERENT fact and must survive intact. Here the slice has no
        /// copy anywhere AND the unit has another eligible volume, so only
        /// gate 1 can be what fires.
        #[test]
        fn the_unprotected_slice_refusal_is_still_absolute_and_still_first() {
            let conn = setup_live(true);
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

            let err = compact_finish(&conn, &crate::config::Config::default(), "L6-SRC", true)
                .expect_err("an unprotected live slice is its own absolute refusal");
            assert!(
                err.to_string().contains("have no copy on another volume"),
                "gate 1 must still fire, and still first: {err}"
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
        mem_store_v2_tape_identified(label, MEM_TAPE_UUID, None, slice_claimed, slice_on_tape)
    }

    /// The uuid every `mem_store_v2_tape` fixture writes into File 0, so a
    /// test that cares can put the same one on its `volumes` row.
    const MEM_TAPE_UUID: &str = "11111111-2222-3333-4444-555555555555";

    /// [`mem_store_v2_tape`] with File 0's cartridge identity spelled out:
    /// `cartridge` is `[media].cartridge_serial` plus its identity source,
    /// `None` meaning a pre-#192 thunk that omits both.
    ///
    /// File 0 is generated by `layout::generate_id_thunk_v2` — the SAME
    /// producer the write path uses — rather than hand-rolled TOML, so what
    /// the contact check parses here is the bytes a real tape carries. A
    /// hand-rolled thunk with no uuid parsed as an absence, which silently
    /// disabled the corroboration these fixtures exist to exercise.
    fn mem_store_v2_tape_identified(
        label: &str,
        uuid: &str,
        cartridge: Option<(&str, Option<&str>)>,
        slice_claimed: &[u8],
        slice_on_tape: &[u8],
    ) -> MemStore {
        const BS: usize = 4096;
        let (mam_serial, identity_source) = cartridge.unwrap_or(("", None));
        let id_thunk = layout::generate_id_thunk_v2(&layout::IdThunkV2Params {
            label,
            uuid,
            media_type: "LTO-6",
            tapectl_version: "0.0.0-test",
            nominal_capacity: 2_500_000_000_000,
            mam_capacity: 2_400_000_000_000,
            total_files: 6,
            mam_manufacturer: "TESTCO",
            mam_serial,
            mam_length: 846,
            mam_loads: 1,
            created_at: "2026-09-16T00:00:00Z",
            cartridge_identity_source: identity_source,
        })
        .into_bytes();
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
            site(Operation::VolumeVerify),
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
            site(Operation::VolumeVerify),
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

    /// `VerifyReport` must carry the `verification_sessions` row OUT of the
    /// function that created it (issue #295).
    ///
    /// Without this field the session id died inside
    /// `volume_verify_with_store`, which is why `health_logs.session_id` —
    /// declared with a foreign key to that table since `001_initial.sql` —
    /// had no writer for the whole life of the project. `volume_verify` is
    /// the only caller that then collects drive health, and it can only pass
    /// what the report hands it.
    ///
    /// Positive control: the id must EQUAL the row the verify wrote, not
    /// merely be `Some`.
    #[test]
    fn a_verify_carries_its_session_id_out_on_the_report() {
        let conn = crate::db::open_memory().unwrap();
        let good = b"bytes whose verify must be joinable to its session. ".repeat(4);
        seed_one_slice_fixture(
            &conn,
            "VR-SESSION",
            "vs-unit",
            4,
            &good,
            "completed",
            "staged",
        );
        let volume_id: i64 = conn
            .query_row(
                "SELECT id FROM volumes WHERE label = 'VR-SESSION'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        let mut store = mem_store_v2_tape("VR-SESSION", &good, &good);
        let report = volume_verify_with_store(
            &conn,
            &mut store,
            "VR-SESSION",
            volume_id,
            4096,
            Tier::Integrity,
            site(Operation::VolumeVerify),
        )
        .unwrap();

        let session_id: i64 = conn
            .query_row(
                "SELECT id FROM verification_sessions WHERE volume_id = ?1",
                params![volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(report.session_id, Some(session_id));
    }

    /// `VerifyReport` must carry the `cartridge_contacts` row OUT of the
    /// seam that opened and closed it (issue #296) — `session_id`'s shape,
    /// for the same reason: `volume_verify` collects drive health only after
    /// the guard is gone, and can only name what the report hands it.
    ///
    /// By VALUE, against a second, unrelated contact opened first, so the
    /// assertion cannot pass on "the first/only contact row" by accident.
    #[test]
    fn a_verify_carries_its_contact_id_out_on_the_report() {
        let conn = crate::db::open_memory().unwrap();
        let good = b"bytes whose verify must name the contact it was made in. ".repeat(4);
        seed_one_slice_fixture(
            &conn,
            "VR-CONTACT",
            "vc-unit",
            4,
            &good,
            "completed",
            "staged",
        );
        let volume_id: i64 = conn
            .query_row(
                "SELECT id FROM volumes WHERE label = 'VR-CONTACT'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        // A decoy contact: an earlier, unrelated one.
        let decoy = site(Operation::VolumeIdentify).open(&conn, None);
        let decoy_id = decoy
            .id()
            .expect("positive control: the decoy contact exists");
        decoy.finish(contact::OUTCOME_OK, None);

        let mut store = mem_store_v2_tape("VR-CONTACT", &good, &good);
        let report = volume_verify_with_store(
            &conn,
            &mut store,
            "VR-CONTACT",
            volume_id,
            4096,
            Tier::Integrity,
            site(Operation::VolumeVerify),
        )
        .unwrap();

        let verify_contact: i64 = conn
            .query_row(
                "SELECT id FROM cartridge_contacts
                  WHERE volume_id = ?1 AND operation = 'volume verify'",
                params![volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(
            verify_contact, decoy_id,
            "positive control: two distinct contacts"
        );
        assert_eq!(report.contact_id, Some(verify_contact));
    }

    /// A contact on `conn` for the health-recorder tests below, plus a volume
    /// to hang the reading on.
    fn contact_and_volume(conn: &Connection, label: &str) -> (i64, i64) {
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
            params![label],
        )
        .unwrap();
        let vid = conn.last_insert_rowid();
        let guard = site(Operation::VolumeWrite).open(conn, Some(vid));
        let cid = guard
            .id()
            .expect("positive control: the contact row exists");
        guard.finish(contact::OUTCOME_OK, None);
        (vid, cid)
    }

    fn identity_with_serial(serial: Option<&str>) -> drive_identity::DriveIdentity {
        drive_identity::DriveIdentity {
            serial: serial.map(str::to_string),
            vendor: Some("HP".to_string()),
            model: Some("Ultrium 6-SCSI".to_string()),
            firmware_rev: Some("35GD".to_string()),
        }
    }

    fn drive_of_contact(conn: &Connection, contact_id: i64) -> Option<i64> {
        conn.query_row(
            "SELECT drive_id FROM cartridge_contacts WHERE id = ?1",
            params![contact_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// THE attribution issue #296 exists for, by value: a health reading
    /// names the contact it was taken in, and that contact names the drive
    /// the reading identified. Before this, `record_drive` had no production
    /// caller and `cartridge_contacts.drive_id` was NULL on every real row
    /// (20 of 20 in the pass-1 mhvtl gate).
    ///
    /// Two contacts and two drives, with the reading attributed to the
    /// SECOND of each — so "some contact got some drive" cannot pass for
    /// "this contact got this drive". `collect_health_best_effort` only
    /// WARNS on a failed insert, so the rows are asserted to EXIST, not
    /// inferred from the absence of an error.
    #[test]
    fn a_health_reading_names_its_contact_and_the_contact_names_its_drive() {
        let conn = crate::db::open_memory().unwrap();
        let (_, other_cid) = contact_and_volume(&conn, "HR-OTHER");
        let other_drive =
            drive_identity::upsert(&conn, &identity_with_serial(Some("OTHER_SERIAL")))
                .unwrap()
                .unwrap();
        let (vid, cid) = contact_and_volume(&conn, "HR-THIS");
        assert_ne!(cid, other_cid, "positive control: two distinct contacts");

        let counters = health::HealthCounters {
            total_uncorrected: 2,
            tape_alerts: 0,
            ..Default::default()
        };
        let drive_id = record_health_and_drive(
            &conn,
            Some(vid),
            Some(cid),
            None,
            health::Reading::Write,
            Some((&counters, "=== page 0x02 ===\nraw")),
            identity_with_serial(Some("HUJ808A5L4")),
            "/dev/nst-test",
        )
        .expect("an identity with a serial must produce a drive");

        let expected_drive: i64 = conn
            .query_row(
                "SELECT id FROM drives WHERE serial = 'HUJ808A5L4'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(
            expected_drive, other_drive,
            "positive control: two distinct drives"
        );
        assert_eq!(drive_id, expected_drive);
        assert_eq!(
            drive_of_contact(&conn, cid),
            Some(expected_drive),
            "the contact must name the drive this reading identified"
        );
        assert_eq!(
            drive_of_contact(&conn, other_cid),
            None,
            "an unrelated contact must not be touched"
        );

        let rows: Vec<(Option<i64>, Option<i64>, String, i64)> = conn
            .prepare("SELECT volume_id, contact_id, operation, total_uncorrected FROM health_logs")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![(Some(vid), Some(cid), "write".to_string(), 2)],
            "exactly one health row, naming THIS contact"
        );
    }

    /// `sg_logs` failed: there is no reading, so no health row — but the
    /// drive was still the one contacted, and its identity is read by a
    /// different route, so the contact still gets its drive.
    #[test]
    fn a_failed_collection_still_attaches_the_drive_to_the_contact() {
        let conn = crate::db::open_memory().unwrap();
        let (vid, cid) = contact_and_volume(&conn, "HR-NOLOG");
        let drive_id = record_health_and_drive(
            &conn,
            Some(vid),
            Some(cid),
            None,
            health::Reading::Verify,
            None,
            identity_with_serial(Some("XYZZY_A1")),
            "/dev/nst-test",
        )
        .expect("the identity has a serial");
        assert_eq!(drive_of_contact(&conn, cid), Some(drive_id));
        let health_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM health_logs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            health_rows, 0,
            "no sg_logs output means no reading to record"
        );
    }

    /// A drive that publishes no serial gets no `drives` row (migration
    /// 019's rule) and the contact's `drive_id` stays NULL — unknown by
    /// absence, never guessed. The reading is still recorded and still
    /// names its contact: identity is an addition to the record, never a
    /// precondition for it.
    #[test]
    fn a_drive_with_no_serial_leaves_the_contact_without_a_drive() {
        let conn = crate::db::open_memory().unwrap();
        let (vid, cid) = contact_and_volume(&conn, "HR-NOSERIAL");
        let drive = record_health_and_drive(
            &conn,
            Some(vid),
            Some(cid),
            None,
            health::Reading::Write,
            Some((&health::HealthCounters::default(), "raw")),
            identity_with_serial(None),
            "/dev/nst-test",
        );
        assert_eq!(drive, None);
        assert_eq!(drive_of_contact(&conn, cid), None);
        let drives: i64 = conn
            .query_row("SELECT COUNT(*) FROM drives", [], |r| r.get(0))
            .unwrap();
        assert_eq!(drives, 0);
        let named: Option<i64> = conn
            .query_row("SELECT contact_id FROM health_logs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(named, Some(cid));
    }

    /// Every production path that writes a health row must hand it a
    /// contact (issue #296). The recorder is hardware-free and tested by
    /// value above; what no ungated test can drive is the three CALLERS
    /// (`sg_logs` needs a drive), so their wiring is pinned by source scan —
    /// calibrated by a positive control that the scan found each one.
    #[test]
    fn every_health_writer_passes_its_contact() {
        const SRC: &str = include_str!("write.rs");
        let prod = SRC.split("#[cfg(test)]\nmod tests").next().unwrap();
        assert!(
            prod.len() < SRC.len(),
            "positive control: the production half was separated from the tests"
        );
        assert_eq!(
            prod.matches("health::record(").count(),
            1,
            "health::record must have ONE production caller, record_health_and_drive — a \
             second writer is a second place to forget the contact"
        );
        for (f, needle) in [
            ("fn volume_write_contacted", "contact.id(),"),
            ("fn volume_resume_contacted", "contact.id(),"),
            ("pub fn volume_verify(", "report.contact_id,"),
        ] {
            let start = prod.find(f).unwrap_or_else(|| panic!("no {f}"));
            let end = prod[start..].find("\n}\n").unwrap() + start;
            let body = &prod[start..end];
            assert!(
                !body[f.len()..].contains("\npub fn "),
                "{f}: body extraction overran into another function"
            );
            assert!(
                body.contains("collect_health_best_effort(")
                    || body.contains("collect_and_record_health("),
                "positive control: {f} must still collect health"
            );
            assert!(
                body.contains(needle),
                "{f} must pass its contact ({needle}) to the health collection"
            );
        }
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
            site(Operation::VolumeVerify),
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

    // ── issue #234: a failed verify quarantines, but only on medium evidence ──

    /// The volume's `volumes.status`, read straight from the row rather than
    /// from a report — the whole point of issue #234 is what the CATALOG
    /// says afterwards, not what the command printed.
    fn volume_status(conn: &Connection, label: &str) -> String {
        conn.query_row(
            "SELECT status FROM volumes WHERE label = ?1",
            params![label],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// `volumes.observed_condition`, read straight from the row — companion
    /// to `volume_status` for ADR-0012's 2026-09-17 amendment (issue #242):
    /// a verify's quarantine finding moves this column now, never `status`.
    fn volume_condition(conn: &Connection, label: &str) -> String {
        conn.query_row(
            "SELECT observed_condition FROM volumes WHERE label = ?1",
            params![label],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// Every `events` row recorded against a volume, as `(action, old, new)`.
    fn volume_events(conn: &Connection, label: &str) -> Vec<(String, String, String)> {
        conn.prepare(
            "SELECT action, COALESCE(old_value, ''), COALESCE(new_value, '')
             FROM events WHERE entity_type = 'volume' AND entity_label = ?1
             ORDER BY id",
        )
        .unwrap()
        .query_map(params![label], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    }

    /// ADR-0012's 2026-09-17 amendment: a checksum mismatch PROVES the medium
    /// is bad, so the verify that found it must leave the volume
    /// `quarantined` — and the catalog must carry the fact that an operator
    /// tried to read this tape and it failed.
    ///
    /// Before issue #234 `volume_verify` recorded a `verification_sessions`
    /// row and left `volumes.status` alone, so the escape hatch ADR-0012
    /// names twice from its own Tier-3 refusal did not exist.
    #[test]
    fn a_content_hash_mismatch_quarantines_the_volume() {
        let conn = crate::db::open_memory().unwrap();
        let good = b"the bytes the front index promises. ".repeat(4);
        let rotted = b"the bytes the tape actually holds!! ".repeat(4);
        seed_one_slice_fixture(
            &conn,
            "Q-ROT",
            "q-rot-unit",
            4,
            &good,
            "completed",
            "current",
        );
        conn.execute(
            "UPDATE volumes SET status = 'sealed' WHERE label = 'Q-ROT'",
            [],
        )
        .unwrap();
        let volume_id: i64 = conn
            .query_row("SELECT id FROM volumes WHERE label = 'Q-ROT'", [], |r| {
                r.get(0)
            })
            .unwrap();

        let mut store = mem_store_v2_tape("Q-ROT", &good, &rotted);
        let report = volume_verify_with_store(
            &conn,
            &mut store,
            "Q-ROT",
            volume_id,
            4096,
            Tier::Integrity,
            site(Operation::VolumeVerify),
        )
        .unwrap();
        assert_eq!(report.failed, 1, "mismatches: {:?}", report.mismatches);

        // ADR-0012's 2026-09-17 amendment (issue #242): `status` is the
        // operator's and is never moved by a verify; the medium's condition
        // is its own fact.
        assert_eq!(
            volume_status(&conn, "Q-ROT"),
            "sealed",
            "a verify must never move volumes.status (issue #242)"
        );
        assert_eq!(
            volume_condition(&conn, "Q-ROT"),
            "quarantined",
            "a proven-bad medium must leave the volume's condition quarantined"
        );
        let effect = report.quarantine.as_ref().expect("the report must say so");
        assert_eq!(effect.previous_condition, "ok");
        assert!(effect.condition_changed());
        assert_eq!(effect.proof.len(), 1);
        assert_eq!(
            effect.proof[0].kind,
            crate::store::MismatchKind::ContentHashMismatch
        );
        let events = volume_events(&conn, "Q-ROT");
        assert!(
            events
                .iter()
                .any(|(action, old, new)| action.contains("quarantined")
                    && old == "ok"
                    && new == "quarantined"),
            "the catalog must record the condition transition: {events:?}"
        );
    }

    /// THE point of issue #234, end to end. ADR-0012's Tier-3 floor refuses
    /// to retire the last eligible copy of a live version and takes no flag
    /// by construction; the ADR names a failed verify as the one way out.
    /// `versions_at_stake`'s third condition (the volume passes `eligible`
    /// RIGHT NOW) is what makes the escape work — so quarantining is what
    /// converts the refusal into an ordinary Tier-2 retirement.
    #[test]
    fn a_medium_proving_verify_failure_unblocks_retiring_the_last_copy() {
        let conn = crate::db::open_memory().unwrap();
        let good = b"the only copy of this version, on one tape. ".repeat(4);
        let rotted = b"what the drive actually read back today!!!! ".repeat(4);
        assert_eq!(good.len(), rotted.len());
        seed_one_slice_fixture(
            &conn,
            "Q-LAST",
            "q-last-unit",
            4,
            &good,
            "completed",
            "current",
        );
        conn.execute(
            "UPDATE volumes SET status = 'sealed' WHERE label = 'Q-LAST'",
            [],
        )
        .unwrap();
        let volume_id: i64 = conn
            .query_row("SELECT id FROM volumes WHERE label = 'Q-LAST'", [], |r| {
                r.get(0)
            })
            .unwrap();

        // BEFORE: the floor refuses — otherwise the "after" assertion below
        // would be green for the wrong reason.
        let impacts = crate::cli::operations::retire_impacts(&conn, volume_id).unwrap();
        assert!(
            crate::cli::operations::refuse_last_eligible_copy(
                &conn,
                "retire volume \"Q-LAST\"",
                "Q-LAST",
                &impacts,
            )
            .is_err(),
            "fixture must actually trip the Tier-3 floor before the verify"
        );

        let mut store = mem_store_v2_tape("Q-LAST", &good, &rotted);
        let report = volume_verify_with_store(
            &conn,
            &mut store,
            "Q-LAST",
            volume_id,
            4096,
            Tier::Integrity,
            site(Operation::VolumeVerify),
        )
        .unwrap();
        assert_eq!(report.failed, 1, "mismatches: {:?}", report.mismatches);
        assert_eq!(
            volume_status(&conn, "Q-LAST"),
            "sealed",
            "a verify must never move volumes.status (issue #242)"
        );
        assert_eq!(volume_condition(&conn, "Q-LAST"), "quarantined");

        // AFTER: the same floor, on the same volume, no longer refuses.
        let impacts = crate::cli::operations::retire_impacts(&conn, volume_id).unwrap();
        crate::cli::operations::refuse_last_eligible_copy(
            &conn,
            "retire volume \"Q-LAST\"",
            "Q-LAST",
            &impacts,
        )
        .expect("a quarantined volume is no longer the last eligible copy of anything");
    }

    /// A clean verify changes nothing. The status assertion is the point:
    /// the quarantine is reached from inside the verify transaction, so a
    /// passing tape must come out the other side untouched.
    #[test]
    fn a_clean_verify_leaves_the_volume_status_alone() {
        let conn = crate::db::open_memory().unwrap();
        let good = b"intact slice bytes, repeated a few times. ".repeat(4);
        seed_one_slice_fixture(&conn, "Q-OK", "q-ok-unit", 4, &good, "completed", "current");
        conn.execute(
            "UPDATE volumes SET status = 'sealed' WHERE label = 'Q-OK'",
            [],
        )
        .unwrap();
        let volume_id: i64 = conn
            .query_row("SELECT id FROM volumes WHERE label = 'Q-OK'", [], |r| {
                r.get(0)
            })
            .unwrap();

        let mut store = mem_store_v2_tape("Q-OK", &good, &good);
        let report = volume_verify_with_store(
            &conn,
            &mut store,
            "Q-OK",
            volume_id,
            4096,
            Tier::Integrity,
            site(Operation::VolumeVerify),
        )
        .unwrap();

        assert_eq!(report.failed, 0, "mismatches: {:?}", report.mismatches);
        assert!(report.quarantine.is_none());
        assert_eq!(volume_status(&conn, "Q-OK"), "sealed");
        assert_eq!(volume_condition(&conn, "Q-OK"), "ok");
        assert!(volume_events(&conn, "Q-OK").is_empty());
    }

    /// An unparseable seal marker is `SealUnreadable`, whose own doc has
    /// always said it is the NORMAL signal for an unsealed tape, "never an
    /// error". It must not quarantine — and this goes through the real
    /// `volume_verify_with_store` call site, not the seam, so it proves the
    /// wiring and not just the predicate.
    #[test]
    fn a_seal_unreadable_failure_does_not_quarantine() {
        let conn = crate::db::open_memory().unwrap();
        let good = b"slice bytes that are perfectly fine on tape. ".repeat(4);
        seed_one_slice_fixture(
            &conn,
            "Q-SEAL",
            "q-seal-unit",
            4,
            &good,
            "completed",
            "current",
        );
        conn.execute(
            "UPDATE volumes SET status = 'sealed' WHERE label = 'Q-SEAL'",
            [],
        )
        .unwrap();
        let volume_id: i64 = conn
            .query_row("SELECT id FROM volumes WHERE label = 'Q-SEAL'", [], |r| {
                r.get(0)
            })
            .unwrap();

        let mut store = mem_store_v2_tape("Q-SEAL", &good, &good);
        store.files[5] = b"NOT TOML AT ALL [".to_vec();

        let report = volume_verify_with_store(
            &conn,
            &mut store,
            "Q-SEAL",
            volume_id,
            4096,
            Tier::Integrity,
            site(Operation::VolumeVerify),
        )
        .unwrap();

        assert_eq!(report.failed, 1, "mismatches: {:?}", report.mismatches);
        assert_eq!(
            report.mismatches[0].kind,
            crate::store::MismatchKind::SealUnreadable
        );
        assert!(
            report.quarantine.is_none(),
            "an unreadable seal is not evidence about the medium"
        );
        assert_eq!(
            volume_status(&conn, "Q-SEAL"),
            "sealed",
            "the volume's status must be exactly what it was"
        );
        assert_eq!(
            volume_condition(&conn, "Q-SEAL"),
            "ok",
            "the volume's condition must be exactly what it was"
        );
        assert!(
            volume_events(&conn, "Q-SEAL").is_empty(),
            "nothing to record: no quarantine happened"
        );
    }

    /// One `Evidence` with exactly one mismatch of `kind`, as `chain_walk`
    /// would have returned it.
    fn evidence_of(kind: crate::store::MismatchKind) -> crate::store::Evidence {
        crate::store::Evidence {
            tier: Tier::Integrity,
            files_checked: 2,
            mismatches: vec![crate::store::Mismatch {
                position: 3,
                kind,
                expected: "front index present and readable".into(),
                actual: "read failed: Input/output error".into(),
            }],
        }
    }

    /// THE crux of ADR-0012's 2026-09-17 amendment.
    ///
    /// `FrontIndexUnreadable` is produced by `store.rs` from a genuine I/O
    /// or transport error and from a short read; neither tells a bad tape
    /// from a dirty drive, a wrong block size or a transient SCSI error. It
    /// must leave the volume EXACTLY as it was, because `quarantined` is
    /// what makes a volume stop counting as a copy — a false one silently
    /// takes real coverage to zero, and a bad drive would condemn a library
    /// one cartridge at a time.
    ///
    /// Driven through the seam rather than a fixture because
    /// `volume_verify_with_store` reads File 3 itself before `confirm`, so
    /// no MemStore tape can reach this kind: a File 3 it cannot read fails
    /// earlier, as an `Err`. On a real drive it is exactly the transient
    /// case, which is why the arm exists at all.
    #[test]
    fn a_front_index_unreadable_failure_does_not_quarantine() {
        let conn = crate::db::open_memory().unwrap();
        let good = b"bytes that were never even reached. ".repeat(4);
        seed_one_slice_fixture(&conn, "Q-FI", "q-fi-unit", 4, &good, "completed", "current");
        conn.execute(
            "UPDATE volumes SET status = 'sealed' WHERE label = 'Q-FI'",
            [],
        )
        .unwrap();
        let volume_id: i64 = conn
            .query_row("SELECT id FROM volumes WHERE label = 'Q-FI'", [], |r| {
                r.get(0)
            })
            .unwrap();

        let effect = quarantine_on_medium_evidence(
            &conn,
            volume_id,
            "Q-FI",
            &evidence_of(crate::store::MismatchKind::FrontIndexUnreadable),
        )
        .unwrap();

        assert!(effect.is_none(), "\"could not read it today\" is not proof");
        assert_eq!(volume_status(&conn, "Q-FI"), "sealed");
        assert_eq!(volume_condition(&conn, "Q-FI"), "ok");
        assert!(volume_events(&conn, "Q-FI").is_empty());
    }

    /// **A passing FULL verify returns the volume to service** (ADR-0012's
    /// 2026-09-18 amendment, issue #268). Nothing in the tree ever wrote
    /// `observed_condition` back to `'ok'` — twelve write sites, all writing
    /// `'quarantined'` — so every quarantine was permanent whatever caused
    /// it, which is what made a false one expensive enough to argue about.
    ///
    /// The column holds what tapectl OBSERVED about the medium, so a later
    /// and better observation is precisely the thing entitled to update it.
    /// The operator instinct this serves — clean the drive, verify again —
    /// is one the tool should reward.
    #[test]
    fn a_clean_full_verify_returns_a_quarantined_volume_to_service() {
        let conn = crate::db::open_memory().unwrap();
        let good = b"bytes that verify cleanly. ".repeat(4);
        seed_one_slice_fixture(
            &conn,
            "Q-BACK",
            "q-back-unit",
            4,
            &good,
            "completed",
            "current",
        );
        conn.execute(
            "UPDATE volumes SET status = 'sealed', observed_condition = 'quarantined' \
             WHERE label = 'Q-BACK'",
            [],
        )
        .unwrap();
        let volume_id: i64 = conn
            .query_row("SELECT id FROM volumes WHERE label = 'Q-BACK'", [], |r| {
                r.get(0)
            })
            .unwrap();

        let cleared = clear_condition_on_clean_full_verify(
            &conn,
            volume_id,
            "Q-BACK",
            Tier::Integrity,
            &crate::store::Evidence {
                tier: Tier::Integrity,
                files_checked: 1,
                mismatches: vec![],
            },
        )
        .unwrap();

        assert!(
            cleared.is_some(),
            "a clean full verify must clear the condition it found"
        );
        assert_eq!(cleared.unwrap().previous_condition, "quarantined");
        assert_eq!(volume_condition(&conn, "Q-BACK"), "ok");
        assert_eq!(
            volume_status(&conn, "Q-BACK"),
            "sealed",
            "clearing the condition must not touch the operator's status"
        );
        assert!(
            !volume_events(&conn, "Q-BACK").is_empty(),
            "the return to service is a fact and must be recorded"
        );
    }

    /// A NAVIGABLE verify is not enough. Only a full readback can license
    /// the claim that the medium is sound, so a lesser tier leaves the
    /// condition exactly as it found it (ADR-0012, same amendment).
    #[test]
    fn a_quick_verify_does_not_clear_the_condition() {
        let conn = crate::db::open_memory().unwrap();
        let good = b"bytes. ".repeat(4);
        seed_one_slice_fixture(
            &conn,
            "Q-QUICK",
            "q-quick-unit",
            4,
            &good,
            "completed",
            "current",
        );
        conn.execute(
            "UPDATE volumes SET status = 'sealed', observed_condition = 'quarantined' \
             WHERE label = 'Q-QUICK'",
            [],
        )
        .unwrap();
        let volume_id: i64 = conn
            .query_row("SELECT id FROM volumes WHERE label = 'Q-QUICK'", [], |r| {
                r.get(0)
            })
            .unwrap();

        let cleared = clear_condition_on_clean_full_verify(
            &conn,
            volume_id,
            "Q-QUICK",
            Tier::Navigable,
            &crate::store::Evidence {
                tier: Tier::Navigable,
                files_checked: 1,
                mismatches: vec![],
            },
        )
        .unwrap();

        assert!(
            cleared.is_none(),
            "a quick verify proves nothing about the medium"
        );
        assert_eq!(volume_condition(&conn, "Q-QUICK"), "quarantined");
    }

    /// The other three medium-proving kinds reach the status write through
    /// the same seam, and the two non-proving ones do not. Asserted against
    /// a REAL row each time rather than against the predicate, so a wiring
    /// mistake (say, filtering on the wrong side) cannot pass.
    #[test]
    fn the_seam_quarantines_exactly_the_medium_proving_kinds() {
        use crate::store::MismatchKind::*;
        for (kind, expect_quarantine) in [
            (ContentHashMismatch, true),
            (FrontIndexDivergesFromSeal, true),
            (FrontIndexInconsistent, true),
            (NavigationDisagreement, true),
            // Issue #239: the kind that used to be folded into
            // `ContentHashMismatch` and so reached the condition write.
            (ContentUnreadable, false),
            (FrontIndexUnreadable, false),
            (SealUnreadable, false),
        ] {
            let conn = crate::db::open_memory().unwrap();
            let good = b"fixture bytes. ".repeat(4);
            let label = "Q-SEAM";
            seed_one_slice_fixture(
                &conn,
                label,
                "q-seam-unit",
                4,
                &good,
                "completed",
                "current",
            );
            conn.execute(
                "UPDATE volumes SET status = 'sealed' WHERE label = ?1",
                params![label],
            )
            .unwrap();
            let volume_id: i64 = conn
                .query_row(
                    "SELECT id FROM volumes WHERE label = ?1",
                    params![label],
                    |r| r.get(0),
                )
                .unwrap();

            quarantine_on_medium_evidence(&conn, volume_id, label, &evidence_of(kind)).unwrap();
            // Issue #242: `status` never moves here, regardless of kind --
            // only `observed_condition` can.
            assert_eq!(
                volume_status(&conn, label),
                "sealed",
                "{}: a verify must never move volumes.status",
                kind.label()
            );
            let expected_condition = if expect_quarantine {
                "quarantined"
            } else {
                "ok"
            };
            assert_eq!(
                volume_condition(&conn, label),
                expected_condition,
                "{} must leave the volume's condition {expected_condition}",
                kind.label()
            );
        }
    }

    /// A `MemStore` whose read fails at exactly one position — the issue
    /// #239 drive: it reads File 0, the seal marker and the front index
    /// cleanly, then faults partway through a data slice.
    ///
    /// Deliberately a `Store` wrapper rather than a `MemStore` flag: the
    /// fault has to be seen by the DEFAULT `confirm` (the real `chain_walk`),
    /// which reaches the medium only through `read_file`.
    struct ReadFaultStore {
        inner: MemStore,
        fault_at: u32,
    }

    impl Store for ReadFaultStore {
        fn capacity(&mut self) -> Result<crate::store::CapacityReport> {
            self.inner.capacity()
        }
        fn execute(&mut self, src: &mut dyn std::io::Read, len: u64, sync: bool) -> Result<u64> {
            self.inner.execute(src, len, sync)
        }
        fn read_file(&mut self, position: u32, sink: &mut dyn std::io::Write) -> Result<u64> {
            if position == self.fault_at {
                // The shape `TapeDevice::read_file_streaming` produces from a
                // kernel read error (`src/tape/ioctl.rs`).
                return Err(TapectlError::TapeIo(
                    "read: Input/output error (os error 5)".to_string(),
                ));
            }
            self.inner.read_file(position, sink)
        }
        fn reposition_for_resume(&mut self, file_index: u32) -> Result<()> {
            self.inner.reposition_for_resume(file_index)
        }
    }

    /// ISSUE #239. A raw read/transport error at a content position is NOT
    /// evidence about the medium — "we could not read it today" is not "the
    /// bytes are gone" (ADR-0012's 2026-09-17 amendment). A dirty head or a
    /// marginal cable that gets through File 0, the seal and the front index
    /// and then faults on a slice must leave a sound cartridge exactly as it
    /// was, because `quarantined` is what makes a volume stop counting as a
    /// copy.
    ///
    /// Asserted against the `volumes` ROW, not the report: the report is the
    /// thing that would be re-derived, and the row is the fact that silently
    /// takes real coverage to zero.
    #[test]
    fn a_read_error_at_a_content_position_does_not_quarantine() {
        let conn = crate::db::open_memory().unwrap();
        let good = b"the bytes the front index promises. ".repeat(4);
        seed_one_slice_fixture(
            &conn,
            "Q-DIRTY",
            "q-dirty-unit",
            4,
            &good,
            "completed",
            "current",
        );
        conn.execute(
            "UPDATE volumes SET status = 'sealed' WHERE label = 'Q-DIRTY'",
            [],
        )
        .unwrap();
        let volume_id: i64 = conn
            .query_row("SELECT id FROM volumes WHERE label = 'Q-DIRTY'", [], |r| {
                r.get(0)
            })
            .unwrap();

        let mut store = ReadFaultStore {
            inner: mem_store_v2_tape("Q-DIRTY", &good, &good),
            fault_at: 4,
        };
        let report = volume_verify_with_store(
            &conn,
            &mut store,
            "Q-DIRTY",
            volume_id,
            4096,
            Tier::Integrity,
            site(Operation::VolumeVerify),
        )
        .unwrap();

        assert_eq!(report.failed, 1, "mismatches: {:?}", report.mismatches);
        assert_eq!(report.mismatches[0].position, 4);
        assert_eq!(
            volume_status(&conn, "Q-DIRTY"),
            "sealed",
            "a drive fault must leave volumes.status exactly as it was"
        );
        assert_eq!(
            volume_condition(&conn, "Q-DIRTY"),
            "ok",
            "a drive fault must leave volumes.observed_condition exactly as it was"
        );
        assert!(
            !report.mismatches[0].kind.proves_medium_bad(),
            "a read error says nothing about the tape: {:?}",
            report.mismatches[0]
        );
        assert!(
            report.quarantine.is_none(),
            "nothing was quarantined: {:?}",
            report.quarantine
        );
        assert!(
            !volume_events(&conn, "Q-DIRTY")
                .iter()
                .any(|(action, _, _)| action.contains("quarantined")),
            "no quarantine event either: {:?}",
            volume_events(&conn, "Q-DIRTY")
        );

        // Issue #142's columns stop lying as a consequence of the #239
        // split: no hash was compared, so the row is `failed_read` with both
        // sha256 columns NULL — not `failed_checksum` with "read failed: …"
        // sitting in `expected_sha256`, which is what the shared kind
        // produced. Pinned here because it is the same correction on the
        // confirm path, which has no test of its own for this shape.
        let rows = verification_result_rows(&conn);
        assert_eq!(rows.len(), 1, "expected exactly one recorded row: {rows:?}");
        assert_eq!(rows[0].0, "4", "the row must name the failing position");
        assert_eq!(rows[0].1, "failed_read");
        assert!(
            rows[0].2.contains("content_unreadable"),
            "notes must carry the true MismatchKind: {}",
            rows[0].2
        );
        let (expected_sha, actual_sha): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT expected_sha256, actual_sha256 FROM verification_results",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(expected_sha, None, "nothing was hashed, so nothing to say");
        assert_eq!(actual_sha, None, "nothing was hashed, so nothing to say");
    }

    /// ISSUE #239, the short-read half. Fewer bytes came back than the front
    /// index claims. Ambiguous by construction — a truncated write, or a
    /// drive giving up early — and the enum already rules on exactly this
    /// event one position over: `FrontIndexUnreadable`'s arm says "a genuine
    /// I/O or transport error becomes this variant, and so does a short
    /// read; neither distinguishes a bad tape from a dirty drive". Same
    /// event, different position, same epistemics, so the conservative side
    /// applies here too.
    #[test]
    fn a_short_read_at_a_content_position_does_not_quarantine() {
        let conn = crate::db::open_memory().unwrap();
        let good = b"the bytes the front index promises. ".repeat(4);
        seed_one_slice_fixture(
            &conn,
            "Q-SHORT",
            "q-short-unit",
            4,
            &good,
            "completed",
            "current",
        );
        conn.execute(
            "UPDATE volumes SET status = 'sealed' WHERE label = 'Q-SHORT'",
            [],
        )
        .unwrap();
        let volume_id: i64 = conn
            .query_row("SELECT id FROM volumes WHERE label = 'Q-SHORT'", [], |r| {
                r.get(0)
            })
            .unwrap();

        let mut store = mem_store_v2_tape("Q-SHORT", &good, &good);
        // The slice comes back SHORT of the size the front index claims —
        // `chain_walk`'s `want_size > n_read` arm, with the claimed hash
        // left untouched so nothing else can be what failed.
        store.files[4].truncate(good.len() - 1);

        let report = volume_verify_with_store(
            &conn,
            &mut store,
            "Q-SHORT",
            volume_id,
            4096,
            Tier::Integrity,
            site(Operation::VolumeVerify),
        )
        .unwrap();

        assert_eq!(report.failed, 1, "mismatches: {:?}", report.mismatches);
        assert_eq!(report.mismatches[0].position, 4);
        assert_eq!(
            volume_status(&conn, "Q-SHORT"),
            "sealed",
            "a short read must leave volumes.status exactly as it was"
        );
        assert_eq!(
            volume_condition(&conn, "Q-SHORT"),
            "ok",
            "a short read must leave volumes.observed_condition exactly as it was"
        );
        assert!(
            !report.mismatches[0].kind.proves_medium_bad(),
            "a short read says nothing conclusive about the tape: {:?}",
            report.mismatches[0]
        );
        assert!(
            report.quarantine.is_none(),
            "nothing was quarantined: {:?}",
            report.quarantine
        );
    }

    /// A volume that was ALREADY quarantined reports the fact honestly:
    /// there is fresh evidence, but the condition is not new. A report that
    /// said "QUARANTINED (was ok)" here would be lying.
    #[test]
    fn re_verifying_an_already_quarantined_volume_reports_no_condition_change() {
        let conn = crate::db::open_memory().unwrap();
        let good = b"the bytes the front index promises. ".repeat(4);
        let rotted = b"the bytes the tape actually holds!! ".repeat(4);
        seed_one_slice_fixture(
            &conn,
            "Q-AGAIN",
            "q-again-unit",
            4,
            &good,
            "completed",
            "current",
        );
        // Issue #242: an already-quarantined volume stays `sealed` (an
        // operator's status is never overwritten by an observation) with
        // `observed_condition = 'quarantined'` already set.
        conn.execute(
            "UPDATE volumes SET status = 'sealed', observed_condition = 'quarantined' \
             WHERE label = 'Q-AGAIN'",
            [],
        )
        .unwrap();
        let volume_id: i64 = conn
            .query_row("SELECT id FROM volumes WHERE label = 'Q-AGAIN'", [], |r| {
                r.get(0)
            })
            .unwrap();

        let mut store = mem_store_v2_tape("Q-AGAIN", &good, &rotted);
        let report = volume_verify_with_store(
            &conn,
            &mut store,
            "Q-AGAIN",
            volume_id,
            4096,
            Tier::Integrity,
            site(Operation::VolumeVerify),
        )
        .unwrap();

        let effect = report
            .quarantine
            .expect("the evidence is still medium-proving");
        assert_eq!(effect.previous_condition, "quarantined");
        assert!(
            !effect.condition_changed(),
            "the evidence is fresh; the condition is not new"
        );
        assert_eq!(volume_status(&conn, "Q-AGAIN"), "sealed");
        assert_eq!(volume_condition(&conn, "Q-AGAIN"), "quarantined");
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

        let report = read_slices(
            &conn,
            &config,
            "RSLABEL",
            "rs-unit",
            &mut store,
            site(Operation::VolumeReadSlices),
        )
        .unwrap();
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

        let report = compact_read(
            &conn,
            &config,
            "CRLABEL",
            &mut store,
            site(Operation::VolumeCompactRead),
        )
        .unwrap();
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
            // Must be able to write the volume's own LTO-6 media (issue
            // #166: volume_write now checks this before the pre-write
            // validate this test is aimed at) — an unrelated mismatch here
            // would refuse earlier, for the wrong reason.
            generation: "LTO-6".into(),
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

    /// ADR-0012 (issue #161): `volume write` must refuse every status other
    /// than `initialized` BEFORE it touches anything else -- no session
    /// check, no staged-data lookup, no MAM read/update, no binding, and
    /// (like the escrow model test above) no device contact. Modelled
    /// directly on `force_does_not_bypass_the_escrow_check_and_never_reaches_the_device`:
    /// a nonexistent device path, so reaching it at all is itself the
    /// failure this test looks for.
    ///
    /// `quarantined` is deliberately absent from this array (issue #242): it
    /// left the `status` CHECK entirely when it became a value of
    /// `observed_condition` instead. See
    /// `volume_write_refuses_an_initialized_volume_with_a_quarantined_condition`
    /// for that dimension's own regression test.
    #[test]
    fn volume_write_refuses_every_non_initialized_status_before_touching_the_device() {
        let statuses = [
            "sealed", "retired", "erased", "active", "full", "blank", "missing",
        ];
        for status in statuses {
            let conn = crate::db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                      capacity_bytes, status, mam_capacity_bytes)
                 VALUES ('L6-STATUS', 'lto', 'lto0', 'LTO-6', 2500000000000, ?1, 123456)",
                params![status],
            )
            .unwrap();
            let volume_id = conn.last_insert_rowid();

            let mam_before: Option<i64> = conn
                .query_row(
                    "SELECT mam_capacity_bytes FROM volumes WHERE id = ?1",
                    params![volume_id],
                    |r| r.get(0),
                )
                .unwrap();
            let cartridge_volumes_before: i64 = conn
                .query_row("SELECT COUNT(*) FROM cartridge_volumes", [], |r| r.get(0))
                .unwrap();

            let tmp = tempfile::TempDir::new().unwrap();
            let paths = TapectlPaths::new(tmp.path().join("home"));
            let config = Config::default();

            let err = volume_write(
                &conn,
                &paths,
                &config,
                "L6-STATUS",
                "/nonexistent/tapectl-write-target-test-nst",
                512 * 1024,
                false, // force
                false, // allow_missing_escrow
            )
            .unwrap_err();

            match &err {
                TapectlError::VolumeNotWriteTarget {
                    label,
                    status: got_status,
                } => {
                    assert_eq!(label, "L6-STATUS", "status {status}");
                    assert_eq!(got_status, status, "status {status}");
                }
                other => panic!("status {status}: expected VolumeNotWriteTarget, got: {other:?}"),
            }
            let msg = err.to_string();
            assert!(
                msg.contains("L6-STATUS"),
                "status {status}: message must name the label: {msg}"
            );
            assert!(
                msg.contains(status),
                "status {status}: message must name the status: {msg}"
            );
            assert!(
                msg.contains("ADR-0012"),
                "status {status}: message must cite ADR-0012: {msg}"
            );
            assert!(
                !msg.contains("tapectl-write-target-test-nst"),
                "status {status}: the device path must never be reached: {msg}"
            );

            let writes: i64 = conn
                .query_row("SELECT COUNT(*) FROM writes", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                writes, 0,
                "status {status}: a refused write must plan nothing"
            );

            let mam_after: Option<i64> = conn
                .query_row(
                    "SELECT mam_capacity_bytes FROM volumes WHERE id = ?1",
                    params![volume_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                mam_after, mam_before,
                "status {status}: mam_capacity_bytes must be untouched"
            );

            let cartridge_volumes_after: i64 = conn
                .query_row("SELECT COUNT(*) FROM cartridge_volumes", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                cartridge_volumes_after, cartridge_volumes_before,
                "status {status}: cartridge_volumes must be untouched"
            );
        }
    }

    /// THE negative control for issue #242's `is_write_target` regression
    /// (ADR-0012's 2026-09-17 amendment, point 5, "is_write_target is the
    /// trap"), driven through the real `volume_write` entry point rather
    /// than the bare predicate. Modelled on the loop test just above: a
    /// nonexistent device path, so reaching it at all is itself the
    /// failure this test looks for.
    ///
    /// Before the fix landed, this reached the device (or some later
    /// check) instead of refusing here, because `status = 'initialized'`
    /// alone used to be sufficient -- a write-path quarantine used to move
    /// `status` OFF `initialized` as a side effect of recording the
    /// quarantine, and once that side effect stopped happening (this
    /// issue's own fix), an `initialized`-but-quarantined volume would
    /// silently become writable again without this test.
    #[test]
    fn volume_write_refuses_an_initialized_volume_with_a_quarantined_condition() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status, observed_condition)
             VALUES ('L6-QCOND', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized', \
             'quarantined')",
            [],
        )
        .unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().join("home"));
        let config = Config::default();

        let err = volume_write(
            &conn,
            &paths,
            &config,
            "L6-QCOND",
            "/nonexistent/tapectl-quarantined-condition-test-nst",
            512 * 1024,
            false, // force
            false, // allow_missing_escrow
        )
        .unwrap_err();

        match &err {
            TapectlError::VolumeQuarantined { label } => {
                assert_eq!(label, "L6-QCOND");
            }
            other => panic!("expected VolumeQuarantined, got: {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("L6-QCOND"),
            "message must name the label: {msg}"
        );
        assert!(
            msg.contains("ADR-0012"),
            "message must cite ADR-0012: {msg}"
        );
        assert!(
            !msg.contains("tapectl-quarantined-condition-test-nst"),
            "the device path must never be reached: {msg}"
        );

        let writes: i64 = conn
            .query_row("SELECT COUNT(*) FROM writes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(writes, 0, "a refused write must plan nothing");
    }

    /// ADR-0012 amendment, issue #199: the rebuild scenario the issue was
    /// filed about. `catalog rebuild --from-volume` can attach a rebuilt
    /// row's contents to a pre-existing `initialized` volume without ever
    /// moving its status (#158 deliberately leaves an existing row's
    /// status alone) -- `rebuild::ensure_write` always inserts its
    /// `writes` row as `status = 'completed'`, which is reproduced here
    /// directly rather than by running a real rebuild. `volume write` must
    /// refuse this BEFORE touching anything else, exactly like every
    /// non-`initialized` status in the loop above: no staged-data lookup,
    /// no MAM read/update, no binding, no device contact, and (the whole
    /// point of #161's ordering) no new `writes` row.
    #[test]
    fn volume_write_refuses_an_initialized_volume_with_a_completed_write_before_touching_the_device(
    ) {
        let (conn, _tenant_id, stage_set_id) = escrow_check_fixture();
        let snapshot_id: i64 = conn
            .query_row(
                "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status, mam_capacity_bytes)
             VALUES ('L6-REBUILT', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized', 123456)",
            [],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, write_verified,
                                 completed_at, notes)
             VALUES (?1, ?2, ?3, 'completed', 0, datetime('now'),
                     'rebuilt from the volume itself; never verified by a read-back')",
            params![stage_set_id, snapshot_id, volume_id],
        )
        .unwrap();

        let mam_before: Option<i64> = conn
            .query_row(
                "SELECT mam_capacity_bytes FROM volumes WHERE id = ?1",
                params![volume_id],
                |r| r.get(0),
            )
            .unwrap();
        let cartridge_volumes_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM cartridge_volumes", [], |r| r.get(0))
            .unwrap();
        let writes_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM writes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(writes_before, 1, "the rebuilt row itself, seeded above");

        let tmp = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().join("home"));
        let config = Config::default();

        let err = volume_write(
            &conn,
            &paths,
            &config,
            "L6-REBUILT",
            "/nonexistent/tapectl-rebuilt-write-target-test-nst",
            512 * 1024,
            false, // force
            false, // allow_missing_escrow
        )
        .unwrap_err();

        match &err {
            TapectlError::VolumeHasRecordedWrite { label } => {
                assert_eq!(label, "L6-REBUILT");
            }
            other => panic!("expected VolumeHasRecordedWrite, got: {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("L6-REBUILT"),
            "message must name the label: {msg}"
        );
        assert!(
            msg.contains("ADR-0012"),
            "message must cite ADR-0012: {msg}"
        );
        assert!(
            !msg.contains("tapectl-rebuilt-write-target-test-nst"),
            "the device path must never be reached: {msg}"
        );

        let mam_after: Option<i64> = conn
            .query_row(
                "SELECT mam_capacity_bytes FROM volumes WHERE id = ?1",
                params![volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            mam_after, mam_before,
            "mam_capacity_bytes must be untouched"
        );

        let cartridge_volumes_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM cartridge_volumes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            cartridge_volumes_after, cartridge_volumes_before,
            "cartridge_volumes must be untouched"
        );

        let writes_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM writes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            writes_after, writes_before,
            "a refused write must plan nothing -- no new `writes` row"
        );
    }

    /// ADR-0012 amendment, issue #199: the same rebuild scenario, but for
    /// `volume resume` -- an `initialized` row with a completed write
    /// attached must be named by that fact, not fall through to
    /// `nothing_to_resume`'s message (which would be confusing here: the
    /// completed row means there is nothing UNRESOLVED to resume, but the
    /// volume still is not writable).
    #[test]
    fn volume_resume_refuses_an_initialized_volume_with_a_completed_write() {
        let (conn, _tenant_id, stage_set_id) = escrow_check_fixture();
        let snapshot_id: i64 = conn
            .query_row(
                "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES ('L6-REBUILT-R', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
            [],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, write_verified,
                                 completed_at, notes)
             VALUES (?1, ?2, ?3, 'completed', 0, datetime('now'),
                     'rebuilt from the volume itself; never verified by a read-back')",
            params![stage_set_id, snapshot_id, volume_id],
        )
        .unwrap();
        let write_id = conn.last_insert_rowid();

        let tmp = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().join("home"));
        let config = Config::default();

        let err = volume_resume(
            &conn,
            &paths,
            &config,
            "L6-REBUILT-R",
            "/nonexistent/tapectl-resume-rebuilt-test-nst",
            512 * 1024,
        )
        .unwrap_err();

        match &err {
            TapectlError::VolumeHasRecordedWrite { label } => {
                assert_eq!(label, "L6-REBUILT-R");
            }
            other => panic!("expected VolumeHasRecordedWrite, got: {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            !msg.contains("nothing to resume"),
            "must be refused by the recorded-write fact, not fall through to \
             nothing_to_resume: {msg}"
        );
        assert!(
            msg.contains("ADR-0012"),
            "message must cite ADR-0012: {msg}"
        );

        let write_status: String = conn
            .query_row(
                "SELECT status FROM writes WHERE id = ?1",
                params![write_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            write_status, "completed",
            "a refused resume must leave the writes row untouched"
        );
    }

    /// ADR-0012 (issue #161): an `initialized` volume must NOT be refused
    /// by status -- the whitelist admits it, and the call must reach a
    /// LATER, unrelated check (here, no staged data exists at all).
    #[test]
    fn volume_write_accepts_initialized_and_proceeds_to_the_next_check() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES ('L6-INIT', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
            [],
        )
        .unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().join("home"));
        let config = Config::default();

        let err = volume_write(
            &conn,
            &paths,
            &config,
            "L6-INIT",
            "/nonexistent/tapectl-init-proceeds-test-nst",
            512 * 1024,
            false,
            false,
        )
        .expect_err("no staged data exists, so this must fail at a LATER check");

        assert!(
            !matches!(err, TapectlError::VolumeNotWriteTarget { .. }),
            "an initialized volume must not be refused by status: {err}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("no staged data to write"),
            "expected the existing no-staged-data refusal, got: {msg}"
        );
    }

    // ── issue #201: `volume write` announces the staged sets it is about
    // to write ──

    /// Exact text, multi-unit multi-version — issue #201's first acceptance
    /// property. Pinned, not `contains`-checked: a silently reworded
    /// announcement is exactly the kind of drift a substring match would
    /// miss. Matches `volume plan`'s row shape (`src/cli/volume.rs`'s `Plan`
    /// arm) verbatim, minus the "x {copies}" term that does not apply to a
    /// single physical write.
    #[test]
    fn render_staged_selection_pins_exact_text_for_multi_unit_multi_version() {
        let units = vec![
            BuildUnit {
                stage_set_id: 1,
                snapshot_id: 1,
                unit_name: "alpha-collection".to_string(),
                unit_uuid: "u1".to_string(),
                tenant_id: 1,
                dar_version: None,
                dar_command: None,
                catalog_path: None,
                snapshot_version: 3,
                slices: vec![
                    BuildSlice {
                        slice_id: 1,
                        slice_number: 1,
                        size_bytes: 50 * 1024 * 1024,
                        encrypted_bytes: 50 * 1024 * 1024,
                        sha256_plain: "a".to_string(),
                        sha256_encrypted: "b".to_string(),
                        staging_path: PathBuf::from("/tmp/a1"),
                    },
                    BuildSlice {
                        slice_id: 2,
                        slice_number: 2,
                        size_bytes: 30 * 1024 * 1024,
                        encrypted_bytes: 30 * 1024 * 1024,
                        sha256_plain: "c".to_string(),
                        sha256_encrypted: "d".to_string(),
                        staging_path: PathBuf::from("/tmp/a2"),
                    },
                ],
            },
            BuildUnit {
                stage_set_id: 2,
                snapshot_id: 2,
                unit_name: "zeta-notes".to_string(),
                unit_uuid: "u2".to_string(),
                tenant_id: 1,
                dar_version: None,
                dar_command: None,
                catalog_path: None,
                snapshot_version: 1,
                slices: vec![BuildSlice {
                    slice_id: 3,
                    slice_number: 1,
                    size_bytes: 5 * 1024 * 1024,
                    encrypted_bytes: 5 * 1024 * 1024,
                    sha256_plain: "e".to_string(),
                    sha256_encrypted: "f".to_string(),
                    staging_path: PathBuf::from("/tmp/z1"),
                }],
            },
        ];

        let rendered = render_staged_selection("VOL-F", &units);
        assert_eq!(
            rendered,
            "about to write to volume \"VOL-F\":\n\
             \x20\x20alpha-collection v3: 2 slices, 80.0 MiB\n\
             \x20\x20zeta-notes v1: 1 slices, 5.0 MiB\n\
             \n\
             total: 3 slices, 85.0 MiB\n"
        );
    }

    /// Single-unit selection — issue #201's second acceptance property: no
    /// plural/grammar bug. The header names the volume, never a unit count,
    /// so there is no "1 units" to get wrong; the per-row and total lines
    /// keep `volume plan`'s own "N slices" wording unconditionally (Plan
    /// does not pluralize for N == 1 either — `src/cli/volume.rs`'s `Plan`
    /// arm prints "1 slices" the same way), so a single unit with a single
    /// slice is not special-cased into a second vocabulary.
    #[test]
    fn render_staged_selection_reads_sensibly_for_a_single_unit() {
        let units = vec![BuildUnit {
            stage_set_id: 1,
            snapshot_id: 1,
            unit_name: "solo".to_string(),
            unit_uuid: "u1".to_string(),
            tenant_id: 1,
            dar_version: None,
            dar_command: None,
            catalog_path: None,
            snapshot_version: 1,
            slices: vec![BuildSlice {
                slice_id: 1,
                slice_number: 1,
                size_bytes: 1024 * 1024,
                encrypted_bytes: 1024 * 1024,
                sha256_plain: "a".to_string(),
                sha256_encrypted: "b".to_string(),
                staging_path: PathBuf::from("/tmp/solo1"),
            }],
        }];

        let rendered = render_staged_selection("VOL-SOLO", &units);
        assert_eq!(
            rendered,
            "about to write to volume \"VOL-SOLO\":\n\
             \x20\x20solo v1: 1 slices, 1.0 MiB\n\
             \n\
             total: 1 slices, 1.0 MiB\n"
        );
        assert!(
            !rendered.contains("1 units"),
            "header must never carry a unit-count phrase to get wrong: {rendered}"
        );
    }

    /// Ordering proof (issue #201), the closest this suite can get without a
    /// drive: `volume_write` has no store injection (`TapeStore::open` runs
    /// unconditionally later on), so the full function cannot be exercised
    /// end-to-end here, and capturing this process's own stderr to observe
    /// `eprint!` output would need OS-level fd redirection this suite does
    /// not otherwise use. What this test DOES prove: given a non-empty
    /// staged selection, `volume_write` does not error at (or before) the
    /// announcement call site — it proceeds past `find_staged_data` and
    /// `announce_staged_selection` and fails at the NEXT step,
    /// `resolve_lto_backend` (issue #201's call site sits between those two,
    /// see the comment above `announce_staged_selection`'s call in
    /// `volume_write`). Modelled on
    /// `volume_write_accepts_initialized_and_proceeds_to_the_next_check`
    /// just above, which proves the analogous thing one check earlier.
    #[test]
    fn volume_write_with_staged_data_passes_the_announcement_and_reaches_backend_resolution() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES ('L6-ANNOUNCE', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t1', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('announce-unit', 'announce-unit', ?1, 'mtime_size', 1, 'active')",
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
             VALUES (?1, 1, 10, 10, 'a', 'b', '/tmp/x')",
            params![ss_id],
        )
        .unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().join("home"));
        // No `[[backends.lto]]` configured, so `resolve_lto_backend` is the
        // very next thing that can fail — and it fails without ever naming
        // or touching a device, exactly like the fast-refusal tests above.
        let config = Config::default();

        let err = volume_write(
            &conn,
            &paths,
            &config,
            "L6-ANNOUNCE",
            "/nonexistent/tapectl-announce-test-nst",
            512 * 1024,
            false,
            false,
        )
        .expect_err("no backend is configured, so this must fail at backend resolution");

        assert!(
            matches!(err, TapectlError::Config(_)),
            "expected a Config error from resolve_lto_backend, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("no [[backends.lto]] entry"),
            "expected resolve_lto_backend's message, got: {msg}"
        );
        assert!(
            !msg.contains("no staged data"),
            "staged data was present, so the earlier refusal must not fire: {msg}"
        );

        let writes: i64 = conn
            .query_row("SELECT COUNT(*) FROM writes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            writes, 0,
            "a write refused this early must still plan nothing"
        );
    }

    /// ADR-0012 (issue #161, amended 2026-09-17 for issue #242): `volume
    /// resume` must refuse a quarantined volume BEFORE `rehydrate` -- named
    /// by its condition, not answered with `nothing_to_resume`'s message
    /// about `writes` rows, and the seeded `interrupted` row must be left
    /// alone (a refused resume attempts nothing).
    ///
    /// The fixture's `status` stays `initialized` deliberately: a
    /// write-path quarantine fires only on a volume that never sealed, and
    /// since issue #242 it moves `observed_condition`, never `status` --
    /// this is the realistic shape a crashed, quarantined-mid-write session
    /// leaves behind.
    #[test]
    fn volume_resume_refuses_by_status_not_by_nothing_to_resume() {
        let (conn, _tenant_id, stage_set_id) = escrow_check_fixture();
        let snapshot_id: i64 = conn
            .query_row(
                "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status, observed_condition)
             VALUES ('L6-QUAR', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized', \
             'quarantined')",
            [],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'interrupted')",
            params![stage_set_id, snapshot_id, volume_id],
        )
        .unwrap();
        let write_id = conn.last_insert_rowid();

        let tmp = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().join("home"));
        let config = Config::default();

        let err = volume_resume(
            &conn,
            &paths,
            &config,
            "L6-QUAR",
            "/nonexistent/tapectl-resume-status-test-nst",
            512 * 1024,
        )
        .unwrap_err();

        match &err {
            TapectlError::VolumeQuarantined { label } => {
                assert_eq!(label, "L6-QUAR");
            }
            other => panic!("expected VolumeQuarantined, got: {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            !msg.contains("nothing to resume"),
            "must be refused by condition, not fall through to nothing_to_resume: {msg}"
        );
        assert!(
            msg.contains("ADR-0012"),
            "message must cite ADR-0012: {msg}"
        );

        let write_status: String = conn
            .query_row(
                "SELECT status FROM writes WHERE id = ?1",
                params![write_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            write_status, "interrupted",
            "a refused resume must leave the writes row untouched"
        );
    }

    // ---- issue #199: the resumable states must keep working ----
    //
    // ADR-0012's dated amendment ("The write target") warns that the
    // attached-rows check this issue adds must NOT disqualify a volume
    // whose `writes` rows are `planned`/`in_progress`/`interrupted` --
    // that is exactly the set `volume resume` exists to continue. Each
    // state gets its own test, per the ruling's own instruction ("it wants
    // a test per state rather than one aggregate test"), asserting the
    // SPECIFIC downstream behavior for that state rather than merely "not
    // VolumeNotWriteTarget" -- a bare negative could also pass because of
    // an unrelated early failure.
    //
    // Run on UNMODIFIED code first (before #199's fix lands) to prove these
    // three states already reach past the status guard today; they must
    // still pass unchanged once the attached-rows check is added.

    /// A `planned` row (layout validated, nothing on tape yet) must not be
    /// treated as "this volume already holds bytes" -- `nothing_to_resume`
    /// names it by its own specific message ("not an interrupted one"),
    /// which only fires if `is_write_target`'s guard let the call through.
    #[test]
    fn volume_resume_still_targets_a_volume_with_a_planned_write() {
        let (conn, _tenant_id, stage_set_id) = escrow_check_fixture();
        let snapshot_id: i64 = conn
            .query_row(
                "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES ('L6-PLANNED', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
            [],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'planned')",
            params![stage_set_id, snapshot_id, volume_id],
        )
        .unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().join("home"));
        let config = Config::default();

        let err = volume_resume(
            &conn,
            &paths,
            &config,
            "L6-PLANNED",
            "/nonexistent/tapectl-resume-planned-test-nst",
            512 * 1024,
        )
        .unwrap_err();

        assert!(
            !matches!(err, TapectlError::VolumeNotWriteTarget { .. }),
            "a `planned` row must not be refused as a non-write-target: {err}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("planned") && msg.contains("not an interrupted one"),
            "must reach nothing_to_resume's planned-specific message, proving it passed \
             the write-target guard: {msg}"
        );
    }

    /// An `in_progress` row means a live writer in another process
    /// (`db::open`'s startup sweep is what would otherwise convert it to
    /// `interrupted`; this test uses `open_memory`, which does not sweep,
    /// so the row stays `in_progress` on purpose). It must still reach
    /// `nothing_to_resume`'s own refusal for that state, not be caught by
    /// the write-target guard.
    #[test]
    fn volume_resume_still_targets_a_volume_with_an_in_progress_write() {
        let (conn, _tenant_id, stage_set_id) = escrow_check_fixture();
        let snapshot_id: i64 = conn
            .query_row(
                "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES ('L6-INPROG', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
            [],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'in_progress')",
            params![stage_set_id, snapshot_id, volume_id],
        )
        .unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().join("home"));
        let config = Config::default();

        let err = volume_resume(
            &conn,
            &paths,
            &config,
            "L6-INPROG",
            "/nonexistent/tapectl-resume-inprogress-test-nst",
            512 * 1024,
        )
        .unwrap_err();

        assert!(
            !matches!(err, TapectlError::VolumeNotWriteTarget { .. }),
            "an `in_progress` row must not be refused as a non-write-target: {err}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("ANOTHER PROCESS IS WRITING THIS TAPE RIGHT NOW"),
            "must reach nothing_to_resume's in_progress-specific message, proving it \
             passed the write-target guard: {msg}"
        );
    }

    /// An `interrupted` row is the one `rehydrate` actually adopts. Without
    /// a real frozen session directory on disk this cannot rehydrate all
    /// the way through, but it must get FAR ENOUGH to fail on the missing
    /// `session_dir` (a `rehydrate`-internal error), not be turned away at
    /// the write-target guard.
    #[test]
    fn volume_resume_still_targets_a_volume_with_an_interrupted_write() {
        let (conn, _tenant_id, stage_set_id) = escrow_check_fixture();
        let snapshot_id: i64 = conn
            .query_row(
                "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES ('L6-INTR', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
            [],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();
        // `session_dir` left NULL on purpose: `rehydrate` treats that as
        // "predates migration 006 / cannot be resumed", which is a
        // downstream failure distinct from the write-target guard -- the
        // point of this test is which check reaches it first.
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'interrupted')",
            params![stage_set_id, snapshot_id, volume_id],
        )
        .unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().join("home"));
        let config = Config::default();

        let err = volume_resume(
            &conn,
            &paths,
            &config,
            "L6-INTR",
            "/nonexistent/tapectl-resume-interrupted-test-nst",
            512 * 1024,
        )
        .unwrap_err();

        assert!(
            !matches!(err, TapectlError::VolumeNotWriteTarget { .. }),
            "an `interrupted` row must not be refused as a non-write-target: {err}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("no recorded session directory"),
            "must reach rehydrate's own missing-session_dir failure, proving it passed \
             the write-target guard: {msg}"
        );
    }

    // ---- issue #166: the drive/medium refusal at every write contact ----

    /// `volume_write` never re-checked the drive against the medium after
    /// `volume_init` — a volume initialised on one drive could be written
    /// from a different one, failing loudly on the drive's first physical
    /// write instead of refusing here, free, before the store is ever
    /// opened.
    ///
    /// The backend's generation (LTO-8) cannot write the volume's recorded
    /// generation (LTO-6); the device is nonexistent, so `detect` finds
    /// nothing and the check falls back to the volume's own row — the same
    /// cannot-see-cannot-refuse rule `check_loaded_generation` already
    /// follows. `force` changes nothing: `check_drive_can_write` has no
    /// `force` parameter at all (ADR-0010 decision 2, ADR-0008 Tier 3). And
    /// the refusal fires before `bind_late`, before the session directory,
    /// before `build()` — nothing progresses at all (issue #154's
    /// ordering).
    #[test]
    fn volume_write_refuses_a_drive_that_cannot_write_the_recorded_generation_before_bind_late() {
        let (conn, _tenant_id, stage_set_id) = escrow_check_fixture();
        conn.execute(
            "INSERT INTO stage_slices
                (stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain,
                 sha256_encrypted, staging_path)
             VALUES (?1, 1, 10, 10, 'p', 'e', '/nonexistent/tapectl-gencheck-slice.dar.age')",
            params![stage_set_id],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES ('L6-GENCHK', 'lto', 'lto8', 'LTO-6', 2500000000000, 'initialized')",
            [],
        )
        .unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.staging.directory = tmp.path().join("staging").to_string_lossy().into_owned();
        config.backends.lto.push(crate::config::LtoBackendConfig {
            name: "lto8".into(),
            device_tape: "/nonexistent/tapectl-gencheck-nst".into(),
            device_sg: "/nonexistent/tapectl-gencheck-sg".into(),
            generation: "LTO-8".into(),
            capacity_override: None,
            usable_capacity_factor: 1.0,
            enospc_buffer: "0".into(),
        });
        let paths = TapectlPaths::new(tmp.path().join("home"));

        for force in [false, true] {
            let err = volume_write(
                &conn,
                &paths,
                &config,
                "L6-GENCHK",
                "/nonexistent/tapectl-gencheck-nst",
                512 * 1024,
                force,
                false,
            )
            .unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("cannot write LTO-6"), "{msg}");
            assert!(msg.contains("physical"), "{msg}");
            assert!(msg.contains("--force does not override it"), "{msg}");
            assert!(msg.contains("[[backends.lto]]"), "{msg}");
        }

        // Ordering (issue #154): the refusal ran before `bind_late`, before
        // the session directory / `build()` — nothing progressed at all.
        let writes: i64 = conn
            .query_row("SELECT COUNT(*) FROM writes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            writes, 0,
            "no write session may exist after a refused write"
        );
        let bindings: i64 = conn
            .query_row("SELECT COUNT(*) FROM cartridge_volumes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(bindings, 0, "a refused write must displace nothing");
        assert!(
            !tmp.path().join("staging").join("sessions").exists(),
            "the session directory must never be created for a refused write"
        );
    }

    /// `volume_resume` never checked the drive against the medium at all.
    /// Same refusal as `volume_write`'s test above, reached via a manually
    /// assembled interrupted session: `InterruptedSession::rehydrate` only
    /// reads the `writes`/`write_positions` rows and the frozen
    /// `layout.json` sidecar, never re-derives a Layout, so a hand-built
    /// `Layout` (mirroring
    /// `stage_set_ids_for_layout_maps_slice_entries_back_to_their_stage_sets`'s
    /// fixture) is enough to reach `volume_resume`'s new check without a
    /// real prior write.
    #[test]
    fn volume_resume_refuses_a_drive_that_cannot_write_the_recorded_generation() {
        let (conn, tenant_id, stage_set_id) = escrow_check_fixture();

        conn.execute(
            "INSERT INTO stage_slices
                (stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain,
                 sha256_encrypted, staging_path)
             VALUES (?1, 1, 10, 10, 'p', 'e',
                     '/nonexistent/tapectl-resume-gencheck-slice.dar.age')",
            params![stage_set_id],
        )
        .unwrap();
        let slice_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES ('L6-RESUMEGEN', 'lto', 'lto8', 'LTO-6', 2500000000000, 'initialized')",
            [],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();

        let session_dir = tempfile::TempDir::new().unwrap();
        let layout = Layout {
            label: "L6-RESUMEGEN".into(),
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
                    position: 1,
                    kind: ZoneKind::TenantEnvelope { tenant_id },
                    size_bytes: Some(10),
                    sha256: None,
                    source: ContentSource::Generated,
                },
                LayoutEntry {
                    position: 4,
                    kind: ZoneKind::Slice {
                        stage_slice_id: slice_id,
                    },
                    size_bytes: Some(10),
                    sha256: Some("e".into()),
                    source: ContentSource::Generated,
                },
            ],
        };
        let json = serde_json::to_vec_pretty(&layout).unwrap();
        std::fs::write(
            session_dir
                .path()
                .join(crate::volume::build::LAYOUT_SIDECAR),
            json,
        )
        .unwrap();

        let snapshot_id: i64 = conn
            .query_row(
                "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, session_dir)
             VALUES (?1, ?2, ?3, 'interrupted', ?4)",
            params![
                stage_set_id,
                snapshot_id,
                volume_id,
                session_dir.path().to_string_lossy(),
            ],
        )
        .unwrap();
        let write_id = conn.last_insert_rowid();

        let tmp = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().join("home"));
        let mut config = Config::default();
        config.backends.lto.push(crate::config::LtoBackendConfig {
            name: "lto8".into(),
            device_tape: "/nonexistent/tapectl-resume-gencheck-nst".into(),
            device_sg: "/nonexistent/tapectl-resume-gencheck-sg".into(),
            generation: "LTO-8".into(),
            capacity_override: None,
            usable_capacity_factor: 1.0,
            enospc_buffer: "0".into(),
        });

        let err = volume_resume(
            &conn,
            &paths,
            &config,
            "L6-RESUMEGEN",
            "/nonexistent/tapectl-resume-gencheck-nst",
            512 * 1024,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cannot write LTO-6"), "{msg}");
        assert!(msg.contains("physical"), "{msg}");

        // Nothing progressed: the interrupted session is untouched (never
        // even reached `TapeStore::open`, let alone `session.resume`).
        let write_status: String = conn
            .query_row(
                "SELECT status FROM writes WHERE id = ?1",
                params![write_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(write_status, "interrupted");
    }

    /// ADR-0012 / issue #161, fix item 3: the status guard runs BEFORE the
    /// unresolved-write-session check, and this pins that ORDER rather than
    /// merely the guard's existence.
    ///
    /// The item-7 loop above cannot pin it: none of its fixtures seed a
    /// `writes` row, so `find_staged_data`'s "no staged data" refusal would
    /// have fired anyway whichever order the two checks ran in. Here the
    /// volume is BOTH non-initialized AND carries an `interrupted` session,
    /// so exactly one of the two refusals can win and the winner names the
    /// order. Guard-second would answer "already has an unresolved write
    /// session" — true, but not the truer fact: a sealed volume is not a
    /// write target at all, and telling the operator to `volume resume` it
    /// would point them at a dead end (ADR-0003: never written again).
    #[test]
    fn volume_write_refuses_by_status_before_the_unresolved_session_check() {
        let (conn, _tenant_id, stage_set_id) = escrow_check_fixture();
        let snapshot_id: i64 = conn
            .query_row(
                "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES ('L6-BOTH', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
            [],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();

        // `planned`/`in_progress`/`interrupted` are what the session check
        // counts as unresolved (see `volume_write`); `interrupted` is the one
        // an operator would actually meet.
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'interrupted')",
            params![stage_set_id, snapshot_id, volume_id],
        )
        .unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().join("home"));
        let config = Config::default();

        let err = volume_write(
            &conn,
            &paths,
            &config,
            "L6-BOTH",
            "/nonexistent/tapectl-order-test-nst",
            512 * 1024,
            false,
            false,
        )
        .unwrap_err();

        assert!(
            matches!(err, TapectlError::VolumeNotWriteTarget { ref status, .. } if status == "sealed"),
            "the status guard must win over the unresolved-session check, got: {err}"
        );
        let msg = format!("{err}");
        assert!(
            !msg.contains("unresolved write session"),
            "guard ran second — the session check answered first: {msg}"
        );
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

        fn call(conn: &Connection, vol_id: i64, det: &Detected) -> Result<Option<i64>> {
            bind_late(conn, vol_id, "L6-0001", det, Some("LTO-6"), "LTO-6")
        }

        fn cartridge_id_for_barcode(conn: &Connection, barcode: &str) -> i64 {
            conn.query_row(
                "SELECT id FROM cartridges WHERE barcode = ?1",
                params![barcode],
                |r| r.get(0),
            )
            .unwrap()
        }

        /// The headline: a serial readable now auto-registers a cartridge
        /// whose barcode IS that serial, exactly as init's ladder does.
        #[test]
        fn a_readable_serial_binds_a_volume_init_left_unbound() {
            let (conn, vol_id) = unbound_volume();
            let bound = call(&conn, vol_id, &det_with_serial(Some("SER-1"))).unwrap();
            assert_eq!(bound_barcode(&conn, vol_id).as_deref(), Some("SER-1"));
            // Issue #296: the CHIP named this one, so the contact may record
            // it. `cartridge_id_for_barcode` re-reads the row rather than
            // trusting the return, so this compares two independent answers.
            assert_eq!(
                bound,
                Some(cartridge_id_for_barcode(&conn, "SER-1")),
                "an auto-registration from the medium's own serial IS a chip identity"
            );
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
            let bound = call(&conn, vol_id, &det_with_serial(Some("SER-1"))).unwrap();
            assert_eq!(bound_barcode(&conn, vol_id).as_deref(), Some("A001L6"));
            assert_eq!(bound, Some(cartridge_id_for_barcode(&conn, "A001L6")));
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
            let bound = call(&conn, vol_id, &det_with_serial(None)).unwrap();
            assert_eq!(bound_barcode(&conn, vol_id), None);
            assert_eq!(bound, None, "no serial named nothing, so nothing to record");
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

            let bound = call(&conn, vol_id, &det_with_serial(Some("SER-1"))).unwrap();
            assert_eq!(bound_barcode(&conn, vol_id).as_deref(), Some("A001L6"));
            assert_eq!(
                bound,
                Some(cart_id),
                "a no-op rebind still reports the cartridge the chip named"
            );
            let events: i64 = conn
                .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
                .unwrap();
            assert_eq!(events, 0, "a no-op must write nothing at all");
        }

        /// Issue #162's realistic trigger: a volume `volume init --cartridge
        /// <barcode>` bound with NO medium serial (`identity_source =
        /// 'operator'`, cartridge A has no `serial_number` recorded) must
        /// not be silently reported as bound to a DIFFERENT, already
        /// registered cartridge (B) that THIS contact's medium resolves to.
        ///
        /// In the full `volume_write` pipeline `corroborate_volume` runs
        /// before `bind_late` and would itself catch a serial already
        /// recorded on another row — but `bind_late` gets its own
        /// independent refusal too (issue #162's "every writer of
        /// `cartridge_volumes` gets the refusal"), which is what this test,
        /// driving `bind_late` directly, proves.
        #[test]
        fn an_operator_bound_volume_refuses_when_the_medium_resolves_elsewhere() {
            let (conn, vol_id) = unbound_volume();
            // Cartridge A: bound at init by barcode alone, no serial ever
            // read — the `volume init --cartridge A` path.
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number,
                                         status)
                 VALUES ('BC-A', 'LTO-6', 2500000000000, NULL, 'in_use')",
                [],
            )
            .unwrap();
            let cart_a = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO cartridge_volumes (cartridge_id, volume_id, identity_source)
                 VALUES (?1, ?2, 'operator')",
                params![cart_a, vol_id],
            )
            .unwrap();

            // Cartridge B: a DIFFERENT, already-registered cartridge whose
            // serial this contact's medium reports.
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number,
                                         status)
                 VALUES ('BC-B', 'LTO-6', 2500000000000, 'SER-2', 'available')",
                [],
            )
            .unwrap();

            let err = call(&conn, vol_id, &det_with_serial(Some("SER-2")))
                .expect_err("a write whose medium resolves to a DIFFERENT cartridge must refuse");
            let msg = err.to_string();
            assert!(
                msg.contains("BC-A"),
                "must name the existing cartridge: {msg}"
            );
            assert!(
                msg.contains("BC-B"),
                "must name the resolved cartridge: {msg}"
            );
            assert!(
                msg.contains("permanent once its mount is closed"),
                "must cite ADR-0012's closing ruling: {msg}"
            );
            // The refusal must not have touched the existing binding.
            assert_eq!(bound_barcode(&conn, vol_id).as_deref(), Some("BC-A"));
        }

        /// The fourth arm, and the one that carries the whole `identity_
        /// source` distinction (issue #296): a volume bound by the
        /// operator's typed barcode, with a medium whose serial matches NO
        /// registered row. Absence is not contradiction (ADR-0012), so this
        /// stays the no-op it always was — and returns `None`, because
        /// nothing this chip said established that binding.
        ///
        /// `cartridge_contacts` has no `identity_source` column, so a
        /// `Some` here would write an operator's assertion into a column
        /// that reads as an observation off the medium.
        #[test]
        fn a_bound_volume_whose_medium_is_unregistered_reports_no_chip_identity() {
            let (conn, vol_id) = unbound_volume();
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number,
                                         status)
                 VALUES ('BC-TYPED', 'LTO-6', 2500000000000, NULL, 'in_use')",
                [],
            )
            .unwrap();
            let cart = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO cartridge_volumes (cartridge_id, volume_id, identity_source)
                 VALUES (?1, ?2, 'operator')",
                params![cart, vol_id],
            )
            .unwrap();

            let bound = call(&conn, vol_id, &det_with_serial(Some("SER-UNKNOWN"))).unwrap();
            assert_eq!(
                bound, None,
                "the chip named a serial the catalog does not know, so it established \
                 nothing — the binding here is the operator's, not the medium's"
            );
            // Unchanged, as the arm has always promised.
            assert_eq!(bound_barcode(&conn, vol_id).as_deref(), Some("BC-TYPED"));
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
            // `unretire`, not `mark-erased` (issue #163): the way back from
            // retired_permanent is the operator correcting a claim about the
            // MEDIUM, not declaring the bytes gone. This assertion is the
            // reason the string matters — it is the late-binding twin of
            // `binding::tests::a_retired_permanent_cartridge_is_refused`, and
            // both must name the same escape or the two contact points would
            // tell an operator different things.
            assert!(err.to_string().contains("unretire"), "got: {err}");
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
                // Must be able to write ORDERTEST's own LTO-6 media (issue
                // #166: volume_write now checks this before the pre-write
                // validate this test is aimed at) — an unrelated mismatch
                // here would refuse earlier, for the wrong reason.
                generation: "LTO-6".into(),
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

        /// What `volume write`/`volume resume` hand the rule: the MAM read
        /// alone, no File 0 (their File-0 discipline is the ADR-0003 consent
        /// gate, which a fact refusal must not pre-empt).
        fn mam(serial: Option<&str>) -> binding::MediumFacts {
            binding::MediumFacts::from_serial(serial.map(str::to_string))
        }

        #[test]
        fn the_same_cartridge_passes() {
            let (conn, vol) = bound_volume(Some("SER-1"));
            binding::corroborate_volume(&conn, vol, "L6-0001", &mam(Some("SER-1"))).unwrap();
        }

        #[test]
        fn a_different_cartridge_is_refused_naming_both_serials() {
            let (conn, vol) = bound_volume(Some("SER-1"));
            let err = binding::corroborate_volume(&conn, vol, "L6-0001", &mam(Some("SER-2")))
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
            binding::corroborate_volume(&conn, vol, "L6-0001", &mam(None)).unwrap();
        }

        /// REWRITTEN for ADR-0012 (issue #193). This test was
        /// `a_cartridge_row_with_no_recorded_serial_is_silent` and asserted
        /// that the contact did NOTHING — which was the defect: "a bound row
        /// that has no serial yet learns it at that contact, once". Silence
        /// left the row unable to be corroborated at any later contact,
        /// forever.
        #[test]
        fn a_cartridge_row_with_no_recorded_serial_learns_it() {
            let (conn, vol) = bound_volume(None);
            binding::corroborate_volume(&conn, vol, "L6-0001", &mam(Some("SER-2"))).unwrap();
            let recorded: Option<String> = conn
                .query_row(
                    "SELECT serial_number FROM cartridges WHERE barcode = 'BC001'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(recorded.as_deref(), Some("SER-2"));
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
            binding::corroborate_volume(&conn, vol, "L6-0001", &mam(Some("SER-2"))).unwrap();
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

    // --- Per-call-site contact tests (ADR-0012, issues #193/#164) ----------
    //
    // The RULE's own branch tests live in `volume::binding`. These prove only
    // that each contact CALLS it — "a contact that skips corroboration is the
    // defect returning" — by loading a tape whose File 0 names a DIFFERENT
    // volume and asserting the contact refuses before doing its work.
    mod contacts {
        use super::*;

        /// A volume row plus one slice at position 4, and a MemStore holding
        /// a complete v2 tape that claims to be `on_tape_label`.
        ///
        /// When the two labels differ, this is the wrong cartridge in the
        /// drive — the whole point.
        fn wrong_tape(
            conn: &Connection,
            catalog_label: &str,
            on_tape_label: &str,
            unit: &str,
        ) -> MemStore {
            let data = b"contact fixture slice bytes, repeated. ".repeat(4);
            seed_one_slice_fixture(conn, catalog_label, unit, 4, &data, "completed", "staged");
            mem_store_v2_tape(on_tape_label, &data, &data)
        }

        fn volume_id(conn: &Connection, label: &str) -> i64 {
            conn.query_row(
                "SELECT id FROM volumes WHERE label = ?1",
                params![label],
                |r| r.get(0),
            )
            .unwrap()
        }

        fn verification_sessions(conn: &Connection) -> i64 {
            conn.query_row("SELECT COUNT(*) FROM verification_sessions", [], |r| {
                r.get(0)
            })
            .unwrap()
        }

        // ── issue #164: `volume verify` ──

        /// THE #164 test: verify `L6-B` with `L6-A` in the drive refuses,
        /// errors (a non-zero exit — `cli::volume` propagates this `Err`
        /// rather than reaching `verify_exit_code` at all), and leaves
        /// `verification_sessions` EMPTY FOR BOTH volumes.
        ///
        /// Evidence recorded against the wrong volume is worse than no
        /// evidence, because it refreshes a staleness clock that gates
        /// nothing else; and quarantine is for the volume whose claims were
        /// contradicted, never the innocent one whose label was typed. So
        /// neither row may exist.
        #[test]
        fn verify_refuses_when_file0_names_another_volume_and_records_nothing() {
            let conn = crate::db::open_memory().unwrap();
            let mut store = wrong_tape(&conn, "L6-B", "L6-A", "cb-unit");
            // The innocent volume whose tape is actually loaded.
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
                 VALUES ('L6-A', 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
                [],
            )
            .unwrap();

            let b = volume_id(&conn, "L6-B");
            let err = volume_verify_with_store(
                &conn,
                &mut store,
                "L6-B",
                b,
                4096,
                Tier::Integrity,
                site(Operation::VolumeVerify),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("wrong tape"), "{err}");
            assert!(err.contains("L6-B"), "must name what was asked for: {err}");
            assert!(err.contains("L6-A"), "must name what was found: {err}");

            assert_eq!(
                verification_sessions(&conn),
                0,
                "no verification_sessions row may exist for EITHER volume"
            );
            let results: i64 = conn
                .query_row("SELECT COUNT(*) FROM verification_results", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(results, 0);
        }

        /// The other half of #164: the CORRECT tape still records the
        /// session exactly as before.
        #[test]
        fn verify_with_the_correct_tape_records_the_session_as_before() {
            let conn = crate::db::open_memory().unwrap();
            let mut store = wrong_tape(&conn, "L6-RIGHT", "L6-RIGHT", "cb-unit");
            let v = volume_id(&conn, "L6-RIGHT");
            let report = volume_verify_with_store(
                &conn,
                &mut store,
                "L6-RIGHT",
                v,
                4096,
                Tier::Integrity,
                site(Operation::VolumeVerify),
            )
            .unwrap();
            assert_eq!(report.failed, 0, "{:?}", report.mismatches);
            let (vol, outcome): (i64, String) = conn
                .query_row(
                    "SELECT volume_id, outcome FROM verification_sessions",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(vol, v);
            assert_eq!(outcome, "passed");
        }

        /// A wrong CARTRIDGE, rather than a wrong volume label: File 0 agrees
        /// with the catalog about which volume this is, but the bound row's
        /// serial and the drive's disagree. Same refusal, same empty table.
        #[test]
        fn verify_refuses_a_wrong_cartridge_and_records_nothing() {
            let conn = crate::db::open_memory().unwrap();
            let mut store = wrong_tape(&conn, "L6-C", "L6-C", "cb-unit");
            let v = volume_id(&conn, "L6-C");
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number, status)
                 VALUES ('BC-BOUND', 'LTO-6', 2500000000000, 'SER-BOUND', 'in_use')",
                [],
            )
            .unwrap();
            let cart = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO cartridge_volumes (cartridge_id, volume_id, identity_source)
                 VALUES (?1, ?2, 'mam')",
                params![cart, v],
            )
            .unwrap();

            let err = volume_verify_with_store(
                &conn,
                &mut store,
                "L6-C",
                v,
                4096,
                Tier::Integrity,
                site_observed(Operation::VolumeVerify, &mam_serial("SER-LOADED")),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("wrong cartridge"), "{err}");
            assert_eq!(verification_sessions(&conn), 0);
        }

        // ── the other store-injectable contacts ──

        #[test]
        fn read_slices_refuses_the_wrong_tape_before_reading_a_slice() {
            let conn = crate::db::open_memory().unwrap();
            let mut store = wrong_tape(&conn, "RS-WANT", "RS-LOADED", "rs-unit2");
            let tmp = tempfile::TempDir::new().unwrap();
            let mut config = Config::default();
            config.staging.directory = tmp.path().to_string_lossy().into_owned();

            let err = read_slices(
                &conn,
                &config,
                "RS-WANT",
                "rs-unit2",
                &mut store,
                site(Operation::VolumeReadSlices),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("wrong tape"), "{err}");
            let staged: Option<String> = conn
                .query_row("SELECT staging_path FROM stage_slices", [], |r| r.get(0))
                .unwrap();
            assert!(
                staged.is_none(),
                "a refused contact must not have staged anything: {staged:?}"
            );
        }

        #[test]
        fn compact_read_refuses_the_wrong_tape_before_reading_a_slice() {
            let conn = crate::db::open_memory().unwrap();
            let mut store = wrong_tape(&conn, "CR-WANT", "CR-LOADED", "cr-unit2");
            let tmp = tempfile::TempDir::new().unwrap();
            let mut config = Config::default();
            config.staging.directory = tmp.path().to_string_lossy().into_owned();

            let err = compact_read(
                &conn,
                &config,
                "CR-WANT",
                &mut store,
                site(Operation::VolumeCompactRead),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("wrong tape"), "{err}");
        }

        // ── `volume identify` ──

        /// `identify` REPORTS a contradiction rather than withholding the
        /// answer. Every other contact refuses because it is about to act on
        /// the tape; this one exists to say what is loaded, and it is run
        /// precisely when the catalog and the shelf have stopped agreeing.
        #[test]
        fn identify_reports_the_contradiction_but_still_prints_the_tape() {
            let conn = crate::db::open_memory().unwrap();
            let mut store = wrong_tape(&conn, "ID-VOL", "ID-VOL", "id-unit");
            let v = volume_id(&conn, "ID-VOL");
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number, status)
                 VALUES ('BC-ID', 'LTO-6', 2500000000000, 'SER-SHELF', 'in_use')",
                [],
            )
            .unwrap();
            let cart = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO cartridge_volumes (cartridge_id, volume_id, identity_source)
                 VALUES (?1, ?2, 'mam')",
                params![cart, v],
            )
            .unwrap();

            let id = volume_identify_corroborated(
                &conn,
                &mut store,
                site_observed(Operation::VolumeIdentify, &mam_serial("SER-DRIVE")),
            )
            .unwrap();
            // The answer is printed, not withheld: the tape's own File 0 comes
            // back intact, and the disagreement rides alongside it.
            assert!(id.text.contains("ID-VOL"), "{}", id.text);
            let why = id
                .contradiction
                .expect("the contradiction must be reported");
            assert!(why.contains("wrong cartridge"), "{why}");
        }

        /// `identify` is what an operator runs when they do NOT know what is
        /// loaded, so a tape this catalog has never heard of is the ANSWER,
        /// not a contradiction: it prints and returns Ok.
        #[test]
        fn identify_prints_a_tape_the_catalog_has_never_heard_of() {
            let conn = crate::db::open_memory().unwrap();
            let data = b"x".repeat(16);
            let mut store = mem_store_v2_tape("NEVER-SEEN", &data, &data);
            let id = volume_identify_corroborated(
                &conn,
                &mut store,
                site_observed(Operation::VolumeIdentify, &mam_serial("SER-ANY")),
            )
            .unwrap();
            assert!(id.text.contains("NEVER-SEEN"), "{}", id.text);
            assert!(
                id.contradiction.is_none(),
                "an absence is not a contradiction"
            );
        }

        // ── the two device-bound contacts ──

        // ── issue #296: the contact ROW each of these writes ──
        //
        // `operation` and `outcome` are asserted BY VALUE, never by
        // `is_some()`: a row exists either way, and "which command, ending
        // how" is the entire question `cartridge_contacts` was added to
        // answer. Every refusal below records its contact too — the contact
        // happened; the command refusing is what `outcome` is for.

        /// The happy path. One row, `volume verify`, `ok`.
        #[test]
        fn verify_records_one_contact_naming_the_command_and_a_clean_outcome() {
            let conn = crate::db::open_memory().unwrap();
            let good = b"intact slice bytes for the contact row test. ".repeat(4);
            seed_one_slice_fixture(
                &conn,
                "VC-OK",
                "vc-ok-unit",
                4,
                &good,
                "completed",
                "current",
            );
            let v = volume_id(&conn, "VC-OK");
            let mut store = mem_store_v2_tape("VC-OK", &good, &good);
            let report = volume_verify_with_store(
                &conn,
                &mut store,
                "VC-OK",
                v,
                4096,
                Tier::Integrity,
                site(Operation::VolumeVerify),
            )
            .unwrap();
            assert_eq!(report.failed, 0, "{:?}", report.mismatches);

            assert_eq!(contact_count(&conn), 1);
            let (operation, outcome, detail) = only_contact(&conn);
            assert_eq!(operation, "volume verify");
            assert_eq!(outcome.as_deref(), Some("ok"));
            assert_eq!(detail, None);
            let vol: Option<i64> = conn
                .query_row("SELECT volume_id FROM cartridge_contacts", [], |r| r.get(0))
                .unwrap();
            assert_eq!(vol, Some(v), "the contact names the volume it verified");
        }

        /// A verify that FOUND mismatches returns `Ok` — the non-zero exit is
        /// `verify_exit_code`'s job one layer up — so `finish_result` would
        /// have recorded `ok`. Migration 020 says this table exists for "the
        /// 2031 operator holding a tape with two uncorrected read errors";
        /// that operator must be able to find this contact by its outcome.
        #[test]
        fn a_verify_that_found_mismatches_records_a_failed_contact_not_an_ok_one() {
            let conn = crate::db::open_memory().unwrap();
            let good = b"the only copy of this version, on one tape. ".repeat(4);
            let rotted = b"what the drive actually read back today!!!! ".repeat(4);
            assert_eq!(good.len(), rotted.len());
            seed_one_slice_fixture(
                &conn,
                "VC-ROT",
                "vc-rot-unit",
                4,
                &good,
                "completed",
                "current",
            );
            let v = volume_id(&conn, "VC-ROT");
            let mut store = mem_store_v2_tape("VC-ROT", &good, &rotted);
            let report = volume_verify_with_store(
                &conn,
                &mut store,
                "VC-ROT",
                v,
                4096,
                Tier::Integrity,
                site(Operation::VolumeVerify),
            )
            .unwrap();
            assert_eq!(report.failed, 1, "fixture must actually mismatch");

            let (operation, outcome, detail) = only_contact(&conn);
            assert_eq!(operation, "volume verify");
            assert_eq!(
                outcome.as_deref(),
                Some("failed"),
                "an Ok(report) with mismatches is not an `ok` contact"
            );
            assert!(
                detail.as_deref().unwrap_or("").contains("mismatched"),
                "the count belongs in detail: {detail:?}"
            );
        }

        /// The refusal shape, for the read seams: a wrong tape records the
        /// contact with a non-OK outcome. The contact happened — a cartridge
        /// was in a drive and File 0 was read off it — even though the
        /// command then refused.
        #[test]
        fn a_verify_refused_for_the_wrong_tape_still_records_its_contact() {
            let conn = crate::db::open_memory().unwrap();
            let mut store = wrong_tape(&conn, "VC-WANT", "VC-LOADED", "vc-w-unit");
            let v = volume_id(&conn, "VC-WANT");
            let err = volume_verify_with_store(
                &conn,
                &mut store,
                "VC-WANT",
                v,
                4096,
                Tier::Integrity,
                site(Operation::VolumeVerify),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("wrong tape"), "{err}");

            assert_eq!(
                verification_sessions(&conn),
                0,
                "issue #164 still holds: no evidence row for either volume"
            );
            assert_eq!(
                contact_count(&conn),
                1,
                "but the CONTACT is recorded — it physically happened"
            );
            let (operation, outcome, detail) = only_contact(&conn);
            assert_eq!(operation, "volume verify");
            assert_eq!(outcome.as_deref(), Some("failed"));
            assert!(
                detail.as_deref().unwrap_or("").contains("wrong tape"),
                "the refusal is the detail: {detail:?}"
            );
        }

        #[test]
        fn read_slices_records_one_contact_naming_the_command() {
            let conn = crate::db::open_memory().unwrap();
            let data = b"read-slices contact fixture bytes, repeated. ".repeat(4);
            seed_one_slice_fixture(&conn, "RSC-OK", "rsc-unit", 4, &data, "completed", "staged");
            let v = volume_id(&conn, "RSC-OK");
            let mut store = mem_store_v2_tape("RSC-OK", &data, &data);
            let tmp = tempfile::TempDir::new().unwrap();
            let mut config = Config::default();
            config.staging.directory = tmp.path().to_string_lossy().into_owned();

            read_slices(
                &conn,
                &config,
                "RSC-OK",
                "rsc-unit",
                &mut store,
                site(Operation::VolumeReadSlices),
            )
            .unwrap();

            assert_eq!(contact_count(&conn), 1);
            let (operation, outcome, _) = only_contact(&conn);
            assert_eq!(operation, "volume read-slices");
            assert_eq!(outcome.as_deref(), Some("ok"));
            let vol: Option<i64> = conn
                .query_row("SELECT volume_id FROM cartridge_contacts", [], |r| r.get(0))
                .unwrap();
            assert_eq!(vol, Some(v));
        }

        #[test]
        fn read_slices_refused_for_the_wrong_tape_still_records_its_contact() {
            let conn = crate::db::open_memory().unwrap();
            let mut store = wrong_tape(&conn, "RSC-WANT", "RSC-LOADED", "rsc-w-unit");
            let tmp = tempfile::TempDir::new().unwrap();
            let mut config = Config::default();
            config.staging.directory = tmp.path().to_string_lossy().into_owned();

            read_slices(
                &conn,
                &config,
                "RSC-WANT",
                "rsc-w-unit",
                &mut store,
                site(Operation::VolumeReadSlices),
            )
            .unwrap_err();

            let (operation, outcome, _) = only_contact(&conn);
            assert_eq!(operation, "volume read-slices");
            assert_eq!(outcome.as_deref(), Some("failed"));
        }

        /// `volume compact-read` and `volume compact` share ONE function and
        /// differ only in the operation their site carries. Both halves are
        /// asserted, because a constant inside `compact_read` would pass the
        /// first and silently mislabel every step 1 of the interactive
        /// command.
        #[test]
        fn compact_read_records_the_operation_its_site_carries_not_a_constant() {
            for (operation, expected) in [
                (Operation::VolumeCompactRead, "volume compact-read"),
                (Operation::VolumeCompact, "volume compact"),
            ] {
                let conn = crate::db::open_memory().unwrap();
                let data = b"compact-read contact fixture bytes, repeated. ".repeat(4);
                seed_one_slice_fixture(
                    &conn,
                    "CRC-OK",
                    "crc-unit",
                    4,
                    &data,
                    "completed",
                    "current",
                );
                let mut store = mem_store_v2_tape("CRC-OK", &data, &data);
                let tmp = tempfile::TempDir::new().unwrap();
                let mut config = Config::default();
                config.staging.directory = tmp.path().to_string_lossy().into_owned();

                compact_read(&conn, &config, "CRC-OK", &mut store, site(operation)).unwrap();

                assert_eq!(contact_count(&conn), 1);
                let (recorded, outcome, _) = only_contact(&conn);
                assert_eq!(
                    recorded, expected,
                    "the operation must come from the SITE: `volume compact` step 1 and \
                     `volume compact-read` are different commands through one function"
                );
                assert_eq!(outcome.as_deref(), Some("ok"));
            }
        }

        #[test]
        fn compact_read_refused_for_the_wrong_tape_still_records_its_contact() {
            let conn = crate::db::open_memory().unwrap();
            let mut store = wrong_tape(&conn, "CRC-WANT", "CRC-LOADED", "crc-w-unit");
            let tmp = tempfile::TempDir::new().unwrap();
            let mut config = Config::default();
            config.staging.directory = tmp.path().to_string_lossy().into_owned();

            compact_read(
                &conn,
                &config,
                "CRC-WANT",
                &mut store,
                site(Operation::VolumeCompactRead),
            )
            .unwrap_err();

            let (operation, outcome, _) = only_contact(&conn);
            assert_eq!(operation, "volume compact-read");
            assert_eq!(outcome.as_deref(), Some("failed"));
        }

        /// `identify` runs against whatever tape is loaded, so `volume_id` is
        /// NULL by construction — one of the two cases migration 020 names
        /// for that column being nullable. The volume File 0 CLAIMS is a
        /// claim, not the command's subject.
        #[test]
        fn identify_records_a_contact_with_no_volume_and_no_contradiction() {
            let conn = crate::db::open_memory().unwrap();
            let data = b"y".repeat(16);
            let mut store = mem_store_v2_tape("ID-NEVER-SEEN", &data, &data);
            volume_identify_corroborated(
                &conn,
                &mut store,
                site_observed(Operation::VolumeIdentify, &mam_serial("SER-ANY")),
            )
            .unwrap();

            assert_eq!(contact_count(&conn), 1);
            let (operation, outcome, _) = only_contact(&conn);
            assert_eq!(operation, "volume identify");
            assert_eq!(outcome.as_deref(), Some("ok"));
            let vol: Option<i64> = conn
                .query_row("SELECT volume_id FROM cartridge_contacts", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                vol, None,
                "`identify` takes no label; the tape's own claim is not the catalog's"
            );
        }

        /// `identify` returns `Ok` on a contradiction — the answer is
        /// printed, never withheld — but the CLI turns it into a non-zero
        /// exit, and that is how the contact ENDED.
        #[test]
        fn an_identify_that_contradicts_the_catalog_records_a_failed_contact() {
            let conn = crate::db::open_memory().unwrap();
            let mut store = wrong_tape(&conn, "IDC-VOL", "IDC-VOL", "idc-unit");
            let v = volume_id(&conn, "IDC-VOL");
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number, status)
                 VALUES ('BC-IDC', 'LTO-6', 2500000000000, 'SER-SHELF', 'in_use')",
                [],
            )
            .unwrap();
            let cart = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO cartridge_volumes (cartridge_id, volume_id, identity_source)
                 VALUES (?1, ?2, 'mam')",
                params![cart, v],
            )
            .unwrap();

            let id = volume_identify_corroborated(
                &conn,
                &mut store,
                site_observed(Operation::VolumeIdentify, &mam_serial("SER-DRIVE")),
            )
            .unwrap();
            assert!(id.contradiction.is_some(), "fixture must contradict");

            let (operation, outcome, detail) = only_contact(&conn);
            assert_eq!(operation, "volume identify");
            assert_eq!(outcome.as_deref(), Some("failed"));
            assert!(
                detail.as_deref().unwrap_or("").contains("wrong cartridge"),
                "{detail:?}"
            );
        }

        // ── the three write paths, and the three that inherit ──
        //
        // These CANNOT reach a tape ungated, but they CAN be driven past
        // their own MAM read: `check_drive_can_write` refuses an LTO-8 drive
        // asked to write an LTO-6 volume, and that refusal sits AFTER
        // `detect()` and BEFORE `TapeStore::open`. So the window the contact
        // now covers is exactly the window an ungated test can enter, and
        // the row it leaves is real.

        /// A backend whose generation (LTO-8) cannot write the fixtures'
        /// LTO-6 volumes, on a device that does not exist — so `detect`
        /// finds nothing, `check_drive_can_write` falls back to the volume's
        /// own recorded generation, and refuses.
        fn lto8_config(staging: &std::path::Path) -> Config {
            let mut config = Config::default();
            config.staging.directory = staging.join("staging").to_string_lossy().into_owned();
            config.backends.lto.push(crate::config::LtoBackendConfig {
                name: "lto8".into(),
                device_tape: GENCHK_DEVICE.into(),
                device_sg: "/nonexistent/tapectl-contact-genchk-sg".into(),
                generation: "LTO-8".into(),
                capacity_override: None,
                usable_capacity_factor: 1.0,
                enospc_buffer: "0".into(),
            });
            config
        }

        const GENCHK_DEVICE: &str = "/nonexistent/tapectl-contact-genchk-nst";

        /// One staged unit plus an `initialized` LTO-6 volume named `label`,
        /// enough for `volume_write` to get past `find_staged_data` and down
        /// to the generation refusal.
        fn genchk_fixture(label: &str) -> Connection {
            let (conn, _tenant_id, stage_set_id) = escrow_check_fixture();
            conn.execute(
                "INSERT INTO stage_slices
                    (stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain,
                     sha256_encrypted, staging_path)
                 VALUES (?1, 1, 10, 10, 'p', 'e', '/nonexistent/tapectl-contact-slice.dar.age')",
                params![stage_set_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                      capacity_bytes, status)
                 VALUES (?1, 'lto', 'lto8', 'LTO-6', 2500000000000, 'initialized')",
                params![label],
            )
            .unwrap();
            conn
        }

        #[test]
        fn a_refused_write_records_its_contact_as_volume_write_failed() {
            let conn = genchk_fixture("WC-GEN");
            let tmp = tempfile::TempDir::new().unwrap();
            let config = lto8_config(tmp.path());
            let paths = TapectlPaths::new(tmp.path().join("home"));

            let err = volume_write(
                &conn,
                &paths,
                &config,
                "WC-GEN",
                GENCHK_DEVICE,
                512 * 1024,
                false,
                false,
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("cannot write LTO-6"), "{err}");

            assert_eq!(contact_count(&conn), 1);
            let (operation, outcome, detail) = only_contact(&conn);
            assert_eq!(operation, "volume write");
            assert_eq!(outcome.as_deref(), Some("failed"));
            assert!(
                detail
                    .as_deref()
                    .unwrap_or("")
                    .contains("cannot write LTO-6"),
                "{detail:?}"
            );
        }

        /// The negative half, and the control this whole slot design exists
        /// for: a refusal ABOVE the MAM read is not a contact, and must
        /// leave no row. `volumes.status = 'sealed'` refuses before any
        /// backend is resolved, let alone a drive touched.
        #[test]
        fn a_write_refused_before_the_mam_read_records_no_contact_at_all() {
            let conn = genchk_fixture("WC-SEALED");
            conn.execute(
                "UPDATE volumes SET status = 'sealed' WHERE label = 'WC-SEALED'",
                [],
            )
            .unwrap();
            let tmp = tempfile::TempDir::new().unwrap();
            let config = lto8_config(tmp.path());
            let paths = TapectlPaths::new(tmp.path().join("home"));

            volume_write(
                &conn,
                &paths,
                &config,
                "WC-SEALED",
                GENCHK_DEVICE,
                512 * 1024,
                false,
                false,
            )
            .unwrap_err();
            assert_eq!(
                contact_count(&conn),
                0,
                "nothing was ever in a drive: a catalog-status refusal is not a contact"
            );
        }

        /// `volume compact-write` INHERITS `volume_write`'s contact. Exactly
        /// one row, and it says `volume write` — two rows would mean it
        /// opened its own, recording one physical contact twice.
        #[test]
        fn compact_write_inherits_exactly_one_contact_from_volume_write() {
            let conn = genchk_fixture("WC-COMPACT");
            let tmp = tempfile::TempDir::new().unwrap();
            let config = lto8_config(tmp.path());
            let paths = TapectlPaths::new(tmp.path().join("home"));

            compact_write(
                &conn,
                &paths,
                &config,
                "WC-COMPACT",
                GENCHK_DEVICE,
                512 * 1024,
                false,
            )
            .unwrap_err();

            assert_eq!(
                contact_count(&conn),
                1,
                "compact-write reaches the drive only through volume_write"
            );
            assert_eq!(only_contact(&conn).0, "volume write");
        }

        /// `collection run` INHERITS too, through `collection::batch::
        /// execute_batch`'s write loop. Exactly one row, saying `volume
        /// write`.
        ///
        /// The batch's staging loop is driven to its `(false, "staged")`
        /// no-op arm — an empty source directory matching a snapshot with no
        /// file rows — so no `dar` runs and the loop reaches `volume_write`
        /// with the fixture's own staged data, exactly as a real run does
        /// once its units are already staged.
        #[test]
        fn collection_run_inherits_exactly_one_contact_from_volume_write() {
            let conn = genchk_fixture("WC-COLL");
            let tmp = tempfile::TempDir::new().unwrap();
            let src = tmp.path().join("photos");
            std::fs::create_dir_all(&src).unwrap();
            conn.execute(
                "UPDATE units SET current_path = ?1 WHERE name = 'photos'",
                params![src.to_string_lossy()],
            )
            .unwrap();

            let config = lto8_config(tmp.path());
            let paths = TapectlPaths::new(tmp.path().join("home"));
            let batch = crate::collection::selector::Batch {
                units: vec![crate::collection::selector::PendingUnit {
                    name: "photos".into(),
                    size_bytes: 10,
                }],
                total_bytes: 10,
                padded_bytes: 10,
            };

            let err = crate::collection::batch::execute_batch(
                &conn,
                &paths,
                &config,
                &batch,
                &["WC-COLL".to_string()],
                GENCHK_DEVICE,
                512 * 1024,
            )
            .unwrap_err()
            .to_string();
            assert!(
                err.contains("cannot write LTO-6"),
                "the batch must have reached volume_write's tape-side refusal, not failed \
                 in staging: {err}"
            );

            assert_eq!(
                contact_count(&conn),
                1,
                "collection run reaches the drive only through volume_write"
            );
            assert_eq!(only_contact(&conn).0, "volume write");
        }

        /// `quick-archive` is the third inheritor the `Operation::VolumeWrite`
        /// doc names, and it reaches `volume_write` from `cli::operations`.
        /// Same claim, same proof.
        #[test]
        fn quick_archive_inherits_exactly_one_contact_from_volume_write() {
            const SRC: &str = include_str!("../cli/operations.rs");
            assert!(
                SRC.contains("crate::volume::write::volume_write("),
                "positive control: quick-archive must still reach the drive through \
                 volume_write, or this test proves nothing"
            );
            assert!(
                !SRC.contains("ContactGuard::open") && !SRC.contains("ContactSite::new"),
                "cli::operations must open no contact of its own — volume_write's is the \
                 one physical contact, and a second row would double-count it"
            );
        }

        /// `volume resume` is the one write path no ungated test can drive
        /// past its own MAM read: `InterruptedSession::rehydrate` runs
        /// BEFORE `detect()` and needs a real session directory with the
        /// frozen `layout.json` a completed `build`/`plan` left behind. So
        /// the placement is guarded by source scan — the same shape, and for
        /// the same reason, as `the_two_write_contacts_still_corroborate`
        /// just below.
        ///
        /// The scan covers all THREE write paths, which is what calibrates
        /// it: `volume_write`'s and `volume_init`'s contact rows are
        /// asserted behaviourally above, so a scan that passes on those two
        /// is a scan that recognises the real thing. A guard that only ever
        /// examined the untestable case could not tell "found the fill" from
        /// "found nothing and said so quietly".
        #[test]
        fn the_three_write_paths_open_their_contact_at_their_own_mam_read() {
            const SRC: &str = include_str!("write.rs");
            for f in [
                "fn volume_init_contacted",
                "fn volume_write_contacted",
                "fn volume_resume_contacted",
            ] {
                let start = SRC.find(f).unwrap_or_else(|| panic!("no fn {f}"));
                let end = SRC[start..].find("\n}\n").unwrap() + start;
                let body = &SRC[start..end];
                assert!(
                    !body[f.len()..].contains("\npub fn "),
                    "{f}: body extraction overran into another function; fix this test's \
                     scan before trusting its verdict"
                );
                let detect = body
                    .find("media_detect::detect(")
                    .unwrap_or_else(|| panic!("{f} no longer reads the MAM at all"));
                let fill = body
                    .find("contact.fill(ContactGuard::open(")
                    .unwrap_or_else(|| {
                        panic!(
                            "{f} no longer opens its contact. A write path that reaches the \
                         drive and records nothing is issue #296 returning."
                        )
                    });
                assert!(
                    detect < fill,
                    "{f} must open its contact AFTER its own MAM read, from the reading it \
                     already holds — the st driver refuses a second concurrent open, so a \
                     contact opened earlier could only be filled by a second read that \
                     cannot happen"
                );
                assert_eq!(
                    body.matches("ContactGuard::open(").count(),
                    1,
                    "{f} must open exactly ONE contact: one command holding the drive is \
                     one physical contact"
                );
            }
        }

        /// `volume init` refuses right after its MAM read when the drive
        /// reports no medium serial and no `--cartridge` names one
        /// (ADR-0012's `require_named_cartridge`). The contact is already
        /// open by then, so the refusal is recorded rather than lost.
        #[test]
        fn a_refused_init_records_its_contact_as_volume_init_failed() {
            let conn = crate::db::open_memory().unwrap();
            let tmp = tempfile::TempDir::new().unwrap();
            let config = lto8_config(tmp.path());

            let err = volume_init(
                &conn,
                &config,
                "IC-NEW",
                GENCHK_DEVICE,
                512 * 1024,
                false,
                None,
                None,
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("no medium serial"), "{err}");

            assert_eq!(contact_count(&conn), 1);
            let (operation, outcome, _) = only_contact(&conn);
            assert_eq!(operation, "volume init");
            assert_eq!(outcome.as_deref(), Some("failed"));
            // `volume init` creates the volume row, so a refusal before that
            // has none to name — and NULL here is honest, not missing.
            let vol: Option<i64> = conn
                .query_row("SELECT volume_id FROM cartridge_contacts", [], |r| r.get(0))
                .unwrap();
            assert_eq!(vol, None);
        }

        /// The negative control for `volume init`: a label that already
        /// exists refuses before the MAM read, and records nothing.
        #[test]
        fn an_init_refused_before_the_mam_read_records_no_contact_at_all() {
            let conn = crate::db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                      capacity_bytes, status)
                 VALUES ('IC-DUP', 'lto', 'lto8', 'LTO-6', 2500000000000, 'initialized')",
                [],
            )
            .unwrap();
            let tmp = tempfile::TempDir::new().unwrap();
            let config = lto8_config(tmp.path());

            volume_init(
                &conn,
                &config,
                "IC-DUP",
                GENCHK_DEVICE,
                512 * 1024,
                false,
                None,
                None,
            )
            .unwrap_err();
            assert_eq!(contact_count(&conn), 0);
        }

        /// `volume write` and `volume resume` cannot be driven without a tape
        /// device, so their corroboration is proved on real hardware by the
        /// mhvtl gate. This is the ungated guard that the CALL is still
        /// there: issue #193's "a contact that skips corroboration is the
        /// defect returning" is a statement about the source, and deleting
        /// the call is exactly how the defect comes back.
        #[test]
        fn the_two_write_contacts_still_corroborate() {
            const SRC: &str = include_str!("write.rs");
            // The INNER functions (issue #296): the public entry points are
            // now thin contact wrappers, and the work — corroboration
            // included — lives one call down. The call moved, so the
            // assertion moved with it, which is what this test's own
            // instruction below says to do.
            for f in ["fn volume_write_contacted", "fn volume_resume_contacted"] {
                let start = SRC.find(f).unwrap_or_else(|| panic!("no fn {f}"));
                // Function bodies end at the first `\n}` in column 0.
                let end = SRC[start..].find("\n}\n").unwrap() + start;
                let body = &SRC[start..end];
                // Guard the FALSE PASS, which is the failure mode that would
                // matter: if the `\n}\n` scan ever ran past the end of this
                // function, `body` could pick up a sibling's corroboration
                // call and report a deleted one as present. A second `pub fn`
                // in the slice means the extraction, not the code, is wrong.
                assert!(
                    !body[f.len()..].contains("\npub fn "),
                    "{f}: body extraction overran into another function; fix this test's \
                     scan before trusting its verdict"
                );
                assert!(
                    body.contains("binding::corroborate_volume("),
                    "{f} no longer corroborates the loaded medium at contact (ADR-0012, \
                     issue #193). If this call moved, move this assertion with it; do not \
                     delete it."
                );
            }
        }

        /// Issue #220. The three tape-side refusals -- corroboration,
        /// `check_loaded_generation`, `check_drive_can_write` -- all need a
        /// drive, so "a refused write leaves the MAM columns untouched"
        /// cannot be asserted by calling anything. The existing
        /// untouched-MAM tests cover only the CATALOG-status refusal, which
        /// fires before `detect` runs at all and so never exercised this.
        ///
        /// Same source-scan shape as the corroboration guard above, and for
        /// the same reason: the property is an ORDERING, and deleting or
        /// moving the write back above the refusals is exactly how it
        /// regresses.
        #[test]
        fn volume_write_records_mam_facts_only_after_the_tape_side_refusals() {
            const SRC: &str = include_str!("write.rs");
            let f = "fn volume_write_contacted";
            let start = SRC.find(f).unwrap_or_else(|| panic!("no fn {f}"));
            let end = SRC[start..].find("\n}\n").unwrap() + start;
            let body = &SRC[start..end];
            assert!(
                !body[f.len()..].contains("\npub fn "),
                "body extraction overran into another function; fix this test's scan \
                 before trusting its verdict"
            );

            let mam_update = body
                .find("UPDATE volumes SET mam_capacity_bytes")
                .expect("volume_write no longer records MAM facts at all");
            for (needle, what) in [
                (
                    "binding::corroborate_volume(",
                    "wrong-cartridge corroboration",
                ),
                ("check_loaded_generation(", "the wrong-medium refusal"),
                ("check_drive_can_write(", "the drive-cannot-write refusal"),
            ] {
                let guard = body
                    .find(needle)
                    .unwrap_or_else(|| panic!("volume_write no longer calls {needle}"));
                assert!(
                    guard < mam_update,
                    "the MAM bookkeeping UPDATE must run AFTER {what} (issue #220): a write \
                     refused there would otherwise record THAT cartridge's capacity facts \
                     onto this volume's row -- and on the corroboration path those are facts \
                     about a different physical cartridge, which is the refusal's whole point"
                );
            }
        }

        /// Issue #200. `volume_init` needs a drive, so the parser it picks for
        /// `capacity_override` cannot be pinned by calling it — and the whole
        /// suite passed both before and after the fix, which is exactly why
        /// this guard exists rather than a behavioural one. Same source-scan
        /// shape, same false-pass guard, as the corroboration test above.
        ///
        /// ADR-0012: capacities are DECIMAL. Reaching for
        /// `staging::parse_size_to_bytes` here (binary — right for slice sizes
        /// and the ENOSPC buffer, wrong for a capacity) over-credits the tape
        /// by ~10% and, worse, disagrees with the decimal parser
        /// `Config::validate_sizes` already validated the same string with.
        #[test]
        fn volume_init_parses_capacity_override_decimally() {
            const SRC: &str = include_str!("write.rs");
            let f = "fn volume_init_contacted";
            let start = SRC.find(f).unwrap_or_else(|| panic!("no fn {f}"));
            let end = SRC[start..].find("\n}\n").unwrap() + start;
            let body = &SRC[start..end];
            assert!(
                !body[f.len()..].contains("\npub fn "),
                "body extraction overran into another function; fix this test's scan \
                 before trusting its verdict"
            );

            let parse_site = body
                .find("backend.capacity_override")
                .expect("volume_init no longer reads backend.capacity_override");
            let tail = &body[parse_site..];
            let stmt_end = tail.find("};").map(|i| i + 2).unwrap_or(tail.len());
            let stmt = &tail[..stmt_end];

            assert!(
                stmt.contains("media::parse_capacity_to_bytes("),
                "volume_init must parse capacity_override with the DECIMAL parser \
                 (ADR-0012; issue #200). Found instead:\n{stmt}"
            );
            assert!(
                !stmt.contains("parse_size_to_bytes("),
                "volume_init is parsing capacity_override with the BINARY parser again \
                 (issue #200). That over-credits the tape by ~10% and contradicts the \
                 decimal parse Config::validate_sizes already applied to the same \
                 string.\n{stmt}"
            );
        }
    }
}
