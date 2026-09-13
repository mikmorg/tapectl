use clap::Subcommand;
use rusqlite::{params, Connection};
use serde::Serialize;
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
        /// Media generation (e.g., LTO-6, LTO-7, LTO-7-M8, LTO-8)
        #[arg(long)]
        media_type: String,
        /// Nominal capacity, e.g. "2500G". Defaults to the generation
        /// table's marketed figure (ADR-0010) when omitted — give this
        /// explicitly only when the physical cartridge really differs (a
        /// declared 40 TB LTO-10 cartridge, an mhvtl micro-tape, ...).
        #[arg(long)]
        capacity: Option<String>,
        /// Medium serial number (MAM), if already known — `volume init`
        /// records this itself when it auto-registers or matches a
        /// cartridge from a loaded tape's MAM; set it by hand only when
        /// pre-registering a cartridge that has not been loaded yet.
        #[arg(long)]
        serial: Option<String>,
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
    /// Move a cartridge to a location, taking its volumes with it
    ///
    /// ADR-0011: a cartridge's PLACE is a location, not a status. This moves
    /// `cartridges.location_id` and the `location_id` of every volume
    /// currently on the cartridge, in one transaction, so the shelf and the
    /// catalog cannot disagree. `volume move` does the same from the other
    /// end.
    Move {
        /// Barcode
        barcode: String,
        /// Destination location name
        #[arg(long)]
        to: String,
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

#[derive(Tabled, Serialize)]
struct CartridgeRow {
    #[tabled(rename = "Barcode")]
    barcode: String,
    /// Table-only until CTO decision 2026-09-11 (architecture review C2
    /// follow-up, C2b). JSON key is the field's own name, matching
    /// `cartridge info --json`'s existing "media_type" key for the same
    /// column (see `CartridgeCommands::Info` below).
    #[tabled(rename = "Type")]
    media_type: String,
    #[tabled(rename = "Status")]
    status: String,
    /// Table-only until CTO decision 2026-09-11 (architecture review C2
    /// follow-up, C2b). Raw `total_load_count` (matches `cartridge info
    /// --json`'s "loads" key for the same column).
    #[tabled(rename = "Loads", display_with = "display_opt_i64")]
    loads: Option<i64>,
    /// Table-only until CTO decision 2026-09-11 (architecture review C2
    /// follow-up, C2b).
    #[tabled(rename = "Volume", display_with = "display_opt_string")]
    volume: Option<String>,
}

fn display_opt_i64(v: &Option<i64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_default()
}

fn display_opt_string(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

/// `cartridge list --json` shape. `media_type`/`loads`/`volume` were
/// table-only until CTO decision 2026-09-11 (architecture review C2
/// follow-up, C2b).
fn cartridge_rows_to_json(rows: &[CartridgeRow]) -> serde_json::Value {
    serde_json::to_value(rows).unwrap()
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
            serial,
            notes,
        } => {
            // ADR-0010: stored canonical, not the operator's raw spelling,
            // so a later comparison against a detected generation
            // (`volume init`) is a plain string match.
            let generation = crate::media::Generation::parse(media_type).ok_or_else(|| {
                TapectlError::Other(format!(
                    "{media_type:?} is not a recognised LTO generation \
                     (e.g. LTO-6, LTO-7, LTO-7-M8, LTO-8)"
                ))
            })?;
            let canonical_media_type = generation.as_str();
            let (cap, capacity_display) = match capacity {
                Some(c) => (staging::parse_size_to_bytes(c)?, c.clone()),
                None => {
                    let bytes = generation.native_capacity_bytes();
                    (
                        bytes as i64,
                        format!("{bytes} bytes, the {canonical_media_type} default"),
                    )
                }
            };
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number, notes)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![barcode, canonical_media_type, cap, serial, notes],
            )?;
            let id = conn.last_insert_rowid();
            events::log_created(conn, "cartridge", id, barcode, None)?;
            if json_output {
                println!("{}", serde_json::json!({"id": id, "barcode": barcode}));
            } else {
                println!(
                    "cartridge \"{barcode}\" registered (id={id}, {canonical_media_type}, {capacity_display})"
                );
            }
        }
        CartridgeCommands::List { status } => {
            let rows = cartridge_rows(conn, status.as_deref())?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&cartridge_rows_to_json(&rows)).unwrap()
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
        CartridgeCommands::Move { barcode, to } => {
            let outcome = crate::cli::location::move_cartridge(conn, barcode, to)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "barcode": barcode,
                        "location": to,
                        "volumes_moved": outcome.volumes,
                    })
                );
            } else {
                println!("cartridge \"{barcode}\" moved to \"{to}\"");
                match outcome.volumes.len() {
                    0 => println!("  no volumes on this cartridge"),
                    n => println!(
                        "  {n} volume(s) moved with it: {}",
                        outcome.volumes.join(", ")
                    ),
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
                loads: row.get::<_, Option<i64>>(3)?,
                volume: row.get::<_, Option<String>>(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cartridge list --json` shape (issue: C2 row-listing drift).
    /// `media_type`/`loads`/`volume` are additive since CTO decision
    /// 2026-09-11 (architecture review C2 follow-up, C2b).
    #[test]
    fn pin_cartridge_rows_json_shape() {
        let rows = vec![
            CartridgeRow {
                barcode: "A001L6".to_string(),
                media_type: "LTO-6".to_string(),
                status: "available".to_string(),
                loads: Some(12),
                volume: Some("L6-0001".to_string()),
            },
            CartridgeRow {
                barcode: "A002L6".to_string(),
                media_type: "LTO-6".to_string(),
                status: "retired_permanent".to_string(),
                loads: None,
                volume: None,
            },
        ];
        let value = cartridge_rows_to_json(&rows);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"[{"barcode":"A001L6","loads":12,"media_type":"LTO-6","status":"available","volume":"L6-0001"},{"barcode":"A002L6","loads":null,"media_type":"LTO-6","status":"retired_permanent","volume":null}]"#
        );
    }

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

    // ---- ADR-0010: `cartridge register` ----

    fn register(
        conn: &Connection,
        barcode: &str,
        media_type: &str,
        capacity: Option<&str>,
        serial: Option<&str>,
    ) -> Result<()> {
        run(
            conn,
            &CartridgeCommands::Register {
                barcode: barcode.to_string(),
                media_type: media_type.to_string(),
                capacity: capacity.map(str::to_string),
                serial: serial.map(str::to_string),
                notes: None,
            },
            false,
            true,
            false,
        )
    }

    fn stored_row(conn: &Connection, barcode: &str) -> (String, i64, Option<String>) {
        conn.query_row(
            "SELECT media_type, nominal_capacity, serial_number FROM cartridges WHERE barcode = ?1",
            rusqlite::params![barcode],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }

    #[test]
    fn register_stores_the_canonical_generation_spelling() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "lto6", None, None).unwrap();
        let (media_type, ..) = stored_row(&conn, "B001");
        assert_eq!(media_type, "LTO-6");
    }

    #[test]
    fn register_rejects_an_unrecognised_generation() {
        let conn = crate::db::open_memory().unwrap();
        let err = register(&conn, "B001", "not-a-generation", None, None).unwrap_err();
        assert!(err.to_string().contains("not-a-generation"));
    }

    #[test]
    fn register_without_capacity_defaults_from_the_generation_table() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, None).unwrap();
        let (_, cap, _) = stored_row(&conn, "B001");
        assert_eq!(
            cap,
            crate::media::Generation::Lto6.native_capacity_bytes() as i64
        );
    }

    #[test]
    fn register_with_explicit_capacity_overrides_the_table() {
        let conn = crate::db::open_memory().unwrap();
        // A declared 40 TB LTO-10 cartridge (ADR-0010 explicitly calls this
        // out: a single generation figure cannot express both LTO-10
        // capacities, so the operator states it here).
        register(&conn, "B001", "LTO-10", Some("40000G"), None).unwrap();
        let (_, cap, _) = stored_row(&conn, "B001");
        assert_eq!(cap, staging::parse_size_to_bytes("40000G").unwrap());
    }

    #[test]
    fn register_persists_an_explicit_serial() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, Some("EW7VWMVKF6")).unwrap();
        let (_, _, serial) = stored_row(&conn, "B001");
        assert_eq!(serial.as_deref(), Some("EW7VWMVKF6"));
    }

    #[test]
    fn register_without_serial_leaves_it_null() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, None).unwrap();
        let (_, _, serial) = stored_row(&conn, "B001");
        assert_eq!(serial, None);
    }
}
