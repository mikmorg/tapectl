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
        /// also overrides the refusal to release a stage set whose unit is
        /// below its policy's resolved min_copies (issue #244)
        #[arg(long)]
        force: bool,
    },
}

/// One unit whose eligible copy count is below its own resolved
/// `min_copies`, among the units a non-force `staging clean` is about to
/// release staging for (issue #244, `docs/adr/0012-...md`'s 2026-09-17
/// amendment).
#[derive(Debug)]
struct UnderCopiedUnit {
    unit_name: String,
    copies: i64,
    min_copies: i64,
}

/// Which units a non-force `clean::clean_staging` call is about to release
/// staging for (its `'staged'` branch: at least one `writes` row exists and
/// none is non-`completed` -- the EXACT eligibility guard in
/// `staging::clean::clean_staging`'s `candidate_sql`, never re-derived a
/// second way here) do not yet meet their own resolved `min_copies`.
///
/// `'failed'` stage_sets are never candidates: `clean_staging` reclaims
/// those unconditionally and correctly (no `writes` row, no copy
/// requirement), and this function's SQL deliberately excludes them.
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
    let unit_names: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT u.name
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
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };

    let mut under = Vec::new();
    for name in unit_names {
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
                unit_name: name,
                copies,
                min_copies: resolved.min_copies,
            });
        }
    }
    Ok(under)
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
            // Issue #244, ADR-0012's 2026-09-17 amendment: refuse to
            // release a stage set whose unit is below its own resolved
            // min_copies, unless the operator passes --force. This gate
            // runs BEFORE `clean::clean_staging` (which stays policy-free,
            // per #238's conclusion for `execute_batch` and the amendment's
            // explicit constraint) and is skipped entirely when `force` is
            // set, so `--force`'s existing behaviour is unchanged.
            if !*force {
                let under_copied = under_copied_release_candidates(conn, config)?;
                if !under_copied.is_empty() {
                    // Issue #56's defect class: a `--json` refusal must be
                    // ONE parseable document on stdout, never a JSON object
                    // beside a human line. Printed here, then the process
                    // still exits non-zero via the `Err` below (its text
                    // lands on stderr, not stdout).
                    if json_output {
                        println!(
                            "{}",
                            serde_json::json!({
                                "refused": true,
                                "reason": "min_copies",
                                "under_copied": under_copied.iter().map(|u| serde_json::json!({
                                    "unit": u.unit_name,
                                    "copies": u.copies,
                                    "min_copies": u.min_copies,
                                })).collect::<Vec<_>>(),
                            })
                        );
                    }
                    let mut msg = String::from(
                        "staging clean refused: the following unit(s) have staged data \
                         that has not yet met their policy's min_copies -- cleaning now \
                         would discard the only cheap route to the copy the operator's \
                         own policy requires (issue #244). Pass --force to release \
                         anyway:\n",
                    );
                    for u in &under_copied {
                        msg.push_str(&format!(
                            "  {}: {}/{} copies\n",
                            u.unit_name, u.copies, u.min_copies
                        ));
                    }
                    return Err(TapectlError::Other(msg));
                }
            }

            let mut report = clean::clean_staging(conn, config, *force)?;
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

    /// **The failing test, before the fix (issue #244).** `min_copies`
    /// resolves to 2 and exactly ONE copy has completed — `staging clean`
    /// (no `--force`) must refuse, not release. Before this fix,
    /// `StagingCommands::Clean` called `clean::clean_staging` with no
    /// policy lookup at all, and `clean_staging`'s own non-force guard
    /// (`EXISTS a writes row AND NOT EXISTS a non-completed one`) passes
    /// vacuously on exactly this shape, so the `.age` file was deleted and
    /// the stage_set moved to `'cleaned'` — discarding the only cheap route
    /// to the second copy the operator's own policy requires.
    #[test]
    fn clean_refuses_when_a_unit_is_under_copied() {
        let (conn, staged_file, dir) = seed_unit_needing_a_second_copy("testlib/alpha");
        let config = config_with_staging_dir_and_min_copies(dir.path(), 2);
        let paths = TapectlPaths::new(dir.path().to_path_buf());

        let result = run(
            &conn,
            &paths,
            &config,
            &StagingCommands::Clean { force: false },
            false,
        );

        let err = result
            .expect_err("staging clean must refuse when a unit is below its policy's min_copies");
        let msg = err.to_string();
        assert!(
            msg.contains("testlib/alpha: 1/2 copies") && msg.contains("--force"),
            "refusal must name the unit's N/M copies and --force as the override: {msg}"
        );

        let status: String = conn
            .query_row("SELECT status FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            status, "staged",
            "the under-copied unit's stage_set must NOT be released"
        );
        assert!(
            staged_file.exists(),
            "the staged .age file must survive the refused clean"
        );
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
        );
        assert!(result.is_ok(), "{result:?}");

        let status: String = conn
            .query_row("SELECT status FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "cleaned");
        assert!(!staged_file.exists());
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
