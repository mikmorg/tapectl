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
    let (violations, warnings) = collect_findings(conn, config, unit_filter)?;

    // Output
    let exit_code = if !violations.is_empty() {
        2
    } else if !warnings.is_empty() {
        1
    } else {
        0
    };

    print!(
        "{}",
        render(&violations, &warnings, exit_code, action_plan, json_output)
    );

    Ok(exit_code)
}

/// Render `audit`'s complete stdout as a string, so the output contract is
/// assertable without capturing stdout.
///
/// In `--json` mode the returned string is a *single* parseable JSON object
/// and nothing else — §2.20 specifies JSON "for scripting", so any extra
/// human-readable line makes the stream unparseable for every consumer that
/// pipes it. That is not hypothetical: extracting `collect_findings` out of
/// `run` (issue #56) hoisted the trailing summary line out of the non-JSON
/// branch and silently broke exactly this. `json_output_is_a_single_parseable_object`
/// pins it.
fn render(
    violations: &[AuditFinding],
    warnings: &[AuditFinding],
    exit_code: i32,
    action_plan: bool,
    json_output: bool,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    if json_output {
        let findings: Vec<serde_json::Value> = violations
            .iter()
            .map(|f| finding_json(f, "violation"))
            .chain(warnings.iter().map(|f| finding_json(f, "warning")))
            .collect();
        let _ = writeln!(
            out,
            "{}",
            serde_json::json!({
                "exit_code": exit_code,
                "violations": violations.len(),
                "warnings": warnings.len(),
                "findings": findings,
            })
        );
        return out;
    }

    if violations.is_empty() && warnings.is_empty() {
        let _ = writeln!(out, "audit: clean");
    } else {
        if !violations.is_empty() {
            let _ = writeln!(out, "VIOLATIONS ({}):", violations.len());
            for f in violations {
                let _ = writeln!(out, "  [{}] {}: {}", f.check, f.unit, f.message);
                if action_plan {
                    let _ = writeln!(out, "    fix: {}", f.action);
                }
            }
        }
        if !warnings.is_empty() {
            let _ = writeln!(out, "WARNINGS ({}):", warnings.len());
            for f in warnings {
                let _ = writeln!(out, "  [{}] {}: {}", f.check, f.unit, f.message);
                if action_plan {
                    let _ = writeln!(out, "    fix: {}", f.action);
                }
            }
        }
    }
    let _ = writeln!(
        out,
        "audit: {} violations, {} warnings (exit {})",
        violations.len(),
        warnings.len(),
        exit_code,
    );
    out
}

/// Collect every audit finding for `unit_filter` (or all active units),
/// split into (violations, warnings). Extracted from [`run`] so tests can
/// assert on the actual finding set — `check` names, counts, which unit —
/// rather than only the collapsed exit code, which two different finding
/// combinations can produce identically.
fn collect_findings(
    conn: &Connection,
    config: &Config,
    unit_filter: Option<&str>,
) -> Result<(Vec<AuditFinding>, Vec<AuditFinding>)> {
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

    // Dirty scan (design §2.20): computed once for all units (or the one
    // filtered unit) and indexed per-unit below, reusing the exact same
    // `fingerprint::classify`-backed scan `report dirty` uses rather than a
    // second implementation (issue #56 / the #33/#36/#48/#49/#89/#96
    // discipline: one predicate, one place).
    let dirty_rows =
        crate::cli::report::dirty_rows(conn, unit_filter, &config.defaults.global_excludes)?;

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

        // Check dirty status (design §2.20). MUST NOT fire for
        // `PendingReason::New` — a never-archived unit is already reported
        // by `no_archive` above, and firing both would double-report the
        // same condition (`dirty_rows`'s "new" state, distinct from
        // "dirty", exists for exactly this reason).
        if let Some(row) = dirty_rows.iter().find(|r| r.name == unit.name) {
            if row.state == "dirty" {
                warnings.push(AuditFinding {
                    unit: unit.name.clone(),
                    check: "dirty".into(),
                    message: format!(
                        "source has drifted since last archive ({} added, {} removed, {} modified)",
                        row.added.len(),
                        row.removed.len(),
                        row.modified.len(),
                    ),
                    action: format!(
                        "tapectl snapshot create {} && tapectl stage create {} && tapectl volume write <LABEL>",
                        unit.name, unit.name
                    ),
                });
            }
        }

        // Check encryption compliance (design §2.20). Fires when policy
        // requires encryption but at least one `stage_sets` row written to
        // an in-service volume for the unit's current snapshot is
        // unencrypted. The join mirrors `copy_count_for_unit` exactly
        // (writes -> stage_sets -> snapshots, `s.status = 'current'`,
        // `w.status = 'completed'`), scoped the same way every other
        // per-unit check in this file is.
        //
        // Known and accepted gap: a plaintext stage_set written under a
        // now-superseded (non-`current`) snapshot is still plaintext on a
        // tape but will not be reported here, because this check scopes to
        // the current snapshot like its neighbours. Widening that is
        // deliberately out of scope for this task.
        //
        // Uses `policy::coverage::in_service`, not `eligible`: this is an
        // inventory question ("is unencrypted data sitting on media we
        // still account for?"), not a durability/copy-count claim, so the
        // wider in-service set (active/full/sealed) is correct here — see
        // `in_service`'s doc comment. Routing through the shared predicate
        // rather than inlining a status list is the discipline issue #96
        // established.
        if resolved.encrypt {
            let unencrypted_count: i64 = {
                let sql = format!(
                    "SELECT COUNT(*)
                     FROM writes w
                     JOIN stage_sets ss ON ss.id = w.stage_set_id
                     JOIN snapshots s ON s.id = ss.snapshot_id
                     JOIN volumes v ON v.id = w.volume_id
                     WHERE s.unit_id = ?1 AND s.status = 'current' AND w.status = 'completed'
                       AND ss.encrypted = 0 AND {}",
                    policy::coverage::in_service("v")
                );
                conn.query_row(&sql, params![unit.id], |row| row.get(0))?
            };

            if unencrypted_count > 0 {
                violations.push(AuditFinding {
                    unit: unit.name.clone(),
                    check: "encryption".into(),
                    message: format!(
                        "{unencrypted_count} unencrypted stage set(s) on tape, policy requires encryption"
                    ),
                    action: format!(
                        "tapectl stage create {} && tapectl volume write <LABEL>",
                        unit.name
                    ),
                });
            }
        }
    }

    // Check compaction candidates (volume-level, not per-unit)
    if unit_filter.is_none() {
        warnings.extend(compaction_findings(
            conn,
            config.compaction.utilization_threshold,
        )?);
    }

    Ok((violations, warnings))
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
        /// warning list, so the audit cannot report clean while sealed
        /// volumes sit at 10% utilization.
        ///
        /// The fixture is built so that a compaction warning is the ONLY
        /// finding `run` can produce, which is what makes `exit_code == 1`
        /// proof of propagation rather than a coincidence:
        ///   - TWO sealed volumes, so `copy_count` (2) meets the default
        ///     `min_copies` (2) and raises no violation. With one volume
        ///     this test passed off the copy_count violation alone and
        ///     would have survived deleting the `warnings.extend(...)`
        ///     call from `run` entirely.
        ///   - `required_locations` is empty and `verify_interval_days` is
        ///     `None` by default (`policy::resolve`), so neither
        ///     `location_presence` nor `verify_age` can fire.
        ///   - each volume has a `current` snapshot, so `no_archive` is out.
        #[test]
        fn run_surfaces_the_compaction_warning_for_a_sealed_volume() {
            let (conn, unit_id) = setup();
            seed_written_volume(&conn, unit_id, "SEAL03", "sealed", 1000, 100, 900);
            seed_written_volume(&conn, unit_id, "SEAL04", "sealed", 1000, 100, 900);

            let config = Config::default();
            assert_eq!(
                copy_count_for_unit(&conn, unit_id).unwrap(),
                2,
                "fixture guard: copy_count must clear min_copies so no violation masks the result"
            );

            let exit_code = run(&conn, &config, None, false, false).unwrap();
            assert_eq!(
                exit_code, 1,
                "exactly one warning class is reachable here — the compaction check — \
                 so exit 1 means the finding really reached the warning list, and \
                 exit 0 means audit passed silently on two under-utilized sealed tapes"
            );
        }
    }

    /// Issue #56 / design §2.20: the two previously-missing checks —
    /// encryption compliance (violation) and dirty status (warning).
    mod encryption_and_dirty_checks {
        use super::*;
        use tempfile::TempDir;

        /// tenant + one active unit (no `current_path`, matching
        /// `setup_unit_with_two_volumes`) + one `current` snapshot + one
        /// `stage_sets` row with the given `encrypted` flag, written
        /// (`writes.status = 'completed'`) to one volume of the given
        /// status. Returns `(conn, unit_id)`.
        fn setup_unit_with_stage_set(
            name: &str,
            encrypted: i64,
            volume_status: &str,
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
                "INSERT INTO stage_sets (snapshot_id, status, slice_size, encrypted)
                 VALUES (?1, 'staged', 524288, ?2)",
                params![snap_id, encrypted],
            )
            .unwrap();
            let stage_set_id = conn.last_insert_rowid();

            conn.execute(
                &format!(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
                     VALUES ('{name}-VOL', 'lto', 'lto0', 'LTO-6', 2500000000000, '{volume_status}')"
                ),
                [],
            )
            .unwrap();
            let vol_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, 'completed')",
                params![stage_set_id, snap_id, vol_id],
            )
            .unwrap();

            (conn, unit_id)
        }

        /// A `Config` with `min_copies_for_tape_only` zeroed out, so the
        /// `copy_count` check (min_copies default 2) never fires in these
        /// fixtures, which intentionally have 0 or 1 eligible (`sealed`)
        /// copies. Without this, `copy_count`'s violation/warning noise
        /// would make an exit-code-only assertion pass for the wrong
        /// reason — exactly the trap the doc comments below call out.
        fn config_without_min_copies() -> Config {
            let mut config = Config::default();
            config.defaults.min_copies_for_tape_only = 0;
            config
        }

        /// A `current`-snapshot stage_set with `encrypted = 0` on a
        /// `sealed` (in-service) volume, with policy demanding encryption
        /// (the `Config` default), must be a VIOLATION — plaintext really
        /// is sitting on tape. Asserted on the specific `check` name, not
        /// just the exit code: confirmed this fails without the change by
        /// running it against the pre-change `run` (no `encryption` check
        /// at all) — `violations` was empty and `exit_code` was 0, since
        /// `config_without_min_copies` also neutralizes `copy_count`.
        #[test]
        fn encryption_check_fires_for_unencrypted_stage_set_on_in_service_volume() {
            let (conn, _unit_id) = setup_unit_with_stage_set("audit-plain", 0, "sealed");
            let config = config_without_min_copies();
            let (violations, _warnings) =
                collect_findings(&conn, &config, Some("audit-plain")).unwrap();
            assert_eq!(violations.len(), 1);
            assert_eq!(violations[0].check, "encryption");
            assert_eq!(violations[0].unit, "audit-plain");
        }

        /// Same fixture, but `encrypted = 1`: must NOT fire.
        #[test]
        fn encryption_check_does_not_fire_when_encrypted() {
            let (conn, unit_id) = setup_unit_with_stage_set("audit-enc", 1, "sealed");
            let count: i64 = {
                let sql = format!(
                    "SELECT COUNT(*)
                     FROM writes w
                     JOIN stage_sets ss ON ss.id = w.stage_set_id
                     JOIN snapshots s ON s.id = ss.snapshot_id
                     JOIN volumes v ON v.id = w.volume_id
                     WHERE s.unit_id = ?1 AND s.status = 'current' AND w.status = 'completed'
                       AND ss.encrypted = 0 AND {}",
                    policy::coverage::in_service("v")
                );
                conn.query_row(&sql, params![unit_id], |row| row.get(0))
                    .unwrap()
            };
            assert_eq!(count, 0, "an encrypted stage set must not count");
        }

        /// Same unencrypted stage_set, but the volume is `retired` — not
        /// `in_service` — so the check must NOT fire, proving the
        /// coverage predicate is actually applied rather than the check
        /// firing on any unencrypted stage_set regardless of where it
        /// lives.
        #[test]
        fn encryption_check_ignores_a_not_in_service_volume() {
            let (conn, _unit_id) = setup_unit_with_stage_set("audit-retired", 0, "retired");
            let config = config_without_min_copies();
            let (violations, _warnings) =
                collect_findings(&conn, &config, Some("audit-retired")).unwrap();
            assert!(
                violations.iter().all(|f| f.check != "encryption"),
                "an unencrypted stage set on a retired (not in-service) volume is not a live finding"
            );
        }

        /// A unit whose on-disk directory drifted from its recorded
        /// fingerprint since its last snapshot must produce a `dirty`
        /// WARNING (exit 1), mirroring `report.rs`'s dirty tests
        /// (`report::tests::setup_two_units`).
        #[test]
        fn dirty_check_fires_for_a_unit_that_drifted_since_its_snapshot() {
            let root = TempDir::new().unwrap();
            let conn = crate::db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
                [],
            )
            .unwrap();
            let tid = conn.last_insert_rowid();

            let dir = root.path().join("dirty_unit");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("f.txt"), b"hello").unwrap();

            conn.execute(
                "INSERT INTO units (uuid, name, tenant_id, current_path, checksum_mode, encrypt, status)
                 VALUES ('u-dirty', 'dirty_unit', ?1, ?2, 'mtime_size', 1, 'active')",
                params![tid, dir.to_string_lossy().to_string()],
            )
            .unwrap();

            crate::staging::snapshot_create(&conn, "dirty_unit", &Config::default()).unwrap();
            // Mark the snapshot 'current' (snapshot_create alone leaves it
            // 'created') so `no_archive` doesn't also fire here — this test
            // isolates the `dirty` check specifically.
            conn.execute(
                "UPDATE snapshots SET status = 'current' WHERE unit_id = \
                 (SELECT id FROM units WHERE name = 'dirty_unit')",
                [],
            )
            .unwrap();
            std::fs::write(dir.join("g.txt"), b"new file").unwrap();

            let config = config_without_min_copies();
            let (violations, warnings) =
                collect_findings(&conn, &config, Some("dirty_unit")).unwrap();
            assert!(violations.is_empty());
            assert_eq!(warnings.len(), 1, "exactly one warning (dirty) is expected");
            assert_eq!(warnings[0].check, "dirty");
            assert_eq!(warnings[0].unit, "dirty_unit");
        }

        /// A never-archived (`PendingReason::New`) unit must NOT trigger
        /// the dirty check — that would double-report the same condition
        /// `no_archive` already reports. Confirmed this fails without the
        /// `state == "dirty"` guard: `dirty_rows` classifies a brand new
        /// unit as `state == "new"`, and a naive `!= "clean"` check (which
        /// is what an implementation without the guard would plausibly
        /// write) would fire here, producing a SECOND (dirty) warning
        /// alongside `no_archive`, which the exact-count assertion below
        /// would catch.
        #[test]
        fn dirty_check_does_not_fire_for_a_never_archived_unit() {
            let root = TempDir::new().unwrap();
            let conn = crate::db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
                [],
            )
            .unwrap();
            let tid = conn.last_insert_rowid();

            let dir = root.path().join("new_unit");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("f.txt"), b"hello").unwrap();

            conn.execute(
                "INSERT INTO units (uuid, name, tenant_id, current_path, checksum_mode, encrypt, status)
                 VALUES ('u-new', 'new_unit', ?1, ?2, 'mtime_size', 1, 'active')",
                params![tid, dir.to_string_lossy().to_string()],
            )
            .unwrap();

            let config = config_without_min_copies();
            let (violations, warnings) =
                collect_findings(&conn, &config, Some("new_unit")).unwrap();
            assert!(violations.is_empty());
            assert_eq!(
                warnings.len(),
                1,
                "exactly one warning (no_archive) is expected; a second (dirty) warning \
                 here would mean the New-exclusion guard is missing or wrong — confirmed \
                 by temporarily replacing the `row.state == \"dirty\"` guard with \
                 `row.state != \"clean\"` while developing this test, which made this \
                 assertion fail with warnings.len() == 2"
            );
            assert_eq!(warnings[0].check, "no_archive");
        }
    }

    // ── output contract (issue #56) ──

    fn sample_findings() -> (Vec<AuditFinding>, Vec<AuditFinding>) {
        (
            vec![AuditFinding {
                unit: "photos".into(),
                check: "encryption".into(),
                message: "1 unencrypted stage set(s) on tape".into(),
                action: "tapectl stage create photos".into(),
            }],
            vec![AuditFinding {
                unit: "docs".into(),
                check: "dirty".into(),
                message: "source has drifted".into(),
                action: "tapectl snapshot create docs".into(),
            }],
        )
    }

    /// §2.20 specifies `--format json` "for scripting", so in JSON mode the
    /// ENTIRE stdout must parse as one object — a trailing human-readable
    /// summary line makes it unparseable for anything that pipes it.
    ///
    /// This is a real regression, not a hypothetical: extracting
    /// `collect_findings` out of `run` moved the summary `println!` out of
    /// the non-JSON branch, so `audit --json` emitted valid JSON followed by
    /// `audit: 1 violations, 1 warnings (exit 2)`. Every existing test
    /// asserted on findings or exit codes, so nothing caught it.
    #[test]
    fn json_output_is_a_single_parseable_object() {
        let (violations, warnings) = sample_findings();
        let out = render(&violations, &warnings, 2, true, true);

        let parsed: serde_json::Value = serde_json::from_str(&out)
            .expect("the whole of --json stdout must parse as one JSON object");
        assert_eq!(parsed["exit_code"], 2);
        assert_eq!(parsed["violations"], 1);
        assert_eq!(parsed["warnings"], 1);
        assert_eq!(parsed["findings"].as_array().unwrap().len(), 2);

        assert!(
            !out.contains("audit: 1 violations"),
            "the human-readable summary line must not appear in JSON mode, got: {out}"
        );
    }

    /// The summary line is part of the human-readable contract and must
    /// survive — the fix for the above must not delete it outright.
    #[test]
    fn human_output_keeps_the_summary_line_and_action_plan() {
        let (violations, warnings) = sample_findings();
        let out = render(&violations, &warnings, 2, true, false);
        assert!(out.contains("VIOLATIONS (1):"), "got: {out}");
        assert!(out.contains("[encryption] photos:"), "got: {out}");
        assert!(out.contains("WARNINGS (1):"), "got: {out}");
        assert!(
            out.contains("fix: tapectl stage create photos"),
            "got: {out}"
        );
        assert!(
            out.contains("audit: 1 violations, 1 warnings (exit 2)"),
            "got: {out}"
        );
        assert!(serde_json::from_str::<serde_json::Value>(&out).is_err());
    }

    #[test]
    fn human_output_says_clean_when_there_are_no_findings() {
        let out = render(&[], &[], 0, false, false);
        assert!(out.contains("audit: clean"), "got: {out}");
    }
}
