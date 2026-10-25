use clap::Subcommand;
use rusqlite::{params, params_from_iter, Connection};
use serde::Serialize;
use tabled::{Table, Tabled};

use crate::config::{Config, TapectlPaths};
use crate::error::{Result, TapectlError};
use crate::staging::clean;

#[derive(Subcommand, Debug)]
pub enum StagingCommands {
    /// Show staging area status
    Status,

    /// Clean staged files from disk
    Clean {
        /// Clean all staged sets, not just those with completed writes;
        /// also releases a unit's staged data that a bare `staging clean`
        /// would otherwise retain for being below its policy's resolved
        /// min_copies
        #[arg(long)]
        force: bool,

        /// Narrow which staged sets are considered for release to this
        /// unit (repeatable). Without `--force`, the same
        /// min_copies split still applies within the named unit(s): one
        /// below its own resolved min_copies keeps its staged bytes
        /// retained (pass `--force` to release it too, exactly as for the
        /// whole-archive form). A `'failed'` stage_set is always swept
        /// regardless of this scope -- see `CleanScope::Units`'s own
        /// contract in `src/staging/clean.rs`. An unknown unit name is an
        /// error, not a silent no-op.
        #[arg(long = "unit")]
        unit: Vec<String>,

        /// Narrow release further, to just this snapshot version within
        /// the single unit named by `--unit`. Requires
        /// exactly one `--unit`; zero or more than one is a usage error
        /// naming the constraint, never a silent widening back to
        /// unit-level scope. Exists so a `--force` release meant for one
        /// under-written version of a unit can never also release a
        /// DIFFERENT, still-unwritten version of that same unit -- see
        /// `CleanScope::UnitVersion`'s contract in `src/staging/clean.rs`.
        #[arg(long = "version")]
        version: Option<i64>,
    },
}

/// One unit whose eligible copy count is below its own resolved
/// `min_copies`, among the units a non-force `staging clean` is about to
/// consider releasing staging for (issue #244, `docs/adr/0012-...md`'s
/// 2026-09-17 amendment). Since issue #262, this unit's own staged bytes
/// are RETAINED rather than causing the whole command to refuse; every
/// OTHER candidate unit that meets its own `min_copies` is still released.
#[derive(Debug)]
struct UnderCopiedUnit {
    unit_id: i64,
    unit_name: String,
    copies: i64,
    min_copies: i64,
}

/// Every unit with at least one `'staged'` stage_set that is a release
/// candidate under the non-force guard: at least one `writes` row is
/// `completed` and every other is `completed` or `cut` -- the EXACT
/// eligibility guard in `staging::clean::clean_staging`'s non-force
/// `'staged'` branch ([`crate::staging::clean::release_candidate_predicate_sql`]),
/// never re-derived a second way here. Distinct by unit: a unit can have more
/// than one such stage_set (e.g. two superseded versions, each with its
/// own completed copy).
///
/// `'failed'` stage_sets are never candidates here -- `clean_staging`
/// reclaims those unconditionally regardless of scope (issue #262), so
/// they carry no min_copies question for this function to answer.
///
/// `unit_filter`, when `Some`, narrows the candidate set to just those unit
/// ids (issue #274's `--unit`) -- the SAME query, not a second one, so a
/// `--unit`-scoped `staging clean` and the whole-archive form can never
/// disagree about what counts as a release candidate (issue #96). `None`
/// reproduces the unfiltered, whole-archive query verbatim.
///
/// Returns `(unit id, unit name)` pairs. Shared by
/// [`under_copied_release_candidates`] (which unit_names to check) and
/// `StagingCommands::Clean`'s non-force path (the full candidate set, so it
/// can compute "covered" = candidates minus under-copied).
fn staged_release_candidate_units(
    conn: &Connection,
    unit_filter: Option<&[i64]>,
) -> Result<Vec<(i64, String)>> {
    let filter_ids: &[i64] = unit_filter.unwrap_or(&[]);
    let filter_predicate = match unit_filter {
        None => String::new(),
        Some(ids) => format!(
            "AND u.id IN ({})",
            ids.iter().map(|_| "?").collect::<Vec<_>>().join(",")
        ),
    };
    // The guard itself, never restated (issue #96): since 1.6.0 (#420) it
    // also reads a `cut` row as no write, which an inline copy here missed.
    let candidate = crate::staging::clean::release_candidate_predicate_sql("ss.id");
    let sql = format!(
        "SELECT DISTINCT u.id, u.name
         FROM stage_sets ss
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN units u ON u.id = s.unit_id
         WHERE ss.status = 'staged'
           AND {candidate}
           {filter_predicate}
         ORDER BY u.name"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(filter_ids.iter()), |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Which of [`staged_release_candidate_units`] do not yet meet their own
/// resolved `min_copies`.
///
/// The copy count itself is `policy::coverage::copy_count_expr` against
/// `policy::resolve(...).min_copies` -- the SAME derivation
/// `collection::batch::under_copied_units` uses for the exact same
/// question after `execute_batch`'s own write loop (issue #96's
/// single-derivation rule: `policy::coverage` is the sole owner of "how
/// many copies does this unit have"). This is not a second definition —
/// it is the same expression, applied to units enumerated from the DB
/// instead of a known `Batch`.
fn under_copied_release_candidates(
    conn: &Connection,
    config: &Config,
    unit_filter: Option<&[i64]>,
) -> Result<Vec<UnderCopiedUnit>> {
    let candidates = staged_release_candidate_units(conn, unit_filter)?;

    let mut under = Vec::new();
    for (_, name) in candidates {
        let unit = crate::db::queries::get_unit_by_name(conn, &name)?.ok_or_else(|| {
            TapectlError::Other(format!(
                "staging clean: unit \"{name}\" has a staged stage_set but is missing \
                 from the catalog -- the catalog is inconsistent"
            ))
        })?;
        let resolved = crate::policy::resolve(conn, config, &unit)?;
        let sql = format!(
            "SELECT {}",
            crate::policy::coverage::copy_count_expr(
                &crate::policy::coverage::CoverageQuery::current_unit("?1")
            )
        );
        let copies: i64 = conn.query_row(&sql, params![unit.id], |row| row.get(0))?;
        // The copy floor through the one tape-only rule (ADR-0012
        // 2026-10-12 item 43): a tape-only unit's staged bytes (staged
        // again by `volume read-slices`) are retained until it has
        // `min_copies` times the multiplier, the floor `audit` judges it by.
        let min_copies =
            crate::policy::coverage::copy_floor(config, resolved.copy_floor_base(), &unit.status);
        if copies < min_copies {
            under.push(UnderCopiedUnit {
                unit_id: unit.id,
                unit_name: name,
                copies,
                min_copies,
            });
        }
    }
    Ok(under)
}

/// The scope actually passed to [`clean::clean_staging`] when releasing
/// `ids` (issue #278). Ordinarily this is just `CleanScope::Units(ids)` --
/// but when a `--version` was given, `ids` is guaranteed (by the pairing
/// check in `StagingCommands::Clean`'s handler) to be exactly the one unit
/// id that version belongs to, so the release narrows one step further, to
/// `CleanScope::UnitVersion`, matching only that unit's stage_set at that
/// version rather than every `'staged'` set the unit has.
fn release_scope(ids: &[i64], version: Option<i64>) -> clean::CleanScope<'_> {
    match (version, ids) {
        (Some(v), [unit_id]) => clean::CleanScope::UnitVersion {
            unit_id: *unit_id,
            version: v,
        },
        _ => clean::CleanScope::Units(ids),
    }
}

/// Lines naming each retained unit and its shortfall, for both the
/// human-readable report and the JSON `"retained"` array. Factored out --
/// like [`staging_rows_to_json`] below -- so a test can assert on the exact
/// wording without scraping `println!` output.
fn retention_notice(under_copied: &[UnderCopiedUnit]) -> String {
    let mut msg = format!(
        "retained {} unit(s) below their policy's min_copies -- not cleaned; \
         releasing them now would discard the only cheap route to the copy \
         their own policy requires. Pass --force to release \
         them too:\n",
        under_copied.len()
    );
    for u in under_copied {
        msg.push_str(&format!(
            "  {}: {}/{} copies\n",
            u.unit_name, u.copies, u.min_copies
        ));
    }
    msg
}

/// `[{"stage_set_id": N, "slices": [..]}, ..]` for a report's per-slice
/// lists (#478).
pub(crate) fn slice_runs_json(runs: &[(i64, Vec<i64>)]) -> serde_json::Value {
    runs.iter()
        .map(|(id, slices)| serde_json::json!({ "stage_set_id": id, "slices": slices }))
        .collect::<Vec<_>>()
        .into()
}

/// The lines naming what the per-slice release did (#478; ADR-0012 item
/// 129): each set's slices released, and each set's slices kept because they
/// are short of copies, with what writes them. Empty when it did nothing.
pub(crate) fn slice_release_notice(report: &clean::CleanReport) -> String {
    let mut msg = String::new();
    for (id, slices) in &report.slices_released {
        msg.push_str(&format!(
            "  stage set {id}: slice(s) {} released, each now has its copies (the set spans \
             cartridges and is released slice by slice)\n",
            clean::describe_slice_numbers(slices)
        ));
    }
    for (id, slices) in &report.slices_short {
        msg.push_str(&format!(
            "  stage set {id} kept in part: slice(s) {} are short of copies and continue its \
             span; `tapectl volume write <LABEL>` onto another cartridge of the same \
             generation writes them (--force would leave the ranges already on tape unable \
             to be continued)\n",
            clean::describe_slice_numbers(slices)
        ));
    }
    msg
}

/// `staging status`'s line for a set released in part (#478).
fn partly_released_notice(r: &StagingRow) -> String {
    format!(
        "stage set {} ({} v{}) spans cartridges and is released slice by slice: slice(s) {} \
         released, {} still staged",
        r.id,
        r.unit,
        r.version,
        clean::describe_slice_numbers(&r.released_slices),
        clean::describe_slice_numbers(&r.staged_slices),
    )
}

#[derive(Tabled, Serialize)]
struct StagingRow {
    #[tabled(rename = "ID")]
    #[serde(rename = "stage_set_id")]
    id: i64,
    #[tabled(rename = "Unit")]
    unit: String,
    #[tabled(rename = "Ver")]
    version: i64,
    #[tabled(rename = "Status")]
    status: String,
    #[tabled(rename = "Slices", display_with = "display_opt_i64")]
    #[serde(rename = "num_slices")]
    slices: Option<i64>,
    #[tabled(rename = "Size (MiB)", display_with = "display_size_mb")]
    #[serde(rename = "total_encrypted_size")]
    encrypted_bytes: Option<i64>,
    #[tabled(rename = "Writes")]
    #[serde(rename = "write_count")]
    writes: i64,
    /// Table-only until CTO decision 2026-09-11 (architecture review C2
    /// follow-up, C2b). Renamed to `staged_at` (not `staged`) to match its
    /// sibling fields' convention in this struct — `id`/`slices`/
    /// `encrypted_bytes`/`writes` all serialize under the fuller DB-native
    /// name (`stage_set_id`/`num_slices`/`total_encrypted_size`/
    /// `write_count`) rather than the short display-oriented field name.
    #[tabled(rename = "Staged", display_with = "display_opt_string")]
    #[serde(rename = "staged_at")]
    staged: Option<String>,
    /// Issue #402: what is wrong with a 'staged' set's files on disk —
    /// empty when it is whole. Printed below the table, not in it.
    #[tabled(skip)]
    #[serde(rename = "on_disk_problems")]
    problems: Vec<String>,
    /// The command that makes the set whole again; `null` when it is.
    #[tabled(skip)]
    remedy: Option<String>,
    /// #478 (ADR-0012 item 129): a set released in part, slice by slice —
    /// the slice numbers released. Empty unless it was. Printed below the
    /// table, not in it.
    #[tabled(skip)]
    released_slices: Vec<i64>,
    /// With `released_slices`, the slice numbers still staged; empty when it
    /// is.
    #[tabled(skip)]
    staged_slices: Vec<i64>,
}

fn display_opt_string(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

fn display_opt_i64(v: &Option<i64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_default()
}

fn display_size_mb(v: &Option<i64>) -> String {
    v.map(|s| (s / (1024 * 1024)).to_string())
        .unwrap_or_default()
}

/// `staging status --json` shape. Change 1 already types `slices` and
/// `encrypted_bytes` as `Option<i64>` (rather than the pre-formatted display
/// strings the table needs) because the MiB division the table performs is
/// lossy -- a string-typed intermediate row could not reconstruct the exact
/// byte count the JSON contract requires, so there is no honest "verbatim,
/// then retype later" split for this one field. The row is built once in
/// `run` and shared by both the table and JSON branches (issue: C2
/// row-listing drift; previously the JSON branch read straight from the
/// query results and the table branch built `StagingRow` separately from
/// the same source, which is how #125-style drift happens even without a
/// hand-rolled reverse-parse). `staged` (JSON `staged_at`) was table-only
/// until CTO decision 2026-09-11 (architecture review C2 follow-up, C2b).
fn staging_rows_to_json(rows: &[StagingRow]) -> serde_json::Value {
    serde_json::to_value(rows).unwrap()
}

pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    command: &StagingCommands,
    json_output: bool,
    dry_run: bool,
) -> Result<()> {
    match command {
        StagingCommands::Status => {
            let info = clean::staging_status(conn)?;
            let rows: Vec<StagingRow> = info
                .into_iter()
                .map(|i| StagingRow {
                    id: i.stage_set_id,
                    unit: i.unit_name,
                    version: i.version,
                    status: i.status,
                    slices: i.num_slices,
                    encrypted_bytes: i.total_encrypted_size,
                    writes: i.write_count,
                    staged: i.staged_at,
                    problems: i.on_disk_problems,
                    remedy: i.remedy,
                    released_slices: i.released_slices,
                    staged_slices: i.staged_slices,
                })
                .collect();
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&staging_rows_to_json(&rows)).unwrap()
                );
            } else if rows.is_empty() {
                println!("no staged data");
            } else {
                // Issue #402: a staged set that is not whole on disk cannot
                // be written (`volume write` refuses it) — say so here,
                // where the operator looks before a write.
                let broken: Vec<String> = rows
                    .iter()
                    .filter(|r| !r.problems.is_empty())
                    .map(|r| {
                        let mut s = format!(
                            "stage set {} ({} v{}) is NOT whole on disk — `volume write` \
                             will refuse it:",
                            r.id, r.unit, r.version
                        );
                        for p in &r.problems {
                            s.push_str(&format!("\n  - {p}"));
                        }
                        if let Some(remedy) = &r.remedy {
                            s.push_str(&format!("\n  remedy: {remedy}"));
                        }
                        s
                    })
                    .collect();
                // #478 (ADR-0012 item 129): a set that spans cartridges is
                // released slice by slice, and stays 'staged' until its last
                // slice goes.
                let partly: Vec<String> = rows
                    .iter()
                    .filter(|r| !r.released_slices.is_empty())
                    .map(partly_released_notice)
                    .collect();
                println!("{}", Table::new(rows));
                for b in broken {
                    println!("{b}");
                }
                for p in partly {
                    println!("{p}");
                }
            }
        }

        StagingCommands::Clean {
            force,
            unit,
            version,
        } => {
            // Issue #241: the #244 min_copies gate right below must
            // reproduce exactly, or a dry run would claim a release is
            // safe when the real run refuses it (or vice versa) — "a dry
            // run that hides a refusal is worse than no dry run".
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "staging clean",
                    "it enforces the min_copies release gate, which a preview would \
                     have to reproduce exactly or risk being wrong.",
                ));
            }

            // Issue #274: resolve every named `--unit` up front, eagerly --
            // an unknown name is an error naming the unit, never silently
            // ignored (it would otherwise just vanish from the narrowed
            // candidate set below and look like "nothing to clean").
            let named_ids: Vec<i64> = unit
                .iter()
                .map(|name| {
                    crate::db::queries::get_unit_by_name(conn, name)?
                        .map(|u| u.id)
                        .ok_or_else(|| TapectlError::UnitNotFound(name.clone()))
                })
                .collect::<Result<Vec<i64>>>()?;

            // Issue #278: `--version` only ever narrows within a SINGLE
            // named unit -- it says nothing about which unit if more than
            // one (or none) was named, so that pairing is a usage error
            // naming the constraint, never a silent fallback to unit-level
            // scope or to `--version`'s first/only unit guessed some other
            // way.
            if version.is_some() && named_ids.len() != 1 {
                return Err(TapectlError::Other(format!(
                    "`--version` requires exactly one `--unit` naming the unit that \
                     version belongs to, got {} -- pass exactly one `--unit <name>` \
                     alongside `--version <N>`, or drop `--version` to scope by unit alone",
                    named_ids.len()
                )));
            }

            // `None` here is exactly the pre-#274 unscoped path -- every
            // branch below falls back to `CleanScope::Whole` in that case,
            // reproducing today's output and `CleanScope::Whole`/
            // covered-ids calls verbatim for a bare `staging clean`.
            let unit_scope: Option<&[i64]> = if named_ids.is_empty() {
                None
            } else {
                Some(named_ids.as_slice())
            };

            // Issue #244, ADR-0012's 2026-09-17 amendment, revised by issue
            // #262: a unit below its own resolved min_copies must have its
            // staged bytes RETAINED, but that no longer refuses the WHOLE
            // command -- every other candidate unit that already meets its
            // own min_copies is released. `--force` is unaffected: it still
            // releases everything in scope, including under-copied units,
            // exactly as before.
            //
            // `clean_staging` itself stays policy-free (#238's conclusion
            // for `execute_batch`, restated by ADR-0012's amendment): this
            // function decides WHICH units are covered, then hands that
            // selection to `clean_staging` via `CleanScope::Units` -- never
            // a policy decision inside `clean_staging` itself. `'failed'`
            // stage_sets carry no copy requirement at all, so they are
            // swept by `clean_staging` regardless of this scope (issue
            // #262's ruling on `CleanScope::Units`).
            let (mut report, retained): (clean::CleanReport, Vec<UnderCopiedUnit>) = if *force {
                let scope = match unit_scope {
                    Some(ids) => release_scope(ids, *version),
                    None => clean::CleanScope::Whole,
                };
                (clean::clean_staging(conn, config, true, scope)?, Vec::new())
            } else {
                let candidates = staged_release_candidate_units(conn, unit_scope)?;
                let under_copied = under_copied_release_candidates(conn, config, unit_scope)?;
                if under_copied.is_empty() {
                    // Nobody in scope is under-copied -- identical to the
                    // pre-#262 behaviour when unscoped, and still narrowed
                    // to exactly the named units when `--unit` was given
                    // (never widened to `Whole` just because nothing here
                    // needed retaining).
                    let scope = match unit_scope {
                        Some(ids) => release_scope(ids, *version),
                        None => clean::CleanScope::Whole,
                    };
                    (
                        clean::clean_staging(conn, config, false, scope)?,
                        Vec::new(),
                    )
                } else {
                    let under_ids: std::collections::HashSet<i64> =
                        under_copied.iter().map(|u| u.unit_id).collect();
                    // The candidates are the whole-set rule's
                    // (`release_candidate_predicate_sql`): a set holding a
                    // range is never one, it is released slice by slice
                    // below, whatever this split decides.
                    let covered_ids: Vec<i64> = candidates
                        .into_iter()
                        .map(|(id, _)| id)
                        .filter(|id| !under_ids.contains(id))
                        .collect();
                    // `covered_ids` may be empty (every candidate is
                    // under-copied) -- still call `clean_staging`, never
                    // skip it, because its `'failed'` sweep is
                    // unconditional and must run regardless.
                    (
                        clean::clean_staging(
                            conn,
                            config,
                            false,
                            clean::CleanScope::Units(&covered_ids),
                        )?,
                        under_copied,
                    )
                }
            };
            // #478 (ADR-0012 items 85 and 129; ruling X15): the sets that
            // hold a range are released slice by slice, each slice once its
            // copies meet its unit's floor, over the whole scope and
            // whatever the min_copies split above decided. `--force` has
            // already released them whole.
            if !*force {
                let scope = match unit_scope {
                    Some(ids) => release_scope(ids, *version),
                    None => clean::CleanScope::Whole,
                };
                report.absorb(crate::staging::release::release_ranges(
                    conn, config, scope,
                )?);
            }
            clean::reclaim_session_dirs_and_lockfiles(
                conn,
                config,
                &paths.db_file,
                *force,
                &mut report,
            )?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "sets_cleaned": report.sets_cleaned,
                        "files_removed": report.files_removed,
                        "bytes_freed": report.bytes_freed,
                        "errors": report.errors,
                        "session_dirs_reclaimed": report.session_dirs_reclaimed,
                        "session_dirs_retained": report.session_dirs_retained,
                        "session_dirs_orphaned": report.session_dirs_orphaned,
                        // Issue #402: kept for a sealed, aborted session
                        // `volume resume` can still re-confirm.
                        "session_dirs_awaiting_reconfirm": report.session_dirs_awaiting_reconfirm,
                        "lockfiles_reclaimed": report.lockfiles_reclaimed,
                        // Issue #465 item 5: a killed run's catalogue
                        // directories and <home>/tmp entries a day old.
                        "home_leftovers_reclaimed": report.home_leftovers_reclaimed,
                        // Issue #108: nothing will ever rediscover these,
                        // so a scripted consumer needs the paths, not a count.
                        "stranded": report.stranded,
                        // Issue #376: stage sets a live session is using,
                        // never released, `--force` or not.
                        "held_by_live_session": report.held_by_live_session.iter().map(|(id, labels)| serde_json::json!({
                            "stage_set_id": id,
                            "volumes": labels,
                        })).collect::<Vec<_>>(),
                        // Issue #483: stage sets a pending `volume rewrite`
                        // holds (they may be the only copy until its new
                        // volume has passed a full readback), never
                        // released, `--force` or not.
                        "held_by_pending_rewrite": report.held_by_pending_rewrite.iter().map(|(id, hold)| serde_json::json!({
                            "stage_set_id": id,
                            "source": hold.source,
                            "destination": hold.destination,
                            "resume": hold.resume_command(),
                        })).collect::<Vec<_>>(),
                        // Issue #262: units whose staged bytes were
                        // retained because they have not met their
                        // resolved min_copies. Always present (empty when
                        // nothing was retained) so a scripted consumer
                        // never has to branch on whether the key exists.
                        "retained": retained.iter().map(|u| serde_json::json!({
                            "unit": u.unit_name,
                            "copies": u.copies,
                            "min_copies": u.min_copies,
                        })).collect::<Vec<_>>(),
                        // #478 (ADR-0012 item 129): slices released on their
                        // own from sets that span cartridges, and those
                        // still short of copies. Always present.
                        "slices_released": slice_runs_json(&report.slices_released),
                        "slices_short": slice_runs_json(&report.slices_short),
                    })
                );
            } else {
                println!(
                    "cleaned {} stage set(s), {} files removed, {} freed",
                    report.sets_cleaned,
                    report.files_removed,
                    crate::util::format_bytes_binary(report.bytes_freed),
                );
                println!(
                    "  sessions: {} reclaimed, {} retained, {} orphaned; {} lockfiles reclaimed; \
                     {} leftovers of killed runs removed",
                    report.session_dirs_reclaimed,
                    report.session_dirs_retained,
                    report.session_dirs_orphaned,
                    report.lockfiles_reclaimed,
                    report.home_leftovers_reclaimed,
                );
                if report.errors > 0 {
                    println!("  {} errors", report.errors);
                }
                // Issue #402: a sealed tape whose aborted session can still
                // be counted — its frozen layout stays until then.
                for label in &report.session_dirs_awaiting_reconfirm {
                    println!(
                        "  session of volume \"{label}\" kept: its tape is sealed and `tapectl \
                         volume resume {label}` can re-confirm it after a clean `tapectl volume \
                         verify {label} --full` (`--force` removes it, forfeiting that)"
                    );
                }
                // Issue #376: a live session's slices are never released —
                // a fact, not a judgement, so `--force` does not reach it.
                for (id, labels) in &report.held_by_live_session {
                    println!(
                        "  stage set {id} kept: a live session is using it on volume(s) {} \
                         (`--force` does not apply; run this again once it has finished)",
                        labels.join(", ")
                    );
                }
                // Issue #483 (ADR-0012 2026-10-19 item 106): a pending
                // rewrite's staged slices may be the only copy of a unit.
                for (id, hold) in &report.held_by_pending_rewrite {
                    println!("  {}", hold.describe(*id));
                }
                // Issue #108: `staging clean` nulls `staging_path` before it
                // unlinks, so a file whose unlink failed can never be found
                // by a later clean. This printout is the operator's only
                // notice, which is why it names every path rather than
                // reporting a count.
                if !report.stranded.is_empty() {
                    println!(
                        "  {} file(s) could NOT be removed and are now stranded permanently —",
                        report.stranded.len()
                    );
                    println!("  no future `staging clean` can find them. Remove by hand:");
                    for p in &report.stranded {
                        println!("    {}", p.display());
                    }
                }
                if !retained.is_empty() {
                    print!("{}", retention_notice(&retained));
                }
                print!("{}", slice_release_notice(&report));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use rusqlite::params;

    /// Local to this file (scope fence: `src/collection/batch.rs` is owned
    /// by another worker and is not edited here). Mirrors
    /// `collection::batch::tests::seed_unit_with_one_completed_copy` +
    /// `..._and_staged_file`: one active unit with exactly one `completed`
    /// write on a `sealed` volume, its lone `stage_set` still `'staged'`
    /// with a real `.age` file on disk and a `stage_slices` row pointing at
    /// it. This is the exact vacuous-pass shape
    /// `default_guard_cleans_when_the_only_planned_copy_completed`
    /// (`src/staging/clean.rs`) pins as CORRECT for `clean_staging` in
    /// isolation — the defect (issue #244) is this file's caller invoking
    /// it unconditionally. Returns `(conn, staged_file_path, TempDir
    /// guard)`; the guard must outlive the assertions.
    fn seed_unit_needing_a_second_copy(
        unit_name: &str,
    ) -> (Connection, std::path::PathBuf, tempfile::TempDir) {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES (?1, ?1, ?2, '/tmp/u', 'active')",
            params![unit_name, tenant_id],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        // 'current', not 'staged': `policy::coverage::CoverageQuery::current_unit`
        // (what `under_copied_release_candidates` uses, same as
        // `collection::batch::under_copied_units`) only counts a unit's
        // CURRENT snapshot(s) — matching what `volume::session` actually
        // promotes a just-sealed snapshot to.
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'current', '/tmp/u', 1, 10)",
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

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("slice_1.age");
        std::fs::write(&path, b"staged slice bytes").unwrap();
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                        sha256_plain, sha256_encrypted, staging_path)
             VALUES (?1, 1, 19, 19, 'deadbeef', 'deadbeef', ?2)",
            params![stage_set_id, path.to_string_lossy()],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
             VALUES ('V1', 'lto', 'lto0', 2500000000000, 'sealed')",
            [],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'completed')",
            params![stage_set_id, snap_id, volume_id],
        )
        .unwrap();

        (conn, path, dir)
    }

    fn config_with_staging_dir_and_min_copies(dir: &std::path::Path, min_copies: i32) -> Config {
        let mut config = Config {
            staging: crate::config::StagingConfig {
                directory: dir.to_string_lossy().to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        config.defaults.min_copies = min_copies;
        config
    }

    /// **Originally "the failing test, before the fix" for issue #244;
    /// revised for issue #262.** Before #244's fix, `StagingCommands::Clean`
    /// called `clean::clean_staging` with no policy lookup at all, and
    /// `clean_staging`'s own non-force guard (`EXISTS a writes row AND NOT
    /// EXISTS a non-completed one`) passed vacuously on exactly this shape,
    /// so the `.age` file was deleted — discarding the only cheap route to
    /// the second copy the operator's own policy requires. #244 fixed that
    /// by refusing the WHOLE command. #262 revised it again: with only ONE
    /// candidate unit in the whole database (this fixture), there is
    /// nothing else to release, so the command still retains this unit's
    /// staged bytes exactly as #244 intended -- but it no longer treats
    /// that as a command-level error. It now succeeds (there was valid work
    /// to do: sweeping any `'failed'` sets and session dirs/lockfiles,
    /// though this fixture has none) and reports the retention instead of
    /// erroring on it.
    #[test]
    fn clean_retains_the_under_copied_unit_without_refusing_the_command() {
        let (conn, staged_file, dir) = seed_unit_needing_a_second_copy("testlib/alpha");
        let config = config_with_staging_dir_and_min_copies(dir.path(), 2);
        let paths = TapectlPaths::new(dir.path().to_path_buf());

        let result = run(
            &conn,
            &paths,
            &config,
            &StagingCommands::Clean {
                force: false,
                unit: Vec::new(),
                version: None,
            },
            false,
            false,
        );

        assert!(
            result.is_ok(),
            "issue #262: a bare `staging clean` must not refuse the WHOLE \
             command just because one unit is under-copied -- it must still \
             succeed and merely retain this unit's staged bytes: {result:?}"
        );

        let status: String = conn
            .query_row("SELECT status FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            status, "staged",
            "the under-copied unit's stage_set must NOT be released"
        );
        assert!(staged_file.exists(), "the staged .age file must survive");
    }

    /// ADR-0012 2026-10-12 item 43: the retention gate's floor is the one
    /// tape-only rule's. One copy meets `min_copies = 1` for an active unit
    /// (the positive control: released); the same unit tape-only at 3x
    /// needs 3, so its staged bytes — the cheap route to those copies — are
    /// retained.
    #[test]
    fn the_retention_floor_takes_the_tape_only_multiplier() {
        let (conn, _staged_file, dir) = seed_unit_needing_a_second_copy("testlib/alpha");
        let mut config = config_with_staging_dir_and_min_copies(dir.path(), 1);
        config.compaction.tape_only_safety_multiplier = 3;

        let under = under_copied_release_candidates(&conn, &config, None).unwrap();
        assert!(
            under.is_empty(),
            "an active unit is held to min_copies alone: {under:?}"
        );

        conn.execute("UPDATE units SET status = 'tape_only'", [])
            .unwrap();
        let under = under_copied_release_candidates(&conn, &config, None).unwrap();
        assert_eq!(under.len(), 1, "{under:?}");
        assert_eq!((under[0].copies, under[0].min_copies), (1, 3));
    }

    /// [`retention_notice`] is what both the human-readable and (indirectly)
    /// the JSON report surface for a retained unit -- pinned directly since
    /// `println!` output cannot be asserted on otherwise.
    #[test]
    fn retention_notice_names_the_unit_its_shortfall_and_force() {
        let under = vec![UnderCopiedUnit {
            unit_id: 1,
            unit_name: "solo".to_string(),
            copies: 1,
            min_copies: 2,
        }];
        let msg = retention_notice(&under);
        assert!(
            msg.contains("solo: 1/2 copies"),
            "must name the unit and its N/M copies: {msg}"
        );
        assert!(msg.contains("--force"), "must name --force: {msg}");
    }

    /// Trap 5: `--force` must still override the new gate exactly as it
    /// already overrides `clean_staging`'s own guard — no change to what
    /// `--force` does, only to when the gate is reached at all.
    #[test]
    fn clean_force_overrides_the_under_copied_refusal() {
        let (conn, staged_file, dir) = seed_unit_needing_a_second_copy("testlib/alpha");
        let config = config_with_staging_dir_and_min_copies(dir.path(), 2);
        let paths = TapectlPaths::new(dir.path().to_path_buf());

        let result = run(
            &conn,
            &paths,
            &config,
            &StagingCommands::Clean {
                force: true,
                unit: Vec::new(),
                version: None,
            },
            false,
            false,
        );
        assert!(result.is_ok(), "{result:?}");

        let status: String = conn
            .query_row("SELECT status FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "cleaned");
        assert!(!staged_file.exists());
    }

    /// Shared building block for issue #262's negative controls: one unit
    /// with `n_writes` completed writes (each on its own volume) and one
    /// real staged `.age` file on disk, in a DB that may already hold other
    /// units. Returns the staged file's path.
    fn seed_staged_unit_with_completed_writes(
        conn: &Connection,
        dir: &std::path::Path,
        tenant_id: i64,
        unit_name: &str,
        n_writes: i64,
    ) -> std::path::PathBuf {
        let safe_name = unit_name.replace('/', "_");
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES (?1, ?1, ?2, '/tmp/u', 'active')",
            params![unit_name, tenant_id],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'current', '/tmp/u', 1, 10)",
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

        let path = dir.join(format!("{safe_name}.age"));
        std::fs::write(&path, b"staged slice bytes").unwrap();
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                        sha256_plain, sha256_encrypted, staging_path)
             VALUES (?1, 1, 19, 19, 'deadbeef', 'deadbeef', ?2)",
            params![stage_set_id, path.to_string_lossy()],
        )
        .unwrap();

        for i in 0..n_writes {
            let label = format!("V-{safe_name}-{i}");
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
                 VALUES (?1, 'lto', 'lto0', 2500000000000, 'sealed')",
                params![label],
            )
            .unwrap();
            let volume_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, 'completed')",
                params![stage_set_id, snap_id, volume_id],
            )
            .unwrap();
        }

        path
    }

    /// **Issue #262's first negative control, written before the fix.**
    /// Against pre-fix code, `staging clean` (no `--force`) refuses the
    /// WHOLE command the instant ANY unit is under-copied -- releasing
    /// NOTHING, not even a completely unrelated unit that is fully
    /// covered. This seeds one covered unit ("testlib/covered", 2
    /// completed copies against min_copies=2) alongside one under-copied
    /// unit ("testlib/solo", 1 completed copy against min_copies=2) and
    /// asserts the covered unit's staged bytes ARE released while the
    /// under-copied unit's are retained, with the command as a whole
    /// succeeding. Must fail against unfixed code: the whole command
    /// errors out before touching either unit, so `covered_path` still
    /// exists and `result.is_ok()` is false.
    #[test]
    fn clean_releases_the_covered_unit_and_retains_the_under_copied_one() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_staging_dir_and_min_copies(dir.path(), 2);
        let paths = TapectlPaths::new(dir.path().to_path_buf());

        let covered_path = seed_staged_unit_with_completed_writes(
            &conn,
            dir.path(),
            tenant_id,
            "testlib/covered",
            2,
        );
        let under_path =
            seed_staged_unit_with_completed_writes(&conn, dir.path(), tenant_id, "testlib/solo", 1);

        let result = run(
            &conn,
            &paths,
            &config,
            &StagingCommands::Clean {
                force: false,
                unit: Vec::new(),
                version: None,
            },
            false,
            false,
        );

        assert!(
            result.is_ok(),
            "issue #262: a covered unit's release must not be blocked by an \
             unrelated under-copied unit: {result:?}"
        );
        assert!(
            !covered_path.exists(),
            "the covered unit's staged bytes must be released"
        );
        assert!(
            under_path.exists(),
            "the under-copied unit's staged bytes must be retained"
        );
    }

    /// **Issue #262's second negative control, written before the fix.** A
    /// `'failed'` stage_set carries no copy requirement at all --
    /// `clean_staging`'s own doc says it is swept unconditionally, `force`
    /// or not. It must be swept by a bare `staging clean` even while a
    /// DIFFERENT unit is under-copied and retained. Must fail against
    /// unfixed code: the whole command refuses before `clean_staging` is
    /// ever reached, so the failed set's `.age` file survives and its
    /// status stays `'failed'`.
    #[test]
    fn clean_sweeps_a_failed_set_even_while_another_unit_is_under_copied() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_staging_dir_and_min_copies(dir.path(), 2);
        let paths = TapectlPaths::new(dir.path().to_path_buf());

        // Under-copied unit, unrelated to the failed set below.
        let _under_path =
            seed_staged_unit_with_completed_writes(&conn, dir.path(), tenant_id, "testlib/solo", 1);

        // A 'failed' stage_set for a second unit, with a real .age file.
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES ('other-uuid', 'testlib/other', ?1, '/tmp/u', 'active')",
            params![tenant_id],
        )
        .unwrap();
        let other_unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'created', '/tmp/u', 1, 10)",
            params![other_unit_id],
        )
        .unwrap();
        let other_snap_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'failed', 524288)",
            params![other_snap_id],
        )
        .unwrap();
        let failed_stage_set_id = conn.last_insert_rowid();
        let failed_path = dir.path().join("other_failed.age");
        std::fs::write(&failed_path, b"failed slice bytes").unwrap();
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                        sha256_plain, sha256_encrypted, staging_path)
             VALUES (?1, 1, 19, 19, 'deadbeef', 'deadbeef', ?2)",
            params![failed_stage_set_id, failed_path.to_string_lossy()],
        )
        .unwrap();

        let result = run(
            &conn,
            &paths,
            &config,
            &StagingCommands::Clean {
                force: false,
                unit: Vec::new(),
                version: None,
            },
            false,
            false,
        );

        // Assert the collateral damage FIRST, so a red run names the actual
        // defect (a 'failed' set with no copy requirement was retained
        // anyway) rather than only "the command returned an error" -- that
        // distinguishes this control from the first one.
        assert!(
            !failed_path.exists(),
            "a 'failed' set has no copy requirement and must be swept \
             regardless of any other unit's min_copies shortfall"
        );
        let failed_status: String = conn
            .query_row(
                "SELECT status FROM stage_sets WHERE id = ?1",
                params![failed_stage_set_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(failed_status, "cleaned");
        assert!(result.is_ok(), "{result:?}");
    }

    /// Negative control: a unit whose resolved `min_copies = 1` is already
    /// met by the one completed copy must still auto-release exactly as it
    /// did before this fix — the behaviour the new gate could most easily
    /// break.
    #[test]
    fn clean_still_releases_when_min_copies_is_already_met() {
        let (conn, staged_file, dir) = seed_unit_needing_a_second_copy("testlib/alpha");
        let config = config_with_staging_dir_and_min_copies(dir.path(), 1);
        let paths = TapectlPaths::new(dir.path().to_path_buf());

        let result = run(
            &conn,
            &paths,
            &config,
            &StagingCommands::Clean {
                force: false,
                unit: Vec::new(),
                version: None,
            },
            false,
            false,
        );
        assert!(result.is_ok(), "{result:?}");

        let status: String = conn
            .query_row("SELECT status FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "cleaned");
        assert!(!staged_file.exists());
    }

    // ── issue #274: `--unit` scoping ──

    /// `--unit` naming a covered unit releases only that unit's staged
    /// bytes and leaves a DIFFERENT covered unit's staged bytes untouched
    /// -- the narrowing `CleanScope::Units` exists for, exercised through
    /// the CLI's own `--unit` flag rather than the internal `CleanScope`
    /// directly.
    #[test]
    fn unit_flag_releases_only_the_named_covered_unit() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_staging_dir_and_min_copies(dir.path(), 2);
        let paths = TapectlPaths::new(dir.path().to_path_buf());

        let named_path = seed_staged_unit_with_completed_writes(
            &conn,
            dir.path(),
            tenant_id,
            "testlib/named",
            2,
        );
        let other_path = seed_staged_unit_with_completed_writes(
            &conn,
            dir.path(),
            tenant_id,
            "testlib/other-covered",
            2,
        );

        let result = run(
            &conn,
            &paths,
            &config,
            &StagingCommands::Clean {
                force: false,
                unit: vec!["testlib/named".to_string()],
                version: None,
            },
            false,
            false,
        );
        assert!(result.is_ok(), "{result:?}");
        assert!(
            !named_path.exists(),
            "the named unit's staged bytes must be released"
        );
        assert!(
            other_path.exists(),
            "an unrelated covered unit outside --unit's scope must survive"
        );
    }

    /// `--unit` naming a unit the catalog has never heard of is an error
    /// that names the unit -- not a silent no-op that quietly cleans
    /// nothing.
    #[test]
    fn unit_flag_with_an_unknown_name_errors() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_staging_dir_and_min_copies(dir.path(), 2);
        let paths = TapectlPaths::new(dir.path().to_path_buf());

        let result = run(
            &conn,
            &paths,
            &config,
            &StagingCommands::Clean {
                force: false,
                unit: vec!["testlib/ghost".to_string()],
                version: None,
            },
            false,
            false,
        );
        let err = result.expect_err("an unknown --unit name must error, not silently no-op");
        assert!(
            err.to_string().contains("testlib/ghost"),
            "the error must name the unknown unit: {err}"
        );
    }

    /// `--unit` + `--force` on an under-copied unit releases it -- the
    /// exact mechanism `restage_action`'s forced recipe (issue #274)
    /// relies on, exercised here through the CLI flag rather than
    /// `clean_staging` directly.
    #[test]
    fn unit_flag_with_force_releases_an_under_copied_unit() {
        let (conn, staged_file, dir) = seed_unit_needing_a_second_copy("testlib/alpha");
        let config = config_with_staging_dir_and_min_copies(dir.path(), 2);
        let paths = TapectlPaths::new(dir.path().to_path_buf());

        let result = run(
            &conn,
            &paths,
            &config,
            &StagingCommands::Clean {
                force: true,
                unit: vec!["testlib/alpha".to_string()],
                version: None,
            },
            false,
            false,
        );
        assert!(result.is_ok(), "{result:?}");

        let status: String = conn
            .query_row("SELECT status FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "cleaned");
        assert!(!staged_file.exists());
    }

    /// Issue #278's acceptance shape, verbatim: the SAME unit has a
    /// version (1) already written and retained only for being under
    /// min_copies, and a DIFFERENT version (4) that was staged and never
    /// written anywhere. `restage_action` (`src/cli/audit.rs`) now emits
    /// `--force`'s recipe scoped to `--version 1` for exactly this
    /// shape; this is the CLI-level proof that the flag itself enforces
    /// the narrowing, independent of what `restage_action` composes: v4's
    /// bytes -- which exist nowhere else -- must survive a `--force`
    /// clean aimed at v1.
    #[test]
    fn version_flag_with_force_releases_only_the_named_version_of_the_unit() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES ('photos', 'photos', ?1, '/tmp/u', 'active')",
            params![tenant_id],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_staging_dir_and_min_copies(dir.path(), 2);
        let paths = TapectlPaths::new(dir.path().to_path_buf());

        // v1: one completed write -- under min_copies=2, so a bare clean
        // would retain it (the "retained" half of the shape).
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'current', '/tmp/u', 1, 10)",
            params![unit_id],
        )
        .unwrap();
        let snap1 = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
            params![snap1],
        )
        .unwrap();
        let stage_set_v1 = conn.last_insert_rowid();
        let path_v1 = dir.path().join("v1.age");
        std::fs::write(&path_v1, b"v1 bytes").unwrap();
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                        sha256_plain, sha256_encrypted, staging_path)
             VALUES (?1, 1, 8, 8, 'deadbeef', 'deadbeef', ?2)",
            params![stage_set_v1, path_v1.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
             VALUES ('V1', 'lto', 'lto0', 2500000000000, 'sealed')",
            [],
        )
        .unwrap();
        let vol1 = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'completed')",
            params![stage_set_v1, snap1, vol1],
        )
        .unwrap();

        // v4: staged, never written anywhere -- these bytes exist nowhere
        // else.
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 4, 'current', '/tmp/u', 1, 10)",
            params![unit_id],
        )
        .unwrap();
        let snap4 = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
            params![snap4],
        )
        .unwrap();
        let stage_set_v4 = conn.last_insert_rowid();
        let path_v4 = dir.path().join("v4.age");
        std::fs::write(&path_v4, b"v4 bytes").unwrap();
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                        sha256_plain, sha256_encrypted, staging_path)
             VALUES (?1, 1, 8, 8, 'deadbeef', 'deadbeef', ?2)",
            params![stage_set_v4, path_v4.to_string_lossy()],
        )
        .unwrap();

        let result = run(
            &conn,
            &paths,
            &config,
            &StagingCommands::Clean {
                force: true,
                unit: vec!["photos".to_string()],
                version: Some(1),
            },
            false,
            false,
        );
        assert!(result.is_ok(), "{result:?}");

        assert!(!path_v1.exists(), "v1's staged bytes must be released");
        assert!(
            path_v4.exists(),
            "issue #278: v4 was never written anywhere -- a --force clean \
             scoped to --version 1 must not reach it just because it shares \
             a unit with v1"
        );

        let status_v4: String = conn
            .query_row(
                "SELECT status FROM stage_sets WHERE id = ?1",
                params![stage_set_v4],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status_v4, "staged", "v4's stage_set must be untouched");
    }

    /// `--version` with no `--unit` at all is a usage error naming the
    /// pairing constraint -- never a silent no-op and never a guess at
    /// which unit was meant.
    #[test]
    fn version_flag_with_no_unit_errors_naming_the_constraint() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_staging_dir_and_min_copies(dir.path(), 2);
        let paths = TapectlPaths::new(dir.path().to_path_buf());

        let result = run(
            &conn,
            &paths,
            &config,
            &StagingCommands::Clean {
                force: false,
                unit: Vec::new(),
                version: Some(1),
            },
            false,
            false,
        );
        let err = result.expect_err("--version with zero --unit must error");
        let msg = err.to_string();
        assert!(msg.contains("--version"), "{msg}");
        assert!(msg.contains("--unit"), "{msg}");
    }

    /// `--version` with more than one `--unit` is the same usage error --
    /// which of the named units the version belongs to is ambiguous, so
    /// this must not silently pick one.
    #[test]
    fn version_flag_with_two_units_errors_naming_the_constraint() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        for name in ["testlib/one", "testlib/two"] {
            conn.execute(
                "INSERT INTO units (uuid, name, tenant_id, current_path, status)
                 VALUES (?1, ?1, ?2, '/tmp/u', 'active')",
                params![name, tenant_id],
            )
            .unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_staging_dir_and_min_copies(dir.path(), 2);
        let paths = TapectlPaths::new(dir.path().to_path_buf());

        let result = run(
            &conn,
            &paths,
            &config,
            &StagingCommands::Clean {
                force: false,
                unit: vec!["testlib/one".to_string(), "testlib/two".to_string()],
                version: Some(1),
            },
            false,
            false,
        );
        let err = result.expect_err("--version with two --unit values must error");
        let msg = err.to_string();
        assert!(msg.contains("--version"), "{msg}");
        assert!(msg.contains("--unit"), "{msg}");
    }

    /// #478 (ADR-0012 items 85 and 129): a bare `staging clean` releases a
    /// spanning set slice by slice: the slices tape 1's range gave their
    /// copies go, the others stay, and the report names both. A whole unit
    /// short of copies beside it is still retained by the min_copies split,
    /// which does not reach the span (ruling X15).
    #[test]
    fn clean_releases_a_spans_covered_slices_and_names_the_short_ones() {
        use crate::staging::release::fixtures::*;
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_staging_dir_and_min_copies(dir.path(), 1);
        let paths = TapectlPaths::new(dir.path().to_path_buf());
        let t = tenant(&conn);
        let alpha = unit(&conn, t, "testlib/alpha");
        let beta = unit(&conn, t, "testlib/beta");
        let (span, files) = staged_span(&conn, alpha, 1, "staged", dir.path(), 3);
        range_on(&conn, &span, "V-T1", 1..=2);
        // Beta: written whole, but onto a volume that is not eligible, so it
        // has no copy and the min_copies split retains it.
        let short = staged_version(&conn, beta, 1, "current", dir.path(), 0);
        add_write(&conn, &short, "V-B1", "initialized", "completed");

        run(
            &conn,
            &paths,
            &config,
            &StagingCommands::Clean {
                force: false,
                unit: Vec::new(),
                version: None,
            },
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            files.iter().map(|f| f.exists()).collect::<Vec<_>>(),
            [false, false, true]
        );
        assert_eq!(stage_set_status(&conn, span.stage_set_id), "staged");
        assert!(short.file.exists(), "beta has no eligible copy: retained");

        let mut report = clean::CleanReport::default();
        report.slices_released.push((span.stage_set_id, vec![1, 2]));
        report.slices_short.push((span.stage_set_id, vec![3]));
        let notice = slice_release_notice(&report);
        assert!(
            notice.contains(&format!(
                "stage set {}: slice(s) 1-2 released",
                span.stage_set_id
            )),
            "{notice}"
        );
        assert!(
            notice.contains(&format!(
                "stage set {} kept in part: slice(s) 3 are short of copies",
                span.stage_set_id
            )) && notice.contains("`tapectl volume write <LABEL>`"),
            "{notice}"
        );
        assert!(slice_release_notice(&clean::CleanReport::default()).is_empty());
    }

    /// `staging clean --unit` narrows the per-slice release too: another
    /// unit's span is never reached.
    #[test]
    fn clean_with_unit_releases_only_that_units_span() {
        use crate::staging::release::fixtures::*;
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_staging_dir_and_min_copies(dir.path(), 1);
        let paths = TapectlPaths::new(dir.path().to_path_buf());
        let t = tenant(&conn);
        let alpha = unit(&conn, t, "testlib/alpha");
        let beta = unit(&conn, t, "testlib/beta");
        let (a, a_files) = staged_span(&conn, alpha, 1, "staged", dir.path(), 2);
        let (b, b_files) = staged_span(&conn, beta, 1, "staged", dir.path(), 2);
        range_on(&conn, &a, "V-A", 1..=1);
        range_on(&conn, &b, "V-B", 1..=1);

        run(
            &conn,
            &paths,
            &config,
            &StagingCommands::Clean {
                force: false,
                unit: vec!["testlib/alpha".to_string()],
                version: None,
            },
            false,
            false,
        )
        .unwrap();
        assert!(!a_files[0].exists() && a_files[1].exists());
        assert!(b_files[0].exists(), "beta was not named");
        assert_eq!(stage_set_status(&conn, b.stage_set_id), "staged");
    }

    /// `staging status` names a set released in part, with the slices
    /// released and those still staged.
    #[test]
    fn partly_released_notice_names_released_and_staged_slices() {
        let row = StagingRow {
            id: 7,
            unit: "photos".to_string(),
            version: 2,
            status: "staged".to_string(),
            slices: Some(5),
            encrypted_bytes: Some(1),
            writes: 1,
            staged: None,
            problems: Vec::new(),
            remedy: None,
            released_slices: vec![1, 2, 3],
            staged_slices: vec![4, 5],
        };
        assert_eq!(
            partly_released_notice(&row),
            "stage set 7 (photos v2) spans cartridges and is released slice by slice: \
             slice(s) 1-3 released, 4-5 still staged"
        );
    }

    /// `staging status --json` shape (issue: C2 row-listing drift). One row
    /// has every optional field populated with a byte count that is NOT an
    /// even multiple of 1 MiB -- proof the JSON carries raw bytes, not the
    /// table's lossy MiB-divided display value. The other row has every
    /// optional field absent (`null`).
    #[test]
    fn pin_staging_rows_json_shape() {
        let rows = vec![
            StagingRow {
                id: 1,
                unit: "backups".to_string(),
                version: 2,
                status: "staged".to_string(),
                slices: Some(3),
                encrypted_bytes: Some(5_242_881),
                writes: 1,
                staged: Some("2026-07-01T00:00:00Z".to_string()),
                problems: vec!["slice 2: no staged file recorded".to_string()],
                remedy: Some(
                    "tapectl volume read-slices --from L6-0001 --unit backups".to_string(),
                ),
                released_slices: vec![1],
                staged_slices: vec![2, 3],
            },
            StagingRow {
                id: 2,
                unit: "photos".to_string(),
                version: 1,
                status: "staging".to_string(),
                slices: None,
                encrypted_bytes: None,
                writes: 0,
                staged: None,
                problems: Vec::new(),
                remedy: None,
                released_slices: Vec::new(),
                staged_slices: Vec::new(),
            },
        ];
        let value = staging_rows_to_json(&rows);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"[{"num_slices":3,"on_disk_problems":["slice 2: no staged file recorded"],"released_slices":[1],"remedy":"tapectl volume read-slices --from L6-0001 --unit backups","stage_set_id":1,"staged_at":"2026-07-01T00:00:00Z","staged_slices":[2,3],"status":"staged","total_encrypted_size":5242881,"unit":"backups","version":2,"write_count":1},{"num_slices":null,"on_disk_problems":[],"released_slices":[],"remedy":null,"stage_set_id":2,"staged_at":null,"staged_slices":[],"status":"staging","total_encrypted_size":null,"unit":"photos","version":1,"write_count":0}]"#
        );
    }
}
