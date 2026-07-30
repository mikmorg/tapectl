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

        // Check copy count. Routes through the same ADR-0004 eligibility
        // predicate (issue #89) the gates use, so `audit` and `unit
        // mark-tape-only`/`snapshot mark-reclaimable` can never disagree
        // about how many copies a unit has.
        let copy_count = copy_count_for_unit(conn, unit.id)?;

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
        let location_count = location_count_for_unit(conn, unit.id)?;

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
                    action: "tapectl volume write <LABEL> (at missing location)".to_string(),
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
                    age.num_days() > verify_days
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
                    action: "tapectl volume verify <LABEL>".to_string(),
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
        warnings.extend(compaction_findings(
            conn,
            config.compaction.utilization_threshold,
        )?);
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

/// A unit's current copy count for `audit`'s `copy_count` check.
///
/// `pub(crate)` (not private) so `cli::operations`' tests can call this
/// directly against the same fixture a gate (`unit mark-tape-only`,
/// `snapshot mark-reclaimable`) is tested with, proving `audit` and the
/// gates agree on the number rather than merely asserting each in
/// isolation — that equality is the property issue #89 exists to
/// establish.
///
/// Routes through the shared ADR-0004 predicate
/// (`policy::coverage::eligible`): a write's own `status = 'completed'`
/// only proves its volume was sealed at write time, not that it still is.
/// The audit's volume-level compaction check: every volume whose live-byte
/// utilization has fallen below `threshold` yields one `compaction_candidate`
/// warning.
///
/// Group-A (issue #96): compaction targets a *finished* volume, so the
/// candidate set is the ADR-0004 `sealed` predicate. Before that fix this
/// query filtered `status IN ('active','full')` — a set no v2 write has ever
/// left behind (`SealedPending::confirm` writes `sealed`) — so the loop
/// iterated nothing and the audit passed *silently*, which is
/// indistinguishable from a clean result. That is the failure ADR-0001
/// exists to prevent.
///
/// The caller keeps the `unit_filter.is_none()` gate: this is a volume-level
/// check and must not start firing for `audit --unit X`.
fn compaction_findings(conn: &Connection, threshold: f64) -> Result<Vec<AuditFinding>> {
    let sql = format!(
        "SELECT v.label, v.bytes_written,
                SUM(CASE WHEN s.status NOT IN ('reclaimable','purged') THEN ss.encrypted_bytes ELSE 0 END) as live_bytes
         FROM volumes v
         JOIN writes w ON w.volume_id = v.id AND w.status = 'completed'
         JOIN stage_sets sts ON sts.id = w.stage_set_id
         JOIN snapshots s ON s.id = sts.snapshot_id
         JOIN stage_slices ss ON ss.stage_set_id = sts.id
         WHERE {}
         GROUP BY v.id",
        crate::policy::coverage::eligible("v")
    );
    let mut stmt = conn.prepare(&sql)?;
    let candidates: Vec<(String, i64, i64)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut findings = Vec::new();
    for (label, total, live) in &candidates {
        if *total > 0 {
            let utilization = *live as f64 / *total as f64;
            if utilization < threshold {
                findings.push(AuditFinding {
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
    Ok(findings)
}

pub(crate) fn copy_count_for_unit(conn: &Connection, unit_id: i64) -> Result<i64> {
    let sql = format!(
        "SELECT COUNT(DISTINCT w.volume_id)
         FROM writes w
         JOIN stage_sets ss ON ss.id = w.stage_set_id
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN volumes v ON v.id = w.volume_id
         WHERE s.unit_id = ?1 AND s.status = 'current' AND w.status = 'completed' AND {}",
        crate::policy::coverage::eligible("v")
    );
    Ok(conn.query_row(&sql, params![unit_id], |row| row.get(0))?)
}

/// A unit's current distinct-location count for `audit`'s
/// `location_presence` check. Same ADR-0004 routing as
/// [`copy_count_for_unit`] and for the same reason.
pub(crate) fn location_count_for_unit(conn: &Connection, unit_id: i64) -> Result<i64> {
    let sql = format!(
        "SELECT COUNT(DISTINCT v.location_id)
         FROM writes w
         JOIN stage_sets ss ON ss.id = w.stage_set_id
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN volumes v ON v.id = w.volume_id
         WHERE s.unit_id = ?1 AND s.status = 'current' AND w.status = 'completed'
           AND v.location_id IS NOT NULL AND {}",
        crate::policy::coverage::eligible("v")
    );
    Ok(conn.query_row(&sql, params![unit_id], |row| row.get(0))?)
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

#[cfg(test)]
mod tests {
    //! Issue #89 / ADR-0004: `copy_count_for_unit`/`location_count_for_unit`
    //! must re-qualify eligibility at USE time via the shared
    //! `policy::coverage::eligible` predicate, not trust `writes.status =
    //! 'completed'` alone -- see the doc comments on those two functions
    //! above for why. `audit` had no test module at all before this
    //! change; these tests cover both the extracted per-unit helpers and
    //! one full `run()` pass proving the violation actually surfaces.
    use super::*;

    /// tenant + unit (no `current_path` — `audit::run` never touches disk
    /// for this check) + one 'current' snapshot + one 'staged' stage_set
    /// completed-written to two volumes: `{name}-SEALED` (always
    /// `sealed`) and `{name}-OTHER` (status = `second_volume_status`).
    /// Returns (conn, unit_id). Mirrors
    /// `operations::tests::adr0004_copy_eligibility::setup_unit_with_two_volumes`
    /// — duplicated rather than shared across files, consistent with this
    /// crate's existing per-file test-fixture convention.
    fn setup_unit_with_two_volumes(name: &str, second_volume_status: &str) -> (Connection, i64) {
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

    fn assert_copy_count_excludes_status(status: &str) {
        let (conn, unit_id) = setup_unit_with_two_volumes(&format!("audit-{status}"), status);
        let count = copy_count_for_unit(&conn, unit_id).unwrap();
        assert_eq!(count, 1, "a {status} second volume must not count");
    }

    #[test]
    fn copy_count_excludes_a_quarantined_volume() {
        assert_copy_count_excludes_status("quarantined");
    }

    #[test]
    fn copy_count_excludes_a_retired_volume() {
        assert_copy_count_excludes_status("retired");
    }

    #[test]
    fn copy_count_excludes_an_erased_volume() {
        assert_copy_count_excludes_status("erased");
    }

    #[test]
    fn copy_count_excludes_a_missing_volume() {
        assert_copy_count_excludes_status("missing");
    }

    #[test]
    fn copy_count_counts_two_sealed_volumes_as_two() {
        let (conn, unit_id) = setup_unit_with_two_volumes("audit-both-sealed", "sealed");
        let count = copy_count_for_unit(&conn, unit_id).unwrap();
        assert_eq!(count, 2, "two sealed volumes must both count");
    }

    #[test]
    fn location_count_ignores_a_quarantined_volume_even_with_a_location_set() {
        let (conn, unit_id) = setup_unit_with_two_volumes("audit-loc-quar", "quarantined");
        conn.execute(
            "INSERT INTO locations (name) VALUES ('home'), ('offsite')",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE volumes SET location_id = (SELECT id FROM locations WHERE name = 'home')
             WHERE label = 'audit-loc-quar-SEALED'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE volumes SET location_id = (SELECT id FROM locations WHERE name = 'offsite')
             WHERE label = 'audit-loc-quar-OTHER'",
            [],
        )
        .unwrap();

        let count = location_count_for_unit(&conn, unit_id).unwrap();
        assert_eq!(
            count, 1,
            "a quarantined volume's location must not count, even though location_id is set"
        );
    }

    /// End-to-end: a unit whose only eligible copy sits on a quarantined
    /// volume must surface as a real `copy_count` VIOLATION through the
    /// full `audit::run` pipeline, not just through the extracted helper
    /// in isolation — this is the silent-failure scenario issue #89
    /// describes (quarantine is entered automatically at contact, with no
    /// operator decision, so `audit` is the one surface that can catch it
    /// after the fact).
    #[test]
    fn run_reports_a_violation_when_the_only_other_copy_is_quarantined() {
        let (conn, _unit_id) = setup_unit_with_two_volumes("audit-e2e", "quarantined");
        let config = Config::default();
        let exit_code = run(&conn, &config, Some("audit-e2e"), false, false).unwrap();
        assert_eq!(
            exit_code, 2,
            "a unit with only 1 eligible copy (min_copies=2 default) must be a violation, not clean"
        );
    }

    /// Issue #96: the volume-status drift. `SealedPending::confirm` writes
    /// `volumes.status = 'sealed'`, but this check filtered `IN
    /// ('active','full')` — a set no v2 write ever leaves behind — so the
    /// candidate loop iterated nothing and the audit passed *silently*.
    /// A silently-clean audit is indistinguishable from a real clean
    /// result, which is exactly the failure ADR-0001 exists to prevent.
    mod issue96_volume_status_drift {
        use super::*;

        /// A conn with one tenant + one active unit; returns `(conn, unit_id)`.
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
                 VALUES ('u-96', 'u96', ?1, 'mtime_size', 1, 'active')",
                params![tid],
            )
            .unwrap();
            let unit_id = conn.last_insert_rowid();
            (conn, unit_id)
        }

        /// One volume of `status` holding `bytes_written` of media, of which
        /// `live` bytes belong to a `current` snapshot and `reclaimable`
        /// bytes to a `reclaimable` one. Every write is `completed`, so the
        /// only thing standing between this volume and the compaction query
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
                 VALUES (?1, 'lto', 'lto0', 'LTO-6', 1000000, ?2, ?3)",
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

        /// The severity case: a `sealed` volume 10% utilized must produce a
        /// real `compaction_candidate` finding. Asserted on the finding
        /// itself, not on an exit code — an exit code of 1 could come from
        /// any other warning.
        #[test]
        fn compaction_check_fires_for_a_sealed_volume_under_threshold() {
            let (conn, unit_id) = setup();
            seed_written_volume(&conn, unit_id, "SEAL01", "sealed", 1000, 100, 900);

            let findings = compaction_findings(&conn, 0.50).unwrap();
            assert_eq!(
                findings.len(),
                1,
                "a sealed volume at 10% utilization must be flagged"
            );
            assert_eq!(findings[0].unit, "volume:SEAL01");
            assert_eq!(findings[0].check, "compaction_candidate");
        }

        /// A sealed volume above the threshold must NOT be flagged — proves
        /// the test above is measuring utilization, not merely presence.
        #[test]
        fn compaction_check_ignores_a_sealed_volume_above_threshold() {
            let (conn, unit_id) = setup();
            seed_written_volume(&conn, unit_id, "SEAL02", "sealed", 1000, 900, 100);

            assert!(compaction_findings(&conn, 0.50).unwrap().is_empty());
        }

        /// A retired volume is not a compaction target.
        #[test]
        fn compaction_check_ignores_a_retired_volume() {
            let (conn, unit_id) = setup();
            seed_written_volume(&conn, unit_id, "RET01", "retired", 1000, 100, 900);

            assert!(compaction_findings(&conn, 0.50).unwrap().is_empty());
        }

        /// End-to-end through `run`: the finding must actually reach the
        /// warning list, so the audit cannot report clean while a sealed
        /// volume sits at 10% utilization.
        #[test]
        fn run_surfaces_the_compaction_warning_for_a_sealed_volume() {
            let (conn, unit_id) = setup();
            seed_written_volume(&conn, unit_id, "SEAL03", "sealed", 1000, 100, 900);

            let exit_code = run(&conn, &Config::default(), None, false, false).unwrap();
            assert_ne!(exit_code, 0, "audit must not report clean");
            assert_eq!(
                compaction_findings(&conn, Config::default().compaction.utilization_threshold)
                    .unwrap()
                    .len(),
                1,
            );
        }
    }
}
