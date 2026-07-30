//! The `snapshot mark-reclaimable` precondition set, extracted so that
//! the destructive gate and the advisory `report supersedable` surface
//! can never disagree about which snapshots are releasable.
//!
//! Issue #90 (as re-scoped): the arithmetic in `report
//! compaction-candidates` was never wrong — `reclaimable` is the sole,
//! manual demotion (CONTEXT.md **Current**), so every `current` snapshot
//! is genuinely live until an operator releases it. What is missing is
//! *discoverability*: superseded versions pile up silently because
//! nothing ever prompts the operator to release them.
//!
//! `report supersedable` is that prompt. For it to be safe it must list
//! exactly what `snapshot mark-reclaimable` would accept — a report that
//! advertises a release the gate then refuses is the same
//! two-paths-disagree failure class this codebase has already hit in
//! #33, #36, #48, #49 and #89. So the preconditions live here, once, and
//! both callers go through [`assess`].

use rusqlite::{params, Connection};

use crate::config::Config;
use crate::db::models::Unit;
use crate::error::Result;

/// The outcome of the non-`--force` precondition set for one snapshot.
///
/// `--force` is deliberately not modelled: the report must describe the
/// ordinary path, or it would list force-only cases as releasable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReclaimVerdict {
    /// Every precondition passes; `snapshot mark-reclaimable` will accept.
    Releasable {
        superseding_version: i64,
        freeable_bytes: i64,
    },
    /// A precondition fails. `reason` is the exact error text the gate
    /// produces, "(use --force to override)" suffix included.
    ///
    /// `freeable_bytes` is carried here too, and is the same number the
    /// `Releasable` arm would report. A blocked candidate is the one an
    /// operator most needs the figure for: it is what clearing the
    /// blocker would buy back. Reporting a hard 0 on these rows would
    /// say the opposite.
    Blocked {
        superseding_version: Option<i64>,
        freeable_bytes: i64,
        reason: String,
    },
}

/// One supersedable snapshot: a `current` snapshot of `unit_name` that a
/// higher-versioned `current` snapshot of the same unit supersedes.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub unit_name: String,
    pub version: i64,
    pub verdict: ReclaimVerdict,
}

/// Assess whether `version` of `unit` may be marked reclaimable without
/// `--force`.
///
/// The message text of every [`ReclaimVerdict::Blocked`] is the gate's
/// own error text, verbatim: `snapshot_mark_reclaimable` returns it
/// unchanged, and the report prints it as the blocker. One string, one
/// meaning, in both places.
///
/// `policy::resolve` is called here rather than passed in — it reads the
/// unit's dotfile from disk, so a caller resolving separately could
/// diverge from the gate on exactly the axis this function exists to
/// pin down.
pub fn assess(
    conn: &Connection,
    config: &Config,
    unit: &Unit,
    version: i64,
) -> Result<ReclaimVerdict> {
    // The candidate's own bytes, resolved once for every return path.
    // `.ok()` rather than `?`: `assess` is public API, and a version
    // that does not exist is not an error condition it should invent --
    // it has nothing to free. (The gate rejects a missing version with
    // its own message before ever calling here, and `candidates` only
    // ever passes rows it just read.)
    let freeable = match conn
        .query_row(
            "SELECT id FROM snapshots WHERE unit_id = ?1 AND version = ?2",
            params![unit.id, version],
            |row| row.get::<_, i64>(0),
        )
        .ok()
    {
        Some(snap_id) => freeable_bytes(conn, snap_id)?,
        None => 0,
    };

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

    let superseding = match superseding {
        Some(s) => s,
        None => {
            return Ok(ReclaimVerdict::Blocked {
                superseding_version: None,
                freeable_bytes: freeable,
                reason: format!(
                "no superseding current snapshot exists for v{version} (use --force to override)"
            ),
            })
        }
    };

    // Precondition 2: Superseding snapshot meets policy
    let resolved = super::resolve(conn, config, unit);
    let mut required_copies = resolved.min_copies;
    let mut required_locations = resolved.required_locations.len() as i64;

    // Precondition 3: tape-only units get multiplied requirements
    if unit.status == "tape_only" {
        let multiplier = config.compaction.tape_only_safety_multiplier as i64;
        required_copies *= multiplier;
        required_locations *= multiplier;
    }

    // ADR-0004 (issue #89): this query previously had no JOIN to
    // volumes at all, so it counted every completed write regardless
    // of whether the volume holding it had since been quarantined,
    // retired, erased, or reported missing. The shared eligibility
    // predicate re-qualifies at use time instead of trusting
    // write-time status forever.
    let sql = format!(
        "SELECT COUNT(DISTINCT w.volume_id)
         FROM writes w
         JOIN stage_sets ss ON ss.id = w.stage_set_id
         JOIN volumes v ON v.id = w.volume_id
         WHERE ss.snapshot_id = ?1 AND w.status = 'completed' AND {}",
        super::coverage::eligible("v")
    );
    let copy_count: i64 = conn.query_row(&sql, params![superseding.0], |row| row.get(0))?;

    if copy_count < required_copies {
        return Ok(ReclaimVerdict::Blocked {
            superseding_version: Some(superseding.1),
            freeable_bytes: freeable,
            reason: format!(
                "superseding v{} has {copy_count} copies, needs {required_copies}{} (use --force to override)",
                superseding.1,
                if unit.status == "tape_only" { " (tape-only 2x)" } else { "" }
            ),
        });
    }

    let sql = format!(
        "SELECT COUNT(DISTINCT v.location_id)
         FROM writes w
         JOIN stage_sets ss ON ss.id = w.stage_set_id
         JOIN volumes v ON v.id = w.volume_id
         WHERE ss.snapshot_id = ?1 AND w.status = 'completed' AND v.location_id IS NOT NULL
           AND {}",
        super::coverage::eligible("v")
    );
    let location_count: i64 = conn.query_row(&sql, params![superseding.0], |row| row.get(0))?;

    if required_locations > 0 && location_count < required_locations {
        return Ok(ReclaimVerdict::Blocked {
            superseding_version: Some(superseding.1),
            freeable_bytes: freeable,
            reason: format!(
                "superseding v{} in {location_count} locations, needs {required_locations} (use --force to override)",
                superseding.1,
            ),
        });
    }

    Ok(ReclaimVerdict::Releasable {
        superseding_version: superseding.1,
        freeable_bytes: freeable,
    })
}

/// Enumerate every supersedable snapshot across all units: for each unit
/// with two or more `current` snapshots, the highest version is the
/// keeper and every lower-versioned `current` snapshot is a candidate.
///
/// Nothing here demotes anything — `reclaimable` stays the operator's
/// sole, manual demotion (CONTEXT.md **Current**). This only reports.
pub fn candidates(conn: &Connection, config: &Config) -> Result<Vec<Candidate>> {
    let mut stmt = conn.prepare(
        "SELECT u.name, s.version
         FROM units u
         JOIN snapshots s ON s.unit_id = u.id AND s.status = 'current'
         WHERE s.version < (SELECT MAX(s2.version) FROM snapshots s2
                            WHERE s2.unit_id = u.id AND s2.status = 'current')
         ORDER BY u.name, s.version",
    )?;
    let rows: Vec<(String, i64)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut out = Vec::with_capacity(rows.len());
    for (unit_name, version) in rows {
        // `queries::get_unit_by_name` rather than a by-id helper: the
        // latter does not exist, and `db/queries.rs` is out of scope.
        let Some(unit) = crate::db::queries::get_unit_by_name(conn, &unit_name)? else {
            continue;
        };
        let verdict = assess(conn, config, &unit, version)?;
        out.push(Candidate {
            unit_name,
            version,
            verdict,
        });
    }
    Ok(out)
}

/// Bytes that releasing `snapshot_id` would free: the sum of its
/// `stage_slices.encrypted_bytes`.
///
/// Deliberately NOT the `report compaction-candidates` join shape. That
/// one reaches `stage_slices` *through* `writes`, which is right when
/// grouping per volume but would multiply a per-snapshot figure by the
/// snapshot's copy count. Here `writes` appears only inside an EXISTS
/// guard, which cannot fan out. The guard itself is not optional: an
/// unwritten (staging/failed/cleaned) stage_set occupies no tape, so
/// counting it would advertise space that does not exist. The volume
/// behind that write is re-qualified with the shared ADR-0004 predicate
/// for the same reason the copy count is: a retired or erased volume's
/// space is not the operator's to reclaim here.
fn freeable_bytes(conn: &Connection, snapshot_id: i64) -> Result<i64> {
    let sql = format!(
        "SELECT COALESCE(SUM(sl.encrypted_bytes), 0)
         FROM stage_sets ss
         JOIN stage_slices sl ON sl.stage_set_id = ss.id
         WHERE ss.snapshot_id = ?1
           AND EXISTS (SELECT 1 FROM writes w
                       JOIN volumes v ON v.id = w.volume_id
                       WHERE w.stage_set_id = ss.id AND w.status = 'completed'
                         AND {})",
        super::coverage::eligible("v")
    );
    let bytes: i64 = conn.query_row(&sql, params![snapshot_id], |row| row.get(0))?;
    Ok(bytes)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// tenant + unit (`unit_status`) + `versions` snapshots, all
    /// 'current', each with one 'staged' stage_set carrying a single
    /// 1000-byte slice. The HIGHEST version is additionally written to
    /// two volumes: `{name}-SEALED` (always `sealed`) and `{name}-OTHER`
    /// (status = `second_volume_status`, the ADR-0004 dimension under
    /// test). Lower versions get one completed write each to
    /// `{name}-SEALED` so their bytes are on media and countable.
    pub(crate) fn setup(
        name: &str,
        versions: i64,
        second_volume_status: &str,
        unit_status: &str,
    ) -> (Connection, Unit) {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES (?1, ?2, ?3, 'mtime_size', 1, ?4)",
            params![format!("uuid-{name}"), name, tid, unit_status],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();

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
            &format!(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
                 VALUES ('{name}-OTHER', 'lto', 'lto0', 'LTO-6', 2500000000000, '{second_volume_status}')"
            ),
            [],
        )
        .unwrap();
        let vol2_id = conn.last_insert_rowid();

        for v in 1..=versions {
            conn.execute(
                "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
                 VALUES (?1, ?2, 'full', 'current', '/src')",
                params![unit_id, v],
            )
            .unwrap();
            let snap_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO stage_sets (snapshot_id, status, slice_size)
                 VALUES (?1, 'staged', 524288)",
                params![snap_id],
            )
            .unwrap();
            let ss_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO stage_slices
                    (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                     sha256_plain, sha256_encrypted)
                 VALUES (?1, 1, 900, 1000, 'p', 'e')",
                params![ss_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, 'completed')",
                params![ss_id, snap_id, vol1_id],
            )
            .unwrap();
            if v == versions {
                conn.execute(
                    "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                     VALUES (?1, ?2, ?3, 'completed')",
                    params![ss_id, snap_id, vol2_id],
                )
                .unwrap();
            }
        }

        let unit = crate::db::queries::get_unit_by_name(&conn, name)
            .unwrap()
            .unwrap();
        (conn, unit)
    }

    /// (a) One `current` snapshot: nothing is superseded, so nothing is
    /// a candidate. Age alone demotes nothing (CONTEXT.md **Current**).
    #[test]
    fn a_single_current_snapshot_yields_no_candidates() {
        let (conn, _unit) = setup("sup-single", 1, "sealed", "active");
        let found = candidates(&conn, &Config::default()).unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    /// (b) Two `current` snapshots, superseding one on two sealed
    /// volumes: releasable, and `freeable_bytes` is the candidate's own
    /// slice bytes (1000), not the unit's or the superseding one's.
    #[test]
    fn b_two_current_snapshots_with_enough_copies_are_releasable() {
        let (conn, unit) = setup("sup-ok", 2, "sealed", "active");
        let verdict = assess(&conn, &Config::default(), &unit, 1).unwrap();
        assert_eq!(
            verdict,
            ReclaimVerdict::Releasable {
                superseding_version: 2,
                freeable_bytes: 1000,
            }
        );

        let found = candidates(&conn, &Config::default()).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].unit_name, "sup-ok");
        assert_eq!(found[0].version, 1);
    }

    /// (c) The #89 / ADR-0004 interaction: the superseding snapshot's
    /// second volume is quarantined, so only ONE copy is eligible. This
    /// must go through `coverage::eligible`, not count raw completed
    /// writes — otherwise the report would promise a release the gate
    /// refuses.
    fn blocked_by_ineligible_second_volume(status: &str) {
        let name = format!("sup-{status}");
        let (conn, unit) = setup(&name, 2, status, "active");
        let verdict = assess(&conn, &Config::default(), &unit, 1).unwrap();
        match verdict {
            ReclaimVerdict::Blocked {
                superseding_version,
                freeable_bytes,
                reason,
            } => {
                assert_eq!(superseding_version, Some(2));
                assert_eq!(
                    freeable_bytes, 1000,
                    "a blocked candidate must still report what clearing the blocker would buy"
                );
                assert!(
                    reason.contains("superseding v2 has 1 copies, needs 2"),
                    "status {status}: {reason}"
                );
                assert!(reason.contains("(use --force to override)"), "{reason}");
            }
            other => panic!("status {status}: expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn c_quarantined_superseding_volume_blocks_on_copy_shortfall() {
        blocked_by_ineligible_second_volume("quarantined");
    }

    #[test]
    fn c_retired_superseding_volume_blocks_on_copy_shortfall() {
        blocked_by_ineligible_second_volume("retired");
    }

    /// (d) A `tape_only` unit multiplies the requirement, so the two
    /// sealed copies that satisfy an active unit no longer suffice.
    #[test]
    fn d_tape_only_units_double_the_copy_requirement() {
        let (conn, unit) = setup("sup-tapeonly", 2, "sealed", "tape_only");
        let config = Config::default();
        let verdict = assess(&conn, &config, &unit, 1).unwrap();
        match verdict {
            ReclaimVerdict::Blocked { reason, .. } => {
                let needed = 2 * config.compaction.tape_only_safety_multiplier as i64;
                assert!(
                    reason.contains(&format!("has 2 copies, needs {needed}")),
                    "{reason}"
                );
                assert!(reason.contains("(tape-only 2x)"), "{reason}");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    /// No superseding snapshot at all is Blocked with no version.
    #[test]
    fn the_highest_current_version_has_no_superseding_snapshot() {
        let (conn, unit) = setup("sup-top", 2, "sealed", "active");
        let verdict = assess(&conn, &Config::default(), &unit, 2).unwrap();
        assert_eq!(
            verdict,
            ReclaimVerdict::Blocked {
                superseding_version: None,
                freeable_bytes: 1000,
                reason: "no superseding current snapshot exists for v2 (use --force to override)"
                    .to_string(),
            }
        );
    }

    /// Freeing is per-snapshot even when a snapshot has been staged more
    /// than once (schema allows many stage_sets per snapshot): the sum
    /// must not fan out over `writes`, or two copies of one slice would
    /// read as twice the space.
    #[test]
    fn freeable_bytes_does_not_multiply_by_copy_count() {
        // The superseding snapshot (v2) has TWO completed writes of the
        // same stage_set; its freeable figure must still be 1000.
        let (conn, _unit) = setup("sup-fanout", 2, "sealed", "active");
        let snap2: i64 = conn
            .query_row("SELECT id FROM snapshots WHERE version = 2", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(freeable_bytes(&conn, snap2).unwrap(), 1000);
    }
}
