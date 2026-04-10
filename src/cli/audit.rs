use rusqlite::{params, Connection};

use crate::config::Config;
use crate::error::Result;
use crate::policy;

/// Run policy compliance audit. Returns exit code: 0=clean, 1=warnings, 2=violations.
pub fn run(
    conn: &Connection,
    config: &Config,
    unit_filter: Option<&str>,
    action_plan: bool,
    json_output: bool,
) -> Result<i32> {
    let mut warnings: Vec<AuditFinding> = Vec::new();
    let mut violations: Vec<AuditFinding> = Vec::new();

    // Get units to audit
    let units = if let Some(name) = unit_filter {
        let unit = crate::db::queries::get_unit_by_name(conn, name)?
            .ok_or_else(|| crate::error::TapectlError::UnitNotFound(name.to_string()))?;
        vec![unit]
    } else {
        crate::db::queries::list_units(conn, None, Some("active"))?
    };

    for unit in &units {
        let resolved = policy::resolve(conn, config, unit);

        // Check copy count
        let copy_count: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT w.volume_id)
             FROM writes w
             JOIN stage_sets ss ON ss.id = w.stage_set_id
             JOIN snapshots s ON s.id = ss.snapshot_id
             WHERE s.unit_id = ?1 AND s.status = 'current' AND w.status = 'completed'",
            params![unit.id],
            |row| row.get(0),
        )?;

        if copy_count < resolved.min_copies as i64 {
            violations.push(AuditFinding {
                unit: unit.name.clone(),
                check: "copy_count".into(),
                message: format!("has {copy_count} copies, needs {}", resolved.min_copies),
                action: format!(
                    "tapectl stage create {} && tapectl volume write <LABEL>",
                    unit.name
                ),
            });
        }

        // Check location presence
        let location_count: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT v.location_id)
             FROM writes w
             JOIN stage_sets ss ON ss.id = w.stage_set_id
             JOIN snapshots s ON s.id = ss.snapshot_id
             JOIN volumes v ON v.id = w.volume_id
             WHERE s.unit_id = ?1 AND s.status = 'current' AND w.status = 'completed'
               AND v.location_id IS NOT NULL",
            params![unit.id],
            |row| row.get(0),
        )?;

        if !resolved.required_locations.is_empty() {
            let needed = resolved.required_locations.len() as i64;
            if location_count < needed {
                violations.push(AuditFinding {
                    unit: unit.name.clone(),
                    check: "location_presence".into(),
                    message: format!(
                        "in {location_count} locations, needs {needed} ({:?})",
                        resolved.required_locations
                    ),
                    action: format!("tapectl volume write <LABEL> (at missing location)",),
                });
            }
        }

        // Check verification age
        if let Some(verify_days) = resolved.verify_interval_days {
            let last_verify: Option<String> = conn
                .query_row(
                    "SELECT MAX(vs.completed_at)
                     FROM verification_sessions vs
                     JOIN writes w ON w.volume_id = vs.volume_id
                     JOIN stage_sets ss ON ss.id = w.stage_set_id
                     JOIN snapshots s ON s.id = ss.snapshot_id
                     WHERE s.unit_id = ?1 AND vs.outcome = 'passed'",
                    params![unit.id],
                    |row| row.get(0),
                )
                .ok()
                .flatten();

            let overdue = if let Some(ref last) = last_verify {
                if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(last, "%Y-%m-%d %H:%M:%S") {
                    let age = chrono::Utc::now().naive_utc() - dt;
                    age.num_days() > verify_days as i64
                } else {
                    true
                }
            } else {
                copy_count > 0 // only warn if there are copies to verify
            };

            if overdue {
                warnings.push(AuditFinding {
                    unit: unit.name.clone(),
                    check: "verify_age".into(),
                    message: format!(
                        "not verified within {verify_days} days (last: {})",
                        last_verify.as_deref().unwrap_or("never")
                    ),
                    action: format!("tapectl volume verify <LABEL>"),
                });
            }
        }

        // Check if current snapshot exists
        let has_current: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM snapshots WHERE unit_id = ?1 AND status = 'current'",
                params![unit.id],
                |row| row.get::<_, i64>(0),
            )
            .map(|c| c > 0)?;

        if !has_current && copy_count == 0 {
            warnings.push(AuditFinding {
                unit: unit.name.clone(),
                check: "no_archive".into(),
                message: "no current snapshot or tape copies".into(),
                action: format!(
                    "tapectl snapshot create {} && tapectl stage create {} && tapectl volume write <LABEL>",
                    unit.name, unit.name
                ),
            });
        }
    }

    // Check compaction candidates (volume-level, not per-unit)
    if unit_filter.is_none() {
        let threshold = config.compaction.utilization_threshold;
        let mut stmt = conn.prepare(
            "SELECT v.label, v.bytes_written,
                    SUM(CASE WHEN s.status NOT IN ('reclaimable','purged') THEN ss.encrypted_bytes ELSE 0 END) as live_bytes
             FROM volumes v
             JOIN writes w ON w.volume_id = v.id AND w.status = 'completed'
             JOIN stage_sets sts ON sts.id = w.stage_set_id
             JOIN snapshots s ON s.id = sts.snapshot_id
             JOIN stage_slices ss ON ss.stage_set_id = sts.id
             WHERE v.status IN ('active','full')
             GROUP BY v.id",
        )?;
        let candidates: Vec<(String, i64, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        for (label, total, live) in &candidates {
            if *total > 0 {
                let utilization = *live as f64 / *total as f64;
                if utilization < threshold {
                    warnings.push(AuditFinding {
                        unit: format!("volume:{label}"),
                        check: "compaction_candidate".into(),
                        message: format!(
                            "utilization {:.0}% < {:.0}% threshold",
                            utilization * 100.0,
                            threshold * 100.0
                        ),
                        action: format!("tapectl volume compact-read {label}"),
                    });
                }
            }
        }
    }

    // Output
    let exit_code = if !violations.is_empty() {
        2
    } else if !warnings.is_empty() {
        1
    } else {
        0
    };

    if json_output {
        let findings: Vec<serde_json::Value> = violations
            .iter()
            .map(|f| finding_json(f, "violation"))
            .chain(warnings.iter().map(|f| finding_json(f, "warning")))
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "exit_code": exit_code,
                "violations": violations.len(),
                "warnings": warnings.len(),
                "findings": findings,
            })
        );
    } else {
        if violations.is_empty() && warnings.is_empty() {
            println!("audit: clean");
        } else {
            if !violations.is_empty() {
                println!("VIOLATIONS ({}):", violations.len());
                for f in &violations {
                    println!("  [{}] {}: {}", f.check, f.unit, f.message);
                    if action_plan {
                        println!("    fix: {}", f.action);
                    }
                }
            }
            if !warnings.is_empty() {
                println!("WARNINGS ({}):", warnings.len());
                for f in &warnings {
                    println!("  [{}] {}: {}", f.check, f.unit, f.message);
                    if action_plan {
                        println!("    fix: {}", f.action);
                    }
                }
            }
        }
        println!(
            "audit: {} violations, {} warnings (exit {})",
            violations.len(),
            warnings.len(),
            exit_code,
        );
    }

    Ok(exit_code)
}

struct AuditFinding {
    unit: String,
    check: String,
    message: String,
    action: String,
}

fn finding_json(f: &AuditFinding, severity: &str) -> serde_json::Value {
    serde_json::json!({
        "severity": severity,
        "unit": f.unit,
        "check": f.check,
        "message": f.message,
        "action": f.action,
    })
}
