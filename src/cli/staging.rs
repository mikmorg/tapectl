use clap::Subcommand;
use rusqlite::{params, Connection};
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
        /// min_copies (issue #244, #262)
        #[arg(long)]
        force: bool,
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
/// candidate under the non-force guard: at least one `writes` row exists
/// for it and none is non-`completed` -- the EXACT eligibility guard in
/// `staging::clean::clean_staging`'s non-force `'staged'` branch, never
/// re-derived a second way here. Distinct by unit: a unit can have more
/// than one such stage_set (e.g. two superseded versions, each with its
/// own completed copy).
///
/// `'failed'` stage_sets are never candidates here -- `clean_staging`
/// reclaims those unconditionally regardless of scope (issue #262), so
/// they carry no min_copies question for this function to answer.
///
/// Returns `(unit id, unit name)` pairs. Shared by
/// [`under_copied_release_candidates`] (which unit_names to check) and
/// `StagingCommands::Clean`'s non-force path (the full candidate set, so it
/// can compute "covered" = candidates minus under-copied).
fn staged_release_candidate_units(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT u.id, u.name
         FROM stage_sets ss
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN units u ON u.id = s.unit_id
         WHERE ss.status = 'staged'
           AND EXISTS (SELECT 1 FROM writes w WHERE w.stage_set_id = ss.id)
           AND NOT EXISTS (
               SELECT 1 FROM writes w
               WHERE w.stage_set_id = ss.id AND w.status <> 'completed'
           )
         ORDER BY u.name",
    )?;
    let rows = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
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
) -> Result<Vec<UnderCopiedUnit>> {
    let candidates = staged_release_candidate_units(conn)?;

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
        if copies < resolved.min_copies {
            under.push(UnderCopiedUnit {
                unit_id: unit.id,
                unit_name: name,
                copies,
                min_copies: resolved.min_copies,
            });
        }
    }
    Ok(under)
}

/// Lines naming each retained unit and its shortfall, for both the
/// human-readable report and the JSON `"retained"` array. Factored out --
/// like [`staging_rows_to_json`] below -- so a test can assert on the exact
/// wording without scraping `println!` output.
fn retention_notice(under_copied: &[UnderCopiedUnit]) -> String {
    let mut msg = format!(
        "retained {} unit(s) below their policy's min_copies -- not cleaned; \
         releasing them now would discard the only cheap route to the copy \
         their own policy requires (issue #244). Pass --force to release \
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
                println!("{}", Table::new(rows));
            }
        }

        StagingCommands::Clean { force } => {
            // Issue #241: the #244 min_copies gate right below must
            // reproduce exactly, or a dry run would claim a release is
            // safe when the real run refuses it (or vice versa) — "a dry
            // run that hides a refusal is worse than no dry run".
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "staging clean",
                    "it enforces the #244 min_copies release gate, which a preview would \
                     have to reproduce exactly or risk being wrong.",
                ));
            }
            // Issue #244, ADR-0012's 2026-09-17 amendment, revised by issue
            // #262: a unit below its own resolved min_copies must have its
            // staged bytes RETAINED, but that no longer refuses the WHOLE
            // command -- every other candidate unit that already meets its
            // own min_copies is released. `--force` is unaffected: it still
            // releases everything, including under-copied units, exactly
            // as before.
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
                (
                    clean::clean_staging(conn, config, true, clean::CleanScope::Whole)?,
                    Vec::new(),
                )
            } else {
                let candidates = staged_release_candidate_units(conn)?;
                let under_copied = under_copied_release_candidates(conn, config)?;
                if under_copied.is_empty() {
                    // Nobody is under-copied -- identical to the pre-#262
                    // behaviour, archive-wide, no scoping needed.
                    (
                        clean::clean_staging(conn, config, false, clean::CleanScope::Whole)?,
                        Vec::new(),
                    )
                } else {
                    let under_ids: std::collections::HashSet<i64> =
                        under_copied.iter().map(|u| u.unit_id).collect();
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
                        "lockfiles_reclaimed": report.lockfiles_reclaimed,
                        // Issue #108: nothing will ever rediscover these,
                        // so a scripted consumer needs the paths, not a count.
                        "stranded": report.stranded,
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
                    "  sessions: {} reclaimed, {} retained, {} orphaned; {} lockfiles reclaimed",
                    report.session_dirs_reclaimed,
                    report.session_dirs_retained,
                    report.session_dirs_orphaned,
                    report.lockfiles_reclaimed,
                );
                if report.errors > 0 {
                    println!("  {} errors", report.errors);
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
            },
            ..Default::default()
        };
        config.defaults.min_copies_for_tape_only = min_copies;
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
            &StagingCommands::Clean { force: false },
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
            &StagingCommands::Clean { force: true },
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
            &StagingCommands::Clean { force: false },
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
            &StagingCommands::Clean { force: false },
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
            &StagingCommands::Clean { force: false },
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
            },
        ];
        let value = staging_rows_to_json(&rows);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"[{"num_slices":3,"stage_set_id":1,"staged_at":"2026-07-01T00:00:00Z","status":"staged","total_encrypted_size":5242881,"unit":"backups","version":2,"write_count":1},{"num_slices":null,"stage_set_id":2,"staged_at":null,"status":"staging","total_encrypted_size":null,"unit":"photos","version":1,"write_count":0}]"#
        );
    }
}
