use clap::Subcommand;
use rusqlite::Connection;

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

/// Per-unit `(name, status, copies, locations, warehouse_deposits)` rows
/// behind `report fire-risk`, split out from the printing so the computed
/// counts are directly assertable in tests without capturing stdout — the
/// same pattern as [`copies_rows`] and `dirty_rows`.
///
/// `warehouse_deposits` is reported separately from `copies` on purpose
/// (issue #73): a deposit counts as a copy, but ADR-0006 names its
/// evidence a different class — never re-verified, and it "dies weeks
/// after payment stops". Folding it invisibly into one number would let an
/// operator read "2 copies" as two cartridges.
/// Why a unit appears in `report fire-risk`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FireRisk {
    /// No copies at all.
    NoCopies,
    /// Fewer copies than this unit's own resolved `min_copies`.
    BelowMinimum,
    /// The unit's policy could not be resolved, so its threshold is unknown
    /// (issue #106). It is LISTED rather than skipped: a unit whose policy
    /// will not resolve cannot be shown as safe, and silently dropping it
    /// reproduces the exact silent-pass class #59 removed from `audit`.
    PolicyUnresolvable(String),
}

/// One at-risk unit. A struct rather than the old 5-tuple because #106 added
/// the *resolved per-unit* threshold and a risk reason, and a 7-tuple of
/// same-typed integers is a bug waiting to happen at the call site.
#[derive(Debug, Clone)]
pub(crate) struct FireRiskRow {
    pub unit: String,
    pub status: String,
    pub copies: i64,
    pub locations: i64,
    pub deposits: i64,
    /// This unit's own resolved `min_copies`, not the global default
    /// (issue #106). `None` when the policy would not resolve.
    pub min_copies: Option<i64>,
    pub risk: FireRisk,
}

/// The parenthetical the human-readable copy/location surfaces append when
/// some of a unit's copies are warehouse deposits (issue #73). Empty when
/// there are none, so nothing changes for an all-tape fleet.
///
/// Every word has to be true read alone: a deposit IS a copy (it is inside
/// the count it qualifies) and it is NOT a cartridge on a shelf.
fn warehouse_note(deposits: i64) -> String {
    match deposits {
        0 => String::new(),
        1 => " (1 is a warehouse deposit, never re-verified)".to_string(),
        n => format!(" ({n} are warehouse deposits, never re-verified)"),
    }
}

pub(crate) fn fire_risk_rows(conn: &Connection, config: &Config) -> Result<Vec<FireRiskRow>> {
    // Units with fewer copies than min_copies. Copies and locations come
    // from the shared deposit-aware expressions (issue #73), correlated to
    // `u.id`: they re-qualify volume eligibility at use time per ADR-0004
    // (issue #89) AND count recorded warehouse deposits per ADR-0006. A
    // correlated subquery rather than the old `COUNT(DISTINCT CASE WHEN
    // ...)` aggregate because deposits are not reachable from the
    // writes/volumes join at all — and it keeps units with zero writes in
    // the result (they simply evaluate to 0) without needing the LEFT JOIN
    // chain the aggregate form required.
    //
    // The threshold filter has to sit in an OUTER query: with the joins
    // gone there is no GROUP BY, so a HAVING clause would be a
    // single-group aggregate filter, not a per-row one.
    // Issue #106: the threshold is now resolved PER UNIT, so it cannot be a
    // bound parameter in the SQL any more. The query returns every active
    // unit with its counts and the comparison happens in Rust, against the
    // same dotfile > archive_set > defaults chain `audit` uses.
    //
    // That matters because both commands answer one question — "is this unit
    // under-covered?" — and before this they could DISAGREE: `audit` resolved
    // per unit while fire-risk applied `defaults.min_copies_for_tape_only` to
    // everything. The one an operator glances at was the wrong one.
    //
    // The old `OR copies = 0` is gone with it. Once the threshold is a real
    // per-unit `min_copies`, zero is already below any positive minimum, so
    // it read like a guard while guarding nothing.
    let scope = crate::policy::coverage::CoverageQuery::current_unit("u.id");
    let sql = format!(
        "SELECT u.id, u.name, u.status,
                {} as copies,
                {} as locations,
                {} as deposits
         FROM units u
         WHERE u.status = 'active'",
        crate::policy::coverage::copy_count_expr(&scope),
        crate::policy::coverage::location_count_expr(&scope),
        crate::policy::coverage::deposit_count_expr(&scope),
    );
    let mut stmt = conn.prepare(&sql)?;
    let counts: std::collections::HashMap<i64, (String, String, i64, i64, i64)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                (
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ),
            ))
        })?
        .collect::<std::result::Result<_, _>>()?;

    // Two queries total, indexed by id — not one query per unit. `resolve`
    // needs a whole `Unit`, and `list_units` is the same call `audit` makes,
    // which keeps the two commands reading the same unit set as well as the
    // same threshold.
    let mut at_risk = Vec::new();
    for unit in crate::db::queries::list_units(conn, None, Some("active"))? {
        let Some((name, status, copies, locations, deposits)) = counts.get(&unit.id).cloned()
        else {
            continue;
        };
        let (min_copies, risk) = match crate::policy::resolve(conn, config, &unit) {
            Ok(p) => {
                if copies == 0 {
                    (Some(p.min_copies), FireRisk::NoCopies)
                } else if copies < p.min_copies {
                    (Some(p.min_copies), FireRisk::BelowMinimum)
                } else {
                    continue;
                }
            }
            Err(e) => (None, FireRisk::PolicyUnresolvable(e.to_string())),
        };
        at_risk.push(FireRiskRow {
            unit: name,
            status,
            copies,
            locations,
            deposits,
            min_copies,
            risk,
        });
    }
    Ok(at_risk)
}

fn report_fire_risk(conn: &Connection, config: &Config, json_output: bool) -> Result<()> {
    let at_risk = fire_risk_rows(conn, config)?;

    let risks: Vec<serde_json::Value> = at_risk
        .iter()
        .map(|r| {
            serde_json::json!({
                "unit": r.unit, "status": r.status, "copies": r.copies,
                "locations": r.locations, "warehouse_deposits": r.deposits,
                // The threshold this unit was actually judged against, so a
                // scripted consumer can tell a 2-copy unit failing a
                // min_copies=3 archive set from one failing the default.
                "min_copies": r.min_copies,
                "risk": match &r.risk {
                    FireRisk::NoCopies => "no_copies",
                    FireRisk::BelowMinimum => "below_minimum",
                    FireRisk::PolicyUnresolvable(_) => "policy_unresolvable",
                },
                "detail": match &r.risk {
                    FireRisk::PolicyUnresolvable(e) => Some(e.clone()),
                    _ => None,
                },
            })
        })
        .collect();

    if json_output {
        println!(
            "{}",
            serde_json::json!({"at_risk": risks.len(), "units": risks})
        );
    } else if at_risk.is_empty() {
        println!("fire-risk: all units meet their resolved minimum copy requirements");
    } else {
        println!("FIRE RISK: {} unit(s) at risk", at_risk.len());
        for r in &at_risk {
            match &r.risk {
                // Named separately from a copy shortfall because it is a
                // different problem with a different fix: nothing is known
                // to be wrong with this unit's coverage — the tool cannot
                // tell either way, which is strictly worse.
                FireRisk::PolicyUnresolvable(e) => println!(
                    "  {}: {} copies{}, {} locations — POLICY UNRESOLVABLE, \
                     coverage NOT checked ({e})",
                    r.unit,
                    r.copies,
                    warehouse_note(r.deposits),
                    r.locations,
                ),
                other => {
                    let severity = if *other == FireRisk::NoCopies {
                        "ZERO COPIES"
                    } else {
                        "below minimum"
                    };
                    println!(
                        "  {}: {} copies{}, {} locations — {severity} (needs {})",
                        r.unit,
                        r.copies,
                        warehouse_note(r.deposits),
                        r.locations,
                        r.min_copies
                            .map(|m| m.to_string())
                            .unwrap_or_else(|| "?".into()),
                    );
                }
            }
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
/// Copies and locations route through `policy::coverage`'s shared
/// deposit-aware expressions, so this report can never disagree with the
/// gates about either count (ADR-0004 eligibility + ADR-0006 deposits).
/// The trailing `i64` is how many of `copies` are warehouse deposits —
/// see [`FireRiskRow`] for why that is a separate number.
pub(crate) type CopyRow = (String, i64, i64, Option<String>, i64);

pub(crate) fn copies_rows(conn: &Connection, unit_filter: Option<&str>) -> Result<Vec<CopyRow>> {
    let sealed = crate::policy::coverage::eligible("v");
    let scope = crate::policy::coverage::CoverageQuery::current_unit("u.id");
    // Copies/locations/deposits come from the shared deposit-aware
    // expressions (issue #73). The LEFT JOIN chain survives only for
    // `volumes` — the GROUP_CONCAT of TAPE labels, which is a list of
    // cartridges to go fetch and deliberately does not list warehouse
    // deposits; the deposit count is reported as its own column instead.
    let mut sql = format!(
        "SELECT u.name,
                {} as copies,
                {} as locations,
                GROUP_CONCAT(DISTINCT CASE WHEN {sealed} THEN v.label END) as volumes,
                {} as deposits
         FROM units u",
        crate::policy::coverage::copy_count_expr(&scope),
        crate::policy::coverage::location_count_expr(&scope),
        crate::policy::coverage::deposit_count_expr(&scope),
    );
    sql.push_str(
        "
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
    let rows: Vec<CopyRow> = stmt
        .query_map(params_ref.as_slice(), |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn report_copies(conn: &Connection, unit_filter: Option<&str>, json_output: bool) -> Result<()> {
    let rows = copies_rows(conn, unit_filter)?;

    if json_output {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|(name, copies, locs, vols, deposits)| {
                serde_json::json!({"unit": name, "copies": copies, "locations": locs,
                                   "volumes": vols, "warehouse_deposits": deposits})
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else {
        for (name, copies, locs, vols, deposits) in &rows {
            println!(
                "  {name}: {copies} copies{}, {locs} locations [{}]",
                warehouse_note(*deposits),
                vols.as_deref().unwrap_or("-")
            );
        }
    }
    Ok(())
}

/// Per-unit `(name, copies, locations, warehouse_deposits)` rows behind
/// `report tape-only`, split out from the printing for the same
/// testability reason as [`copies_rows`] and [`fire_risk_rows`].
///
/// Tape-only is where the deposit distinction bites hardest: the source
/// directory is gone, so the copies listed here are all that exists. See
/// [`FireRiskRow`] for why the deposit count is reported separately.
pub(crate) type TapeOnlyRow = (String, i64, i64, i64);

pub(crate) fn tape_only_rows(
    conn: &Connection,
    unit_filter: Option<&str>,
) -> Result<Vec<TapeOnlyRow>> {
    // Same shared deposit-aware expressions as `report_fire_risk` and
    // `copies_rows` (issues #89 and #73): ADR-0004 eligibility re-checked
    // at use time, ADR-0006 warehouse deposits counted, one expression for
    // every surface so they cannot disagree.
    let scope = crate::policy::coverage::CoverageQuery::current_unit("u.id");
    let mut sql = format!(
        "SELECT u.name,
                {} as copies,
                {} as locations,
                {} as deposits
         FROM units u
         WHERE u.status = 'tape_only'",
        crate::policy::coverage::copy_count_expr(&scope),
        crate::policy::coverage::location_count_expr(&scope),
        crate::policy::coverage::deposit_count_expr(&scope),
    );
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(name) = unit_filter {
        sql.push_str(" AND u.name = ?");
        param_values.push(Box::new(name.to_string()));
    }
    sql.push_str(" ORDER BY u.name");

    let params_ref: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<TapeOnlyRow> = stmt
        .query_map(params_ref.as_slice(), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn report_tape_only(conn: &Connection, unit_filter: Option<&str>, json_output: bool) -> Result<()> {
    let rows = tape_only_rows(conn, unit_filter)?;

    if json_output {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|(name, copies, locs, deposits)| {
                serde_json::json!({"unit": name, "copies": copies,
                 "locations": locs, "warehouse_deposits": deposits})
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else if rows.is_empty() {
        println!("no tape-only units");
    } else {
        println!("tape-only units:");
        for (name, copies, locs, deposits) in &rows {
            println!(
                "  {name}: {copies} copies{}, {locs} locations",
                warehouse_note(*deposits)
            );
        }
    }
    Ok(())
}

/// One unit's dirty-scan row, split out from `report_dirty`'s printing so
/// the computed result is directly assertable in tests without capturing
/// stdout.
pub(crate) struct DirtyRow {
    pub(crate) name: String,
    pub(crate) state: &'static str, // "clean" | "new" | "dirty"
    pub(crate) added: Vec<String>,
    pub(crate) removed: Vec<String>,
    pub(crate) modified: Vec<String>,
}

/// The scan behind `report dirty`: reuses `fingerprint::classify` — the
/// same scan `unit status --dirty` and `mark-tape-only`'s guard use — over
/// every `active` unit (mirroring `pending_units_for_collection`'s own
/// scope: `missing`/`tape_only`/`retired` units have no live directory this
/// scan should second-guess), optionally narrowed to one unit by name.
/// `global_excludes` is `config.defaults.global_excludes` (issue #49),
/// kept in lockstep with those other callers.
pub(crate) fn dirty_rows(
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
    use rusqlite::params;
    use tempfile::TempDir;

    /// Issue #73 / ADR-0006: the three advisory surfaces that print a
    /// copies/locations pair must all see a recorded warehouse deposit.
    /// A unit whose second copy is a deposit is NOT at fire risk and is
    /// NOT single-location, and saying otherwise sends the operator to
    /// buy a tape they do not need.
    mod warehouse_deposits_count {
        use super::*;

        #[test]
        fn copies_rows_counts_the_deposit() {
            let (conn, _unit_id, _vol) =
                crate::policy::coverage::tests::setup_unit_with_deposit("active");
            let rows = copies_rows(&conn, Some("photos")).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].1, 2, "copies must include the warehouse deposit");
            assert_eq!(rows[0].2, 2, "locations must include the warehouse");
        }

        #[test]
        fn fire_risk_rows_counts_the_deposit() {
            let (conn, _unit_id, _vol) =
                crate::policy::coverage::tests::setup_unit_with_deposit("active");
            // min_copies = 3 keeps the unit in the at-risk set either way,
            // so the assertion is about the COUNT, not about membership.
            // The fixture unit has no dotfile and no archive set, so the
            // resolved threshold IS the default (issue #106).
            let mut config = Config::default();
            config.defaults.min_copies_for_tape_only = 3;
            let rows = fire_risk_rows(&conn, &config).unwrap();
            let row = rows
                .iter()
                .find(|r| r.unit == "photos")
                .expect("photos listed");
            assert_eq!(row.copies, 2, "copies must include the warehouse deposit");
            assert_eq!(row.locations, 2, "locations must include the warehouse");
        }

        /// Issue #106, the defect itself: an archive set demanding MORE
        /// copies than the global default must put the unit at risk. Before
        /// this, fire-risk applied `defaults.min_copies_for_tape_only` to
        /// every unit while `audit` resolved per unit — so the two commands
        /// answered the same question differently, and the one an operator
        /// glances at was the wrong one.
        #[test]
        fn fire_risk_uses_the_units_archive_set_threshold_not_the_global_default() {
            let (conn, unit_id, _vol) =
                crate::policy::coverage::tests::setup_unit_with_deposit("active");
            // The unit has 2 copies. Global default says 2 = fine; the
            // archive set it belongs to demands 3.
            conn.execute(
                "INSERT INTO archive_sets (name, min_copies) VALUES ('strict', 3)",
                [],
            )
            .unwrap();
            let as_id = conn.last_insert_rowid();
            conn.execute(
                "UPDATE units SET archive_set_id = ?1 WHERE id = ?2",
                params![as_id, unit_id],
            )
            .unwrap();

            let mut config = Config::default();
            config.defaults.min_copies_for_tape_only = 2;

            let rows = fire_risk_rows(&conn, &config).unwrap();
            let row = rows
                .iter()
                .find(|r| r.unit == "photos")
                .expect("the archive set demands 3 copies and the unit has 2 — it is at risk");
            assert_eq!(
                row.min_copies,
                Some(3),
                "the reported threshold must be the RESOLVED one, not the global default"
            );
            assert_eq!(row.risk, FireRisk::BelowMinimum);
        }

        /// The other direction, so the test above cannot pass by simply
        /// reporting everything: an archive set demanding FEWER copies than
        /// the global default must take the unit OUT of the at-risk set.
        #[test]
        fn fire_risk_respects_an_archive_set_that_is_laxer_than_the_default() {
            let (conn, unit_id, _vol) =
                crate::policy::coverage::tests::setup_unit_with_deposit("active");
            conn.execute(
                "INSERT INTO archive_sets (name, min_copies) VALUES ('lax', 1)",
                [],
            )
            .unwrap();
            let as_id = conn.last_insert_rowid();
            conn.execute(
                "UPDATE units SET archive_set_id = ?1 WHERE id = ?2",
                params![as_id, unit_id],
            )
            .unwrap();

            let mut config = Config::default();
            config.defaults.min_copies_for_tape_only = 5;

            let rows = fire_risk_rows(&conn, &config).unwrap();
            assert!(
                !rows.iter().any(|r| r.unit == "photos"),
                "a unit meeting its own archive set's min_copies=1 is not at risk, \
                 whatever the global default says: {rows:?}"
            );
        }

        /// A unit whose policy will not resolve must be LISTED, not skipped.
        /// Skipping reproduces the silent-pass class #59 removed from
        /// `audit`: the tool cannot tell whether the unit is covered, which
        /// is strictly worse than knowing it is not.
        #[test]
        fn fire_risk_lists_a_unit_whose_policy_will_not_resolve() {
            let (conn, unit_id, _vol) =
                crate::policy::coverage::tests::setup_unit_with_deposit("active");
            // A dangling archive_set_id makes `policy::resolve` fail (#105).
            conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
            conn.execute(
                "UPDATE units SET archive_set_id = 9999 WHERE id = ?1",
                params![unit_id],
            )
            .unwrap();
            conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();

            let rows = fire_risk_rows(&conn, &Config::default()).unwrap();
            let row = rows
                .iter()
                .find(|r| r.unit == "photos")
                .expect("an unresolvable unit must not vanish from the risk report");
            assert!(matches!(row.risk, FireRisk::PolicyUnresolvable(_)));
            assert_eq!(
                row.min_copies, None,
                "no threshold is known, and reporting one would be a guess"
            );
        }

        #[test]
        fn fire_risk_drops_a_unit_whose_second_copy_is_a_deposit() {
            let (conn, _unit_id, _vol) =
                crate::policy::coverage::tests::setup_unit_with_deposit("active");
            let mut config = Config::default();
            config.defaults.min_copies_for_tape_only = 2;
            let rows = fire_risk_rows(&conn, &config).unwrap();
            assert!(
                !rows.iter().any(|r| r.unit == "photos"),
                "a unit with a tape copy AND a warehouse deposit meets min_copies=2: {rows:?}"
            );
        }

        #[test]
        fn tape_only_rows_counts_the_deposit() {
            let (conn, _unit_id, _vol) =
                crate::policy::coverage::tests::setup_unit_with_deposit("tape_only");
            let rows = tape_only_rows(&conn, Some("photos")).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].1, 2, "copies must include the warehouse deposit");
            assert_eq!(rows[0].2, 2, "locations must include the warehouse");
        }
    }

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

        crate::staging::snapshot_create(&conn, "clean_unit", &Config::default()).unwrap();
        crate::staging::snapshot_create(&conn, "dirty_unit", &Config::default()).unwrap();

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
