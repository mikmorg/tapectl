use clap::Subcommand;
use rusqlite::Connection;
use serde::Serialize;
use tabled::{Table, Tabled};

use crate::config::{Config, TapectlPaths};
use crate::error::Result;
use crate::staging::clean;

#[derive(Subcommand, Debug)]
pub enum StagingCommands {
    /// Show staging area status
    Status,

    /// Clean staged files from disk
    Clean {
        /// Clean all staged sets, not just those with completed writes
        #[arg(long)]
        force: bool,
    },
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
    #[tabled(rename = "Size (MB)", display_with = "display_size_mb")]
    #[serde(rename = "total_encrypted_size")]
    encrypted_bytes: Option<i64>,
    #[tabled(rename = "Writes")]
    #[serde(rename = "write_count")]
    writes: i64,
    #[tabled(rename = "Staged")]
    #[serde(skip)]
    staged: String,
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
/// strings the table needs) because the MB division the table performs is
/// lossy -- a string-typed intermediate row could not reconstruct the exact
/// byte count the JSON contract requires, so there is no honest "verbatim,
/// then retype later" split for this one field. The row is built once in
/// `run` and shared by both the table and JSON branches (issue: C2
/// row-listing drift; previously the JSON branch read straight from the
/// query results and the table branch built `StagingRow` separately from
/// the same source, which is how #125-style drift happens even without a
/// hand-rolled reverse-parse).
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
                    staged: i.staged_at.unwrap_or_default(),
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
                    "cleaned {} stage set(s), {} files removed, {} MB freed",
                    report.sets_cleaned,
                    report.files_removed,
                    report.bytes_freed / (1024 * 1024),
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

    /// `staging status --json` shape (issue: C2 row-listing drift). One row
    /// has every optional field populated with a byte count that is NOT an
    /// even multiple of 1 MiB -- proof the JSON carries raw bytes, not the
    /// table's lossy MB-divided display value. The other row has every
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
                staged: "2026-07-01T00:00:00Z".to_string(),
            },
            StagingRow {
                id: 2,
                unit: "photos".to_string(),
                version: 1,
                status: "staging".to_string(),
                slices: None,
                encrypted_bytes: None,
                writes: 0,
                staged: String::new(),
            },
        ];
        let value = staging_rows_to_json(&rows);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"[{"num_slices":3,"stage_set_id":1,"status":"staged","total_encrypted_size":5242881,"unit":"backups","version":2,"write_count":1},{"num_slices":null,"stage_set_id":2,"status":"staging","total_encrypted_size":null,"unit":"photos","version":1,"write_count":0}]"#
        );
    }
}
