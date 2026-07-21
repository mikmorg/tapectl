use std::collections::HashMap;
use std::fs;

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::config::{Config, TapectlPaths};
use crate::db::{events, queries};
use crate::error::{Result, TapectlError};
use crate::staging;
use crate::tape::health;
use crate::tape::ioctl::TapeDevice;

use crate::store::{Store, TapeStore};

use super::layout;
use super::layout_model::{ContentSource, LayoutEntry, ZoneKind};

/// A generated (non-slice) zone entry for the Layout enumeration.
fn gen_entry(position: i32, kind: ZoneKind, size: usize) -> LayoutEntry {
    LayoutEntry {
        position,
        kind,
        size_bytes: Some(size as u64),
        sha256: None,
        source: ContentSource::Generated,
    }
}

/// Map the Layout enumeration to the mini-index generator's input. The
/// mini-index is generated from this complete list — every file including the
/// envelopes — so an heir can trim block padding on any of them (fixes H1).
fn mini_index_tuples(entries: &[LayoutEntry]) -> Vec<(i32, &'static str, usize)> {
    entries
        .iter()
        .map(|e| {
            (
                e.position,
                e.kind.type_label(),
                e.size_bytes.unwrap_or(0) as usize,
            )
        })
        .collect()
}

/// Initialize a volume: create DB record and write ID thunk to tape.
pub fn volume_init(
    conn: &Connection,
    config: &Config,
    label: &str,
    device: &str,
    block_size: usize,
) -> Result<i64> {
    // Check label not already used
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

    // Determine backend info from config
    let backend = config
        .backends
        .lto
        .first()
        .ok_or_else(|| TapectlError::Config("no LTO backend configured".into()))?;

    let nominal_capacity = staging::parse_size_to_bytes(&backend.nominal_capacity);
    let media_type = &backend.media_type;

    // Insert volume record
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES (?1, 'lto', ?2, ?3, ?4, 'initialized')",
        params![label, backend.name, media_type, nominal_capacity],
    )?;
    let volume_id = conn.last_insert_rowid();

    // Write ID thunk to tape through the store seam (ADR-0006).
    let mut store = TapeStore::open(device, block_size)?;

    let id_thunk = layout::generate_id_thunk(
        label,
        media_type,
        env!("CARGO_PKG_VERSION"),
        "lto",
        nominal_capacity,
        0, // mam_capacity — filled on write
        4, // data_start (placeholder)
        4, // data_end (placeholder)
        5, // mini_index (placeholder)
        6, // first_envelope (placeholder)
        0, // num_envelopes (placeholder)
        6, // op_envelope (placeholder)
        7, // op_backup (placeholder)
        8, // total_files (placeholder)
        "",
        "",
        0,
        0,
    );

    store.execute(id_thunk.as_bytes(), false)?;
    info!(label = label, "volume initialized");

    events::log_created(conn, "volume", volume_id, label, None)?;
    Ok(volume_id)
}

/// Full volume write pipeline.
pub fn volume_write(
    conn: &Connection,
    _paths: &TapectlPaths,
    config: &Config,
    label: &str,
    device: &str,
    block_size: usize,
) -> Result<()> {
    // Look up volume
    let volume_id: i64 = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            params![label],
            |row| row.get(0),
        )
        .map_err(|_| TapectlError::VolumeNotFound(label.to_string()))?;

    // Find staged data to write
    let mut staged = find_staged_data(conn)?;
    if staged.is_empty() {
        return Err(TapectlError::Other(
            "no staged data to write — run `tapectl stage create` first".into(),
        ));
    }

    // Pre-write capacity gate (§2.8): refuse if the staged data will not fit,
    // rather than writing past end-of-tape and silently producing an incomplete
    // copy (the failure the #8 dry-run reproduced — the write reported success
    // with dead slices and the snapshot marked current). mhvtl reports
    // non-physical MAM capacity, so gate on the configured nominal capacity,
    // which is reliable; MAM is populated below for the record only.
    {
        let capacity_bytes: i64 = conn
            .query_row(
                "SELECT capacity_bytes FROM volumes WHERE id = ?1",
                params![volume_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let (reserve, block) = config
            .backends
            .lto
            .first()
            .map(|b| {
                (
                    staging::parse_size_to_bytes(&b.manifest_reserve)
                        + staging::parse_size_to_bytes(&b.enospc_buffer),
                    block_size as u64,
                )
            })
            .unwrap_or((0, block_size as u64));
        let staged_on_tape: i64 = staged
            .iter()
            .flat_map(|u| &u.slices)
            .map(|s| {
                super::layout_model::pad_to_blocks(s.encrypted_bytes.max(0) as u64, block) as i64
            })
            .sum();
        if capacity_bytes > 0 && staged_on_tape + reserve > capacity_bytes {
            return Err(TapectlError::Other(format!(
                "staged data ({} MB, block-padded) + reserve ({} MB) exceeds volume \"{label}\" \
                 capacity ({} MB) — it will not fit; use a larger tape or split the write",
                staged_on_tape / (1024 * 1024),
                reserve / (1024 * 1024),
                capacity_bytes / (1024 * 1024),
            )));
        }
    }

    // Create write records and assign write_ids to staged units
    let mut write_ids: Vec<(i64, i64)> = Vec::new();
    for ss in &mut staged {
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'planned')",
            params![ss.stage_set_id, ss.snapshot_id, volume_id],
        )?;
        let write_id = conn.last_insert_rowid();
        write_ids.push((write_id, ss.snapshot_id));
        // Update all slices with the correct write_id
        for slice in &mut ss.slices {
            slice.write_id = write_id;
        }
    }

    info!(
        slices = staged.iter().map(|s| s.slices.len()).sum::<usize>(),
        units = staged.len(),
        "writing to volume {label}"
    );

    // Open the store (rewinds to BOT, disables compression) — writes go through
    // the ADR-0006 seam.
    let mut store = TapeStore::open(device, block_size)?;

    // Record the cartridge's MAM (informational; mhvtl reports non-physical
    // values). Best-effort — a read failure never blocks the write.
    if let Some(bk) = config.backends.lto.first() {
        match crate::tape::mam::read_mam(&bk.device_sg) {
            Ok(mam) => {
                let _ = conn.execute(
                    "UPDATE volumes SET mam_capacity_bytes = ?1, mam_remaining_at_start = ?2
                     WHERE id = ?3",
                    params![mam.max_capacity_bytes, mam.remaining_bytes, volume_id],
                );
            }
            Err(e) => warn!(err = %e, "MAM read failed (continuing)"),
        }
    }

    // Collect all slices in order for writing
    let all_slices: Vec<&SliceInfo> = staged.iter().flat_map(|s| &s.slices).collect();
    let total_slices = all_slices.len();

    // Compute file positions
    let data_start = 4i32; // files 0-3 are metadata
    let data_end = data_start + total_slices as i32 - 1;
    let mini_index_pos = data_end + 1;

    // Count unique tenants
    let tenant_ids: Vec<i64> = staged.iter().map(|s| s.tenant_id).collect();
    let unique_tenants: Vec<i64> = {
        let mut t = tenant_ids.clone();
        t.sort();
        t.dedup();
        t
    };
    let num_tenant_envelopes = unique_tenants.len() as i32;
    let first_envelope_pos = mini_index_pos + 1;
    let op_envelope_pos = first_envelope_pos + num_tenant_envelopes;
    let op_backup_pos = op_envelope_pos + 1;
    let total_files = op_backup_pos + 1;

    // == File 0: ID thunk ==
    let backend = config.backends.lto.first().unwrap();
    let id_thunk = layout::generate_id_thunk(
        label,
        &backend.media_type,
        env!("CARGO_PKG_VERSION"),
        "lto",
        staging::parse_size_to_bytes(&backend.nominal_capacity),
        0,
        data_start,
        data_end,
        mini_index_pos,
        first_envelope_pos,
        num_tenant_envelopes,
        op_envelope_pos,
        op_backup_pos,
        total_files,
        "",
        "",
        0,
        0,
    );
    store.execute(id_thunk.as_bytes(), false)?;
    info!("wrote file 0: ID thunk");

    // == File 1: System guide ==
    let guide = layout::generate_system_guide(label, total_files);
    store.execute(guide.as_bytes(), false)?;
    info!("wrote file 1: system guide");

    // == File 2: RESTORE.sh ==
    let script = layout::generate_restore_script(label, total_files);
    store.execute(script.as_bytes(), false)?;
    info!("wrote file 2: RESTORE.sh");

    // == File 3: Planning header (encrypted to operator) ==
    let operator = queries::get_operator_tenant(conn)?
        .ok_or_else(|| TapectlError::Other("no operator".into()))?;
    let op_keys = queries::get_active_keys_for_tenant(conn, operator.id)?;
    let op_pubkeys: Vec<String> = op_keys.iter().map(|k| k.public_key.clone()).collect();

    let plan_units: Vec<(String, String, i64, i64)> = staged
        .iter()
        .map(|s| {
            (
                s.unit_name.clone(),
                s.unit_uuid.clone(),
                s.slices.len() as i64,
                s.slices.iter().map(|sl| sl.encrypted_bytes).sum(),
            )
        })
        .collect();
    let planning = layout::generate_planning_header(label, &plan_units);
    let planning_enc = staging::encrypt_data(planning.as_bytes(), &op_pubkeys)?;
    store.execute(&planning_enc, false)?;
    info!("wrote file 3: planning header");

    // Update write status
    for (write_id, _) in &write_ids {
        conn.execute(
            "UPDATE writes SET status = 'in_progress', started_at = datetime('now')
             WHERE id = ?1",
            params![write_id],
        )?;
    }

    // == Files 4..N: Data slices ==
    // ADR-0002: all on-tape metadata (mini-index, envelope manifests) is
    // generated from the complete Layout below, never from write order. So we
    // record every file as a LayoutEntry as it is produced, encrypt the
    // envelopes *before* the mini-index (to fix their sizes), then generate the
    // mini-index from the full enumeration — which now includes the envelopes
    // (fixes H1: the old mini-index was generated before the envelope entries
    // existed, so RESTORE.sh could never trim their block padding).
    let mut entries: Vec<LayoutEntry> = vec![
        gen_entry(0, ZoneKind::IdThunk, id_thunk.len()),
        gen_entry(1, ZoneKind::SystemGuide, guide.len()),
        gen_entry(2, ZoneKind::RestoreSh, script.len()),
        gen_entry(3, ZoneKind::PlanningHeader, planning_enc.len()),
    ];

    let mut bytes_written: i64 = 0;
    let mut slice_write_map: HashMap<i64, (i64, i32)> = HashMap::new(); // slice_id -> (write_id, tape_pos)

    for (i, slice) in all_slices.iter().enumerate() {
        // Check SIGINT
        if crate::signal::is_interrupted() {
            warn!("interrupted by signal — writing metadata and stopping");
            break;
        }

        let tape_pos = data_start + i as i32;

        // Read encrypted slice from staging
        let slice_data = fs::read(&slice.staging_path).map_err(|e| {
            TapectlError::Other(format!("read staged slice {}: {e}", slice.staging_path))
        })?;

        // Verify checksum
        let actual_hash = sha256_hex(&slice_data);
        if actual_hash != slice.sha256_encrypted {
            return Err(TapectlError::Other(format!(
                "slice {} checksum mismatch: expected {}, got {}",
                slice.slice_id,
                &slice.sha256_encrypted[..16],
                &actual_hash[..16],
            )));
        }

        // Write to tape
        let written = store.execute(&slice_data, false)?;
        bytes_written += written as i64;

        entries.push(LayoutEntry {
            position: tape_pos,
            kind: ZoneKind::Slice {
                stage_slice_id: slice.slice_id,
            },
            size_bytes: Some(slice_data.len() as u64),
            sha256: Some(actual_hash.clone()),
            source: ContentSource::Staged(slice.staging_path.clone().into()),
        });
        slice_write_map.insert(slice.slice_id, (slice.write_id, tape_pos));

        // Record write position
        conn.execute(
            "INSERT INTO write_positions (write_id, stage_slice_id, position, status, written_at, sha256_on_volume)
             VALUES (?1, ?2, ?3, 'written', datetime('now'), ?4)",
            params![slice.write_id, slice.slice_id, tape_pos.to_string(), actual_hash],
        )?;

        info!(
            slice = i + 1,
            total = total_slices,
            mb = slice_data.len() / (1024 * 1024),
            "wrote data slice"
        );
    }

    // == Encrypt all envelopes up front (fixes their sizes for the mini-index) ==
    // (position, sync_mark, encrypted_bytes) — written after the mini-index, in
    // position order.
    let mut envelopes: Vec<(i32, bool, Vec<u8>)> = Vec::new();

    for (env_idx, &tenant_id) in unique_tenants.iter().enumerate() {
        let tenant = queries::get_tenant_by_id(conn, tenant_id)?
            .ok_or_else(|| TapectlError::Other("tenant not found".into()))?;
        let tenant_keys = queries::get_active_keys_for_tenant(conn, tenant_id)?;
        let mut all_keys: Vec<String> = tenant_keys.iter().map(|k| k.public_key.clone()).collect();
        all_keys.extend(op_pubkeys.iter().cloned());

        let manifest_units = build_manifest_units(&staged, tenant_id, &slice_write_map);
        let manifest = layout::generate_manifest_toml(label, &tenant.name, &manifest_units);
        let recovery = layout::generate_recovery_md(label, &tenant.name, &manifest_units);

        // Build tar archive with MANIFEST.toml + RECOVERY.md
        let catalogs: Vec<(String, Vec<u8>)> = staged
            .iter()
            .filter(|s| s.tenant_id == tenant_id)
            .filter_map(|s| s.catalog_path.as_deref())
            .flat_map(read_catalog_files)
            .collect();
        let tar_data = build_envelope_tar(&manifest, &recovery, &catalogs)?;
        let encrypted = staging::encrypt_data(&tar_data, &all_keys)?;

        let env_pos = first_envelope_pos + env_idx as i32;
        entries.push(LayoutEntry {
            position: env_pos,
            kind: ZoneKind::TenantEnvelope { tenant_id },
            size_bytes: Some(encrypted.len() as u64),
            sha256: None,
            source: ContentSource::Generated,
        });
        envelopes.push((env_pos, false, encrypted));
    }

    let all_manifest_units = build_manifest_units_all(&staged, &slice_write_map);
    let op_manifest = layout::generate_manifest_toml(label, "operator", &all_manifest_units);
    let op_recovery = layout::generate_recovery_md(label, "operator", &all_manifest_units);
    let all_catalogs: Vec<(String, Vec<u8>)> = staged
        .iter()
        .filter_map(|s| s.catalog_path.as_deref())
        .flat_map(read_catalog_files)
        .collect();
    let op_tar = build_envelope_tar(&op_manifest, &op_recovery, &all_catalogs)?;
    let op_env_encrypted = staging::encrypt_data(&op_tar, &op_pubkeys)?;
    entries.push(LayoutEntry {
        position: op_envelope_pos,
        kind: ZoneKind::OperatorEnvelope,
        size_bytes: Some(op_env_encrypted.len() as u64),
        sha256: None,
        source: ContentSource::Generated,
    });
    entries.push(LayoutEntry {
        position: op_backup_pos,
        kind: ZoneKind::OperatorEnvelopeBackup,
        size_bytes: Some(op_env_encrypted.len() as u64),
        sha256: None,
        source: ContentSource::Generated,
    });
    envelopes.push((op_envelope_pos, true, op_env_encrypted.clone()));
    envelopes.push((op_backup_pos, true, op_env_encrypted));

    // == File N+1: Mini-index, generated from the complete Layout ==
    // Include the mini-index's own entry, sized by a two-pass. Its self-size is
    // informational (RESTORE.sh reads the mini-index by filemark, not by size);
    // envelope and slice sizes — which the two-pass leaves exact — are what an
    // heir consumes to trim block padding.
    entries.push(gen_entry(mini_index_pos, ZoneKind::MiniIndex, 0));
    entries.sort_by_key(|e| e.position);
    let mini_len = layout::generate_mini_index(label, &mini_index_tuples(&entries)).len();
    if let Some(mi) = entries.iter_mut().find(|e| e.kind == ZoneKind::MiniIndex) {
        mi.size_bytes = Some(mini_len as u64);
    }
    let mini = layout::generate_mini_index(label, &mini_index_tuples(&entries));
    store.execute(mini.as_bytes(), false)?;
    info!("wrote mini-index");

    // == Write the pre-encrypted envelopes, in position order ==
    for (pos, sync_mark, bytes) in &envelopes {
        store.execute(bytes, *sync_mark)?;
        info!(position = pos, "wrote envelope");
    }

    // Update DB atomically — all status changes commit together
    {
        let tx = conn.unchecked_transaction()?;
        for (write_id, snapshot_id) in &write_ids {
            tx.execute(
                "UPDATE writes SET status = 'completed', completed_at = datetime('now')
                 WHERE id = ?1",
                params![write_id],
            )?;
            tx.execute(
                "UPDATE snapshots SET status = 'current'
                 WHERE id = ?1 AND status IN ('created', 'staged')",
                params![snapshot_id],
            )?;
        }

        tx.execute(
            "UPDATE volumes SET status = 'active', bytes_written = ?1,
             num_data_files = ?2, has_manifest = 1,
             first_write = COALESCE(first_write, datetime('now')),
             last_write = datetime('now')
             WHERE id = ?3",
            params![bytes_written, total_slices as i64, volume_id],
        )?;

        events::log_event(
            &tx,
            "volume",
            volume_id,
            Some(label),
            "write_completed",
            None,
            None,
            Some(&format!("{total_slices} slices")),
            None,
            None,
        )?;
        tx.commit()?;
    }

    info!(
        label = label,
        slices = total_slices,
        bytes = bytes_written,
        "volume write complete"
    );

    // Best-effort sg_logs health collection. Never abort the write.
    // Resolve sg device from the configured LTO backend matching this tape device.
    let sg_device = config
        .backends
        .lto
        .iter()
        .find(|b| b.device_tape == device)
        .map(|b| b.device_sg.clone());
    if let Some(sg) = sg_device {
        match health::collect(&sg) {
            Ok((counters, raw)) => {
                if let Err(e) = health::record(conn, volume_id, "write", &counters, &raw) {
                    warn!(err = %e, "health_logs insert failed");
                }
            }
            Err(e) => warn!(sg_device = sg, err = %e, "sg_logs collection failed"),
        }
    }

    Ok(())
}

/// Verify a volume by reading slices and checking checksums.
pub fn volume_verify(
    conn: &Connection,
    config: &Config,
    label: &str,
    device: &str,
    block_size: usize,
) -> Result<VerifyReport> {
    let volume_id: i64 = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            params![label],
            |row| row.get(0),
        )
        .map_err(|_| TapectlError::VolumeNotFound(label.to_string()))?;

    // Get write positions
    let mut stmt = conn.prepare(
        "SELECT wp.id, wp.position, wp.sha256_on_volume, ss.sha256_encrypted, wp.stage_slice_id, ss.encrypted_bytes
         FROM write_positions wp
         JOIN stage_slices ss ON ss.id = wp.stage_slice_id
         JOIN writes w ON w.id = wp.write_id
         WHERE w.volume_id = ?1 AND wp.status = 'written'
         ORDER BY CAST(wp.position AS INTEGER)",
    )?;
    let positions: Vec<(i64, String, String, String, i64, i64)> = stmt
        .query_map(params![volume_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    if positions.is_empty() {
        return Err(TapectlError::Other("no write positions found".into()));
    }

    // Create verification session
    conn.execute(
        "INSERT INTO verification_sessions (volume_id, verify_type, outcome)
         VALUES (?1, 'full', 'in_progress')",
        params![volume_id],
    )?;
    let session_id = conn.last_insert_rowid();

    let mut tape = TapeDevice::open_read(device, block_size)?;
    tape.rewind()?;

    let mut report = VerifyReport::default();

    for (wp_id, pos_str, expected_hash, orig_hash, slice_id, orig_size) in &positions {
        let pos: i32 = pos_str.parse().unwrap_or(0);

        // Seek to position
        tape.rewind()?;
        if pos > 0 {
            tape.forward_space_file(pos)?;
        }

        let data = tape.read_file()?;
        // In fixed block mode, data may be padded — trim to original size
        let trimmed = if (*orig_size as usize) < data.len() {
            &data[..*orig_size as usize]
        } else {
            &data
        };
        let actual = sha256_hex(trimmed);

        report.checked += 1;
        if actual == *expected_hash || actual == *orig_hash {
            report.passed += 1;
            conn.execute(
                "INSERT INTO verification_results (session_id, write_position_id, stage_slice_id, result, expected_sha256, actual_sha256)
                 VALUES (?1, ?2, ?3, 'passed', ?4, ?5)",
                params![session_id, wp_id, slice_id, expected_hash, actual],
            )?;
            info!(slice_id = slice_id, position = pos, "PASS");
        } else {
            report.failed += 1;
            conn.execute(
                "INSERT INTO verification_results (session_id, write_position_id, stage_slice_id, result, expected_sha256, actual_sha256)
                 VALUES (?1, ?2, ?3, 'failed_checksum', ?4, ?5)",
                params![session_id, wp_id, slice_id, expected_hash, actual],
            )?;
            warn!(
                slice_id = slice_id,
                position = pos,
                expected = &expected_hash[..16],
                actual = &actual[..16],
                "FAIL — checksum mismatch"
            );
        }
    }

    // Finalize verification session
    let outcome = if report.failed == 0 {
        "passed"
    } else {
        "failed"
    };
    conn.execute(
        "UPDATE verification_sessions
         SET completed_at = datetime('now'), outcome = ?1,
             slices_checked = ?2, slices_passed = ?3, slices_failed = ?4
         WHERE id = ?5",
        params![
            outcome,
            report.checked as i64,
            report.passed as i64,
            report.failed as i64,
            session_id
        ],
    )?;

    // Best-effort sg_logs collection after verify. Advisory only.
    let sg_device = config
        .backends
        .lto
        .iter()
        .find(|b| b.device_tape == device)
        .map(|b| b.device_sg.clone());
    if let Some(sg) = sg_device {
        if let Ok((counters, raw)) = health::collect(&sg) {
            if let Err(e) = health::record(conn, volume_id, "verify", &counters, &raw) {
                warn!(err = %e, "health_logs insert failed");
            }
        }
    }

    Ok(report)
}

/// Read and display the ID thunk from a tape.
pub fn volume_identify(device: &str, block_size: usize) -> Result<String> {
    let mut tape = TapeDevice::open_read(device, block_size)?;
    tape.rewind()?;
    let data = tape.read_file()?;
    let text = String::from_utf8_lossy(&data).to_string();
    // Trim padding zeros
    Ok(text.trim_end_matches('\0').to_string())
}

/// Read encrypted slices for a unit from a volume into staging.
/// After this, use `volume write` to write them to a destination tape
/// with the full self-describing 10-file layout.
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
    let mut affected_stage_sets = std::collections::HashSet::new();

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
    let mut affected_stage_sets = std::collections::HashSet::new();

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

struct StagedUnit {
    stage_set_id: i64,
    snapshot_id: i64,
    unit_name: String,
    unit_uuid: String,
    tenant_id: i64,
    dar_version: Option<String>,
    dar_command: Option<String>,
    catalog_path: Option<String>,
    snapshot_version: i64,
    slices: Vec<SliceInfo>,
}

struct SliceInfo {
    slice_id: i64,
    write_id: i64,
    slice_number: i64,
    size_bytes: i64,
    encrypted_bytes: i64,
    sha256_plain: String,
    sha256_encrypted: String,
    staging_path: String,
}

#[derive(Debug, Default)]
pub struct VerifyReport {
    pub checked: usize,
    pub passed: usize,
    pub failed: usize,
}

fn find_staged_data(conn: &Connection) -> Result<Vec<StagedUnit>> {
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
        // Get write_id for this stage_set (created in the caller — check if exists)
        let write_id: i64 = conn
            .query_row(
                "SELECT id FROM writes WHERE stage_set_id = ?1 ORDER BY id DESC LIMIT 1",
                params![ss_id],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let mut slice_stmt = conn.prepare(
            "SELECT id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted, staging_path
             FROM stage_slices WHERE stage_set_id = ?1 AND staging_path IS NOT NULL
             ORDER BY slice_number",
        )?;
        let slices: Vec<SliceInfo> = slice_stmt
            .query_map(params![ss_id], |row| {
                Ok(SliceInfo {
                    slice_id: row.get(0)?,
                    write_id,
                    slice_number: row.get(1)?,
                    size_bytes: row.get(2)?,
                    encrypted_bytes: row.get(3)?,
                    sha256_plain: row.get(4)?,
                    sha256_encrypted: row.get(5)?,
                    staging_path: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        if !slices.is_empty() {
            units.push(StagedUnit {
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

fn build_manifest_units(
    staged: &[StagedUnit],
    tenant_id: i64,
    positions: &HashMap<i64, (i64, i32)>,
) -> Vec<layout::ManifestUnit> {
    staged
        .iter()
        .filter(|s| s.tenant_id == tenant_id)
        .map(|s| layout::ManifestUnit {
            name: s.unit_name.clone(),
            uuid: s.unit_uuid.clone(),
            snapshot_version: s.snapshot_version,
            stage_set_id: s.stage_set_id,
            dar_version: s.dar_version.clone(),
            dar_command: s.dar_command.clone(),
            slices: s
                .slices
                .iter()
                .map(|sl| {
                    let (_, tape_pos) = positions.get(&sl.slice_id).copied().unwrap_or((0, 0));
                    layout::ManifestSlice {
                        number: sl.slice_number,
                        tape_position: tape_pos,
                        size_bytes: sl.size_bytes,
                        encrypted_bytes: sl.encrypted_bytes,
                        sha256_plain: sl.sha256_plain.clone(),
                        sha256_encrypted: sl.sha256_encrypted.clone(),
                    }
                })
                .collect(),
        })
        .collect()
}

fn build_manifest_units_all(
    staged: &[StagedUnit],
    positions: &HashMap<i64, (i64, i32)>,
) -> Vec<layout::ManifestUnit> {
    staged
        .iter()
        .map(|s| layout::ManifestUnit {
            name: s.unit_name.clone(),
            uuid: s.unit_uuid.clone(),
            snapshot_version: s.snapshot_version,
            stage_set_id: s.stage_set_id,
            dar_version: s.dar_version.clone(),
            dar_command: s.dar_command.clone(),
            slices: s
                .slices
                .iter()
                .map(|sl| {
                    let (_, tape_pos) = positions.get(&sl.slice_id).copied().unwrap_or((0, 0));
                    layout::ManifestSlice {
                        number: sl.slice_number,
                        tape_position: tape_pos,
                        size_bytes: sl.size_bytes,
                        encrypted_bytes: sl.encrypted_bytes,
                        sha256_plain: sl.sha256_plain.clone(),
                        sha256_encrypted: sl.sha256_encrypted.clone(),
                    }
                })
                .collect(),
        })
        .collect()
}

/// Read a unit's isolated dar catalogue slice files (catalog_base.N.dar) for
/// inclusion in an envelope. Best-effort: a missing catalogue yields no files.
fn read_catalog_files(catalog_path: &str) -> Vec<(String, Vec<u8>)> {
    let base = std::path::Path::new(catalog_path);
    let (Some(dir), Some(stem)) = (base.parent(), base.file_name().and_then(|f| f.to_str())) else {
        return Vec::new();
    };
    let prefix = format!("{stem}.");
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let fname = e.file_name().to_string_lossy().into_owned();
            if fname.starts_with(&prefix) && fname.ends_with(".dar") {
                if let Ok(bytes) = fs::read(e.path()) {
                    out.push((fname, bytes));
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn build_envelope_tar(
    manifest: &str,
    recovery: &str,
    catalogs: &[(String, Vec<u8>)],
) -> Result<Vec<u8>> {
    let mut tar_buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);

        let manifest_bytes = manifest.as_bytes();
        let mut header = tar::Header::new_gnu();
        header.set_path("MANIFEST.toml").unwrap();
        header.set_size(manifest_bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(chrono::Utc::now().timestamp() as u64);
        header.set_cksum();
        builder.append(&header, manifest_bytes).unwrap();

        let recovery_bytes = recovery.as_bytes();
        let mut header = tar::Header::new_gnu();
        header.set_path("RECOVERY.md").unwrap();
        header.set_size(recovery_bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(chrono::Utc::now().timestamp() as u64);
        header.set_cksum();
        builder.append(&header, recovery_bytes).unwrap();

        // Per-unit isolated dar catalogues under catalogs/ — an heir can list
        // contents (`dar -l`) and selectively restore without the database (#39).
        for (name, bytes) in catalogs {
            let mut header = tar::Header::new_gnu();
            header.set_path(format!("catalogs/{name}")).unwrap();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(chrono::Utc::now().timestamp() as u64);
            header.set_cksum();
            builder.append(&header, bytes.as_slice()).unwrap();
        }

        builder.finish().unwrap();
    }
    Ok(tar_buf)
}

fn sha256_hex(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_tar_includes_catalogues() {
        // #39: per-unit dar catalogues ride the envelope under catalogs/.
        let cats = vec![("abcd_v1.1.dar".to_string(), b"catalogue-bytes".to_vec())];
        let tar = build_envelope_tar("[manifest]\n", "# recovery\n", &cats).unwrap();
        let mut ar = tar::Archive::new(std::io::Cursor::new(tar));
        let names: Vec<String> = ar
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"MANIFEST.toml".to_string()));
        assert!(names.contains(&"RECOVERY.md".to_string()));
        assert!(
            names.contains(&"catalogs/abcd_v1.1.dar".to_string()),
            "catalogue missing from envelope: {names:?}"
        );
    }
}
