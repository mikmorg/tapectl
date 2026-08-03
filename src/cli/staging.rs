use clap::Subcommand;
use rusqlite::Connection;
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

#[derive(Tabled)]
struct StagingRow {
    #[tabled(rename = "ID")]
    id: i64,
    #[tabled(rename = "Unit")]
    unit: String,
    #[tabled(rename = "Ver")]
    version: i64,
    #[tabled(rename = "Status")]
    status: String,
    #[tabled(rename = "Slices")]
    slices: String,
    #[tabled(rename = "Size (MB)")]
    size_mb: String,
    #[tabled(rename = "Writes")]
    writes: i64,
    #[tabled(rename = "Staged")]
    staged: String,
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
            if json_output {
                let json_rows: Vec<serde_json::Value> = info
                    .iter()
                    .map(|i| {
                        serde_json::json!({
                            "stage_set_id": i.stage_set_id,
                            "unit": i.unit_name,
                            "version": i.version,
                            "status": i.status,
                            "num_slices": i.num_slices,
                            "total_encrypted_size": i.total_encrypted_size,
                            "write_count": i.write_count,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json_rows).unwrap());
            } else if info.is_empty() {
                println!("no staged data");
            } else {
                let rows: Vec<StagingRow> = info
                    .into_iter()
                    .map(|i| StagingRow {
                        id: i.stage_set_id,
                        unit: i.unit_name,
                        version: i.version,
                        status: i.status,
                        slices: i.num_slices.map(|n| n.to_string()).unwrap_or_default(),
                        size_mb: i
                            .total_encrypted_size
                            .map(|s| (s / (1024 * 1024)).to_string())
                            .unwrap_or_default(),
                        writes: i.write_count,
                        staged: i.staged_at.unwrap_or_default(),
                    })
                    .collect();
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
