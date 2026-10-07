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

/// The outcome of the precondition set for one snapshot.
///
/// Consent is deliberately not modelled: the report must describe the
/// ordinary path, or it would list cases that need `--force` (advance
/// ADR-0008 Tier-2 consent) as releasable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReclaimVerdict {
    /// Every precondition passes; `snapshot mark-reclaimable` will accept.
    Releasable {
        superseding_version: i64,
        freeable_bytes: i64,
    },
    /// A precondition fails. `reason` is the fact the gate puts to the
    /// operator at its consent prompt (issue #348: `snapshot
    /// mark-reclaimable` asks through `cli::consent::confirm`, which says
    /// how to confirm — the reason itself is only the shortfall).
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
/// consent.
///
/// The message text of every [`ReclaimVerdict::Blocked`] is the gate's
/// own fact, verbatim: `snapshot_mark_reclaimable` puts it to the operator
/// at the consent gate, and the report prints it as the blocker. One
/// string, one meaning, in both places.
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
                reason: format!("no superseding current snapshot exists for v{version}"),
            })
        }
    };
    superseding_verdict(conn, config, unit, superseding, freeable)
}

/// Preconditions 2 and on, for a release whose superseding snapshot is
/// `superseding` (`(id, version)`): whether that snapshot is covered as the
/// unit's policy requires. Nothing here depends on the version being
/// released — only on the unit and the snapshot that supersedes it — so
/// [`candidates`] asks once per unit, not once per superseded snapshot
/// (issue #417). `freeable` is carried into the verdict as given.
fn superseding_verdict(
    conn: &Connection,
    config: &Config,
    unit: &Unit,
    superseding: (i64, i64),
    freeable: i64,
) -> Result<ReclaimVerdict> {
    // Precondition 2: Superseding snapshot meets policy
    let resolved = super::resolve(conn, config, unit)?;
    let mut required_copies = resolved.min_copies;
    // The count floor the named locations imply: one distinct place per
    // distinct name (a name listed twice is one place). The names
    // themselves are checked below; the tape-only multiplier scales this
    // count, and the copies each name must hold (ADR-0012, 2026-10-07,
    // item 15).
    let mut required_locations = {
        let mut distinct: Vec<&String> = resolved.required_locations.iter().collect();
        distinct.sort();
        distinct.dedup();
        distinct.len() as i64
    };

    // Precondition 3: tape-only units get multiplied requirements.
    //
    // `tape_only_multiplier` is carried past this block (rather than
    // recomputed at each `if unit.status == "tape_only"` site below) so the
    // refusal text below can name the CONFIGURED multiplier. Before issue
    // #215 finding 1, the refusal hardcoded the literal "2x" regardless of
    // `config.compaction.tape_only_safety_multiplier`'s actual value, so an
    // operator who had (legitimately) set a different multiplier read a
    // rule that was not the one actually enforced. `Config::load` now
    // refuses a multiplier below 1 (`Config::range_problems`), so this is
    // always `>= 1` here.
    let tape_only_multiplier = if unit.status == "tape_only" {
        let multiplier = config.compaction.tape_only_safety_multiplier as i64;
        required_copies *= multiplier;
        required_locations *= multiplier;
        Some(multiplier)
    } else {
        None
    };

    // ADR-0004 (issue #89): this query previously had no JOIN to
    // volumes at all, so it counted every completed write regardless
    // of whether the volume holding it had since been quarantined,
    // retired, erased, or reported missing. The shared eligibility
    // predicate re-qualifies at use time instead of trusting
    // write-time status forever.
    let scoped = super::coverage::CoverageQuery {
        scope: super::coverage::CoverageScope::Snapshot { id_expr: "?1" },
        exclude_volume: None,
    };
    let sql = format!("SELECT {}", super::coverage::copy_count_expr(&scoped));
    let copy_count: i64 = conn.query_row(&sql, params![superseding.0], |row| row.get(0))?;

    if copy_count < required_copies {
        return Ok(ReclaimVerdict::Blocked {
            superseding_version: Some(superseding.1),
            freeable_bytes: freeable,
            reason: format!(
                "superseding v{} has {copy_count} copies, needs {required_copies}{}",
                superseding.1,
                match tape_only_multiplier {
                    Some(m) => format!(" (tape-only {m}x)"),
                    None => String::new(),
                }
            ),
        });
    }

    // Issue #348: `required_locations` is a list of NAMES, and each must
    // hold a copy of the superseding version — checked through the one
    // named-location predicate `audit` and `unit mark-tape-only` share, in
    // its one-version form (the version being released must not count).
    // Checked by count alone, copies at `home` and `garage` met
    // `["home","offsite"]` and the older version was released with nothing
    // offsite.
    let missing = super::coverage::missing_required_locations_for_snapshot(
        conn,
        superseding.0,
        &resolved.required_locations,
    )?;
    if !missing.is_empty() {
        return Ok(ReclaimVerdict::Blocked {
            superseding_version: Some(superseding.1),
            freeable_bytes: freeable,
            reason: format!(
                "superseding v{} has no copy at required location(s) {} (policy requires {})",
                superseding.1,
                missing.join(", "),
                resolved.required_locations.join(", "),
            ),
        });
    }

    // ADR-0012, 2026-10-07 amendment, item 15: for a tape-only unit each
    // named location must hold `multiplier` copies of the superseding
    // version, as the distinct-location count below is multiplied. One copy
    // at `offsite` meets `["offsite"]` for an active unit, not at 2x.
    if let Some(m) = tape_only_multiplier {
        let short = super::coverage::short_required_locations_for_snapshot(
            conn,
            superseding.0,
            &resolved.required_locations,
            m,
        )?;
        if !short.is_empty() {
            let held = short
                .iter()
                .map(|(name, copies)| {
                    let noun = if *copies == 1 { "copy" } else { "copies" };
                    format!("{copies} {noun} at required location {name}")
                })
                .collect::<Vec<_>>()
                .join(", ");
            let each = if short.len() == 1 { "" } else { " at each" };
            return Ok(ReclaimVerdict::Blocked {
                superseding_version: Some(superseding.1),
                freeable_bytes: freeable,
                reason: format!(
                    "superseding v{} has {held}, needs {m}{each} (tape-only {m}x)",
                    superseding.1,
                ),
            });
        }
    }

    // With every name met, the distinct-location count is at least the
    // number of names, so this binds only for a tape-only unit, whose
    // floor the multiplier raises above it.
    let sql = format!("SELECT {}", super::coverage::location_count_expr(&scoped));
    let location_count: i64 = conn.query_row(&sql, params![superseding.0], |row| row.get(0))?;

    if required_locations > 0 && location_count < required_locations {
        return Ok(ReclaimVerdict::Blocked {
            superseding_version: Some(superseding.1),
            freeable_bytes: freeable,
            reason: format!(
                "superseding v{} in {location_count} locations, needs {required_locations}{}",
                superseding.1,
                match tape_only_multiplier {
                    Some(m) => format!(" (tape-only {m}x)"),
                    None => String::new(),
                }
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
    // Issue #417: one statement for every candidate — its unit, its own
    // freeable bytes ([`freeable_bytes`]'s figure) and the snapshot that
    // supersedes it, which for every candidate of a unit is that unit's
    // highest `current` snapshot (precondition 1's query, answered for all
    // of them at once). Then the policy half of [`assess`] once per unit:
    // it depends on the unit and that snapshot, never on the version being
    // released. Before, every candidate cost a unit lookup and all of
    // `assess` (a dozen statements and a dotfile read).
    let sql = format!(
        "SELECT u.id, u.uuid, u.name, u.tenant_id, u.archive_set_id, u.current_path,
                u.checksum_mode, u.encrypt, u.status, u.created_at, u.last_scanned, u.notes,
                s.version, top.id, top.version, {}
         FROM units u
         JOIN snapshots s ON s.unit_id = u.id AND s.status = 'current'
         JOIN snapshots top ON top.unit_id = u.id AND top.status = 'current'
              AND top.version = (SELECT MAX(s2.version) FROM snapshots s2
                                 WHERE s2.unit_id = u.id AND s2.status = 'current')
         WHERE s.version < top.version
         ORDER BY u.name, s.version",
        freeable_bytes_expr("s.id")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<(Unit, i64, (i64, i64), i64)> = stmt
        .query_map([], |row| {
            Ok((
                Unit {
                    id: row.get(0)?,
                    uuid: row.get(1)?,
                    name: row.get(2)?,
                    tenant_id: row.get(3)?,
                    archive_set_id: row.get(4)?,
                    current_path: row.get(5)?,
                    checksum_mode: row.get(6)?,
                    encrypt: row.get(7)?,
                    status: row.get(8)?,
                    created_at: row.get(9)?,
                    last_scanned: row.get(10)?,
                    notes: row.get(11)?,
                },
                row.get(12)?,
                (row.get(13)?, row.get(14)?),
                row.get(15)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut out = Vec::with_capacity(rows.len());
    // The unit whose verdict was last asked, and that verdict. Rows come
    // ordered by unit name, so each unit's candidates are adjacent.
    let mut asked: Option<(i64, ReclaimVerdict)> = None;
    for (unit, version, superseding, freeable) in rows {
        let verdict = match &asked {
            Some((id, v)) if *id == unit.id => v.clone(),
            _ => {
                let v = superseding_verdict(conn, config, &unit, superseding, 0)?;
                asked = Some((unit.id, v.clone()));
                v
            }
        };
        let verdict = match verdict {
            ReclaimVerdict::Releasable {
                superseding_version,
                ..
            } => ReclaimVerdict::Releasable {
                superseding_version,
                freeable_bytes: freeable,
            },
            ReclaimVerdict::Blocked {
                superseding_version,
                reason,
                ..
            } => ReclaimVerdict::Blocked {
                superseding_version,
                freeable_bytes: freeable,
                reason,
            },
        };
        out.push(Candidate {
            unit_name: unit.name,
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
    let sql = format!("SELECT {}", freeable_bytes_expr("?1"));
    let bytes: i64 = conn.query_row(&sql, params![snapshot_id], |row| row.get(0))?;
    Ok(bytes)
}

/// [`freeable_bytes`] as a scalar subquery over the snapshot whose id is
/// `snapshot_id_expr` — one expression, so [`candidates`]' single statement
/// and [`assess`]'s per-snapshot lookup cannot drift apart.
fn freeable_bytes_expr(snapshot_id_expr: &str) -> String {
    format!(
        "(SELECT COALESCE(SUM(sl.encrypted_bytes), 0)
          FROM stage_sets ss
          JOIN stage_slices sl ON sl.stage_set_id = ss.id
          WHERE ss.snapshot_id = {snapshot_id_expr}
            AND EXISTS (SELECT 1 FROM writes w
                        JOIN volumes v ON v.id = w.volume_id
                        WHERE w.stage_set_id = ss.id AND w.status = 'completed'
                          AND {}))",
        super::coverage::eligible("v")
    )
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
        // Issue #242: 'quarantined' is a condition now, not a status --
        // translate it onto `observed_condition`, leaving `status` at
        // 'sealed' (the value a real write-path quarantine would leave a
        // volume that had already sealed, exactly the shape this fixture
        // exists to test).
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

    /// Issue #417: `report supersedable` asked the whole of `assess` — a
    /// unit lookup and a dozen statements — for every superseded snapshot.
    /// The statements `candidates` prepares are counted through SQLite's
    /// authorizer (one `SELECT` action per SELECT prepared) for a unit with
    /// 3 versions and one with 9: they must not grow with the snapshots.
    /// And every candidate's verdict must still be exactly `assess`'s, the
    /// gate's own (both a releasable unit and a blocked one).
    #[test]
    fn candidates_ask_per_unit_not_per_snapshot_and_agree_with_assess() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let count = |conn: &Connection| -> (usize, Vec<Candidate>) {
            let n = Arc::new(AtomicUsize::new(0));
            let counter = n.clone();
            conn.authorizer(Some(move |ctx: rusqlite::hooks::AuthContext<'_>| {
                if matches!(ctx.action, rusqlite::hooks::AuthAction::Select) {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
                rusqlite::hooks::Authorization::Allow
            }))
            .unwrap();
            let found = candidates(conn, &Config::default()).unwrap();
            conn.authorizer(
                None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>,
            )
            .unwrap();
            (n.load(Ordering::SeqCst), found)
        };

        for second in ["sealed", "quarantined"] {
            let (few_conn, few_unit) = setup("sup-few", 3, second, "active");
            let (many_conn, many_unit) = setup("sup-many", 9, second, "active");
            let (few, few_found) = count(&few_conn);
            let (many, many_found) = count(&many_conn);
            assert_eq!(few_found.len(), 2);
            assert_eq!(many_found.len(), 8);
            assert!(few > 0, "positive control: the authorizer counts SELECTs");
            assert_eq!(
                few, many,
                "{second}: candidates must prepare the same statements for 2 superseded \
                 snapshots as for 8"
            );
            for (conn, unit, found) in [
                (&few_conn, &few_unit, &few_found),
                (&many_conn, &many_unit, &many_found),
            ] {
                for c in found {
                    assert_eq!(
                        c.verdict,
                        assess(conn, &Config::default(), unit, c.version).unwrap(),
                        "{second}: v{} must be judged exactly as the gate judges it",
                        c.version
                    );
                }
            }
        }
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
                assert!(
                    !reason.contains("--force"),
                    "the reason is the fact; the consent gate says how to confirm: {reason}"
                );
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

    /// Issue #215 finding 1: the "(tape-only Nx)" fragment in the refusal
    /// text must name the CONFIGURED multiplier, not a hardcoded literal
    /// "2x" — an operator who set `tape_only_safety_multiplier = 3` must
    /// read "3x" in the message that names the multiplied requirement, not
    /// a "2x" that was never actually applied.
    #[test]
    fn d_tape_only_message_names_the_configured_multiplier_not_a_hardcoded_2x() {
        let (conn, unit) = setup("sup-tapeonly3", 2, "sealed", "tape_only");
        let mut config = Config::default();
        config.compaction.tape_only_safety_multiplier = 3;
        let verdict = assess(&conn, &config, &unit, 1).unwrap();
        match verdict {
            ReclaimVerdict::Blocked { reason, .. } => {
                assert!(
                    reason.contains("has 2 copies, needs 6"),
                    "expected the requirement multiplied by 3 (needs 6), got: {reason}"
                );
                assert!(
                    reason.contains("(tape-only 3x)"),
                    "must name the configured multiplier (3x): {reason}"
                );
                assert!(
                    !reason.contains("(tape-only 2x)"),
                    "must not name a hardcoded 2x once the multiplier is configured \
                     differently: {reason}"
                );
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
                reason: "no superseding current snapshot exists for v2".to_string(),
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

    /// Issue #73 / ADR-0006: the reclaim gate's copy precondition must
    /// count a recorded warehouse deposit. Negative control: the
    /// superseding snapshot's second volume is retired, so the tape half
    /// alone is ONE copy and the gate blocks; recording a deposit of the
    /// surviving sealed volume makes it two and the gate releases.
    #[test]
    fn a_warehouse_deposit_satisfies_the_copy_precondition() {
        let (conn, unit) = setup("sup-deposit-copies", 2, "retired", "active");
        conn.execute(
            "INSERT INTO locations (name, kind) VALUES ('glacier', 'warehouse')",
            [],
        )
        .unwrap();
        let loc = conn.last_insert_rowid();
        let vol: i64 = conn
            .query_row(
                "SELECT id FROM volumes WHERE label = 'sup-deposit-copies-SEALED'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO volume_deposits (volume_id, location_id) VALUES (?1, ?2)",
            params![vol, loc],
        )
        .unwrap();

        let verdict = assess(&conn, &Config::default(), &unit, 1).unwrap();
        assert_eq!(
            verdict,
            ReclaimVerdict::Releasable {
                superseding_version: 2,
                freeable_bytes: 1000,
            },
            "a recorded warehouse deposit is the second copy"
        );
    }

    /// Issue #73 / ADR-0006: the reclaim gate's LOCATION precondition
    /// must count the warehouse as a distinct location. Both tape
    /// volumes sit at `home`, so the tape half alone is one location.
    #[test]
    fn a_warehouse_deposit_satisfies_the_location_precondition() {
        let (conn, _unit) = setup("sup-deposit-locs", 2, "sealed", "active");
        conn.execute(
            "INSERT INTO locations (name, kind) VALUES ('home', 'shelf')",
            [],
        )
        .unwrap();
        let home = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO locations (name, kind) VALUES ('glacier', 'warehouse')",
            [],
        )
        .unwrap();
        let glacier = conn.last_insert_rowid();
        conn.execute(
            "UPDATE volumes SET location_id = ?1 WHERE label LIKE 'sup-deposit-locs-%'",
            params![home],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO archive_sets (name, required_locations)
             VALUES ('two-places', '[\"home\",\"glacier\"]')",
            [],
        )
        .unwrap();
        let as_id = conn.last_insert_rowid();
        conn.execute(
            "UPDATE units SET archive_set_id = ?1 WHERE name = 'sup-deposit-locs'",
            params![as_id],
        )
        .unwrap();
        let unit = crate::db::queries::get_unit_by_name(&conn, "sup-deposit-locs")
            .unwrap()
            .unwrap();

        // Negative control: without the deposit this blocks at 1 location.
        let vol: i64 = conn
            .query_row(
                "SELECT id FROM volumes WHERE label = 'sup-deposit-locs-SEALED'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO volume_deposits (volume_id, location_id) VALUES (?1, ?2)",
            params![vol, glacier],
        )
        .unwrap();

        let verdict = assess(&conn, &Config::default(), &unit, 1).unwrap();
        assert_eq!(
            verdict,
            ReclaimVerdict::Releasable {
                superseding_version: 2,
                freeable_bytes: 1000,
            },
            "glacier is a second distinct location"
        );
    }

    /// Shelve `{name}-SEALED` at `sealed_at` and `{name}-OTHER` at
    /// `other_at`, and bind the unit to an archive set requiring
    /// `required` (a JSON array). Returns the re-read unit.
    fn place_and_require(
        conn: &Connection,
        name: &str,
        sealed_at: &str,
        other_at: &str,
        required: &str,
    ) -> Unit {
        for (suffix, loc) in [("SEALED", sealed_at), ("OTHER", other_at)] {
            conn.execute(
                "INSERT OR IGNORE INTO locations (name, kind) VALUES (?1, 'shelf')",
                params![loc],
            )
            .unwrap();
            conn.execute(
                "UPDATE volumes SET location_id = (SELECT id FROM locations WHERE name = ?1)
                 WHERE label = ?2",
                params![loc, format!("{name}-{suffix}")],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO archive_sets (name, required_locations) VALUES ('named', ?1)",
            params![required],
        )
        .unwrap();
        conn.execute(
            "UPDATE units SET archive_set_id = (SELECT id FROM archive_sets WHERE name = 'named')
             WHERE name = ?1",
            params![name],
        )
        .unwrap();
        crate::db::queries::get_unit_by_name(conn, name)
            .unwrap()
            .unwrap()
    }

    /// Issue #348's defect class on the delete path: `required_locations`
    /// was checked by COUNT only, so the superseding version's copies at
    /// `home` and `garage` — two places — satisfied `["home","offsite"]`,
    /// and the older version was released with no copy offsite at all.
    /// Now each NAME must hold a copy of the superseding version, through
    /// the predicate `audit` and `unit mark-tape-only` use.
    #[test]
    fn required_locations_are_checked_by_name_not_by_count() {
        let (conn, _unit) = setup("sup-named", 2, "sealed", "active");
        let unit = place_and_require(
            &conn,
            "sup-named",
            "home",
            "garage",
            r#"["home","offsite"]"#,
        );
        match assess(&conn, &Config::default(), &unit, 1).unwrap() {
            ReclaimVerdict::Blocked {
                superseding_version,
                reason,
                ..
            } => {
                assert_eq!(superseding_version, Some(2));
                assert!(
                    reason.contains(
                        "superseding v2 has no copy at required location(s) offsite \
                         (policy requires home, offsite)"
                    ),
                    "{reason}"
                );
            }
            other => panic!("home + garage must not satisfy [home, offsite]: {other:?}"),
        }
    }

    /// ADR-0012, 2026-10-07 amendment, item 15: for a tape-only unit the
    /// multiplier applies to each NAMED required location too, as it does to
    /// the distinct-location count. `["offsite"]` at 2x asks for two copies of
    /// the superseding version at `offsite`. Here it has one there and one at
    /// `home`: two copies (min_copies 1, x2), two places (one name, x2), and
    /// `offsite` present — every check but this one is met.
    #[test]
    fn tape_only_multiplies_the_copies_each_named_location_must_hold() {
        let (conn, _unit) = setup("sup-named-tape", 2, "sealed", "tape_only");
        let unit = place_and_require(&conn, "sup-named-tape", "offsite", "home", r#"["offsite"]"#);
        let mut config = Config::default();
        config.defaults.min_copies = 1;
        match assess(&conn, &config, &unit, 1).unwrap() {
            ReclaimVerdict::Blocked {
                superseding_version,
                reason,
                ..
            } => {
                assert_eq!(superseding_version, Some(2));
                assert!(
                    reason.contains(
                        "superseding v2 has 1 copy at required location offsite, needs 2 \
                         (tape-only 2x)"
                    ),
                    "{reason}"
                );
            }
            other => panic!("one copy at offsite must not meet [offsite] at 2x: {other:?}"),
        }

        // The configured multiplier, not a literal 2 (#215's lesson).
        config.compaction.tape_only_safety_multiplier = 3;
        config.defaults.min_copies = 0;
        match assess(&conn, &config, &unit, 1).unwrap() {
            ReclaimVerdict::Blocked { reason, .. } => assert!(
                reason.contains("has 1 copy at required location offsite, needs 3 (tape-only 3x)"),
                "{reason}"
            ),
            other => panic!("expected Blocked at 3x: {other:?}"),
        }
    }

    /// The positive half of item 15: two copies of the superseding version at
    /// `offsite` and a third at `home` meet `["offsite"]` at 2x — the per-name
    /// count is a count of copies there, tape and warehouse alike.
    #[test]
    fn tape_only_named_location_is_met_by_multiplied_copies_there() {
        let (conn, _unit) = setup("sup-named-tape-ok", 2, "sealed", "tape_only");
        let unit = place_and_require(
            &conn,
            "sup-named-tape-ok",
            "offsite",
            "offsite",
            r#"["offsite"]"#,
        );
        // A warehouse deposit of one of the offsite tapes, at `home`: the
        // second distinct place the 2x count needs, and not a copy at
        // `offsite`.
        conn.execute(
            "INSERT INTO locations (name, kind) VALUES ('home', 'shelf')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO volume_deposits (volume_id, location_id)
             SELECT v.id, l.id FROM volumes v, locations l
             WHERE v.label = 'sup-named-tape-ok-SEALED' AND l.name = 'home'",
            [],
        )
        .unwrap();
        let mut config = Config::default();
        config.defaults.min_copies = 1;
        assert_eq!(
            assess(&conn, &config, &unit, 1).unwrap(),
            ReclaimVerdict::Releasable {
                superseding_version: 2,
                freeable_bytes: 1000,
            }
        );

        // The same shape with the deposit AT offsite instead of home is two
        // tapes and a deposit there: three copies at offsite, but one place.
        // The distinct-location count still binds (needs 2).
        conn.execute(
            "UPDATE volume_deposits SET location_id = (SELECT id FROM locations WHERE name = 'offsite')",
            [],
        )
        .unwrap();
        match assess(&conn, &config, &unit, 1).unwrap() {
            ReclaimVerdict::Blocked { reason, .. } => assert!(
                reason.contains("in 1 locations, needs 2 (tape-only 2x)"),
                "{reason}"
            ),
            other => panic!("one place must not meet the 2x location count: {other:?}"),
        }
    }

    /// The question is asked of the SUPERSEDING version alone: v1 (the one
    /// being released) sits only at `home`, and v2 at `home` and `offsite`.
    /// The unit-wide form intersects over every current version, v1
    /// included, and would report `offsite` missing — holding back exactly
    /// the release the policy allows.
    #[test]
    fn the_version_being_released_does_not_hold_its_own_release_back() {
        let (conn, _unit) = setup("sup-named-ok", 2, "sealed", "active");
        let unit = place_and_require(
            &conn,
            "sup-named-ok",
            "home",
            "offsite",
            r#"["home","offsite"]"#,
        );
        assert_eq!(
            assess(&conn, &Config::default(), &unit, 1).unwrap(),
            ReclaimVerdict::Releasable {
                superseding_version: 2,
                freeable_bytes: 1000,
            }
        );
    }
}
