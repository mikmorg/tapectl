//! `tapectl db` command bodies (issue #112).
//!
//! These lived inlined in `main.rs`. That mattered beyond tidiness: the crate
//! is a dual lib+bin target with `main.rs` as a thin wrapper precisely because
//! integration tests import `tapectl::` and CANNOT reach anything defined in
//! the binary — so logic inlined there was logic no integration test could
//! exercise. `db stats`' raw SQL was a concrete example.
//!
//! Process exit stays in `main.rs` where it belongs: this returns the exit
//! code `db fsck` computed and lets the binary decide to act on it.

use rusqlite::Connection;

use crate::cli::DbCommands;
use crate::config::TapectlPaths;
use crate::error::Result;

/// Run a `db` subcommand. Returns the process exit code the caller should
/// honour — non-zero only for `fsck`, whose whole point is that it must not
/// report success when it found real problems (issue #45/H10).
pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    command: &DbCommands,
    json_output: bool,
    yes: bool,
    dry_run: bool,
) -> Result<i32> {
    let mut exit_code = crate::error::EXIT_SUCCESS;
    match command {
        DbCommands::Backup { to, include_keys } => {
            // Issue #247: `db_backup` itself checks `dry_run` FIRST, before
            // either `Connection::open` call — so on the dry path it has
            // already printed its own preview and returned, and the success
            // lines below must not print a second, contradictory message.
            crate::cli::operations::db_backup(paths, to, *include_keys, dry_run, json_output)?;
            if !dry_run {
                if json_output {
                    println!(
                        "{}",
                        serde_json::json!({"backup": to, "keys_included": include_keys})
                    );
                } else if *include_keys {
                    println!("database and keys backed up to {to}");
                } else {
                    println!(
                        "database backed up to {to} (private keys not included — pass --include-keys to copy them)"
                    );
                }
            }
        }
        DbCommands::Fsck { repair } => {
            // Issue #247: `dry_run` only changes behaviour when `--repair`
            // is also set — `db_fsck` computes the SAME violation report
            // either way and only skips the DELETE loop, so this arm's own
            // job is just to word "would repair" instead of "repaired" via
            // `report.dry_run`. A plain `fsck` (no `--repair`) already never
            // mutates, matching every other `Verdict::ReadOnly` leaf.
            let report = crate::cli::operations::db_fsck(conn, *repair, dry_run)?;
            // Issue #233: `conn` here may be `db::open_for_repair`'s
            // connection — opened deliberately WITHOUT running `migrate()`,
            // so a repair against it leaves the schema exactly where it
            // started (behind head) until the very next ordinary
            // `db::open()` call. An operator who sees "repaired N rows" and
            // then hits a schema error on the next command must not think
            // the tool is broken, so say so here. Cheap to check
            // unconditionally: on the ordinary (already-migrated) path this
            // is always `false`, so it changes nothing there. The `--json`
            // object below carries `schema_pending` unconditionally,
            // dry run included; the human "note:" line further down,
            // though, is printed only on a real repair (the `else` branch)
            // — the human dry-run branch does not print it, a known gap
            // (issue #257) rather than something this comment should claim
            // is already covered.
            let schema_pending = !crate::db::schema_is_current(conn)?;
            if json_output {
                let mut obj = serde_json::json!({
                    "integrity_ok": report.integrity_ok,
                    "issues": report.issues,
                    "repaired": report.repaired,
                    "schema_pending": schema_pending,
                });
                if report.dry_run {
                    obj["dry_run"] = serde_json::json!(true);
                }
                println!("{obj}");
            } else if report.dry_run {
                println!(
                    "fsck: integrity={}, issues={} (DRY RUN — would repair; no changes made)",
                    if report.integrity_ok { "ok" } else { "FAIL" },
                    report.issues.len(),
                );
                for issue in &report.issues {
                    println!("  {issue}");
                }
            } else {
                println!(
                    "fsck: integrity={}, issues={}, repaired={}",
                    if report.integrity_ok { "ok" } else { "FAIL" },
                    report.issues.len(),
                    report.repaired,
                );
                for issue in &report.issues {
                    println!("  {issue}");
                }
                if schema_pending {
                    println!(
                        "note: this repair ran against a database that has not finished \
                         migrating (issue #233) — the next tapectl command will complete the \
                         migration."
                    );
                }
            }
            // issue #45/H10: fsck must not exit 0 when it found real
            // problems. The CODE is computed here; ACTING on it (i.e.
            // terminating the process) stays in `main.rs` — see
            // `fsck_exit_code` there for the exact rule. Unaffected by
            // `dry_run`: a preview that finds issues is exactly as much a
            // warning as a real run that finds and leaves the same issues
            // (i.e. `fsck` with no `--repair` at all) — see
            // `fsck_exit_code_issues_found_but_not_repaired_is_still_warning`.
            exit_code = fsck_exit_code(&report);
        }
        DbCommands::Export => {
            // Full streaming JSON dump of the database (issue #61):
            // schema_version + every user table, enumerated from
            // sqlite_master at runtime. `--json` is a no-op here since
            // the output is unconditionally JSON. Nothing but the JSON
            // document itself may go to stdout on this path.
            let stdout = std::io::stdout();
            let mut writer = std::io::BufWriter::new(stdout.lock());
            crate::db::export::export_json(conn, &mut writer)?;
        }
        DbCommands::Import { path: import_path } => {
            crate::cli::operations::db_import(paths, import_path, yes, dry_run, json_output)?;
        }
        DbCommands::Stats => {
            let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
            let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
            let db_size = page_count * page_size;
            let table_count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
                [],
                |r| r.get(0),
            )?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"size_bytes": db_size, "tables": table_count, "pages": page_count})
                );
            } else {
                println!(
                    "database: {}, {table_count} tables, {page_count} pages",
                    crate::util::format_bytes_binary(db_size)
                );
            }
        }
    }
    Ok(exit_code)
}

/// Decide the process exit code for `db fsck` from its report (issue
/// #45/H10). Mirrors the `audit` convention: 0=clean, 1=warning,
/// 2=violation.
///
/// - The integrity check itself failing is a violation, full stop — no
///   amount of orphan-row repair changes that.
/// - Any other issue (e.g. orphaned rows) is a warning regardless of
///   whether `--repair` fixed it: an unrepaired finding (fsck run without
///   `--repair`) is still a finding nobody should see reported as a clean
///   0 — that is exactly the "reports success while finding problems" bug
///   this ticket exists to kill. It just isn't tape/DB corruption, so it's
///   1, not 2.
/// - No issues at all is genuinely clean.
pub fn fsck_exit_code(report: &crate::cli::operations::FsckReport) -> i32 {
    if !report.integrity_ok {
        crate::error::EXIT_ERROR
    } else if !report.issues.is_empty() {
        crate::error::EXIT_WARNING
    } else {
        crate::error::EXIT_SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::operations::FsckReport;

    #[test]
    fn fsck_exit_code_clean_is_success() {
        let report = FsckReport {
            integrity_ok: true,
            issues: vec![],
            repaired: 0,
            dry_run: false,
        };
        assert_eq!(fsck_exit_code(&report), crate::error::EXIT_SUCCESS);
    }

    #[test]
    fn fsck_exit_code_broken_integrity_is_violation() {
        let report = FsckReport {
            integrity_ok: false,
            issues: vec!["integrity_check: corrupted".to_string()],
            repaired: 0,
            dry_run: false,
        };
        assert_eq!(fsck_exit_code(&report), crate::error::EXIT_ERROR);
    }

    #[test]
    fn fsck_exit_code_broken_integrity_is_violation_even_if_other_things_repaired() {
        // Integrity failure outranks repair count — repairing orphan rows
        // does not paper over a corrupted database.
        let report = FsckReport {
            integrity_ok: false,
            issues: vec!["integrity_check: corrupted".to_string(), "1 orphan".into()],
            repaired: 1,
            dry_run: false,
        };
        assert_eq!(fsck_exit_code(&report), crate::error::EXIT_ERROR);
    }

    #[test]
    fn fsck_exit_code_issues_found_and_repaired_is_warning() {
        let report = FsckReport {
            integrity_ok: true,
            issues: vec!["3 orphaned write records".to_string()],
            repaired: 1,
            dry_run: false,
        };
        assert_eq!(fsck_exit_code(&report), crate::error::EXIT_WARNING);
    }

    #[test]
    fn fsck_exit_code_issues_found_but_not_repaired_is_still_warning() {
        // Ran without --repair: nothing got fixed (repaired == 0), but the
        // finding is real and must not be swallowed as a clean 0.
        let report = FsckReport {
            integrity_ok: true,
            issues: vec!["3 orphaned write records".to_string()],
            repaired: 0,
            dry_run: false,
        };
        assert_eq!(fsck_exit_code(&report), crate::error::EXIT_WARNING);
    }
}
