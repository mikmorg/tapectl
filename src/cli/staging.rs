use clap::Subcommand;
use rusqlite::Connection;
use tabled::{Table, Tabled};

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

pub fn run(conn: &Connection, command: &StagingCommands, json_output: bool) -> Result<()> {
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
            let report = clean::clean_staging(conn, *force)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "sets_cleaned": report.sets_cleaned,
                        "files_removed": report.files_removed,
                        "bytes_freed": report.bytes_freed,
                        "errors": report.errors,
                    })
                );
            } else {
                println!(
                    "cleaned {} stage set(s), {} files removed, {} MB freed",
                    report.sets_cleaned,
                    report.files_removed,
                    report.bytes_freed / (1024 * 1024),
                );
                if report.errors > 0 {
                    println!("  {} errors", report.errors);
                }
            }
        }
    }
    Ok(())
}
