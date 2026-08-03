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
        /// Override the pending_erase lifecycle precondition (ADR-0008
        /// Tier 2 — see cli::consent)
        #[arg(long)]
        force: bool,
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

pub fn run(
    conn: &Connection,
    command: &CartridgeCommands,
    json_output: bool,
    yes: bool,
    dry_run: bool,
) -> Result<()> {
    match command {
        CartridgeCommands::Register {
            barcode,
            media_type,
            capacity,
            notes,
        } => {
            let cap = staging::parse_size_to_bytes(capacity)?;
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
            let rows = cartridge_rows(conn, status.as_deref())?;
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
        CartridgeCommands::MarkErased { barcode, force } => {
            crate::cli::operations::cartridge_mark_erased(
                conn,
                barcode,
                *force,
                yes,
                dry_run,
                json_output,
            )?;
        }
    }
    Ok(())
}

/// Cartridge listing rows, split out from the printing so they are assertable
/// in tests without capturing stdout — the same pattern as `report`'s
/// `dirty_rows` / `fire_risk_rows` / `copies_rows`.
///
/// Issue #110: the status filter used to be INTERPOLATED into the SQL
/// (`WHERE c.status = '{st}'`). It arrives from a clap arg on a
/// single-operator tool, so it was hygiene rather than a live exploit — but
/// every other query in this file binds, and one interpolated string is how
/// the habit erodes.
fn cartridge_rows(conn: &Connection, status: Option<&str>) -> Result<Vec<CartridgeRow>> {
    const SELECT: &str = "SELECT c.barcode, c.media_type, c.status, c.total_load_count,
                (SELECT v.label FROM cartridge_volumes cv
                 JOIN volumes v ON v.id = cv.volume_id
                 WHERE cv.cartridge_id = c.id AND cv.unmounted_at IS NULL
                 LIMIT 1) as current_vol
         FROM cartridges c";
    let sql = match status {
        Some(_) => format!("{SELECT} WHERE c.status = ?1 ORDER BY c.barcode"),
        None => format!("{SELECT} ORDER BY c.barcode"),
    };
    let mut stmt = conn.prepare(&sql)?;
    let bound: Vec<&dyn rusqlite::types::ToSql> = match &status {
        Some(st) => vec![st],
        None => vec![],
    };
    let rows = stmt
        .query_map(bound.as_slice(), |row| {
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
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed() -> Connection {
        let conn = crate::db::open_memory().unwrap();
        // Real statuses from the 001 CHECK constraint — an invented one is
        // rejected outright, which is the schema doing its job.
        for (bc, st) in [
            ("A001L6", "available"),
            ("A002L6", "retired_permanent"),
            ("A003L6", "available"),
        ] {
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, status, nominal_capacity)
                 VALUES (?1, 'LTO-6', ?2, 2500000000000)",
                rusqlite::params![bc, st],
            )
            .unwrap();
        }
        conn
    }

    /// Issue #110 item 2: the status filter is a bound parameter now. This
    /// proves it still FILTERS — a binding mistake that silently returned
    /// everything would look fine to a smoke test that only checked the
    /// command exits 0.
    #[test]
    fn status_filter_is_applied_and_bound() {
        let conn = seed();
        let rows = cartridge_rows(&conn, Some("available")).unwrap();
        assert_eq!(rows.len(), 2, "only the two available cartridges");
        assert!(rows.iter().all(|r| r.status == "available"));
    }

    #[test]
    fn no_filter_lists_everything() {
        let conn = seed();
        assert_eq!(cartridge_rows(&conn, None).unwrap().len(), 3);
    }

    /// A value containing a quote must be treated as data, not SQL. Under the
    /// old interpolation this string would have produced a syntax error or
    /// worse; bound, it simply matches nothing.
    #[test]
    fn a_quote_in_the_status_is_data_not_sql() {
        let conn = seed();
        let rows = cartridge_rows(&conn, Some("available' OR '1'='1")).unwrap();
        assert!(
            rows.is_empty(),
            "a quoted payload must match no rows, not inject (got {} rows)",
            rows.len()
        );
    }
}
