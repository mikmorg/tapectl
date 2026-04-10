use clap::Subcommand;
use rusqlite::{params, Connection};
use tabled::{Table, Tabled};

use crate::db::events;
use crate::error::{Result, TapectlError};
use crate::staging;

#[derive(Subcommand, Debug)]
pub enum CartridgeCommands {
    /// Register a physical cartridge
    Register {
        /// Barcode label
        #[arg(long)]
        barcode: String,
        /// Media type (e.g., LTO-6, LTO-7, LTO-8)
        #[arg(long)]
        media_type: String,
        /// Nominal capacity (e.g., "2500G")
        #[arg(long, default_value = "2500G")]
        capacity: String,
        /// Notes
        #[arg(long)]
        notes: Option<String>,
    },
    /// List cartridges
    List {
        /// Filter by status
        #[arg(long)]
        status: Option<String>,
    },
    /// Show cartridge details
    Info {
        /// Barcode
        barcode: String,
    },
    /// Mark a cartridge as erased (available for reuse)
    MarkErased {
        /// Barcode
        barcode: String,
    },
}

#[derive(Tabled)]
struct CartridgeRow {
    #[tabled(rename = "Barcode")]
    barcode: String,
    #[tabled(rename = "Type")]
    media_type: String,
    #[tabled(rename = "Status")]
    status: String,
    #[tabled(rename = "Loads")]
    loads: String,
    #[tabled(rename = "Volume")]
    volume: String,
}

pub fn run(conn: &Connection, command: &CartridgeCommands, json_output: bool) -> Result<()> {
    match command {
        CartridgeCommands::Register {
            barcode,
            media_type,
            capacity,
            notes,
        } => {
            let cap = staging::parse_size_to_bytes(capacity);
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, notes)
                 VALUES (?1, ?2, ?3, ?4)",
                params![barcode, media_type, cap, notes],
            )?;
            let id = conn.last_insert_rowid();
            events::log_created(conn, "cartridge", id, barcode, None)?;
            if json_output {
                println!("{}", serde_json::json!({"id": id, "barcode": barcode}));
            } else {
                println!("cartridge \"{barcode}\" registered (id={id}, {media_type}, {capacity})");
            }
        }
        CartridgeCommands::List { status } => {
            let sql = if let Some(st) = status {
                format!(
                    "SELECT c.barcode, c.media_type, c.status, c.total_load_count,
                            (SELECT v.label FROM cartridge_volumes cv
                             JOIN volumes v ON v.id = cv.volume_id
                             WHERE cv.cartridge_id = c.id AND cv.unmounted_at IS NULL
                             LIMIT 1) as current_vol
                     FROM cartridges c WHERE c.status = '{st}' ORDER BY c.barcode"
                )
            } else {
                "SELECT c.barcode, c.media_type, c.status, c.total_load_count,
                        (SELECT v.label FROM cartridge_volumes cv
                         JOIN volumes v ON v.id = cv.volume_id
                         WHERE cv.cartridge_id = c.id AND cv.unmounted_at IS NULL
                         LIMIT 1) as current_vol
                 FROM cartridges c ORDER BY c.barcode"
                    .to_string()
            };
            let mut stmt = conn.prepare(&sql)?;
            let rows: Vec<CartridgeRow> = stmt
                .query_map([], |row| {
                    Ok(CartridgeRow {
                        barcode: row.get(0)?,
                        media_type: row.get(1)?,
                        status: row.get(2)?,
                        loads: row
                            .get::<_, Option<i64>>(3)?
                            .map(|n| n.to_string())
                            .unwrap_or_default(),
                        volume: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!(rows
                        .iter()
                        .map(|r| serde_json::json!({"barcode": r.barcode, "status": r.status}))
                        .collect::<Vec<_>>()))
                    .unwrap()
                );
            } else if rows.is_empty() {
                println!("no cartridges registered");
            } else {
                println!("{}", Table::new(rows));
            }
        }
        CartridgeCommands::Info { barcode } => {
            let (id, media, status, loads, cap, created, notes): (i64, String, String, Option<i64>, i64, String, Option<String>) = conn
                .query_row(
                    "SELECT id, media_type, status, total_load_count, nominal_capacity, created_at, notes
                     FROM cartridges WHERE barcode = ?1",
                    params![barcode],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
                )
                .map_err(|_| TapectlError::Other(format!("cartridge \"{barcode}\" not found")))?;

            // Get volume history
            let mut stmt = conn.prepare(
                "SELECT v.label, cv.mounted_at, cv.unmounted_at
                 FROM cartridge_volumes cv
                 JOIN volumes v ON v.id = cv.volume_id
                 WHERE cv.cartridge_id = ?1
                 ORDER BY cv.mounted_at DESC",
            )?;
            let volumes: Vec<(String, String, Option<String>)> = stmt
                .query_map(params![id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"barcode": barcode, "media_type": media, "status": status, "loads": loads, "volumes": volumes.len()})
                );
            } else {
                println!("Cartridge: {barcode}");
                println!("  Type:     {media}");
                println!("  Status:   {status}");
                println!("  Loads:    {}", loads.unwrap_or(0));
                println!("  Capacity: {} GB", cap / (1024 * 1024 * 1024));
                println!("  Created:  {created}");
                if let Some(n) = &notes {
                    println!("  Notes:    {n}");
                }
                if !volumes.is_empty() {
                    println!("  Volume history:");
                    for (label, mounted, unmounted) in &volumes {
                        let status = if unmounted.is_some() {
                            "unmounted"
                        } else {
                            "current"
                        };
                        println!("    {label} ({status}, mounted {mounted})");
                    }
                }
            }
        }
        CartridgeCommands::MarkErased { barcode } => {
            let id: i64 = conn
                .query_row(
                    "SELECT id FROM cartridges WHERE barcode = ?1",
                    params![barcode],
                    |row| row.get(0),
                )
                .map_err(|_| TapectlError::Other(format!("cartridge \"{barcode}\" not found")))?;

            conn.execute(
                "UPDATE cartridges SET status = 'available' WHERE id = ?1",
                params![id],
            )?;

            // Unmount any current volume
            conn.execute(
                "UPDATE cartridge_volumes SET unmounted_at = datetime('now')
                 WHERE cartridge_id = ?1 AND unmounted_at IS NULL",
                params![id],
            )?;

            events::log_field_change(
                conn,
                "cartridge",
                id,
                barcode,
                "erased",
                "status",
                None,
                "available",
                None,
            )?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"barcode": barcode, "status": "available"})
                );
            } else {
                println!("cartridge \"{barcode}\" marked as erased (available for reuse)");
            }
        }
    }
    Ok(())
}
