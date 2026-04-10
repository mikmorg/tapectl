use clap::Subcommand;
use rusqlite::{params, Connection, OptionalExtension};
use tabled::{Table, Tabled};

use crate::db::events;
use crate::error::{Result, TapectlError};

#[derive(Subcommand, Debug)]
pub enum LocationCommands {
    /// Add a storage location
    Add {
        /// Location name (e.g., "home-rack", "parents-house")
        name: String,
        /// Description
        #[arg(long, short)]
        description: Option<String>,
    },
    /// List locations
    List,
    /// Show location details
    Info {
        /// Location name
        name: String,
    },
    /// Rename a location
    Rename {
        /// Current name
        current: String,
        /// New name
        new: String,
    },
}

#[derive(Tabled)]
struct LocationRow {
    #[tabled(rename = "Name")]
    name: String,
    #[tabled(rename = "Volumes")]
    volumes: i64,
    #[tabled(rename = "Description")]
    description: String,
}

pub fn run(conn: &Connection, command: &LocationCommands, json_output: bool) -> Result<()> {
    match command {
        LocationCommands::Add { name, description } => {
            conn.execute(
                "INSERT INTO locations (name, description) VALUES (?1, ?2)",
                params![name, description],
            )?;
            let id = conn.last_insert_rowid();
            events::log_created(conn, "location", id, name, None)?;
            if json_output {
                println!("{}", serde_json::json!({"id": id, "name": name}));
            } else {
                println!("location \"{name}\" added (id={id})");
            }
        }
        LocationCommands::List => {
            let mut stmt = conn.prepare(
                "SELECT l.name, l.description,
                        (SELECT COUNT(*) FROM volumes v WHERE v.location_id = l.id) as vol_count
                 FROM locations l ORDER BY l.name",
            )?;
            let rows: Vec<LocationRow> = stmt
                .query_map([], |row| {
                    Ok(LocationRow {
                        name: row.get(0)?,
                        description: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        volumes: row.get(2)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!(rows
                        .iter()
                        .map(|r| serde_json::json!({"name": r.name, "volumes": r.volumes}))
                        .collect::<Vec<_>>()))
                    .unwrap()
                );
            } else if rows.is_empty() {
                println!("no locations defined");
            } else {
                println!("{}", Table::new(rows));
            }
        }
        LocationCommands::Info { name } => {
            let (id, desc, created): (i64, Option<String>, String) = conn
                .query_row(
                    "SELECT id, description, created_at FROM locations WHERE name = ?1",
                    params![name],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .map_err(|_| TapectlError::Other(format!("location \"{name}\" not found")))?;

            let mut stmt = conn.prepare(
                "SELECT label, status FROM volumes WHERE location_id = ?1 ORDER BY label",
            )?;
            let volumes: Vec<(String, String)> = stmt
                .query_map(params![id], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"name": name, "description": desc, "volumes": volumes})
                );
            } else {
                println!("Location: {name}");
                if let Some(d) = &desc {
                    println!("  Description: {d}");
                }
                println!("  Created:     {created}");
                println!("  Volumes:     {}", volumes.len());
                for (label, status) in &volumes {
                    println!("    {label} [{status}]");
                }
            }
        }
        LocationCommands::Rename { current, new } => {
            let id: i64 = conn
                .query_row(
                    "SELECT id FROM locations WHERE name = ?1",
                    params![current],
                    |row| row.get(0),
                )
                .map_err(|_| TapectlError::Other(format!("location \"{current}\" not found")))?;
            conn.execute(
                "UPDATE locations SET name = ?1 WHERE id = ?2",
                params![new, id],
            )?;
            events::log_field_change(
                conn,
                "location",
                id,
                new,
                "renamed",
                "name",
                Some(current),
                new,
                None,
            )?;
            if json_output {
                println!("{}", serde_json::json!({"old": current, "new": new}));
            } else {
                println!("location \"{current}\" renamed to \"{new}\"");
            }
        }
    }
    Ok(())
}

/// Move a volume to a location (used by volume move command).
pub fn move_volume(conn: &Connection, volume_label: &str, location_name: &str) -> Result<()> {
    let vol_id: i64 = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            params![volume_label],
            |row| row.get(0),
        )
        .map_err(|_| TapectlError::VolumeNotFound(volume_label.to_string()))?;

    let loc_id: i64 = conn
        .query_row(
            "SELECT id FROM locations WHERE name = ?1",
            params![location_name],
            |row| row.get(0),
        )
        .map_err(|_| TapectlError::Other(format!("location \"{location_name}\" not found")))?;

    let old_loc: Option<i64> = conn.query_row(
        "SELECT location_id FROM volumes WHERE id = ?1",
        params![vol_id],
        |row| row.get(0),
    )?;

    // Record movement
    conn.execute(
        "INSERT INTO volume_movements (volume_id, from_location, to_location)
         VALUES (?1, ?2, ?3)",
        params![vol_id, old_loc, loc_id],
    )?;

    conn.execute(
        "UPDATE volumes SET location_id = ?1 WHERE id = ?2",
        params![loc_id, vol_id],
    )?;

    events::log_field_change(
        conn,
        "volume",
        vol_id,
        volume_label,
        "moved",
        "location",
        old_loc.map(|id| id.to_string()).as_deref(),
        location_name,
        None,
    )?;

    Ok(())
}
