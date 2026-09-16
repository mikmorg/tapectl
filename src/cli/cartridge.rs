use clap::Subcommand;
use rusqlite::{params, Connection, OptionalExtension};
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
        generation: String,
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
        /// Filter by status (available, in_use, pending_erase,
        /// retired_permanent). `offsite` was removed by ADR-0011 -- a
        /// cartridge's place is a location now; use --location.
        #[arg(long)]
        status: Option<String>,
        /// Filter by physical location name (issue #157) -- the sibling
        /// of `--status`, same bound-parameter discipline (issue #110).
        #[arg(long)]
        location: Option<String>,
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
    /// Retire a cartridge permanently — it must never be written again
    ///
    /// ADR-0011: for wear, read errors, or any judgement that the medium is
    /// no longer fit to hold data. This is NOT an erasure and NOT a location
    /// change: the bytes may still be readable, but nothing will ever write
    /// to this cartridge again, and `volume init` refuses to bind it.
    /// `cartridge mark-erased` is the only way back.
    ///
    /// ADR-0008 Tier 2: the coverage impact is displayed first, and
    /// `--force`/`--yes` is required when a unit is left below its policy.
    Retire {
        /// Barcode
        barcode: String,
        /// Why (appended to the cartridge's notes, never overwriting them)
        #[arg(long)]
        reason: Option<String>,
        /// Proceed even when a unit is left with no other copy (ADR-0008
        /// Tier 2 — see cli::consent)
        #[arg(long)]
        force: bool,
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
    /// Correct a cartridge's barcode label
    ///
    /// ADR-0012: a cartridge's identity is its chip serial; the barcode is a
    /// relabelable sticker. This is a Tier 1 label correction under
    /// ADR-0008, not a destructive act — no prompt, no `--force`, no status
    /// gate — and it is the remedy `volume init`'s auto-register refuses
    /// with when the loaded medium's serial collides with an
    /// already-registered barcode.
    Relabel {
        /// Current barcode
        barcode: String,
        /// New barcode
        new_barcode: String,
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
    /// ADR-0011: the question an operator asks of a cartridge most often
    /// ("where is it?") and could not ask before — `cartridges.location_id`
    /// had no reader. Empty when the cartridge has never been placed.
    /// ADDITIVE in `--json`, per the C2b discipline noted above.
    #[tabled(rename = "Location", display_with = "display_opt_string")]
    location: Option<String>,
    /// Table-only until CTO decision 2026-09-11 (architecture review C2
    /// follow-up, C2b). Raw `total_load_count` (matches `cartridge info
    /// --json`'s "loads" key for the same column). Since issue #184,
    /// `None` here is a REAL state (never observed via MAM), not merely
    /// LEFT JOIN defensiveness -- `--json` already serialised it as `null`
    /// (`pin_cartridge_rows_json_shape` below), but every real row used to
    /// come back `Some` (the column's unused `DEFAULT 0`), so a consumer
    /// could previously treat this key as always-a-number in practice. It
    /// can now genuinely be `null`.
    #[tabled(rename = "Loads", display_with = "display_opt_i64")]
    loads: Option<i64>,
    /// Table-only until CTO decision 2026-09-11 (architecture review C2
    /// follow-up, C2b).
    #[tabled(rename = "Volume", display_with = "display_opt_string")]
    volume: Option<String>,
}

/// Renders a load count. `None` means the MAM load count has never been
/// observed for this cartridge (issue #184) -- rendered as the word
/// "unknown", the one spelling used here and in `cartridge info`'s
/// plain-text render, never as blank (indistinguishable from a stripped 0)
/// or as `0` (a false claim about wear).
fn display_opt_i64(v: &Option<i64>) -> String {
    v.map(|n| n.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn display_opt_string(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

/// `cartridge list --json` shape. `media_type`/`loads`/`volume` were
/// table-only until CTO decision 2026-09-11 (architecture review C2
/// follow-up, C2b); `location` is additive since ADR-0011.
fn cartridge_rows_to_json(rows: &[CartridgeRow]) -> serde_json::Value {
    serde_json::to_value(rows).unwrap()
}

/// `cartridges.status`'s CHECK constraint as of migration 012
/// (`src/db/migrations/012_cartridge_lifecycle.sql`) — ADR-0011 dropped
/// `offsite` from this set: a cartridge's place is a location now, not a
/// status.
const CARTRIDGE_STATUSES: &[&str] = &["available", "in_use", "pending_erase", "retired_permanent"];

/// `cartridge list --status` is a usage error when it names anything other
/// than one of `CARTRIDGE_STATUSES` (issue #171, ADR-0012) — silently
/// answering "no cartridges registered" for a typo, or for a status
/// ADR-0011 removed, is worse than refusing.
fn validate_cartridge_status(value: &str) -> Result<()> {
    if value == "offsite" {
        return Err(TapectlError::Other(format!(
            "--status \"offsite\" was removed by ADR-0011 -- a cartridge's place is a \
             location now, not a status. Use `cartridge list --location <NAME>` (see \
             `location list` for the names), or one of: {}",
            CARTRIDGE_STATUSES.join(", ")
        )));
    }
    crate::config::validate_closed_set("--status", value, CARTRIDGE_STATUSES)
        .map_err(TapectlError::Other)
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
            generation,
            capacity,
            serial,
            notes,
        } => {
            // MAM serials are trimmed at parse (src/tape/mam.rs); this flag
            // is typed by a human and was stored raw, so a trailing space
            // used to produce a second row the unique index cannot catch
            // and that would never match the medium.
            let barcode = barcode.trim();
            let serial = serial.as_deref().map(str::trim);

            // Pre-check both UNIQUE columns and refuse by name — the
            // established idiom in this crate (see
            // src/volume/write.rs's volume-label check) is pre-check-then-
            // named-error, never matching the raw SQLite constraint
            // failure.
            let barcode_taken: Option<i64> = conn
                .query_row(
                    "SELECT id FROM cartridges WHERE barcode = ?1",
                    params![barcode],
                    |row| row.get(0),
                )
                .optional()?;
            if barcode_taken.is_some() {
                return Err(TapectlError::Other(format!(
                    "cartridge \"{barcode}\" already exists"
                )));
            }
            if let Some(s) = serial {
                let serial_taken: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM cartridges WHERE serial_number = ?1",
                        params![s],
                        |row| row.get(0),
                    )
                    .optional()?;
                if serial_taken.is_some() {
                    return Err(TapectlError::Other(format!(
                        "medium serial \"{s}\" is already registered to another cartridge"
                    )));
                }
            }

            // ADR-0010: stored canonical, not the operator's raw spelling,
            // so a later comparison against a detected generation
            // (`volume init`) is a plain string match.
            let parsed = crate::media::Generation::parse(generation).ok_or_else(|| {
                TapectlError::Other(format!(
                    "{generation:?} is not a recognised LTO generation \
                     (e.g. LTO-6, LTO-7, LTO-7-M8, LTO-8)"
                ))
            })?;
            let canonical_generation = parsed.as_str();
            let (cap, capacity_display) = match capacity {
                Some(c) => (staging::parse_size_to_bytes(c)?, c.clone()),
                None => {
                    let bytes = parsed.native_capacity_bytes();
                    (
                        bytes as i64,
                        format!("{bytes} bytes, the {canonical_generation} default"),
                    )
                }
            };
            // `total_load_count` is bound explicitly as NULL rather than
            // left to the schema's `DEFAULT 0` (issue #184): a hand-
            // registered cartridge has by construction never had its MAM
            // load count read, so "unknown" is the honest state, not a
            // false "zero loads" that a later bind with no readable load
            // count (`bind_cartridge`'s `COALESCE`) would otherwise make
            // permanent.
            conn.execute(
                "INSERT INTO cartridges
                    (barcode, media_type, nominal_capacity, serial_number, notes, total_load_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
                params![barcode, canonical_generation, cap, serial, notes],
            )?;
            let id = conn.last_insert_rowid();
            events::log_created(conn, "cartridge", id, barcode, None)?;
            if json_output {
                println!("{}", serde_json::json!({"id": id, "barcode": barcode}));
            } else {
                println!(
                    "cartridge \"{barcode}\" registered (id={id}, {canonical_generation}, {capacity_display})"
                );
            }
        }
        CartridgeCommands::List { status, location } => {
            if let Some(s) = status {
                validate_cartridge_status(s)?;
            }
            let rows = cartridge_rows(conn, status.as_deref(), location.as_deref())?;
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
            #[allow(clippy::type_complexity)]
            let (id, media, status, loads, cap, created, notes, location): (
                i64,
                String,
                String,
                Option<i64>,
                i64,
                String,
                Option<String>,
                Option<String>,
            ) = conn
                .query_row(
                    // LEFT JOIN: a cartridge that has never been placed must
                    // still be inspectable (ADR-0011).
                    "SELECT c.id, c.media_type, c.status, c.total_load_count, c.nominal_capacity,
                            c.created_at, c.notes, l.name
                     FROM cartridges c
                     LEFT JOIN locations l ON l.id = c.location_id
                     WHERE c.barcode = ?1",
                    params![barcode],
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
                    // `location` is ADDITIVE (ADR-0011).
                    serde_json::json!({"barcode": barcode, "media_type": media, "status": status, "loads": loads, "location": location, "volumes": volumes.len()})
                );
            } else {
                println!("Cartridge: {barcode}");
                println!("  Type:     {media}");
                println!("  Status:   {status}");
                println!(
                    "  Location: {}",
                    location.as_deref().unwrap_or("(not placed)")
                );
                println!("  Loads:    {}", display_opt_i64(&loads));
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
        CartridgeCommands::Retire {
            barcode,
            reason,
            force,
        } => {
            crate::cli::operations::cartridge_retire(
                conn,
                barcode,
                reason.as_deref(),
                *force,
                yes,
                dry_run,
                json_output,
            )?;
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
        CartridgeCommands::Relabel {
            barcode,
            new_barcode,
        } => {
            // Same reasoning as `Register` above: a typed barcode is stored
            // trimmed everywhere else, so `relabel` must not become the one
            // remaining writer that leaves a trailing space the unique index
            // cannot catch and that `register`/`info`/`move` would never
            // match again.
            let barcode = barcode.trim();
            let new_barcode = new_barcode.trim();
            let id: i64 = conn
                .query_row(
                    "SELECT id FROM cartridges WHERE barcode = ?1",
                    params![barcode],
                    |row| row.get(0),
                )
                .map_err(|_| TapectlError::Other(format!("cartridge \"{barcode}\" not found")))?;
            // `barcode` is `TEXT NOT NULL UNIQUE` (012_cartridge_lifecycle.sql).
            // Refuse a taken destination by name rather than surface the raw
            // constraint failure — same discipline as the auto-register
            // collision this command exists to remedy.
            let taken: Option<i64> = conn
                .query_row(
                    "SELECT id FROM cartridges WHERE barcode = ?1",
                    params![new_barcode],
                    |row| row.get(0),
                )
                .optional()?;
            if taken.is_some() {
                return Err(TapectlError::Other(format!(
                    "cartridge \"{new_barcode}\" already exists; choose a different barcode"
                )));
            }
            // Tier 1 (ADR-0008): no consent gate, but still honours
            // --dry-run like its peers (Retire, MarkErased).
            if dry_run {
                if json_output {
                    println!(
                        "{}",
                        serde_json::json!({"old": barcode, "new": new_barcode, "dry_run": true})
                    );
                } else {
                    println!(
                        "cartridge \"{barcode}\" would be relabelled to \"{new_barcode}\" \
                         (DRY RUN — no changes made)"
                    );
                }
                return Ok(());
            }
            conn.execute(
                "UPDATE cartridges SET barcode = ?1 WHERE id = ?2",
                params![new_barcode, id],
            )?;
            // Convention: matches `location rename` (src/cli/location.rs) —
            // action "renamed", entity_label the NEW label, old/new value in
            // old_value/new_value.
            events::log_field_change(
                conn,
                "cartridge",
                id,
                new_barcode,
                "renamed",
                "barcode",
                Some(barcode),
                new_barcode,
                None,
            )?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"old": barcode, "new": new_barcode})
                );
            } else {
                println!("cartridge \"{barcode}\" relabelled to \"{new_barcode}\"");
            }
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
/// the habit erodes. `location` (issue #157) is bound the same way, by
/// name against the already-present `LEFT JOIN locations`, never by
/// resolving to an id first — one fewer query, and nothing to interpolate.
fn cartridge_rows(
    conn: &Connection,
    status: Option<&str>,
    location: Option<&str>,
) -> Result<Vec<CartridgeRow>> {
    // LEFT JOIN, not JOIN: `location_id` is nullable and a cartridge that
    // has never been placed must still appear in the list. The same
    // reasoning as `catalog.rs`'s volume listing.
    const SELECT: &str = "SELECT c.barcode, c.media_type, c.status, c.total_load_count,
                (SELECT v.label FROM cartridge_volumes cv
                 JOIN volumes v ON v.id = cv.volume_id
                 WHERE cv.cartridge_id = c.id AND cv.unmounted_at IS NULL
                 LIMIT 1) as current_vol,
                l.name as location
         FROM cartridges c
         LEFT JOIN locations l ON l.id = c.location_id";
    let sql = match (status, location) {
        (None, None) => format!("{SELECT} ORDER BY c.barcode"),
        (Some(_), None) => format!("{SELECT} WHERE c.status = ?1 ORDER BY c.barcode"),
        (None, Some(_)) => format!("{SELECT} WHERE l.name = ?1 ORDER BY c.barcode"),
        (Some(_), Some(_)) => {
            format!("{SELECT} WHERE c.status = ?1 AND l.name = ?2 ORDER BY c.barcode")
        }
    };
    let mut stmt = conn.prepare(&sql)?;
    let bound: Vec<&dyn rusqlite::types::ToSql> = match (&status, &location) {
        (None, None) => vec![],
        (Some(st), None) => vec![st],
        (None, Some(loc)) => vec![loc],
        (Some(st), Some(loc)) => vec![st, loc],
    };
    let rows = stmt
        .query_map(bound.as_slice(), |row| {
            Ok(CartridgeRow {
                barcode: row.get(0)?,
                media_type: row.get(1)?,
                status: row.get(2)?,
                loads: row.get::<_, Option<i64>>(3)?,
                volume: row.get::<_, Option<String>>(4)?,
                location: row.get::<_, Option<String>>(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #184: the table (and `cartridge info`'s plain-text render,
    /// which calls this same helper) must not tell a genuinely never-observed
    /// load count apart from zero by rendering it blank -- that reads as "0
    /// but the column was empty", indistinguishable at a glance from an
    /// actual 0. Spelling it out as "unknown" is the one spelling used on
    /// both surfaces.
    #[test]
    fn display_opt_i64_renders_none_as_unknown_not_zero_or_blank() {
        assert_eq!(display_opt_i64(&Some(5)), "5");
        assert_eq!(display_opt_i64(&None), "unknown");
    }

    /// `cartridge list --json` shape (issue: C2 row-listing drift).
    /// `media_type`/`loads`/`volume` are additive since CTO decision
    /// 2026-09-11 (architecture review C2 follow-up, C2b); `location` is
    /// additive since ADR-0011, which made a cartridge's place a location
    /// rather than a status and gave `cartridges.location_id` its first
    /// reader.
    #[test]
    fn pin_cartridge_rows_json_shape() {
        let rows = vec![
            CartridgeRow {
                barcode: "A001L6".to_string(),
                media_type: "LTO-6".to_string(),
                status: "available".to_string(),
                loads: Some(12),
                volume: Some("L6-0001".to_string()),
                location: Some("home-rack".to_string()),
            },
            CartridgeRow {
                barcode: "A002L6".to_string(),
                media_type: "LTO-6".to_string(),
                status: "retired_permanent".to_string(),
                loads: None,
                volume: None,
                location: None,
            },
        ];
        let value = cartridge_rows_to_json(&rows);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"[{"barcode":"A001L6","loads":12,"location":"home-rack","media_type":"LTO-6","status":"available","volume":"L6-0001"},{"barcode":"A002L6","loads":null,"location":null,"media_type":"LTO-6","status":"retired_permanent","volume":null}]"#
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
        let rows = cartridge_rows(&conn, Some("available"), None).unwrap();
        assert_eq!(rows.len(), 2, "only the two available cartridges");
        assert!(rows.iter().all(|r| r.status == "available"));
    }

    /// Issue #171 / ADR-0012: `cartridge list --status offsite` must name
    /// ADR-0011 and point at locations, not silently answer "no cartridges
    /// registered". Exercised through `run()`, not the helper, because the
    /// acceptance criterion names the COMMAND.
    #[test]
    fn list_status_offsite_names_adr_0011_and_points_at_locations() {
        let conn = seed();
        let err = run(
            &conn,
            &CartridgeCommands::List {
                status: Some("offsite".to_string()),
                location: None,
            },
            false,
            false,
            false,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ADR-0011"), "{msg}");
        assert!(msg.contains("location"), "{msg}");
        assert!(
            msg.contains("available"),
            "must still list the current statuses: {msg}"
        );
    }

    /// A typo (not the retired `offsite`) is a plain usage error naming the
    /// accepted set, not a silent empty list.
    #[test]
    fn list_status_typo_is_a_usage_error_naming_accepted_values() {
        let conn = seed();
        let err = run(
            &conn,
            &CartridgeCommands::List {
                status: Some("avilable".to_string()),
                location: None,
            },
            false,
            false,
            false,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("avilable"), "{msg}");
        assert!(msg.contains("available"), "{msg}");
        assert!(msg.contains("retired_permanent"), "{msg}");
    }

    /// ADR-0011: `cartridges.location_id` finally has a reader. The LEFT
    /// JOIN matters — a cartridge that has never been placed must still be
    /// listed, not silently dropped, which is exactly what an inner join
    /// would do to every cartridge in a fresh catalog.
    #[test]
    fn list_shows_the_location_and_keeps_unplaced_cartridges() {
        let conn = seed();
        conn.execute(
            "INSERT INTO locations (name, kind) VALUES ('home-rack', 'shelf')",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE cartridges SET location_id = (SELECT id FROM locations WHERE name = 'home-rack')
             WHERE barcode = 'A001L6'",
            [],
        )
        .unwrap();

        let rows = cartridge_rows(&conn, None, None).unwrap();
        assert_eq!(rows.len(), 3, "an unplaced cartridge must not vanish");
        let placed = rows.iter().find(|r| r.barcode == "A001L6").unwrap();
        assert_eq!(placed.location.as_deref(), Some("home-rack"));
        let unplaced = rows.iter().find(|r| r.barcode == "A002L6").unwrap();
        assert_eq!(unplaced.location, None);
    }

    #[test]
    fn no_filter_lists_everything() {
        let conn = seed();
        assert_eq!(cartridge_rows(&conn, None, None).unwrap().len(), 3);
    }

    /// A value containing a quote must be treated as data, not SQL. Under the
    /// old interpolation this string would have produced a syntax error or
    /// worse; bound, it simply matches nothing.
    #[test]
    fn a_quote_in_the_status_is_data_not_sql() {
        let conn = seed();
        let rows = cartridge_rows(&conn, Some("available' OR '1'='1"), None).unwrap();
        assert!(
            rows.is_empty(),
            "a quoted payload must match no rows, not inject (got {} rows)",
            rows.len()
        );
    }

    // ---- issue #157: `cartridge list --location` ----------------------

    /// The sibling of `--status` (issue #157's cheap half). Filters to
    /// exactly the cartridges parked at the named location.
    #[test]
    fn location_filter_is_applied_and_bound() {
        let conn = seed();
        conn.execute(
            "INSERT INTO locations (name, kind) VALUES ('home-rack', 'shelf')",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE cartridges SET location_id = (SELECT id FROM locations WHERE name = 'home-rack')
             WHERE barcode IN ('A001L6', 'A002L6')",
            [],
        )
        .unwrap();

        let rows = cartridge_rows(&conn, None, Some("home-rack")).unwrap();
        assert_eq!(rows.len(), 2, "only the two cartridges parked there");
        assert!(rows
            .iter()
            .all(|r| r.location.as_deref() == Some("home-rack")));
    }

    /// `--location` and `--status` combine (AND), matching how the two
    /// filters are meant to be used together.
    #[test]
    fn location_and_status_filters_combine() {
        let conn = seed();
        conn.execute(
            "INSERT INTO locations (name, kind) VALUES ('home-rack', 'shelf')",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE cartridges SET location_id = (SELECT id FROM locations WHERE name = 'home-rack')
             WHERE barcode IN ('A001L6', 'A002L6')",
            [],
        )
        .unwrap();

        let rows = cartridge_rows(&conn, Some("retired_permanent"), Some("home-rack")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].barcode, "A002L6");
    }

    /// Issue #110's precedent, restated for the new filter: a quoted
    /// injection payload must be treated as data and match no rows, never
    /// interpolated into the query.
    #[test]
    fn a_quote_in_the_location_is_data_not_sql() {
        let conn = seed();
        let rows = cartridge_rows(&conn, None, Some("home-rack' OR '1'='1")).unwrap();
        assert!(
            rows.is_empty(),
            "a quoted payload must match no rows, not inject (got {} rows)",
            rows.len()
        );
    }

    /// An unknown location name is simply a filter that matches nothing —
    /// `cartridge_rows` itself (below the `run()`-level usage-error guard
    /// issue #171 added for `--status`) still has no equivalent "location
    /// not found" error path, and none is wanted: locations are not a
    /// closed set the way `cartridges.status` is.
    #[test]
    fn an_unknown_location_filter_matches_nothing() {
        let conn = seed();
        let rows = cartridge_rows(&conn, None, Some("nowhere")).unwrap();
        assert!(rows.is_empty());
    }

    // ---- ADR-0010: `cartridge register` ----

    fn register(
        conn: &Connection,
        barcode: &str,
        generation: &str,
        capacity: Option<&str>,
        serial: Option<&str>,
    ) -> Result<()> {
        run(
            conn,
            &CartridgeCommands::Register {
                barcode: barcode.to_string(),
                generation: generation.to_string(),
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

    /// Issue #184: a hand-registered cartridge has never had its MAM load
    /// count read, so the column must land NULL (unknown) rather than the
    /// schema's unused `DEFAULT 0` -- otherwise `bind_cartridge`'s
    /// `COALESCE(mam.load_count, total_load_count)` would make that false
    /// zero permanent the first time this cartridge is bound on a drive
    /// that reports no load count.
    #[test]
    fn register_leaves_the_load_count_null() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, None).unwrap();
        let loads: Option<i64> = conn
            .query_row(
                "SELECT total_load_count FROM cartridges WHERE barcode = 'B001'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(loads, None);
    }

    // ---- #160: named refusals on the UNIQUE columns, and trimming --------

    #[test]
    fn register_onto_a_taken_barcode_is_refused_by_name() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, None).unwrap();
        let err = register(&conn, "B001", "LTO-6", None, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("B001"), "got: {msg}");
        assert!(msg.contains("already exists"), "got: {msg}");
        assert!(
            !msg.to_lowercase().contains("constraint"),
            "must be a named refusal, not a raw SQLite error: {msg}"
        );
    }

    #[test]
    fn register_onto_a_taken_serial_is_refused_by_name() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, Some("SER-1")).unwrap();
        let err = register(&conn, "B002", "LTO-6", None, Some("SER-1")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("SER-1"), "got: {msg}");
        assert!(msg.contains("already registered"), "got: {msg}");
        assert!(
            !msg.to_lowercase().contains("constraint"),
            "must be a named refusal, not a raw SQLite error: {msg}"
        );
    }

    /// MAM serials are trimmed at parse (src/tape/mam.rs); this flag is
    /// typed by a human and used to be stored raw, so a trailing space
    /// produced a second row the unique index could not catch and that
    /// would never match the medium.
    #[test]
    fn register_trims_barcode_and_serial() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "  B001  ", "LTO-6", None, Some("  SER-1  ")).unwrap();
        let (_, _, serial) = stored_row(&conn, "B001");
        assert_eq!(
            serial.as_deref(),
            Some("SER-1"),
            "both the barcode lookup and the stored serial must be trimmed"
        );
    }

    // ---- #160: `cartridge relabel` ----------------------------------------

    fn relabel(conn: &Connection, barcode: &str, new_barcode: &str, dry_run: bool) -> Result<()> {
        run(
            conn,
            &CartridgeCommands::Relabel {
                barcode: barcode.to_string(),
                new_barcode: new_barcode.to_string(),
            },
            false,
            true,
            dry_run,
        )
    }

    #[test]
    fn relabel_renames_and_logs_an_event() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "OLD001", "LTO-6", None, None).unwrap();
        relabel(&conn, "OLD001", "NEW001", false).unwrap();

        let barcode: String = conn
            .query_row(
                "SELECT barcode FROM cartridges WHERE barcode = 'NEW001'",
                [],
                |r| r.get(0),
            )
            .expect("the row must now be findable under its new barcode");
        assert_eq!(barcode, "NEW001");

        let found_old: Option<i64> = conn
            .query_row(
                "SELECT id FROM cartridges WHERE barcode = 'OLD001'",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert!(found_old.is_none(), "the old barcode must no longer exist");

        let (action, field, old_value, new_value, entity_label): (
            String,
            String,
            Option<String>,
            Option<String>,
            String,
        ) = conn
            .query_row(
                "SELECT action, field, old_value, new_value, entity_label
                 FROM events WHERE entity_type = 'cartridge' ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(action, "renamed");
        assert_eq!(field, "barcode");
        assert_eq!(old_value.as_deref(), Some("OLD001"));
        assert_eq!(new_value.as_deref(), Some("NEW001"));
        assert_eq!(
            entity_label, "NEW001",
            "matches the `location rename` convention: entity_label is the NEW label"
        );
    }

    #[test]
    fn relabel_onto_a_taken_barcode_is_refused_by_name() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "A001", "LTO-6", None, None).unwrap();
        register(&conn, "A002", "LTO-6", None, None).unwrap();
        let err = relabel(&conn, "A001", "A002", false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("A002"), "got: {msg}");
        assert!(msg.contains("already exists"), "got: {msg}");
        assert!(
            !msg.to_lowercase().contains("constraint"),
            "must be a named refusal, not a raw SQLite error: {msg}"
        );

        // Refused, so nothing changed.
        let still_a001: String = conn
            .query_row(
                "SELECT barcode FROM cartridges WHERE barcode = 'A001'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still_a001, "A001");
    }

    #[test]
    fn relabel_trims_both_barcodes() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "OLD001", "LTO-6", None, None).unwrap();
        relabel(&conn, "  OLD001  ", "  NEW001  ", false).unwrap();
        let barcode: String = conn
            .query_row(
                "SELECT barcode FROM cartridges WHERE id = (SELECT id FROM cartridges LIMIT 1)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            barcode, "NEW001",
            "the stored barcode must be trimmed, matching `register`"
        );
    }

    #[test]
    fn relabel_of_an_unregistered_barcode_is_an_error() {
        let conn = crate::db::open_memory().unwrap();
        let err = relabel(&conn, "NOPE", "NEW001", false).unwrap_err();
        assert!(err.to_string().contains("\"NOPE\" not found"));
    }

    /// Tier 1 (ADR-0008): no consent gate, but --dry-run must still be
    /// honoured, like its `Retire`/`MarkErased` peers.
    #[test]
    fn relabel_dry_run_changes_nothing() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "OLD001", "LTO-6", None, None).unwrap();
        relabel(&conn, "OLD001", "NEW001", true).unwrap();

        let still_old: String = conn
            .query_row(
                "SELECT barcode FROM cartridges WHERE barcode = 'OLD001'",
                [],
                |r| r.get(0),
            )
            .expect("dry-run must not have renamed the row");
        assert_eq!(still_old, "OLD001");
        let event_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            event_count, 1,
            "only the original registration event -- dry-run logs nothing"
        );
    }
}
