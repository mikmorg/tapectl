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

    // Delete files and manifests atomically — keep the snapshot row as 'purged'
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM manifest_entries WHERE manifest_id IN
         (SELECT id FROM manifests WHERE snapshot_id = ?1)",
        params![snap_id],
    )?;
    tx.execute(
        "DELETE FROM manifests WHERE snapshot_id = ?1",
        params![snap_id],
    )?;
    tx.execute("DELETE FROM files WHERE snapshot_id = ?1", params![snap_id])?;
    tx.execute(
        "UPDATE snapshots SET status = 'purged' WHERE id = ?1",
        params![snap_id],
    )?;

    events::log_field_change(
        &tx,
        "snapshot",
        snap_id,
        &format!("{unit_name}/v{version}"),
        "purged",
        "status",
        Some("reclaimable"),
        "purged",
        Some(unit.tenant_id),
    )?;
    tx.commit()?;

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
        // SHA256 check — streamed (issue #32/H6, the last H9-class
        // whole-file-in-RAM site): reuses `staging::validate::hash_source_file`
        // instead of `fs::read`-ing the whole file, so peak RAM here is a
        // fixed buffer, never the file's size, and this can never disagree
        // with the hash `stage_create`'s own baseline was established with.
        let (actual, _) = crate::staging::validate::hash_source_file(&full_path, rel_path)?;
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
///
/// ADR-0008 Tier 2: if any unit would drop to ZERO remaining copies, the
/// retirement needs consent before it proceeds (`--yes` overrides; a
/// non-interactive session with no `--yes` refuses rather than assuming
/// consent — see `cli::consent`). `--dry-run` reports the same impact
/// analysis and changes nothing. A refusal is reported through the normal
/// return channel (`Err`) *and*, when `--json` was requested, as a JSON
/// object on stdout — a JSON consumer must be able to see why, not just
/// observe a non-zero exit.
pub fn volume_retire(
    conn: &Connection,
    label: &str,
    assume_yes: bool,
    dry_run: bool,
    json_output: bool,
) -> Result<()> {
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

    let at_risk: Vec<String> = impacts
        .iter()
        .filter(|(_, _, other_copies)| *other_copies == 0)
        .map(|(name, _, _)| name.clone())
        .collect();

    // --dry-run: report the impact analysis and stop, before any consent
    // prompt and before any mutation.
    if dry_run {
        if json_output {
            let mut obj = serde_json::json!({
                "volume": label,
                "affected_units": retire_impacts_json(&impacts),
                "at_risk_units": at_risk,
            });
            obj["dry_run"] = serde_json::json!(true);
            println!("{obj}");
        } else {
            print_retire_impact(label, &status, &impacts, &at_risk);
            println!("\n  DRY RUN — no changes made.");
        }
        return Ok(());
    }

    // ADR-0008 Tier 2: only the zero-copy case needs consent -- a
    // retirement that leaves every affected unit with at least one other
    // copy is a normal, non-risky operation and proceeds unconditionally,
    // same as before this change.
    if !at_risk.is_empty() {
        let action = format!("retire volume \"{label}\"");
        let facts: Vec<String> = at_risk
            .iter()
            .map(|name| {
                format!("unit \"{name}\" would have ZERO copies remaining after this retirement")
            })
            .collect();

        if let Err(e) = crate::cli::consent::confirm(&action, &facts, assume_yes) {
            let reason = e.to_string();
            if json_output {
                println!(
                    "{}",
                    retire_refusal_json(label, &impacts, &at_risk, &reason)
                );
            } else {
                print_retire_impact(label, &status, &impacts, &at_risk);
                println!("\n  REFUSED: {reason}");
            }
            return Err(e);
        }
    }

    if json_output {
        println!(
            "{}",
            serde_json::json!({"volume": label, "affected_units": retire_impacts_json(&impacts)})
        );
    } else {
        print_retire_impact(label, &status, &impacts, &at_risk);
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

fn retire_impacts_json(impacts: &[(String, String, i64)]) -> Vec<serde_json::Value> {
    impacts
        .iter()
        .map(|(name, status, copies)| {
            serde_json::json!({"unit": name, "status": status, "remaining_copies": copies})
        })
        .collect()
}

/// The JSON object `volume_retire` emits to stdout when the Tier-2 consent
/// gate refuses. Split out from the call site so its shape — in
/// particular, that `reason` carries the actual refusal text — is
/// directly testable without capturing stdout (issue #38 / H12: "a JSON
/// consumer must be able to see the refusal reason, not just a non-zero
/// exit").
fn retire_refusal_json(
    label: &str,
    impacts: &[(String, String, i64)],
    at_risk: &[String],
    reason: &str,
) -> serde_json::Value {
    serde_json::json!({
        "volume": label,
        "affected_units": retire_impacts_json(impacts),
        "at_risk_units": at_risk,
        "consent": "refused",
        "reason": reason,
    })
}

fn print_retire_impact(
    label: &str,
    status: &str,
    impacts: &[(String, String, i64)],
    at_risk: &[String],
) {
    println!("Retiring volume \"{label}\"");
    println!("  Current status: {status}");
    println!("  Affected units:");
    for (name, unit_status, other_copies) in impacts {
        let warning = if *other_copies == 0 {
            " *** ZERO copies remaining! ***"
        } else {
            ""
        };
        println!("    {name} [{unit_status}]: {other_copies} other copy/copies{warning}");
    }
    if !at_risk.is_empty() {
        println!(
            "\n  WARNING: {} unit(s) will have ZERO copies after retirement!",
            at_risk.len()
        );
        println!("  Consider writing additional copies before retiring.");
    }
}

/// Mark a cartridge as erased (available for reuse), moving any
/// currently-mounted volume to `erased`.
///
/// Enforces the physical-reuse lifecycle (ADR-0008 Tier 2): the cartridge
/// should be in `pending_erase` — the state `volume compact-finish` (and
/// the write path generally) leaves it in once its data has actually been
/// superseded and the physical tape is meant to be bulk-erased next.
/// Marking a cartridge erased from any OTHER status skips that checkpoint
/// and needs consent (`--force`, the global `--yes`, or an interactive
/// confirmation; a non-interactive session with neither refuses rather
/// than assuming consent — see `cli::consent`). `--dry-run` reports what
/// would happen and changes nothing.
///
/// Note: `cartridges.status` has no `'erased'` value in its CHECK
/// constraint (`available|in_use|pending_erase|retired_permanent|offsite`)
/// — only `volumes.status` does. So this command's own namesake mutation
/// is the cartridge moving to `'available'` (freed for reuse); it is the
/// volume(s) that were mounted on it that move to `'erased'`.
pub fn cartridge_mark_erased(
    conn: &Connection,
    barcode: &str,
    force: bool,
    assume_yes: bool,
    dry_run: bool,
    json_output: bool,
) -> Result<()> {
    let (id, status): (i64, String) = conn
        .query_row(
            "SELECT id, status FROM cartridges WHERE barcode = ?1",
            params![barcode],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| TapectlError::Other(format!("cartridge \"{barcode}\" not found")))?;

    // Currently-mounted volume(s), if any -- these physically lose their
    // data the instant the cartridge is bulk-erased, so they move to
    // 'erased' regardless of which path (pending_erase, or an override)
    // got us here.
    let mut stmt = conn.prepare(
        "SELECT v.id, v.label FROM cartridge_volumes cv
         JOIN volumes v ON v.id = cv.volume_id
         WHERE cv.cartridge_id = ?1 AND cv.unmounted_at IS NULL",
    )?;
    let mounted_volumes: Vec<(i64, String)> = stmt
        .query_map(params![id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let volume_labels: Vec<String> = mounted_volumes.iter().map(|(_, l)| l.clone()).collect();

    if dry_run {
        if json_output {
            let mut obj = serde_json::json!({
                "barcode": barcode,
                "status": status,
                "volumes_to_erase": volume_labels,
            });
            obj["dry_run"] = serde_json::json!(true);
            println!("{obj}");
        } else {
            println!("would mark cartridge \"{barcode}\" erased (currently: {status})");
            for label in &volume_labels {
                println!("  would move volume \"{label}\" to \"erased\"");
            }
            println!("DRY RUN — no changes made.");
        }
        return Ok(());
    }

    // ADR-0008 Tier 2: the normal path (cartridge already pending_erase)
    // needs no consent at all -- it's the expected end of the retire ->
    // bulk-erase -> mark-erased lifecycle. Any OTHER status is a
    // precondition violation and needs an explicit override.
    if status != "pending_erase" {
        let action = format!("mark cartridge \"{barcode}\" erased");
        let facts = vec![format!(
            "cartridge \"{barcode}\" is in status \"{status}\", not \"pending_erase\" -- \
             marking it erased skips the normal bulk-erase lifecycle checkpoint"
        )];
        if let Err(e) = crate::cli::consent::confirm(&action, &facts, force || assume_yes) {
            let reason = e.to_string();
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "barcode": barcode, "status": status,
                        "consent": "refused", "reason": reason,
                    })
                );
            }
            return Err(e);
        }
    }

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE cartridges SET status = 'available' WHERE id = ?1",
        params![id],
    )?;
    tx.execute(
        "UPDATE cartridge_volumes SET unmounted_at = datetime('now')
         WHERE cartridge_id = ?1 AND unmounted_at IS NULL",
        params![id],
    )?;
    for (vol_id, _) in &mounted_volumes {
        tx.execute(
            "UPDATE volumes SET status = 'erased' WHERE id = ?1",
            params![vol_id],
        )?;
    }
    events::log_field_change(
        &tx,
        "cartridge",
        id,
        barcode,
        "erased",
        "status",
        Some(&status),
        "available",
        None,
    )?;
    tx.commit()?;

    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "barcode": barcode, "status": "available", "volumes_erased": volume_labels,
            })
        );
    } else {
        println!("cartridge \"{barcode}\" marked as erased (available for reuse)");
        for label in &volume_labels {
            println!("  volume \"{label}\" marked erased");
        }
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

        // Dirty guard (issue #36/H10, the safety-critical gap this fixes):
        // tape_only is the operator's signal that local data may now be
        // deleted. If the on-disk fingerprint no longer matches the last
        // snapshot, the tape copy is stale — deleting local data would
        // silently destroy the un-archived changes. Reuses
        // `fingerprint::classify`, the same scan `unit status --dirty` and
        // `report dirty` use, so this can never disagree with them about
        // whether the unit is dirty.
        //
        // `New` is guarded explicitly rather than left to the copy-count
        // check above. That check only covers it incidentally: it catches a
        // never-archived unit because such a unit has zero completed
        // writes, which fails `copy_count < min_copies`. But
        // `min_copies_for_tape_only` is operator-configurable, and at 0 the
        // comparison passes vacuously — so a unit that was NEVER archived
        // could be marked tape_only, telling the operator it is safe to
        // delete local data that exists nowhere else. Marking a unit with
        // no snapshot at all as tape_only is incoherent under any
        // configuration, so it is refused on its own terms here.
        if let Some(pending) = crate::collection::fingerprint::classify(conn, &unit)? {
            match pending.reason {
                crate::collection::fingerprint::PendingReason::Dirty => {
                    return Err(TapectlError::Other(format!(
                        "unit is dirty: on-disk contents changed since the last snapshot — {} \
                         (use --force to override)",
                        pending.changes.describe()
                    )));
                }
                crate::collection::fingerprint::PendingReason::New => {
                    return Err(TapectlError::Other(
                        "unit has never been archived: no snapshot exists, so there is no tape \
                         copy — marking it tape-only would greenlight deleting the only copy \
                         (use --force to override)"
                            .to_string(),
                    ));
                }
            }
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

    // Select a SINGLE stage_set — the latest staged one for this unit — so the
    // export never interleaves slices from two dar runs (H11). Two stage sets
    // staged simultaneously (a re-stage, or two versions) would otherwise land
    // duplicate slice numbers in one directory and an heir following
    // RECOVERY.md would get an ambiguous, unrestorable set.
    let (stage_set_id, snapshot_version): (i64, i64) = conn
        .query_row(
            "SELECT ss.id, s.version
             FROM stage_sets ss
             JOIN snapshots s ON s.id = ss.snapshot_id
             WHERE s.unit_id = ?1 AND ss.status = 'staged'
             ORDER BY ss.created_at DESC, ss.id DESC
             LIMIT 1",
            params![unit.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| {
            TapectlError::Other(format!(
                "no staged slices for unit \"{unit_name}\" — run `tapectl stage create` first"
            ))
        })?;

    let mut stmt = conn.prepare(
        "SELECT sl.staging_path, sl.slice_number, sl.encrypted_bytes, sl.sha256_encrypted
         FROM stage_slices sl
         WHERE sl.stage_set_id = ?1 AND sl.staging_path IS NOT NULL
         ORDER BY sl.slice_number",
    )?;
    let slices: Vec<(String, i64, i64, String)> = stmt
        .query_map(params![stage_set_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    if slices.is_empty() {
        return Err(TapectlError::Other(format!(
            "no staged slices for unit \"{unit_name}\" — run `tapectl stage create` first"
        )));
    }

    fs::create_dir_all(dest_dir)?;
    let mut total = 0i64;
    let mut manifest_entries = Vec::new();

    for (src, num, size, sha256) in &slices {
        let src_path = Path::new(src);
        let file_name = src_path
            .file_name()
            .unwrap_or(std::ffi::OsStr::new("slice.dar.age"))
            .to_string_lossy()
            .to_string();
        let dest_file = Path::new(dest_dir).join(&file_name);
        fs::copy(src_path, &dest_file)?;
        total += size;
        manifest_entries.push((*num, file_name, *size, sha256.clone()));
        info!(slice = num, dest = %dest_file.display(), "exported");
    }

    // Write MANIFEST.toml
    let mut manifest = format!(
        "# tapectl export manifest\n\
         # Generated by tapectl — do not edit\n\n\
         [export]\n\
         unit = \"{unit_name}\"\n\
         snapshot_version = {snapshot_version}\n\
         stage_set_id = {stage_set_id}\n\
         total_slices = {}\n\
         total_bytes = {total}\n\
         exported_at = \"{}\"\n\n\
         [[slices]]\n",
        slices.len(),
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
    );
    // Overwrite the last [[slices]] header — build all entries
    manifest.truncate(manifest.len() - "[[slices]]\n".len());
    for (num, filename, size, sha256) in &manifest_entries {
        manifest.push_str(&format!(
            "[[slices]]\n\
             number = {num}\n\
             file = \"{filename}\"\n\
             encrypted_bytes = {size}\n\
             sha256_encrypted = \"{sha256}\"\n\n",
        ));
    }
    fs::write(Path::new(dest_dir).join("MANIFEST.toml"), &manifest)?;

    // SHA256SUMS in the exact format `sha256sum -c` expects — "<hash>  <file>",
    // two spaces. The old awk-in-RECOVERY.md recipe emitted three spaces, which
    // sha256sum rejects, so the documented verification step always failed.
    let mut sums = String::new();
    for (_num, filename, _size, sha256) in &manifest_entries {
        sums.push_str(&format!("{sha256}  {filename}\n"));
    }
    fs::write(Path::new(dest_dir).join("SHA256SUMS"), &sums)?;

    // Archive base = the common `base.N.dar` prefix of the exported slices
    // (strip the `.N.dar.age` suffix), so RECOVERY.md can name the exact
    // `dar -x` argument instead of an ARCHIVE_BASE placeholder.
    let archive_base = manifest_entries
        .first()
        .map(|(_, f, _, _)| {
            let stem = f.strip_suffix(".dar.age").unwrap_or(f);
            stem.rsplit_once('.')
                .map(|(b, _)| b)
                .unwrap_or(stem)
                .to_string()
        })
        .unwrap_or_else(|| "archive".to_string());

    // Write RECOVERY.md
    let recovery = format!(
        "# Recovery Instructions\n\n\
         ## Unit: {unit_name}\n\
         ## Snapshot version: {snapshot_version}\n\n\
         ### Prerequisites\n\n\
         - `age` CLI (https://github.com/FiloSottile/age)\n\
         - `dar` >= 2.6\n\
         - The age secret key used to encrypt this data\n\n\
         ### Steps\n\n\
         1. Verify the encrypted slices against their checksums:\n\
         ```bash\n\
         sha256sum -c SHA256SUMS\n\
         ```\n\n\
         2. Decrypt each slice:\n\
         ```bash\n\
         for f in *.dar.age; do\n\
           age -d -i YOUR_KEY.age.key -o \"${{f%.age}}\" \"$f\"\n\
         done\n\
         ```\n\n\
         3. Extract with dar (the slices share the base name `{archive_base}`):\n\
         ```bash\n\
         dar -x {archive_base} -R /destination/path -O -Q\n\
         ```\n\
         `-O` ignores stored ownership, needed when restoring as a non-root user.\n",
    );
    fs::write(Path::new(dest_dir).join("RECOVERY.md"), &recovery)?;

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
        .map_err(TapectlError::Database)?;

    // Also copy keys directory
    let keys_backup = Path::new(dest).with_extension("keys");
    if paths.keys_dir.exists() {
        copy_dir_all(&paths.keys_dir, &keys_backup)?;
    }

    info!(dest = dest, "database backup complete");
    Ok(())
}

/// DB import: restore a backup file over the live database.
///
/// Always destructive — the live database's entire contents are replaced
/// with `import_path`'s. Always gated on consent (ADR-0008 Tier 2: `--yes`
/// overrides; a non-interactive session with no `--yes` refuses rather
/// than assuming consent — see `cli::consent`). `--dry-run` reports what
/// would happen and changes nothing.
pub fn db_import(
    paths: &TapectlPaths,
    import_path: &str,
    assume_yes: bool,
    dry_run: bool,
    json_output: bool,
) -> Result<()> {
    if !Path::new(import_path).exists() {
        return Err(TapectlError::Other(format!(
            "import source not found: {import_path}"
        )));
    }

    let dest_display = paths.db_file.display().to_string();

    if dry_run {
        if json_output {
            println!(
                "{}",
                serde_json::json!({
                    "source": import_path, "destination": dest_display, "dry_run": true,
                })
            );
        } else {
            println!(
                "would import {import_path} over the live database at {dest_display} — no changes made"
            );
        }
        return Ok(());
    }

    let action = format!("import \"{import_path}\" over the live database");
    let facts = vec![format!(
        "this OVERWRITES the entire live database at {dest_display} with the contents of {import_path}"
    )];

    if let Err(e) = crate::cli::consent::confirm(&action, &facts, assume_yes) {
        let reason = e.to_string();
        if json_output {
            println!(
                "{}",
                serde_json::json!({
                    "source": import_path, "consent": "refused", "reason": reason,
                })
            );
        }
        return Err(e);
    }

    let src_conn = rusqlite::Connection::open(import_path)?;
    let mut dst_conn = rusqlite::Connection::open(&paths.db_file)?;
    let backup = rusqlite::backup::Backup::new(&src_conn, &mut dst_conn)?;
    backup
        .run_to_completion(100, std::time::Duration::from_millis(10), None)
        .map_err(TapectlError::Database)?;

    if json_output {
        println!(
            "{}",
            serde_json::json!({"source": import_path, "status": "imported"})
        );
    } else {
        println!("database imported from {import_path}");
    }
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

#[cfg(test)]
mod tests {
    //! Issue #32/H6: `unit_check_integrity` was the last H9-class
    //! whole-file-in-RAM site (`fs::read` the entire on-disk file before
    //! hashing it). It now streams through
    //! `staging::validate::hash_source_file` instead — the same function
    //! `stage_create`'s own baseline is established with, so the two call
    //! sites can never disagree about a file's sha256. These tests prove
    //! the streamed hash is byte-for-byte identical to the old buffered
    //! `fs::read` + `Sha256::digest` path it replaces (the same equivalence
    //! trap issue #84 hit), then exercise the function end-to-end.
    use super::*;
    use tempfile::TempDir;

    /// The exact pre-#32 buffered hashing `unit_check_integrity` used to do
    /// inline: whole-file `fs::read`, `Sha256::digest`, byte-iteration hex
    /// (not `{:x}`) — reproduced verbatim so the comparison is against the
    /// literal old behavior, not a paraphrase of it.
    fn direct_old_style_hash(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn setup_conn_with_unit(current_path: &str) -> (Connection, i64) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        let schema = include_str!("../db/migrations/001_initial.sql");
        conn.execute_batch(schema).unwrap();

        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, checksum_mode, encrypt, status)
             VALUES ('u1', 'unit1', ?1, ?2, 'mtime_size', 1, 'active')",
            params![tid, current_path],
        )
        .unwrap();
        let uid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
             VALUES (?1, 1, 'full', 'current', ?2)",
            params![uid, current_path],
        )
        .unwrap();
        let sid = conn.last_insert_rowid();
        (conn, sid)
    }

    fn insert_file(conn: &Connection, snapshot_id: i64, path: &str, size: i64, sha256: &str) {
        conn.execute(
            "INSERT INTO files (snapshot_id, path, size_bytes, sha256, is_directory)
             VALUES (?1, ?2, ?3, ?4, 0)",
            params![snapshot_id, path, size, sha256],
        )
        .unwrap();
    }

    #[test]
    fn streamed_hash_source_file_matches_the_old_buffered_check_integrity_path() {
        // Multi-chunk content (larger than one streaming buffer) proves
        // this isn't a one-read toy case — varied content per line, not
        // one repeated byte, so the hash reflects the whole input.
        let tmp = TempDir::new().unwrap();
        let mut content = Vec::new();
        for i in 0..6000u32 {
            content.extend_from_slice(format!("check-integrity line {i}\n").as_bytes());
        }
        let path = tmp.path().join("big.bin");
        std::fs::write(&path, &content).unwrap();

        let expected = direct_old_style_hash(&content);
        let (streamed, streamed_len) =
            crate::staging::validate::hash_source_file(&path, "big.bin").unwrap();

        assert_eq!(streamed_len, content.len() as i64);
        assert_eq!(
            streamed, expected,
            "streamed check-integrity hash must equal the old fs::read+Sha256::digest hash"
        );
    }

    #[test]
    fn check_integrity_runs_end_to_end_against_a_real_directory() {
        // Regression coverage for the fix itself: the function must still
        // run correctly now that its hashing goes through
        // `hash_source_file` instead of `fs::read` + `Sha256` inline.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), b"hello").unwrap();

        let (conn, sid) = setup_conn_with_unit(tmp.path().to_str().unwrap());
        let hash = direct_old_style_hash(b"hello");
        insert_file(&conn, sid, "f.txt", 5, &hash);

        unit_check_integrity(&conn, "unit1", true).expect("check-integrity must succeed");
    }

    #[test]
    fn check_integrity_still_detects_bitrot_after_the_streaming_swap() {
        // Same-size, different-content must still classify BITROT after
        // the streaming refactor — a regression guard so the fix for issue
        // #32/H6's H9 remainder can't silently defang the existing BITROT
        // detection this command already had.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), b"HELLO").unwrap(); // same size as "hello"

        let (conn, sid) = setup_conn_with_unit(tmp.path().to_str().unwrap());
        let stale_hash = direct_old_style_hash(b"hello"); // recorded for different bytes
        insert_file(&conn, sid, "f.txt", 5, &stale_hash);

        // unit_check_integrity only prints/returns Ok even when BITROT is
        // found (it's a diagnostic report, not a gate) — so confirm the
        // streamed hash itself actually diverges from the recorded one,
        // proving the classification this call depends on still fires.
        let (actual, _) =
            crate::staging::validate::hash_source_file(&tmp.path().join("f.txt"), "f.txt").unwrap();
        assert_ne!(actual, stale_hash);
        unit_check_integrity(&conn, "unit1", true).expect("check-integrity must still succeed");
    }

    /// Issue #36/H10: `unit_mark_tape_only`'s dirty guard. Full migrations
    /// (not just 001, unlike `setup_conn_with_unit` above) because these
    /// tests exercise the real `fingerprint::classify` via a real
    /// `snapshot_create`, not a hand-inserted `files` row.
    fn setup_unit_for_tape_only(current_path: &str) -> Connection {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, checksum_mode, encrypt, status)
             VALUES ('u1', 'unit1', ?1, ?2, 'mtime_size', 1, 'active')",
            params![tid, current_path],
        )
        .unwrap();
        conn
    }

    /// Tape-only copy/location thresholds relaxed to 0: 0 completed writes
    /// < 0 required is false, so the pre-existing copy/location checks
    /// trivially pass and the ONLY thing that can refuse in these tests is
    /// the new dirty guard — isolating exactly what's under test without
    /// also having to fabricate volumes/writes/locations fixtures.
    fn config_with_zero_tape_only_thresholds() -> Config {
        let mut config = Config::default();
        config.defaults.min_copies_for_tape_only = 0;
        config.defaults.min_locations_for_tape_only = 0;
        config
    }

    fn unit_status(conn: &Connection) -> String {
        conn.query_row("SELECT status FROM units WHERE name = 'unit1'", [], |r| {
            r.get(0)
        })
        .unwrap()
    }

    #[test]
    fn mark_tape_only_refuses_a_dirty_unit_without_force() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("f.txt");
        std::fs::write(&file_path, b"hello").unwrap();
        let conn = setup_unit_for_tape_only(tmp.path().to_str().unwrap());
        crate::staging::snapshot_create(&conn, "unit1").unwrap();
        std::fs::write(&file_path, b"hello, world! now a different size").unwrap();

        let config = config_with_zero_tape_only_thresholds();
        let err = unit_mark_tape_only(&conn, &config, "unit1", false, false)
            .expect_err("a dirty unit must refuse mark-tape-only without --force");
        let msg = err.to_string();
        assert!(msg.contains("dirty"), "error must mention dirty: {msg}");
        assert!(
            msg.contains("f.txt"),
            "error must name the specific changed file: {msg}"
        );
        assert!(
            msg.contains("--force"),
            "error must mention the override: {msg}"
        );
        assert_eq!(
            unit_status(&conn),
            "active",
            "a refused mark-tape-only must not have changed unit status"
        );
    }

    #[test]
    fn mark_tape_only_succeeds_on_a_dirty_unit_with_force() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("f.txt");
        std::fs::write(&file_path, b"hello").unwrap();
        let conn = setup_unit_for_tape_only(tmp.path().to_str().unwrap());
        crate::staging::snapshot_create(&conn, "unit1").unwrap();
        std::fs::write(&file_path, b"hello, world! now a different size").unwrap();

        let config = config_with_zero_tape_only_thresholds();
        unit_mark_tape_only(&conn, &config, "unit1", true, false)
            .expect("--force must override the dirty guard, same as the copy/location checks");
        assert_eq!(unit_status(&conn), "tape_only");
    }

    #[test]
    fn mark_tape_only_does_not_block_a_clean_unit() {
        // Regression guard for the guard itself: proves it doesn't
        // false-positive on a unit nothing has changed for.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), b"hello").unwrap();
        let conn = setup_unit_for_tape_only(tmp.path().to_str().unwrap());
        crate::staging::snapshot_create(&conn, "unit1").unwrap();
        // No mutation after the snapshot — stays clean.

        let config = config_with_zero_tape_only_thresholds();
        unit_mark_tape_only(&conn, &config, "unit1", false, false)
            .expect("a clean unit must not be blocked by the new dirty guard");
        assert_eq!(unit_status(&conn), "tape_only");
    }

    #[test]
    fn mark_tape_only_refuses_a_never_archived_unit_even_with_zero_min_copies() {
        // The copy-count check catches a never-archived unit only
        // INCIDENTALLY: zero completed writes fails `copy_count <
        // min_copies` at the default of 2. But min_copies_for_tape_only is
        // operator-configurable, and at 0 that comparison passes vacuously
        // (0 < 0 is false) — which is exactly what
        // `config_with_zero_tape_only_thresholds` sets up. Without an
        // explicit `New` guard, a unit that was never archived would be
        // marked tape_only, telling the operator it is safe to delete the
        // ONLY copy of that data.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), b"never archived").unwrap();
        let conn = setup_unit_for_tape_only(tmp.path().to_str().unwrap());
        // Deliberately NO snapshot_create — the unit has never been archived.

        let config = config_with_zero_tape_only_thresholds();
        let err = unit_mark_tape_only(&conn, &config, "unit1", false, false)
            .expect_err("a never-archived unit must refuse mark-tape-only");
        let msg = err.to_string();
        assert!(
            msg.contains("never been archived"),
            "error must say the unit was never archived: {msg}"
        );
        assert!(
            msg.contains("--force"),
            "error must mention the override: {msg}"
        );
        assert_eq!(
            unit_status(&conn),
            "active",
            "a refused mark-tape-only must not have changed unit status"
        );
    }
}
