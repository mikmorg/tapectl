use std::fs;
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
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
/// **Two gates, in ADR-0008's tier order** (ADR-0012, issue #147 — this
/// command shipped them inverted, prompting only at zero and letting
/// `--force` through):
///
/// 1. **Tier 3, absolute.** If retiring this volume takes any live version
///    to zero copies, [`refuse_last_eligible_copy`] refuses. It takes no
///    `force` parameter, so neither `--yes` nor anything else can reach
///    past it, and it runs first so no prompt can arrive before it.
/// 2. **Tier 2, overridable.** If the retirement leaves a live version
///    below its RESOLVED policy but above zero
///    ([`below_policy_facts`]), the facts are displayed and consent is
///    required; `--yes` overrides, and a non-interactive session with no
///    `--yes` refuses rather than assuming consent (see `cli::consent`).
///
/// Tier 1 (ADR-0004) is unchanged and never gates: evidence age is
/// displayed for every impacted unit that retains coverage, both in the
/// impact analysis and at the moment consent is asked.
///
/// `--dry-run` reports the same impact analysis — naming any version the
/// Tier-3 floor would refuse on — and changes nothing. A refusal is
/// reported through the normal return channel (`Err`) *and*, when `--json`
/// was requested, as a JSON object on stdout — a JSON consumer must be
/// able to see why, not just observe a non-zero exit.
pub fn volume_retire(
    conn: &Connection,
    config: &Config,
    label: &str,
    assume_yes: bool,
    dry_run: bool,
    json_output: bool,
) -> Result<()> {
    let (vol_id, status, condition): (i64, String, String) = conn
        .query_row(
            "SELECT id, status, observed_condition FROM volumes WHERE label = ?1",
            params![label],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
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
            print_retire_impact(label, &status, &condition, &impacts, &at_risk);
            println!("\n  DRY RUN — no changes made.");
        }
        return Ok(());
    }

    let action = format!("retire volume \"{label}\"");

    // ADR-0008 TIER 3, the absolute floor (ADR-0012, issue #147). First,
    // and structurally undefeatable -- `refuse_last_eligible_copy` takes no
    // `force`, so `assume_yes` is not even in scope for this decision.
    if let Err(e) = refuse_last_eligible_copy(conn, &action, label, &impacts) {
        if json_output {
            println!(
                "{}",
                retire_refusal_json(label, &impacts, &at_risk, &e.to_string())
            );
        } else {
            // Impact analysis only -- it already carries the per-version
            // "REFUSED" line. The floor's own text is long, and `main`'s
            // error handler prints it to stderr on the way out; echoing it
            // here too would give the operator the same three paragraphs
            // twice at the one moment they most need to read them once.
            print_retire_impact(label, &status, &condition, &impacts, &at_risk);
        }
        return Err(e);
    }

    // ADR-0008 TIER 2: degraded but non-zero. Two populations reach it --
    // a version left below its resolved policy (issue #147's own case,
    // which had no gate at all), and a unit the impact analysis reads as
    // zero-copy WITHOUT the Tier-3 floor having fired. The second is not a
    // contradiction: the floor is defined by what the act REMOVES, so a
    // unit whose only claim is on this very volume while it sits
    // quarantined, or on a version already released, was at zero before
    // this command ran and stays there. Worth showing; not worth refusing.
    let below_policy = below_policy_facts(conn, config, &impacts)?;
    if !at_risk.is_empty() || !below_policy.is_empty() {
        let mut facts: Vec<String> = at_risk
            .iter()
            .map(|name| {
                format!("unit \"{name}\" would have ZERO copies remaining after this retirement")
            })
            .collect();
        facts.extend(below_policy);
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
                // Impact analysis only, same as the Tier-3 path above: the
                // refusal now carries its facts (`cli::consent`), and
                // `main` prints it to stderr on the way out. One copy.
                print_retire_impact(label, &status, &condition, &impacts, &at_risk);
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
        print_retire_impact(label, &status, &condition, &impacts, &at_risk);
    }

    // Actually retire. ONE transaction: the volume's status and the
    // cartridge's are the two halves of the same fact (ADR-0011, "Retiring a
    // volume frees its cartridge"), and a crash between them would leave a
    // cartridge marked in_use with nothing live on it — a tape the operator
    // is told not to reuse and has no reason not to.
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE volumes SET status = 'retired' WHERE id = ?1",
        params![vol_id],
    )?;
    events::log_field_change(
        &tx,
        "volume",
        vol_id,
        label,
        "retired",
        "status",
        Some(&status),
        "retired",
        None,
    )?;
    let freed = free_cartridge_if_last_live(&tx, vol_id)?;
    tx.commit()?;

    if !json_output {
        println!("  Volume \"{label}\" retired.");
        if let Some(barcode) = &freed {
            println!(
                "  Cartridge {barcode} is now pending_erase — it holds no live volume. \
                 `tapectl cartridge mark-erased {barcode}` after you erase it."
            );
        }
    }
    Ok(())
}

/// Move the cartridge this volume was on to `pending_erase`, if this was the
/// last live volume on it. Returns the barcode when the cartridge changed.
///
/// ADR-0011's lifecycle diagram names `volume retire` as a writer of
/// `pending_erase`, and until ADR-0010's binding no volume knew which
/// cartridge it was on, so only `compact-finish` ever wrote that state. This
/// is what makes it reachable by the ordinary path.
///
/// Three refusals, each for its own reason:
///
/// - **Other live volumes remain.** The cartridge still holds data someone
///   could restore from; nothing about it has changed. "Live" is
///   `policy::coverage::in_service`, the named predicate, never an inlined
///   status list (an `initialized` volume is deliberately NOT live here: it
///   is provisioned and holds no bytes, and `volume init` binds a
///   `pending_erase` cartridge without consent anyway, so this costs nothing
///   and reopening the tape for reuse is the point).
/// - **The cartridge is `retired_permanent`.** `cartridge unretire` is its
///   only exit (ADR-0011, corrected 2026-09-14) — "the operator saying they
///   were wrong about the medium". Retiring a volume is not that statement,
///   and must not quietly undo a condemnation.
/// - **The cartridge is already `available` or `pending_erase`.** Nothing to
///   do; both already mean "not holding live data".
///
/// The mount is deliberately left OPEN (`cartridge_volumes.unmounted_at`
/// untouched): the bytes are still physically there until someone erases
/// them, and `cartridge mark-erased` is the step that says otherwise.
pub(crate) fn free_cartridge_if_last_live(
    conn: &Connection,
    vol_id: i64,
) -> Result<Option<String>> {
    let mounted: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT c.id, c.barcode, c.status FROM cartridge_volumes cv
             JOIN cartridges c ON c.id = cv.cartridge_id
             WHERE cv.volume_id = ?1 AND cv.unmounted_at IS NULL",
            params![vol_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((cartridge_id, barcode, status)) = mounted else {
        return Ok(None);
    };

    if status != "in_use" {
        return Ok(None);
    }

    let sql = format!(
        "SELECT COUNT(*) FROM cartridge_volumes cv
         JOIN volumes v ON v.id = cv.volume_id
         WHERE cv.cartridge_id = ?1 AND cv.unmounted_at IS NULL
           AND cv.volume_id != ?2 AND {}",
        crate::policy::coverage::in_service("v")
    );
    let others: i64 = conn.query_row(&sql, params![cartridge_id, vol_id], |row| row.get(0))?;
    if others > 0 {
        return Ok(None);
    }

    // `AND status = 'in_use'` again in the UPDATE, not only in the check
    // above: belt and braces against a future caller reaching this with a
    // cartridge it has not read.
    let changed = conn.execute(
        "UPDATE cartridges SET status = 'pending_erase' WHERE id = ?1 AND status = 'in_use'",
        params![cartridge_id],
    )?;
    if changed != 1 {
        return Ok(None);
    }
    events::log_field_change(
        conn,
        "cartridge",
        cartridge_id,
        &barcode,
        "updated",
        "status",
        Some(&status),
        "pending_erase",
        None,
    )?;
    Ok(Some(barcode))
}

/// One impacted unit's retire-impact row: its name/status, its remaining
/// ADR-0004-eligible copy count after the volume being retired is
/// excluded, and (issue #91) the per-volume evidence backing that
/// remaining coverage.
#[derive(Clone)]
pub(crate) struct RetireImpact {
    pub(crate) unit_name: String,
    pub(crate) unit_status: String,
    pub(crate) other_copies: i64,
    pub(crate) evidence: Vec<crate::policy::evidence::CoverageEvidence>,
    /// The CURRENT versions of this unit whose coverage the retirement
    /// actually CONSUMES, and what each would have left (ADR-0012, issue
    /// #147) — `policy::coverage::versions_at_stake`, the one derivation
    /// both consent tiers read.
    ///
    /// Deliberately NOT the same question as `other_copies`, which stays
    /// what it always was: a per-UNIT count across ANY snapshot the volume
    /// participates in, for DISPLAY and `--json`. The tiers cannot be read
    /// off that number. A unit can show two remaining copies there and
    /// still be losing the last copy of v2 (issue #153: a newer version is
    /// not a copy of an older one), and a unit can show zero there while
    /// the retirement removes nothing at all — the volume was quarantined,
    /// or every version on it was released. Both readings are wrong in a
    /// direction that matters, so the gates read these rows instead.
    ///
    /// EMPTY is the ordinary case for a volume that counts for nothing:
    /// quarantined, unsealed, already retired, or holding only released
    /// versions. Nothing gates on it then, which is the point.
    pub(crate) at_stake: Vec<crate::policy::coverage::VersionAtStake>,
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
pub(crate) fn retire_impacts(conn: &Connection, vol_id: i64) -> Result<Vec<RetireImpact>> {
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
        // Issue #147: the per-version rows both ADR-0008 tiers read. Derived
        // HERE rather than at each gate, so `volume retire`, `cartridge
        // retire` and `volume compact-finish` cannot come to different
        // conclusions about the same retirement -- the whole reason this
        // function is shared in the first place.
        let at_stake = crate::policy::coverage::versions_at_stake(conn, unit_id, vol_id)?;
        impacts.push(RetireImpact {
            unit_name,
            unit_status,
            other_copies,
            evidence,
            at_stake,
        });
    }
    Ok(impacts)
}

/// **ADR-0008 Tier 3, the retire family's absolute floor.** Refuse when
/// retiring `volume_label` would take any live version to zero copies
/// (ADR-0012, issue #147).
///
/// **This takes no `force` parameter, structurally**, and that is the
/// design, not an omission. It joins the family `src/volume/binding.rs`
/// documents — `refuse_retired`, `require_named_cartridge`,
/// `refuse_unwitnessed_displacement`, `corroborate_contact` — plus
/// `session.rs`'s `check_tape_contact`/`AlreadySealed`: a caller cannot
/// defeat any of them even by mistake, because there is no argument to
/// pass. ADR-0008 says Tier 3 conditions "must NEVER call
/// [`crate::cli::consent::confirm`]" (see that module's header); routing
/// this one through consent would itself be the bug, since `--yes` would
/// then waive a guard ADR-0008 says nothing may waive.
///
/// The three callers are `volume retire`, `cartridge retire` and
/// `volume compact-finish` — the three commands ADR-0012 names as having
/// shipped the tiers inverted. Each calls this BEFORE reaching any
/// Tier-2 prompt, so no ordering can let consent arrive first.
///
/// `act` names what the operator asked for, in the imperative
/// (`retire volume "L6-0007"`), and `volume_label` is the volume whose
/// coverage is at stake — for a cartridge those differ, and the recovery
/// commands must name the VOLUME.
///
/// Returns `Ok(())` when nothing is at stake, which is the ordinary case:
/// `impacts` carries no `at_stake` rows at all for a quarantined,
/// unsealed or already-retired volume, nor for one holding only released
/// versions.
/// Whether `unit`'s version `version` still has a stage set with live
/// slices — i.e. its ciphertext is still sitting in staging.
///
/// Routed through [`crate::staging::stage_set_has_live_slices`] rather than
/// inlining the status list, so this and `stage create`'s own refusal
/// (`cli::stage.rs`) cannot drift apart; five inlined status lists is how
/// issue #96 happened.
fn version_has_live_stage_set(conn: &Connection, unit: &str, version: i64) -> Result<bool> {
    let statuses: Vec<String> = conn
        .prepare(
            "SELECT ss.status FROM stage_sets ss
             JOIN snapshots s ON s.id = ss.snapshot_id
             JOIN units u ON u.id = s.unit_id
             WHERE u.name = ?1 AND s.version = ?2",
        )?
        .query_map(params![unit, version], |row| row.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(statuses
        .iter()
        .any(|st| crate::staging::stage_set_has_live_slices(st)))
}

pub(crate) fn refuse_last_eligible_copy(
    conn: &Connection,
    act: &str,
    volume_label: &str,
    impacts: &[RetireImpact],
) -> Result<()> {
    let mut doomed: Vec<(&str, i64)> = Vec::new();
    for impact in impacts {
        for version in &impact.at_stake {
            if version.copies_after == 0 {
                doomed.push((impact.unit_name.as_str(), version.version));
            }
        }
    }
    if doomed.is_empty() {
        return Ok(());
    }

    let named = doomed
        .iter()
        .map(|(unit, version)| format!("unit \"{unit}\" v{version}"))
        .collect::<Vec<_>>()
        .join(", ");
    let (count, those) = if doomed.len() == 1 {
        ("1 live version".to_string(), "that version with no copy")
    } else {
        (
            format!("{} live versions", doomed.len()),
            "those versions with no copies",
        )
    };

    // One recovery line per DISTINCT unit, and one release line per
    // (unit, version): the operator has to act on each separately, and a
    // single example would leave them guessing at the rest.
    let mut units: Vec<&str> = doomed.iter().map(|(unit, _)| *unit).collect();
    units.dedup();
    let copy_out = units
        .iter()
        .map(|unit| format!("    tapectl volume read-slices --from {volume_label} --unit {unit}"))
        .collect::<Vec<_>>()
        .join("\n");
    let release = doomed
        .iter()
        .map(|(unit, version)| {
            format!("    tapectl snapshot mark-reclaimable {unit} --version {version}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    // Per (unit, VERSION), not per unit: the doomed version need not be the
    // newest — v2 only here while v3 sits elsewhere is a perfectly legal
    // at-stake row — and `stage create` without `--version` takes the
    // latest unstaged one, which would re-copy the version that was never
    // in danger. `snapshot create` is deliberately NOT offered as the first
    // step: under ADR-0012 it mints nothing when content is unchanged, so
    // it cannot reproduce a version the operator still has on disk.
    //
    // Split by whether the version's slices are STILL STAGED, because
    // `stage create --version` refuses exactly then ("already has a stage
    // set with live slices"). That is reachable rather than theoretical:
    // `volume write` leaves every set it writes `staged` and `staging
    // clean` is the release half, so a version written and never released
    // is still staged when this refusal fires. A recipe that hands the
    // operator a command which will be refused is not a recipe.
    let mut restage_lines: Vec<String> = Vec::new();
    let mut already_staged: Vec<String> = Vec::new();
    for (unit, version) in &doomed {
        if version_has_live_stage_set(conn, unit, *version)? {
            already_staged.push(format!("unit \"{unit}\" v{version}"));
        } else {
            restage_lines.push(format!(
                "    tapectl stage create {unit} --version {version}"
            ));
        }
    }
    let restage = restage_lines.join("\n");

    // Three shapes, because the honest instruction differs and a single
    // hedged paragraph would make the operator work out which half applies
    // to them mid-incident.
    let write_tail =
        "    tapectl volume init <OTHER-LABEL>\n    tapectl volume write <OTHER-LABEL>";
    let on_disk_branch = match (restage.is_empty(), already_staged.is_empty()) {
        // Nothing left in staging: re-stage from the source, if it is there.
        (false, true) => format!(
            "or, if the content is still on disk, stage it again — one line per version \
             at stake — and write that:\n{restage}\n{write_tail}\n"
        ),
        // Everything is still staged: the bytes are already in staging, so
        // `stage create` would be refused and re-staging is not the act.
        (true, false) => format!(
            "or skip the copy-out entirely — {} still {} slices in staging from the \
             write that put {} on this volume, so nothing needs re-staging. Write \
             what is already there:\n{write_tail}\n",
            already_staged.join(", "),
            if already_staged.len() == 1 {
                "has"
            } else {
                "have"
            },
            if already_staged.len() == 1 {
                "it"
            } else {
                "them"
            },
        ),
        // Mixed: name which half is which, then one write covers both.
        (false, false) => format!(
            "or work from what is already staged plus the rest from disk. {} still {} \
             slices in staging and {} nothing re-staged. Stage the others:\n{restage}\n\
             then one write covers both:\n{write_tail}\n",
            already_staged.join(", "),
            if already_staged.len() == 1 {
                "has"
            } else {
                "have"
            },
            if already_staged.len() == 1 {
                "needs"
            } else {
                "need"
            },
        ),
        // Unreachable: `doomed` is non-empty by the early return above, so
        // every version landed in one list or the other.
        (true, true) => String::new(),
    };

    Err(TapectlError::Other(format!(
        "cannot {act}: \"{volume_label}\" holds the LAST eligible copy of {count} \
         — {named}.\n\
         \n\
         Retiring it leaves {those} at all: nothing else sealed, unquarantined and \
         unretired carries the content, and no recorded warehouse deposit stands in for \
         it. That is not a thinner safety margin to accept — it is the data ceasing to \
         exist, and ADR-0008 puts it in Tier 3. There is no --force for this, and --yes \
         does not reach it.\n\
         \n\
         Make another copy first, then re-run this:\n\
         {copy_out}\n    \
         tapectl volume init <OTHER-LABEL>      (a blank or erased cartridge)\n    \
         tapectl volume write <OTHER-LABEL>\n\
         {on_disk_branch}\
         \n\
         Or give the version up on purpose — a different statement, with its own command \
         and its own preconditions:\n\
         {release}\n\
         \n\
         If you are retiring this tape BECAUSE it no longer reads, say that with the drive \
         rather than with the catalog: `tapectl volume verify {volume_label}` quarantines \
         the volume when the failure PROVES the medium is bad — a checksum mismatch, or a \
         block it cannot read where the layout says data lives. A quarantined volume counts \
         for nothing, and retiring it is then Tier 2 at most. If it instead reports a read \
         or transport failure the volume is left untouched on purpose (ADR-0012): that is \
         the drive talking, not the tape, so clean it, check the block size and the cabling, \
         and verify again. A catalog saying \"one copy, unverified\" is telling the truth; \
         \"no copy\" for data nobody has tried to read is not."
    )))
}

/// **ADR-0008 Tier 2, the gate the retire family never had** (issue #147):
/// the retirement leaves a live version below its RESOLVED policy, but
/// above zero.
///
/// Returns one already-formatted fact line per shortfall, for
/// [`crate::cli::consent::confirm`] to display at the moment consent is
/// asked (ADR-0004: the one place the operator is guaranteed to read
/// them). An empty result means every version the act touches stays at or
/// above policy — no prompt, no flag, exactly as before.
///
/// Zero cannot appear here: zero-after-removal is
/// [`refuse_last_eligible_copy`]'s absolute floor, and every caller runs
/// that first. So "degraded but non-zero", ADR-0008's own words for Tier 2,
/// is a property of the ordering rather than a filter written here.
///
/// **Policy comes from the 3-level resolver** (`policy::resolve`: dotfile >
/// archive_set > defaults), never from `config.defaults` directly — a unit
/// that carries its own `min_copies` must be gated on ITS number, not on
/// the fleet default. The location floor is `required_locations.len()`,
/// which is the resolver's expression of that requirement and exactly what
/// `audit`'s `location_presence` check compares against; an empty list
/// means no location requirement was configured, and silence is then
/// correct rather than a gate on a number nobody set.
///
/// An unresolvable policy PROPAGATES (issue #105's rule, applied at a
/// destructive moment): tapectl cannot say whether the retirement is within
/// policy, and quietly falling back to the weaker defaults would gate — or
/// fail to gate — against a policy the operator never chose.
pub(crate) fn below_policy_facts(
    conn: &Connection,
    config: &Config,
    impacts: &[RetireImpact],
) -> Result<Vec<String>> {
    let mut facts = Vec::new();
    for impact in impacts {
        // Nothing at stake, or every touched version is Tier 3's business
        // already: no policy to resolve and nothing to say.
        if impact.at_stake.iter().all(|v| v.copies_after == 0) {
            continue;
        }
        let unit = queries::get_unit_by_name(conn, &impact.unit_name)?
            .ok_or_else(|| TapectlError::UnitNotFound(impact.unit_name.clone()))?;
        let policy = crate::policy::resolve(conn, config, &unit)?;
        let needed_locations = policy.required_locations.len() as i64;
        for version in &impact.at_stake {
            if version.copies_after == 0 {
                continue;
            }
            if version.copies_after < policy.min_copies {
                facts.push(format!(
                    "unit \"{}\" v{} would be left with {} copy/copies, below its policy of {}",
                    impact.unit_name, version.version, version.copies_after, policy.min_copies
                ));
            }
            if needed_locations > 0 && version.locations_after < needed_locations {
                facts.push(format!(
                    "unit \"{}\" v{} would be left in {} location(s), below its policy of {} ({:?})",
                    impact.unit_name,
                    version.version,
                    version.locations_after,
                    needed_locations,
                    policy.required_locations
                ));
            }
        }
    }
    Ok(facts)
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
            // `last_copy_versions` is ADDITIVE (issue #147) and is the ONLY
            // field that answers the Tier-3 question. `remaining_copies`
            // keeps its meaning exactly -- a per-unit count across any
            // snapshot -- and deliberately cannot be read as the tier: a
            // unit can show 2 there and still be losing the last copy of
            // v2 (issue #153), and show 0 there while this retirement
            // removes nothing at all.
            let last_copy_versions: Vec<i64> = impact
                .at_stake
                .iter()
                .filter(|v| v.copies_after == 0)
                .map(|v| v.version)
                .collect();
            serde_json::json!({
                "unit": impact.unit_name,
                "status": impact.unit_status,
                "remaining_copies": impact.other_copies,
                "last_copy_versions": last_copy_versions,
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

fn print_retire_impact(
    label: &str,
    status: &str,
    condition: &str,
    impacts: &[RetireImpact],
    at_risk: &[String],
) {
    println!("Retiring volume \"{label}\"");
    println!("  Current status: {status}");
    // Issue #242: a quarantined volume can read status "sealed" -- shown
    // before this destructive command runs, not just after.
    println!("  Current condition: {condition}");
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
        // ADR-0008 Tier 3 (issue #147): name the versions this volume is
        // the LAST eligible copy of. In `--dry-run` this is the whole
        // point -- a dry run that stayed silent about an absolute refusal
        // the real run is about to hit would be worse than no dry run.
        for version in impact.at_stake.iter().filter(|v| v.copies_after == 0) {
            println!(
                "      *** v{} — this volume is its LAST eligible copy; retirement is \
                 REFUSED (ADR-0008 Tier 3) ***",
                version.version
            );
        }
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

/// Retire a cartridge permanently: the medium must never be written again
/// (ADR-0011, issue #148).
///
/// This is the cartridge-level peer of [`volume_retire`], and ADR-0011 says
/// so explicitly — "it reuses that analysis rather than growing a second
/// one". So the coverage question is answered by [`retire_impacts`], the
/// one derivation `volume retire`, `unit mark-tape-only`, `compact-finish`
/// and ADR-0010's binding already share.
///
/// **Why the bound volumes are retired too.** ADR-0011's justification for
/// putting this at Tier 2 is that retiring a cartridge "removes a physical
/// copy from every coverage count that policy computes". That is only TRUE
/// if the volumes on it stop counting, and every coverage count in this
/// codebase runs through `policy::coverage`, which is a `volumes.status`
/// predicate. Leaving them `sealed` would mean displaying an impact
/// analysis for a loss that never happens, `audit` going on crediting a
/// medium the operator has declared unfit, and a Tier-2 gate that can never
/// fire. The `cartridge_volumes` mounts stay OPEN — the volume is still
/// physically on the cartridge; it is `cartridge mark-erased` that closes
/// them, because that is when the bytes actually go.
///
/// **Both ADR-0008 tiers, in order** (ADR-0012, issue #147 — this command
/// shipped them inverted too).
///
/// The old reading was that retirement is Tier 2 throughout, because
/// ADR-0011 is explicit that retiring a cartridge is "not an erasure" and
/// the data may still be readable. ADR-0012 overruled it: what the tier
/// turns on is not whether the plastic still holds bits, it is whether the
/// CATALOG will still credit a copy afterwards — and this command's own
/// justification is that it "removes a physical copy from every coverage
/// count that policy computes". A cartridge carrying the last eligible copy
/// of a live version therefore hits the absolute floor
/// ([`refuse_last_eligible_copy`]) before any consent is asked, and no flag
/// reaches past it.
///
/// Everything short of that stays Tier 2, and consent is still asked EVERY
/// time — retiring a medium permanently is a declaration worth confirming
/// even when no unit loses coverage by it. `--force`/`--yes` overrides; a
/// non-interactive session with neither refuses rather than hanging.
/// `cartridge unretire` is the way back out of `retired_permanent` — the
/// operator saying they were wrong about the *medium*; `cartridge
/// mark-erased` is the separate, irreversible statement that the bytes are
/// gone (ADR-0011, corrected 2026-09-14; the command landed with #163 after
/// this branch was written, which is why the tier rewrite below still named
/// mark-erased as the only route back).
#[allow(clippy::too_many_arguments)]
pub fn cartridge_retire(
    conn: &Connection,
    config: &Config,
    barcode: &str,
    reason: Option<&str>,
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

    if status == "retired_permanent" {
        if json_output {
            println!(
                "{}",
                serde_json::json!({
                    "barcode": barcode, "status": status, "changed": false,
                })
            );
        } else {
            println!("cartridge \"{barcode}\" is already retired_permanent; nothing to do");
        }
        return Ok(());
    }

    let mut stmt = conn.prepare(
        "SELECT v.id, v.label, v.status FROM cartridge_volumes cv
         JOIN volumes v ON v.id = cv.volume_id
         WHERE cv.cartridge_id = ?1 AND cv.unmounted_at IS NULL
         ORDER BY v.label",
    )?;
    let mounted: Vec<(i64, String, String)> = stmt
        .query_map(params![id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);
    let volume_labels: Vec<String> = mounted.iter().map(|(_, l, _)| l.clone()).collect();

    // One merged impact per unit across every volume on the cartridge.
    //
    // `bind_cartridge` closes every other open mount when it binds, so in
    // practice a cartridge has AT MOST ONE open mount and this loop runs
    // once. The merge is defensive, and it keeps the WORST reading of each
    // unit (the lowest remaining copy count) so it can never under-report
    // risk at the moment consent is asked.
    let mut merged: Vec<RetireImpact> = Vec::new();
    // Kept per-VOLUME as well as merged: the Tier-3 refusal's recovery
    // commands name a volume (`volume read-slices --from <LABEL>`), so the
    // merge — which deliberately keeps only the worst reading per unit —
    // is the wrong shape to refuse from.
    let mut per_volume: Vec<(String, Vec<RetireImpact>)> = Vec::new();
    for (vol_id, vol_label, _) in &mounted {
        let impacts = retire_impacts(conn, *vol_id)?;
        per_volume.push((vol_label.clone(), impacts.clone()));
        for impact in impacts {
            match merged.iter_mut().find(|m| m.unit_name == impact.unit_name) {
                Some(existing) if impact.other_copies < existing.other_copies => {
                    *existing = impact;
                }
                Some(_) => {}
                None => merged.push(impact),
            }
        }
    }
    merged.sort_by(|a, b| a.unit_name.cmp(&b.unit_name));

    let at_risk: Vec<String> = merged
        .iter()
        .filter(|impact| impact.other_copies == 0)
        .map(|impact| impact.unit_name.clone())
        .collect();

    if dry_run {
        if json_output {
            println!(
                "{}",
                serde_json::json!({
                    "barcode": barcode,
                    "status": status,
                    "volumes_to_retire": volume_labels,
                    "affected_units": retire_impacts_json(&merged),
                    "at_risk_units": at_risk,
                    "dry_run": true,
                })
            );
        } else {
            print_cartridge_retire_impact(barcode, &status, &volume_labels, &merged, &at_risk);
            println!("\n  DRY RUN — no changes made.");
        }
        return Ok(());
    }

    let action = format!("retire cartridge \"{barcode}\" permanently");

    // ADR-0008 TIER 3, the absolute floor (ADR-0012, issue #147). Before
    // consent, and with no `force` in scope to defeat it. Per volume, so
    // the refusal can name the one whose slices have to be copied off.
    for (vol_label, impacts) in &per_volume {
        if let Err(e) = refuse_last_eligible_copy(conn, &action, vol_label, impacts) {
            let reason_text = e.to_string();
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "barcode": barcode,
                        "status": status,
                        "affected_units": retire_impacts_json(&merged),
                        "at_risk_units": at_risk,
                        "consent": "refused",
                        "reason": reason_text,
                    })
                );
            } else {
                // Impact analysis only; `main` prints the floor's own text
                // once, on stderr. See `volume_retire`.
                print_cartridge_retire_impact(barcode, &status, &volume_labels, &merged, &at_risk);
            }
            return Err(e);
        }
    }

    // ADR-0008 TIER 2: consent is asked EVERY time, because retiring a
    // cartridge is an irreversible-by-design declaration about a physical
    // medium even when no unit loses coverage by it. What varies is the
    // facts shown — and, per ADR-0004, the facts are shown at exactly this
    // moment.
    let mut facts: Vec<String> = at_risk
        .iter()
        .map(|name| format!("unit \"{name}\" would have ZERO copies remaining after this"))
        .collect();
    facts.extend(below_policy_facts(conn, config, &merged)?);
    let now = chrono::Utc::now().naive_utc();
    for impact in &merged {
        if impact.other_copies != 0 {
            if let Some(line) =
                crate::policy::evidence::describe(&impact.unit_name, &impact.evidence, now)
            {
                facts.push(line);
            }
        }
    }
    for label in &volume_labels {
        facts.push(format!(
            "volume \"{label}\" is on this cartridge and will be retired with it"
        ));
    }
    facts.push(format!(
        "cartridge \"{barcode}\" will never be written again; \
         `tapectl cartridge unretire {barcode}` is the way back if this is a mistake"
    ));

    if let Err(e) = crate::cli::consent::confirm(&action, &facts, force || assume_yes) {
        let reason_text = e.to_string();
        if json_output {
            println!(
                "{}",
                serde_json::json!({
                    "barcode": barcode,
                    "status": status,
                    "affected_units": retire_impacts_json(&merged),
                    "at_risk_units": at_risk,
                    "consent": "refused",
                    "reason": reason_text,
                })
            );
        } else {
            // Impact analysis only; see `volume_retire`.
            print_cartridge_retire_impact(barcode, &status, &volume_labels, &merged, &at_risk);
        }
        return Err(e);
    }

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE cartridges SET status = 'retired_permanent' WHERE id = ?1",
        params![id],
    )?;
    if let Some(reason) = reason {
        // Append, never overwrite: the note is a maintenance log, and the
        // reason a cartridge was retired is exactly the sort of thing that
        // must not silently replace what someone wrote before it.
        let line = format!(
            "[{}] retired_permanent: {reason}",
            chrono::Utc::now().format("%Y-%m-%d")
        );
        tx.execute(
            "UPDATE cartridges
             SET notes = CASE WHEN notes IS NULL OR notes = '' THEN ?1
                              ELSE notes || char(10) || ?1 END
             WHERE id = ?2",
            params![line, id],
        )?;
    }
    // `log_event` rather than `log_field_change`, for the `details` column:
    // the reason a cartridge was declared unfit is the single most useful
    // thing in its audit trail, and the field-change helper has no room for
    // it (its last parameter is a tenant id).
    events::log_event(
        &tx,
        "cartridge",
        id,
        Some(barcode),
        "retired",
        Some("status"),
        Some(&status),
        Some("retired_permanent"),
        reason,
        None,
    )?;
    for (vol_id, label, prior) in &mounted {
        tx.execute(
            "UPDATE volumes SET status = 'retired' WHERE id = ?1",
            params![vol_id],
        )?;
        events::log_event(
            &tx,
            "volume",
            *vol_id,
            Some(label),
            "retired",
            Some("status"),
            Some(prior),
            Some("retired"),
            Some(&format!(
                "cartridge \"{barcode}\" was retired permanently (ADR-0011); \
                 this volume's medium must never be written again"
            )),
            None,
        )?;
    }
    tx.commit()?;

    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "barcode": barcode,
                "status": "retired_permanent",
                "volumes_retired": volume_labels,
                "affected_units": retire_impacts_json(&merged),
                "at_risk_units": at_risk,
                "changed": true,
            })
        );
    } else {
        print_cartridge_retire_impact(barcode, &status, &volume_labels, &merged, &at_risk);
        println!("  Cartridge \"{barcode}\" retired permanently.");
        for label in &volume_labels {
            println!("  Volume \"{label}\" retired with it.");
        }
        println!("  `tapectl cartridge unretire {barcode}` is the way back if this was a mistake.");
    }
    Ok(())
}

fn print_cartridge_retire_impact(
    barcode: &str,
    status: &str,
    volumes: &[String],
    impacts: &[RetireImpact],
    at_risk: &[String],
) {
    println!("Retiring cartridge \"{barcode}\" permanently");
    println!("  Current status: {status}");
    if volumes.is_empty() {
        println!("  Volumes on it:  none");
    } else {
        println!("  Volumes on it:  {}", volumes.join(", "));
    }
    println!("  Affected units:");
    if impacts.is_empty() {
        println!("    (none)");
    }
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
        // ADR-0008 Tier 3 (issue #147): name the versions this cartridge
        // carries the last eligible copy of, same as `volume retire`.
        for version in impact.at_stake.iter().filter(|v| v.copies_after == 0) {
            println!(
                "      *** v{} — this cartridge holds its LAST eligible copy; retirement \
                 is REFUSED (ADR-0008 Tier 3) ***",
                version.version
            );
        }
        // ADR-0004 Tier 1: evidence age is displayed wherever a destructive
        // operation consumes coverage, and never gates.
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
            "\n  WARNING: {} unit(s) will have ZERO copies after this retirement!",
            at_risk.len()
        );
    }
}

/// The ADR-0008 Tier-2 consent facts for `cartridge_mark_erased` when the
/// cartridge is not already `pending_erase`.
///
/// Issue #163's merged audit finding: an operator consenting to "mark
/// cartridge X erased" is consenting to "these volumes are recorded as
/// having no bytes" just as much as to the cartridge's own status change,
/// and the facts must say so BY NAME, not just by count -- each line reads
/// true alone (the #91 lesson: `cli::consent::confirm` prints facts as
/// standalone lines).
fn mark_erased_consent_facts(barcode: &str, status: &str, volume_labels: &[String]) -> Vec<String> {
    let mut facts = vec![format!(
        "cartridge \"{barcode}\" is in status \"{status}\", not \"pending_erase\" -- \
         marking it erased skips the normal bulk-erase lifecycle checkpoint"
    )];
    for label in volume_labels {
        facts.push(format!(
            "volume \"{label}\" is mounted on cartridge \"{barcode}\" and will be recorded \
             as erased -- its bytes declared gone"
        ));
    }
    facts
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
/// constraint (`available|in_use|pending_erase|retired_permanent` since
/// migration 012 — ADR-0011 took `offsite` out, because a cartridge's
/// place is a location) — only `volumes.status` does. So this command's
/// own namesake mutation is the cartridge moving to `'available'` (freed
/// for reuse); it is the volume(s) that were mounted on it that move to
/// `'erased'`.
///
/// `cartridge unretire` — not this command — is the way out of
/// `retired_permanent` (ADR-0011, corrected 2026-09-14): the operator
/// saying they were wrong about the medium being unfit. The lifecycle
/// diagram in that correction draws NO edge from `retired_permanent`
/// through `mark-erased` at all, so this command REFUSES a
/// `retired_permanent` cartridge outright, before the consent branch
/// below and with no `force`/`--yes` parameter reaching it — structurally
/// like `binding::refuse_retired` on the write path (issue #207). Writing
/// `status = 'available'` here, even under consent, would silently reverse
/// the operator's "never write this medium again" declaration; no amount
/// of consent makes a medium declared permanently unfit fit again.
///
/// Issue #163 audit finding: the ADR-0008 Tier-2 consent facts named only
/// the cartridge's status, never the volumes about to be recorded erased —
/// an operator consenting to "the bytes are gone" could not read which
/// tapes that was about. [`mark_erased_consent_facts`] fixes that, split
/// out so the exact wording is assertable without stdout capture (the
/// `retire_refusal_json` pattern above).
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

    // ADR-0011 (corrected 2026-09-14): `cartridge unretire` REPLACED
    // mark-erased as the way back from `retired_permanent` -- the
    // lifecycle diagram draws no edge from that state through this
    // command. This is a fact about the medium, not a risk to accept, so
    // -- structurally like `binding::refuse_retired` on the write path --
    // it takes no `force`/`--yes` parameter at all and is checked before
    // the consent branch below (issue #207).
    if status == "retired_permanent" {
        return Err(TapectlError::Other(format!(
            "cartridge \"{barcode}\" is retired_permanent and cannot be marked erased: \
             that would also return it to available, and no amount of consent makes a \
             medium declared permanently unfit fit again (ADR-0011) -- there is no \
             --force/--yes for this. If the cartridge is in fact usable, say so with \
             `tapectl cartridge unretire {barcode}`, \
             which is the way back."
        )));
    }

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
        let facts = mark_erased_consent_facts(barcode, &status, &volume_labels);
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

/// Reverse a `cartridge retire` (issue #163 / ADR-0012's consequences
/// bullet).
///
/// Tier 1 under ADR-0008: a correction of a claim about the *medium*, not
/// a destructive act — no prompt, no `--force`, no `--yes`. Refuses when
/// the cartridge is not `retired_permanent`, naming its actual status.
///
/// The prior status of the cartridge, and of each volume retired with it,
/// is recovered from the `events` audit trail rather than a new column:
/// `cartridge_retire` already logs each change (`action = "retired"`,
/// `field = "status"`, `old_value` = the status just before), so the most
/// recent such event per entity IS the fact this command needs.
/// `ORDER BY id DESC` — not `timestamp`, which is only second-resolution —
/// makes "most recent" exact even when two events land in the same
/// second. Read per entity, independently: a volume that was already
/// `retired` (via `volume retire`) before the cartridge retirement has its
/// own `retired -> retired` event and is correctly restored to `retired`,
/// not resurrected into something it never was.
///
/// If an entity's own retirement event is missing (a catalog rebuilt from
/// tape mints no `events` history), that entity's status is NOT guessed:
/// the cartridge falls back to `available` — its ordinary pre-retirement
/// state — and a volume with no recoverable event is left exactly as it
/// is, `retired`. Both cases are named in the output; an honest partial
/// restore beats a guessed one.
///
/// `cartridge mark-erased` is untouched by this command and remains the
/// separate, irreversible statement that the bytes are gone (ADR-0011,
/// corrected 2026-09-14).
pub fn cartridge_unretire(
    conn: &Connection,
    barcode: &str,
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

    if status != "retired_permanent" {
        return Err(TapectlError::Other(format!(
            "cartridge \"{barcode}\" is not retired_permanent (status: \"{status}\"); \
             nothing to unretire"
        )));
    }

    let prior_cartridge_status: Option<String> = conn
        .query_row(
            "SELECT old_value FROM events
             WHERE entity_type = 'cartridge' AND entity_id = ?1
               AND action = 'retired' AND field = 'status'
             ORDER BY id DESC LIMIT 1",
            params![id],
            |row| row.get(0),
        )
        .optional()?;
    let cartridge_event_found = prior_cartridge_status.is_some();
    let restored_cartridge_status =
        prior_cartridge_status.unwrap_or_else(|| "available".to_string());

    let mut stmt = conn.prepare(
        "SELECT v.id, v.label, v.status FROM cartridge_volumes cv
         JOIN volumes v ON v.id = cv.volume_id
         WHERE cv.cartridge_id = ?1 AND cv.unmounted_at IS NULL
         ORDER BY v.label",
    )?;
    let mounted: Vec<(i64, String, String)> = stmt
        .query_map(params![id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);

    // Only volumes still `retired` are this command's business: anything
    // else (already erased, or moved on some other route since) is not
    // this retirement's doing to undo.
    let mut restorable: Vec<(i64, String, Option<String>)> = Vec::new();
    for (vol_id, label, vol_status) in &mounted {
        if vol_status != "retired" {
            continue;
        }
        let prior: Option<String> = conn
            .query_row(
                "SELECT old_value FROM events
                 WHERE entity_type = 'volume' AND entity_id = ?1
                   AND action = 'retired' AND field = 'status'
                 ORDER BY id DESC LIMIT 1",
                params![vol_id],
                |row| row.get(0),
            )
            .optional()?;
        restorable.push((*vol_id, label.clone(), prior));
    }
    let volumes_restored: Vec<(String, String)> = restorable
        .iter()
        .filter_map(|(_, label, prior)| prior.as_ref().map(|p| (label.clone(), p.clone())))
        .collect();
    let volumes_not_restored: Vec<String> = restorable
        .iter()
        .filter(|(_, _, prior)| prior.is_none())
        .map(|(_, label, _)| label.clone())
        .collect();

    if dry_run {
        if json_output {
            println!(
                "{}",
                serde_json::json!({
                    "barcode": barcode,
                    "status": status,
                    "restored_status": restored_cartridge_status,
                    "cartridge_event_found": cartridge_event_found,
                    "volumes_restored": volumes_restored,
                    "volumes_not_restored": volumes_not_restored,
                    "dry_run": true,
                })
            );
        } else {
            println!("cartridge \"{barcode}\" would be unretired");
            println!("  status: retired_permanent -> {restored_cartridge_status}");
            if !cartridge_event_found {
                println!(
                    "  (no retirement event found -- falling back to \"available\" rather \
                     than a guessed status)"
                );
            }
            for (label, prior) in &volumes_restored {
                println!("  volume \"{label}\" would be restored to \"{prior}\"");
            }
            for label in &volumes_not_restored {
                println!(
                    "  volume \"{label}\" would be LEFT AS \"retired\" -- no retirement \
                     event found to recover its prior status"
                );
            }
            println!("\n  DRY RUN — no changes made.");
        }
        return Ok(());
    }

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE cartridges SET status = ?1 WHERE id = ?2",
        params![restored_cartridge_status, id],
    )?;
    events::log_field_change(
        &tx,
        "cartridge",
        id,
        barcode,
        "unretired",
        "status",
        Some(&status),
        &restored_cartridge_status,
        None,
    )?;
    for (vol_id, label, prior) in &restorable {
        if let Some(prior_status) = prior {
            tx.execute(
                "UPDATE volumes SET status = ?1 WHERE id = ?2",
                params![prior_status, vol_id],
            )?;
            events::log_field_change(
                &tx,
                "volume",
                *vol_id,
                label,
                "unretired",
                "status",
                Some("retired"),
                prior_status,
                None,
            )?;
        }
    }
    tx.commit()?;

    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "barcode": barcode,
                "status": restored_cartridge_status,
                "cartridge_event_found": cartridge_event_found,
                "volumes_restored": volumes_restored,
                "volumes_not_restored": volumes_not_restored,
                "changed": true,
            })
        );
    } else {
        println!(
            "cartridge \"{barcode}\" unretired: retired_permanent -> {restored_cartridge_status}"
        );
        if !cartridge_event_found {
            println!(
                "  no retirement event found for this cartridge -- restored to \"available\" \
                 rather than a guessed status"
            );
        }
        for (label, prior) in &volumes_restored {
            println!("  volume \"{label}\" restored to \"{prior}\"");
        }
        for label in &volumes_not_restored {
            println!(
                "  volume \"{label}\" left as \"retired\" -- no retirement event found to \
                 recover its prior status"
            );
        }
    }
    Ok(())
}

/// Issue #153 / ADR-0012: which of a unit's `'current'` versions has the
/// FEWEST copies, and how many. `copy_count_expr` under
/// `CoverageScope::Unit { current_only: true }` already returns exactly
/// this minimum as a single number ("a unit is as covered as its
/// least-covered live version") — this answers the operator's next
/// question, staring at that number: WHICH version, out of how many.
struct ThinnestCurrentVersion {
    version: i64,
    copies: i64,
    total_current_versions: i64,
}

impl ThinnestCurrentVersion {
    /// A standalone-safe evidence line (the #91 lesson: this is printed
    /// with zero surrounding context, so it must read as true alone).
    /// Naming the VERSION — not the unit — is load-bearing: "coverage
    /// rests on 1 copy" would misread as the unit's overall coverage,
    /// when `copy_count` (the unit-wide minimum, printed alongside this)
    /// already IS that number; this line exists only to say which
    /// version it came from.
    fn describe(&self) -> String {
        format!(
            "version {} of {} current versions has the fewest copies: {}",
            self.version, self.total_current_versions, self.copies
        )
    }
}

/// Query, not expression string (unlike [`crate::policy::coverage`]'s
/// functions): this needs the actual per-version numbers to report which
/// version is thinnest, not just the aggregate minimum. Built on the same
/// [`crate::policy::coverage::copy_count_expr`], scoped per snapshot, so
/// it can never disagree with the unit-wide count it explains. Returns
/// `None` for a unit with no current snapshot.
fn thinnest_current_version(
    conn: &Connection,
    unit_id: i64,
) -> Result<Option<ThinnestCurrentVersion>> {
    let per_snapshot = crate::policy::coverage::CoverageQuery {
        scope: crate::policy::coverage::CoverageScope::Snapshot { id_expr: "pcv.id" },
        exclude_volume: None,
    };
    let copy_expr = crate::policy::coverage::copy_count_expr(&per_snapshot);
    let sql = format!(
        "SELECT pcv.version, {copy_expr} AS c
         FROM snapshots pcv
         WHERE pcv.unit_id = ?1 AND pcv.status = 'current'
         ORDER BY c ASC, pcv.version ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<(i64, i64)> = stmt
        .query_map(params![unit_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let total = rows.len() as i64;
    Ok(rows
        .into_iter()
        .next()
        .map(|(version, copies)| ThinnestCurrentVersion {
            version,
            copies,
            total_current_versions: total,
        }))
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

    // Issue #153 / ADR-0012: `copy_count` above is already the MINIMUM
    // across the unit's current versions -- this names WHICH version that
    // minimum belongs to, so the operator isn't left staring at a single
    // number for a unit that may carry several.
    let thinnest_version = thinnest_current_version(conn, unit.id)?;

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
        let thinnest_json = thinnest_version.as_ref().map(|t| {
            serde_json::json!({
                "version": t.version,
                "copies": t.copies,
                "total_current_versions": t.total_current_versions,
                "note": t.describe(),
            })
        });
        println!(
            "{}",
            serde_json::json!({
                "unit": unit_name,
                "status": "tape_only",
                "copies": copy_count,
                "locations": location_count,
                "evidence": evidence_json,
                "evidence_summary": evidence_summary,
                "thinnest_current_version": thinnest_json,
            })
        );
    } else {
        println!(
            "unit \"{unit_name}\" marked tape-only ({copy_count} copies, {location_count} locations)"
        );
        if let Some(t) = &thinnest_version {
            println!("  {}", t.describe());
        }
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
            "exported {} slices ({}) to {}",
            slices.len(),
            crate::util::format_bytes_binary(total),
            dest_dir,
        );
    }
    Ok(())
}

/// The volumes whose `interrupted` write rows a delete of `snap_id` will
/// remove (issue #176 step 4).
///
/// Extracted so the rule can be tested directly. It used to be inline, and
/// its test asserted on CAPTURED TRACING OUTPUT — which is not deterministic:
/// `tracing`'s per-callsite `Interest` is process-global, so under a parallel
/// `cargo test` whichever thread reaches the `warn!` first can decide the
/// callsite is uninteresting for every later one, and the capture comes back
/// empty. That flake reached master and failed a coordinator gate on
/// 2026-09-16; `rebuild_interest_cache()` narrowed the window but did not
/// close it. The SQL is the part worth pinning, and it needs no subscriber.
fn interrupted_write_volumes(conn: &Connection, snap_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT v.label FROM writes w
         JOIN volumes v ON v.id = w.volume_id
         WHERE w.snapshot_id = ?1 AND w.status = 'interrupted'",
    )?;
    let rows = stmt.query_map(params![snap_id], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
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

    // `writes.session_dir` (migration 006) is the only handle a restarted
    // process has on a resumable write session's staging directory. Same
    // reasoning as `staging_paths` above (issue #176, mirrors #55):
    // collected before the transaction, removed after commit, and only for
    // a directory no OTHER writes row still references — a multi-unit
    // `collection run` session shares one `session_dir` across snapshots.
    let session_dirs: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT session_dir FROM writes
             WHERE snapshot_id = ?1 AND session_dir IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![snap_id], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };

    // Any `writes` row this delete removes that sits at `interrupted` names
    // a write session that can never be resumed again — its Layout
    // referenced slices this command is about to drop, and resume's
    // revalidation would refuse it anyway. #94 settled that only the
    // operator judges a session unrecoverable, so this collects a warning
    // naming the volume rather than auto-aborting the row.
    let interrupted_volumes = interrupted_write_volumes(conn, snap_id)?;

    // Cascade delete: verification_results -> write_positions -> writes ->
    // stage_slices -> stage_sets -> manifest_entries -> manifests -> files
    // -> snapshot
    //
    // One transaction, including the event (issue #55): as bare
    // `conn.execute` calls, a failure partway left a half-deleted snapshot —
    // e.g. `stage_slices` gone but `stage_sets` still present, referencing
    // slices that no longer exist. Mirrors `snapshot_purge` above, which
    // already had this treatment.
    //
    // The new head (issue #176): a snapshot's `writes` row can sit at any
    // non-`completed` status (the guard above only refuses `completed`),
    // and `write_positions`/`writes` are never touched by the rest of the
    // cascade, so their FKs on `stage_slices`/`stage_sets`/`snapshots`
    // tripped on the very first DELETE below. `verification_results` rows
    // (written on a failed confirm, or by a later `volume verify`) point at
    // both `write_positions` and `stage_slices`, so they must go first;
    // the `stage_slice_id` predicate alone covers both FKs, because every
    // such row's `write_position_id` and `stage_slice_id` come from the
    // same `write_positions` row. `verification_sessions` is evidence about
    // the *volume*, not this snapshot, and is never touched.
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM verification_results WHERE stage_slice_id IN
         (SELECT sl.id FROM stage_slices sl
          JOIN stage_sets ss ON ss.id = sl.stage_set_id
          WHERE ss.snapshot_id = ?1)",
        params![snap_id],
    )?;
    tx.execute(
        "DELETE FROM write_positions WHERE write_id IN
         (SELECT id FROM writes WHERE snapshot_id = ?1)",
        params![snap_id],
    )?;
    tx.execute(
        "DELETE FROM writes WHERE snapshot_id = ?1",
        params![snap_id],
    )?;
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

    // Same after-commit treatment as the staged slices above, for the same
    // reason: a rolled-back delete that had already removed a session
    // directory would leave a live, resumable snapshot pointing at
    // nothing. A directory another surviving `writes` row still names (a
    // multi-unit `collection run` session) is left alone — `staging
    // clean`'s RETAIN/RECLAIM rules still govern it.
    let mut session_dirs_removed = 0usize;
    for dir in &session_dirs {
        // Fail OPEN on the query and CLOSED on the removal. The delete is
        // already committed by this point, so propagating an error here
        // would report failure for work that actually succeeded — and the
        // operator's natural retry would then answer "snapshot not found".
        // That is the same principle the `remove_dir_all` below already
        // follows (warn, never error). Not being able to prove a directory
        // is unreferenced is also the wrong moment to delete it, so an
        // unreadable count skips the removal rather than forcing it.
        let still_referenced: i64 = match conn.query_row(
            "SELECT COUNT(*) FROM writes WHERE session_dir = ?1",
            params![dir],
            |row| row.get(0),
        ) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    dir = %dir,
                    error = %e,
                    "could not tell whether this write session directory is still \
                     referenced; leaving it in place (`tapectl staging clean` will \
                     reconsider it)"
                );
                continue;
            }
        };
        if still_referenced > 0 {
            continue;
        }
        match std::fs::remove_dir_all(dir) {
            Ok(()) => session_dirs_removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(dir = %dir, error = %e, "could not remove write session directory")
            }
        }
    }
    if session_dirs_removed > 0 {
        tracing::info!(
            count = session_dirs_removed,
            snapshot = %format!("{unit_name}/v{version}"),
            "removed write session directories no snapshot references any more"
        );
    }

    // Issue #176 step 4: an `interrupted` write's session cannot be resumed
    // once its slices are gone -- #94 settled that only the operator
    // decides a session is unrecoverable, so this names the volume rather
    // than auto-aborting the sibling `writes` row (already deleted above).
    let mut warnings = Vec::new();
    for label in &interrupted_volumes {
        let msg = format!(
            "snapshot {unit_name} v{version} had an interrupted write on volume \"{label}\" \
             -- that session can no longer be resumed now that its slices are gone; \
             run `tapectl volume abort {label}` before reusing the cartridge"
        );
        tracing::warn!(volume = %label, unit = %unit_name, version, "{msg}");
        warnings.push(msg);
    }

    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "unit": unit_name,
                "version": version,
                "deleted": true,
                "warnings": warnings,
            })
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
/// 4. Issue #177: reporting covered 2 of 35 declared FK edges via two
///    hand-rolled `writes`/`stage_slices` scans, and `--repair`'s DELETEs
///    ran under immediate FK enforcement, so any orphan with children of
///    its own (a `writes` row with `write_positions`, a `stage_slices` row
///    with `write_positions`/`sacrificed_slice_id`/`verification_results`)
///    tripped the very constraint it was trying to close and rolled the
///    whole repair back. Reporting now runs `pragma_foreign_key_check`,
///    which is schema-derived and covers every edge without hand-keeping
///    a table list. Repair sets `PRAGMA defer_foreign_keys = ON` for the
///    transaction and loops (check → delete each distinct offending row →
///    check again) until the graph is closed, then commits — deferred
///    checks mean deletion order inside the loop does not matter, only
///    that nothing dangling is left by COMMIT. A **static leaf-first
///    table list was considered and rejected**: it is the same defect
///    (two scans vs. 35 edges) waiting for the next migration to add a
///    36th edge nobody remembers to add to the list.
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

    // Foreign-key check — schema-derived, covers every declared edge (issue
    // #177). Run regardless of the integrity_check outcome above.
    report.issues.extend(foreign_key_check_issues(conn)?);

    if repair {
        match repair_foreign_key_violations(conn) {
            Ok((repaired, _by_table)) => report.repaired = repaired,
            Err(e) => {
                // The pre-repair findings are already in `report.issues` —
                // attach them so a failed repair never surfaces a bare
                // "FOREIGN KEY constraint failed" with no context (issue
                // #177 Defect 3). After the fix above this path should be
                // unreachable for FK reasons; it stays as the belt.
                let issues_text = if report.issues.is_empty() {
                    "(none)".to_string()
                } else {
                    report.issues.join("; ")
                };
                return Err(TapectlError::Other(format!(
                    "db fsck --repair failed: {e}. pre-repair issues: {issues_text}"
                )));
            }
        }
    }

    Ok(report)
}

/// Read `pragma_foreign_key_check` and render one `issues` line per
/// (child table, parent table, constraint index) group — issue #177.
/// Grouping by constraint index (not just child+parent) keeps two
/// different columns on the same child that both reference the same
/// parent table (e.g. `volume_movements.from_location` and
/// `.to_location`, both -> `locations`) as separate findings, while a
/// single row with several dangling columns still contributes to
/// several groups without inflating the reported row COUNT within any
/// one group beyond that row's single appearance in it.
fn foreign_key_check_issues(conn: &Connection) -> Result<Vec<String>> {
    let rows: Vec<(String, i64, String, i64)> = {
        let mut stmt =
            conn.prepare("SELECT \"table\", rowid, parent, fkid FROM pragma_foreign_key_check")?;
        let mapped = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;
        mapped
    };

    let mut groups: std::collections::BTreeMap<(String, String, i64), Vec<i64>> =
        std::collections::BTreeMap::new();
    for (table, rowid, parent, fkid) in rows {
        groups.entry((table, parent, fkid)).or_default().push(rowid);
    }

    let mut issues = Vec::with_capacity(groups.len());
    for ((table, parent, _fkid), mut rowids) in groups {
        rowids.sort_unstable();
        rowids.dedup();
        let shown: Vec<String> = rowids.iter().take(5).map(i64::to_string).collect();
        let more = if rowids.len() > shown.len() {
            ", ..."
        } else {
            ""
        };
        let (noun, verb) = if rowids.len() == 1 {
            ("row", "references")
        } else {
            ("rows", "reference")
        };
        issues.push(format!(
            "foreign_key_check: {} {noun} in {table} {verb} missing {parent} (rowids {}{more})",
            rowids.len(),
            shown.join(", "),
        ));
    }
    Ok(issues)
}

/// Delete every row `pragma_foreign_key_check` names, children first in
/// effect (not by a hand-kept order — see `db_fsck`'s issue #177 note),
/// inside one transaction with `PRAGMA defer_foreign_keys = ON` so
/// deleting a row that is still referenced does not itself trip
/// immediate FK enforcement mid-transaction. Loops: check -> delete each
/// distinct (table, rowid) it named -> check again -> until empty, then
/// commits. A row named for three dangling columns is one row, deleted
/// once. Returns the total distinct rows deleted and a per-table
/// breakdown for the audit event; on any failure the transaction is
/// dropped (rolled back) and the error is returned to the caller, who
/// attaches the pre-repair findings.
fn repair_foreign_key_violations(
    conn: &Connection,
) -> Result<(usize, std::collections::BTreeMap<String, usize>)> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch("PRAGMA defer_foreign_keys = ON")?;

    let mut deleted_by_table: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    loop {
        let violations: Vec<(String, i64)> = {
            let mut stmt =
                tx.prepare("SELECT DISTINCT \"table\", rowid FROM pragma_foreign_key_check")?;
            let mapped = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            mapped
        };
        if violations.is_empty() {
            break;
        }
        for (table, rowid) in violations {
            let n = tx.execute(
                &format!("DELETE FROM \"{table}\" WHERE rowid = ?1"),
                params![rowid],
            )?;
            *deleted_by_table.entry(table).or_insert(0) += n;
        }
    }

    let repaired: usize = deleted_by_table.values().sum();
    if repaired > 0 {
        let detail = deleted_by_table
            .iter()
            .map(|(table, count)| format!("{table}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        events::log_event(
            &tx,
            "system",
            0,
            None,
            "db_fsck_repair",
            None,
            None,
            None,
            Some(&format!("deleted {detail}")),
            None,
        )?;
    }
    tx.commit()?;
    Ok((repaired, deleted_by_table))
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
///
/// `device` selects WHICH configured drive the imported volume belongs to
/// (issue #151). Before it existed this resolved with `None` and errored
/// outright on a multi-drive config — the command had a `backend_name`
/// column to fill and nothing to fill it from. Resolution is LENIENT
/// (`config::resolve_device`, ADR-0010) rather than strict, because this
/// command writes one catalog row and never opens the device: a path no
/// backend claims, or no backend at all, falls back to the backend TYPE
/// string so the row stays self-consistent, exactly as before.
///
/// `generation` is validated exactly as `cartridge register --generation`
/// is (`media::parse_generation_or_error`) and the row's `media_type` stores
/// the CANONICAL spelling, never the operator's raw one — ADR-0010 promises
/// every `volumes.media_type` row "always hold[s] a generation string
/// tapectl can parse", and this was the one writer that broke it (issue
/// #169; there is no more unvalidated default). `capacity`, when omitted,
/// follows ADR-0010 decision 3's plain form for a fresh row with no drive
/// override and no cartridge row yet bound: the generation table's native
/// capacity.
#[allow(clippy::too_many_arguments)]
pub fn volume_import(
    conn: &Connection,
    config: &Config,
    label: &str,
    backend: &str,
    generation: &str,
    capacity: Option<&str>,
    device: Option<&str>,
    notes: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let parsed = crate::media::parse_generation_or_error(generation)?;
    let canonical_generation = parsed.as_str();
    let (cap_bytes, capacity_display) = match capacity {
        Some(c) => (crate::media::parse_capacity_to_bytes(c)?, c.to_string()),
        None => {
            let bytes = parsed.native_capacity_bytes();
            (
                bytes as i64,
                format!("{bytes} bytes, the {canonical_generation} default"),
            )
        }
    };
    // Resolve backend_name from the configured backend this device names,
    // else fall back to the type string so the row remains self-consistent.
    let backend_name = match backend {
        "lto" => crate::config::resolve_device(config, device)
            .ok()
            .and_then(|(_, b)| b)
            .map(|b| b.name.clone())
            .unwrap_or_else(|| backend.to_string()),
        _ => backend.to_string(),
    };
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status, notes)
         VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6)",
        rusqlite::params![
            label,
            backend,
            backend_name,
            canonical_generation,
            cap_bytes,
            notes
        ],
    )?;
    let vol_id = conn.last_insert_rowid();
    crate::db::events::log_created(conn, "volume", vol_id, label, None)?;
    if json_output {
        println!(
            "{}",
            serde_json::json!({"id": vol_id, "label": label, "status": "imported"})
        );
    } else {
        println!(
            "volume \"{label}\" imported (id={vol_id}, {canonical_generation}, {capacity_display})"
        );
    }
    Ok(())
}
/// What `quick-archive` says when its `--volume` label does not exist.
///
/// Pure, and separate from the check, for the same reason
/// `config::no_lto_backend_error` is (#124b): the bare
/// `error: volume not found: VOL-Q` said nothing about the contract or the
/// fix, and cost a real debugging detour — the lifecycle suite's quick-archive
/// scenario had no `volume init` at all, and #128 recorded the resulting
/// failure as a single-cartridge media limitation rather than a missing setup
/// step.
/// The labels this archive already has, newest first, for the missing-volume
/// error to name.
///
/// The error used to end "`tapectl volume list` shows the labels you already
/// have". There is no `volume list` subcommand (issue #190) — and there is no
/// other command that lists volume labels either: `report capacity` and
/// `report summary` give counts, `catalog locate` answers a different question,
/// and `report copies` lists only volumes that already carry a unit, so it
/// cannot show the initialized-but-unwritten volume this error is about. So the
/// error answers the question itself instead of naming a command.
fn known_volume_labels(conn: &Connection) -> Vec<String> {
    let mut stmt = match conn.prepare(
        "SELECT label FROM volumes ORDER BY COALESCE(first_write, created_at) DESC, id DESC",
    ) {
        Ok(s) => s,
        // Listing existing labels is a courtesy on an error path; failing to
        // gather them must never replace the real error with a database one.
        Err(_) => return Vec::new(),
    };
    let rows = match stmt.query_map([], |r| r.get::<_, String>(0)) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    rows.filter_map(|r| r.ok()).collect()
}

fn missing_volume_error(volume: &str, device: &str, known: &[String]) -> TapectlError {
    // At most ten, so an archive with hundreds of tapes does not bury the
    // instruction above it; the count tells the operator there are more.
    const SHOWN: usize = 10;
    let have = if known.is_empty() {
        "This archive has no volumes yet — the command above creates its first.".to_string()
    } else {
        let listed = known
            .iter()
            .take(SHOWN)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        if known.len() > SHOWN {
            format!(
                "Volumes you already have ({} total, most recent first): {listed}, …",
                known.len()
            )
        } else {
            format!("Volumes you already have: {listed}")
        }
    };
    TapectlError::Other(format!(
        "volume \"{volume}\" does not exist\n\n\
         quick-archive writes to a volume that is already initialized — it does \
         not create one.\n\
         Initialize it first, then re-run this command:\n\n    \
         tapectl volume init {volume} --device {device}\n\n\
         {have}"
    ))
}

/// Does a volume with this label exist? Separated from `quick_archive` so the
/// pre-flight can be tested without a tape device or a staging pipeline.
fn volume_exists(conn: &Connection, label: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM volumes WHERE label = ?1",
        rusqlite::params![label],
        |row| row.get(0),
    )?;
    Ok(n > 0)
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
    device: Option<&str>,
    json_output: bool,
) -> Result<()> {
    // quick-archive ends in `volume write`, so `--device` resolves STRICTLY
    // (ADR-0010, "Backends resolve by device"): the drive must be a
    // configured backend, because the write path needs its usable-capacity
    // factor, ENOSPC buffer and sg node.
    let device = &crate::cli::write_device(config, device)?;

    // Step 0: the volume must already exist.
    //
    // Without this, the label is not looked up until deep inside
    // `volume_write` — by which time steps 1-3 have created a unit, taken a
    // snapshot and staged an encrypted slice set. The operator saw three
    // success lines followed by a bare "volume not found", with staged data
    // left behind to clean up (#132). Checking first costs one query and makes
    // the failure free.
    //
    // quick-archive deliberately does NOT initialize the volume itself. That
    // keeps `volume init`'s device/blank-cartridge checks and ADR-0003's
    // refusal to touch a sealed tape in one place. Whether "quick" ought to
    // imply auto-init is a separate question, left open on #132.
    if !volume_exists(conn, volume)? {
        return Err(missing_volume_error(
            volume,
            device,
            &known_volume_labels(conn),
        ));
    }

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
    crate::volume::write::volume_write(
        conn,
        paths,
        config,
        volume,
        device,
        512 * 1024,
        false,
        false,
    )?;
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

    /// #132: quick-archive's `--volume` must already exist, and the failure
    /// has to say so. The old message was a bare "volume not found", raised
    /// deep inside `volume_write` — after a unit, a snapshot and a staged
    /// slice set had already been created and left behind.
    #[test]
    fn a_missing_quick_archive_volume_names_the_command_that_creates_it() {
        let conn = crate::db::open_memory().unwrap();
        assert!(!volume_exists(&conn, "VOL-Q").unwrap());

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('VOL-Q', 'lto', 'lto', 'LTO-6', 1, 'initialized')",
            [],
        )
        .unwrap();
        assert!(volume_exists(&conn, "VOL-Q").unwrap());

        let msg =
            missing_volume_error("VOL-Q", "/dev/nst3", &known_volume_labels(&conn)).to_string();
        assert!(msg.contains("does not exist"), "{msg}");
        assert!(
            msg.contains("tapectl volume init VOL-Q --device /dev/nst3"),
            "the error must name the exact command, with the device the \
             operator actually passed:\n{msg}"
        );
        assert!(
            msg.contains("does not create one"),
            "the contract must be stated, not implied:\n{msg}"
        );
        // Issue #190: the error used to end by naming `tapectl volume list`,
        // which does not exist. Nothing lists volume labels, so the error
        // lists them itself.
        assert!(
            !msg.contains("volume list"),
            "must not name a subcommand that does not exist:\n{msg}"
        );
        assert!(
            msg.contains("VOL-Q") && msg.contains("Volumes you already have"),
            "the error must NAME the labels the archive has:\n{msg}"
        );
    }

    #[test]
    fn a_missing_volume_on_an_empty_archive_says_so_rather_than_listing_nothing() {
        let conn = crate::db::open_memory().unwrap();
        let msg =
            missing_volume_error("VOL-Q", "/dev/nst3", &known_volume_labels(&conn)).to_string();
        assert!(
            msg.contains("no volumes yet"),
            "an empty archive must say so, not print an empty list:\n{msg}"
        );
        assert!(!msg.contains("Volumes you already have"), "{msg}");
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
    /// Issue #151: `import` had a `backend_name` column to fill and no way
    /// for the operator to say which drive the volume came off, so it
    /// resolved with `None` — fine with one drive, an outright error with
    /// two, and silently "whichever was first" in spirit.
    mod import_device {
        use super::*;
        use crate::config::{Config, LtoBackendConfig};

        fn backend(name: &str, device_tape: &str) -> LtoBackendConfig {
            LtoBackendConfig {
                name: name.to_string(),
                device_tape: device_tape.to_string(),
                device_sg: "/dev/null".to_string(),
                generation: "LTO-6".to_string(),
                capacity_override: None,
                usable_capacity_factor: 0.92,
                enospc_buffer: "50M".to_string(),
            }
        }

        fn backend_name_of(conn: &Connection, label: &str) -> String {
            conn.query_row(
                "SELECT backend_name FROM volumes WHERE label = ?1",
                rusqlite::params![label],
                |r| r.get(0),
            )
            .unwrap()
        }

        /// The regression: two drives configured, and `--device` picks the
        /// one the operator names rather than erroring or guessing.
        #[test]
        fn device_selects_which_of_two_backends_is_recorded() {
            let conn = crate::db::open_memory().unwrap();
            let mut config = Config::default();
            config.backends.lto.push(backend("lto-a", "/dev/null"));
            config.backends.lto.push(backend("lto-b", "/dev/zero"));

            volume_import(
                &conn,
                &config,
                "L6-IMP",
                "lto",
                "LTO-6",
                Some("2500G"),
                Some("/dev/zero"),
                None,
                false,
            )
            .unwrap();
            assert_eq!(backend_name_of(&conn, "L6-IMP"), "lto-b");
        }

        /// No flag, one drive: unchanged from before #151.
        #[test]
        fn no_device_with_one_backend_is_unchanged() {
            let conn = crate::db::open_memory().unwrap();
            let mut config = Config::default();
            config.backends.lto.push(backend("lto-a", "/dev/null"));

            volume_import(
                &conn,
                &config,
                "L6-IMP",
                "lto",
                "LTO-6",
                Some("2500G"),
                None,
                None,
                false,
            )
            .unwrap();
            assert_eq!(backend_name_of(&conn, "L6-IMP"), "lto-a");
        }

        /// LENIENT on purpose (ADR-0010's read-path rule): `import` writes
        /// one catalog row and never opens the device, so no configured
        /// backend at all still imports, falling back to the backend TYPE
        /// so the row stays self-consistent. A strict resolver here would
        /// make the DR machine — keys restored, no `backend add` yet —
        /// unable to register the tapes it is holding.
        #[test]
        fn no_backend_configured_still_imports_under_the_type_name() {
            let conn = crate::db::open_memory().unwrap();
            let config = Config::default();

            volume_import(
                &conn,
                &config,
                "L6-IMP",
                "lto",
                "LTO-6",
                Some("2500G"),
                None,
                None,
                false,
            )
            .unwrap();
            assert_eq!(backend_name_of(&conn, "L6-IMP"), "lto");
        }

        /// A `--device` no backend claims is likewise not an error, for the
        /// same reason.
        #[test]
        fn an_unclaimed_device_falls_back_to_the_type_name() {
            let conn = crate::db::open_memory().unwrap();
            let mut config = Config::default();
            config.backends.lto.push(backend("lto-a", "/dev/null"));

            volume_import(
                &conn,
                &config,
                "L6-IMP",
                "lto",
                "LTO-6",
                Some("2500G"),
                Some("/dev/nst9"),
                None,
                false,
            )
            .unwrap();
            assert_eq!(backend_name_of(&conn, "L6-IMP"), "lto");
        }
    }

    /// Issue #168: `import --capacity` writes straight into
    /// `volumes.capacity_bytes`, so it means the same decimal unit as
    /// `cartridge register --capacity` and the generation table
    /// (ADR-0012's "cartridge capacities are decimal" ruling) — not the
    /// binary unit `slice_size`/`enospc_buffer` use.
    mod import_capacity {
        use super::*;

        fn capacity_bytes_of(conn: &Connection, label: &str) -> i64 {
            conn.query_row(
                "SELECT capacity_bytes FROM volumes WHERE label = ?1",
                rusqlite::params![label],
                |r| r.get(0),
            )
            .unwrap()
        }

        #[test]
        fn capacity_2_5t_records_the_generation_tables_decimal_figure() {
            let conn = crate::db::open_memory().unwrap();
            let config = Config::default();
            volume_import(
                &conn,
                &config,
                "L6-CAP",
                "lto",
                "LTO-6",
                Some("2.5T"),
                None,
                None,
                false,
            )
            .unwrap();
            assert_eq!(
                capacity_bytes_of(&conn, "L6-CAP"),
                2_500_000_000_000,
                "\"2.5T\" must mean the marketed 2.5 TB (10^12), not the \
                 binary parser's 2,748,779,069,440"
            );
        }
    }

    /// Issue #169: `import --generation` is validated exactly as
    /// `cartridge register --generation` is (`media::parse_generation_or_error`),
    /// and `media_type` is stored as the CANONICAL spelling — ADR-0010's
    /// "What this does not change" promise that `volumes.media_type` "now
    /// always hold[s] a generation string tapectl can parse", which
    /// `import`'s old unvalidated pass-through broke. `--capacity`, when
    /// omitted, resolves from the generation table (ADR-0010 decision 3),
    /// not a fixed "2500G" default that lied for anything but LTO-6
    /// (ADR-0012: "the LTO-6 default that wrote unvalidated rows is gone").
    mod import_generation {
        use super::*;

        fn media_type_of(conn: &Connection, label: &str) -> String {
            conn.query_row(
                "SELECT media_type FROM volumes WHERE label = ?1",
                rusqlite::params![label],
                |r| r.get(0),
            )
            .unwrap()
        }

        fn capacity_bytes_of(conn: &Connection, label: &str) -> i64 {
            conn.query_row(
                "SELECT capacity_bytes FROM volumes WHERE label = ?1",
                rusqlite::params![label],
                |r| r.get(0),
            )
            .unwrap()
        }

        #[test]
        fn an_unparseable_generation_is_rejected_naming_the_accepted_set() {
            let conn = crate::db::open_memory().unwrap();
            let config = Config::default();
            let err = volume_import(
                &conn,
                &config,
                "BAD-GEN",
                "lto",
                "not-a-generation",
                Some("2500G"),
                None,
                None,
                false,
            )
            .unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("not-a-generation"), "{msg}");
            assert!(
                msg.contains("LTO-6") && msg.contains("LTO-7") && msg.contains("LTO-8"),
                "error should name the accepted generation set, the same text \
                 `cartridge register` uses: {msg}"
            );
        }

        /// A non-canonical spelling ("lto6") is accepted but stored
        /// canonically, so a later comparison against a detected generation
        /// (`volume init`) is a plain string match (ADR-0010).
        #[test]
        fn a_non_canonical_spelling_records_the_canonical_string() {
            let conn = crate::db::open_memory().unwrap();
            let config = Config::default();
            volume_import(
                &conn,
                &config,
                "LC-IMP",
                "lto",
                "lto6",
                Some("2500G"),
                None,
                None,
                false,
            )
            .unwrap();
            assert_eq!(media_type_of(&conn, "LC-IMP"), "LTO-6");
        }

        /// Checked against two different generations so a wrong table
        /// lookup (e.g. always returning LTO-6's figure) cannot pass.
        #[test]
        fn omitted_capacity_records_lto6_table_figure() {
            let conn = crate::db::open_memory().unwrap();
            let config = Config::default();
            volume_import(
                &conn, &config, "L6-DEF", "lto", "LTO-6", None, None, None, false,
            )
            .unwrap();
            assert_eq!(capacity_bytes_of(&conn, "L6-DEF"), 2_500_000_000_000);
        }

        #[test]
        fn omitted_capacity_records_lto7_table_figure() {
            let conn = crate::db::open_memory().unwrap();
            let config = Config::default();
            volume_import(
                &conn, &config, "L7-DEF", "lto", "LTO-7", None, None, None, false,
            )
            .unwrap();
            assert_eq!(capacity_bytes_of(&conn, "L7-DEF"), 6_000_000_000_000);
        }

        /// The ADR-0010 invariant, directly: every row `import` can write
        /// has a `volumes.media_type` that `Generation::parse` accepts —
        /// canonical, bare, and short spellings alike.
        #[test]
        fn every_row_import_writes_has_a_parseable_media_type() {
            let conn = crate::db::open_memory().unwrap();
            let config = Config::default();
            for (i, spelling) in ["LTO-5", "lto6", "L7", "LTO-7-M8", "LTO8"]
                .into_iter()
                .enumerate()
            {
                let label = format!("INV-{i}");
                volume_import(
                    &conn, &config, &label, "lto", spelling, None, None, None, false,
                )
                .unwrap();
                let stored = media_type_of(&conn, &label);
                assert!(
                    crate::media::Generation::parse(&stored).is_some(),
                    "media_type {stored:?} written for --generation {spelling:?} \
                     must be parseable back (ADR-0010)"
                );
            }
        }
    }

    mod volume_retire_consent {
        use super::*;

        /// tenant + unit + snapshot + stage_set + a volume `label` with a
        /// completed write of that stage_set to it. When `with_other_copy`,
        /// the same stage_set is also completed-written to a second volume,
        /// so the unit keeps one copy after `label` is retired (not at
        /// risk). Returns (conn, retiring_volume_id).
        /// `pub(super)` since ADR-0011: `cartridge_retire`'s tests reuse
        /// this exact shape, and a second copy of it would be a second
        /// coverage fixture that could drift from this one.
        pub(super) fn setup_volume_with_one_unit(
            label: &str,
            with_other_copy: bool,
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
            volume_retire(&conn, &Config::default(), "L6-DRYRUN", false, true, false)
                .expect("dry-run must succeed");

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

        /// `--yes` waives TIER 2, and this fixture is a Tier-2 case for a
        /// reason worth stating (issue #147): the volume being retired is
        /// `active`, i.e. never sealed, so under ADR-0012 it counts for
        /// nothing and retiring it REMOVES nothing. The unit reads
        /// zero-copy before the command runs and zero-copy after it — a
        /// state worth showing the operator, not one to refuse. The
        /// sibling `tier3_*` tests below cover the case where the volume
        /// IS a copy, and there `--yes` gets nowhere.
        #[test]
        fn assume_yes_proceeds_when_the_retirement_removes_nothing() {
            // Safe: assume_yes=true short-circuits confirm() before any
            // stdin interaction, regardless of the test process's TTY-ness.
            let (conn, vol_id) = setup_volume_with_one_unit("L6-FORCED", false);
            volume_retire(&conn, &Config::default(), "L6-FORCED", true, false, false)
                .expect("--yes must override the zero-copy consent gate");
            assert_eq!(volume_status(&conn, vol_id), "retired");
        }

        /// An `active` (never-sealed) volume with the unit also covered
        /// elsewhere: nothing at stake, nothing below policy, no gate.
        #[test]
        fn proceeds_without_any_consent_gate_when_no_unit_is_at_risk() {
            // Safe with assume_yes=false: with_other_copy=true means no
            // unit drops to zero copies, so `confirm()` (and therefore any
            // stdin interaction) is never reached at all.
            let (conn, vol_id) = setup_volume_with_one_unit("L6-SAFE", true);
            volume_retire(&conn, &Config::default(), "L6-SAFE", false, false, false)
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
            // pre-existing tests above still see a real second copy); flip
            // its CONDITION to 'quarantined' here, after setup, to isolate
            // exactly this test's point without changing that default
            // (issue #242: quarantine is a condition now, not a status
            // move).
            conn.execute(
                "UPDATE volumes SET observed_condition = 'quarantined' WHERE label = 'OTHER-VOL'",
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
                at_stake: vec![],
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

        // ── ADR-0008 Tier 3 / Tier 2, restored (ADR-0012, issue #147) ──
        //
        // Before this, `volume retire` gated ONLY at zero and at Tier 2, so
        // `--yes` retired the last copy of live data and a retirement that
        // left a unit one copy short of its policy was not gated at all.

        fn seal(conn: &Connection, label: &str) {
            conn.execute(
                "UPDATE volumes SET status = 'sealed' WHERE label = ?1",
                params![label],
            )
            .unwrap();
        }

        /// `setup_volume_with_one_unit` with the retiring volume SEALED —
        /// the only shape in which retiring it removes anything (ADR-0012)
        /// — plus `copies_elsewhere` further sealed volumes carrying the
        /// same v1 of `unitA`.
        fn setup_sealed(label: &str, copies_elsewhere: usize) -> (Connection, i64) {
            let (conn, vol_id) = setup_volume_with_one_unit(label, false);
            seal(&conn, label);
            let (ss_id, snap_id): (i64, i64) = conn
                .query_row("SELECT id, snapshot_id FROM stage_sets LIMIT 1", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .unwrap();
            for n in 0..copies_elsewhere {
                conn.execute(
                    &format!(
                        "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                              capacity_bytes, status)
                         VALUES ('COPY-{n}', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')"
                    ),
                    [],
                )
                .unwrap();
                let other = conn.last_insert_rowid();
                conn.execute(
                    "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                     VALUES (?1, ?2, ?3, 'completed')",
                    params![ss_id, snap_id, other],
                )
                .unwrap();
            }
            (conn, vol_id)
        }

        fn config_with_min_copies(n: i32) -> Config {
            let mut config = Config::default();
            config.defaults.min_copies_for_tape_only = n;
            config
        }

        // ── the Tier-3 recipe must be executable as written (issue #147) ──
        //
        // `stage create --version` is REFUSED while that version still has a
        // stage set with live slices, and `volume write` leaves every set it
        // writes `staged`. So a recipe that always says "stage it again"
        // hands the operator a command that fails, in the very common case
        // where `staging clean` has not run. These pin all three shapes.

        fn set_all_stage_sets(conn: &Connection, status: &str) {
            conn.execute("UPDATE stage_sets SET status = ?1", params![status])
                .unwrap();
        }

        #[test]
        fn recipe_offers_a_plain_write_when_the_slices_are_still_staged() {
            let (conn, _) = setup_sealed("L6-STAGED", 0);
            set_all_stage_sets(&conn, "staged");
            let err = volume_retire(&conn, &Config::default(), "L6-STAGED", false, false, false)
                .expect_err("Tier 3 must fire");
            let msg = err.to_string();
            assert!(
                !msg.contains("tapectl stage create"),
                "must not tell the operator to re-stage a version `stage create` will \
                 refuse: {msg}"
            );
            assert!(
                msg.contains("still") && msg.contains("slices in staging"),
                "must say the bytes are already staged: {msg}"
            );
            assert!(msg.contains("tapectl volume write <OTHER-LABEL>"), "{msg}");
        }

        #[test]
        fn recipe_offers_a_restage_when_staging_has_been_released() {
            let (conn, _) = setup_sealed("L6-CLEANED", 0);
            set_all_stage_sets(&conn, "cleaned");
            let err = volume_retire(&conn, &Config::default(), "L6-CLEANED", false, false, false)
                .expect_err("Tier 3 must fire");
            let msg = err.to_string();
            assert!(
                msg.contains("tapectl stage create"),
                "with staging released, re-staging IS the instruction: {msg}"
            );
            assert!(
                !msg.contains("slices in staging"),
                "must not claim bytes are staged when they are not: {msg}"
            );
        }

        /// The mixed case is the one a single hedged paragraph got wrong:
        /// some versions still staged, some not, and the operator has to
        /// know which is which without working it out mid-incident.
        #[test]
        fn recipe_names_both_halves_when_only_some_versions_are_staged() {
            let (conn, _) = setup_sealed("L6-MIXED", 0);
            set_all_stage_sets(&conn, "staged");
            // Add a SECOND unit whose version has no live stage set.
            conn.execute(
                "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                 VALUES ('u-two', 'second', 1, 'mtime_size', 1, 'active')",
                [],
            )
            .unwrap();
            let unit2 = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
                 VALUES (?1, 1, 'full', 'current', '/src2')",
                params![unit2],
            )
            .unwrap();
            let snap2 = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO stage_sets (snapshot_id, status, slice_size)
                 VALUES (?1, 'cleaned', 524288)",
                params![snap2],
            )
            .unwrap();
            let ss2 = conn.last_insert_rowid();
            let vol_id: i64 = conn
                .query_row("SELECT id FROM volumes WHERE label = 'L6-MIXED'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, 'completed')",
                params![ss2, snap2, vol_id],
            )
            .unwrap();

            let err = volume_retire(&conn, &Config::default(), "L6-MIXED", false, false, false)
                .expect_err("Tier 3 must fire");
            let msg = err.to_string();
            assert!(
                msg.contains("tapectl stage create second --version 1"),
                "the released version must get a re-stage line: {msg}"
            );
            assert!(
                !msg.contains("tapectl stage create photos"),
                "the STAGED version must not get one -- it would be refused: {msg}"
            );
            assert!(msg.contains("slices in staging"), "{msg}");
        }

        /// THE headline of issue #147: the last eligible copy of a current
        /// version is refused, full stop.
        #[test]
        fn tier3_refuses_the_last_eligible_copy_of_a_current_version() {
            let (conn, vol_id) = setup_sealed("L6-LAST", 0);
            let err = volume_retire(&conn, &Config::default(), "L6-LAST", false, false, false)
                .expect_err("the last eligible copy of a live version must be refused");
            assert!(
                err.to_string().contains("LAST eligible copy"),
                "the Tier-3 floor must be what fired, not a consent prompt: {err}"
            );
            assert_eq!(volume_status(&conn, vol_id), "sealed");
        }

        /// Tier 3 is ABSOLUTE: `--yes` is not a way past it. This is the
        /// exact inversion ADR-0012 names — the command used to prompt here
        /// and let the flag through.
        #[test]
        fn tier3_is_not_defeated_by_assume_yes() {
            let (conn, vol_id) = setup_sealed("L6-LAST-YES", 0);
            let err = volume_retire(&conn, &Config::default(), "L6-LAST-YES", true, false, false)
                .expect_err("no flag may defeat ADR-0008 Tier 3");
            assert!(err.to_string().contains("LAST eligible copy"), "got: {err}");
            assert!(
                err.to_string().contains("no --force for this"),
                "the refusal must say plainly that no flag reaches it: {err}"
            );
            assert_eq!(volume_status(&conn, vol_id), "sealed");
        }

        /// A zero `min_copies` must not buy past the floor either: Tier 3
        /// is not a threshold comparison, it is a fact about what the act
        /// removes.
        #[test]
        fn tier3_is_not_defeated_by_a_zero_min_copies_policy() {
            let (conn, _) = setup_sealed("L6-LAST-ZERO", 0);
            let err = volume_retire(
                &conn,
                &config_with_min_copies(0),
                "L6-LAST-ZERO",
                true,
                false,
                false,
            )
            .expect_err("Tier 3 is not a policy threshold");
            assert!(err.to_string().contains("LAST eligible copy"), "got: {err}");
        }

        /// ADR-0012's per-version rule (issue #153): v1 elsewhere, v2 only
        /// here. A per-UNIT copy count reads this unit as covered; the
        /// floor must still fire, and name v2.
        #[test]
        fn tier3_fires_per_version_when_only_the_newest_is_here() {
            let (conn, _) = setup_sealed("L6-V2", 1);
            let unit_id: i64 = conn
                .query_row("SELECT id FROM units WHERE name = 'unitA'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            let vol_id: i64 = conn
                .query_row("SELECT id FROM volumes WHERE label = 'L6-V2'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            conn.execute(
                "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
                 VALUES (?1, 2, 'full', 'current', '/src')",
                params![unit_id],
            )
            .unwrap();
            let snap2 = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO stage_sets (snapshot_id, status, slice_size)
                 VALUES (?1, 'staged', 524288)",
                params![snap2],
            )
            .unwrap();
            let ss2 = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, 'completed')",
                params![ss2, snap2, vol_id],
            )
            .unwrap();

            let err = volume_retire(&conn, &Config::default(), "L6-V2", true, false, false)
                .expect_err("v2 is only here; a newer version is not a copy of an older one");
            let msg = err.to_string();
            assert!(
                msg.contains("v2"),
                "the refusal must name the version: {msg}"
            );
            assert!(
                !msg.contains("unit \"unitA\" v1"),
                "v1 has a copy elsewhere and must NOT be named: {msg}"
            );
        }

        /// The other direction, and the one that matters for the read-error
        /// tape (ADR-0012): a volume that counts for nothing removes
        /// nothing. Refusing here would make `volume retire` useless
        /// exactly when an operator needs it most — `volume verify` failing
        /// is the documented escape, and it works by quarantining.
        fn tier3_does_not_fire_for_status(status: &str, label: &str) {
            let (conn, vol_id) = setup_sealed(label, 0);
            // Issue #242: 'quarantined' is a condition now, not a status --
            // translate it onto `observed_condition`, leaving `status` at
            // whatever `setup_sealed` gave it ('sealed').
            if status == "quarantined" {
                conn.execute(
                    "UPDATE volumes SET observed_condition = 'quarantined' WHERE id = ?1",
                    params![vol_id],
                )
                .unwrap();
            } else {
                conn.execute(
                    "UPDATE volumes SET status = ?1 WHERE id = ?2",
                    params![status, vol_id],
                )
                .unwrap();
            }
            // assume_yes: this is a Tier-2 case (the unit reads zero-copy),
            // and Tier 2 is exactly what a flag is allowed to waive.
            volume_retire(&conn, &Config::default(), label, true, false, false).unwrap_or_else(
                |e| panic!("a {status} volume removes nothing and must not hit the floor: {e}"),
            );
            assert_eq!(volume_status(&conn, vol_id), "retired");
        }

        #[test]
        fn tier3_does_not_fire_for_a_quarantined_volume() {
            tier3_does_not_fire_for_status("quarantined", "L6-QUAR-RETIRE");
        }

        #[test]
        fn tier3_does_not_fire_for_an_unsealed_volume() {
            tier3_does_not_fire_for_status("active", "L6-UNSEALED");
        }

        #[test]
        fn tier3_does_not_fire_for_an_already_retired_volume() {
            tier3_does_not_fire_for_status("retired", "L6-ALREADY");
        }

        /// A RELEASED version is one the operator gave up on purpose. The
        /// floor does not protect it — `snapshot mark-reclaimable` IS the
        /// documented escape, so it must actually work.
        #[test]
        fn tier3_does_not_fire_for_a_released_version() {
            let (conn, vol_id) = setup_sealed("L6-RELEASED", 0);
            conn.execute("UPDATE snapshots SET status = 'reclaimable'", [])
                .unwrap();
            volume_retire(&conn, &Config::default(), "L6-RELEASED", true, false, false)
                .expect("mark-reclaimable is the escape the refusal names; it must work");
            assert_eq!(volume_status(&conn, vol_id), "retired");
        }

        /// The refusal is what the operator has to act on, so its content is
        /// part of the contract: the escapes are COMMANDS, not flags.
        #[test]
        fn tier3_refusal_names_the_escapes_and_denies_a_flag() {
            let (conn, _) = setup_sealed("L6-TEXT", 0);
            // EXPLICIT, not incidental: `setup_sealed` leaves the stage set
            // `staged`, and a still-staged version deliberately gets a
            // `volume write` line instead of a `stage create` one -- the
            // latter would be refused. This test is about the refusal naming
            // every escape, so it pins the released-staging shape and
            // `recipe_offers_a_plain_write_when_the_slices_are_still_staged`
            // pins the other. Before that split this assertion passed by
            // pinning the very defect it now guards against.
            set_all_stage_sets(&conn, "cleaned");
            let msg = volume_retire(&conn, &Config::default(), "L6-TEXT", true, false, false)
                .expect_err("must refuse")
                .to_string();
            for needle in [
                "tapectl volume read-slices --from L6-TEXT --unit unitA",
                "tapectl volume init <OTHER-LABEL>",
                "tapectl volume write <OTHER-LABEL>",
                "tapectl stage create unitA --version 1",
                "tapectl snapshot mark-reclaimable unitA --version 1",
                "tapectl volume verify L6-TEXT",
                // Issue #234: the escape is CONDITIONAL, and the refusal has
                // to say so. It used to promise "quarantines the volume when
                // it fails" — the blanket wording ADR-0012's 2026-09-17
                // amendment corrected. Both halves are pinned, because an
                // operator told only the first half would follow this recipe
                // with a dirty drive and get no explanation for why nothing
                // changed.
                "quarantines the volume when the failure PROVES the medium is bad",
                "read or transport failure the volume is left untouched",
                "no --force for this",
            ] {
                assert!(
                    msg.contains(needle),
                    "refusal must contain {needle:?}: {msg}"
                );
            }
            assert!(
                !msg.contains("re-run with --yes"),
                "the Tier-3 refusal must never suggest a flag: {msg}"
            );
        }

        /// ADR-0008 TIER 2, the gate that did not exist: one copy left
        /// against a policy of two is below policy and above zero.
        #[test]
        fn tier2_gates_when_a_version_is_left_below_min_copies() {
            let (conn, vol_id) = setup_sealed("L6-THIN", 1);
            let err = volume_retire(&conn, &Config::default(), "L6-THIN", false, false, false)
                .expect_err("one copy against min_copies = 2 must gate");
            assert!(err.to_string().contains("refused"), "got: {err}");
            assert_eq!(
                volume_status(&conn, vol_id),
                "sealed",
                "a refused retirement must not retire the volume"
            );
        }

        /// ...and Tier 2 is what a flag IS allowed to waive.
        #[test]
        fn tier2_below_policy_is_waived_by_assume_yes() {
            let (conn, vol_id) = setup_sealed("L6-THIN-YES", 1);
            volume_retire(&conn, &Config::default(), "L6-THIN-YES", true, false, false)
                .expect("--yes waives Tier 2, which is the whole distinction");
            assert_eq!(volume_status(&conn, vol_id), "retired");
        }

        /// No gate at all when every version stays at or above policy —
        /// ordinary retirement must not become a ceremony (ADR-0008's own
        /// warning).
        #[test]
        fn no_gate_at_all_when_every_version_stays_at_or_above_policy() {
            let (conn, vol_id) = setup_sealed("L6-FAT", 2);
            volume_retire(&conn, &Config::default(), "L6-FAT", false, false, false)
                .expect("two copies left against min_copies = 2 is within policy");
            assert_eq!(volume_status(&conn, vol_id), "retired");
        }

        /// ADR-0004 Tier 1: evidence age is DISPLAYED and never gates. The
        /// testable form of "never gates" is that a within-policy
        /// retirement whose remaining coverage was last verified in 2011
        /// still needs no consent at all.
        #[test]
        fn tier1_evidence_age_is_displayed_and_never_gates() {
            let (conn, vol_id) = setup_sealed("L6-ANCIENT", 2);
            // Both survivors verified long ago, so the summary's WEAKEST
            // reading is the ancient one rather than a never-verified peer.
            conn.execute(
                "INSERT INTO verification_sessions (volume_id, completed_at, outcome)
                 SELECT id, '2011-08-01 00:00:00', 'passed' FROM volumes
                 WHERE label IN ('COPY-0', 'COPY-1')",
                [],
            )
            .unwrap();

            let impacts = retire_impacts(&conn, vol_id).unwrap();
            let summary = crate::policy::evidence::describe(
                &impacts[0].unit_name,
                &impacts[0].evidence,
                chrono::Utc::now().naive_utc(),
            )
            .expect("evidence must be described for a unit that retains coverage");
            assert!(
                summary.contains("days ago"),
                "the age must be rendered, not just the volume: {summary}"
            );

            volume_retire(&conn, &Config::default(), "L6-ANCIENT", false, false, false)
                .expect("ADR-0004: evidence age is shown, never a gate");
            assert_eq!(volume_status(&conn, vol_id), "retired");
        }

        /// `--dry-run` must say what the real run would do — a dry run that
        /// stayed silent about an absolute refusal would be worse than none.
        #[test]
        fn dry_run_names_the_versions_the_floor_would_refuse_on() {
            let (conn, vol_id) = setup_sealed("L6-DRY3", 0);
            volume_retire(&conn, &Config::default(), "L6-DRY3", false, true, false)
                .expect("dry-run reports, it does not gate");
            assert_eq!(volume_status(&conn, vol_id), "sealed");
            let impacts = retire_impacts(&conn, vol_id).unwrap();
            let json = retire_impacts_json(&impacts);
            assert_eq!(
                json[0]["last_copy_versions"][0], 1,
                "the dry-run JSON must name the version the floor protects: {json:?}"
            );
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

            // Issue #242: 'quarantined' is a condition now, not a status --
            // translate it onto `observed_condition`, leaving `status` at
            // 'sealed'.
            let (status_value, condition_value) = if second_volume_status == "quarantined" {
                ("sealed", "quarantined")
            } else {
                (second_volume_status, "ok")
            };
            conn.execute(
                &format!(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status, observed_condition)
                     VALUES ('{name}-OTHER', 'lto', 'lto0', 'LTO-6', 2500000000000, '{status_value}', '{condition_value}')"
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
            // Issue #242: quarantine is a condition now, not a status move.
            conn.execute(
                &format!(
                    "UPDATE volumes SET observed_condition = 'quarantined' WHERE label = '{name}-SEALED'"
                ),
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

            // Issue #242: 'quarantined' is a condition now, not a status --
            // translate it onto `observed_condition`, leaving `status` at
            // 'sealed'.
            let (status_value, condition_value) = if second_volume_status == "quarantined" {
                ("sealed", "quarantined")
            } else {
                (second_volume_status, "ok")
            };
            conn.execute(
                &format!(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status, observed_condition)
                     VALUES ('{name}-OTHER', 'lto', 'lto0', 'LTO-6', 2500000000000, '{status_value}', '{condition_value}')"
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

    /// Issue #153 / ADR-0012: which of a unit's current versions is
    /// thinnest, surfaced by `unit mark-tape-only` alongside the
    /// unit-wide minimum `copy_count` computes.
    mod thinnest_version_display {
        use super::*;

        /// tenant + unit + TWO `'current'` snapshots with DIFFERING copy
        /// counts: v1 on two sealed volumes (2 copies), v2 on one (1
        /// copy) -- the thinnest. Returns `(conn, unit_id)`.
        fn setup_unit_with_two_current_versions_of_differing_thinness(
            name: &str,
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

            // v1: two sealed volumes -- 2 copies.
            conn.execute(
                "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
                 VALUES (?1, 1, 'full', 'current', '/src')",
                params![unit_id],
            )
            .unwrap();
            let snap1 = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
                params![snap1],
            )
            .unwrap();
            let ss1 = conn.last_insert_rowid();
            for label in [format!("{name}-A"), format!("{name}-B")] {
                conn.execute(
                    &format!(
                        "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                              capacity_bytes, status)
                         VALUES ('{label}', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')"
                    ),
                    [],
                )
                .unwrap();
                let vid = conn.last_insert_rowid();
                conn.execute(
                    "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                     VALUES (?1, ?2, ?3, 'completed')",
                    params![ss1, snap1, vid],
                )
                .unwrap();
            }

            // v2: one sealed volume -- 1 copy, the thinnest.
            conn.execute(
                "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
                 VALUES (?1, 2, 'full', 'current', '/src')",
                params![unit_id],
            )
            .unwrap();
            let snap2 = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
                params![snap2],
            )
            .unwrap();
            let ss2 = conn.last_insert_rowid();
            conn.execute(
                &format!(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                          capacity_bytes, status)
                     VALUES ('{name}-C', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')"
                ),
                [],
            )
            .unwrap();
            let vid2 = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, 'completed')",
                params![ss2, snap2, vid2],
            )
            .unwrap();

            (conn, unit_id)
        }

        #[test]
        fn thinnest_current_version_picks_the_version_with_fewer_copies() {
            let (conn, unit_id) =
                setup_unit_with_two_current_versions_of_differing_thinness("mv-thin");
            let thinnest = thinnest_current_version(&conn, unit_id).unwrap().unwrap();
            assert_eq!(thinnest.version, 2, "v2 has only 1 copy, v1 has 2");
            assert_eq!(thinnest.copies, 1);
            assert_eq!(thinnest.total_current_versions, 2);
            assert_eq!(
                thinnest.describe(),
                "version 2 of 2 current versions has the fewest copies: 1",
                "must name the VERSION, not just the count -- the #91 lesson: \
                 this line is read with zero surrounding context"
            );
        }

        #[test]
        fn thinnest_current_version_is_none_without_a_current_snapshot() {
            let conn = crate::db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
                [],
            )
            .unwrap();
            let tid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                 VALUES ('uuid-mv-empty', 'mv-empty', ?1, 'mtime_size', 1, 'active')",
                params![tid],
            )
            .unwrap();
            let unit_id = conn.last_insert_rowid();
            assert!(thinnest_current_version(&conn, unit_id).unwrap().is_none());
        }

        /// The CLI command must not choke computing this display: same
        /// fixture, driven through `unit_mark_tape_only` end to end.
        #[test]
        fn mark_tape_only_succeeds_and_computes_a_thinnest_version_for_a_multi_version_unit() {
            let (conn, unit_id) =
                setup_unit_with_two_current_versions_of_differing_thinness("mv-thin-cli");
            let mut config = Config::default();
            // Isolate the display from the Tier-2 gate: this fixture's
            // unit-wide copy_count is MIN(2, 1) = 1 under the #153 fix,
            // which would otherwise refuse below the default threshold of
            // 2 -- zero out the thresholds so the command reaches the
            // evidence display unconditionally.
            config.defaults.min_copies_for_tape_only = 0;
            config.defaults.min_locations_for_tape_only = 0;
            unit_mark_tape_only(&conn, &config, "mv-thin-cli", false, false)
                .expect("command must succeed and compute the thinnest-version display");
            let thinnest = thinnest_current_version(&conn, unit_id).unwrap().unwrap();
            assert_eq!(thinnest.version, 2);
            assert_eq!(thinnest.copies, 1);
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

    /// ADR-0011 / issue #148: `cartridge retire` — the writer
    /// `retired_permanent` never had.
    ///
    /// Same no-real-stdin discipline as `volume_retire_consent` above:
    /// `cartridge_retire` asks for consent on EVERY call (retiring a medium
    /// is a declaration, not merely a risk), so every test here passes
    /// `force` or `assume_yes`, both of which short-circuit `confirm()`
    /// before it can touch stdin — or asserts the non-interactive refusal,
    /// which `cli::consent`'s own tests prove never reads stdin either.
    /// ADR-0011, "Retiring a volume frees its cartridge": until ADR-0010's
    /// binding, no volume knew which cartridge it was on, so only
    /// `compact-finish` ever wrote `pending_erase`. `volume retire` is named
    /// as a writer of it in the lifecycle diagram and finally is one.
    mod volume_retire_frees_its_cartridge {
        use super::*;

        /// `L6-CART` on cartridge `BC-FREE` (in_use, open mount), with
        /// another sealed copy of its unit elsewhere so retirement needs no
        /// consent gate.
        fn setup() -> (Connection, i64, i64) {
            let (conn, vol_id) =
                super::volume_retire_consent::setup_volume_with_one_unit("L6-CART", true);
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status)
                 VALUES ('BC-FREE', 'LTO-6', 2500000000000, 'in_use')",
                [],
            )
            .unwrap();
            let cart_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
                params![cart_id, vol_id],
            )
            .unwrap();
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

        /// Mount a second volume with `status` on the same cartridge.
        fn add_volume_on(conn: &Connection, cart_id: i64, label: &str, status: &str) -> i64 {
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                      capacity_bytes, status)
                 VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, ?2)",
                params![label, status],
            )
            .unwrap();
            let id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
                params![cart_id, id],
            )
            .unwrap();
            id
        }

        /// The headline: the last live volume goes, the cartridge follows.
        #[test]
        fn retiring_the_last_live_volume_moves_the_cartridge_to_pending_erase() {
            let (conn, cart_id, _) = setup();
            volume_retire(&conn, &Config::default(), "L6-CART", false, false, false).unwrap();
            assert_eq!(cartridge_status(&conn, cart_id), "pending_erase");

            // With an event, so the lifecycle is auditable rather than a
            // status that changed by itself.
            let events: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM events
                     WHERE entity_type = 'cartridge' AND entity_id = ?1
                       AND new_value = 'pending_erase'",
                    params![cart_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(events, 1, "the cartridge status change must be logged");
        }

        /// The mount stays OPEN: the bytes are still physically on the tape
        /// until someone erases it, and `cartridge mark-erased` is the step
        /// that says otherwise (ADR-0011).
        #[test]
        fn the_mount_is_left_open_for_mark_erased_to_close() {
            let (conn, cart_id, vol_id) = setup();
            volume_retire(&conn, &Config::default(), "L6-CART", false, false, false).unwrap();
            let open: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM cartridge_volumes
                     WHERE cartridge_id = ?1 AND volume_id = ?2 AND unmounted_at IS NULL",
                    params![cart_id, vol_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(open, 1);
        }

        /// Another live volume remains: the cartridge still holds data
        /// someone could restore from, and nothing about it has changed.
        #[test]
        fn another_live_volume_on_the_cartridge_leaves_it_in_use() {
            let (conn, cart_id, _) = setup();
            add_volume_on(&conn, cart_id, "L6-OTHER", "sealed");
            volume_retire(&conn, &Config::default(), "L6-CART", false, false, false).unwrap();
            assert_eq!(cartridge_status(&conn, cart_id), "in_use");
        }

        /// A volume that is already retired/erased is NOT live, so it does
        /// not hold the cartridge hostage — `coverage::in_service` is the
        /// predicate, not "is there any row".
        #[test]
        fn an_already_retired_neighbour_does_not_hold_the_cartridge() {
            let (conn, cart_id, _) = setup();
            add_volume_on(&conn, cart_id, "L6-DEAD", "retired");
            volume_retire(&conn, &Config::default(), "L6-CART", false, false, false).unwrap();
            assert_eq!(cartridge_status(&conn, cart_id), "pending_erase");
        }

        /// `cartridge unretire` is the ONLY exit from `retired_permanent`
        /// (ADR-0011, corrected 2026-09-14) — "the operator saying they
        /// were wrong about the medium". Retiring a volume is not that
        /// statement and must not quietly undo a condemnation.
        #[test]
        fn a_retired_permanent_cartridge_is_never_reopened() {
            let (conn, cart_id, _) = setup();
            conn.execute(
                "UPDATE cartridges SET status = 'retired_permanent' WHERE id = ?1",
                params![cart_id],
            )
            .unwrap();
            volume_retire(&conn, &Config::default(), "L6-CART", false, false, false).unwrap();
            assert_eq!(cartridge_status(&conn, cart_id), "retired_permanent");
        }

        /// A volume bound to nothing (pre-ADR-0010, or a drive with no
        /// readable medium serial) retires exactly as before.
        #[test]
        fn an_unbound_volume_retires_with_no_cartridge_to_free() {
            let (conn, vol_id) =
                super::volume_retire_consent::setup_volume_with_one_unit("L6-LOOSE", true);
            volume_retire(&conn, &Config::default(), "L6-LOOSE", false, false, false).unwrap();
            let status: String = conn
                .query_row(
                    "SELECT status FROM volumes WHERE id = ?1",
                    params![vol_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(status, "retired");
        }

        /// `--dry-run` reports and changes nothing — including the
        /// cartridge, which is the half a new writer could easily forget.
        #[test]
        fn dry_run_does_not_free_the_cartridge() {
            let (conn, cart_id, _) = setup();
            volume_retire(&conn, &Config::default(), "L6-CART", false, true, false).unwrap();
            assert_eq!(cartridge_status(&conn, cart_id), "in_use");
        }
    }

    mod cartridge_retire {
        use super::*;

        /// A `retirable` cartridge holding volume `L6-CART` (via an open
        /// `cartridge_volumes` mount) that carries `unitA`'s only completed
        /// write, plus — when `with_other_copy` — a second sealed volume on
        /// no cartridge carrying the same stage set. Returns
        /// (conn, cartridge_id, volume_id).
        /// `pub(super)` since issue #163: `cartridge_unretire`'s tests
        /// reuse this exact shape (retire it for real, then unretire it),
        /// rather than a second hand-seeded double that could drift.
        pub(super) fn setup(with_other_copy: bool) -> (Connection, i64, i64) {
            let (conn, vol_id) = super::volume_retire_consent::setup_volume_with_one_unit(
                "L6-CART",
                with_other_copy,
            );
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status)
                 VALUES ('BC-RET', 'LTO-6', 2500000000000, 'in_use')",
                [],
            )
            .unwrap();
            let cart_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
                params![cart_id, vol_id],
            )
            .unwrap();
            (conn, cart_id, vol_id)
        }

        fn status_of(conn: &Connection, table: &str, id: i64) -> String {
            conn.query_row(
                &format!("SELECT status FROM {table} WHERE id = ?1"),
                params![id],
                |r| r.get(0),
            )
            .unwrap()
        }

        /// The headline: `retired_permanent` finally has a writer, and the
        /// volume on the cartridge is retired with it — without which
        /// ADR-0011's own justification for the Tier-2 gate ("removes a
        /// physical copy from every coverage count that policy computes")
        /// would be false, since every coverage count is a
        /// `volumes.status` predicate.
        #[test]
        fn retire_writes_the_status_and_retires_the_volume_on_it() {
            let (conn, cart_id, vol_id) = setup(true);
            cartridge_retire(
                &conn,
                &Config::default(),
                "BC-RET",
                None,
                false,
                true,
                false,
                false,
            )
            .expect("--yes must satisfy the gate");

            assert_eq!(status_of(&conn, "cartridges", cart_id), "retired_permanent");
            assert_eq!(
                status_of(&conn, "volumes", vol_id),
                "retired",
                "a volume on a permanently retired medium must stop counting as coverage"
            );
        }

        /// The `cartridge_volumes` mount stays OPEN: the volume is still
        /// physically on the cartridge. `cartridge mark-erased` closes it,
        /// because that is when the bytes actually go.
        #[test]
        fn retire_leaves_the_mount_open_because_the_bytes_are_still_there() {
            let (conn, cart_id, _) = setup(true);
            cartridge_retire(
                &conn,
                &Config::default(),
                "BC-RET",
                None,
                false,
                true,
                false,
                false,
            )
            .unwrap();
            let open: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM cartridge_volumes
                     WHERE cartridge_id = ?1 AND unmounted_at IS NULL",
                    params![cart_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(open, 1);
        }

        /// ADR-0008's non-hanging requirement, the half that matters most:
        /// a non-interactive session with no `--yes`/`--force` REFUSES
        /// rather than blocking on a prompt nobody can answer.
        #[test]
        fn non_interactive_without_consent_refuses_and_changes_nothing() {
            let (conn, cart_id, vol_id) = setup(false);
            // assume_yes=false, force=false. `confirm()` reads the real
            // `stdin().is_terminal()`, which is false under `cargo test`'s
            // captured stdin -- and `cli::consent`'s own tests prove that
            // branch never attempts a read.
            let err = cartridge_retire(
                &conn,
                &Config::default(),
                "BC-RET",
                None,
                false,
                false,
                false,
                false,
            )
            .expect_err("no consent in a non-interactive session must refuse");
            assert!(err.to_string().contains("refused"), "got: {err}");
            assert_eq!(status_of(&conn, "cartridges", cart_id), "in_use");
            assert_eq!(
                status_of(&conn, "volumes", vol_id),
                "active",
                "a refused retirement must not retire the volume either"
            );
        }

        /// Tier 2, and the fixture is one for a reason worth stating
        /// (issue #147): the volume on this cartridge is `active`, never
        /// sealed, so under ADR-0012 it counts for nothing and retiring the
        /// cartridge REMOVES nothing. `--force` waives a Tier-2 prompt;
        /// `tier3_is_not_defeated_by_force` below proves it waives nothing
        /// when the cartridge actually carries the last copy.
        #[test]
        fn force_overrides_when_the_retirement_removes_nothing() {
            let (conn, cart_id, _) = setup(false);
            cartridge_retire(
                &conn,
                &Config::default(),
                "BC-RET",
                None,
                true,
                false,
                false,
                false,
            )
            .expect("--force must override a zero-copy impact");
            assert_eq!(status_of(&conn, "cartridges", cart_id), "retired_permanent");
        }

        // ── ADR-0008 Tier 3, restored (ADR-0012, issue #147) ──

        /// `setup` with the cartridge's volume SEALED, so retiring the
        /// cartridge really does remove the catalog's last eligible copy of
        /// `unitA`'s current v1. ADR-0011's own justification for gating
        /// this command is that it "removes a physical copy from every
        /// coverage count that policy computes" — which is exactly the
        /// thing ADR-0008 puts an absolute floor under.
        fn setup_sealed_cartridge() -> (Connection, i64, i64) {
            let (conn, cart_id, vol_id) = setup(false);
            conn.execute(
                "UPDATE volumes SET status = 'sealed' WHERE id = ?1",
                params![vol_id],
            )
            .unwrap();
            (conn, cart_id, vol_id)
        }

        #[test]
        fn tier3_refuses_when_the_cartridge_holds_the_last_eligible_copy() {
            let (conn, cart_id, vol_id) = setup_sealed_cartridge();
            let err = cartridge_retire(
                &conn,
                &Config::default(),
                "BC-RET",
                None,
                false,
                false,
                false,
                false,
            )
            .expect_err("the last eligible copy of a live version must be refused");
            assert!(err.to_string().contains("LAST eligible copy"), "got: {err}");
            assert_eq!(status_of(&conn, "cartridges", cart_id), "in_use");
            assert_eq!(status_of(&conn, "volumes", vol_id), "sealed");
        }

        /// Neither `--force` nor `--yes` reaches the floor. Both are passed
        /// here because `cartridge_retire` ORs them into one waiver, and a
        /// waiver is precisely what Tier 3 does not accept.
        #[test]
        fn tier3_is_not_defeated_by_force() {
            let (conn, cart_id, _) = setup_sealed_cartridge();
            let err = cartridge_retire(
                &conn,
                &Config::default(),
                "BC-RET",
                None,
                true,
                true,
                false,
                false,
            )
            .expect_err("no flag may defeat ADR-0008 Tier 3");
            assert!(
                err.to_string().contains("no --force for this"),
                "got: {err}"
            );
            assert_eq!(status_of(&conn, "cartridges", cart_id), "in_use");
        }

        /// The refusal must name the VOLUME to copy off, not the cartridge:
        /// `volume read-slices` takes a volume label.
        #[test]
        fn tier3_refusal_names_the_volume_not_the_cartridge() {
            let (conn, _, _) = setup_sealed_cartridge();
            let msg = cartridge_retire(
                &conn,
                &Config::default(),
                "BC-RET",
                None,
                true,
                true,
                false,
                false,
            )
            .expect_err("must refuse")
            .to_string();
            assert!(
                msg.contains("tapectl volume read-slices --from L6-CART --unit unitA"),
                "recovery must name the volume: {msg}"
            );
        }

        /// The other direction: a quarantined volume on the cartridge
        /// counts for nothing, so retiring the cartridge removes nothing
        /// and the floor must stay out of the way.
        #[test]
        fn tier3_does_not_fire_for_a_quarantined_volume_on_the_cartridge() {
            let (conn, cart_id, vol_id) = setup_sealed_cartridge();
            // Issue #242: quarantine is a condition now, not a status move.
            conn.execute(
                "UPDATE volumes SET observed_condition = 'quarantined' WHERE id = ?1",
                params![vol_id],
            )
            .unwrap();
            cartridge_retire(
                &conn,
                &Config::default(),
                "BC-RET",
                None,
                true,
                false,
                false,
                false,
            )
            .expect("a quarantined volume removes nothing; this must stay Tier 2");
            assert_eq!(status_of(&conn, "cartridges", cart_id), "retired_permanent");
        }

        /// `--dry-run` still reports before any gate, floor included.
        #[test]
        fn tier3_dry_run_still_reports_and_changes_nothing() {
            let (conn, cart_id, vol_id) = setup_sealed_cartridge();
            cartridge_retire(
                &conn,
                &Config::default(),
                "BC-RET",
                None,
                false,
                false,
                true,
                false,
            )
            .expect("dry-run reports, it does not gate");
            assert_eq!(status_of(&conn, "cartridges", cart_id), "in_use");
            assert_eq!(status_of(&conn, "volumes", vol_id), "sealed");
        }

        #[test]
        fn dry_run_mutates_nothing() {
            let (conn, cart_id, vol_id) = setup(false);
            cartridge_retire(
                &conn,
                &Config::default(),
                "BC-RET",
                None,
                false,
                false,
                true,
                false,
            )
            .expect("dry-run must succeed before any consent gate");
            assert_eq!(status_of(&conn, "cartridges", cart_id), "in_use");
            assert_eq!(status_of(&conn, "volumes", vol_id), "active");
            let events: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE action = 'retired'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(events, 0);
        }

        /// The reason APPENDS. A retirement reason silently replacing an
        /// earlier maintenance note is how the one fact that explains a
        /// dead cartridge gets lost.
        #[test]
        fn reason_appends_to_notes_and_never_overwrites() {
            let (conn, _, _) = setup(true);
            conn.execute(
                "UPDATE cartridges SET notes = 'bought 2019' WHERE barcode = 'BC-RET'",
                [],
            )
            .unwrap();
            cartridge_retire(
                &conn,
                &Config::default(),
                "BC-RET",
                Some("read errors on 3 consecutive verifies"),
                false,
                true,
                false,
                false,
            )
            .unwrap();
            let notes: String = conn
                .query_row(
                    "SELECT notes FROM cartridges WHERE barcode = 'BC-RET'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(notes.starts_with("bought 2019"), "got: {notes}");
            assert!(notes.contains("read errors on 3 consecutive verifies"));
        }

        /// A cartridge with no notes at all gets the reason as the whole
        /// note, not a stray leading newline.
        #[test]
        fn reason_on_an_empty_note_does_not_lead_with_a_newline() {
            let (conn, _, _) = setup(true);
            cartridge_retire(
                &conn,
                &Config::default(),
                "BC-RET",
                Some("worn"),
                false,
                true,
                false,
                false,
            )
            .unwrap();
            let notes: String = conn
                .query_row(
                    "SELECT notes FROM cartridges WHERE barcode = 'BC-RET'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(!notes.starts_with('\n'), "got: {notes:?}");
            assert!(notes.contains("worn"));
        }

        /// Re-retiring is a no-op, not a second event and not a second
        /// notes line.
        #[test]
        fn retiring_an_already_retired_cartridge_changes_nothing() {
            let (conn, _, _) = setup(true);
            cartridge_retire(
                &conn,
                &Config::default(),
                "BC-RET",
                Some("worn"),
                false,
                true,
                false,
                false,
            )
            .unwrap();
            let before: i64 = conn
                .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
                .unwrap();
            cartridge_retire(
                &conn,
                &Config::default(),
                "BC-RET",
                Some("worn again"),
                false,
                true,
                false,
                false,
            )
            .expect("a second retire is a no-op, not an error");
            let after: i64 = conn
                .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
                .unwrap();
            assert_eq!(before, after);
            let notes: String = conn
                .query_row(
                    "SELECT notes FROM cartridges WHERE barcode = 'BC-RET'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(!notes.contains("worn again"));
        }

        #[test]
        fn an_unknown_barcode_says_so() {
            let (conn, _, _) = setup(true);
            let err = cartridge_retire(
                &conn,
                &Config::default(),
                "NOPE",
                None,
                true,
                true,
                false,
                false,
            )
            .unwrap_err();
            assert!(err.to_string().contains("NOPE"));
        }
    }

    /// Issue #163 / ADR-0012's consequences bullet: `cartridge unretire`
    /// reverses `cartridge retire`, recovering the prior statuses from the
    /// `events` audit trail that `cartridge_retire` already writes, rather
    /// than a new column.
    mod cartridge_unretire {
        use super::*;

        /// A retired cartridge `BC-RET` holding volume `L6-CART`, produced
        /// by actually calling `cartridge_retire` first -- so the `events`
        /// rows this command reads are the real ones that function writes,
        /// not hand-seeded doubles. Before retirement: cartridge `in_use`,
        /// volume `active` (see `cartridge_retire::setup`).
        fn setup_retired() -> (Connection, i64, i64) {
            let (conn, cart_id, vol_id) = super::cartridge_retire::setup(true);
            cartridge_retire(
                &conn,
                &Config::default(),
                "BC-RET",
                None,
                false,
                true,
                false,
                false,
            )
            .expect("--yes must satisfy the gate");
            (conn, cart_id, vol_id)
        }

        fn status_of(conn: &Connection, table: &str, id: i64) -> String {
            conn.query_row(
                &format!("SELECT status FROM {table} WHERE id = ?1"),
                params![id],
                |r| r.get(0),
            )
            .unwrap()
        }

        /// The headline: both the cartridge and the volume retired with it
        /// come back to exactly what they were before `cartridge retire`
        /// touched them.
        #[test]
        fn unretire_restores_the_cartridge_and_volume_prior_statuses() {
            let (conn, cart_id, vol_id) = setup_retired();
            assert_eq!(status_of(&conn, "cartridges", cart_id), "retired_permanent");
            assert_eq!(status_of(&conn, "volumes", vol_id), "retired");

            cartridge_unretire(&conn, "BC-RET", false, false).unwrap();

            assert_eq!(
                status_of(&conn, "cartridges", cart_id),
                "in_use",
                "the cartridge was in_use before cartridge_retire touched it"
            );
            assert_eq!(
                status_of(&conn, "volumes", vol_id),
                "active",
                "the volume was active before cartridge_retire touched it"
            );
        }

        /// The reversal is itself auditable: `events` gains rows that read
        /// forwards (`unretired`, old = retired-family value, new =
        /// recovered value), not just a status that changed by itself.
        #[test]
        fn unretire_logs_reversal_events_for_the_cartridge_and_the_volume() {
            let (conn, cart_id, vol_id) = setup_retired();
            cartridge_unretire(&conn, "BC-RET", false, false).unwrap();

            let (action, old_value, new_value): (String, Option<String>, Option<String>) = conn
                .query_row(
                    "SELECT action, old_value, new_value FROM events
                     WHERE entity_type = 'cartridge' AND entity_id = ?1
                     ORDER BY id DESC LIMIT 1",
                    params![cart_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert_eq!(action, "unretired");
            assert_eq!(old_value.as_deref(), Some("retired_permanent"));
            assert_eq!(new_value.as_deref(), Some("in_use"));

            let (v_action, v_old, v_new): (String, Option<String>, Option<String>) = conn
                .query_row(
                    "SELECT action, old_value, new_value FROM events
                     WHERE entity_type = 'volume' AND entity_id = ?1
                     ORDER BY id DESC LIMIT 1",
                    params![vol_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert_eq!(v_action, "unretired");
            assert_eq!(v_old.as_deref(), Some("retired"));
            assert_eq!(v_new.as_deref(), Some("active"));
        }

        /// Tier 1 under ADR-0008: refuses outright, naming the cartridge's
        /// ACTUAL status, rather than silently doing nothing or guessing.
        #[test]
        fn unretire_on_a_non_retired_cartridge_refuses_naming_the_status() {
            let (conn, _cart_id, _vol_id) = super::cartridge_retire::setup(true);
            let err = cartridge_unretire(&conn, "BC-RET", false, false).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("in_use"), "must name the actual status: {msg}");
            assert!(msg.contains("BC-RET"));
        }

        /// The honest-partial-restore case (issue #163 item 3): a catalog
        /// rebuilt from tape since the retirement has no `events` history
        /// at all. The cartridge falls back to its ordinary pre-retirement
        /// state (`available`) rather than a guessed one, and the volume
        /// -- with no recoverable event either -- is left exactly as it
        /// is, not resurrected on a guess.
        #[test]
        fn unretire_with_events_removed_restores_available_and_leaves_the_volume() {
            let (conn, cart_id, vol_id) = setup_retired();
            conn.execute("DELETE FROM events", []).unwrap();

            cartridge_unretire(&conn, "BC-RET", false, false).expect(
                "a missing history must not be an error -- it is an honest partial restore",
            );

            assert_eq!(
                status_of(&conn, "cartridges", cart_id),
                "available",
                "no recoverable event -- falls back to the ordinary pre-retirement state"
            );
            assert_eq!(
                status_of(&conn, "volumes", vol_id),
                "retired",
                "left alone: no recoverable event means no guessed status"
            );
        }

        #[test]
        fn dry_run_mutates_nothing() {
            let (conn, cart_id, vol_id) = setup_retired();
            cartridge_unretire(&conn, "BC-RET", true, false).expect("dry-run must succeed");
            assert_eq!(status_of(&conn, "cartridges", cart_id), "retired_permanent");
            assert_eq!(status_of(&conn, "volumes", vol_id), "retired");
            let unretired_events: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE action = 'unretired'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(unretired_events, 0, "dry-run must not write an audit event");
        }

        #[test]
        fn an_unknown_barcode_says_so() {
            let conn = crate::db::open_memory().unwrap();
            let err = cartridge_unretire(&conn, "NOPE", false, false).unwrap_err();
            assert!(err.to_string().contains("NOPE"));
        }

        /// A volume that was independently `retired` (e.g. via `volume
        /// retire`) BEFORE the cartridge was retired has its own
        /// `retired -> retired` event from `cartridge_retire` (which does
        /// not skip already-retired volumes). Unretiring the cartridge
        /// must restore it to `retired`, not resurrect it into whatever
        /// came before that independent retirement.
        #[test]
        fn a_volume_already_retired_before_the_cartridge_stays_retired() {
            let (conn, cart_id, vol_id) = super::cartridge_retire::setup(true);
            conn.execute(
                "UPDATE volumes SET status = 'retired' WHERE id = ?1",
                params![vol_id],
            )
            .unwrap();
            cartridge_retire(
                &conn,
                &Config::default(),
                "BC-RET",
                None,
                false,
                true,
                false,
                false,
            )
            .unwrap();
            assert_eq!(status_of(&conn, "volumes", vol_id), "retired");

            cartridge_unretire(&conn, "BC-RET", false, false).unwrap();

            assert_eq!(
                status_of(&conn, "cartridges", cart_id),
                "in_use",
                "the cartridge's own history is unaffected by the volume's"
            );
            assert_eq!(
                status_of(&conn, "volumes", vol_id),
                "retired",
                "the volume's most recent 'retired' event says retired -> retired"
            );
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

        /// Issue #163's merged audit finding: the Tier-2 consent facts must
        /// NAME the volume(s) about to be recorded erased, not just the
        /// cartridge's own status. Tests the exact function the gate
        /// calls, so production and test share one code path -- no stdout
        /// capture needed to prove the wording (the `retire_refusal_json`
        /// pattern above).
        #[test]
        fn mark_erased_consent_facts_name_each_volume_by_label() {
            let facts = mark_erased_consent_facts(
                "BC001",
                "retired_permanent",
                &["L6-0001".to_string(), "L6-0002".to_string()],
            );
            assert_eq!(facts.len(), 3, "the status line plus one per volume");
            assert!(facts[0].contains("retired_permanent"));
            assert!(
                facts
                    .iter()
                    .any(|f| f.contains("L6-0001") && f.contains("erased")),
                "got: {facts:?}"
            );
            assert!(
                facts
                    .iter()
                    .any(|f| f.contains("L6-0002") && f.contains("erased")),
                "got: {facts:?}"
            );
            // #91: each fact must read true ALONE -- no bare label with no
            // context about what is about to happen to it.
            for f in &facts[1..] {
                assert!(
                    f.contains("cartridge \"BC001\""),
                    "must not be a bare label: {f}"
                );
            }
        }

        /// No mounted volume -- e.g. a cartridge whose volume was already
        /// moved on -- is just the status line, not a phantom entry.
        #[test]
        fn mark_erased_consent_facts_with_no_volumes_is_just_the_status_line() {
            let facts = mark_erased_consent_facts("BC001", "in_use", &[]);
            assert_eq!(facts.len(), 1);
        }

        /// End-to-end through the real gate, not just the pure builder
        /// above: a non-interactive refusal (no `--force`/`--yes`) still
        /// names the cartridge in its message. `cli::consent::confirm`
        /// echoes `action` (which names the cartridge), not `facts` (which
        /// names the volumes), into the refusal text it returns -- the
        /// volume-naming fix lives in what an interactive operator is
        /// shown before answering, proven by
        /// `mark_erased_consent_facts_name_each_volume_by_label` above.
        #[test]
        fn refusal_message_names_the_cartridge_being_marked_erased() {
            let (conn, _cart_id, _vol_id) = setup_cartridge("in_use", true);
            let err = cartridge_mark_erased(&conn, "BC001", false, false, false, false)
                .expect_err("non-interactive with no consent must refuse");
            assert!(err.to_string().contains("BC001"));
        }

        /// Issue #207: `cartridge_mark_erased` read the cartridge's status
        /// only to decide whether consent was needed, then wrote
        /// `status = 'available'` UNCONDITIONALLY -- including over
        /// `retired_permanent`. ADR-0011's dated correction (2026-09-14) is
        /// explicit that `cartridge unretire` REPLACED mark-erased as the
        /// way back from that state, and the lifecycle diagram draws no
        /// edge from `retired_permanent` through this command at all: one
        /// `cartridge mark-erased BC --yes` silently un-condemned a medium
        /// the operator had declared permanently unfit. This must FAIL
        /// against the unmodified code and pass once the guard mirrors
        /// `binding::refuse_retired`, which takes no `force` parameter for
        /// exactly this reason.
        #[test]
        fn retired_permanent_is_refused_and_never_becomes_available() {
            let (conn, cart_id, vol_id) = setup_cartridge("retired_permanent", true);
            let err = cartridge_mark_erased(&conn, "BC001", false, false, false, false)
                .expect_err("a retired_permanent cartridge must refuse mark-erased outright");
            assert_eq!(
                cartridge_status(&conn, cart_id),
                "retired_permanent",
                "must not silently un-condemn a medium declared permanently unfit"
            );
            assert_eq!(
                volume_status(&conn, vol_id.unwrap()),
                "full",
                "a refused mark-erased must not touch the mounted volume either"
            );
            assert!(
                err.to_string().contains("cartridge unretire"),
                "the refusal must name `cartridge unretire` (ADR-0011's correction), \
                 not mark-erased, as the way back: {err}"
            );
        }

        /// Not defeatable by `--force`/`--yes` -- like
        /// `binding::refuse_retired`, this guard takes no force parameter
        /// at all: no amount of consent makes a permanently unfit medium
        /// fit again.
        #[test]
        fn retired_permanent_is_refused_even_with_force_and_global_yes() {
            let (conn, cart_id, _vol_id) = setup_cartridge("retired_permanent", false);
            let err = cartridge_mark_erased(&conn, "BC001", true, true, false, false)
                .expect_err("--force/--yes must not defeat the retired_permanent refusal");
            assert_eq!(cartridge_status(&conn, cart_id), "retired_permanent");
            assert!(err.to_string().contains("unretire"));
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

        let (snap_id, slice_files) = add_unit_snapshot(&conn, tid, "unit1", dir);
        (conn, snap_id, slice_files)
    }

    /// Registers one unit + snapshot + staged stage_set (two slices) +
    /// manifest under an existing connection/tenant. Factored out of
    /// `setup_deletable_snapshot` (issue #176) so a multi-unit fixture —
    /// two units sharing one connection and one write session — can be
    /// built without two separate in-memory databases.
    fn add_unit_snapshot(
        conn: &Connection,
        tenant_id: i64,
        unit_name: &str,
        dir: &Path,
    ) -> (i64, Vec<std::path::PathBuf>) {
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES (?1, ?2, ?3, 'mtime_size', 1, 'active')",
            params![format!("u-{unit_name}"), unit_name, tenant_id],
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
            let f = dir.join(format!("{unit_name}-slice{n}.dar.age"));
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
        insert_file(conn, snap_id, &format!("/src/{unit_name}.txt"), 3, "cc");

        (snap_id, slice_files)
    }

    /// Registers a `volumes` row and returns its id — the minimal shape
    /// `insert_write` needs to attach a `writes` row (issue #176).
    fn insert_volume(conn: &Connection, label: &str) -> i64 {
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
             VALUES (?1, 'lto', 'drive0', 1000000, 'active')",
            params![label],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// Adds one `writes` row (at `status`, optionally with a `session_dir`)
    /// plus one `write_positions` row for `snap_id`'s first stage slice —
    /// the exact shape issue #176's pre-fix cascade could not delete:
    /// `write_positions` references `stage_slices` and `writes`, and
    /// `writes` references `stage_sets`/`snapshots`/`volumes`, none of
    /// which the old six-statement cascade touched. Returns
    /// `(write_id, write_position_id)`.
    fn insert_write(
        conn: &Connection,
        snap_id: i64,
        volume_id: i64,
        status: &str,
        session_dir: Option<&str>,
    ) -> (i64, i64) {
        let stage_set_id: i64 = conn
            .query_row(
                "SELECT id FROM stage_sets WHERE snapshot_id = ?1",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        let slice_id: i64 = conn
            .query_row(
                "SELECT id FROM stage_slices WHERE stage_set_id = ?1
                 ORDER BY slice_number LIMIT 1",
                params![stage_set_id],
                |row| row.get(0),
            )
            .unwrap();

        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, session_dir)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![stage_set_id, snap_id, volume_id, status, session_dir],
        )
        .unwrap();
        let write_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO write_positions (write_id, stage_slice_id, position)
             VALUES (?1, ?2, '1')",
            params![write_id, slice_id],
        )
        .unwrap();
        let wp_id = conn.last_insert_rowid();

        (write_id, wp_id)
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
    /// the final `DELETE FROM snapshots`, i.e. a failure at the LAST
    /// statement — the case that, untransacted, left every dependent row
    /// deleted while the snapshot itself survived, referencing nothing.
    /// Extended by issue #176 to also carry a `writes`/`write_positions`
    /// row through the rollback, now that the cascade's new head deletes
    /// them too.
    #[test]
    fn a_failure_late_in_the_cascade_rolls_back_the_whole_delete() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (conn, snap_id, slice_files) = setup_deletable_snapshot(tmp.path());
        let volume_id = insert_volume(&conn, "VOL-ROLLBACK");
        insert_write(&conn, snap_id, volume_id, "planned", None);

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

        // Every dependent row must survive: without a transaction the
        // earlier DELETEs would have committed individually.
        for (table, sql) in [
            ("stage_slices", "SELECT COUNT(*) FROM stage_slices"),
            ("stage_sets", "SELECT COUNT(*) FROM stage_sets"),
            ("manifests", "SELECT COUNT(*) FROM manifests"),
            ("files", "SELECT COUNT(*) FROM files"),
            ("writes", "SELECT COUNT(*) FROM writes"),
            ("write_positions", "SELECT COUNT(*) FROM write_positions"),
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

    /// Issue #176: `snapshot_delete`'s only write-related guard checked
    /// `status = 'completed'`; every other reachable `writes` status —
    /// `planned` (right after `volume write` plans, before a byte is
    /// written), `in_progress`, `failed`, `aborted` (a failed confirm) and
    /// `interrupted` (a crash mid-write, per `recover_orphaned_sessions`)
    /// — passed the guard straight into a cascade that never touched
    /// `writes`/`write_positions`, tripping their FKs on the very first
    /// `DELETE FROM stage_slices`.
    ///
    /// Negative control (pre-fix HEAD): `snapshot_delete` returns
    /// `Err("FOREIGN KEY constraint failed")` for every one of these five
    /// statuses.
    #[test]
    fn delete_succeeds_with_a_non_completed_write_row() {
        for status in ["planned", "in_progress", "failed", "aborted", "interrupted"] {
            let tmp = tempfile::TempDir::new().unwrap();
            let (conn, snap_id, _slices) = setup_deletable_snapshot(tmp.path());
            let volume_id = insert_volume(&conn, &format!("VOL-{status}"));
            insert_write(&conn, snap_id, volume_id, status, None);

            snapshot_delete(&conn, "unit1", 1, true, false)
                .unwrap_or_else(|e| panic!("status '{status}' must not block delete: {e}"));

            let writes: i64 = conn
                .query_row("SELECT COUNT(*) FROM writes", [], |row| row.get(0))
                .unwrap();
            assert_eq!(writes, 0, "writes rows must be gone for status '{status}'");
            let wps: i64 = conn
                .query_row("SELECT COUNT(*) FROM write_positions", [], |row| row.get(0))
                .unwrap();
            assert_eq!(
                wps, 0,
                "write_positions rows must be gone for status '{status}'"
            );
            let fk_violations: i64 = conn
                .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(
                fk_violations, 0,
                "no dangling FK rows may remain for status '{status}'"
            );
        }
    }

    /// Issue #176: a failed confirm (`SealedPending::confirm` erroring)
    /// leaves `writes.status = 'aborted'` AND writes
    /// `verification_results` rows keyed to that write's
    /// `write_positions` — a third FK layer the audit's two-DELETE fix
    /// missed. `verification_sessions` is evidence about the *volume*
    /// (a later `volume verify` reuses it across many snapshots' writes),
    /// so it must survive; only its per-mismatch `verification_results`
    /// children belonging to THIS snapshot are deleted.
    #[test]
    fn delete_succeeds_after_a_failed_confirm() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (conn, snap_id, _slices) = setup_deletable_snapshot(tmp.path());
        let volume_id = insert_volume(&conn, "VOL-FAIL");
        let (_write_id, wp_id) = insert_write(&conn, snap_id, volume_id, "aborted", None);
        let stage_slice_id: i64 = conn
            .query_row(
                "SELECT stage_slice_id FROM write_positions WHERE id = ?1",
                params![wp_id],
                |row| row.get(0),
            )
            .unwrap();

        conn.execute(
            "INSERT INTO verification_sessions (volume_id, verify_type, outcome)
             VALUES (?1, 'full', 'failed')",
            params![volume_id],
        )
        .unwrap();
        let session_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO verification_results
             (session_id, write_position_id, stage_slice_id, result)
             VALUES (?1, ?2, ?3, 'failed_checksum')",
            params![session_id, wp_id, stage_slice_id],
        )
        .unwrap();

        snapshot_delete(&conn, "unit1", 1, true, false).unwrap();

        let vr: i64 = conn
            .query_row("SELECT COUNT(*) FROM verification_results", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            vr, 0,
            "verification_results rows belonging to the deleted snapshot must be gone"
        );

        let vs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM verification_sessions WHERE id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            vs, 1,
            "verification_sessions is evidence about the VOLUME, not this \
             snapshot -- it must survive"
        );

        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(fk_violations, 0, "no dangling FK rows may remain");
    }

    /// Issue #176 step 3: a `writes.session_dir` still referenced by
    /// another snapshot's `writes` row — `collection run`'s normal
    /// multi-unit-per-session shape — must survive this delete, exactly as
    /// #55 already guarantees for `stage_slices.staging_path`. Only once
    /// the LAST referencing row is gone does the directory get removed.
    #[test]
    fn delete_leaves_a_sibling_snapshots_session_intact() {
        let tmp = tempfile::TempDir::new().unwrap();
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();

        let (snap1_id, _slices1) = add_unit_snapshot(&conn, tid, "unit1", tmp.path());
        let (snap2_id, slices2) = add_unit_snapshot(&conn, tid, "unit2", tmp.path());

        let session_dir = tmp.path().join("sessions").join("shared-session");
        fs::create_dir_all(&session_dir).unwrap();
        let session_dir_str = session_dir.to_string_lossy().to_string();

        let volume_id = insert_volume(&conn, "VOL-SHARED");
        insert_write(
            &conn,
            snap1_id,
            volume_id,
            "interrupted",
            Some(&session_dir_str),
        );
        insert_write(
            &conn,
            snap2_id,
            volume_id,
            "interrupted",
            Some(&session_dir_str),
        );

        // Delete unit1's snapshot: unit2's sibling row and the shared
        // directory must both survive.
        snapshot_delete(&conn, "unit1", 1, true, false).unwrap();

        assert!(
            session_dir.exists(),
            "session dir still referenced by unit2's writes row must survive"
        );
        let snap2_writes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM writes WHERE snapshot_id = ?1",
                params![snap2_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            snap2_writes, 1,
            "sibling snapshot's writes row must survive"
        );
        let snap2_write_positions: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM write_positions wp
                 JOIN writes w ON w.id = wp.write_id
                 WHERE w.snapshot_id = ?1",
                params![snap2_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            snap2_write_positions, 1,
            "sibling snapshot's write_positions row must survive"
        );
        for f in &slices2 {
            assert!(f.exists(), "sibling's staged slice files must survive");
        }

        // Delete unit2's snapshot too: nothing references the directory
        // any more, so it goes.
        snapshot_delete(&conn, "unit2", 1, true, false).unwrap();
        assert!(
            !session_dir.exists(),
            "session dir must be removed once no writes row references it"
        );
    }

    /// Issue #176 step 4: an `interrupted` write's session can never be
    /// resumed once this delete removes the slices its Layout referenced,
    /// so the operator needs a pointer to `tapectl volume abort <label>`.
    /// #94 already settled that this must be a warning, never an
    /// auto-abort of the row.
    #[test]
    fn delete_of_an_interrupted_session_names_the_volume() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (conn, snap_id, _slices) = setup_deletable_snapshot(tmp.path());
        let volume_id = insert_volume(&conn, "VOL-NAMED-INTERRUPTED");
        insert_write(&conn, snap_id, volume_id, "interrupted", None);

        // Asserted against the rule itself, NOT against captured tracing
        // output: `tracing`'s per-callsite `Interest` is process-global, so
        // under a parallel `cargo test` whichever thread reaches the `warn!`
        // first can decide the callsite is uninteresting for every later
        // one and the capture comes back empty. That is a property of the
        // test harness, not of this code, and it failed a gate on master.
        let named = interrupted_write_volumes(&conn, snap_id).unwrap();
        assert_eq!(
            named,
            vec!["VOL-NAMED-INTERRUPTED".to_string()],
            "the interrupted write's volume must be the one the warning names"
        );

        snapshot_delete(&conn, "unit1", 1, true, false).unwrap();

        // And it named it because the row was really there and is really
        // gone — the warning is about work this delete actually did.
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM writes WHERE snapshot_id = ?1",
                params![snap_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0, "the interrupted write row must be gone");
    }

    /// Issue #176 step 2: the `completed` guard is the ONLY write-related
    /// refusal and must stay that way — this pins that a `completed` row
    /// still refuses exactly as before the fix.
    #[test]
    fn delete_still_refuses_a_completed_write() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (conn, snap_id, _slices) = setup_deletable_snapshot(tmp.path());
        let volume_id = insert_volume(&conn, "VOL-DONE");
        insert_write(&conn, snap_id, volume_id, "completed", None);

        let err = snapshot_delete(&conn, "unit1", 1, true, false).unwrap_err();
        assert!(
            err.to_string().contains("completed write"),
            "expected the existing completed-write refusal, got: {err}"
        );
        assert!(
            err.to_string().contains("cannot delete"),
            "expected the existing completed-write refusal, got: {err}"
        );
    }

    /// Issue #177: the pre-fix `db_fsck` hand-rolled exactly two orphan
    /// scans (`writes.volume_id`, `stage_slices.stage_set_id`) out of 35
    /// FK edges the schema declares, so a database with orphans anywhere
    /// else reported clean. This fixture orphans four OTHER edges the old
    /// scans never looked at.
    ///
    /// Negative control (pre-fix HEAD): `db_fsck(&conn, false).unwrap()`
    /// returns `issues.is_empty() == true` for this exact fixture.
    #[test]
    fn fsck_reports_orphans_on_edges_the_old_scans_ignored() {
        let conn = crate::db::open_memory().unwrap();

        conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();

        // A valid cartridge and location so only the ONE targeted column
        // on each orphan row dangles -- never two edges confused as one.
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity)
             VALUES ('BC-ORPHAN', 'LTO-6', 2500000000000)",
            [],
        )
        .unwrap();
        let cart_id = conn.last_insert_rowid();

        conn.execute("INSERT INTO locations (name) VALUES ('loc-orphan')", [])
            .unwrap();
        let loc_id = conn.last_insert_rowid();

        // snapshots.unit_id -> units: dangling.
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, source_path)
             VALUES (99999, 1, '/nonexistent')",
            [],
        )
        .unwrap();

        // manifest_entries.manifest_id -> manifests: dangling.
        conn.execute(
            "INSERT INTO manifest_entries (manifest_id, path, size_bytes, mtime)
             VALUES (99999, '/nonexistent', 1, '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();

        // cartridge_volumes.volume_id -> volumes: dangling.
        conn.execute(
            "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, 99999)",
            [cart_id],
        )
        .unwrap();

        // volume_deposits.volume_id -> volumes: dangling.
        conn.execute(
            "INSERT INTO volume_deposits (volume_id, location_id) VALUES (99999, ?1)",
            [loc_id],
        )
        .unwrap();

        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();

        let report = db_fsck(&conn, false).unwrap();
        assert!(report.integrity_ok);
        assert_eq!(report.repaired, 0, "a dry run must delete nothing");

        let has_edge = |child: &str, parent: &str| {
            report
                .issues
                .iter()
                .any(|i| i.contains(child) && i.contains(parent))
        };
        assert!(
            has_edge("snapshots", "units"),
            "issues: {:?}",
            report.issues
        );
        assert!(
            has_edge("manifest_entries", "manifests"),
            "issues: {:?}",
            report.issues
        );
        assert!(
            has_edge("cartridge_volumes", "volumes"),
            "issues: {:?}",
            report.issues
        );
        assert!(
            has_edge("volume_deposits", "volumes"),
            "issues: {:?}",
            report.issues
        );
    }

    /// Issue #177: `--repair`'s naive DELETEs failed with a bare
    /// `FOREIGN KEY constraint failed` whenever an orphan row had children
    /// of its own -- exactly the shape a real corrupt catalog has, and the
    /// only shape `--repair` exists to fix. This fixture builds a `writes`
    /// row orphaned on `volumes` with a `write_positions` child and a
    /// `verification_results` grandchild (via that child), plus a
    /// `stage_slices` row orphaned on `stage_sets` referenced by a second
    /// `write_positions` row and by the orphan write's
    /// `sacrificed_slice_id`.
    ///
    /// Negative control (pre-fix HEAD): `db_fsck(&conn, true)` returns
    /// `Err` whose text contains `FOREIGN KEY constraint failed`, and every
    /// orphan row is left in place.
    #[test]
    fn fsck_repair_deletes_children_before_parents() {
        let conn = crate::db::open_memory().unwrap();

        conn.execute(
            "INSERT INTO tenants (name, is_operator) VALUES ('t1', 0)",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id) VALUES ('u-1', 'unit1', ?1)",
            [tenant_id],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, source_path) VALUES (?1, 1, '/src')",
            [unit_id],
        )
        .unwrap();
        let snap_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, slice_size) VALUES (?1, 1048576)",
            [snap_id],
        )
        .unwrap();
        let stage_set_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes,
                                       encrypted_bytes, sha256_plain, sha256_encrypted)
             VALUES (?1, 0, 1, 1, 'aa', 'bb')",
            [stage_set_id],
        )
        .unwrap();
        let slice_valid_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes)
             VALUES ('vol1', 'lto', 'drive0', 1000000)",
            [],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO verification_sessions (volume_id, verify_type, outcome)
             VALUES (?1, 'full', 'passed')",
            [volume_id],
        )
        .unwrap();
        let session_id = conn.last_insert_rowid();

        conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();

        // An orphan stage_slices row -- its OWN stage_set_id dangles.
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes,
                                       encrypted_bytes, sha256_plain, sha256_encrypted)
             VALUES (88888, 0, 1, 1, 'cc', 'dd')",
            [],
        )
        .unwrap();
        let slice_orphan_id = conn.last_insert_rowid();

        // An orphan writes row -- its OWN volume_id dangles -- that also
        // references the orphan slice via `sacrificed_slice_id` (that
        // reference is not itself dangling: the row exists).
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, sacrificed_slice_id)
             VALUES (?1, ?2, 99999, 'completed', ?3)",
            params![stage_set_id, snap_id, slice_orphan_id],
        )
        .unwrap();
        let write_orphan_id = conn.last_insert_rowid();

        // write_positions child of the orphan write, pointing at the VALID
        // slice -- it only dangles once its parent write is deleted.
        conn.execute(
            "INSERT INTO write_positions (write_id, stage_slice_id, position)
             VALUES (?1, ?2, '1')",
            params![write_orphan_id, slice_valid_id],
        )
        .unwrap();
        let wp_child_id = conn.last_insert_rowid();

        // A second write_positions row referencing the orphan slice
        // directly -- it only dangles once the orphan slice is deleted.
        conn.execute(
            "INSERT INTO write_positions (write_id, stage_slice_id, position)
             VALUES (?1, ?2, '2')",
            params![write_orphan_id, slice_orphan_id],
        )
        .unwrap();
        let wp_slice_ref_id = conn.last_insert_rowid();

        // verification_results grandchild of the orphan write, via
        // wp_child -- only dangles once wp_child is deleted.
        conn.execute(
            "INSERT INTO verification_results (session_id, write_position_id, stage_slice_id, result)
             VALUES (?1, ?2, ?3, 'passed')",
            params![session_id, wp_child_id, slice_valid_id],
        )
        .unwrap();
        let vr_id = conn.last_insert_rowid();

        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();

        let report = db_fsck(&conn, true).unwrap();
        assert!(report.integrity_ok);
        assert_eq!(
            report.repaired, 5,
            "must delete write_orphan, slice_orphan, both write_positions \
             rows, and the verification_results row -- report: {report:?}"
        );

        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            remaining, 0,
            "the FK graph must be fully closed after repair"
        );

        for (table, id) in [
            ("writes", write_orphan_id),
            ("stage_slices", slice_orphan_id),
            ("write_positions", wp_child_id),
            ("write_positions", wp_slice_ref_id),
            ("verification_results", vr_id),
        ] {
            let left: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE rowid = {id}"),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(left, 0, "{table} rowid {id} should have been deleted");
        }

        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE action = 'db_fsck_repair'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 1, "a repair must leave exactly one audit event");
    }

    /// Issue #104: insert orphans into BOTH tables `db_fsck` repairs, then
    /// repair. Three properties at once: `repaired` is a row count (3, not
    /// the old category count of 2), both tables are actually emptied by
    /// the one transaction, and exactly one `events` row records it.
    ///
    /// This is the test that fails against pre-#104 code: it asserted
    /// `repaired == 2` there, and found no event at all.
    ///
    /// Issue #177 strengthens this test's `issues` expectation (2 -> 4):
    /// `pragma_foreign_key_check` reports one line per (child, parent,
    /// constraint) group, and these two `writes` rows dangle on THREE
    /// parents (volumes, stage_sets, snapshots) while the `stage_slices`
    /// row dangles on one (stage_sets) -- four groups, not the old two
    /// hand-kept categories. `repaired == 3` and the single-event
    /// assertion are unchanged.
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
        assert_eq!(dry.issues.len(), 4, "issues: {:?}", dry.issues);
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
