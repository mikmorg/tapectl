use std::fs;
use std::path::Path;

use rusqlite::{params, Connection};
use tracing::{info, warn};

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

    let impacts = retire_impacts(conn, vol_id)?;

    let at_risk: Vec<String> = impacts
        .iter()
        .filter(|impact| impact.other_copies == 0)
        .map(|impact| impact.unit_name.clone())
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
        let mut facts: Vec<String> = at_risk
            .iter()
            .map(|name| {
                format!("unit \"{name}\" would have ZERO copies remaining after this retirement")
            })
            .collect();
        // ADR-0004 Tier 1: also show evidence age for any OTHER impacted
        // unit that still retains coverage, so the prompt carries the full
        // picture, not just the zero-copy units (issue #91). Tier 1 is
        // display-only here too -- these units never gate the prompt.
        let now = chrono::Utc::now().naive_utc();
        for impact in &impacts {
            if impact.other_copies != 0 {
                if let Some(line) =
                    crate::policy::evidence::describe(&impact.unit_name, &impact.evidence, now)
                {
                    facts.push(line);
                }
            }
        }

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

/// One impacted unit's retire-impact row: its name/status, its remaining
/// ADR-0004-eligible copy count after the volume being retired is
/// excluded, and (issue #91) the per-volume evidence backing that
/// remaining coverage.
struct RetireImpact {
    unit_name: String,
    unit_status: String,
    other_copies: i64,
    evidence: Vec<crate::policy::evidence::CoverageEvidence>,
}

/// The impact analysis behind `volume_retire`: one [`RetireImpact`] per
/// unit with a completed write on `vol_id`. Split out from the call site
/// (same reasoning as `report::copies_rows`/`audit::copy_count_for_unit`)
/// so the `other_copies` derivation is directly testable without going
/// anywhere near `volume_retire`'s consent gate — which reads real stdin
/// when `assume_yes` is false and a unit is genuinely at risk, exactly the
/// hazard `volume_retire_consent`'s tests are written to avoid.
///
/// `other_copies` is the ADR-0004 coverage derivation: does this unit
/// have a claim on some OTHER volume that is currently eligible (sealed,
/// unquarantined, unretired)? Routes through the shared predicate
/// (`policy::coverage::eligible`) — a write's own `completed` status only
/// proves its volume was sealed at write time, not that it still is
/// (issue #89). The volume being retired (`vol_id`) is excluded from its
/// own "other copies" by identity, not by status, since we are retiring
/// it regardless of what its current status happens to be. Issue #73: it
/// goes through `coverage::copy_count_expr`, so a recorded warehouse
/// deposit of some OTHER eligible volume counts as another copy.
fn retire_impacts(conn: &Connection, vol_id: i64) -> Result<Vec<RetireImpact>> {
    let sql = format!(
        "SELECT DISTINCT u.id, u.name, u.status,
                {} as other_copies
         FROM units u
         JOIN snapshots s ON s.unit_id = u.id
         JOIN stage_sets ss ON ss.snapshot_id = s.id
         JOIN writes w ON w.stage_set_id = ss.id
         WHERE w.volume_id = ?1 AND w.status = 'completed'
         ORDER BY u.name",
        crate::policy::coverage::copy_count_expr(&crate::policy::coverage::CoverageQuery {
            scope: crate::policy::coverage::CoverageScope::Unit {
                id_expr: "u.id",
                current_only: false,
            },
            exclude_volume: Some("?1"),
        })
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<(i64, String, String, i64)> = stmt
        .query_map(params![vol_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut impacts = Vec::with_capacity(rows.len());
    for (unit_id, unit_name, unit_status, other_copies) in rows {
        let evidence =
            crate::policy::evidence::remaining_coverage_evidence(conn, unit_id, Some(vol_id))?;
        impacts.push(RetireImpact {
            unit_name,
            unit_status,
            other_copies,
            evidence,
        });
    }
    Ok(impacts)
}

/// The `--json` shape for ONE piece of remaining-coverage evidence,
/// written once so `volume retire`, `unit mark-tape-only` and
/// `volume compact-finish` cannot drift (issue #99 wired all three).
///
/// `kind` and `deposited_at` are ADDITIVE (issue #73): `last_verified`
/// keeps its meaning and stays `null` for a warehouse deposit, which has
/// never been verified and never will be. A consumer that folded
/// `deposited_at` into `last_verified` would be asserting a verification
/// that did not happen.
pub(crate) fn evidence_json(e: &crate::policy::evidence::CoverageEvidence) -> serde_json::Value {
    serde_json::json!({
        "volume": e.volume_label,
        "kind": match e.kind {
            crate::policy::evidence::EvidenceKind::Tape => "tape",
            crate::policy::evidence::EvidenceKind::WarehouseDeposit => "warehouse_deposit",
        },
        "last_verified": e.last_verified,
        "deposited_at": e.deposited_at,
        "location": e.location,
    })
}

fn retire_impacts_json(impacts: &[RetireImpact]) -> Vec<serde_json::Value> {
    impacts
        .iter()
        .map(|impact| {
            let evidence: Vec<serde_json::Value> =
                impact.evidence.iter().map(evidence_json).collect();
            let now = chrono::Utc::now().naive_utc();
            let evidence_summary =
                crate::policy::evidence::describe(&impact.unit_name, &impact.evidence, now);
            serde_json::json!({
                "unit": impact.unit_name,
                "status": impact.unit_status,
                "remaining_copies": impact.other_copies,
                "evidence": evidence,
                "evidence_summary": evidence_summary,
            })
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
    impacts: &[RetireImpact],
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

fn print_retire_impact(label: &str, status: &str, impacts: &[RetireImpact], at_risk: &[String]) {
    println!("Retiring volume \"{label}\"");
    println!("  Current status: {status}");
    println!("  Affected units:");
    let now = chrono::Utc::now().naive_utc();
    for impact in impacts {
        let warning = if impact.other_copies == 0 {
            " *** ZERO copies remaining! ***"
        } else {
            ""
        };
        println!(
            "    {} [{}]: {} other copy/copies{warning}",
            impact.unit_name, impact.unit_status, impact.other_copies
        );
        // ADR-0004 Tier 1: display evidence age wherever a destructive
        // operation consumes copy coverage -- never gate, never a flag.
        // Zero-copy units have no evidence to describe (they keep only the
        // ZERO-copies line above).
        if impact.other_copies != 0 {
            if let Some(line) =
                crate::policy::evidence::describe(&impact.unit_name, &impact.evidence, now)
            {
                println!("      {line}");
            }
        }
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

    // Count copies and locations. ADR-0004: a write's own `status =
    // 'completed'` only proves the volume was sealed AT WRITE TIME —
    // `volumes.status` keeps moving afterwards (retired/quarantined/
    // erased/missing), so eligibility must be re-checked at USE time via
    // the shared predicate (issue #89).
    //
    // Issue #73: both counts now come from the shared deposit-aware
    // expressions, so a recorded warehouse deposit counts as a copy and as
    // a location here exactly as it does in `audit` and the reports. This
    // also retires this site's private `COUNT(DISTINCT w.id)` shape — it
    // counted WRITES, so a unit staged twice onto one cartridge read as
    // two copies and this gate was quietly more permissive than every
    // other one. Copies are distinct volumes everywhere now.
    let unit_scope = crate::policy::coverage::CoverageQuery::current_unit("?1");
    let sql = format!(
        "SELECT {}, {}",
        crate::policy::coverage::copy_count_expr(&unit_scope),
        crate::policy::coverage::location_count_expr(&unit_scope)
    );
    let (copy_count, location_count): (i64, i64) =
        conn.query_row(&sql, params![unit.id], |row| Ok((row.get(0)?, row.get(1)?)))?;

    // Reuses `fingerprint::classify` — the same scan backing `unit status
    // --dirty` and `report dirty` — so these can never disagree about
    // whether a unit is dirty. Passing `config.defaults.global_excludes`
    // keeps this in lockstep with the other two callers (issue #49).
    let pending =
        crate::collection::fingerprint::classify(conn, &unit, &config.defaults.global_excludes)?;

    // TIER 3 (ADR-0008): zero coverage is ABSOLUTE. Checked BEFORE `force`
    // is consulted, and deliberately outside the `if !force` block below.
    //
    // A never-archived unit has no snapshot, so no tape holds it and there
    // is no claim to be stale. Marking it tape-only is not a riskier
    // version of a thin safety margin — it is incoherent, and it greenlights
    // deleting the only copy of data that exists nowhere else. ADR-0008
    // draws exactly this line: `--force` means "I accept a degraded but
    // non-zero margin", never "I accept total loss". The escape hatch is
    // `snapshot create`, which resolves the incoherence rather than waiving
    // it.
    //
    // The copy-count check below cannot stand in for this. It catches a
    // never-archived unit only INCIDENTALLY (zero completed writes fails
    // `copy_count < min_copies`), and `min_copies_for_tape_only` is
    // operator-configurable — at 0 that comparison passes vacuously.
    if matches!(
        pending.as_ref().map(|p| &p.reason),
        Some(crate::collection::fingerprint::PendingReason::New)
    ) {
        return Err(TapectlError::Other(
            "unit has never been archived: no snapshot exists, so there is no tape copy — \
             marking it tape-only would greenlight deleting the only copy. This cannot be \
             overridden (ADR-0008 Tier 3); run `tapectl snapshot create` first."
                .to_string(),
        ));
    }

    // TIER 2 (ADR-0008): degraded but non-zero coverage — `--force` overrides.
    //
    // Issue #89 / ADR-0004 interaction, decided by the coordinator and
    // recorded here so it is not re-litigated: `copy_count` above already
    // excludes ineligible (quarantined/retired/erased/missing) volumes,
    // so a unit whose every copy has become ineligible reads `copy_count
    // == 0` right here — the SAME zero the Tier-3 guard above would use
    // if it applied. It does not apply. Tier 3 is reserved for
    // INCOHERENCE: a never-archived unit, where no tape ever held the
    // data at all (caught above via `PendingReason::New`, independent of
    // this count). A unit whose only copy sits on a quarantined volume is
    // DEGRADED, not incoherent — the cartridge still physically exists,
    // and quarantine means "claims unreliable until reconciled at
    // contact," not "gone." So this stays Tier 2 and remains
    // `--force`-overridable, same as any other below-threshold copy
    // count. Do not move this case into the Tier-3 guard above.
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

        // Dirty is Tier 2, not Tier 3: the tape copy is stale relative to
        // disk, but it exists. An operator may legitimately know the delta
        // is junk. `tape_only` is the signal that local data may be deleted,
        // so a stale copy still warrants refusing by default.
        if let Some(p) = &pending {
            if matches!(
                p.reason,
                crate::collection::fingerprint::PendingReason::Dirty
            ) {
                return Err(TapectlError::Other(format!(
                    "unit is dirty: on-disk contents changed since the last snapshot — {} \
                     (use --force to override)",
                    p.changes.describe()
                )));
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

    // ADR-0004 Tier 1: display evidence age for the coverage this unit is
    // now relying on -- marking tape-only is exactly the point at which
    // local data may be deleted, so the operator should see how strong
    // that remaining coverage is. Display-only: never gates, never changes
    // the tier logic above, never affects the exit code.
    let evidence = crate::policy::evidence::remaining_coverage_evidence(conn, unit.id, None)?;
    let now = chrono::Utc::now().naive_utc();
    let evidence_summary = crate::policy::evidence::describe(unit_name, &evidence, now);

    if json_output {
        let evidence_json: Vec<serde_json::Value> = evidence.iter().map(evidence_json).collect();
        println!(
            "{}",
            serde_json::json!({
                "unit": unit_name,
                "status": "tape_only",
                "copies": copy_count,
                "locations": location_count,
                "evidence": evidence_json,
                "evidence_summary": evidence_summary,
            })
        );
    } else {
        println!(
            "unit \"{unit_name}\" marked tape-only ({copy_count} copies, {location_count} locations)"
        );
        if let Some(line) = &evidence_summary {
            println!("  {line}");
        }
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

    // The `stage_slices.staging_path` rows about to be deleted are the ONLY
    // handle anything has on the encrypted `.age` files in staging:
    // `staging::clean_staging` finds files exclusively by joining
    // `stage_slices`. So dropping these rows without first recording the
    // paths orphans those files permanently — no cleanup path can ever see
    // them again. Reachable today via `--force` (the only way to delete a
    // snapshot that still has `staged` sets). Collected before the
    // transaction; unlinked after it commits, for the reason below.
    let staging_paths: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT sl.staging_path
             FROM stage_slices sl
             JOIN stage_sets ss ON ss.id = sl.stage_set_id
             WHERE ss.snapshot_id = ?1 AND sl.staging_path IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![snap_id], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };

    // Cascade delete: stage_slices -> stage_sets -> manifest_entries -> manifests -> files -> snapshot
    //
    // One transaction, including the event (issue #55): as six bare
    // `conn.execute` calls, a failure partway left a half-deleted snapshot —
    // e.g. `stage_slices` gone but `stage_sets` still present, referencing
    // slices that no longer exist. Mirrors `snapshot_purge` above, which
    // already had this treatment.
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM stage_slices WHERE stage_set_id IN
         (SELECT id FROM stage_sets WHERE snapshot_id = ?1)",
        params![snap_id],
    )?;
    tx.execute(
        "DELETE FROM stage_sets WHERE snapshot_id = ?1",
        params![snap_id],
    )?;
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
    tx.execute("DELETE FROM snapshots WHERE id = ?1", params![snap_id])?;

    events::log_event(
        &tx,
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

    tx.commit()?;

    // Unlink AFTER the commit, deliberately. If the transaction fails, the
    // snapshot still exists and its slices must still be on disk for it to
    // remain usable — deleting files first would strand a live snapshot
    // pointing at nothing, and `volume write` would then fail on rows that
    // look perfectly valid. Unlinking after means the worst case (a crash
    // between commit and unlink) is an orphaned file, which is exactly
    // today's behaviour and strictly better than a corrupt live snapshot.
    // Best-effort: a file already gone, or one we cannot remove, must not
    // turn a completed delete into an error.
    let mut removed = 0usize;
    for path in &staging_paths {
        match std::fs::remove_file(path) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(path = %path, error = %e, "could not remove staged slice"),
        }
    }
    if removed > 0 {
        tracing::info!(
            count = removed,
            snapshot = %format!("{unit_name}/v{version}"),
            "removed staged slice files belonging to the deleted snapshot"
        );
    }

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

    // Issue #90: the preconditions live in `policy::reclaimable::assess`,
    // not here, so that `report supersedable` measures releasability with
    // this exact code rather than a second copy of it. `Blocked.reason`
    // IS this function's historical error text.
    if !force {
        if let crate::policy::reclaimable::ReclaimVerdict::Blocked { reason, .. } =
            crate::policy::reclaimable::assess(conn, config, &unit, version)?
        {
            return Err(TapectlError::Other(reason));
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
pub fn db_backup(paths: &TapectlPaths, dest: &str, include_keys: bool) -> Result<()> {
    let src_conn = rusqlite::Connection::open(&paths.db_file)?;
    let mut dst_conn = rusqlite::Connection::open(dest)?;

    let backup = rusqlite::backup::Backup::new(&src_conn, &mut dst_conn)?;
    backup
        .run_to_completion(100, std::time::Duration::from_millis(10), None)
        .map_err(TapectlError::Database)?;

    // Issue #40: unconditionally copying every private key to an arbitrary
    // operator-chosen destination (USB stick, network share, cloud-synced
    // folder) with no flag and no warning made that destination a silent
    // key-escrow point. `--include-keys` is opt-in (default off — the DB
    // alone is the common case), and the copy path still ONLY existed
    // because ADR-0005's Heir Kit (#69) is deferred and unbuilt: gating +
    // warning is the right call today, not deprecating the only key-export
    // path there is.
    if include_keys {
        if paths.keys_dir.exists() {
            let keys_backup = Path::new(dest).with_extension("keys");
            copy_dir_all(&paths.keys_dir, &keys_backup)?;
            warn!(
                destination = %keys_backup.display(),
                "private key material copied to backup destination — treat this location as secret"
            );
        }
    } else {
        info!("--include-keys not set; private keys were not copied to this backup");
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
///
/// Issue #104 fixed three defects here, all worth not reintroducing:
///
/// 1. `PRAGMA integrity_check` returns **many** rows on a damaged database
///    — one per problem, up to SQLite's built-in cap of 100. The old code
///    read it with `query_row`, so a corrupt catalog reported exactly one
///    issue no matter how bad it was, and `fsck` looked *more* reassuring
///    the worse things got. Every row is collected now. The clean case is
///    not "empty" and not "contains ok": a healthy database returns
///    exactly one row whose text is `ok`, so that is the predicate.
/// 2. `--repair`'s DELETEs ran unwrapped, so a failure between them left
///    the catalog half-repaired. Both now share one transaction.
/// 3. A repair deletes records of what is on tape and logged nothing. It
///    now writes an `events` row — **inside** the transaction, so an event
///    can never outlive a rolled-back repair.
///
/// `repaired` counts deleted **rows**, not categories (it is rendered as
/// "repaired=N", where a category count is close to meaningless).
pub fn db_fsck(conn: &Connection, repair: bool) -> Result<FsckReport> {
    let mut report = FsckReport::default();

    // Run integrity check — collect every row, not just the first.
    let integrity: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA integrity_check")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    report.integrity_ok = integrity.len() == 1 && integrity[0] == "ok";
    if !report.integrity_ok {
        for line in &integrity {
            report.issues.push(format!("integrity_check: {line}"));
        }
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
    }

    if repair && (orphan_writes > 0 || orphan_slices > 0) {
        let tx = conn.unchecked_transaction()?;
        let mut deleted = 0usize;
        if orphan_writes > 0 {
            deleted += tx.execute(
                "DELETE FROM writes WHERE volume_id NOT IN (SELECT id FROM volumes)",
                [],
            )?;
        }
        if orphan_slices > 0 {
            deleted += tx.execute(
                "DELETE FROM stage_slices WHERE stage_set_id NOT IN (SELECT id FROM stage_sets)",
                [],
            )?;
        }
        events::log_event(
            &tx,
            "system",
            0,
            None,
            "db_fsck_repair",
            None,
            None,
            None,
            Some(&format!(
                "deleted {orphan_writes} orphaned write records, \
                 {orphan_slices} orphaned stage slices"
            )),
            None,
        )?;
        tx.commit()?;
        report.repaired = deleted;
    }

    Ok(report)
}

#[derive(Debug, Default)]
pub struct FsckReport {
    pub integrity_ok: bool,
    pub issues: Vec<String>,
    /// Number of rows deleted by `--repair` (0 when `--repair` was not passed).
    pub repaired: usize,
}

/// Recursively copy `src` into `dst`, creating `dst` and every directory
/// under it 0700 as it goes (issue #41 addendum on #40: `db backup`'s
/// `<dest>.keys/` directory used to be created with no mode of its own,
/// even though the `.key` files inside stay 0600 via `fs::copy` preserving
/// source permissions). `secure_path` is best-effort — an operator-chosen
/// backup destination can be a non-Unix filesystem (FAT/exFAT USB stick),
/// and a chmod failing there must not sink an otherwise-good backup.
fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    crate::config::secure_path(dst, 0o700);
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

// ── Commands moved out of `main.rs` (issue #112) ──
//
// The crate is a dual lib+bin target with `main.rs` as a thin wrapper for a
// reason: integration tests import `tapectl::` and CANNOT reach anything
// defined in the binary. Logic inlined in `main.rs` was therefore logic no
// integration test could exercise. These two were the last such command
// bodies alongside `db` and `config`.

/// `tapectl import`: register a pre-existing volume in the database.
#[allow(clippy::too_many_arguments)]
pub fn volume_import(
    conn: &Connection,
    config: &Config,
    label: &str,
    backend: &str,
    media_type: &str,
    capacity: &str,
    notes: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let cap_bytes = crate::staging::parse_size_to_bytes(capacity)?;
    // Resolve backend_name from configured backend of this type, else fall back
    // to the type string so the row remains self-consistent.
    let backend_name = match backend {
        "lto" => config
            .backends
            .lto
            .first()
            .map(|b| b.name.clone())
            .unwrap_or_else(|| backend.to_string()),
        _ => backend.to_string(),
    };
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status, notes)
         VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6)",
        rusqlite::params![label, backend, backend_name, media_type, cap_bytes, notes],
    )?;
    let vol_id = conn.last_insert_rowid();
    crate::db::events::log_created(conn, "volume", vol_id, label, None)?;
    if json_output {
        println!(
            "{}",
            serde_json::json!({"id": vol_id, "label": label, "status": "imported"})
        );
    } else {
        println!("volume \"{label}\" imported (id={vol_id}, {media_type}, {capacity})");
    }
    Ok(())
}

/// `tapectl quick-archive`: unit init -> snapshot -> stage -> write.
#[allow(clippy::too_many_arguments)]
pub fn quick_archive(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    path: &str,
    tenant: &str,
    volume: &str,
    tag: &[String],
    device: &str,
    json_output: bool,
) -> Result<()> {
    // Step 1: init unit
    let unit_id = crate::unit::init_unit(conn, paths, path, tenant, None, tag, None)?;
    let unit_name: String = conn.query_row(
        "SELECT name FROM units WHERE id = ?1",
        rusqlite::params![unit_id],
        |row| row.get(0),
    )?;
    println!("unit \"{unit_name}\" initialized");
    // Step 2: snapshot
    let snap_id = crate::staging::snapshot_create(conn, &unit_name, config)?;
    println!("snapshot created (id={snap_id})");
    // Step 3: stage
    let ss_id = crate::staging::stage_create(conn, paths, config, snap_id)?;
    println!("staged (stage_set={ss_id})");
    // Step 4: write
    // force=false: quick-archive writes to a caller-provided volume
    // label with no override surface of its own (issue #27 scopes
    // --force to `volume init`/`volume write` only).
    crate::volume::write::volume_write(conn, paths, config, volume, device, 512 * 1024, false)?;
    if json_output {
        println!(
            "{}",
            serde_json::json!({"unit": unit_name, "volume": volume, "status": "completed"})
        );
    } else {
        println!("quick-archive complete: \"{unit_name}\" written to \"{volume}\"");
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
        // Full ordered migration chain (issue #44) — was a hand-applied
        // 001-only snapshot.
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
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();
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
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();
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
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();
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
        assert_eq!(
            unit_status(&conn),
            "active",
            "a refused mark-tape-only must not have changed unit status"
        );
    }

    #[test]
    fn mark_tape_only_refuses_a_never_archived_unit_even_with_force() {
        // ADR-0008 Tier 3: zero coverage is ABSOLUTE — no flag defeats it.
        //
        // This guard originally shipped (issue #36) INSIDE the `if !force`
        // block, with an error message advertising "(use --force to
        // override)". ADR-0008 was ratified afterwards and reclassified
        // zero coverage as absolute, which made that placement a live
        // violation: `--force` really did greenlight marking a unit
        // tape-only when no tape held it. This test pins the corrected
        // behavior so the guard can never drift back inside `if !force`.
        //
        // Compare `AlreadySealed` (src/volume/session.rs), the other Tier-3
        // case: `check_tape_contact` takes no `force` parameter at all, so
        // it is structurally impossible to defeat. That is the stronger
        // pattern; this check achieves the same outcome by running before
        // `force` is ever consulted.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), b"never archived").unwrap();
        let conn = setup_unit_for_tape_only(tmp.path().to_str().unwrap());
        // Deliberately NO snapshot_create.

        let config = config_with_zero_tape_only_thresholds();
        let err = unit_mark_tape_only(&conn, &config, "unit1", true, false)
            .expect_err("--force must NOT defeat the Tier-3 never-archived guard");
        let msg = err.to_string();
        assert!(
            msg.contains("never been archived"),
            "error must say the unit was never archived: {msg}"
        );
        assert!(
            msg.contains("cannot be overridden"),
            "error must state the guard is absolute, not overridable: {msg}"
        );
        assert!(
            msg.contains("snapshot create"),
            "error must name the real escape hatch instead of --force: {msg}"
        );
        assert_eq!(
            unit_status(&conn),
            "active",
            "a refused mark-tape-only must not have changed unit status"
        );
    }

    /// Issue #38/H12: `volume_retire`'s ADR-0008 Tier-2 consent gate.
    ///
    /// None of these tests call `volume_retire` with `assume_yes: false`
    /// in a scenario that would actually reach `cli::consent::confirm` --
    /// doing so would read the REAL `std::io::stdin().is_terminal()`, and
    /// on a dev box running `cargo test` attached to an actual terminal
    /// that could attempt a real prompt and hang the suite (the exact
    /// issue #33 class of bug consent.rs's own tests exist to rule out).
    /// So every test here is constructed so the gate either isn't reached
    /// at all (no at-risk units) or is bypassed via `assume_yes`/`--force`
    /// (which short-circuit before any stdin interaction). The "refuses
    /// without consent" behavior itself is proven exhaustively, with
    /// dependency-injected stdin, in `cli::consent`'s own test module.
    mod volume_retire_consent {
        use super::*;

        /// tenant + unit + snapshot + stage_set + a volume `label` with a
        /// completed write of that stage_set to it. When `with_other_copy`,
        /// the same stage_set is also completed-written to a second volume,
        /// so the unit keeps one copy after `label` is retired (not at
        /// risk). Returns (conn, retiring_volume_id).
        fn setup_volume_with_one_unit(label: &str, with_other_copy: bool) -> (Connection, i64) {
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
                 VALUES (?1, 1, 'full', 'current', '/src')",
                params![unit_id],
            )
            .unwrap();
            let snap_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
                params![snap_id],
            )
            .unwrap();
            let stage_set_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
                 VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
                params![label],
            )
            .unwrap();
            let vol_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, 'completed')",
                params![stage_set_id, snap_id, vol_id],
            )
            .unwrap();

            if with_other_copy {
                // 'sealed', not 'active' -- post-#89 the "other copy" only
                // counts toward coverage if it is currently sealed, so an
                // 'active' (never-sealed) stand-in would no longer satisfy
                // `proceeds_without_any_consent_gate_when_no_unit_is_at_risk`
                // below for the reason the test intends.
                conn.execute(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
                     VALUES ('OTHER-VOL', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
                    [],
                )
                .unwrap();
                let other_vol_id = conn.last_insert_rowid();
                conn.execute(
                    "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                     VALUES (?1, ?2, ?3, 'completed')",
                    params![stage_set_id, snap_id, other_vol_id],
                )
                .unwrap();
            }

            (conn, vol_id)
        }

        fn volume_status(conn: &Connection, vol_id: i64) -> String {
            conn.query_row(
                "SELECT status FROM volumes WHERE id = ?1",
                params![vol_id],
                |r| r.get(0),
            )
            .unwrap()
        }

        #[test]
        fn dry_run_mutates_nothing_even_when_a_unit_is_at_risk() {
            let (conn, vol_id) = setup_volume_with_one_unit("L6-DRYRUN", false);
            volume_retire(&conn, "L6-DRYRUN", false, true, false).expect("dry-run must succeed");

            assert_eq!(
                volume_status(&conn, vol_id),
                "active",
                "dry-run must not change the volume's status"
            );
            let event_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE entity_type = 'volume'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(event_count, 0, "dry-run must not write an audit event");
        }

        #[test]
        fn assume_yes_proceeds_even_when_a_unit_is_at_risk() {
            // Safe: assume_yes=true short-circuits confirm() before any
            // stdin interaction, regardless of the test process's TTY-ness.
            let (conn, vol_id) = setup_volume_with_one_unit("L6-FORCED", false);
            volume_retire(&conn, "L6-FORCED", true, false, false)
                .expect("--yes must override the zero-copy consent gate");
            assert_eq!(volume_status(&conn, vol_id), "retired");
        }

        #[test]
        fn proceeds_without_any_consent_gate_when_no_unit_is_at_risk() {
            // Safe with assume_yes=false: with_other_copy=true means no
            // unit drops to zero copies, so `confirm()` (and therefore any
            // stdin interaction) is never reached at all.
            let (conn, vol_id) = setup_volume_with_one_unit("L6-SAFE", true);
            volume_retire(&conn, "L6-SAFE", false, false, false)
                .expect("no at-risk units must retire without any consent gate");
            assert_eq!(volume_status(&conn, vol_id), "retired");
        }

        #[test]
        fn other_copies_excludes_a_quarantined_second_volume() {
            // Issue #89 / Change 2: `retire_impacts`'s `other_copies` must
            // not count a second volume that no longer passes ADR-0004's
            // eligibility rule. Exercises `retire_impacts` directly rather
            // than `volume_retire` itself -- calling the full function
            // with `assume_yes: false` in a genuinely at-risk scenario is
            // exactly the stdin hazard this module's doc comment (top of
            // `volume_retire_consent`) warns every other test away from.
            let (conn, vol_id) = setup_volume_with_one_unit("L6-QUAR", true);
            // The fixture's OTHER-VOL is 'sealed' by default (so the
            // pre-existing tests above still see a real second copy);
            // flip it to 'quarantined' here, after setup, to isolate
            // exactly this test's point without changing that default.
            conn.execute(
                "UPDATE volumes SET status = 'quarantined' WHERE label = 'OTHER-VOL'",
                [],
            )
            .unwrap();

            let impacts = retire_impacts(&conn, vol_id).unwrap();
            assert_eq!(impacts.len(), 1);
            let impact = &impacts[0];
            assert_eq!(impact.unit_name, "unitA");
            assert_eq!(
                impact.other_copies, 0,
                "a quarantined second volume must not count as another copy"
            );
        }

        /// Issue #73 / ADR-0006: `retire_impacts`'s `other_copies` must
        /// count a recorded warehouse deposit of an eligible OTHER volume.
        /// Retiring a cartridge when a second copy sits in a warehouse is
        /// not a zero-coverage event, and reporting it as one would push
        /// the operator through a consent gate that is simply untrue.
        #[test]
        fn other_copies_includes_a_warehouse_deposit_of_another_volume() {
            let (conn, vol_id) = setup_volume_with_one_unit("L6-DEP", true);
            conn.execute(
                "INSERT INTO locations (name, kind) VALUES ('glacier', 'warehouse')",
                [],
            )
            .unwrap();
            let loc = conn.last_insert_rowid();
            let other: i64 = conn
                .query_row(
                    "SELECT id FROM volumes WHERE label = 'OTHER-VOL'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            conn.execute(
                "INSERT INTO volume_deposits (volume_id, location_id) VALUES (?1, ?2)",
                params![other, loc],
            )
            .unwrap();

            let impacts = retire_impacts(&conn, vol_id).unwrap();
            assert_eq!(impacts.len(), 1);
            assert_eq!(
                impacts[0].other_copies, 2,
                "the sealed other volume AND its warehouse deposit are both copies"
            );
        }

        #[test]
        fn refusal_json_carries_the_volume_and_the_reason() {
            // Change 3's explicit requirement: a JSON consumer must be
            // able to see WHY retirement was refused, not just observe a
            // non-zero exit. Tests the exact function the refusal branch
            // calls, so production and test share one code path -- no
            // stdout capture needed to prove the object's shape.
            let impacts = vec![RetireImpact {
                unit_name: "unitA".to_string(),
                unit_status: "active".to_string(),
                other_copies: 0,
                evidence: vec![],
            }];
            let at_risk = vec!["unitA".to_string()];
            let reason = "retire volume \"L6-0001\" refused: non-interactive session with no \
                           confirmation given — refusing rather than assuming consent \
                           (re-run with --yes to proceed)";
            let json = retire_refusal_json("L6-0001", &impacts, &at_risk, reason);

            assert_eq!(json["volume"], "L6-0001");
            assert_eq!(json["consent"], "refused");
            assert_eq!(json["reason"], reason);
            assert_eq!(json["at_risk_units"][0], "unitA");
            assert_eq!(json["affected_units"][0]["unit"], "unitA");
            assert_eq!(json["affected_units"][0]["remaining_copies"], 0);
        }
    }

    /// Issue #89 / ADR-0004: copy-count derivations must re-qualify
    /// eligibility at USE time (is the volume currently sealed?), not
    /// trust `writes.status = 'completed'` forever — that only proves the
    /// volume was sealed AT WRITE TIME (`src/volume/session.rs` sets both
    /// in the same transaction, at confirm). `volumes.status` keeps
    /// moving afterwards; the `writes` row does not.
    ///
    /// Each test here builds a unit with one completed write to a
    /// permanently-`sealed` volume and a second completed write to a
    /// volume whose status is the dimension under test, then proves:
    ///   - the gate (`unit_mark_tape_only` / `snapshot_mark_reclaimable`)
    ///     sees 1 copy, not 2, and refuses at `min_copies = 2`;
    ///   - two SEALED volumes still count as 2 (the guard must not
    ///     false-positive on the happy path);
    ///   - `report copies` and `audit` see the SAME count the gate does —
    ///     the equality this whole change exists to establish.
    mod adr0004_copy_eligibility {
        use super::*;

        /// tenant + unit (deliberately no `current_path`: `fingerprint::
        /// classify` returns `Ok(None)` for a unit with no path, so
        /// `unit_mark_tape_only`'s New/Dirty guards never fire here — only
        /// the copy/location-count gate under test can refuse) + one
        /// 'current' snapshot + one 'staged' stage_set completed-written
        /// to two volumes: `{name}-SEALED` (always `sealed`) and
        /// `{name}-OTHER` (status = `second_volume_status`, the dimension
        /// under test). Returns (conn, unit_id).
        fn setup_unit_with_two_volumes(
            name: &str,
            second_volume_status: &str,
        ) -> (Connection, i64) {
            let conn = crate::db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
                [],
            )
            .unwrap();
            let tid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                 VALUES (?1, ?2, ?3, 'mtime_size', 1, 'active')",
                params![format!("uuid-{name}"), name, tid],
            )
            .unwrap();
            let unit_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
                 VALUES (?1, 1, 'full', 'current', '/src')",
                params![unit_id],
            )
            .unwrap();
            let snap_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
                params![snap_id],
            )
            .unwrap();
            let stage_set_id = conn.last_insert_rowid();

            conn.execute(
                &format!(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
                     VALUES ('{name}-SEALED', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')"
                ),
                [],
            )
            .unwrap();
            let vol1_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, 'completed')",
                params![stage_set_id, snap_id, vol1_id],
            )
            .unwrap();

            conn.execute(
                &format!(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
                     VALUES ('{name}-OTHER', 'lto', 'lto0', 'LTO-6', 2500000000000, '{second_volume_status}')"
                ),
                [],
            )
            .unwrap();
            let vol2_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, 'completed')",
                params![stage_set_id, snap_id, vol2_id],
            )
            .unwrap();

            (conn, unit_id)
        }

        /// `min_copies_for_tape_only` at its default (2);
        /// `min_locations_for_tape_only` zeroed to isolate the copy-count
        /// gate from the location-count gate — neither volume above sets
        /// `location_id`, so without this override `location_count` would
        /// also read 0 and every refusal below would be "insufficient
        /// locations" instead of "insufficient copies", masking which
        /// check actually fired (same isolation technique as the
        /// pre-existing `config_with_zero_tape_only_thresholds`, which
        /// zeroes both because it is isolating a THIRD guard, the dirty
        /// check).
        fn config_isolating_copy_count() -> Config {
            let mut config = Config::default();
            config.defaults.min_locations_for_tape_only = 0;
            config
        }

        fn mark_tape_only_refuses_for_status(status: &str) {
            let name = format!("mto-{status}");
            let (conn, _unit_id) = setup_unit_with_two_volumes(&name, status);
            let config = config_isolating_copy_count();
            let err = unit_mark_tape_only(&conn, &config, &name, false, false).expect_err(
                &format!("a {status} second volume must not count toward min_copies"),
            );
            let msg = err.to_string();
            assert!(
                msg.contains("insufficient copies: 1 < 2"),
                "status {status}: {msg}"
            );
        }

        #[test]
        fn mark_tape_only_refuses_when_second_volume_is_quarantined() {
            mark_tape_only_refuses_for_status("quarantined");
        }

        #[test]
        fn mark_tape_only_refuses_when_second_volume_is_retired() {
            mark_tape_only_refuses_for_status("retired");
        }

        #[test]
        fn mark_tape_only_refuses_when_second_volume_is_erased() {
            mark_tape_only_refuses_for_status("erased");
        }

        #[test]
        fn mark_tape_only_refuses_when_second_volume_is_missing() {
            mark_tape_only_refuses_for_status("missing");
        }

        #[test]
        fn mark_tape_only_counts_two_sealed_volumes_as_two() {
            let (conn, _unit_id) = setup_unit_with_two_volumes("mto-both-sealed", "sealed");
            let config = config_isolating_copy_count();
            unit_mark_tape_only(&conn, &config, "mto-both-sealed", false, false).expect(
                "two sealed volumes must satisfy min_copies=2 -- the guard must not false-positive",
            );
        }

        /// Issue #73 / ADR-0006: `unit mark-tape-only`'s copy AND location
        /// gates must count a recorded warehouse deposit. The fixture has
        /// exactly one sealed tape at `home` plus a deposit at `glacier`,
        /// so the tape half alone is 1 copy / 1 location and BOTH default
        /// thresholds (2/2) fail; with the deposit counted both are met.
        #[test]
        fn mark_tape_only_counts_a_warehouse_deposit_toward_copies_and_locations() {
            let (conn, _unit_id, _vol) =
                crate::policy::coverage::tests::setup_unit_with_deposit("active");
            let config = Config::default();
            assert_eq!(config.defaults.min_copies_for_tape_only, 2);
            assert_eq!(config.defaults.min_locations_for_tape_only, 2);
            unit_mark_tape_only(&conn, &config, "photos", false, false).expect(
                "one sealed tape at home plus a warehouse deposit at glacier is 2 copies in 2 locations",
            );
        }

        #[test]
        fn mark_tape_only_with_zero_eligible_copies_is_tier2_not_tier3() {
            // ADR-0008 interaction (issue #89), the coordinator's decision
            // recorded in the code comment above the Tier-2 check in
            // `unit_mark_tape_only`: a unit whose every copy has become
            // ineligible reads `copy_count == 0`, the same number Tier 3
            // would use -- but this unit WAS archived (it has a snapshot
            // and completed writes), so `PendingReason::New` never fires
            // and the Tier-3 guard never applies. It must refuse with the
            // ordinary Tier-2 message and must be overridable by --force,
            // never the "never been archived... cannot be overridden"
            // Tier-3 message that `mark_tape_only_refuses_a_never_archived_
            // unit_even_with_force` (above) proves is absolute.
            let name = "mto-zero-eligible";
            let (conn, _unit_id) = setup_unit_with_two_volumes(name, "quarantined");
            conn.execute(
                &format!("UPDATE volumes SET status = 'quarantined' WHERE label = '{name}-SEALED'"),
                [],
            )
            .unwrap();

            let config = config_isolating_copy_count();
            let err = unit_mark_tape_only(&conn, &config, name, false, false)
                .expect_err("zero eligible copies must still refuse without --force");
            let msg = err.to_string();
            assert!(
                msg.contains("insufficient copies: 0 < 2"),
                "must be the ordinary Tier-2 message: {msg}"
            );
            assert!(
                !msg.contains("never been archived") && !msg.contains("cannot be overridden"),
                "must NOT be classified as the Tier-3 never-archived case: {msg}"
            );

            unit_mark_tape_only(&conn, &config, name, true, false)
                .expect("Tier 2 must be --force-overridable, unlike Tier 3");
        }

        /// tenant + unit + TWO snapshots: v1 ('superseded', the one to be
        /// marked reclaimable) and v2 ('current', the superseding
        /// snapshot whose coverage `snapshot_mark_reclaimable` actually
        /// measures) + v2's 'staged' stage_set completed-written to two
        /// volumes, same SEALED / `second_volume_status` shape as
        /// `setup_unit_with_two_volumes`. Returns (conn, unit_id).
        fn setup_reclaimable_fixture(name: &str, second_volume_status: &str) -> (Connection, i64) {
            let conn = crate::db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
                [],
            )
            .unwrap();
            let tid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                 VALUES (?1, ?2, ?3, 'mtime_size', 1, 'active')",
                params![format!("uuid-{name}"), name, tid],
            )
            .unwrap();
            let unit_id = conn.last_insert_rowid();

            conn.execute(
                "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
                 VALUES (?1, 1, 'full', 'superseded', '/src')",
                params![unit_id],
            )
            .unwrap();

            conn.execute(
                "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
                 VALUES (?1, 2, 'full', 'current', '/src')",
                params![unit_id],
            )
            .unwrap();
            let snap2_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
                params![snap2_id],
            )
            .unwrap();
            let stage_set_id = conn.last_insert_rowid();

            conn.execute(
                &format!(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
                     VALUES ('{name}-SEALED', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')"
                ),
                [],
            )
            .unwrap();
            let vol1_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, 'completed')",
                params![stage_set_id, snap2_id, vol1_id],
            )
            .unwrap();

            conn.execute(
                &format!(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
                     VALUES ('{name}-OTHER', 'lto', 'lto0', 'LTO-6', 2500000000000, '{second_volume_status}')"
                ),
                [],
            )
            .unwrap();
            let vol2_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, 'completed')",
                params![stage_set_id, snap2_id, vol2_id],
            )
            .unwrap();

            (conn, unit_id)
        }

        fn mark_reclaimable_refuses_for_status(status: &str) {
            let name = format!("rec-{status}");
            // `Config::default()`'s `resolved.required_locations` stays
            // empty (no archive_set bound to this unit), so the location
            // precondition is skipped entirely and only the copy-count
            // precondition under test can refuse.
            let config = Config::default();
            let (conn, _unit_id) = setup_reclaimable_fixture(&name, status);
            let err = snapshot_mark_reclaimable(&conn, &config, &name, 1, false, false).expect_err(
                &format!(
                    "a {status} second volume must not count toward the superseding snapshot's coverage"
                ),
            );
            let msg = err.to_string();
            assert!(
                msg.contains("has 1 copies, needs 2"),
                "status {status}: {msg}"
            );
        }

        #[test]
        fn mark_reclaimable_refuses_when_second_volume_is_quarantined() {
            mark_reclaimable_refuses_for_status("quarantined");
        }

        #[test]
        fn mark_reclaimable_refuses_when_second_volume_is_retired() {
            mark_reclaimable_refuses_for_status("retired");
        }

        #[test]
        fn mark_reclaimable_refuses_when_second_volume_is_erased() {
            mark_reclaimable_refuses_for_status("erased");
        }

        #[test]
        fn mark_reclaimable_refuses_when_second_volume_is_missing() {
            mark_reclaimable_refuses_for_status("missing");
        }

        #[test]
        fn mark_reclaimable_counts_two_sealed_volumes_as_two() {
            let (conn, _unit_id) = setup_reclaimable_fixture("rec-both-sealed", "sealed");
            let config = Config::default();
            snapshot_mark_reclaimable(&conn, &config, "rec-both-sealed", 1, false, false)
                .expect("two sealed volumes on the superseding snapshot must satisfy min_copies=2");
        }

        /// The property this whole change exists to establish: the gate
        /// (`unit_mark_tape_only`), `report copies`
        /// (`cli::report::copies_rows`), and `audit`
        /// (`cli::audit::copy_count_for_unit`) must never disagree about
        /// how many copies a unit has. Same fixture, three surfaces, one
        /// number.
        #[test]
        fn gate_report_and_audit_agree_on_the_copy_count() {
            let (conn, unit_id) = setup_unit_with_two_volumes("parity-unit", "quarantined");

            let config = config_isolating_copy_count();
            let err = unit_mark_tape_only(&conn, &config, "parity-unit", false, false)
                .expect_err("gate must refuse with only 1 eligible copy");
            assert!(
                err.to_string().contains("insufficient copies: 1 < 2"),
                "{err}"
            );

            let rows = crate::cli::report::copies_rows(&conn, Some("parity-unit")).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0].1, 1,
                "report copies must agree with the gate: 1, not 2"
            );

            let audit_copies = crate::cli::audit::copy_count_for_unit(&conn, unit_id).unwrap();
            assert_eq!(audit_copies, 1, "audit must agree with the gate: 1, not 2");
        }
    }

    /// Issue #90 (re-scoped): `report supersedable` and `snapshot
    /// mark-reclaimable` must never disagree about what is releasable.
    /// They share `policy::reclaimable::assess`; these tests are the
    /// property that sharing exists to guarantee.
    mod supersedable_agreement {
        use super::*;
        use crate::policy::reclaimable::{assess, tests::setup, ReclaimVerdict};

        #[test]
        fn a_blocked_verdict_means_mark_reclaimable_refuses() {
            let (conn, unit) = setup("agree-blocked", 2, "quarantined", "active");
            let config = Config::default();
            let verdict = assess(&conn, &config, &unit, 1).unwrap();
            let reason = match verdict {
                ReclaimVerdict::Blocked { reason, .. } => reason,
                other => panic!("expected Blocked, got {other:?}"),
            };

            let err = snapshot_mark_reclaimable(&conn, &config, "agree-blocked", 1, false, false)
                .expect_err("assess said Blocked, so the gate must refuse");
            assert_eq!(
                err.to_string(),
                reason,
                "the report's reason text IS the gate's error text"
            );
        }

        #[test]
        fn a_releasable_verdict_means_mark_reclaimable_succeeds() {
            let (conn, unit) = setup("agree-ok", 2, "sealed", "active");
            let config = Config::default();
            assert!(
                matches!(
                    assess(&conn, &config, &unit, 1).unwrap(),
                    ReclaimVerdict::Releasable { .. }
                ),
                "fixture must be releasable"
            );
            snapshot_mark_reclaimable(&conn, &config, "agree-ok", 1, false, false)
                .expect("assess said Releasable, so the gate must accept");
        }
    }

    /// Issue #38/H12: `cartridge_mark_erased`'s ADR-0008 Tier-2 lifecycle
    /// gate, plus the volume -> 'erased' transition (Change 5). Same
    /// no-real-stdin discipline as `volume_retire_consent` above: every
    /// test either avoids the gate (already `pending_erase`) or bypasses
    /// it via `force`/`assume_yes` (both short-circuit before stdin).
    mod cartridge_mark_erased_consent {
        use super::*;

        /// A cartridge in `status`, optionally with a volume currently
        /// mounted on it (`cartridge_volumes.unmounted_at IS NULL`).
        /// Returns (conn, cartridge_id, mounted_volume_id).
        fn setup_cartridge(status: &str, mount_volume: bool) -> (Connection, i64, Option<i64>) {
            let conn = crate::db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status)
                 VALUES ('BC001', 'LTO-6', 2500000000000, ?1)",
                params![status],
            )
            .unwrap();
            let cart_id = conn.last_insert_rowid();

            let vol_id = if mount_volume {
                conn.execute(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
                     VALUES ('L6-MOUNTED', 'lto', 'lto0', 'LTO-6', 2500000000000, 'full')",
                    [],
                )
                .unwrap();
                let vid = conn.last_insert_rowid();
                conn.execute(
                    "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
                    params![cart_id, vid],
                )
                .unwrap();
                Some(vid)
            } else {
                None
            };

            (conn, cart_id, vol_id)
        }

        fn cartridge_status(conn: &Connection, id: i64) -> String {
            conn.query_row(
                "SELECT status FROM cartridges WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap()
        }

        fn volume_status(conn: &Connection, id: i64) -> String {
            conn.query_row(
                "SELECT status FROM volumes WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap()
        }

        #[test]
        fn pending_erase_proceeds_without_consent_and_moves_the_volume_to_erased() {
            // Safe with force=false, assume_yes=false: the cartridge is
            // already pending_erase (the expected end of the retire ->
            // bulk-erase -> mark-erased lifecycle), so the gate is never
            // reached.
            let (conn, cart_id, vol_id) = setup_cartridge("pending_erase", true);
            cartridge_mark_erased(&conn, "BC001", false, false, false, false)
                .expect("pending_erase must mark-erased without any consent gate");

            assert_eq!(cartridge_status(&conn, cart_id), "available");
            assert_eq!(
                volume_status(&conn, vol_id.unwrap()),
                "erased",
                "the volume that was mounted on this cartridge must move to erased"
            );
        }

        #[test]
        fn force_overrides_a_cartridge_not_in_pending_erase() {
            let (conn, cart_id, _vol_id) = setup_cartridge("in_use", false);
            cartridge_mark_erased(&conn, "BC001", true, false, false, false)
                .expect("--force must override the pending_erase precondition");
            assert_eq!(cartridge_status(&conn, cart_id), "available");
        }

        #[test]
        fn global_yes_also_overrides_a_cartridge_not_in_pending_erase() {
            // Proves the OR: the global --yes suffices on its own, not
            // only the command-local --force.
            let (conn, cart_id, _vol_id) = setup_cartridge("in_use", false);
            cartridge_mark_erased(&conn, "BC001", false, true, false, false)
                .expect("the global --yes must also override, not just --force");
            assert_eq!(cartridge_status(&conn, cart_id), "available");
        }

        #[test]
        fn dry_run_mutates_nothing() {
            let (conn, cart_id, vol_id) = setup_cartridge("in_use", true);
            cartridge_mark_erased(&conn, "BC001", false, false, true, false)
                .expect("dry-run must succeed");
            assert_eq!(
                cartridge_status(&conn, cart_id),
                "in_use",
                "dry-run must not change the cartridge's status"
            );
            assert_eq!(
                volume_status(&conn, vol_id.unwrap()),
                "full",
                "dry-run must not change the mounted volume's status"
            );
        }
    }

    /// Issue #38/H12: `db_import`'s always-on ADR-0008 Tier-2 consent
    /// gate. Only the `assume_yes: true` path is exercised at this level
    /// (safe -- short-circuits before stdin); the refusal path itself is
    /// proven, with dependency-injected stdin, in `cli::consent`'s tests.
    mod db_import_consent {
        use super::*;

        /// A fresh tapectl home (real files, not `:memory:` -- `db_import`
        /// opens the destination and source by path) with one tenant row
        /// named `marker_name`, so a test can tell before/after content
        /// apart with a single SELECT.
        fn temp_home_with_marker_tenant(
            marker_name: &str,
        ) -> (tempfile::TempDir, crate::config::TapectlPaths) {
            let tmp = TempDir::new().unwrap();
            let paths = crate::config::TapectlPaths::new(tmp.path().to_path_buf());
            paths.ensure_dirs().unwrap();
            let conn = crate::db::open(&paths.db_file).unwrap();
            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES (?1, 0, 'active')",
                params![marker_name],
            )
            .unwrap();
            drop(conn);
            (tmp, paths)
        }

        fn marker_tenant_name(paths: &crate::config::TapectlPaths) -> String {
            let conn = crate::db::open(&paths.db_file).unwrap();
            conn.query_row("SELECT name FROM tenants LIMIT 1", [], |r| r.get(0))
                .unwrap()
        }

        #[test]
        fn dry_run_does_not_touch_the_destination_database() {
            let (_dest_tmp, dest_paths) = temp_home_with_marker_tenant("dest-original");
            let (_src_tmp, src_paths) = temp_home_with_marker_tenant("source-marker");

            db_import(
                &dest_paths,
                src_paths.db_file.to_str().unwrap(),
                false,
                true,
                false,
            )
            .expect("dry-run must succeed");

            assert_eq!(
                marker_tenant_name(&dest_paths),
                "dest-original",
                "dry-run must not touch the destination database"
            );
        }

        #[test]
        fn assume_yes_overwrites_the_destination_with_the_source() {
            let (_dest_tmp, dest_paths) = temp_home_with_marker_tenant("dest-original");
            let (_src_tmp, src_paths) = temp_home_with_marker_tenant("source-marker");

            db_import(
                &dest_paths,
                src_paths.db_file.to_str().unwrap(),
                true,
                false,
                false,
            )
            .expect("assume_yes must let the import proceed");

            assert_eq!(
                marker_tenant_name(&dest_paths),
                "source-marker",
                "the destination must now hold the source's content"
            );
        }

        #[test]
        fn missing_source_errors_before_touching_anything() {
            let (_dest_tmp, dest_paths) = temp_home_with_marker_tenant("dest-original");
            let err = db_import(
                &dest_paths,
                "/nonexistent/path/to/nowhere.db",
                true,
                false,
                false,
            )
            .expect_err("a missing import source must error");
            assert!(err.to_string().contains("not found"), "{err}");
            assert_eq!(
                marker_tenant_name(&dest_paths),
                "dest-original",
                "a rejected import must not touch the destination"
            );
        }
    }

    /// Issue #40: `db backup` used to copy the entire private-key
    /// directory to `<dest>.keys` unconditionally — no flag, no warning,
    /// no way to get a keys-free backup. `--include-keys` makes that
    /// opt-in (default off). Paired with issue #41's directory-mode fix:
    /// the `.keys` destination directory itself must be 0700 (the `.key`
    /// files inside were already 0600 even before this fix, since
    /// `fs::copy` preserves `crypto::keys::save_secret_key`'s mode).
    mod db_backup_keys {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn mode_of(path: &Path) -> u32 {
            fs::metadata(path).unwrap().permissions().mode() & 0o777
        }

        /// A fresh tapectl home with one real generated tenant keypair, so
        /// there's actual key material under `keys_dir` to (not) copy, and
        /// a real sqlite `tapectl.db` for `db_backup`'s `Connection::open`
        /// to read from.
        fn temp_home_with_a_key() -> (TempDir, TapectlPaths) {
            let tmp = TempDir::new().unwrap();
            let paths = TapectlPaths::new(tmp.path().join(".tapectl"));
            paths.ensure_dirs().unwrap();
            crate::crypto::keys::generate_and_save(&paths.keys_dir, "alice", "primary").unwrap();
            drop(crate::db::open(&paths.db_file).unwrap());
            (tmp, paths)
        }

        #[test]
        fn without_include_keys_copies_the_db_but_no_keys() {
            let (_tmp, paths) = temp_home_with_a_key();
            let dest_tmp = TempDir::new().unwrap();
            let dest = dest_tmp.path().join("backup.db");

            db_backup(&paths, dest.to_str().unwrap(), false).unwrap();

            assert!(dest.exists(), "the database copy itself must still happen");
            let keys_backup = dest.with_extension("keys");
            assert!(
                !keys_backup.exists(),
                "without --include-keys, no .keys directory should be created at all"
            );
        }

        #[test]
        fn with_include_keys_copies_keys_dir_0700_and_key_files_stay_0600() {
            let (_tmp, paths) = temp_home_with_a_key();
            let dest_tmp = TempDir::new().unwrap();
            let dest = dest_tmp.path().join("backup.db");

            db_backup(&paths, dest.to_str().unwrap(), true).unwrap();

            let keys_backup = dest.with_extension("keys");
            assert!(keys_backup.is_dir(), ".keys directory should be created");
            assert_eq!(
                mode_of(&keys_backup),
                0o700,
                ".keys directory should be 0700"
            );

            let mut found_key_file = false;
            for entry in fs::read_dir(&keys_backup).unwrap() {
                let entry = entry.unwrap();
                if entry.file_name().to_string_lossy().ends_with(".age.key") {
                    found_key_file = true;
                    assert_eq!(
                        mode_of(&entry.path()),
                        0o600,
                        "{:?} should be 0600",
                        entry.file_name()
                    );
                }
            }
            assert!(
                found_key_file,
                "fixture should have produced at least one .age.key file to check"
            );
        }

        #[test]
        fn with_include_keys_but_no_keys_dir_present_does_not_error() {
            let tmp = TempDir::new().unwrap();
            let paths = TapectlPaths::new(tmp.path().join(".tapectl"));
            paths.ensure_dirs().unwrap();
            drop(crate::db::open(&paths.db_file).unwrap());
            // keys_dir legitimately doesn't exist — e.g. an operator tenant
            // with keys generated some other way, or a very fresh home.
            fs::remove_dir_all(&paths.keys_dir).unwrap();

            let dest_tmp = TempDir::new().unwrap();
            let dest = dest_tmp.path().join("backup.db");

            db_backup(&paths, dest.to_str().unwrap(), true)
                .expect("a missing keys_dir must not turn --include-keys into an error");

            assert!(dest.exists());
            assert!(!dest.with_extension("keys").exists());
        }
    }

    // ── issue #55: snapshot delete — transactional cascade + no orphaned files ──

    /// Fixture: unit + snapshot v1 with a `staged` stage_set whose slice
    /// rows point at real files on disk, plus manifest/file rows, so a
    /// delete has something to cascade through.
    fn setup_deletable_snapshot(dir: &Path) -> (Connection, i64, Vec<std::path::PathBuf>) {
        // Full ordered migration chain (issue #44) — was a hand-applied
        // 001-only snapshot.
        let conn = crate::db::open_memory().unwrap();

        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('u1', 'unit1', ?1, 'mtime_size', 1, 'active')",
            params![tid],
        )
        .unwrap();
        let uid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
             VALUES (?1, 1, 'full', 'created', '/src')",
            params![uid],
        )
        .unwrap();
        let snap_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, slice_size, compression, encrypted, status)
             VALUES (?1, 1024, 'none', 1, 'staged')",
            params![snap_id],
        )
        .unwrap();
        let ss_id = conn.last_insert_rowid();

        let mut slice_files = Vec::new();
        for n in 1..=2i64 {
            let f = dir.join(format!("slice{n}.dar.age"));
            fs::write(&f, b"ciphertext").unwrap();
            conn.execute(
                "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes,
                                           encrypted_bytes, sha256_plain, sha256_encrypted,
                                           staging_path)
                 VALUES (?1, ?2, 10, 10, 'aa', 'bb', ?3)",
                params![ss_id, n, f.to_string_lossy().to_string()],
            )
            .unwrap();
            slice_files.push(f);
        }

        conn.execute(
            "INSERT INTO manifests (snapshot_id) VALUES (?1)",
            params![snap_id],
        )
        .unwrap();
        insert_file(&conn, snap_id, "/src/a.txt", 3, "cc");

        (conn, snap_id, slice_files)
    }

    /// The encrypted `.age` files must not be orphaned. `stage_slices` rows
    /// are the ONLY handle `staging::clean_staging` has on them (it finds
    /// files exclusively by joining that table), so deleting the rows
    /// without unlinking the files strands them forever with no cleanup
    /// path able to see them. Reachable via `--force`, the only way to
    /// delete a snapshot that still has `staged` sets.
    #[test]
    fn delete_removes_the_staged_slice_files_it_drops_the_rows_for() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (conn, _snap_id, slice_files) = setup_deletable_snapshot(tmp.path());

        for f in &slice_files {
            assert!(f.exists(), "fixture slice should exist before delete");
        }

        snapshot_delete(&conn, "unit1", 1, true, false).unwrap();

        for f in &slice_files {
            assert!(
                !f.exists(),
                "staged slice file {} must be removed — its stage_slices row is gone, \
                 so nothing could ever find it again (issue #55)",
                f.display()
            );
        }
    }

    /// The cascade must be all-or-nothing. Driven by a trigger that rejects
    /// the final `DELETE FROM snapshots`, i.e. a failure at the LAST of the
    /// six statements — the case that, untransacted, left every dependent
    /// row deleted while the snapshot itself survived, referencing nothing.
    #[test]
    fn a_failure_late_in_the_cascade_rolls_back_the_whole_delete() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (conn, snap_id, slice_files) = setup_deletable_snapshot(tmp.path());

        conn.execute_batch(
            "CREATE TRIGGER reject_snapshot_delete
             BEFORE DELETE ON snapshots
             BEGIN SELECT RAISE(ABORT, 'reject for test'); END;",
        )
        .unwrap();

        let err = snapshot_delete(&conn, "unit1", 1, true, false).unwrap_err();
        assert!(
            err.to_string().contains("reject for test"),
            "expected the trigger's abort, got: {err}"
        );

        // Every dependent row must survive: without a transaction the five
        // earlier DELETEs would have committed individually.
        for (table, sql) in [
            ("stage_slices", "SELECT COUNT(*) FROM stage_slices"),
            ("stage_sets", "SELECT COUNT(*) FROM stage_sets"),
            ("manifests", "SELECT COUNT(*) FROM manifests"),
            ("files", "SELECT COUNT(*) FROM files"),
        ] {
            let n: i64 = conn.query_row(sql, [], |row| row.get(0)).unwrap();
            assert!(
                n > 0,
                "{table} rows must be rolled back with the failed delete, found {n}"
            );
        }
        let snaps: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM snapshots WHERE id = ?1",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(snaps, 1, "the snapshot itself must survive");

        // And the files must still be on disk — a rolled-back delete that
        // had already unlinked them would leave a live snapshot pointing at
        // nothing, which is worse than not deleting at all.
        for f in &slice_files {
            assert!(
                f.exists(),
                "slice file {} must survive a rolled-back delete",
                f.display()
            );
        }
    }

    /// Issue #104: insert orphans into BOTH tables `db_fsck` repairs, then
    /// repair. Three properties at once: `repaired` is a row count (3, not
    /// the old category count of 2), both tables are actually emptied by
    /// the one transaction, and exactly one `events` row records it.
    ///
    /// This is the test that fails against pre-#104 code: it asserted
    /// `repaired == 2` there, and found no event at all.
    #[test]
    fn fsck_repair_is_transactional_row_counted_and_audited() {
        let conn = crate::db::open_memory().unwrap();

        // Two writes and one stage slice, all pointing at ids that do not
        // exist. Deliberately asymmetric so a category count (2) and a row
        // count (3) cannot be confused for each other.
        //
        // `db::open*` sets `PRAGMA foreign_keys = ON`, so orphans of this
        // shape cannot be *created* through tapectl today — they arrive
        // from an older database, a hand-edited one, or a partial restore,
        // which is exactly the population `fsck` exists to serve. The
        // pragma is dropped only to build the fixture, then restored so
        // the repair itself runs under production FK semantics.
        conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        for set_id in [9001, 9002] {
            conn.execute(
                "INSERT INTO writes (volume_id, stage_set_id, snapshot_id, status)
                 VALUES (9999, ?1, 9999, 'completed')",
                params![set_id],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes,
                                       encrypted_bytes, sha256_plain, sha256_encrypted,
                                       staging_path)
             VALUES (9999, 0, 1, 1, 'aa', 'bb', '/nonexistent/orphan.age')",
            [],
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();

        // Without --repair: reported, not touched.
        let dry = db_fsck(&conn, false).unwrap();
        assert!(dry.integrity_ok, "in-memory db must pass integrity_check");
        assert_eq!(dry.issues.len(), 2, "issues: {:?}", dry.issues);
        assert_eq!(dry.repaired, 0, "a dry run must delete nothing");

        let report = db_fsck(&conn, true).unwrap();
        assert!(report.integrity_ok);
        assert_eq!(
            report.repaired, 3,
            "`repaired` counts deleted rows (2 writes + 1 slice), not the \
             two categories they fall into"
        );

        for table in ["writes", "stage_slices"] {
            let left: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(left, 0, "{table} still holds orphans after repair");
        }

        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE action = 'db_fsck_repair'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 1, "a repair must leave exactly one audit event");

        // A repair that finds nothing must not log an event.
        let noop = db_fsck(&conn, true).unwrap();
        assert_eq!(noop.repaired, 0);
        let events_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE action = 'db_fsck_repair'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events_after, 1, "a no-op repair must not log an event");
    }

    /// The clean-database predicate, pinned on its own. A healthy SQLite
    /// database returns exactly one `integrity_check` row reading `ok` —
    /// never zero rows — so `integrity_ok` must not be derived from
    /// emptiness. The genuinely-corrupt multi-row path is NOT covered here:
    /// it needs a real damaged file, which the ungated suite cannot
    /// synthesize portably.
    #[test]
    fn fsck_integrity_ok_on_a_clean_database() {
        let conn = crate::db::open_memory().unwrap();
        let report = db_fsck(&conn, false).unwrap();
        assert!(report.integrity_ok);
        assert!(report.issues.is_empty(), "issues: {:?}", report.issues);
    }
}
