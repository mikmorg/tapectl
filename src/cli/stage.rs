use clap::Subcommand;
use rusqlite::{params, Connection};
use tabled::{Table, Tabled};

use crate::config::{Config, TapectlPaths};
use crate::error::{Result, TapectlError};
use crate::staging;

#[derive(Subcommand, Debug)]
pub enum StageCommands {
    /// Create staged slices (validate → dar → encrypt → checksums)
    Create {
        /// Unit name
        name: String,
    },

    /// List stage sets
    List {
        /// Filter by status (staging, staged, failed, cleaned)
        #[arg(long)]
        status: Option<String>,
    },

    /// Show details for a stage set
    Info {
        /// Unit name
        name: String,
        /// Snapshot version
        #[arg(long)]
        version: i64,
    },
}

#[derive(Tabled)]
struct StageRow {
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
    #[tabled(rename = "Encrypted")]
    encrypted_size: String,
    #[tabled(rename = "Staged At")]
    staged_at: String,
}

pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    command: &StageCommands,
    json_output: bool,
) -> Result<()> {
    match command {
        StageCommands::List { status } => {
            let mut sql = String::from(
                "SELECT ss.id, u.name, s.version, ss.status, ss.num_slices,
                        ss.total_encrypted_size, ss.staged_at
                 FROM stage_sets ss
                 JOIN snapshots s ON s.id = ss.snapshot_id
                 JOIN units u ON u.id = s.unit_id
                 WHERE 1=1",
            );
            let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            if let Some(st) = status {
                sql.push_str(" AND ss.status = ?");
                param_values.push(Box::new(st.clone()));
            }
            sql.push_str(" ORDER BY ss.created_at DESC");

            let params_ref: Vec<&dyn rusqlite::types::ToSql> =
                param_values.iter().map(|p| p.as_ref()).collect();
            let mut stmt = conn.prepare(&sql)?;
            let rows: Vec<StageRow> = stmt
                .query_map(params_ref.as_slice(), |row| {
                    let enc_size: Option<i64> = row.get(5)?;
                    Ok(StageRow {
                        id: row.get(0)?,
                        unit: row.get(1)?,
                        version: row.get(2)?,
                        status: row.get(3)?,
                        slices: row
                            .get::<_, Option<i64>>(4)?
                            .map(|n| n.to_string())
                            .unwrap_or_default(),
                        encrypted_size: enc_size
                            .map(|s| format!("{} MB", s / (1024 * 1024)))
                            .unwrap_or_default(),
                        staged_at: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!(rows
                        .iter()
                        .map(|r| serde_json::json!({
                            "id": r.id, "unit": r.unit, "version": r.version,
                            "status": r.status, "slices": r.slices,
                        }))
                        .collect::<Vec<_>>()))
                    .unwrap()
                );
            } else if rows.is_empty() {
                println!("no stage sets found");
            } else {
                println!("{}", Table::new(rows));
            }
        }

        StageCommands::Info { name, version } => {
            let unit = crate::db::queries::get_unit_by_name(conn, name)?
                .ok_or_else(|| TapectlError::UnitNotFound(name.clone()))?;

            let (ss_id, status, dar_ver, dar_cmd, num_slices, dar_size, enc_size, staged_at): (
                i64,
                String,
                Option<String>,
                Option<String>,
                Option<i64>,
                Option<i64>,
                Option<i64>,
                Option<String>,
            ) = conn
                .query_row(
                    "SELECT ss.id, ss.status, ss.dar_version, ss.dar_command,
                            ss.num_slices, ss.total_dar_size, ss.total_encrypted_size, ss.staged_at
                     FROM stage_sets ss
                     JOIN snapshots s ON s.id = ss.snapshot_id
                     WHERE s.unit_id = ?1 AND s.version = ?2
                     ORDER BY ss.created_at DESC LIMIT 1",
                    params![unit.id, version],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                        ))
                    },
                )
                .map_err(|_| {
                    TapectlError::Other(format!("no stage set for \"{name}\" v{version}"))
                })?;

            // Get slices
            let mut stmt = conn.prepare(
                "SELECT slice_number, size_bytes, encrypted_bytes, sha256_encrypted
                 FROM stage_slices WHERE stage_set_id = ?1 ORDER BY slice_number",
            )?;
            let slices: Vec<(i64, i64, Option<i64>, Option<String>)> = stmt
                .query_map(params![ss_id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                let slice_json: Vec<serde_json::Value> = slices
                    .iter()
                    .map(|(num, size, enc, sha)| {
                        serde_json::json!({
                            "slice": num, "dar_bytes": size,
                            "encrypted_bytes": enc, "sha256": sha,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({
                        "unit": name, "version": version, "stage_set_id": ss_id,
                        "status": status, "dar_version": dar_ver,
                        "num_slices": num_slices, "total_dar_size": dar_size,
                        "total_encrypted_size": enc_size, "slices": slice_json,
                    })
                );
            } else {
                println!("Stage set for {name} v{version} (id={ss_id})");
                println!("  Status:    {status}");
                if let Some(dv) = &dar_ver {
                    println!("  dar:       {dv}");
                }
                if let Some(dc) = &dar_cmd {
                    println!("  command:   {dc}");
                }
                println!("  Slices:    {}", num_slices.unwrap_or(0));
                println!("  dar size:  {} MB", dar_size.unwrap_or(0) / (1024 * 1024));
                println!("  encrypted: {} MB", enc_size.unwrap_or(0) / (1024 * 1024));
                if let Some(sa) = &staged_at {
                    println!("  Staged at: {sa}");
                }
                if !slices.is_empty() {
                    println!("  Slices:");
                    for (num, size, enc, sha) in &slices {
                        println!(
                            "    #{num}: {} MB dar, {} MB enc, sha256={}",
                            size / (1024 * 1024),
                            enc.unwrap_or(0) / (1024 * 1024),
                            sha.as_deref().unwrap_or("(none)"),
                        );
                    }
                }
            }
        }

        StageCommands::Create { name } => {
            // Find the latest 'created' snapshot for this unit
            let unit = crate::db::queries::get_unit_by_name(conn, name)?
                .ok_or_else(|| TapectlError::UnitNotFound(name.clone()))?;

            let snapshot_id: i64 = conn
                .query_row(
                    "SELECT id FROM snapshots WHERE unit_id = ?1 AND status = 'created'
                     ORDER BY version DESC LIMIT 1",
                    params![unit.id],
                    |row| row.get(0),
                )
                .map_err(|_| {
                    TapectlError::Other(format!(
                        "no unstaged snapshot for unit \"{name}\" — run `tapectl snapshot create` first"
                    ))
                })?;

            let stage_set_id = staging::stage_create(conn, paths, config, snapshot_id)?;

            // Fetch results for display
            let (num_slices, total_dar, total_enc): (Option<i64>, Option<i64>, Option<i64>) = conn
                .query_row(
                    "SELECT num_slices, total_dar_size, total_encrypted_size
                     FROM stage_sets WHERE id = ?1",
                    params![stage_set_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "stage_set_id": stage_set_id,
                        "unit": name,
                        "num_slices": num_slices,
                        "total_dar_size": total_dar,
                        "total_encrypted_size": total_enc,
                    })
                );
            } else {
                println!(
                    "staged: {} ({} slices, {} MB dar, {} MB encrypted)",
                    name,
                    num_slices.unwrap_or(0),
                    total_dar.unwrap_or(0) / (1024 * 1024),
                    total_enc.unwrap_or(0) / (1024 * 1024),
                );
            }
        }
    }
    Ok(())
}
