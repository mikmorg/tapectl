use clap::Subcommand;
use rusqlite::{params, Connection};
use tabled::{Table, Tabled};

use crate::error::{Result, TapectlError};

#[derive(Subcommand, Debug)]
pub enum CatalogCommands {
    /// List files in a unit's latest snapshot
    Ls {
        /// Unit name
        unit: String,
        /// Snapshot version (default: latest)
        #[arg(long)]
        version: Option<i64>,
    },

    /// Search for files by pattern
    Search {
        /// Search pattern (substring match)
        pattern: String,
        /// Limit results
        #[arg(long, default_value = "50")]
        limit: i64,
    },

    /// Show which volume(s) contain a unit
    Locate {
        /// Unit name
        unit: String,
    },

    /// Show catalog statistics
    Stats,
}

#[derive(Tabled)]
struct FileRow {
    #[tabled(rename = "Path")]
    path: String,
    #[tabled(rename = "Size")]
    size: String,
    #[tabled(rename = "Modified")]
    modified: String,
    #[tabled(rename = "SHA256")]
    sha256: String,
}

#[derive(Tabled)]
struct LocationRow {
    #[tabled(rename = "Volume")]
    volume: String,
    #[tabled(rename = "Snapshot")]
    version: i64,
    #[tabled(rename = "Slices")]
    slices: i64,
    #[tabled(rename = "Written")]
    written: String,
}

pub fn run(conn: &Connection, command: &CatalogCommands, json_output: bool) -> Result<()> {
    match command {
        CatalogCommands::Ls { unit, version } => {
            let unit_row = crate::db::queries::get_unit_by_name(conn, unit)?
                .ok_or_else(|| TapectlError::UnitNotFound(unit.clone()))?;

            let snapshot_id: i64 = if let Some(v) = version {
                conn.query_row(
                    "SELECT id FROM snapshots WHERE unit_id = ?1 AND version = ?2",
                    params![unit_row.id, v],
                    |row| row.get(0),
                )
                .map_err(|_| TapectlError::Other(format!("snapshot v{v} not found")))?
            } else {
                conn.query_row(
                    "SELECT id FROM snapshots WHERE unit_id = ?1 ORDER BY version DESC LIMIT 1",
                    params![unit_row.id],
                    |row| row.get(0),
                )
                .map_err(|_| TapectlError::Other("no snapshots found".into()))?
            };

            let mut stmt = conn.prepare(
                "SELECT path, size_bytes, modified_at, sha256, is_directory
                 FROM files WHERE snapshot_id = ?1 ORDER BY path",
            )?;
            let rows: Vec<FileRow> = stmt
                .query_map(params![snapshot_id], |row| {
                    let size: i64 = row.get(1)?;
                    let is_dir: bool = row.get(4)?;
                    Ok(FileRow {
                        path: format!(
                            "{}{}",
                            if is_dir { "d " } else { "  " },
                            row.get::<_, String>(0)?
                        ),
                        size: if is_dir {
                            "-".into()
                        } else {
                            format_size(size)
                        },
                        modified: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        sha256: row
                            .get::<_, Option<String>>(3)?
                            .map(|s| format!("{}...", &s[..12]))
                            .unwrap_or_else(|| "(unstaged)".into()),
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                let json: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|r| {
                        serde_json::json!({"path": r.path.trim(), "size": r.size, "sha256": r.sha256})
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json).unwrap());
            } else if rows.is_empty() {
                println!("no files found");
            } else {
                println!("{}", Table::new(rows));
            }
        }

        CatalogCommands::Search { pattern, limit } => {
            // Build an FTS5 MATCH expression: split on non-alphanumeric, prefix-match each
            // token with AND. FTS5 default tokenizer already splits paths this way, so a
            // pattern like "foo/bar" becomes `foo* bar*` which matches 'foo/bar.txt'.
            let tokens: Vec<String> = pattern
                .split(|c: char| !c.is_alphanumeric())
                .filter(|s| !s.is_empty())
                .map(|t| format!("{}*", t.to_lowercase()))
                .collect();

            let mut stmt = conn.prepare(
                "SELECT f.path, f.size_bytes, u.name, s.version
                 FROM files_fts fts
                 JOIN files f ON f.rowid = fts.rowid
                 JOIN snapshots s ON s.id = f.snapshot_id
                 JOIN units u ON u.id = s.unit_id
                 WHERE files_fts MATCH ?1 AND f.is_directory = 0
                 ORDER BY rank
                 LIMIT ?2",
            )?;
            let rows: Vec<(String, i64, String, i64)> = if tokens.is_empty() {
                Vec::new()
            } else {
                stmt.query_map(params![tokens.join(" "), limit], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
            };

            if json_output {
                let json: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|(path, size, unit, ver)| {
                        serde_json::json!({"path": path, "size": size, "unit": unit, "version": ver})
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json).unwrap());
            } else if rows.is_empty() {
                println!("no files matching \"{pattern}\"");
            } else {
                for (path, size, unit, ver) in &rows {
                    println!("  {unit} v{ver}: {path} ({})", format_size(*size));
                }
                println!("{} result(s)", rows.len());
            }
        }

        CatalogCommands::Locate { unit } => {
            let unit_row = crate::db::queries::get_unit_by_name(conn, unit)?
                .ok_or_else(|| TapectlError::UnitNotFound(unit.clone()))?;

            let mut stmt = conn.prepare(
                "SELECT v.label, s.version, ss.num_slices, w.completed_at
                 FROM snapshots s
                 JOIN stage_sets ss ON ss.snapshot_id = s.id
                 JOIN writes w ON w.stage_set_id = ss.id
                 JOIN volumes v ON v.id = w.volume_id
                 WHERE s.unit_id = ?1 AND w.status = 'completed'
                 ORDER BY s.version DESC, v.label",
            )?;
            let rows: Vec<LocationRow> = stmt
                .query_map(params![unit_row.id], |row| {
                    Ok(LocationRow {
                        volume: row.get(0)?,
                        version: row.get(1)?,
                        slices: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                        written: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                let json: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|r| {
                        serde_json::json!({"volume": r.volume, "version": r.version, "slices": r.slices})
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json).unwrap());
            } else if rows.is_empty() {
                println!("unit \"{unit}\" not found on any volume");
            } else {
                println!("{}", Table::new(rows));
            }
        }

        CatalogCommands::Stats => {
            let unit_count: i64 =
                conn.query_row("SELECT COUNT(*) FROM units", [], |row| row.get(0))?;
            let snapshot_count: i64 =
                conn.query_row("SELECT COUNT(*) FROM snapshots", [], |row| row.get(0))?;
            let file_count: i64 =
                conn.query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
            let total_size: i64 = conn.query_row(
                "SELECT COALESCE(SUM(size_bytes), 0) FROM files WHERE is_directory = 0",
                [],
                |row| row.get(0),
            )?;
            let volume_count: i64 =
                conn.query_row("SELECT COUNT(*) FROM volumes", [], |row| row.get(0))?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "units": unit_count,
                        "snapshots": snapshot_count,
                        "files": file_count,
                        "total_size": total_size,
                        "volumes": volume_count,
                    })
                );
            } else {
                println!("Catalog statistics:");
                println!("  Units:     {unit_count}");
                println!("  Snapshots: {snapshot_count}");
                println!("  Files:     {file_count}");
                println!("  Total:     {}", format_size(total_size));
                println!("  Volumes:   {volume_count}");
            }
        }
    }
    Ok(())
}

fn format_size(bytes: i64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}
