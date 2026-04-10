use clap::Subcommand;
use rusqlite::{params, Connection};
use tabled::{Table, Tabled};

use crate::config::Config;
use crate::db::events;
use crate::error::{Result, TapectlError};

#[derive(Subcommand, Debug)]
pub enum ArchiveSetCommands {
    /// Create a new archive set policy
    Create {
        /// Archive set name
        name: String,
        /// Minimum copy count
        #[arg(long)]
        min_copies: Option<i64>,
        /// Required locations (comma-separated)
        #[arg(long)]
        required_locations: Option<String>,
        /// Encryption enabled
        #[arg(long)]
        encrypt: Option<bool>,
        /// Compression mode
        #[arg(long)]
        compression: Option<String>,
        /// Checksum mode
        #[arg(long)]
        checksum_mode: Option<String>,
        /// Slice size (e.g., "2400G")
        #[arg(long)]
        slice_size: Option<String>,
        /// Verify interval in days
        #[arg(long)]
        verify_interval_days: Option<i64>,
        /// Description
        #[arg(long, short)]
        description: Option<String>,
    },

    /// Edit an existing archive set
    Edit {
        /// Archive set name
        name: String,
        /// Minimum copy count
        #[arg(long)]
        min_copies: Option<i64>,
        /// Required locations (comma-separated)
        #[arg(long)]
        required_locations: Option<String>,
        /// Encryption enabled
        #[arg(long)]
        encrypt: Option<bool>,
        /// Compression mode
        #[arg(long)]
        compression: Option<String>,
        /// Checksum mode
        #[arg(long)]
        checksum_mode: Option<String>,
        /// Slice size (e.g., "2400G")
        #[arg(long)]
        slice_size: Option<String>,
        /// Verify interval in days
        #[arg(long)]
        verify_interval_days: Option<i64>,
        /// Description
        #[arg(long, short)]
        description: Option<String>,
    },

    /// List archive sets
    List,

    /// Show archive set details
    Info {
        /// Archive set name
        name: String,
    },

    /// Sync archive sets from config.toml
    Sync,
}

#[derive(Tabled)]
struct ArchiveSetRow {
    #[tabled(rename = "Name")]
    name: String,
    #[tabled(rename = "Copies")]
    min_copies: String,
    #[tabled(rename = "Locations")]
    locations: String,
    #[tabled(rename = "Verify Days")]
    verify_days: String,
    #[tabled(rename = "Units")]
    unit_count: i64,
}

pub fn run(
    conn: &Connection,
    config: &Config,
    command: &ArchiveSetCommands,
    json_output: bool,
) -> Result<()> {
    match command {
        ArchiveSetCommands::Create {
            name,
            min_copies,
            required_locations,
            encrypt,
            compression,
            checksum_mode,
            slice_size,
            verify_interval_days,
            description,
        } => {
            let locations_json = required_locations.as_ref().map(|locs| {
                let arr: Vec<&str> = locs.split(',').map(|s| s.trim()).collect();
                serde_json::to_string(&arr).unwrap()
            });
            let slice_bytes = slice_size
                .as_ref()
                .map(|s| crate::staging::parse_size_to_bytes(s));

            conn.execute(
                "INSERT INTO archive_sets (name, description, min_copies, required_locations,
                 encrypt, compression, checksum_mode, slice_size, verify_interval_days)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    name,
                    description,
                    min_copies,
                    locations_json,
                    encrypt.map(|b| b as i64),
                    compression,
                    checksum_mode,
                    slice_bytes,
                    verify_interval_days,
                ],
            )?;
            let id = conn.last_insert_rowid();
            events::log_created(conn, "archive_set", id, name, None)?;

            if json_output {
                println!("{}", serde_json::json!({"id": id, "name": name}));
            } else {
                println!("archive set \"{name}\" created (id={id})");
            }
        }

        ArchiveSetCommands::Edit {
            name,
            min_copies,
            required_locations,
            encrypt,
            compression,
            checksum_mode,
            slice_size,
            verify_interval_days,
            description,
        } => {
            let id: i64 = conn
                .query_row(
                    "SELECT id FROM archive_sets WHERE name = ?1",
                    params![name],
                    |row| row.get(0),
                )
                .map_err(|_| TapectlError::Other(format!("archive set \"{name}\" not found")))?;

            if let Some(v) = min_copies {
                conn.execute(
                    "UPDATE archive_sets SET min_copies = ?1, updated_at = datetime('now') WHERE id = ?2",
                    params![v, id],
                )?;
            }
            if let Some(locs) = required_locations {
                let arr: Vec<&str> = locs.split(',').map(|s| s.trim()).collect();
                let json = serde_json::to_string(&arr).unwrap();
                conn.execute(
                    "UPDATE archive_sets SET required_locations = ?1, updated_at = datetime('now') WHERE id = ?2",
                    params![json, id],
                )?;
            }
            if let Some(v) = encrypt {
                conn.execute(
                    "UPDATE archive_sets SET encrypt = ?1, updated_at = datetime('now') WHERE id = ?2",
                    params![*v as i64, id],
                )?;
            }
            if let Some(v) = compression {
                conn.execute(
                    "UPDATE archive_sets SET compression = ?1, updated_at = datetime('now') WHERE id = ?2",
                    params![v, id],
                )?;
            }
            if let Some(v) = checksum_mode {
                conn.execute(
                    "UPDATE archive_sets SET checksum_mode = ?1, updated_at = datetime('now') WHERE id = ?2",
                    params![v, id],
                )?;
            }
            if let Some(v) = slice_size {
                let bytes = crate::staging::parse_size_to_bytes(v);
                conn.execute(
                    "UPDATE archive_sets SET slice_size = ?1, updated_at = datetime('now') WHERE id = ?2",
                    params![bytes, id],
                )?;
            }
            if let Some(v) = verify_interval_days {
                conn.execute(
                    "UPDATE archive_sets SET verify_interval_days = ?1, updated_at = datetime('now') WHERE id = ?2",
                    params![v, id],
                )?;
            }
            if let Some(v) = description {
                conn.execute(
                    "UPDATE archive_sets SET description = ?1, updated_at = datetime('now') WHERE id = ?2",
                    params![v, id],
                )?;
            }

            events::log_event(
                conn,
                "archive_set",
                id,
                Some(name),
                "edited",
                None,
                None,
                None,
                None,
                None,
            )?;

            if json_output {
                println!("{}", serde_json::json!({"name": name, "updated": true}));
            } else {
                println!("archive set \"{name}\" updated");
            }
        }

        ArchiveSetCommands::List => {
            let mut stmt = conn.prepare(
                "SELECT a.name, a.min_copies, a.required_locations, a.verify_interval_days,
                        (SELECT COUNT(*) FROM units u WHERE u.archive_set_id = a.id) as unit_count
                 FROM archive_sets a ORDER BY a.name",
            )?;
            let rows: Vec<ArchiveSetRow> = stmt
                .query_map([], |row| {
                    Ok(ArchiveSetRow {
                        name: row.get(0)?,
                        min_copies: row
                            .get::<_, Option<i64>>(1)?
                            .map(|n| n.to_string())
                            .unwrap_or("-".into()),
                        locations: row.get::<_, Option<String>>(2)?.unwrap_or("-".into()),
                        verify_days: row
                            .get::<_, Option<i64>>(3)?
                            .map(|n| n.to_string())
                            .unwrap_or("-".into()),
                        unit_count: row.get(4)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!(rows
                        .iter()
                        .map(|r| serde_json::json!({"name": r.name, "min_copies": r.min_copies, "units": r.unit_count}))
                        .collect::<Vec<_>>()))
                    .unwrap()
                );
            } else if rows.is_empty() {
                println!("no archive sets defined");
            } else {
                println!("{}", Table::new(rows));
            }
        }

        ArchiveSetCommands::Info { name } => {
            let (
                id,
                desc,
                min_copies,
                locs,
                encrypt,
                compression,
                checksum_mode,
                slice_size,
                verify_days,
                created,
                updated,
            ): (
                i64,
                Option<String>,
                Option<i64>,
                Option<String>,
                Option<i64>,
                Option<String>,
                Option<String>,
                Option<i64>,
                Option<i64>,
                String,
                String,
            ) = conn
                .query_row(
                    "SELECT id, description, min_copies, required_locations, encrypt,
                            compression, checksum_mode, slice_size, verify_interval_days,
                            created_at, updated_at
                     FROM archive_sets WHERE name = ?1",
                    params![name],
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
                            row.get(8)?,
                            row.get(9)?,
                            row.get(10)?,
                        ))
                    },
                )
                .map_err(|_| TapectlError::Other(format!("archive set \"{name}\" not found")))?;

            let unit_count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM units WHERE archive_set_id = ?1",
                params![id],
                |row| row.get(0),
            )?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "name": name, "description": desc, "min_copies": min_copies,
                        "required_locations": locs, "encrypt": encrypt,
                        "compression": compression, "checksum_mode": checksum_mode,
                        "slice_size": slice_size, "verify_interval_days": verify_days,
                        "units": unit_count,
                    })
                );
            } else {
                println!("Archive set: {name}");
                if let Some(d) = &desc {
                    println!("  Description:      {d}");
                }
                println!(
                    "  Min copies:       {}",
                    min_copies.map(|n| n.to_string()).unwrap_or("-".into())
                );
                println!("  Req. locations:   {}", locs.as_deref().unwrap_or("-"));
                println!(
                    "  Encrypt:          {}",
                    encrypt
                        .map(|n| if n != 0 { "yes" } else { "no" })
                        .unwrap_or("-")
                );
                println!(
                    "  Compression:      {}",
                    compression.as_deref().unwrap_or("-")
                );
                println!(
                    "  Checksum mode:    {}",
                    checksum_mode.as_deref().unwrap_or("-")
                );
                if let Some(sz) = slice_size {
                    println!("  Slice size:       {} GB", sz / (1024 * 1024 * 1024));
                }
                println!(
                    "  Verify interval:  {} days",
                    verify_days.map(|n| n.to_string()).unwrap_or("-".into())
                );
                println!("  Units using:      {unit_count}");
                println!("  Created:          {created}");
                println!("  Updated:          {updated}");
            }
        }

        ArchiveSetCommands::Sync => {
            let mut created = 0;
            let mut updated = 0;
            for as_cfg in &config.archive_sets {
                let locations_json = as_cfg
                    .required_locations
                    .as_ref()
                    .map(|locs| serde_json::to_string(locs).unwrap());
                let slice_bytes = as_cfg
                    .slice_size
                    .as_ref()
                    .map(|s| crate::staging::parse_size_to_bytes(s));
                let encrypt_int = as_cfg.encrypt.map(|b| b as i64);

                let existing: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM archive_sets WHERE name = ?1",
                        params![as_cfg.name],
                        |row| row.get(0),
                    )
                    .ok();

                if let Some(id) = existing {
                    conn.execute(
                        "UPDATE archive_sets SET min_copies = ?1, required_locations = ?2,
                         encrypt = ?3, compression = ?4, checksum_mode = ?5,
                         slice_size = ?6, verify_interval_days = ?7,
                         updated_at = datetime('now')
                         WHERE id = ?8",
                        params![
                            as_cfg.min_copies,
                            locations_json,
                            encrypt_int,
                            as_cfg.compression,
                            as_cfg.checksum_mode,
                            slice_bytes,
                            as_cfg.verify_interval_days,
                            id,
                        ],
                    )?;
                    updated += 1;
                } else {
                    conn.execute(
                        "INSERT INTO archive_sets (name, min_copies, required_locations,
                         encrypt, compression, checksum_mode, slice_size, verify_interval_days)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![
                            as_cfg.name,
                            as_cfg.min_copies,
                            locations_json,
                            encrypt_int,
                            as_cfg.compression,
                            as_cfg.checksum_mode,
                            slice_bytes,
                            as_cfg.verify_interval_days,
                        ],
                    )?;
                    let id = conn.last_insert_rowid();
                    events::log_created(conn, "archive_set", id, &as_cfg.name, None)?;
                    created += 1;
                }
            }
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"created": created, "updated": updated})
                );
            } else {
                println!("sync: {created} created, {updated} updated from config.toml");
            }
        }
    }
    Ok(())
}
