use clap::Subcommand;
use rusqlite::{params, Connection};

use crate::config::Config;
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
        ReportCommands::Dirty { unit } => report_dirty(conn, unit.as_deref(), json_output),
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
    }
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
    let volume_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM volumes WHERE status IN ('active','full')",
        [],
        |r| r.get(0),
    )?;
    let write_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM writes WHERE status = 'completed'",
        [],
        |r| r.get(0),
    )?;
    let total_bytes: i64 = conn.query_row(
        "SELECT COALESCE(SUM(bytes_written),0) FROM volumes",
        [],
        |r| r.get(0),
    )?;
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

    // Units with fewer copies than min_copies
    let mut stmt = conn.prepare(
        "SELECT u.name, u.status,
                COUNT(DISTINCT w.volume_id) as copies,
                COUNT(DISTINCT v.location_id) as locations
         FROM units u
         LEFT JOIN snapshots s ON s.unit_id = u.id AND s.status = 'current'
         LEFT JOIN stage_sets ss ON ss.snapshot_id = s.id
         LEFT JOIN writes w ON w.stage_set_id = ss.id AND w.status = 'completed'
         LEFT JOIN volumes v ON v.id = w.volume_id
         WHERE u.status = 'active'
         GROUP BY u.id
         HAVING copies < ?1 OR copies = 0",
    )?;
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

fn report_copies(conn: &Connection, unit_filter: Option<&str>, json_output: bool) -> Result<()> {
    let mut sql = String::from(
        "SELECT u.name,
                COUNT(DISTINCT w.volume_id) as copies,
                COUNT(DISTINCT v.location_id) as locations,
                GROUP_CONCAT(DISTINCT v.label) as volumes
         FROM units u
         LEFT JOIN snapshots s ON s.unit_id = u.id AND s.status = 'current'
         LEFT JOIN stage_sets ss ON ss.snapshot_id = s.id
         LEFT JOIN writes w ON w.stage_set_id = ss.id AND w.status = 'completed'
         LEFT JOIN volumes v ON v.id = w.volume_id
         WHERE u.status IN ('active', 'tape_only')",
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
    let rows: Vec<(String, i64, i64, Option<String>)> = stmt
        .query_map(params_ref.as_slice(), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

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
    let mut sql = String::from(
        "SELECT u.name,
                COUNT(DISTINCT w.volume_id) as copies,
                COUNT(DISTINCT v.location_id) as locations
         FROM units u
         LEFT JOIN snapshots s ON s.unit_id = u.id AND s.status = 'current'
         LEFT JOIN stage_sets ss ON ss.snapshot_id = s.id
         LEFT JOIN writes w ON w.stage_set_id = ss.id AND w.status = 'completed'
         LEFT JOIN volumes v ON v.id = w.volume_id
         WHERE u.status = 'tape_only'",
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

fn report_dirty(conn: &Connection, _unit_filter: Option<&str>, json_output: bool) -> Result<()> {
    // Dirty = units whose last_scanned < most recent file modification on disk
    // Since we can't scan disk in a report, show units with no current snapshot
    // or whose latest snapshot is older than a configurable threshold
    let mut stmt = conn.prepare(
        "SELECT u.name, u.current_path, MAX(s.created_at) as last_snap
         FROM units u
         LEFT JOIN snapshots s ON s.unit_id = u.id
         WHERE u.status = 'active'
         GROUP BY u.id
         ORDER BY last_snap ASC NULLS FIRST",
    )?;
    let rows: Vec<(String, Option<String>, Option<String>)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    if json_output {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|(name, path, snap)| {
                serde_json::json!({"unit": name, "path": path, "last_snapshot": snap})
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else {
        println!("unit snapshot age (oldest first):");
        for (name, _path, snap) in &rows {
            println!(
                "  {name}: last snapshot {}",
                snap.as_deref().unwrap_or("never")
            );
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
    let rows: Vec<(String, i64, String, Option<i64>, Option<i64>)> = stmt
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
    let rows: Vec<(
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    )> = stmt
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
    let rows: Vec<(
        String,
        Option<String>,
        String,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    )> = stmt
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

fn report_capacity(conn: &Connection, per_volume: bool, json_output: bool) -> Result<()> {
    if per_volume {
        let mut stmt = conn.prepare(
            "SELECT label, capacity_bytes, bytes_written, status FROM volumes
             WHERE status IN ('active','full','initialized')
             ORDER BY label",
        )?;
        let rows: Vec<(String, i64, i64, String)> = stmt
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

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
        let total_cap: i64 = conn.query_row(
            "SELECT COALESCE(SUM(capacity_bytes),0) FROM volumes WHERE status IN ('active','full')",
            [],
            |r| r.get(0),
        )?;
        let total_written: i64 = conn.query_row(
            "SELECT COALESCE(SUM(bytes_written),0) FROM volumes WHERE status IN ('active','full')",
            [],
            |r| r.get(0),
        )?;
        let vol_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM volumes WHERE status IN ('active','full')",
            [],
            |r| r.get(0),
        )?;

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
    let rows: Vec<(
        String,
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = stmt
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

fn report_compaction_candidates(
    conn: &Connection,
    config: &Config,
    json_output: bool,
) -> Result<()> {
    let threshold = config.compaction.utilization_threshold;

    let mut stmt = conn.prepare(
        "SELECT v.label, v.bytes_written,
                SUM(CASE WHEN s.status NOT IN ('reclaimable','purged') THEN ss.encrypted_bytes ELSE 0 END) as live_bytes,
                SUM(CASE WHEN s.status IN ('reclaimable','purged') THEN ss.encrypted_bytes ELSE 0 END) as reclaimable_bytes
         FROM volumes v
         JOIN writes w ON w.volume_id = v.id AND w.status = 'completed'
         JOIN stage_sets sts ON sts.id = w.stage_set_id
         JOIN snapshots s ON s.id = sts.snapshot_id
         JOIN stage_slices ss ON ss.stage_set_id = sts.id
         WHERE v.status IN ('active','full')
         GROUP BY v.id
         ORDER BY live_bytes * 1.0 / NULLIF(v.bytes_written, 0) ASC",
    )?;
    let rows: Vec<(String, i64, i64, i64)> = stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

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
