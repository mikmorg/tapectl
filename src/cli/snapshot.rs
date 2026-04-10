use clap::Subcommand;
use rusqlite::{params, Connection};
use tabled::{Table, Tabled};

use crate::config::{Config, TapectlPaths};
use crate::error::Result;
use crate::staging;

#[derive(Subcommand, Debug)]
pub enum SnapshotCommands {
    /// Create a snapshot (fast directory walk + manifest)
    Create {
        /// Unit name
        name: String,
    },

    /// List snapshots
    List {
        /// Filter by unit name
        #[arg(long)]
        unit: Option<String>,
        /// Filter by status
        #[arg(long)]
        status: Option<String>,
    },

    /// Compare two snapshot versions of a unit
    Diff {
        /// Unit name
        name: String,
        /// First version number
        #[arg(long)]
        v1: i64,
        /// Second version number
        #[arg(long)]
        v2: i64,
    },

    /// Delete an unwritten snapshot
    Delete {
        /// Unit name
        name: String,
        /// Snapshot version to delete
        #[arg(long)]
        version: i64,
        /// Force delete even if staged (but not written)
        #[arg(long)]
        force: bool,
    },

    /// Purge a reclaimable snapshot (remove DB records)
    Purge {
        /// Unit name
        name: String,
        /// Snapshot version
        #[arg(long)]
        version: i64,
    },

    /// Mark a snapshot as reclaimable (with enforced preconditions)
    MarkReclaimable {
        /// Unit name
        name: String,
        /// Snapshot version
        #[arg(long)]
        version: i64,
        /// Override preconditions
        #[arg(long)]
        force: bool,
    },
}

#[derive(Tabled)]
struct SnapshotRow {
    #[tabled(rename = "ID")]
    id: i64,
    #[tabled(rename = "Unit")]
    unit: String,
    #[tabled(rename = "Ver")]
    version: i64,
    #[tabled(rename = "Status")]
    status: String,
    #[tabled(rename = "Files")]
    files: String,
    #[tabled(rename = "Size")]
    size: String,
    #[tabled(rename = "Created")]
    created: String,
}

pub fn run(
    conn: &Connection,
    _paths: &TapectlPaths,
    config: &Config,
    command: &SnapshotCommands,
    json_output: bool,
) -> Result<()> {
    match command {
        SnapshotCommands::Create { name } => {
            let snapshot_id = staging::snapshot_create(conn, name)?;

            let (version, total_size, file_count): (i64, Option<i64>, Option<i64>) = conn
                .query_row(
                    "SELECT version, total_size, file_count FROM snapshots WHERE id = ?1",
                    params![snapshot_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "snapshot_id": snapshot_id,
                        "unit": name,
                        "version": version,
                        "total_size": total_size,
                        "file_count": file_count,
                    })
                );
            } else {
                println!(
                    "snapshot created: {} v{} ({} files, {} MB)",
                    name,
                    version,
                    file_count.unwrap_or(0),
                    total_size.unwrap_or(0) / (1024 * 1024),
                );
            }
        }

        SnapshotCommands::Diff { name, v1, v2 } => {
            crate::cli::operations::snapshot_diff(conn, name, *v1, *v2, json_output)?;
        }

        SnapshotCommands::Delete {
            name,
            version,
            force,
        } => {
            crate::cli::operations::snapshot_delete(conn, name, *version, *force, json_output)?;
        }

        SnapshotCommands::Purge { name, version } => {
            crate::cli::operations::snapshot_purge(conn, name, *version, json_output)?;
        }

        SnapshotCommands::MarkReclaimable {
            name,
            version,
            force,
        } => {
            crate::cli::operations::snapshot_mark_reclaimable(
                conn,
                config,
                name,
                *version,
                *force,
                json_output,
            )?;
        }

        SnapshotCommands::List { unit, status } => {
            let mut sql = String::from(
                "SELECT s.id, u.name, s.version, s.status, s.file_count,
                        s.total_size, s.created_at
                 FROM snapshots s JOIN units u ON u.id = s.unit_id WHERE 1=1",
            );
            let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

            if let Some(unit_name) = unit {
                sql.push_str(" AND u.name = ?");
                param_values.push(Box::new(unit_name.clone()));
            }
            if let Some(st) = status {
                sql.push_str(" AND s.status = ?");
                param_values.push(Box::new(st.clone()));
            }
            sql.push_str(" ORDER BY u.name, s.version DESC");

            let params: Vec<&dyn rusqlite::types::ToSql> =
                param_values.iter().map(|p| p.as_ref()).collect();
            let mut stmt = conn.prepare(&sql)?;
            let rows: Vec<SnapshotRow> = stmt
                .query_map(params.as_slice(), |row| {
                    let size: Option<i64> = row.get(5)?;
                    Ok(SnapshotRow {
                        id: row.get(0)?,
                        unit: row.get(1)?,
                        version: row.get(2)?,
                        status: row.get(3)?,
                        files: row
                            .get::<_, Option<i64>>(4)?
                            .map(|n| n.to_string())
                            .unwrap_or_default(),
                        size: size
                            .map(|s| format!("{} MB", s / (1024 * 1024)))
                            .unwrap_or_default(),
                        created: row.get(6)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!(rows.iter().map(|r| {
                    serde_json::json!({"id": r.id, "unit": r.unit, "version": r.version, "status": r.status})
                }).collect::<Vec<_>>())).unwrap());
            } else if rows.is_empty() {
                println!("no snapshots found");
            } else {
                println!("{}", Table::new(rows));
            }
        }
    }
    Ok(())
}
