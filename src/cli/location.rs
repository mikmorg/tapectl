use clap::Subcommand;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use tabled::{Table, Tabled};

use crate::db::events;
use crate::error::{Result, TapectlError};

#[derive(Subcommand, Debug)]
pub enum LocationCommands {
    /// Add a storage location
    Add {
        /// Location name (e.g., "home-rack", "parents-house")
        name: String,
        /// Description. For a warehouse this is where the endpoint or
        /// prefix goes (e.g. "s3://bucket/prefix") -- there is
        /// deliberately no separate URI column (issue #73).
        #[arg(long, short)]
        description: Option<String>,
        /// Kind of location (ADR-0006). A `shelf` holds physical
        /// cartridges; a `warehouse` is cold cloud storage that can only
        /// receive recorded deposits (`volume deposit add`).
        #[arg(long, default_value = "shelf", value_parser = ["shelf", "warehouse"])]
        kind: String,
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

#[derive(Tabled, Serialize)]
struct LocationRow {
    #[tabled(rename = "Name")]
    name: String,
    /// ADR-0006 location kind: `shelf` or `warehouse`. Shown because the
    /// two are operationally nothing alike -- one you can drive to.
    #[tabled(rename = "Kind")]
    kind: String,
    /// ADR-0011: `cartridges.location_id` is the single mechanism for
    /// where a physical cartridge is, and until now the location side had
    /// no reader for it at all (issue #157) -- a shelf could hold a
    /// cartridge nothing here would ever mention. Counts every status
    /// (retired cartridges still occupy the shelf); `move_together`
    /// already refuses a warehouse destination for a cartridge, so this
    /// is always 0 on a `warehouse`-kind location.
    #[tabled(rename = "Cartridges")]
    cartridges: i64,
    #[tabled(rename = "Volumes")]
    volumes: i64,
    #[tabled(rename = "Deposits")]
    deposits: i64,
    #[tabled(rename = "Description")]
    description: String,
}

/// `location list --json` shape. Every table column already has a JSON
/// counterpart here.
fn location_rows_to_json(rows: &[LocationRow]) -> serde_json::Value {
    serde_json::to_value(rows).unwrap()
}

/// Every location with its cartridge/volume/deposit counts, split out from
/// the printing so it is assertable in tests without capturing stdout --
/// the same discipline `cartridge_rows` uses in `src/cli/cartridge.rs`.
fn location_rows(conn: &Connection) -> Result<Vec<LocationRow>> {
    let mut stmt = conn.prepare(
        "SELECT l.name, l.description,
                (SELECT COUNT(*) FROM volumes v WHERE v.location_id = l.id) as vol_count,
                l.kind,
                (SELECT COUNT(*) FROM volume_deposits d WHERE d.location_id = l.id),
                (SELECT COUNT(*) FROM cartridges c WHERE c.location_id = l.id)
         FROM locations l ORDER BY l.name",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(LocationRow {
                name: row.get(0)?,
                description: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                volumes: row.get(2)?,
                kind: row.get(3)?,
                deposits: row.get(4)?,
                cartridges: row.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Every cartridge currently parked at a location, barcode order --
/// `location info`'s new Cartridges block (issue #157). Split out from the
/// printing for the same testability reason as [`location_rows`].
///
/// `current_vol` is a correlated subquery, not a join, on purpose: a
/// cartridge with nothing currently mounted on it (a blank spare, or one
/// whose last volume was unmounted) must still appear rather than vanish
/// the way an inner join would drop it. Mirrors `cartridge_rows`'s own
/// current-volume subquery in `src/cli/cartridge.rs` exactly, so the two
/// views of "what volume is on this cartridge" cannot drift apart.
///
/// Every cartridge status is included -- ADR-0011: a `retired_permanent`
/// cartridge is still physically on the shelf and still occupies space,
/// so a location audit that hid it would defeat its own purpose.
#[allow(clippy::type_complexity)]
fn cartridges_at(
    conn: &Connection,
    location_id: i64,
) -> Result<Vec<(String, String, Option<String>, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT c.barcode, c.status, c.serial_number,
                (SELECT v.label FROM cartridge_volumes cv
                 JOIN volumes v ON v.id = cv.volume_id
                 WHERE cv.cartridge_id = c.id AND cv.unmounted_at IS NULL
                 LIMIT 1) as current_vol
         FROM cartridges c
         WHERE c.location_id = ?1
         ORDER BY c.barcode",
    )?;
    let rows = stmt
        .query_map(params![location_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn run(conn: &Connection, command: &LocationCommands, json_output: bool) -> Result<()> {
    match command {
        LocationCommands::Add {
            name,
            description,
            kind,
        } => {
            conn.execute(
                "INSERT INTO locations (name, description, kind) VALUES (?1, ?2, ?3)",
                params![name, description, kind],
            )?;
            let id = conn.last_insert_rowid();
            events::log_created(conn, "location", id, name, None)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"id": id, "name": name, "kind": kind})
                );
            } else {
                println!("location \"{name}\" added (id={id}, kind={kind})");
            }
        }
        LocationCommands::List => {
            let rows = location_rows(conn)?;
            if json_output {
                // `description` is included because the table shows it and,
                // since #72, it is where a warehouse's `s3://bucket/prefix`
                // lives -- there is deliberately no separate URI column.
                // Omitting it cost the JSON consumer the one field that says
                // where the bytes actually are.
                println!(
                    "{}",
                    serde_json::to_string_pretty(&location_rows_to_json(&rows)).unwrap()
                );
            } else if rows.is_empty() {
                println!("no locations defined");
            } else {
                println!("{}", Table::new(rows));
            }
        }
        LocationCommands::Info { name } => {
            let (id, desc, created, kind): (i64, Option<String>, String, String) = conn
                .query_row(
                    "SELECT id, description, created_at, kind FROM locations WHERE name = ?1",
                    params![name],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .map_err(|_| TapectlError::Other(format!("location \"{name}\" not found")))?;

            // Issue #157: the ADR-0011 half of this view that never got
            // built. A cartridge can sit here with nothing currently
            // mounted on it (a blank spare, or a `cartridge move` that
            // moved zero volumes because none were mounted), so this must
            // report cartridges independently of the Volumes block below
            // rather than only ever showing volumes and inferring the
            // cartridge from them.
            let cartridges = cartridges_at(conn, id)?;

            let mut stmt = conn.prepare(
                "SELECT label, status FROM volumes WHERE location_id = ?1 ORDER BY label",
            )?;
            let volumes: Vec<(String, String)> = stmt
                .query_map(params![id], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            let mut dep_stmt = conn.prepare(
                "SELECT v.label, d.deposited_at, d.receipt, d.storage_class
                 FROM volume_deposits d JOIN volumes v ON v.id = d.volume_id
                 WHERE d.location_id = ?1 ORDER BY v.label",
            )?;
            #[allow(clippy::type_complexity)]
            let deposits: Vec<(String, String, Option<String>, Option<String>)> = dep_stmt
                .query_map(params![id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"name": name, "kind": kind, "description": desc,
                                       "cartridges": cartridges.iter().map(|(barcode, status, serial, vol)|
                                           serde_json::json!({"barcode": barcode, "status": status,
                                                              "serial_number": serial,
                                                              "volume": vol}))
                                           .collect::<Vec<_>>(),
                                       "volumes": volumes,
                                       "deposits": deposits.iter().map(|(label, at, receipt, class)|
                                           serde_json::json!({"volume": label, "deposited_at": at,
                                                              "receipt": receipt,
                                                              "storage_class": class}))
                                           .collect::<Vec<_>>()})
                );
            } else {
                println!("Location: {name}");
                println!("  Kind:        {kind}");
                if let Some(d) = &desc {
                    println!("  Description: {d}");
                }
                println!("  Created:     {created}");
                // ADR-0012: barcode is the label an operator reads off the
                // shelf; the chip serial (the cartridge's actual identity)
                // stays available in --json above rather than cluttering
                // this line.
                println!("  Cartridges:  {}", cartridges.len());
                for (barcode, status, _serial, vol) in &cartridges {
                    println!(
                        "    {barcode} [{status}] volume={}",
                        vol.as_deref().unwrap_or("(none)")
                    );
                }
                println!("  Volumes:     {}", volumes.len());
                for (label, status) in &volumes {
                    println!("    {label} [{status}]");
                }
                if !deposits.is_empty() {
                    println!("  Deposits:    {}", deposits.len());
                    for (label, at, receipt, class) in &deposits {
                        println!(
                            "    {label} deposited {at}{}{}",
                            receipt
                                .as_deref()
                                .map(|r| format!(" receipt={r}"))
                                .unwrap_or_default(),
                            class
                                .as_deref()
                                .map(|c| format!(" class={c}"))
                                .unwrap_or_default()
                        );
                    }
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

/// Everything one move touched: the cartridge that physically moved (when
/// there was one) and every volume that went with it.
#[derive(Debug)]
pub struct MoveOutcome {
    pub cartridge: Option<String>,
    pub volumes: Vec<String>,
}

/// The single mover behind BOTH `volume move` and `cartridge move`
/// (ADR-0011: "Both write the same two rows, so the cartridge's place and
/// its volumes' places cannot disagree").
///
/// There is deliberately no second implementation. A cartridge is the thing
/// that physically moves and the data goes with it, so whichever noun the
/// operator names, the same rows change: `cartridges.location_id`, and
/// `volumes.location_id` for every volume currently mounted on it. Two
/// writers would let a cartridge sit in `offsite` while its volume claimed
/// `home-rack` — the exact drift ADR-0011 removed the `offsite` STATUS to
/// prevent.
///
/// `volume_movements` gets a row per volume regardless of which command was
/// used, so a volume's movement history does not depend on which noun the
/// operator happened to type.
fn move_together(
    conn: &Connection,
    cartridge: Option<(i64, String)>,
    volumes: &[(i64, String)],
    location_name: &str,
) -> Result<MoveOutcome> {
    let (loc_id, loc_kind): (i64, String) = conn
        .query_row(
            "SELECT id, kind FROM locations WHERE name = ?1",
            params![location_name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| {
            let known = known_location_names(conn).unwrap_or_default();
            let list = if known.is_empty() {
                "no locations are defined yet — add one with `tapectl location add <NAME>`"
                    .to_string()
            } else {
                format!("known locations: {}", known.join(", "))
            };
            TapectlError::Other(format!("location \"{location_name}\" not found ({list})"))
        })?;

    // Issue #100 (fallout from #73). `volumes.location_id` answers exactly one
    // question — "where do I go to fetch this cartridge" — and the answer can
    // never be an S3 bucket. Recording a physical tape as sitting inside cold
    // cloud storage is an incoherent record, not merely an odd one.
    //
    // A warehouse copy is a DEPOSIT of an already-sealed volume, which is why
    // #73 gave it its own table rather than reusing this column: the two facts
    // are asymmetric (a cartridge has one location; a volume can have many
    // deposits, and a deposit never moves). See the header of
    // 007_warehouse_locations.sql.
    //
    // ADR-0011 adds `cartridges.location_id` as a second column this refusal
    // has to cover, and it covers it for the same reason and more literally:
    // a physical cartridge cannot be inside a bucket. This function is the
    // only production writer of EITHER column, so the one refusal still closes
    // the whole path.
    if loc_kind == "warehouse" {
        let hint = volumes
            .first()
            .map(|(_, label)| {
                format!(
                    "To record that a copy of this volume was uploaded to \
                     \"{location_name}\", use:\n    \
                     tapectl volume deposit add {label} --to {location_name}"
                )
            })
            .unwrap_or_else(|| {
                format!(
                    "A warehouse only ever receives RECORDED deposits of sealed volumes \
                     (`tapectl volume deposit add <LABEL> --to {location_name}`)."
                )
            });
        return Err(TapectlError::Other(format!(
            "\"{location_name}\" is a warehouse location, and a physical cartridge cannot \
             be moved into one — `location_id` records where to go to FETCH the \
             tape. {hint}"
        )));
    }

    // One transaction: a cartridge recorded in a new place while its volumes
    // still claim the old one is precisely the disagreement ADR-0011 exists to
    // make impossible, and a half-applied move would create it.
    let tx = conn.unchecked_transaction()?;

    if let Some((cart_id, barcode)) = &cartridge {
        let old_loc: Option<i64> = tx.query_row(
            "SELECT location_id FROM cartridges WHERE id = ?1",
            params![cart_id],
            |row| row.get(0),
        )?;
        tx.execute(
            "UPDATE cartridges SET location_id = ?1 WHERE id = ?2",
            params![loc_id, cart_id],
        )?;
        let old_loc_name = resolve_location_name(&tx, old_loc)?;
        events::log_field_change(
            &tx,
            "cartridge",
            *cart_id,
            barcode,
            "moved",
            "location",
            old_loc_name.as_deref(),
            location_name,
            None,
        )?;
    }

    for (vol_id, label) in volumes {
        let old_loc: Option<i64> = tx.query_row(
            "SELECT location_id FROM volumes WHERE id = ?1",
            params![vol_id],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO volume_movements (volume_id, from_location, to_location)
             VALUES (?1, ?2, ?3)",
            params![vol_id, old_loc, loc_id],
        )?;
        tx.execute(
            "UPDATE volumes SET location_id = ?1 WHERE id = ?2",
            params![loc_id, vol_id],
        )?;
        let old_loc_name = resolve_location_name(&tx, old_loc)?;
        events::log_field_change(
            &tx,
            "volume",
            *vol_id,
            label,
            "moved",
            "location",
            old_loc_name.as_deref(),
            location_name,
            None,
        )?;
    }

    tx.commit()?;

    Ok(MoveOutcome {
        cartridge: cartridge.map(|(_, barcode)| barcode),
        volumes: volumes.iter().map(|(_, label)| label.clone()).collect(),
    })
}

/// Resolves a location id to its current name, for logging into an audit
/// event's `old_value`/`new_value` (ADR-0012's consequences bullet: "Move
/// events carry location *names* on both sides, never an id on one and a
/// name on the other").
///
/// `.optional()` makes this tolerant of a stale id whose row is gone: a
/// move must never fail because history points at a location that no
/// longer exists. When the id is present but the name cannot be resolved,
/// this returns `None` rather than falling back to the id itself — the
/// ruling is that an id must never appear in these fields, not even as a
/// fallback.
///
/// Takes `&Connection` so it can be called with `&tx` (a `Transaction`
/// derefs to `Connection`) — callers must resolve the name from inside the
/// same transaction the move runs in, never a separate `conn` read, or a
/// concurrent rename could be read mid-move.
fn resolve_location_name(conn: &Connection, location_id: Option<i64>) -> Result<Option<String>> {
    let Some(id) = location_id else {
        return Ok(None);
    };
    Ok(conn
        .query_row(
            "SELECT name FROM locations WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()?)
}

fn known_location_names(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT name FROM locations ORDER BY name")?;
    let names = stmt
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<Vec<String>, _>>()?;
    Ok(names)
}

/// Every volume currently mounted on `cartridge_id`, in label order.
fn mounted_volumes(conn: &Connection, cartridge_id: i64) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT v.id, v.label FROM cartridge_volumes cv
         JOIN volumes v ON v.id = cv.volume_id
         WHERE cv.cartridge_id = ?1 AND cv.unmounted_at IS NULL
         ORDER BY v.label",
    )?;
    let rows = stmt
        .query_map(params![cartridge_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Move a volume to a location (used by `volume move`).
///
/// ADR-0011: the name and meaning are unchanged, and now it also moves the
/// cartridge the volume is bound to — plus any OTHER volume on that same
/// cartridge, because they are all one piece of plastic and it is the
/// plastic that travels. A volume with no binding (a warehouse deposit, an
/// export, a tape written before ADR-0010 taught `volume init` to bind)
/// moves alone, exactly as before.
pub fn move_volume(
    conn: &Connection,
    volume_label: &str,
    location_name: &str,
) -> Result<MoveOutcome> {
    let vol_id: i64 = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            params![volume_label],
            |row| row.get(0),
        )
        .map_err(|_| TapectlError::VolumeNotFound(volume_label.to_string()))?;

    let bound: Option<(i64, String)> = conn
        .query_row(
            "SELECT c.id, c.barcode FROM cartridge_volumes cv
             JOIN cartridges c ON c.id = cv.cartridge_id
             WHERE cv.volume_id = ?1 AND cv.unmounted_at IS NULL",
            params![vol_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    match bound {
        Some((cart_id, barcode)) => {
            // Every volume on that cartridge, not just this one: they share
            // the medium, so they share the shelf.
            let volumes = mounted_volumes(conn, cart_id)?;
            move_together(conn, Some((cart_id, barcode)), &volumes, location_name)
        }
        None => move_together(
            conn,
            None,
            &[(vol_id, volume_label.to_string())],
            location_name,
        ),
    }
}

/// Move a cartridge to a location (used by `cartridge move`).
///
/// ADR-0011: `offsite` left `cartridges.status` because it was never a
/// status — it is a place, and this is the writer for it. The cartridge is
/// the thing that physically moves; every volume on it goes along.
pub fn move_cartridge(
    conn: &Connection,
    barcode: &str,
    location_name: &str,
) -> Result<MoveOutcome> {
    let cart_id: i64 = conn
        .query_row(
            "SELECT id FROM cartridges WHERE barcode = ?1",
            params![barcode],
            |row| row.get(0),
        )
        .map_err(|_| TapectlError::Other(format!("cartridge \"{barcode}\" not found")))?;

    let volumes = mounted_volumes(conn, cart_id)?;
    move_together(
        conn,
        Some((cart_id, barcode.to_string())),
        &volumes,
        location_name,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `location list --json` shape (issue: C2 row-listing drift).
    /// `cartridges` is additive since issue #157 (ADR-0011's location-side
    /// reader), following the C2b discipline: raw count, same key on the
    /// table and in `--json`.
    #[test]
    fn pin_location_rows_json_shape() {
        let rows = vec![
            LocationRow {
                name: "home".to_string(),
                kind: "shelf".to_string(),
                cartridges: 2,
                volumes: 3,
                deposits: 0,
                description: "offsite".to_string(),
            },
            LocationRow {
                name: "glacier".to_string(),
                kind: "warehouse".to_string(),
                cartridges: 0,
                volumes: 0,
                deposits: 5,
                description: String::new(),
            },
        ];
        let value = location_rows_to_json(&rows);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"[{"cartridges":2,"deposits":0,"description":"offsite","kind":"shelf","name":"home","volumes":3},{"cartridges":0,"deposits":5,"description":"","kind":"warehouse","name":"glacier","volumes":0}]"#
        );
    }

    /// Seed one shelf location, one warehouse location, and a sealed volume.
    fn setup() -> Connection {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO locations (name, kind) VALUES ('home', 'shelf')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO locations (name, kind) VALUES ('glacier', 'warehouse')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES ('L6-0001', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
            [],
        )
        .unwrap();
        conn
    }

    /// Issue #100: `volumes.location_id` answers "where do I go to fetch this
    /// cartridge", and the answer can never be an S3 bucket. Moving a tape
    /// into a warehouse is an incoherent record, not merely an odd one.
    #[test]
    fn move_refuses_a_warehouse_destination() {
        let conn = setup();
        let err = move_volume(&conn, "L6-0001", "glacier")
            .expect_err("a cartridge cannot be moved into cold cloud storage");
        let msg = err.to_string();
        assert!(
            msg.contains("volume deposit add"),
            "the refusal must name the thing the operator probably meant; got: {msg}"
        );

        // And nothing was recorded — neither the location nor a movement.
        let loc: Option<i64> = conn
            .query_row(
                "SELECT location_id FROM volumes WHERE label = 'L6-0001'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(loc, None, "a refused move must not update location_id");
        let movements: i64 = conn
            .query_row("SELECT COUNT(*) FROM volume_movements", [], |r| r.get(0))
            .unwrap();
        assert_eq!(movements, 0, "a refused move must not log a movement");
    }

    /// The other direction, so the refusal cannot be "reject everything":
    /// a shelf destination still works exactly as before.
    #[test]
    fn move_to_a_shelf_still_succeeds_and_records_the_movement() {
        let conn = setup();
        move_volume(&conn, "L6-0001", "home").expect("a shelf is a valid destination");

        let loc_name: String = conn
            .query_row(
                "SELECT l.name FROM volumes v JOIN locations l ON l.id = v.location_id
                 WHERE v.label = 'L6-0001'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(loc_name, "home");
        let movements: i64 = conn
            .query_row("SELECT COUNT(*) FROM volume_movements", [], |r| r.get(0))
            .unwrap();
        assert_eq!(movements, 1);
    }

    // ---- ADR-0011: a cartridge's place is a location ----

    /// Seed `setup()` plus a cartridge holding `L6-0001` and a second volume
    /// `L6-0002` on the SAME cartridge — the multi-volume shape that makes
    /// "the cartridge is the thing that moves" visible.
    fn setup_bound() -> Connection {
        let conn = setup();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES ('L6-0002', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status)
             VALUES ('A001L6', 'LTO-6', 2500000000000, 'in_use')",
            [],
        )
        .unwrap();
        let cart_id = conn.last_insert_rowid();
        for label in ["L6-0001", "L6-0002"] {
            conn.execute(
                "INSERT INTO cartridge_volumes (cartridge_id, volume_id)
                 SELECT ?1, id FROM volumes WHERE label = ?2",
                params![cart_id, label],
            )
            .unwrap();
        }
        conn
    }

    fn location_of(conn: &Connection, table: &str, key_col: &str, key: &str) -> Option<String> {
        conn.query_row(
            &format!(
                "SELECT l.name FROM {table} t JOIN locations l ON l.id = t.location_id
                 WHERE t.{key_col} = ?1"
            ),
            params![key],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
    }

    /// ADR-0011's whole point: the cartridge's place and its volumes' places
    /// cannot disagree, because one writer sets both.
    #[test]
    fn cartridge_move_moves_the_cartridge_and_every_volume_on_it() {
        let conn = setup_bound();
        let outcome = move_cartridge(&conn, "A001L6", "home").unwrap();

        assert_eq!(outcome.cartridge.as_deref(), Some("A001L6"));
        assert_eq!(outcome.volumes, vec!["L6-0001", "L6-0002"]);
        assert_eq!(
            location_of(&conn, "cartridges", "barcode", "A001L6").as_deref(),
            Some("home")
        );
        for label in ["L6-0001", "L6-0002"] {
            assert_eq!(
                location_of(&conn, "volumes", "label", label).as_deref(),
                Some("home"),
                "volume {label} must move with the plastic it is written on"
            );
        }
        // Movement history does not depend on which noun the operator typed.
        let movements: i64 = conn
            .query_row("SELECT COUNT(*) FROM volume_movements", [], |r| r.get(0))
            .unwrap();
        assert_eq!(movements, 2);
    }

    /// An UNMOUNTED (displaced) volume is not on the cartridge any more, so
    /// it must not be dragged around by it. `cartridge_volumes.unmounted_at`
    /// is the discriminator every other path in this codebase uses.
    #[test]
    fn an_unmounted_volume_does_not_move_with_the_cartridge() {
        let conn = setup_bound();
        conn.execute(
            "UPDATE cartridge_volumes SET unmounted_at = datetime('now')
             WHERE volume_id = (SELECT id FROM volumes WHERE label = 'L6-0002')",
            [],
        )
        .unwrap();

        let outcome = move_cartridge(&conn, "A001L6", "home").unwrap();
        assert_eq!(outcome.volumes, vec!["L6-0001"]);
        assert_eq!(
            location_of(&conn, "volumes", "label", "L6-0002"),
            None,
            "a displaced volume's bytes are gone from this cartridge; it does not travel with it"
        );
    }

    /// ADR-0012's consequences bullet: "Move events carry location *names*
    /// on both sides, never an id on one and a name on the other." Move a
    /// cartridge that is ALREADY located (into "home" first) and then again
    /// (into "bank"), and check the second move's event: the old side must
    /// be the FIRST location's name, not its id.
    #[test]
    fn moving_an_already_located_cartridge_logs_the_previous_locations_name_not_its_id() {
        let conn = setup_bound();
        conn.execute(
            "INSERT INTO locations (name, kind) VALUES ('bank', 'shelf')",
            [],
        )
        .unwrap();

        move_cartridge(&conn, "A001L6", "home").unwrap();
        move_cartridge(&conn, "A001L6", "bank").unwrap();

        let old_value: Option<String> = conn
            .query_row(
                "SELECT old_value FROM events
                 WHERE entity_type = 'cartridge' AND action = 'moved'
                 ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            old_value.as_deref(),
            Some("home"),
            "the event's old value must be the previous location's NAME, not its id (ADR-0012)"
        );
    }

    /// A cartridge with no prior location (`location_id IS NULL`) must log
    /// `None` on the old side of its first move — never the id (there isn't
    /// one) and never some other stand-in.
    #[test]
    fn moving_an_unlocated_cartridge_logs_none_on_the_old_side() {
        let conn = setup_bound();
        move_cartridge(&conn, "A001L6", "home").unwrap();

        let old_value: Option<String> = conn
            .query_row(
                "SELECT old_value FROM events
                 WHERE entity_type = 'cartridge' AND action = 'moved'
                 ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            old_value, None,
            "a previously unlocated cartridge's first move must log None on the old side"
        );
    }

    /// ADR-0012's consequences bullet, proven for `volume move` too (not
    /// just `cartridge move`): both entity kinds go through `move_together`,
    /// but the fix could still have been wired at only one call site.
    #[test]
    fn volume_move_also_logs_the_previous_locations_name_not_its_id() {
        let conn = setup_bound();
        conn.execute(
            "INSERT INTO locations (name, kind) VALUES ('bank', 'shelf')",
            [],
        )
        .unwrap();

        move_volume(&conn, "L6-0001", "home").unwrap();
        move_volume(&conn, "L6-0001", "bank").unwrap();

        let (old_value, new_value): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT old_value, new_value FROM events
                 WHERE entity_type = 'volume' AND action = 'moved' AND entity_label = 'L6-0001'
                 ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            old_value.as_deref(),
            Some("home"),
            "a volume move's event must carry the previous location's NAME, not its id"
        );
        assert_eq!(new_value.as_deref(), Some("bank"));
    }

    /// The other direction (ADR-0011: "`volume move` keeps its name and
    /// meaning, and now also moves the cartridge the volume is bound to").
    #[test]
    fn volume_move_drags_the_cartridge_and_its_sibling_volume_along() {
        let conn = setup_bound();
        let outcome = move_volume(&conn, "L6-0001", "home").unwrap();

        assert_eq!(outcome.cartridge.as_deref(), Some("A001L6"));
        assert_eq!(outcome.volumes, vec!["L6-0001", "L6-0002"]);
        assert_eq!(
            location_of(&conn, "cartridges", "barcode", "A001L6").as_deref(),
            Some("home"),
            "the cartridge is the thing that physically moves"
        );
        assert_eq!(
            location_of(&conn, "volumes", "label", "L6-0002").as_deref(),
            Some("home"),
            "a sibling volume on the same cartridge cannot stay behind"
        );
    }

    /// The unbound shape stays exactly as it was: a volume with no cartridge
    /// — a pre-ADR-0010 tape, an export — moves alone and touches no
    /// cartridge row.
    #[test]
    fn volume_move_without_a_binding_moves_only_the_volume() {
        let conn = setup_bound();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES ('L6-LONE', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
            [],
        )
        .unwrap();

        let outcome = move_volume(&conn, "L6-LONE", "home").unwrap();
        assert!(outcome.cartridge.is_none());
        assert_eq!(outcome.volumes, vec!["L6-LONE"]);
        assert_eq!(
            location_of(&conn, "cartridges", "barcode", "A001L6"),
            None,
            "an unbound volume's move must not touch any cartridge"
        );
    }

    /// Issue #100's refusal now guards `cartridges.location_id` too — a
    /// physical cartridge cannot be inside an S3 bucket, and it is more
    /// literally true of the cartridge than of the volume.
    #[test]
    fn cartridge_move_refuses_a_warehouse_destination() {
        let conn = setup_bound();
        let err = move_cartridge(&conn, "A001L6", "glacier")
            .expect_err("a cartridge cannot be moved into cold cloud storage");
        assert!(
            err.to_string().contains("volume deposit add"),
            "the refusal must name the thing the operator probably meant; got: {err}"
        );
        assert_eq!(
            location_of(&conn, "cartridges", "barcode", "A001L6"),
            None,
            "a refused move must not update the cartridge's location"
        );
        assert_eq!(
            location_of(&conn, "volumes", "label", "L6-0001"),
            None,
            "a refused move must not update any volume's location either"
        );
    }

    /// An unknown destination names the ones that exist — the operator
    /// mistyped, and the fix is right there.
    #[test]
    fn an_unknown_location_names_the_known_ones() {
        let conn = setup_bound();
        let err = move_cartridge(&conn, "A001L6", "hom").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("glacier") && msg.contains("home"),
            "got: {msg}"
        );
    }

    #[test]
    fn moving_an_unknown_cartridge_says_so() {
        let conn = setup_bound();
        let err = move_cartridge(&conn, "NOPE", "home").unwrap_err();
        assert!(err.to_string().contains("NOPE"));
    }

    /// An unknown location must still be a not-found error, not the new
    /// warehouse refusal — the kind lookup must not swallow that case.
    #[test]
    fn move_to_an_unknown_location_still_reports_not_found() {
        let conn = setup();
        let err = move_volume(&conn, "L6-0001", "nowhere").expect_err("no such location");
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    // ---- issue #157: location-side cartridge visibility (ADR-0011) -------

    /// Reproduces the issue's failure mode directly: after `cartridge move
    /// <bc> --to <location>`, the cartridge must be visible from the
    /// location side even though nothing is currently mounted on it — the
    /// "Volumes: 0, no mention of the cartridge" defect the issue
    /// described.
    #[test]
    fn cartridges_at_shows_a_cartridge_with_no_mounted_volume() {
        let conn = setup();
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status, serial_number)
             VALUES ('BC001', 'LTO-6', 2500000000000, 'available', 'SER-BC001')",
            [],
        )
        .unwrap();
        move_cartridge(&conn, "BC001", "home").unwrap();

        let loc_id: i64 = conn
            .query_row("SELECT id FROM locations WHERE name = 'home'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let cartridges = cartridges_at(&conn, loc_id).unwrap();
        assert_eq!(
            cartridges.len(),
            1,
            "the cartridge must be visible from the location side"
        );
        assert_eq!(cartridges[0].0, "BC001");
        assert_eq!(cartridges[0].1, "available");
        assert_eq!(cartridges[0].2.as_deref(), Some("SER-BC001"));
        assert_eq!(cartridges[0].3, None, "nothing is currently mounted on it");
    }

    /// The other half: a cartridge WITH a currently mounted volume reports
    /// it, and an unmounted (displaced) volume never shows as current —
    /// the same `unmounted_at IS NULL` discriminator `cartridge_rows` uses.
    #[test]
    fn cartridges_at_reports_the_current_volume_and_ignores_unmounted_ones() {
        let conn = setup_bound();
        move_cartridge(&conn, "A001L6", "home").unwrap();
        conn.execute(
            "UPDATE cartridge_volumes SET unmounted_at = datetime('now')
             WHERE volume_id = (SELECT id FROM volumes WHERE label = 'L6-0002')",
            [],
        )
        .unwrap();

        let loc_id: i64 = conn
            .query_row("SELECT id FROM locations WHERE name = 'home'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let cartridges = cartridges_at(&conn, loc_id).unwrap();
        assert_eq!(cartridges.len(), 1);
        assert_eq!(
            cartridges[0].3.as_deref(),
            Some("L6-0001"),
            "L6-0002 was unmounted; L6-0001 is the current volume"
        );
    }

    /// ADR-0011: a retired cartridge is still physically on the shelf and
    /// must not be hidden from a location audit — the reader must not
    /// filter by status.
    #[test]
    fn cartridges_at_includes_a_retired_cartridge() {
        let conn = setup();
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status)
             VALUES ('RETIRED1', 'LTO-6', 2500000000000, 'retired_permanent')",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE cartridges SET location_id = (SELECT id FROM locations WHERE name = 'home')
             WHERE barcode = 'RETIRED1'",
            [],
        )
        .unwrap();

        let loc_id: i64 = conn
            .query_row("SELECT id FROM locations WHERE name = 'home'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let cartridges = cartridges_at(&conn, loc_id).unwrap();
        assert_eq!(cartridges.len(), 1);
        assert_eq!(cartridges[0].1, "retired_permanent");
    }

    /// The acceptance case from the issue: a location can hold cartridges
    /// while reporting zero volumes, and `location_rows` must show the
    /// cartridge count rather than implying nothing is there.
    #[test]
    fn location_rows_counts_cartridges_independently_of_volumes() {
        let conn = setup();
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status)
             VALUES ('BC001', 'LTO-6', 2500000000000, 'available')",
            [],
        )
        .unwrap();
        move_cartridge(&conn, "BC001", "home").unwrap();

        let rows = location_rows(&conn).unwrap();
        let home = rows.iter().find(|r| r.name == "home").unwrap();
        assert_eq!(home.cartridges, 1);
        assert_eq!(home.volumes, 0, "the cartridge has no mounted volume");
    }
}
