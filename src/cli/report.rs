use clap::Subcommand;
use rusqlite::{params, Connection};

use crate::config::Config;
use crate::db::queries;
use crate::error::Result;

#[derive(Subcommand, Debug)]
pub enum ReportCommands {
    /// Overview: units, tapes, tenants, capacity
    Summary,
    /// Units at risk: below min copies/locations
    FireRisk,
    /// Copy count distribution per unit
    Copies {
        /// Filter to specific unit
        #[arg(long)]
        unit: Option<String>,
    },
    /// Tape-only unit status
    TapeOnly {
        /// Filter to specific unit
        #[arg(long)]
        unit: Option<String>,
    },
    /// Units with changes since last snapshot
    Dirty {
        /// Filter to specific unit
        #[arg(long)]
        unit: Option<String>,
    },
    /// Staged data pending write
    Pending,
    /// Verification recency
    VerifyStatus {
        /// Filter to specific volume
        #[arg(long)]
        volume: Option<String>,
    },
    /// Drive error trends
    Health {
        /// Filter to specific volume
        #[arg(long)]
        volume: Option<String>,
    },
    /// Volume capacity utilization
    Capacity {
        /// Per-volume breakdown
        #[arg(long)]
        per_volume: bool,
    },
    /// Snapshot age distribution
    Age {
        /// Filter to specific unit
        #[arg(long)]
        unit: Option<String>,
    },
    /// Audit trail browsing
    Events {
        /// Filter by entity type
        #[arg(long)]
        entity: Option<String>,
        /// Limit to last N days
        #[arg(long)]
        days: Option<i64>,
    },
    /// Volumes flagged for compaction
    CompactionCandidates,
    /// Superseded snapshot versions that could be marked reclaimable
    Supersedable,
}

pub fn run(
    conn: &Connection,
    config: &Config,
    command: &ReportCommands,
    json_output: bool,
) -> Result<()> {
    match command {
        ReportCommands::Summary => report_summary(conn, json_output),
        ReportCommands::FireRisk => report_fire_risk(conn, config, json_output),
        ReportCommands::Copies { unit } => report_copies(conn, unit.as_deref(), json_output),
        ReportCommands::TapeOnly { unit } => report_tape_only(conn, unit.as_deref(), json_output),
        ReportCommands::Dirty { unit } => report_dirty(
            conn,
            unit.as_deref(),
            json_output,
            &config.defaults.global_excludes,
        ),
        ReportCommands::Pending => report_pending(conn, json_output),
        ReportCommands::VerifyStatus { volume } => {
            report_verify_status(conn, volume.as_deref(), json_output)
        }
        ReportCommands::Health { volume } => report_health(conn, volume.as_deref(), json_output),
        ReportCommands::Capacity { per_volume } => report_capacity(conn, *per_volume, json_output),
        ReportCommands::Age { unit } => report_age(conn, unit.as_deref(), json_output),
        ReportCommands::Events { entity, days } => {
            report_events(conn, entity.as_deref(), *days, json_output)
        }
        ReportCommands::CompactionCandidates => {
            report_compaction_candidates(conn, config, json_output)
        }
        ReportCommands::Supersedable => report_supersedable(conn, config, json_output),
    }
}

/// Issue #90 (as re-scoped): superseded versions accumulate silently
/// because `snapshot mark-reclaimable` is a manual step nothing prompts.
/// This is the prompt.
///
/// Deliberately a sibling of `compaction-candidates` rather than an
/// extension of it: that report is per-VOLUME and its `live_bytes` must
/// not change (it agrees with `compact_read`/`compact_finish` today, and
/// making it "smarter" would advertise space compaction carries forward
/// anyway). This signal is per-UNIT/per-snapshot — a different grain.
///
/// Releasability is NOT recomputed here. It comes from
/// `policy::reclaimable::assess`, the same function `snapshot
/// mark-reclaimable` gates on, so the two can never disagree. Blocked
/// candidates are listed with their reason rather than hidden: that is
/// strictly more useful and cannot over-promise, and only the releasable
/// ones are summed into the total.
fn report_supersedable(conn: &Connection, config: &Config, json_output: bool) -> Result<()> {
    use crate::policy::reclaimable::ReclaimVerdict;

    let found = crate::policy::reclaimable::candidates(conn, config)?;
    let summary = supersedable_summary(&found);

    if json_output {
        println!("{summary}");
    } else if found.is_empty() {
        println!("supersedable: nothing to release (no unit has more than one current snapshot)");
    } else {
        println!("supersedable snapshots ({} candidate(s)):", found.len());
        for c in &found {
            match &c.verdict {
                ReclaimVerdict::Releasable {
                    superseding_version,
                    freeable_bytes,
                } => println!(
                    "  {} v{}: superseded by v{superseding_version} — RELEASABLE, frees {} MB\n      tapectl snapshot mark-reclaimable {} --version {}",
                    c.unit_name,
                    c.version,
                    freeable_bytes / (1024 * 1024),
                    c.unit_name,
                    c.version,
                ),
                ReclaimVerdict::Blocked {
                    freeable_bytes,
                    reason,
                    ..
                } => println!(
                    "  {} v{}: BLOCKED ({} MB would be freed) — {reason}",
                    c.unit_name,
                    c.version,
                    freeable_bytes / (1024 * 1024),
                ),
            }
        }
        let releasable = summary["releasable"].as_u64().unwrap_or(0);
        if releasable == 0 {
            println!("nothing to release: every candidate is blocked");
        } else {
            println!(
                "{releasable} releasable, freeing {} MB total",
                summary["total_freeable_bytes"].as_i64().unwrap_or(0) / (1024 * 1024)
            );
        }
    }
    Ok(())
}

/// The `--json` payload, built separately from printing so tests can
/// assert the numbers rather than only that rendering did not error.
/// The total sums the RELEASABLE candidates only — a blocked one's bytes
/// are shown per-row (they are what clearing the blocker would buy) but
/// must never be advertised as available.
fn supersedable_summary(found: &[crate::policy::reclaimable::Candidate]) -> serde_json::Value {
    use crate::policy::reclaimable::ReclaimVerdict;

    let mut total_freeable: i64 = 0;
    let mut releasable = 0usize;
    let mut rows: Vec<serde_json::Value> = Vec::new();

    for c in found {
        let (is_releasable, superseding, bytes, reason) = match &c.verdict {
            ReclaimVerdict::Releasable {
                superseding_version,
                freeable_bytes,
            } => {
                releasable += 1;
                total_freeable += freeable_bytes;
                (
                    true,
                    Some(*superseding_version),
                    *freeable_bytes,
                    serde_json::Value::Null,
                )
            }
            ReclaimVerdict::Blocked {
                superseding_version,
                freeable_bytes,
                reason,
            } => (
                false,
                *superseding_version,
                *freeable_bytes,
                serde_json::Value::String(reason.clone()),
            ),
        };
        rows.push(serde_json::json!({
            "unit": c.unit_name, "version": c.version,
            "superseding_version": superseding,
            "freeable_bytes": bytes,
            "releasable": is_releasable,
            "blocked_reason": reason,
        }));
    }

    serde_json::json!({
        "supersedable": rows.len(),
        "releasable": releasable,
        "total_freeable_bytes": total_freeable,
        "candidates": rows,
    })
}

/// Count of volumes whose physical media is in service and accounted for
/// (`report summary`). Group-B (issue #96): inventory, not copy-counting.
fn in_service_volume_count(conn: &Connection) -> Result<i64> {
    let sql = format!(
        "SELECT COUNT(*) FROM volumes WHERE {}",
        crate::policy::coverage::in_service("volumes")
    );
    Ok(conn.query_row(&sql, [], |r| r.get(0))?)
}

/// Bytes held by the volumes [`in_service_volume_count`] counts.
///
/// Filtered by the SAME predicate on purpose (issue #96): an unfiltered sum
/// here would report bytes carried by retired, erased, and missing media
/// beside a count that excludes exactly those volumes, so the two numbers
/// on one `report summary` line would describe different populations.
fn in_service_bytes_written(conn: &Connection) -> Result<i64> {
    let sql = format!(
        "SELECT COALESCE(SUM(bytes_written),0) FROM volumes WHERE {}",
        crate::policy::coverage::in_service("volumes")
    );
    Ok(conn.query_row(&sql, [], |r| r.get(0))?)
}

fn report_summary(conn: &Connection, json_output: bool) -> Result<()> {
    let unit_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM units WHERE status = 'active'",
        [],
        |r| r.get(0),
    )?;
    let tenant_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tenants WHERE status = 'active'",
        [],
        |r| r.get(0),
    )?;
    let snapshot_count: i64 = conn.query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))?;
    let volume_count: i64 = in_service_volume_count(conn)?;
    let write_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM writes WHERE status = 'completed'",
        [],
        |r| r.get(0),
    )?;
    let total_bytes: i64 = in_service_bytes_written(conn)?;
    let staged_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM stage_sets WHERE status = 'staged'",
        [],
        |r| r.get(0),
    )?;

    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "units": unit_count, "tenants": tenant_count, "snapshots": snapshot_count,
                "volumes": volume_count, "writes": write_count, "total_bytes": total_bytes,
                "staged_pending": staged_count,
            })
        );
    } else {
        println!("tapectl summary");
        println!("  Tenants:    {tenant_count}");
        println!("  Units:      {unit_count} active");
        println!("  Snapshots:  {snapshot_count}");
        println!("  Volumes:    {volume_count} active");
        println!("  Writes:     {write_count} completed");
        println!(
            "  Total data: {} GB on tape",
            total_bytes / (1024 * 1024 * 1024)
        );
        if staged_count > 0 {
            println!("  Pending:    {staged_count} stage set(s) awaiting write");
        }
    }
    Ok(())
}

fn report_fire_risk(conn: &Connection, config: &Config, json_output: bool) -> Result<()> {
    let min_copies = config.defaults.min_copies_for_tape_only as i64;
    let mut risks: Vec<serde_json::Value> = Vec::new();

    // Units with fewer copies than min_copies. ADR-0004 (issue #89): a
    // non-sealed volume's write must not count, but this query LEFT JOINs
    // volumes to preserve units with zero writes (so they still surface
    // with copies=0 rather than vanishing from the result) — putting the
    // predicate in the JOIN's ON clause wouldn't work, since
    // `COUNT(DISTINCT w.volume_id)` reads from the `writes` row, which
    // stays non-NULL even when the volumes join fails to match. Wrapping
    // it in a CASE instead correctly nulls out (and so excludes from
    // COUNT DISTINCT) exactly the non-sealed writes, while leaving a
    // unit with no writes at all still NULL -> copies=0, same as before.
    let sealed = crate::policy::coverage::eligible("v");
    let sql = format!(
        "SELECT u.name, u.status,
                COUNT(DISTINCT CASE WHEN {sealed} THEN w.volume_id END) as copies,
                COUNT(DISTINCT CASE WHEN {sealed} THEN v.location_id END) as locations
         FROM units u
         LEFT JOIN snapshots s ON s.unit_id = u.id AND s.status = 'current'
         LEFT JOIN stage_sets ss ON ss.snapshot_id = s.id
         LEFT JOIN writes w ON w.stage_set_id = ss.id AND w.status = 'completed'
         LEFT JOIN volumes v ON v.id = w.volume_id
         WHERE u.status = 'active'
         GROUP BY u.id
         HAVING copies < ?1 OR copies = 0"
    );
    let mut stmt = conn.prepare(&sql)?;
    let at_risk: Vec<(String, String, i64, i64)> = stmt
        .query_map(params![min_copies], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    for (name, status, copies, locations) in &at_risk {
        risks.push(serde_json::json!({
            "unit": name, "status": status, "copies": copies, "locations": locations,
            "risk": if *copies == 0 { "no_copies" } else { "below_minimum" },
        }));
    }

    if json_output {
        println!(
            "{}",
            serde_json::json!({"at_risk": risks.len(), "units": risks})
        );
    } else if risks.is_empty() {
        println!("fire-risk: all units meet minimum copy requirements");
    } else {
        println!("FIRE RISK: {} unit(s) at risk", risks.len());
        for (name, _status, copies, locations) in &at_risk {
            let severity = if *copies == 0 {
                "ZERO COPIES"
            } else {
                "below minimum"
            };
            println!("  {name}: {copies} copies, {locations} locations — {severity}");
        }
    }
    Ok(())
}

/// Per-unit `(name, copies, locations, volume_labels)` rows behind `report
/// copies`, split out from the printing so the computed counts are
/// directly assertable in tests without capturing stdout (same pattern as
/// `dirty_rows` above `report_dirty`).
///
/// `pub(crate)` so `cli::operations`'s tests can call this directly
/// against the same fixture a gate is tested with, proving `report
/// copies` and the gates agree on the count — see
/// `cli::audit::copy_count_for_unit`'s doc comment for why that
/// cross-file equality is worth proving explicitly.
///
/// Routes copies/locations/volume-labels all through the shared ADR-0004
/// predicate: see the comment in `report_fire_risk` above for why it must
/// be a `CASE` inside the aggregate rather than a join condition.
pub(crate) type CopyRow = (String, i64, i64, Option<String>);

pub(crate) fn copies_rows(conn: &Connection, unit_filter: Option<&str>) -> Result<Vec<CopyRow>> {
    let sealed = crate::policy::coverage::eligible("v");
    let mut sql = format!(
        "SELECT u.name,
                COUNT(DISTINCT CASE WHEN {sealed} THEN w.volume_id END) as copies,
                COUNT(DISTINCT CASE WHEN {sealed} THEN v.location_id END) as locations,
                GROUP_CONCAT(DISTINCT CASE WHEN {sealed} THEN v.label END) as volumes
         FROM units u
         LEFT JOIN snapshots s ON s.unit_id = u.id AND s.status = 'current'
         LEFT JOIN stage_sets ss ON ss.snapshot_id = s.id
         LEFT JOIN writes w ON w.stage_set_id = ss.id AND w.status = 'completed'
         LEFT JOIN volumes v ON v.id = w.volume_id
         WHERE u.status IN ('active', 'tape_only')"
    );
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(name) = unit_filter {
        sql.push_str(" AND u.name = ?");
        param_values.push(Box::new(name.to_string()));
    }
    sql.push_str(" GROUP BY u.id ORDER BY copies ASC, u.name");

    let params_ref: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<CopyRow> = stmt
        .query_map(params_ref.as_slice(), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn report_copies(conn: &Connection, unit_filter: Option<&str>, json_output: bool) -> Result<()> {
    let rows = copies_rows(conn, unit_filter)?;

    if json_output {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|(name, copies, locs, vols)| {
                serde_json::json!({"unit": name, "copies": copies, "locations": locs, "volumes": vols})
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else {
        for (name, copies, locs, vols) in &rows {
            println!(
                "  {name}: {copies} copies, {locs} locations [{}]",
                vols.as_deref().unwrap_or("-")
            );
        }
    }
    Ok(())
}

fn report_tape_only(conn: &Connection, unit_filter: Option<&str>, json_output: bool) -> Result<()> {
    // Same ADR-0004 CASE-in-aggregate treatment as `report_fire_risk` and
    // `copies_rows` above (issue #89): the volumes LEFT JOIN must stay a
    // LEFT JOIN (to keep tape-only units with zero eligible copies in the
    // result), so the predicate goes inside the aggregate, not the join.
    let sealed = crate::policy::coverage::eligible("v");
    let mut sql = format!(
        "SELECT u.name,
                COUNT(DISTINCT CASE WHEN {sealed} THEN w.volume_id END) as copies,
                COUNT(DISTINCT CASE WHEN {sealed} THEN v.location_id END) as locations
         FROM units u
         LEFT JOIN snapshots s ON s.unit_id = u.id AND s.status = 'current'
         LEFT JOIN stage_sets ss ON ss.snapshot_id = s.id
         LEFT JOIN writes w ON w.stage_set_id = ss.id AND w.status = 'completed'
         LEFT JOIN volumes v ON v.id = w.volume_id
         WHERE u.status = 'tape_only'"
    );
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(name) = unit_filter {
        sql.push_str(" AND u.name = ?");
        param_values.push(Box::new(name.to_string()));
    }
    sql.push_str(" GROUP BY u.id ORDER BY u.name");

    let params_ref: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<(String, i64, i64)> = stmt
        .query_map(params_ref.as_slice(), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    if json_output {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|(name, copies, locs)| serde_json::json!({"unit": name, "copies": copies, "locations": locs}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else if rows.is_empty() {
        println!("no tape-only units");
    } else {
        println!("tape-only units:");
        for (name, copies, locs) in &rows {
            println!("  {name}: {copies} copies, {locs} locations");
        }
    }
    Ok(())
}

/// One unit's dirty-scan row, split out from `report_dirty`'s printing so
/// the computed result is directly assertable in tests without capturing
/// stdout.
struct DirtyRow {
    name: String,
    state: &'static str, // "clean" | "new" | "dirty"
    added: Vec<String>,
    removed: Vec<String>,
    modified: Vec<String>,
}

/// The scan behind `report dirty`: reuses `fingerprint::classify` — the
/// same scan `unit status --dirty` and `mark-tape-only`'s guard use — over
/// every `active` unit (mirroring `pending_units_for_collection`'s own
/// scope: `missing`/`tape_only`/`retired` units have no live directory this
/// scan should second-guess), optionally narrowed to one unit by name.
/// `global_excludes` is `config.defaults.global_excludes` (issue #49),
/// kept in lockstep with those other callers.
fn dirty_rows(
    conn: &Connection,
    unit_filter: Option<&str>,
    global_excludes: &[String],
) -> Result<Vec<DirtyRow>> {
    use crate::collection::fingerprint::{self, PendingReason};

    // In-memory name filter (same pattern `unit list --tag` already uses)
    // rather than a second SQL path — `queries::list_units` is the one
    // place "active units" is derived from.
    let mut units = queries::list_units(conn, None, Some("active"))?;
    if let Some(name) = unit_filter {
        units.retain(|u| u.name == name);
    }

    let mut rows = Vec::with_capacity(units.len());
    for unit in &units {
        let (state, changes) = match fingerprint::classify(conn, unit, global_excludes)? {
            None => ("clean", fingerprint::FingerprintDiff::default()),
            Some(p) if p.reason == PendingReason::New => {
                ("new", fingerprint::FingerprintDiff::default())
            }
            Some(p) => ("dirty", p.changes),
        };
        rows.push(DirtyRow {
            name: unit.name.clone(),
            state,
            added: changes.added,
            removed: changes.removed,
            modified: changes.modified,
        });
    }
    Ok(rows)
}

fn report_dirty(
    conn: &Connection,
    unit_filter: Option<&str>,
    json_output: bool,
    global_excludes: &[String],
) -> Result<()> {
    let rows = dirty_rows(conn, unit_filter, global_excludes)?;

    if json_output {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "unit": r.name, "state": r.state,
                    "added": r.added, "removed": r.removed, "modified": r.modified,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else if rows.is_empty() {
        println!("no active units");
    } else {
        let dirty_count = rows.iter().filter(|r| r.state == "dirty").count();
        println!(
            "dirty scan: {dirty_count} of {} active unit(s) dirty",
            rows.len()
        );
        for r in &rows {
            match r.state {
                "clean" => println!("  {}: clean", r.name),
                "new" => println!("  {}: new — never archived", r.name),
                _ => println!(
                    "  {}: dirty ({} added, {} removed, {} modified)",
                    r.name,
                    r.added.len(),
                    r.removed.len(),
                    r.modified.len(),
                ),
            }
        }
    }
    Ok(())
}

fn report_pending(conn: &Connection, json_output: bool) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT u.name, s.version, ss.status, ss.num_slices, ss.total_encrypted_size
         FROM stage_sets ss
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN units u ON u.id = s.unit_id
         WHERE ss.status = 'staged'
         ORDER BY u.name",
    )?;
    type Row = (String, i64, String, Option<i64>, Option<i64>);
    let rows: Vec<Row> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    if json_output {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|(name, ver, status, slices, size)| {
                serde_json::json!({"unit": name, "version": ver, "status": status, "slices": slices, "size": size})
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else if rows.is_empty() {
        println!("no pending stage sets");
    } else {
        println!("pending writes:");
        for (name, ver, _status, slices, size) in &rows {
            println!(
                "  {name} v{ver}: {} slices, {} MB",
                slices.unwrap_or(0),
                size.unwrap_or(0) / (1024 * 1024),
            );
        }
    }
    Ok(())
}

fn report_verify_status(
    conn: &Connection,
    volume_filter: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let mut sql = String::from(
        "SELECT v.label, vs.verify_type, vs.outcome, vs.completed_at,
                vs.slices_checked, vs.slices_passed, vs.slices_failed
         FROM verification_sessions vs
         JOIN volumes v ON v.id = vs.volume_id
         WHERE 1=1",
    );
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(label) = volume_filter {
        sql.push_str(" AND v.label = ?");
        param_values.push(Box::new(label.to_string()));
    }
    sql.push_str(" ORDER BY vs.completed_at DESC");

    let params_ref: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    type Row = (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    );
    let rows: Vec<Row> = stmt
        .query_map(params_ref.as_slice(), |row| {
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

    if json_output {
        let json: Vec<serde_json::Value> = rows.iter().map(|(label, vtype, outcome, completed, checked, passed, failed)| {
            serde_json::json!({"volume": label, "type": vtype, "outcome": outcome, "completed": completed, "checked": checked, "passed": passed, "failed": failed})
        }).collect();
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else if rows.is_empty() {
        println!("no verification sessions found");
    } else {
        for (label, vtype, outcome, completed, checked, passed, failed) in &rows {
            println!(
                "  {label}: {} {} at {} ({}/{}/{} checked/passed/failed)",
                vtype.as_deref().unwrap_or("?"),
                outcome.as_deref().unwrap_or("?"),
                completed.as_deref().unwrap_or("?"),
                checked.unwrap_or(0),
                passed.unwrap_or(0),
                failed.unwrap_or(0),
            );
        }
    }
    Ok(())
}

fn report_health(conn: &Connection, volume_filter: Option<&str>, json_output: bool) -> Result<()> {
    let mut sql = String::from(
        "SELECT v.label, h.operation, h.logged_at, h.total_bytes,
                h.total_corrected, h.total_uncorrected
         FROM health_logs h
         JOIN volumes v ON v.id = h.volume_id
         WHERE 1=1",
    );
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(label) = volume_filter {
        sql.push_str(" AND v.label = ?");
        param_values.push(Box::new(label.to_string()));
    }
    sql.push_str(" ORDER BY h.logged_at DESC LIMIT 50");

    let params_ref: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    type Row = (
        String,
        Option<String>,
        String,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    );
    let rows: Vec<Row> = stmt
        .query_map(params_ref.as_slice(), |row| {
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

    if json_output {
        let json: Vec<serde_json::Value> = rows.iter().map(|(label, op, at, bytes, corrected, uncorrected)| {
            serde_json::json!({"volume": label, "operation": op, "at": at, "bytes": bytes, "corrected": corrected, "uncorrected": uncorrected})
        }).collect();
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else if rows.is_empty() {
        println!("no health logs recorded");
    } else {
        for (label, op, at, _bytes, corrected, uncorrected) in &rows {
            println!(
                "  {label} {}: {} — corrected={} uncorrected={}",
                op.as_deref().unwrap_or("?"),
                at,
                corrected.unwrap_or(0),
                uncorrected.unwrap_or(0),
            );
        }
    }
    Ok(())
}

/// Per-volume capacity rows: `(label, capacity_bytes, bytes_written, status)`.
/// Group-B (issue #96) plus `initialized` — this listing deliberately shows
/// provisioned-but-not-yet-written media too, so the operator can see a tape
/// that is ready to receive bytes.
fn per_volume_capacity_rows(conn: &Connection) -> Result<Vec<(String, i64, i64, String)>> {
    let sql = format!(
        "SELECT label, capacity_bytes, bytes_written, status FROM volumes
         WHERE {}
         ORDER BY label",
        crate::policy::coverage::in_service_or_provisioned("volumes")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Aggregate capacity across in-service media: `(total_capacity,
/// total_bytes_written, volume_count)`. Group-B (issue #96).
fn capacity_totals(conn: &Connection) -> Result<(i64, i64, i64)> {
    let sql = format!(
        "SELECT COALESCE(SUM(capacity_bytes),0), COALESCE(SUM(bytes_written),0), COUNT(*)
         FROM volumes WHERE {}",
        crate::policy::coverage::in_service("volumes")
    );
    Ok(conn.query_row(&sql, [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?)
}

fn report_capacity(conn: &Connection, per_volume: bool, json_output: bool) -> Result<()> {
    if per_volume {
        let rows = per_volume_capacity_rows(conn)?;

        if json_output {
            let json: Vec<serde_json::Value> = rows.iter().map(|(label, cap, written, status)| {
                serde_json::json!({"volume": label, "capacity": cap, "written": written, "status": status})
            }).collect();
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        } else {
            for (label, cap, written, status) in &rows {
                let pct = if *cap > 0 {
                    (*written as f64 / *cap as f64) * 100.0
                } else {
                    0.0
                };
                println!(
                    "  {label} [{status}]: {} / {} GB ({pct:.1}%)",
                    written / (1024 * 1024 * 1024),
                    cap / (1024 * 1024 * 1024),
                );
            }
        }
    } else {
        let (total_cap, total_written, vol_count) = capacity_totals(conn)?;

        if json_output {
            println!(
                "{}",
                serde_json::json!({
                    "volumes": vol_count, "total_capacity": total_cap, "total_written": total_written,
                })
            );
        } else {
            let pct = if total_cap > 0 {
                (total_written as f64 / total_cap as f64) * 100.0
            } else {
                0.0
            };
            println!(
                "capacity: {} volumes, {} / {} GB ({pct:.1}%)",
                vol_count,
                total_written / (1024 * 1024 * 1024),
                total_cap / (1024 * 1024 * 1024),
            );
        }
    }
    Ok(())
}

fn report_age(conn: &Connection, unit_filter: Option<&str>, json_output: bool) -> Result<()> {
    let mut sql = String::from(
        "SELECT u.name, s.version, s.status, s.created_at
         FROM snapshots s
         JOIN units u ON u.id = s.unit_id
         WHERE s.status = 'current'",
    );
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(name) = unit_filter {
        sql.push_str(" AND u.name = ?");
        param_values.push(Box::new(name.to_string()));
    }
    sql.push_str(" ORDER BY s.created_at ASC");

    let params_ref: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<(String, i64, String, String)> = stmt
        .query_map(params_ref.as_slice(), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    if json_output {
        let json: Vec<serde_json::Value> = rows.iter().map(|(name, ver, status, created)| {
            serde_json::json!({"unit": name, "version": ver, "status": status, "created_at": created})
        }).collect();
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else if rows.is_empty() {
        println!("no current snapshots");
    } else {
        println!("current snapshot ages (oldest first):");
        for (name, ver, _status, created) in &rows {
            println!("  {name} v{ver}: {created}");
        }
    }
    Ok(())
}

fn report_events(
    conn: &Connection,
    entity_filter: Option<&str>,
    days: Option<i64>,
    json_output: bool,
) -> Result<()> {
    let mut sql = String::from(
        "SELECT timestamp, entity_type, entity_label, action, field, old_value, new_value
         FROM events WHERE 1=1",
    );
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(entity) = entity_filter {
        sql.push_str(" AND entity_type = ?");
        param_values.push(Box::new(entity.to_string()));
    }
    if let Some(d) = days {
        sql.push_str(&format!(" AND timestamp >= datetime('now', '-{d} days')"));
    }
    sql.push_str(" ORDER BY timestamp DESC LIMIT 100");

    let params_ref: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    type Row = (
        String,
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = stmt
        .query_map(params_ref.as_slice(), |row| {
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

    if json_output {
        let json: Vec<serde_json::Value> = rows.iter().map(|(ts, etype, label, action, field, old, new)| {
            serde_json::json!({"timestamp": ts, "entity_type": etype, "entity_label": label, "action": action, "field": field, "old_value": old, "new_value": new})
        }).collect();
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else if rows.is_empty() {
        println!("no events found");
    } else {
        for (ts, etype, label, action, field, _old, _new) in &rows {
            let label_str = label.as_deref().unwrap_or("?");
            let field_str = field.as_ref().map(|f| format!(".{f}")).unwrap_or_default();
            println!("  {ts} {etype}/{label_str} {action}{field_str}");
        }
    }
    Ok(())
}

/// Compaction-candidate rows: `(label, total_bytes, live_bytes,
/// reclaimable_bytes)`. Group-A (issue #96): compaction only ever targets a
/// *finished* volume, so this is the ADR-0004 `sealed` predicate.
fn compaction_candidate_rows(conn: &Connection) -> Result<Vec<(String, i64, i64, i64)>> {
    let sql = format!(
        "SELECT v.label, v.bytes_written,
                SUM(CASE WHEN s.status NOT IN ('reclaimable','purged') THEN ss.encrypted_bytes ELSE 0 END) as live_bytes,
                SUM(CASE WHEN s.status IN ('reclaimable','purged') THEN ss.encrypted_bytes ELSE 0 END) as reclaimable_bytes
         FROM volumes v
         JOIN writes w ON w.volume_id = v.id AND w.status = 'completed'
         JOIN stage_sets sts ON sts.id = w.stage_set_id
         JOIN snapshots s ON s.id = sts.snapshot_id
         JOIN stage_slices ss ON ss.stage_set_id = sts.id
         WHERE {}
         GROUP BY v.id
         ORDER BY live_bytes * 1.0 / NULLIF(v.bytes_written, 0) ASC",
        crate::policy::coverage::eligible("v")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn report_compaction_candidates(
    conn: &Connection,
    config: &Config,
    json_output: bool,
) -> Result<()> {
    let threshold = config.compaction.utilization_threshold;

    let rows = compaction_candidate_rows(conn)?;

    if json_output {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|(label, total, live, reclaimable)| {
                let util = if *total > 0 {
                    *live as f64 / *total as f64
                } else {
                    1.0
                };
                serde_json::json!({
                    "volume": label, "total_bytes": total, "live_bytes": live,
                    "reclaimable_bytes": reclaimable, "utilization": util,
                    "flagged": util < threshold,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else {
        let mut flagged = 0;
        for (label, total, live, reclaimable) in &rows {
            let util = if *total > 0 {
                *live as f64 / *total as f64
            } else {
                1.0
            };
            let flag = if util < threshold {
                " *** CANDIDATE ***"
            } else {
                ""
            };
            if util < threshold {
                flagged += 1;
            }
            println!(
                "  {label}: {:.0}% utilized ({} MB live, {} MB reclaimable){flag}",
                util * 100.0,
                live / (1024 * 1024),
                reclaimable / (1024 * 1024),
            );
        }
        if flagged == 0 {
            println!(
                "no compaction candidates (threshold: {:.0}%)",
                threshold * 100.0
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! `report dirty` (issue #36/H10): the old stub reported snapshot AGE,
    //! not dirtiness, and its `unit` filter was ignored (parameter named
    //! `_unit_filter`). These tests exercise the real classify()-backed
    //! scan and confirm the filter now actually narrows the result.
    use super::*;
    use tempfile::TempDir;

    /// Two active units under one temp root: one left clean after its
    /// snapshot, one mutated (a new file) after its snapshot — full
    /// migrations + a real `snapshot_create`, so this exercises the exact
    /// `fingerprint::classify` path `dirty_rows` calls, not a hand-rolled
    /// substitute.
    fn setup_two_units(root: &TempDir) -> Connection {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();

        let clean_dir = root.path().join("clean_unit");
        let dirty_dir = root.path().join("dirty_unit");
        std::fs::create_dir_all(&clean_dir).unwrap();
        std::fs::create_dir_all(&dirty_dir).unwrap();
        std::fs::write(clean_dir.join("f.txt"), b"hello").unwrap();
        std::fs::write(dirty_dir.join("f.txt"), b"hello").unwrap();

        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, checksum_mode, encrypt, status)
             VALUES ('u-clean', 'clean_unit', ?1, ?2, 'mtime_size', 1, 'active')",
            params![tid, clean_dir.to_string_lossy().to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, checksum_mode, encrypt, status)
             VALUES ('u-dirty', 'dirty_unit', ?1, ?2, 'mtime_size', 1, 'active')",
            params![tid, dirty_dir.to_string_lossy().to_string()],
        )
        .unwrap();

        crate::staging::snapshot_create(&conn, "clean_unit", &[]).unwrap();
        crate::staging::snapshot_create(&conn, "dirty_unit", &[]).unwrap();

        // Mutate dirty_unit's directory after its snapshot was taken.
        std::fs::write(dirty_dir.join("g.txt"), b"new file").unwrap();

        conn
    }

    #[test]
    fn dirty_rows_reports_clean_and_dirty_units_separately() {
        let root = TempDir::new().unwrap();
        let conn = setup_two_units(&root);

        let rows = dirty_rows(&conn, None, &[]).unwrap();
        assert_eq!(rows.len(), 2);

        let clean = rows.iter().find(|r| r.name == "clean_unit").unwrap();
        assert_eq!(clean.state, "clean");
        assert!(clean.added.is_empty() && clean.removed.is_empty() && clean.modified.is_empty());

        let dirty = rows.iter().find(|r| r.name == "dirty_unit").unwrap();
        assert_eq!(dirty.state, "dirty");
        assert_eq!(dirty.added, vec!["g.txt".to_string()]);
    }

    #[test]
    fn dirty_rows_honors_the_unit_filter() {
        // The bug this fixes: the old parameter was named `_unit_filter`
        // and never consulted, so `--unit` silently did nothing.
        let root = TempDir::new().unwrap();
        let conn = setup_two_units(&root);

        let rows = dirty_rows(&conn, Some("dirty_unit"), &[]).unwrap();
        assert_eq!(rows.len(), 1, "--unit must narrow to exactly one unit");
        assert_eq!(rows[0].name, "dirty_unit");
        assert_eq!(rows[0].state, "dirty");

        let rows = dirty_rows(&conn, Some("clean_unit"), &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "clean_unit");
        assert_eq!(rows[0].state, "clean");
    }

    #[test]
    fn report_dirty_runs_end_to_end_in_both_output_modes() {
        let root = TempDir::new().unwrap();
        let conn = setup_two_units(&root);
        report_dirty(&conn, None, false, &[]).expect("plain output must succeed");
        report_dirty(&conn, Some("clean_unit"), true, &[]).expect("json output must succeed");
    }

    /// Issue #89 / ADR-0004: `copies_rows` must re-qualify eligibility at
    /// USE time via the shared `policy::coverage::eligible` predicate, the
    /// same rule the gates (`unit mark-tape-only`, `snapshot
    /// mark-reclaimable`) apply — see that function's doc comment and
    /// `report_fire_risk`'s inline comment for why the predicate has to
    /// live inside a `CASE` here rather than a join condition (this query
    /// LEFT JOINs volumes and must keep a zero-copy unit visible).
    mod adr0004_copy_eligibility {
        use super::*;

        /// tenant + unit (no `current_path` — this scan never touches
        /// disk) + one 'current' snapshot + one 'staged' stage_set
        /// completed-written to two volumes: `{name}-SEALED` (always
        /// `sealed`) and `{name}-OTHER` (status = `second_volume_status`).
        /// Mirrors
        /// `operations::tests::adr0004_copy_eligibility::setup_unit_with_two_volumes`
        /// and `audit::tests::setup_unit_with_two_volumes` — duplicated
        /// rather than shared, consistent with this crate's existing
        /// per-file test-fixture convention.
        fn setup_unit_with_two_volumes(name: &str, second_volume_status: &str) -> Connection {
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

            conn
        }

        fn assert_copies_rows_excludes_status(status: &str) {
            let name = format!("rep-{status}");
            let conn = setup_unit_with_two_volumes(&name, status);
            let rows = copies_rows(&conn, Some(&name)).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].1, 1, "a {status} second volume must not count");
        }

        #[test]
        fn copies_rows_excludes_a_quarantined_volume() {
            assert_copies_rows_excludes_status("quarantined");
        }

        #[test]
        fn copies_rows_excludes_a_retired_volume() {
            assert_copies_rows_excludes_status("retired");
        }

        #[test]
        fn copies_rows_excludes_an_erased_volume() {
            assert_copies_rows_excludes_status("erased");
        }

        #[test]
        fn copies_rows_excludes_a_missing_volume() {
            assert_copies_rows_excludes_status("missing");
        }

        #[test]
        fn copies_rows_counts_two_sealed_volumes_as_two() {
            let conn = setup_unit_with_two_volumes("rep-both-sealed", "sealed");
            let rows = copies_rows(&conn, Some("rep-both-sealed")).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0].1, 2,
                "two sealed volumes must both count -- the guard must not false-positive"
            );
        }

        #[test]
        fn copies_rows_volume_label_list_excludes_a_non_sealed_volume() {
            // The GROUP_CONCAT column must agree with the copies count it
            // sits next to -- a report line claiming "1 copy" must not
            // then list two volume labels as if both still counted.
            let conn = setup_unit_with_two_volumes("rep-label-list", "quarantined");
            let rows = copies_rows(&conn, Some("rep-label-list")).unwrap();
            assert_eq!(rows.len(), 1);
            let volumes = rows[0].3.as_deref().unwrap_or("");
            assert!(
                volumes.contains("rep-label-list-SEALED"),
                "the sealed volume must still be listed: {volumes}"
            );
            assert!(
                !volumes.contains("rep-label-list-OTHER"),
                "the quarantined volume must not be listed: {volumes}"
            );
        }

        #[test]
        fn report_copies_and_report_fire_risk_run_end_to_end_without_panicking() {
            // report_fire_risk shares the same CASE-in-aggregate fix but
            // has no extracted rows function to unit-test directly (it
            // filters units below min_copies rather than listing them
            // all) -- this at least exercises it end-to-end over a
            // fixture with a disqualified copy, so a regression that
            // makes the query itself invalid (bad SQL, wrong arity) is
            // still caught.
            let conn = setup_unit_with_two_volumes("rep-e2e", "quarantined");
            let config = Config::default();
            report_copies(&conn, Some("rep-e2e"), false).expect("plain output must succeed");
            report_copies(&conn, Some("rep-e2e"), true).expect("json output must succeed");
            report_fire_risk(&conn, &config, false).expect("fire-risk plain output must succeed");
            report_tape_only(&conn, Some("rep-e2e"), false)
                .expect("tape-only plain output must succeed");
        }
    }

    /// Issue #90: `report supersedable` must run end-to-end in both
    /// output modes, and must not perturb `report compaction-candidates`
    /// -- the whole reason it is a sibling report rather than an
    /// extension of that one.
    mod supersedable {
        use super::*;
        use crate::policy::reclaimable::tests::setup;

        fn summary_for(conn: &Connection) -> serde_json::Value {
            let found = crate::policy::reclaimable::candidates(conn, &Config::default()).unwrap();
            supersedable_summary(&found)
        }

        #[test]
        fn a_releasable_candidate_is_counted_and_summed() {
            let (conn, _unit) = setup("rep-sup-ok", 2, "sealed", "active");
            let s = summary_for(&conn);
            assert_eq!(s["supersedable"], 1);
            assert_eq!(s["releasable"], 1);
            assert_eq!(s["total_freeable_bytes"], 1000);
            let row = &s["candidates"][0];
            assert_eq!(row["unit"], "rep-sup-ok");
            assert_eq!(row["version"], 1);
            assert_eq!(row["superseding_version"], 2);
            assert_eq!(row["freeable_bytes"], 1000);
            assert_eq!(row["releasable"], true);
            assert!(row["blocked_reason"].is_null());

            report_supersedable(&conn, &Config::default(), false).unwrap();
            report_supersedable(&conn, &Config::default(), true).unwrap();
        }

        /// A blocked candidate carries the gate's own refusal text AND
        /// its byte figure (what clearing the blocker would buy), but
        /// must NOT be summed into the advertised total.
        #[test]
        fn a_blocked_candidate_reports_its_reason_and_bytes_but_is_not_summed() {
            let (conn, _unit) = setup("rep-sup-blocked", 2, "quarantined", "active");
            let s = summary_for(&conn);
            assert_eq!(s["supersedable"], 1);
            assert_eq!(s["releasable"], 0);
            assert_eq!(
                s["total_freeable_bytes"], 0,
                "blocked bytes must never be advertised as available"
            );
            let row = &s["candidates"][0];
            assert_eq!(row["releasable"], false);
            assert_eq!(row["freeable_bytes"], 1000);
            assert!(
                row["blocked_reason"]
                    .as_str()
                    .unwrap()
                    .contains("has 1 copies, needs 2"),
                "{row}"
            );

            report_supersedable(&conn, &Config::default(), false).unwrap();
            report_supersedable(&conn, &Config::default(), true).unwrap();
        }

        #[test]
        fn empty_case_renders() {
            let (conn, _unit) = setup("rep-sup-single", 1, "sealed", "active");
            report_supersedable(&conn, &Config::default(), false).unwrap();
        }

        #[test]
        fn compaction_candidates_still_runs_unchanged() {
            let (conn, _unit) = setup("rep-sup-compact", 2, "sealed", "active");
            report_compaction_candidates(&conn, &Config::default(), false).unwrap();
        }
    }

    /// Issue #96: the volume-status drift. Five inventory/capacity queries
    /// and `report compaction-candidates` filtered `volumes.status IN
    /// ('active','full')` — a set no v2 write ever leaves behind, since
    /// `SealedPending::confirm` writes `sealed`
    /// (`docs/design/layout-session.md`: `blank -> initialized -> active ->
    /// sealed`). Against a v2-only catalog every one of them returned the
    /// empty set. These tests pin BOTH halves of the ruling: the
    /// inventory/capacity surfaces must count `sealed` AND keep legacy
    /// `full`, while compaction must be `sealed`-only.
    mod issue96_volume_status_drift {
        use super::*;

        /// One volume of `status` holding `bytes_written` of media, of which
        /// `live` bytes belong to a `current` snapshot and `reclaimable`
        /// bytes to a `reclaimable` one. Every write is `completed`, so the
        /// only thing standing between this volume and each query under test
        /// is `volumes.status`.
        fn seed_written_volume(
            conn: &Connection,
            unit_id: i64,
            label: &str,
            status: &str,
            bytes_written: i64,
            live: i64,
            reclaimable: i64,
        ) {
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                      capacity_bytes, bytes_written, status)
                 VALUES (?1, 'lto', 'lto0', 'LTO-6', 1000, ?2, ?3)",
                params![label, bytes_written, status],
            )
            .unwrap();
            let vol_id = conn.last_insert_rowid();

            for (n, snap_status, bytes) in
                [(0i64, "current", live), (1i64, "reclaimable", reclaimable)]
            {
                if bytes <= 0 {
                    continue;
                }
                let version = vol_id * 10 + n;
                conn.execute(
                    "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
                     VALUES (?1, ?2, 'full', ?3, '/src')",
                    params![unit_id, version, snap_status],
                )
                .unwrap();
                let snap_id = conn.last_insert_rowid();
                conn.execute(
                    "INSERT INTO stage_sets (snapshot_id, status, slice_size)
                     VALUES (?1, 'staged', 524288)",
                    params![snap_id],
                )
                .unwrap();
                let stage_set_id = conn.last_insert_rowid();
                conn.execute(
                    "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes,
                                               encrypted_bytes, sha256_plain, sha256_encrypted)
                     VALUES (?1, 0, ?2, ?2, 'p', 'e')",
                    params![stage_set_id, bytes],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                     VALUES (?1, ?2, ?3, 'completed')",
                    params![stage_set_id, snap_id, vol_id],
                )
                .unwrap();
            }
        }

        fn setup() -> (Connection, i64) {
            let conn = crate::db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
                [],
            )
            .unwrap();
            let tid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                 VALUES ('u-96r', 'u96r', ?1, 'mtime_size', 1, 'active')",
                params![tid],
            )
            .unwrap();
            let unit_id = conn.last_insert_rowid();
            (conn, unit_id)
        }

        // --- Group A: compaction is sealed-only -------------------------

        #[test]
        fn compaction_rows_see_a_sealed_volume() {
            let (conn, unit) = setup();
            seed_written_volume(&conn, unit, "SEAL01", "sealed", 1000, 100, 900);

            let rows = compaction_candidate_rows(&conn).unwrap();
            assert_eq!(
                rows.len(),
                1,
                "a sealed volume must be evaluated, got {rows:?}"
            );
            assert_eq!(rows[0].0, "SEAL01");
            assert_eq!((rows[0].1, rows[0].2, rows[0].3), (1000, 100, 900));
        }

        #[test]
        fn compaction_rows_exclude_a_retired_volume() {
            let (conn, unit) = setup();
            seed_written_volume(&conn, unit, "RET01", "retired", 1000, 100, 900);

            assert!(compaction_candidate_rows(&conn).unwrap().is_empty());
        }

        // --- Group B: inventory counts sealed AND legacy full -----------

        #[test]
        fn summary_counts_a_sealed_volume() {
            let (conn, unit) = setup();
            seed_written_volume(&conn, unit, "SEAL02", "sealed", 1000, 100, 900);

            assert_eq!(in_service_volume_count(&conn).unwrap(), 1);
        }

        /// The regression an `eligible`-everywhere fix would introduce:
        /// legacy `full` is sealed-equivalent for pre-renovation volumes and
        /// its physical media still exists, so dropping it would silently
        /// under-report inventory.
        #[test]
        fn summary_still_counts_a_legacy_full_volume() {
            let (conn, unit) = setup();
            seed_written_volume(&conn, unit, "FULL01", "full", 1000, 100, 900);

            assert_eq!(in_service_volume_count(&conn).unwrap(), 1);
        }

        #[test]
        fn summary_excludes_retired_and_erased_volumes() {
            let (conn, unit) = setup();
            seed_written_volume(&conn, unit, "RET02", "retired", 1000, 100, 0);
            seed_written_volume(&conn, unit, "ERA01", "erased", 1000, 100, 0);

            assert_eq!(in_service_volume_count(&conn).unwrap(), 0);
        }

        /// `report summary` prints the count and the byte total on one line,
        /// so they must describe the same population (issue #96). An
        /// unfiltered sum would report the retired volume's bytes beside a
        /// count of 1, which reads as "one volume holding 3000 bytes".
        #[test]
        fn summary_bytes_and_count_describe_the_same_volumes() {
            let (conn, unit) = setup();
            seed_written_volume(&conn, unit, "SEAL05", "sealed", 1000, 100, 900);
            seed_written_volume(&conn, unit, "RET03", "retired", 2000, 100, 0);

            assert_eq!(in_service_volume_count(&conn).unwrap(), 1);
            assert_eq!(
                in_service_bytes_written(&conn).unwrap(),
                1000,
                "the retired volume's bytes must not be summed into an in-service total"
            );
        }

        #[test]
        fn capacity_totals_include_sealed_and_legacy_full() {
            let (conn, unit) = setup();
            seed_written_volume(&conn, unit, "SEAL03", "sealed", 400, 400, 0);
            seed_written_volume(&conn, unit, "FULL02", "full", 600, 600, 0);
            seed_written_volume(&conn, unit, "RET03", "retired", 9999, 100, 0);

            let (cap, written, count) = capacity_totals(&conn).unwrap();
            assert_eq!(count, 2, "sealed + full count; retired does not");
            assert_eq!(cap, 2000);
            assert_eq!(written, 1000);
        }

        /// Site 745 keeps `initialized` — a provisioned-but-unwritten tape is
        /// deliberately visible in the per-volume listing.
        #[test]
        fn per_volume_rows_list_sealed_full_and_initialized_but_not_retired() {
            let (conn, unit) = setup();
            seed_written_volume(&conn, unit, "SEAL04", "sealed", 400, 400, 0);
            seed_written_volume(&conn, unit, "FULL03", "full", 600, 600, 0);
            seed_written_volume(&conn, unit, "INIT01", "initialized", 0, 0, 0);
            seed_written_volume(&conn, unit, "RET04", "retired", 100, 100, 0);

            let labels: Vec<String> = per_volume_capacity_rows(&conn)
                .unwrap()
                .into_iter()
                .map(|(l, _, _, _)| l)
                .collect();
            assert_eq!(labels, vec!["FULL03", "INIT01", "SEAL04"]);
        }
    }
}
