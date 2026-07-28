use std::io::Cursor;
use std::path::PathBuf;
use std::{collections::HashSet, fs};

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::{Config, TapectlPaths};
use crate::db::{events, queries};
use crate::error::{Result, TapectlError};
use crate::staging;
use crate::tape::health;
use crate::tape::ioctl::TapeDevice;
use crate::tape::mam::MamInfo;

use crate::store::{Store, TapeStore, Tier};

use super::build::{self, BuildInputs, BuildSlice, BuildUnit, TenantInfo};
use super::format;
use super::layout;
use super::layout_model::{
    CapacityBudget, ContentSource, KeyAvailability, Layout, LayoutEntry, ZoneKind,
};
use super::session::{ConfirmOutcome, ExecuteOutcome, QuarantineReason};

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
pub fn volume_init(
    conn: &Connection,
    config: &Config,
    label: &str,
    device: &str,
    block_size: usize,
) -> Result<i64> {
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

    let backend = config
        .backends
        .lto
        .first()
        .ok_or_else(|| TapectlError::Config("no LTO backend configured".into()))?;

    let nominal_capacity = staging::parse_size_to_bytes(&backend.nominal_capacity);
    let media_type = &backend.media_type;

    conn.execute(
        "INSERT INTO volumes (label, uuid, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES (?1, ?2, 'lto', ?3, ?4, ?5, 'initialized')",
        params![
            label,
            Uuid::new_v4().to_string(),
            backend.name,
            media_type,
            nominal_capacity
        ],
    )?;
    let volume_id = conn.last_insert_rowid();

    // usable_bytes (the T4 capacity oracle) is informational at this stage —
    // volume_init only ever writes the provisional identity thunk; real
    // capacity gating happens in volume_write's pre-open validate.
    let usable_bytes = (nominal_capacity as f64 * backend.usable_capacity_factor) as u64;
    let mut store = TapeStore::open(device, block_size, usable_bytes)?;

    let volume_uuid = volume_uuid(conn, volume_id)?;
    // Provisional total_files: unknown until the write session builds the
    // real Layout. Format §1's minimum shape is 4 front files + >=1 tenant
    // envelope + operator + backup + >=1 slice + the seal marker; 8 is a
    // representative placeholder, thrown away wholesale (not preserved, not
    // interpreted) when the write session rewrites File 0 from BOT.
    const PROVISIONAL_TOTAL_FILES: i32 = 8;
    let created_at = chrono::Utc::now().to_rfc3339();
    let id_thunk = layout::generate_id_thunk_v2(&layout::IdThunkV2Params {
        label,
        uuid: &volume_uuid,
        media_type,
        tapectl_version: env!("CARGO_PKG_VERSION"),
        nominal_capacity,
        mam_capacity: 0,
        total_files: PROVISIONAL_TOTAL_FILES,
        mam_manufacturer: "",
        mam_serial: "",
        mam_length: 0,
        mam_loads: 0,
        created_at: &created_at,
    });

    store.execute(
        &mut Cursor::new(id_thunk.as_bytes()),
        id_thunk.len() as u64,
        false,
    )?;
    info!(label = label, "volume initialized");

    events::log_created(conn, "volume", volume_id, label, None)?;
    Ok(volume_id)
}

/// Full volume write pipeline (`docs/design/v2-implementation-plan.md` T8):
/// orchestration only. Gather the staged batch, assemble `BuildInputs` from
/// the DB, `build()` the Layout, then drive the §9 typestate session —
/// `validate -> plan -> execute -> seal -> confirm`
/// (`docs/design/v2-open-questions.md` §9). Every on-tape byte comes from
/// `build::build` + `session.rs` now; no hand-rolled layout logic (mini-index,
/// manual position arithmetic) remains here.
pub fn volume_write(
    conn: &Connection,
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

    // Refuse fast, before any real work, if this volume already has an
    // unresolved write session. `ValidatedLayout::plan` would otherwise hit
    // `writes`' `UNIQUE(stage_set_id, volume_id)` with a raw constraint
    // error on the retry. Cross-process resume — reconstructing a
    // `BuiltLayout` from a prior session's frozen files without recalling
    // `build()`, per `session::InterruptedSession::resume_checking`'s own
    // doc comment ("the CLI orchestrator's job (T8)") — is not wired into
    // this orchestrator (see the T8 report); this guard only turns what
    // would otherwise be an opaque SQL error into a clear, honest refusal.
    let existing_sessions: i64 = conn.query_row(
        "SELECT COUNT(*) FROM writes WHERE volume_id = ?1 AND status IN ('planned','in_progress','interrupted')",
        params![volume_id],
        |r| r.get(0),
    )?;
    if existing_sessions > 0 {
        return Err(TapectlError::Other(format!(
            "volume \"{label}\" already has an unresolved write session \
             (status planned/in_progress/interrupted) — automatic cross-process resume \
             is not yet wired; inspect the `writes`/`write_positions` rows for volume_id \
             {volume_id} before retrying"
        )));
    }

    let units = find_staged_data(conn)?;
    if units.is_empty() {
        return Err(TapectlError::Other(
            "no staged data to write — run `tapectl stage create` first".into(),
        ));
    }

    let backend = config
        .backends
        .lto
        .first()
        .ok_or_else(|| TapectlError::Config("no LTO backend configured".into()))?;
    let nominal_capacity = staging::parse_size_to_bytes(&backend.nominal_capacity);
    let usable_bytes = (nominal_capacity as f64 * backend.usable_capacity_factor) as u64;
    // v2 collapses the v1 "manifest reserve" into just the ENOSPC buffer
    // (`volume-format-v2.md` §8) — the old `manifest_reserve` config field is
    // gone (T10 config cleanup: nothing read it after this path stopped, and
    // this was that "nothing").
    let enospc_buffer = staging::parse_size_to_bytes(&backend.enospc_buffer).max(0) as u64;

    // Tenants + keys: one TenantInfo per distinct tenant_id in this batch,
    // its own active (non-escrow) keys only — build() appends operator and
    // escrow keys itself (`with_escrow`).
    let mut distinct_tenant_ids: Vec<i64> = units.iter().map(|u| u.tenant_id).collect();
    distinct_tenant_ids.sort_unstable();
    distinct_tenant_ids.dedup();

    let mut tenants = Vec::with_capacity(distinct_tenant_ids.len());
    let mut tenants_with_active_key = HashSet::new();
    for &tenant_id in &distinct_tenant_ids {
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
    // `KeyAvailability.escrow_recipient_present = Some(false)` below, the
    // same way `key rotate` refuses without one.
    let escrow_public_key = queries::escrow_public_key(conn)?;

    let keys = KeyAvailability {
        tenant_ids: distinct_tenant_ids,
        tenants_with_active_key,
        operator_key_present: true,
        escrow_recipient_present: Some(escrow_public_key.is_some()),
    };

    // MAM (best-effort, informational; never gates the write — the pre-flight
    // capacity gate below reads the configured nominal capacity, which is
    // reliable, per `layout-session.md`'s validation point 1). Read before
    // the tape stream itself is touched, so real values (where available)
    // land in the ID thunk instead of the placeholder zeros/blanks v1 always
    // wrote regardless of what MAM reported.
    let mam = match crate::tape::mam::read_mam(&backend.device_sg) {
        Ok(mam) => {
            let _ = conn.execute(
                "UPDATE volumes SET mam_capacity_bytes = ?1, mam_remaining_at_start = ?2
                 WHERE id = ?3",
                params![mam.max_capacity_bytes, mam.remaining_bytes, volume_id],
            );
            mam
        }
        Err(e) => {
            warn!(err = %e, "MAM read failed (continuing)");
            MamInfo::default()
        }
    };

    let volume_uuid = volume_uuid(conn, volume_id)?;
    let created_at = chrono::Utc::now().to_rfc3339();
    // Session directory: materialize-to-staging (`v2-open-questions.md`
    // §2.2) lives under the configured staging directory, namespaced per
    // volume label + a fresh session uuid (so a later attempt never collides
    // with an earlier one's frozen files).
    let session_dir = PathBuf::from(&config.staging.directory)
        .join("sessions")
        .join(format!("{label}-{}", Uuid::new_v4()));

    let inputs = BuildInputs {
        label: label.to_string(),
        volume_uuid,
        media_type: backend.media_type.clone(),
        tapectl_version: env!("CARGO_PKG_VERSION").to_string(),
        created_at,
        block_size: block_size as u64,
        usable_bytes,
        enospc_buffer,
        nominal_capacity,
        mam_capacity: mam.max_capacity_bytes.unwrap_or(0),
        mam_manufacturer: String::new(),
        mam_serial: mam.serial.clone().unwrap_or_default(),
        mam_length: 0,
        mam_loads: mam.load_count.unwrap_or(0),
        units,
        tenants,
        operator_public_keys,
        escrow_public_key,
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
        return Err(TapectlError::Other(format!(
            "volume \"{label}\" failed pre-write validation: {}",
            errs.iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        )));
    }

    let mut store = TapeStore::open(device, block_size, usable_bytes)?;

    let validated = built.into_validated(&keys, &mut store).map_err(|errs| {
        TapectlError::Other(format!(
            "volume \"{label}\" failed validation at contact: {}",
            errs.iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        ))
    })?;

    let planned = validated.plan(conn, volume_id, &inputs.units)?;
    let execute_outcome = planned.execute(conn, &mut store)?;

    let result: Result<()> = match execute_outcome {
        ExecuteOutcome::Ready(ready) => {
            let sealed_pending = ready.seal(&mut store)?;
            match sealed_pending.confirm(conn, &mut store, Tier::default())? {
                ConfirmOutcome::Sealed(sealed) => {
                    record_write_bookkeeping(conn, volume_id, &layout_snapshot, block_size as u64)?;
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
                ConfirmOutcome::Quarantined(q) => {
                    let reason = describe_quarantine(&q.reason);
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
                        "volume \"{label}\" quarantined at confirm: {reason}"
                    )))
                }
            }
        }
        ExecuteOutcome::Interrupted(_) => {
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
                "volume \"{label}\" write interrupted (SIGINT) — the tape is left unsealed and \
                 resumable in principle (`writes`/`write_positions` were left in the \
                 `interrupted` state), but automatic cross-process resume is not yet wired into \
                 this CLI (see the T8 report)"
            )))
        }
        ExecuteOutcome::Aborted(a) => {
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
    };

    // Best-effort sg_logs health collection. Never let a collection failure
    // shadow the real outcome above (matching v1: always attempted, its own
    // errors only logged).
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

    result
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

/// A one-line, human-readable summary of why a session quarantined —
/// `volume_write`'s fresh (non-resumed) path only ever reaches
/// `QuarantineReason::ConfirmFailed`; `IdentityMismatch`/`AlreadySealed` are
/// resume-only outcomes this orchestrator doesn't produce (it never calls
/// `InterruptedSession::resume`), but are handled here anyway so this stays
/// exhaustive if that changes.
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

    let usable_bytes = config
        .backends
        .lto
        .first()
        .map(|b| {
            (staging::parse_size_to_bytes(&b.nominal_capacity) as f64 * b.usable_capacity_factor)
                as u64
        })
        .unwrap_or(0);
    let mut store = TapeStore::open(device, block_size, usable_bytes)?;

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
    conn.execute(
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

    for m in &evidence.mismatches {
        warn!(
            position = m.position,
            kind = ?m.kind,
            expected = %m.expected,
            actual = %m.actual,
            "verify mismatch"
        );
    }

    // Best-effort sg_logs health collection. Advisory only.
    if let Some(bk) = config.backends.lto.iter().find(|b| b.device_tape == device) {
        if let Ok((counters, raw)) = health::collect(&bk.device_sg) {
            if let Err(e) = health::record(conn, volume_id, "verify", &counters, &raw) {
                warn!(err = %e, "health_logs insert failed");
            }
        }
    }

    Ok(VerifyReport {
        checked: evidence.files_checked as usize,
        passed: (evidence.files_checked as usize).saturating_sub(evidence.mismatches.len()),
        failed: evidence.mismatches.len(),
    })
}

/// Read and display the ID thunk from a tape. Reads File 0 as raw text
/// (magic + label, whatever version) — no version-dispatch logic needed
/// here: v1 and v2 thunks are both plain text an heir reads with `dd | tr -d
/// '\0'`, and the v2 magic (`tapectl-volume-v2`) already self-identifies
/// within that text (`v2-open-questions.md` §2.7: "volume_identify reads
/// File 0 only... needs the v2 magic accepted alongside v1" — true by
/// construction, since this never parses the magic at all).
pub fn volume_identify(device: &str, block_size: usize) -> Result<String> {
    let mut tape = TapeDevice::open_read(device, block_size)?;
    tape.rewind()?;
    let data = tape.read_file()?;
    let text = String::from_utf8_lossy(&data).to_string();
    Ok(text.trim_end_matches('\0').to_string())
}

/// Read encrypted slices for a unit from a volume into staging.
/// After this, use `volume write` to write them to a destination tape
/// with the full self-describing volume layout.
///
/// Position-based, driven entirely from `write_positions` (DB), not the
/// on-tape index — unaffected by the v2 index relocation
/// (`v2-open-questions.md` §2.7).
pub fn read_slices(
    conn: &Connection,
    config: &Config,
    from_label: &str,
    unit_name: &str,
    device: &str,
    block_size: usize,
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

    let mut tape = TapeDevice::open_read(device, block_size)?;
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
        let pos: i32 = pos_str.parse().unwrap_or(0);

        tape.rewind()?;
        if pos > 0 {
            tape.forward_space_file(pos)?;
        }

        let data = tape.read_file()?;

        // Trim padding to encrypted_bytes
        let trimmed = if (*enc_bytes as usize) < data.len() {
            &data[..*enc_bytes as usize]
        } else {
            &data
        };

        // Verify checksum
        let actual_sha = sha256_hex(trimmed);
        if actual_sha != *sha_on_vol && actual_sha != *sha_encrypted {
            return Err(TapectlError::Other(format!(
                "checksum mismatch reading slice at position {pos} from {from_label}"
            )));
        }

        let slice_path = clone_dir.join(format!("slice_{slice_db_id}.dat"));
        fs::write(&slice_path, trimmed)?;

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
pub fn compact_read(
    conn: &Connection,
    config: &Config,
    label: &str,
    device: &str,
    block_size: usize,
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

    let mut tape = TapeDevice::open_read(device, block_size)?;
    let mut total_bytes: i64 = 0;
    let mut slices_read: i64 = 0;
    let mut slices_skipped: i64 = 0;
    let mut affected_stage_sets = HashSet::new();

    for (pos_str, sha_on_vol, _slice_id, enc_bytes, sha_encrypted, ss_id, slice_db_id) in
        &live_slices
    {
        let pos: i32 = pos_str.parse().unwrap_or(0);
        tape.rewind()?;
        if pos > 0 {
            tape.forward_space_file(pos)?;
        }

        let data = tape.read_file()?;
        let trimmed = if (*enc_bytes as usize) < data.len() {
            &data[..*enc_bytes as usize]
        } else {
            &data
        };

        let actual_sha = sha256_hex(trimmed);
        if actual_sha != *sha_on_vol && actual_sha != *sha_encrypted {
            warn!(
                position = pos,
                slice_id = slice_db_id,
                "checksum mismatch — skipping slice"
            );
            slices_skipped += 1;
            continue;
        }

        let slice_path = compact_dir.join(format!("slice_{slice_db_id}.dat"));
        fs::write(&slice_path, trimmed)?;

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
) -> Result<()> {
    // The normal volume_write picks up all staged data
    volume_write(conn, paths, config, dest_label, device, block_size)
}

/// Compact-finish: retire the source volume after compaction.
/// Refuses if any live slice on this volume has no copy on another volume.
pub fn compact_finish(conn: &Connection, label: &str) -> Result<()> {
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

    // Retire volume + update cartridge atomically
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE volumes SET status = 'retired' WHERE id = ?1",
        params![vol_id],
    )?;

    // Mark cartridge as pending_erase if bound
    tx.execute(
        "UPDATE cartridges SET status = 'pending_erase'
         WHERE id IN (SELECT cartridge_id FROM cartridge_volumes
                      WHERE volume_id = ?1 AND unmounted_at IS NULL)",
        params![vol_id],
    )?;

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
    Ok(())
}

// ── Internal helpers ──

#[derive(Debug, Default)]
pub struct VerifyReport {
    pub checked: usize,
    pub passed: usize,
    pub failed: usize,
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

fn sha256_hex(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Evidence, Mismatch, MismatchKind};

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
}
