use std::fs;
use std::path::Path;

use rusqlite::{params, Connection};
use tracing::info;

use crate::config::{Config, TapectlPaths};
use crate::db::{events, queries};
use crate::error::{Result, TapectlError};

/// Purge a reclaimable snapshot (remove files/manifests, mark purged).
pub fn snapshot_purge(
    conn: &Connection,
    unit_name: &str,
    version: i64,
    json_output: bool,
) -> Result<()> {
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;

    let (snap_id, status): (i64, String) = conn
        .query_row(
            "SELECT id, status FROM snapshots WHERE unit_id = ?1 AND version = ?2",
            params![unit.id, version],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| {
            TapectlError::Other(format!("snapshot v{version} not found for \"{unit_name}\""))
        })?;

    if status != "reclaimable" {
        return Err(TapectlError::Other(format!(
            "snapshot v{version} status is \"{status}\", must be \"reclaimable\" to purge"
        )));
    }

    // Delete files and manifests but keep the snapshot row as 'purged'
    conn.execute(
        "DELETE FROM manifest_entries WHERE manifest_id IN
         (SELECT id FROM manifests WHERE snapshot_id = ?1)",
        params![snap_id],
    )?;
    conn.execute(
        "DELETE FROM manifests WHERE snapshot_id = ?1",
        params![snap_id],
    )?;
    conn.execute("DELETE FROM files WHERE snapshot_id = ?1", params![snap_id])?;
    conn.execute(
        "UPDATE snapshots SET status = 'purged' WHERE id = ?1",
        params![snap_id],
    )?;

    events::log_field_change(
        conn,
        "snapshot",
        snap_id,
        &format!("{unit_name}/v{version}"),
        "purged",
        "status",
        Some("reclaimable"),
        "purged",
        Some(unit.tenant_id),
    )?;

    if json_output {
        println!(
            "{}",
            serde_json::json!({"unit": unit_name, "version": version, "status": "purged"})
        );
    } else {
        println!("snapshot {unit_name} v{version} purged");
    }
    Ok(())
}

/// Check unit integrity: compare disk files against staged checksums.
pub fn unit_check_integrity(conn: &Connection, unit_name: &str, json_output: bool) -> Result<()> {
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;

    let current_path = unit
        .current_path
        .as_deref()
        .ok_or_else(|| TapectlError::Other("unit has no current path".into()))?;

    // Get latest staged files with sha256
    let mut stmt = conn.prepare(
        "SELECT f.path, f.size_bytes, f.sha256
         FROM files f
         JOIN snapshots s ON s.id = f.snapshot_id
         WHERE s.unit_id = ?1 AND s.status IN ('current', 'staged', 'created')
           AND f.is_directory = 0 AND f.sha256 IS NOT NULL
         ORDER BY s.version DESC",
    )?;
    let staged_files: Vec<(String, i64, String)> = stmt
        .query_map(params![unit.id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    if staged_files.is_empty() {
        return Err(TapectlError::Other(format!(
            "no staged files with checksums for \"{unit_name}\" — stage at least once first"
        )));
    }

    let mut ok = 0i64;
    let mut bitrot = 0i64;
    let mut missing = 0i64;
    let mut size_mismatch = 0i64;
    let mut details: Vec<serde_json::Value> = Vec::new();

    for (rel_path, expected_size, expected_sha) in &staged_files {
        let full_path = Path::new(current_path).join(rel_path);
        if !full_path.exists() {
            missing += 1;
            details.push(serde_json::json!({"path": rel_path, "status": "MISSING"}));
            continue;
        }
        let meta = fs::metadata(&full_path)?;
        if meta.len() as i64 != *expected_size {
            size_mismatch += 1;
            details.push(serde_json::json!({
                "path": rel_path, "status": "SIZE_MISMATCH",
                "expected": expected_size, "actual": meta.len(),
            }));
            continue;
        }
        // SHA256 check
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        let data = fs::read(&full_path)?;
        hasher.update(&data);
        let actual: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if actual != *expected_sha {
            bitrot += 1;
            details.push(serde_json::json!({"path": rel_path, "status": "BITROT"}));
        } else {
            ok += 1;
        }
    }

    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "unit": unit_name, "ok": ok, "bitrot": bitrot,
                "missing": missing, "size_mismatch": size_mismatch,
                "details": details,
            })
        );
    } else {
        println!("integrity check for \"{unit_name}\":");
        println!("  OK:            {ok}");
        if bitrot > 0 {
            println!("  BITROT:        {bitrot}");
        }
        if missing > 0 {
            println!("  MISSING:       {missing}");
        }
        if size_mismatch > 0 {
            println!("  SIZE_MISMATCH: {size_mismatch}");
        }
        for d in &details {
            println!(
                "    {} — {}",
                d["path"].as_str().unwrap_or("?"),
                d["status"].as_str().unwrap_or("?")
            );
        }
    }
    Ok(())
}

/// Retire a volume with impact analysis.
pub fn volume_retire(conn: &Connection, label: &str, json_output: bool) -> Result<()> {
    let (vol_id, status): (i64, String) = conn
        .query_row(
            "SELECT id, status FROM volumes WHERE label = ?1",
            params![label],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| TapectlError::VolumeNotFound(label.to_string()))?;

    // Impact analysis: find all units with data on this volume
    let mut stmt = conn.prepare(
        "SELECT DISTINCT u.name, u.status,
                (SELECT COUNT(DISTINCT w2.volume_id)
                 FROM writes w2
                 JOIN stage_sets ss2 ON ss2.id = w2.stage_set_id
                 JOIN snapshots s2 ON s2.id = ss2.snapshot_id
                 WHERE s2.unit_id = u.id AND w2.status = 'completed' AND w2.volume_id != ?1) as other_copies
         FROM units u
         JOIN snapshots s ON s.unit_id = u.id
         JOIN stage_sets ss ON ss.snapshot_id = s.id
         JOIN writes w ON w.stage_set_id = ss.id
         WHERE w.volume_id = ?1 AND w.status = 'completed'
         ORDER BY u.name",
    )?;

    let impacts: Vec<(String, String, i64)> = stmt
        .query_map(params![vol_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    if json_output {
        let json_impacts: Vec<serde_json::Value> = impacts
            .iter()
            .map(|(name, status, copies)| {
                serde_json::json!({"unit": name, "status": status, "remaining_copies": copies})
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({"volume": label, "affected_units": json_impacts})
        );
    } else {
        println!("Retiring volume \"{label}\"");
        println!("  Current status: {status}");
        println!("  Affected units:");
        let mut at_risk = 0;
        for (name, unit_status, other_copies) in &impacts {
            let warning = if *other_copies == 0 {
                at_risk += 1;
                " *** ZERO copies remaining! ***"
            } else {
                ""
            };
            println!("    {name} [{unit_status}]: {other_copies} other copy/copies{warning}");
        }
        if at_risk > 0 {
            println!("\n  WARNING: {at_risk} unit(s) will have ZERO copies after retirement!");
            println!("  Consider writing additional copies before retiring.");
        }
    }

    // Actually retire
    conn.execute(
        "UPDATE volumes SET status = 'retired' WHERE id = ?1",
        params![vol_id],
    )?;
    events::log_field_change(
        conn,
        "volume",
        vol_id,
        label,
        "retired",
        "status",
        Some(&status),
        "retired",
        None,
    )?;

    if !json_output {
        println!("  Volume \"{label}\" retired.");
    }
    Ok(())
}

/// Mark a unit as tape-only with enforcement.
pub fn unit_mark_tape_only(
    conn: &Connection,
    config: &Config,
    unit_name: &str,
    force: bool,
    json_output: bool,
) -> Result<()> {
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;

    let min_copies = config.defaults.min_copies_for_tape_only;
    let min_locations = config.defaults.min_locations_for_tape_only;

    // Count copies and locations
    let (copy_count, location_count): (i64, i64) = conn.query_row(
        "SELECT COUNT(DISTINCT w.id), COUNT(DISTINCT v.location_id)
         FROM snapshots s
         JOIN stage_sets ss ON ss.snapshot_id = s.id
         JOIN writes w ON w.stage_set_id = ss.id AND w.status = 'completed'
         JOIN volumes v ON v.id = w.volume_id
         WHERE s.unit_id = ?1 AND s.status = 'current'",
        params![unit.id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    if !force {
        if copy_count < min_copies as i64 {
            return Err(TapectlError::Other(format!(
                "insufficient copies: {copy_count} < {min_copies} required (use --force to override)"
            )));
        }
        if location_count < min_locations as i64 {
            return Err(TapectlError::Other(format!(
                "insufficient locations: {location_count} < {min_locations} required (use --force to override)"
            )));
        }
    }

    conn.execute(
        "UPDATE units SET status = 'tape_only' WHERE id = ?1",
        params![unit.id],
    )?;
    events::log_field_change(
        conn,
        "unit",
        unit.id,
        unit_name,
        "mark_tape_only",
        "status",
        Some(&unit.status),
        "tape_only",
        Some(unit.tenant_id),
    )?;

    if json_output {
        println!(
            "{}",
            serde_json::json!({"unit": unit_name, "status": "tape_only", "copies": copy_count, "locations": location_count})
        );
    } else {
        println!(
            "unit \"{unit_name}\" marked tape-only ({copy_count} copies, {location_count} locations)"
        );
    }
    Ok(())
}

/// Export encrypted slices to a directory.
pub fn export_unit(
    conn: &Connection,
    unit_name: &str,
    dest_dir: &str,
    json_output: bool,
) -> Result<()> {
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;

    // Find latest staged slices
    let mut stmt = conn.prepare(
        "SELECT sl.staging_path, sl.slice_number, sl.encrypted_bytes
         FROM stage_slices sl
         JOIN stage_sets ss ON ss.id = sl.stage_set_id
         JOIN snapshots s ON s.id = ss.snapshot_id
         WHERE s.unit_id = ?1 AND ss.status = 'staged' AND sl.staging_path IS NOT NULL
         ORDER BY sl.slice_number",
    )?;
    let slices: Vec<(String, i64, i64)> = stmt
        .query_map(params![unit.id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    if slices.is_empty() {
        return Err(TapectlError::Other(format!(
            "no staged slices for unit \"{unit_name}\" — run `tapectl stage create` first"
        )));
    }

    fs::create_dir_all(dest_dir)?;
    let mut total = 0i64;
    for (src, num, size) in &slices {
        let src_path = Path::new(src);
        let dest_file = Path::new(dest_dir).join(
            src_path
                .file_name()
                .unwrap_or(std::ffi::OsStr::new("slice.dar.age")),
        );
        fs::copy(src_path, &dest_file)?;
        total += size;
        info!(slice = num, dest = %dest_file.display(), "exported");
    }

    if json_output {
        println!(
            "{}",
            serde_json::json!({"unit": unit_name, "slices": slices.len(), "total_bytes": total, "destination": dest_dir})
        );
    } else {
        println!(
            "exported {} slices ({} MB) to {}",
            slices.len(),
            total / (1024 * 1024),
            dest_dir,
        );
    }
    Ok(())
}

/// Delete an unwritten snapshot.
pub fn snapshot_delete(
    conn: &Connection,
    unit_name: &str,
    version: i64,
    force: bool,
    json_output: bool,
) -> Result<()> {
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;

    let (snap_id, status): (i64, String) = conn
        .query_row(
            "SELECT id, status FROM snapshots WHERE unit_id = ?1 AND version = ?2",
            params![unit.id, version],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| {
            TapectlError::Other(format!("snapshot v{version} not found for \"{unit_name}\""))
        })?;

    // Check if snapshot has been written to tape
    let write_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM writes w
         JOIN stage_sets ss ON ss.id = w.stage_set_id
         WHERE ss.snapshot_id = ?1 AND w.status = 'completed'",
        params![snap_id],
        |row| row.get(0),
    )?;
    if write_count > 0 {
        return Err(TapectlError::Other(format!(
            "snapshot v{version} has {write_count} completed write(s) — cannot delete"
        )));
    }

    // Check if staged (allow with --force)
    let staged_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM stage_sets WHERE snapshot_id = ?1 AND status = 'staged'",
        params![snap_id],
        |row| row.get(0),
    )?;
    if staged_count > 0 && !force {
        return Err(TapectlError::Other(format!(
            "snapshot v{version} has staged data — use --force to delete anyway"
        )));
    }

    // Cascade delete: stage_slices -> stage_sets -> manifest_entries -> manifests -> files -> snapshot
    conn.execute(
        "DELETE FROM stage_slices WHERE stage_set_id IN
         (SELECT id FROM stage_sets WHERE snapshot_id = ?1)",
        params![snap_id],
    )?;
    conn.execute(
        "DELETE FROM stage_sets WHERE snapshot_id = ?1",
        params![snap_id],
    )?;
    conn.execute(
        "DELETE FROM manifest_entries WHERE manifest_id IN
         (SELECT id FROM manifests WHERE snapshot_id = ?1)",
        params![snap_id],
    )?;
    conn.execute(
        "DELETE FROM manifests WHERE snapshot_id = ?1",
        params![snap_id],
    )?;
    conn.execute("DELETE FROM files WHERE snapshot_id = ?1", params![snap_id])?;
    conn.execute("DELETE FROM snapshots WHERE id = ?1", params![snap_id])?;

    events::log_event(
        conn,
        "snapshot",
        snap_id,
        Some(&format!("{unit_name}/v{version}")),
        "deleted",
        None,
        None,
        None,
        None,
        Some(unit.tenant_id),
    )?;

    if json_output {
        println!(
            "{}",
            serde_json::json!({"unit": unit_name, "version": version, "deleted": true})
        );
    } else {
        println!("snapshot {unit_name} v{version} deleted (was: {status})");
    }
    Ok(())
}

/// Mark a snapshot as reclaimable with enforced preconditions.
pub fn snapshot_mark_reclaimable(
    conn: &Connection,
    config: &Config,
    unit_name: &str,
    version: i64,
    force: bool,
    json_output: bool,
) -> Result<()> {
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;

    let (snap_id, status): (i64, String) = conn
        .query_row(
            "SELECT id, status FROM snapshots WHERE unit_id = ?1 AND version = ?2",
            params![unit.id, version],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| {
            TapectlError::Other(format!("snapshot v{version} not found for \"{unit_name}\""))
        })?;

    if status == "reclaimable" {
        return Err(TapectlError::Other(format!(
            "snapshot v{version} is already reclaimable"
        )));
    }

    if !force {
        // Precondition 1: A superseding snapshot must exist and be current
        let superseding: Option<(i64, i64)> = conn
            .query_row(
                "SELECT id, version FROM snapshots
                 WHERE unit_id = ?1 AND version > ?2 AND status = 'current'
                 ORDER BY version DESC LIMIT 1",
                params![unit.id, version],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();

        let superseding = superseding.ok_or_else(|| {
            TapectlError::Other(format!(
                "no superseding current snapshot exists for v{version} (use --force to override)"
            ))
        })?;

        // Precondition 2: Superseding snapshot meets policy
        let resolved = crate::policy::resolve(conn, config, &unit);
        let mut required_copies = resolved.min_copies;
        let mut required_locations = resolved.required_locations.len() as i64;

        // Precondition 3: tape-only units get multiplied requirements
        if unit.status == "tape_only" {
            let multiplier = config.compaction.tape_only_safety_multiplier as i64;
            required_copies *= multiplier;
            required_locations *= multiplier;
        }

        let copy_count: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT w.volume_id)
             FROM writes w
             JOIN stage_sets ss ON ss.id = w.stage_set_id
             WHERE ss.snapshot_id = ?1 AND w.status = 'completed'",
            params![superseding.0],
            |row| row.get(0),
        )?;

        if copy_count < required_copies {
            return Err(TapectlError::Other(format!(
                "superseding v{} has {copy_count} copies, needs {required_copies}{} (use --force to override)",
                superseding.1,
                if unit.status == "tape_only" { " (tape-only 2x)" } else { "" }
            )));
        }

        let location_count: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT v.location_id)
             FROM writes w
             JOIN stage_sets ss ON ss.id = w.stage_set_id
             JOIN volumes v ON v.id = w.volume_id
             WHERE ss.snapshot_id = ?1 AND w.status = 'completed' AND v.location_id IS NOT NULL",
            params![superseding.0],
            |row| row.get(0),
        )?;

        if required_locations > 0 && location_count < required_locations {
            return Err(TapectlError::Other(format!(
                "superseding v{} in {location_count} locations, needs {required_locations} (use --force to override)",
                superseding.1,
            )));
        }
    }

    conn.execute(
        "UPDATE snapshots SET status = 'reclaimable' WHERE id = ?1",
        params![snap_id],
    )?;
    events::log_field_change(
        conn,
        "snapshot",
        snap_id,
        &format!("{unit_name}/v{version}"),
        "mark_reclaimable",
        "status",
        Some(&status),
        "reclaimable",
        Some(unit.tenant_id),
    )?;

    if json_output {
        println!(
            "{}",
            serde_json::json!({"unit": unit_name, "version": version, "status": "reclaimable"})
        );
    } else {
        println!("snapshot {unit_name} v{version} marked reclaimable (was: {status})");
    }
    Ok(())
}

/// Snapshot diff: compare two versions of a unit.
pub fn snapshot_diff(
    conn: &Connection,
    unit_name: &str,
    v1: i64,
    v2: i64,
    json_output: bool,
) -> Result<()> {
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;

    let snap1_id: i64 = conn
        .query_row(
            "SELECT id FROM snapshots WHERE unit_id = ?1 AND version = ?2",
            params![unit.id, v1],
            |row| row.get(0),
        )
        .map_err(|_| TapectlError::Other(format!("snapshot v{v1} not found")))?;

    let snap2_id: i64 = conn
        .query_row(
            "SELECT id FROM snapshots WHERE unit_id = ?1 AND version = ?2",
            params![unit.id, v2],
            |row| row.get(0),
        )
        .map_err(|_| TapectlError::Other(format!("snapshot v{v2} not found")))?;

    // Get files from both snapshots
    let files1 = get_file_map(conn, snap1_id)?;
    let files2 = get_file_map(conn, snap2_id)?;

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut modified = Vec::new();
    let mut unchanged = 0;

    for (path, (size2, hash2)) in &files2 {
        match files1.get(path) {
            None => added.push((path.clone(), *size2)),
            Some((size1, hash1)) => {
                if hash1 != hash2 || size1 != size2 {
                    modified.push((path.clone(), *size1, *size2));
                } else {
                    unchanged += 1;
                }
            }
        }
    }
    for path in files1.keys() {
        if !files2.contains_key(path) {
            removed.push((path.clone(), files1[path].0));
        }
    }

    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "unit": unit_name, "v1": v1, "v2": v2,
                "added": added.len(), "removed": removed.len(),
                "modified": modified.len(), "unchanged": unchanged,
            })
        );
    } else {
        println!("diff {} v{v1} → v{v2}:", unit_name);
        for (path, size) in &added {
            println!("  + {path} ({size} bytes)");
        }
        for (path, size) in &removed {
            println!("  - {path} ({size} bytes)");
        }
        for (path, old_size, new_size) in &modified {
            println!("  ~ {path} ({old_size} → {new_size} bytes)");
        }
        println!(
            "  {} added, {} removed, {} modified, {unchanged} unchanged",
            added.len(),
            removed.len(),
            modified.len(),
        );
    }
    Ok(())
}

fn get_file_map(
    conn: &Connection,
    snapshot_id: i64,
) -> Result<std::collections::HashMap<String, (i64, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT path, size_bytes, sha256 FROM files WHERE snapshot_id = ?1 AND is_directory = 0",
    )?;
    let map = stmt
        .query_map(params![snapshot_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (row.get::<_, i64>(1)?, row.get::<_, Option<String>>(2)?),
            ))
        })?
        .collect::<std::result::Result<std::collections::HashMap<_, _>, _>>()?;
    Ok(map)
}

/// DB backup using SQLite backup API.
pub fn db_backup(paths: &TapectlPaths, dest: &str) -> Result<()> {
    let src_conn = rusqlite::Connection::open(&paths.db_file)?;
    let mut dst_conn = rusqlite::Connection::open(dest)?;

    let backup = rusqlite::backup::Backup::new(&src_conn, &mut dst_conn)?;
    backup
        .run_to_completion(100, std::time::Duration::from_millis(10), None)
        .map_err(|e| TapectlError::Database(e))?;

    // Also copy keys directory
    let keys_backup = Path::new(dest).with_extension("keys");
    if paths.keys_dir.exists() {
        copy_dir_all(&paths.keys_dir, &keys_backup)?;
    }

    info!(dest = dest, "database backup complete");
    Ok(())
}

/// DB fsck: integrity check with optional repair.
pub fn db_fsck(conn: &Connection, repair: bool) -> Result<FsckReport> {
    let mut report = FsckReport::default();

    // Run integrity check
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    report.integrity_ok = integrity == "ok";
    if !report.integrity_ok {
        report.issues.push(format!("integrity_check: {integrity}"));
    }

    // Check for orphaned records
    let orphan_writes: i64 = conn.query_row(
        "SELECT COUNT(*) FROM writes WHERE volume_id NOT IN (SELECT id FROM volumes)",
        [],
        |row| row.get(0),
    )?;
    if orphan_writes > 0 {
        report
            .issues
            .push(format!("{orphan_writes} orphaned write records"));
        if repair {
            conn.execute(
                "DELETE FROM writes WHERE volume_id NOT IN (SELECT id FROM volumes)",
                [],
            )?;
            report.repaired += 1;
        }
    }

    let orphan_slices: i64 = conn.query_row(
        "SELECT COUNT(*) FROM stage_slices WHERE stage_set_id NOT IN (SELECT id FROM stage_sets)",
        [],
        |row| row.get(0),
    )?;
    if orphan_slices > 0 {
        report
            .issues
            .push(format!("{orphan_slices} orphaned stage slices"));
        if repair {
            conn.execute(
                "DELETE FROM stage_slices WHERE stage_set_id NOT IN (SELECT id FROM stage_sets)",
                [],
            )?;
            report.repaired += 1;
        }
    }

    Ok(report)
}

#[derive(Debug, Default)]
pub struct FsckReport {
    pub integrity_ok: bool,
    pub issues: Vec<String>,
    pub repaired: usize,
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let dest = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}
