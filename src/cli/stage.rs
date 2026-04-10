use clap::Subcommand;
use rusqlite::{params, Connection};

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
}

pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    command: &StageCommands,
    json_output: bool,
) -> Result<()> {
    match command {
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
