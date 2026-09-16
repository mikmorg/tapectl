use clap::Subcommand;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use tabled::{Table, Tabled};

use crate::db::events;
use crate::error::{Result, TapectlError};

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
        /// Nominal capacity, e.g. "2500G". Decimal, as printed on the
        /// cartridge (K=10^3 ... T=10^12; ADR-0012) — not the binary unit
        /// `slice_size`/`enospc_buffer` use. Defaults to the generation
        /// table's marketed figure (ADR-0010) when omitted — give this
        /// explicitly only when the physical cartridge really differs (a
        /// declared 40 TB LTO-10 cartridge, an mhvtl micro-tape, ...).
        #[arg(long)]
        capacity: Option<String>,
        /// The medium serial you BELIEVE this cartridge carries, for
        /// pre-registering one that has not been loaded yet.
        ///
        /// ADR-0012 amendment (2026-09-16, issue #197): this is an
        /// unconfirmed CLAIM, stored in `operator_serial` — never the chip's
        /// own report. `volume init` writes the confirmed identity
        /// (`serial_number`) itself, from a real MAM read, the first time
        /// this cartridge is loaded; no operator command ever writes that
        /// column. Correct a wrong claim with `cartridge edit --serial`.
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
    /// `cartridge unretire` is the way back — the operator saying they were
    /// wrong about the medium; `cartridge mark-erased` is a different
    /// statement, that the bytes are gone (ADR-0011, corrected 2026-09-14).
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
    /// Correct a registered cartridge's generation and/or its claimed serial
    ///
    /// ADR-0012, *Rulings recorded as consequences*: "`cartridge edit
    /// --generation` corrects a wrong generation (Tier 1: it is a fact
    /// correction, and the wrong-medium check at the next init still
    /// applies)." Tier 1 under ADR-0008 — no prompt, no `--force`, no
    /// `--yes`, and it applies to every status, `retired_permanent`
    /// included. This edits only the `cartridges` row: it never rewrites
    /// `volumes.media_type` or `volumes.capacity_bytes` (ADR-0010 decision
    /// 3 — capacity is decided once at init and stored on the volume).
    ///
    /// `--serial` is a SEPARATE, independently gated correction (ADR-0012
    /// amendment, 2026-09-16; issue #197): gating is per-flag, not
    /// per-command, because `--generation` is a fact correction (Tier 1)
    /// while `--serial` is a claim about IDENTITY (Tier 2 — see
    /// `cli::consent`). It writes only `operator_serial`, never
    /// `serial_number`, which no operator command may ever touch. At least
    /// one of `--generation`/`--serial` must be given; both may be given in
    /// one call, and each is gated independently.
    Edit {
        /// Barcode
        barcode: String,
        /// The cartridge's corrected generation (e.g. LTO-5, LTO-6, LTO-7-M8)
        #[arg(long)]
        generation: Option<String>,
        /// Correct the OPERATOR-claimed serial (never the chip-confirmed
        /// one). Tier 2 under ADR-0008 — gated on consent
        /// (`cli::consent::confirm`), unlike `--generation` above.
        #[arg(long)]
        serial: Option<String>,
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
    /// Reverse a `cartridge retire` — the operator saying they were wrong
    /// about the *medium*, not that its bytes are gone
    ///
    /// ADR-0012's consequences bullet: restores the cartridge, and any
    /// volumes retired with it, to the status recorded in the `events`
    /// audit trail from that retirement (`cartridge retire` already logs
    /// `old_value` there — no schema change needed). If that history is
    /// gone (a catalog rebuilt from tape since), the cartridge falls back
    /// to `available` and any volumes are left untouched — an honest
    /// partial restore beats a guessed status, and the command says so.
    ///
    /// Tier 1 under ADR-0008: a correction of a claim, not a destructive
    /// act — no prompt, no `--force`, no `--yes`. Refuses when the
    /// cartridge is not `retired_permanent`, naming its actual status.
    /// `cartridge mark-erased` is untouched by this command and remains
    /// the separate, irreversible statement that the bytes are gone
    /// (ADR-0011, corrected 2026-09-14).
    Unretire {
        /// Barcode
        barcode: String,
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

/// `cartridge info`'s two serial lines, split out so the exact rendered text
/// is assertable without capturing stdout (the `EditOutcome` pattern below).
///
/// ADR-0012 amendment, 2026-09-16 (issue #197): "`cartridge info` shows
/// both, labelled" — a chip-confirmed identity (`serial_number`) and an
/// operator's unconfirmed claim (`operator_serial`) are different facts, and
/// an operator reading this output must be able to tell which is which at a
/// glance, never inferring confirmation from mere presence.
fn serial_info_lines(
    serial_number: &Option<String>,
    operator_serial: &Option<String>,
) -> (String, String) {
    (
        format!(
            "  Serial (chip-confirmed):   {}",
            serial_number.as_deref().unwrap_or("(none)")
        ),
        format!(
            "  Serial (operator-claimed): {}",
            operator_serial.as_deref().unwrap_or("(none)")
        ),
    )
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
            // ADR-0012 amendment, 2026-09-16 (issue #197): `serial` here is
            // an unconfirmed operator CLAIM, stored in `operator_serial`
            // below -- never `serial_number`, which is written only from a
            // real MAM read. So this checks only against OTHER rows'
            // `serial_number`: a chip cannot report two different
            // cartridges' identity, so typing a value that is already
            // someone else's CONFIRMED serial is almost certainly a typo,
            // worth catching by name. It deliberately does NOT check other
            // rows' `operator_serial` -- two operators can mistype the same
            // wrong value for two different cartridges, and refusing that
            // at registration would be a UNIQUE constraint by another name,
            // which is exactly what `operator_serial` (migration 016)
            // declines to be. A duplicate, unconfirmed claim is tolerated;
            // it resolves itself the first time either cartridge is loaded.
            if let Some(s) = serial {
                let confirmed_elsewhere: Option<String> = conn
                    .query_row(
                        "SELECT barcode FROM cartridges WHERE serial_number = ?1",
                        params![s],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(other_barcode) = confirmed_elsewhere {
                    return Err(TapectlError::Other(format!(
                        "medium serial \"{s}\" is already the CHIP-CONFIRMED serial of \
                         cartridge \"{other_barcode}\" (a real MAM read put it there). \
                         Registering it as \"{barcode}\"'s claimed serial would name a \
                         cartridge that already has a different, verified identity -- almost \
                         certainly a typo. If \"{barcode}\" and \"{other_barcode}\" are the \
                         same physical cartridge, it is already registered; use that barcode."
                    )));
                }
            }

            // ADR-0010: stored canonical, not the operator's raw spelling,
            // so a later comparison against a detected generation
            // (`volume init`) is a plain string match.
            let parsed = crate::media::parse_generation_or_error(generation)?;
            let canonical_generation = parsed.as_str();
            let (cap, capacity_display) = match capacity {
                Some(c) => (crate::media::parse_capacity_to_bytes(c)?, c.clone()),
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
            // `operator_serial`, never `serial_number` (ADR-0012 amendment,
            // 2026-09-16; issue #197): this command records only the
            // operator's claim. `serial_number` is left NULL and is written
            // only from a real MAM read (`volume::binding::record_medium_serial`).
            conn.execute(
                "INSERT INTO cartridges
                    (barcode, media_type, nominal_capacity, operator_serial, notes, total_load_count)
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
            let (
                id,
                media,
                status,
                loads,
                cap,
                created,
                notes,
                location,
                serial_number,
                operator_serial,
            ): (
                i64,
                String,
                String,
                Option<i64>,
                i64,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
            ) = conn
                .query_row(
                    // LEFT JOIN: a cartridge that has never been placed must
                    // still be inspectable (ADR-0011).
                    "SELECT c.id, c.media_type, c.status, c.total_load_count, c.nominal_capacity,
                            c.created_at, c.notes, l.name, c.serial_number, c.operator_serial
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
                            row.get(8)?,
                            row.get(9)?,
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
                    // `location` is ADDITIVE (ADR-0011); `serial_number` /
                    // `operator_serial` likewise (ADR-0012 amendment,
                    // 2026-09-16; issue #197).
                    serde_json::json!({
                        "barcode": barcode, "media_type": media, "status": status,
                        "loads": loads, "location": location, "volumes": volumes.len(),
                        "serial_number": serial_number, "operator_serial": operator_serial,
                    })
                );
            } else {
                println!("Cartridge: {barcode}");
                println!("  Type:     {media}");
                println!("  Status:   {status}");
                let (chip_line, operator_line) =
                    serial_info_lines(&serial_number, &operator_serial);
                println!("{chip_line}");
                println!("{operator_line}");
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
        CartridgeCommands::Edit {
            barcode,
            generation,
            serial,
        } => {
            let barcode = barcode.trim();
            // ADR-0012 amendment, 2026-09-16 (issue #197): gating is
            // PER-FLAG, not per-command -- `--generation` stays Tier 1
            // (ungated), `--serial` is Tier 2 (gated via
            // `cli::consent::confirm`, using the global `--yes` exactly as
            // `db import` does: a plain identity-claim change with no
            // computed coverage risk to show). Each is independently
            // optional, but at least one must be given.
            if generation.is_none() && serial.is_none() {
                return Err(TapectlError::Other(
                    "cartridge edit: nothing to do -- give --generation, --serial, or both"
                        .to_string(),
                ));
            }

            let generation_outcome = generation
                .as_deref()
                .map(|g| cartridge_edit(conn, barcode, g, dry_run))
                .transpose()?;
            let serial_outcome = serial
                .as_deref()
                .map(|s| cartridge_edit_serial(conn, barcode, s, yes, dry_run))
                .transpose()?;

            if json_output {
                let mut obj = serde_json::json!({ "barcode": barcode, "dry_run": dry_run });
                let map = obj.as_object_mut().unwrap();
                if let Some(outcome) = &generation_outcome {
                    map.insert(
                        "media_type".to_string(),
                        serde_json::json!({
                            "old": outcome.old_media_type,
                            "new": outcome.new_media_type,
                        }),
                    );
                    map.insert(
                        "nominal_capacity".to_string(),
                        serde_json::json!({
                            "old": outcome.old_capacity,
                            "new": outcome.new_capacity,
                            "redefaulted": outcome.redefaulted,
                        }),
                    );
                    // Plain `dry_run`, not `dry_run && changed`. The field
                    // answers "was this invocation a dry run?", and a
                    // consumer asking "did anything mutate?" reads
                    // `changed`. Conflating them made `--dry-run` on a
                    // same-value no-op report `dry_run: false`, which reads
                    // alone as "this was a real run" — the opposite of the
                    // truth. `Relabel` already reports a plain `true`; this
                    // matches it.
                    map.insert("changed".to_string(), serde_json::json!(outcome.changed));
                }
                if let Some(outcome) = &serial_outcome {
                    map.insert(
                        "operator_serial".to_string(),
                        serde_json::json!({
                            "old": outcome.old,
                            "new": outcome.new,
                            "changed": outcome.changed,
                        }),
                    );
                }
                println!("{obj}");
            } else {
                if let Some(outcome) = &generation_outcome {
                    if !outcome.changed {
                        println!(
                            "cartridge \"{barcode}\" is already {}; nothing changed",
                            outcome.new_media_type
                        );
                    } else {
                        let suffix = if dry_run {
                            " (DRY RUN — no changes made)"
                        } else {
                            ""
                        };
                        println!(
                            "cartridge \"{barcode}\" media_type: {} -> {}{suffix}",
                            outcome.old_media_type, outcome.new_media_type
                        );
                        println!("{}", outcome.capacity_note);
                        for (label, vol_media) in &outcome.mismatched_volumes {
                            println!(
                                "warning: volume \"{label}\" has an open mount on this \
                                 cartridge and was planned as {vol_media}, which now differs \
                                 from the corrected generation {} -- display only, nothing \
                                 about the volume is changed (ADR-0010 decision 3)",
                                outcome.new_media_type
                            );
                        }
                    }
                }
                if let Some(outcome) = &serial_outcome {
                    if !outcome.changed {
                        println!(
                            "cartridge \"{barcode}\"'s claimed serial is already {}; nothing changed",
                            outcome.new
                        );
                    } else {
                        let suffix = if dry_run {
                            " (DRY RUN — no changes made)"
                        } else {
                            ""
                        };
                        println!(
                            "cartridge \"{barcode}\" operator_serial: {} -> {}{suffix}",
                            outcome.old.as_deref().unwrap_or("(none)"),
                            outcome.new
                        );
                    }
                }
            }
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
        CartridgeCommands::Unretire { barcode } => {
            crate::cli::operations::cartridge_unretire(conn, barcode, dry_run, json_output)?;
        }
    }
    Ok(())
}

/// The result of `cartridge edit --generation` (issue #167, ADR-0012 Tier 1),
/// split out from the printing so it is assertable in tests without
/// capturing stdout — the same pattern as `location::MoveOutcome`.
struct EditOutcome {
    /// `false` for the same-value no-op (step 4): nothing was written, no
    /// event was logged, and `old_media_type == new_media_type`.
    changed: bool,
    old_media_type: String,
    new_media_type: String,
    old_capacity: i64,
    new_capacity: i64,
    /// Did the capacity re-default (step 5's first branch)? `false` for the
    /// no-op case too.
    redefaulted: bool,
    /// Step 5's text, stating which capacity branch ran and why. Empty for
    /// the no-op case (nothing to say about capacity when nothing changed).
    capacity_note: String,
    /// Step 6: `(volume label, volume media_type)` for every volume with an
    /// open `cartridge_volumes` mount on this cartridge whose planned
    /// `media_type` now differs from the corrected generation. Display-only
    /// (ADR-0008 Tier 1) — this command never rewrites `volumes.media_type`
    /// or `volumes.capacity_bytes` (ADR-0010 decision 3: capacity is decided
    /// once at init).
    mismatched_volumes: Vec<(String, String)>,
}

/// `cartridge edit --generation`: corrects a registered cartridge's
/// generation (ADR-0012, *Rulings recorded as consequences*; issue #167).
///
/// Tier 1 under ADR-0008 — no prompt, no `--force`, no `--yes` — and applies
/// to every status, `retired_permanent` included: a fact correction is not
/// gated by fitness. Edits only the `cartridges` row; `volumes.media_type`
/// and `volumes.capacity_bytes` are never rewritten (ADR-0010 decision 3 —
/// capacity is decided once at init and stored on the volume).
///
/// `dry_run` still honours the convention every other mutating command in
/// this file follows (`Retire`/`MarkErased`/`Relabel`/`Unretire`): the
/// computed outcome is returned for display, but the transaction in step 7
/// is skipped, so the row and the events table are untouched.
fn cartridge_edit(
    conn: &Connection,
    barcode: &str,
    generation: &str,
    dry_run: bool,
) -> Result<EditOutcome> {
    // ADR-0012, *Unknown config keys are errors everywhere; closed-set
    // values are validated at load*: the same validator `Register` uses, so
    // the error text cannot drift between the two commands.
    let parsed = crate::media::parse_generation_or_error(generation)?;
    let new_media_type = parsed.as_str();

    let (id, old_media_type, old_capacity): (i64, String, i64) = conn
        .query_row(
            "SELECT id, media_type, nominal_capacity FROM cartridges WHERE barcode = ?1",
            params![barcode],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|_| TapectlError::Other(format!("cartridge \"{barcode}\" not found")))?;

    // Step 4: same value -> no-op. No UPDATE, no event, exit 0.
    if old_media_type == new_media_type {
        return Ok(EditOutcome {
            changed: false,
            old_media_type,
            new_media_type: new_media_type.to_string(),
            old_capacity,
            new_capacity: old_capacity,
            redefaulted: false,
            capacity_note: String::new(),
            mismatched_volumes: Vec::new(),
        });
    }

    // Step 5: capacity re-default, mechanical and exact -- no judgement.
    // Equality is tested against the OLD generation's table figure, so this
    // is independent of the binary/decimal parser question (ADR-0012).
    let (new_capacity, redefaulted, capacity_note) =
        match crate::media::Generation::parse(&old_media_type) {
            Some(old_gen) if old_capacity == old_gen.native_capacity_bytes() as i64 => {
                let redefaulted_capacity = parsed.native_capacity_bytes() as i64;
                (
                    redefaulted_capacity,
                    true,
                    format!(
                        "capacity re-defaulted from the {old_media_type} table figure \
                         ({old_capacity}) to the {new_media_type} table figure \
                         ({redefaulted_capacity})"
                    ),
                )
            }
            Some(_) => (
                old_capacity,
                false,
                format!(
                    "capacity left at {old_capacity} (not the {old_media_type} table \
                     figure, so it was set deliberately)"
                ),
            ),
            None => (
                old_capacity,
                false,
                format!(
                    "capacity left at {old_capacity} (the old media_type {old_media_type:?} \
                     does not parse as a recognised generation, so the old table figure \
                     cannot be computed)"
                ),
            ),
        };

    // Step 6: display-only warning for any volume with an open mount here
    // whose planned media_type now disagrees. Never gates; never writes.
    let mut stmt = conn.prepare(
        "SELECT v.label, v.media_type FROM cartridge_volumes cv
         JOIN volumes v ON v.id = cv.volume_id
         WHERE cv.cartridge_id = ?1 AND cv.unmounted_at IS NULL
           AND v.media_type IS NOT NULL AND v.media_type != ?2",
    )?;
    let mismatched_volumes: Vec<(String, String)> = stmt
        .query_map(params![id, new_media_type], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);

    if dry_run {
        return Ok(EditOutcome {
            changed: true,
            old_media_type,
            new_media_type: new_media_type.to_string(),
            old_capacity,
            new_capacity,
            redefaulted,
            capacity_note,
            mismatched_volumes,
        });
    }

    // Step 7: one transaction for the UPDATE and its event(s).
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE cartridges SET media_type = ?1, nominal_capacity = ?2 WHERE id = ?3",
        params![new_media_type, new_capacity, id],
    )?;
    events::log_field_change(
        &tx,
        "cartridge",
        id,
        barcode,
        "updated",
        "media_type",
        Some(&old_media_type),
        new_media_type,
        None,
    )?;
    if redefaulted {
        events::log_field_change(
            &tx,
            "cartridge",
            id,
            barcode,
            "updated",
            "nominal_capacity",
            Some(&old_capacity.to_string()),
            &new_capacity.to_string(),
            None,
        )?;
    }
    tx.commit()?;

    Ok(EditOutcome {
        changed: true,
        old_media_type,
        new_media_type: new_media_type.to_string(),
        old_capacity,
        new_capacity,
        redefaulted,
        capacity_note,
        mismatched_volumes,
    })
}

/// The result of `cartridge edit --serial` (ADR-0012 amendment, 2026-09-16;
/// issue #197), split out from printing for the same reason as
/// [`EditOutcome`].
struct SerialEditOutcome {
    /// `false` for the same-value no-op: nothing was written, no event was
    /// logged, no consent was asked (there is nothing to confirm).
    changed: bool,
    old: Option<String>,
    new: String,
}

/// `cartridge edit --serial`: corrects the OPERATOR's claim about a
/// cartridge's medium serial (ADR-0012 amendment, 2026-09-16; issue #197).
///
/// Writes ONLY `cartridges.operator_serial` — never `serial_number`, which is
/// written only from a real MAM read
/// (`crate::volume::binding::record_medium_serial`). That is the entire
/// structural point of the ruling: no operator command may ever touch the
/// chip-read column, so "never overwrite a chip-read serial" is true by
/// construction rather than a rule this function has to remember.
///
/// Tier 2 under ADR-0008, unlike `cartridge_edit`'s `--generation` (Tier 1):
/// this is a claim about IDENTITY, gated on consent via
/// `cli::consent::confirm` — the crate's one mechanic for exactly this,
/// used here the same way `db import` uses it: a plain identity-claim
/// change with no COMPUTED coverage risk to show, so `facts` carries at most
/// one informational line (naming a chip-confirmed serial already on the
/// row, if any) rather than ADR-0004 evidence.
///
/// `dry_run` follows the same convention as every other mutating command in
/// this file: the outcome is computed and returned for display, but neither
/// the consent prompt nor the transaction runs.
fn cartridge_edit_serial(
    conn: &Connection,
    barcode: &str,
    serial: &str,
    assume_yes: bool,
    dry_run: bool,
) -> Result<SerialEditOutcome> {
    let serial = serial.trim();
    let (id, old_operator_serial, chip_confirmed): (i64, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT id, operator_serial, serial_number FROM cartridges WHERE barcode = ?1",
            params![barcode],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|_| TapectlError::Other(format!("cartridge \"{barcode}\" not found")))?;

    // Same-value no-op: nothing to confirm, nothing to write.
    if old_operator_serial.as_deref() == Some(serial) {
        return Ok(SerialEditOutcome {
            changed: false,
            old: old_operator_serial,
            new: serial.to_string(),
        });
    }

    if dry_run {
        return Ok(SerialEditOutcome {
            changed: true,
            old: old_operator_serial,
            new: serial.to_string(),
        });
    }

    let action = format!("correct cartridge \"{barcode}\"'s claimed serial to \"{serial}\"");
    let mut facts = Vec::new();
    if let Some(chip) = &chip_confirmed {
        facts.push(format!(
            "cartridge \"{barcode}\" already has a CHIP-CONFIRMED serial ({chip}); this edit \
             changes only the operator's claim and never touches that value"
        ));
    }
    crate::cli::consent::confirm(&action, &facts, assume_yes)?;

    conn.execute(
        "UPDATE cartridges SET operator_serial = ?1 WHERE id = ?2",
        params![serial, id],
    )?;
    events::log_field_change(
        conn,
        "cartridge",
        id,
        barcode,
        "updated",
        "operator_serial",
        old_operator_serial.as_deref(),
        serial,
        None,
    )?;

    Ok(SerialEditOutcome {
        changed: true,
        old: old_operator_serial,
        new: serial.to_string(),
    })
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

    /// `(media_type, nominal_capacity, serial_number, operator_serial)`.
    /// The last two are DIFFERENT columns since migration 016 (ADR-0012
    /// amendment, 2026-09-16; issue #197): `serial_number` is chip-confirmed
    /// only, `operator_serial` is `cartridge register --serial`'s claim.
    fn stored_row(
        conn: &Connection,
        barcode: &str,
    ) -> (String, i64, Option<String>, Option<String>) {
        conn.query_row(
            "SELECT media_type, nominal_capacity, serial_number, operator_serial
             FROM cartridges WHERE barcode = ?1",
            rusqlite::params![barcode],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
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
        let (_, cap, _, _) = stored_row(&conn, "B001");
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
        // capacities, so the operator states it here). Decimal (ADR-0012,
        // issue #168): "40000G" means 40000 * 10^9 = the marketed 40 TB,
        // not the binary parser's ~43.95 TB.
        register(&conn, "B001", "LTO-10", Some("40000G"), None).unwrap();
        let (_, cap, _, _) = stored_row(&conn, "B001");
        assert_eq!(cap, 40_000_000_000_000);
    }

    /// Issue #168: `--capacity` is decimal, the same unit the omitted-
    /// capacity default reads from the generation table — so a declared
    /// figure and the table's own figure must agree exactly for the
    /// generation they both describe.
    #[test]
    fn register_with_explicit_capacity_matches_the_generation_tables_decimal_unit() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", Some("2.5T"), None).unwrap();
        let (_, cap, _, _) = stored_row(&conn, "B001");
        assert_eq!(cap, 2_500_000_000_000);
        assert_eq!(
            cap,
            crate::media::Generation::Lto6.native_capacity_bytes() as i64
        );
    }

    /// ADR-0012 amendment, 2026-09-16 (issue #197): `--serial` is an
    /// unconfirmed operator CLAIM, so it lands in `operator_serial` — and
    /// `serial_number`, the chip-confirmed identity, must stay `NULL`. That
    /// second assertion is the whole point of the fix: before #197,
    /// `register --serial` wrote the SAME column a real MAM read writes.
    #[test]
    fn register_persists_an_explicit_serial_as_the_operators_claim() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, Some("EW7VWMVKF6")).unwrap();
        let (_, _, serial_number, operator_serial) = stored_row(&conn, "B001");
        assert_eq!(operator_serial.as_deref(), Some("EW7VWMVKF6"));
        assert_eq!(
            serial_number, None,
            "register --serial must never write the chip-confirmed column"
        );
    }

    #[test]
    fn register_without_serial_leaves_both_columns_null() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, None).unwrap();
        let (_, _, serial_number, operator_serial) = stored_row(&conn, "B001");
        assert_eq!(serial_number, None);
        assert_eq!(operator_serial, None);
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

    /// ADR-0012 amendment, 2026-09-16 (issue #197), migration 016's own
    /// header: `operator_serial` carries NO uniqueness check, deliberately
    /// -- two operators typing the same wrong serial for two different
    /// cartridges is a duplicate, UNCONFIRMED claim, not a schema violation.
    /// A pre-check refusal here would be a UNIQUE constraint by another
    /// name, which is exactly what the ruling declines to add.
    #[test]
    fn register_allows_a_duplicate_operator_serial_claim() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, Some("SER-1")).unwrap();
        register(&conn, "B002", "LTO-6", None, Some("SER-1"))
            .expect("two unconfirmed claims of the same serial must not be refused");
        let (_, _, _, op1) = stored_row(&conn, "B001");
        let (_, _, _, op2) = stored_row(&conn, "B002");
        assert_eq!(op1.as_deref(), Some("SER-1"));
        assert_eq!(op2.as_deref(), Some("SER-1"));
    }

    /// ...but typing a serial that is already another row's CHIP-CONFIRMED
    /// `serial_number` is still refused by name: a real MAM read cannot
    /// report two different cartridges' identity, so this is almost
    /// certainly a typo, and the register-time check catches it.
    #[test]
    fn register_onto_a_chip_confirmed_serial_is_refused_by_name() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, None).unwrap();
        conn.execute(
            "UPDATE cartridges SET serial_number = 'SER-1' WHERE barcode = 'B001'",
            [],
        )
        .unwrap();

        let err = register(&conn, "B002", "LTO-6", None, Some("SER-1")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("SER-1"), "got: {msg}");
        assert!(msg.contains("CHIP-CONFIRMED"), "got: {msg}");
        assert!(msg.contains("B001"), "got: {msg}");
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
        let (_, _, _, operator_serial) = stored_row(&conn, "B001");
        assert_eq!(
            operator_serial.as_deref(),
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

    // ---- issue #167: `cartridge edit --generation` ------------------------

    fn edit(conn: &Connection, barcode: &str, generation: &str) -> Result<()> {
        run(
            conn,
            &CartridgeCommands::Edit {
                barcode: barcode.to_string(),
                generation: Some(generation.to_string()),
                serial: None,
            },
            false,
            true,
            false,
        )
    }

    /// `run()` with the global `--yes` NOT assumed, so `--serial`'s Tier 2
    /// gate (`cli::consent::confirm`) is actually exercised rather than
    /// bypassed.
    fn edit_serial(conn: &Connection, barcode: &str, serial: &str, yes: bool) -> Result<()> {
        run(
            conn,
            &CartridgeCommands::Edit {
                barcode: barcode.to_string(),
                generation: None,
                serial: Some(serial.to_string()),
            },
            false,
            yes,
            false,
        )
    }

    /// `--dry-run` was not in issue #167's twelve steps; it was added because
    /// every other mutating cartridge command honours it, and an `Edit` that
    /// silently ignored it would mean `tapectl --dry-run cartridge edit ...`
    /// writes to the catalog. That makes this the one behaviour here with no
    /// acceptance test of its own, so it gets one: a write-prevention path
    /// nothing exercises is a path that regresses quietly.
    #[test]
    fn edit_generation_dry_run_changes_nothing() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, None).unwrap();
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();

        run(
            &conn,
            &CartridgeCommands::Edit {
                barcode: "B001".to_string(),
                generation: Some("LTO-5".to_string()),
                serial: None,
            },
            false,
            true,
            true,
        )
        .unwrap();

        let (media_type, cap, _, _) = stored_row(&conn, "B001");
        assert_eq!(media_type, "LTO-6", "dry-run must not rewrite media_type");
        assert_eq!(
            cap,
            crate::media::Generation::Lto6.native_capacity_bytes() as i64,
            "dry-run must not re-default the capacity"
        );
        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after, "dry-run logs no event");
    }

    #[test]
    fn edit_generation_stores_the_canonical_spelling() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, None).unwrap();
        edit(&conn, "B001", "lto5").unwrap();
        let (media_type, ..) = stored_row(&conn, "B001");
        assert_eq!(media_type, "LTO-5");
    }

    #[test]
    fn edit_generation_redefaults_a_table_valued_capacity() {
        let conn = crate::db::open_memory().unwrap();
        // Registered with no --capacity: the table figure
        // (2,500,000,000,000), never operator-set.
        register(&conn, "B001", "LTO-6", None, None).unwrap();
        let outcome = cartridge_edit(&conn, "B001", "LTO-5", false).unwrap();
        assert!(outcome.redefaulted, "JSON redefaulted must be true");
        let (_, cap, _, _) = stored_row(&conn, "B001");
        assert_eq!(
            cap,
            crate::media::Generation::Lto5.native_capacity_bytes() as i64
        );
    }

    #[test]
    fn edit_generation_leaves_an_operator_set_capacity_alone() {
        let conn = crate::db::open_memory().unwrap();
        // 40000G is never the LTO-10 table figure (30 TB), so it was set
        // deliberately and must survive the edit untouched.
        register(&conn, "B001", "LTO-10", Some("40000G"), None).unwrap();
        let outcome = cartridge_edit(&conn, "B001", "LTO-9", false).unwrap();
        assert!(!outcome.redefaulted, "JSON redefaulted must be false");
        let (_, cap, _, _) = stored_row(&conn, "B001");
        assert_eq!(cap, 40_000_000_000_000);
    }

    #[test]
    fn edit_generation_leaves_capacity_alone_when_the_old_row_does_not_parse() {
        let conn = crate::db::open_memory().unwrap();
        // A pre-ADR-0010 or hand-edited row whose media_type does not parse
        // (issue #167's third reachability path) -- the old table figure
        // cannot be computed, so the capacity branch must leave it alone
        // rather than guess.
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity)
             VALUES ('B001', 'ULTRIUM6', 2500000000000)",
            [],
        )
        .unwrap();
        edit(&conn, "B001", "LTO-6").unwrap();
        let (media_type, cap, _, _) = stored_row(&conn, "B001");
        assert_eq!(media_type, "LTO-6");
        assert_eq!(cap, 2_500_000_000_000, "capacity must be left untouched");
    }

    #[test]
    fn edit_generation_rejects_an_unrecognised_generation() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, None).unwrap();
        let err = edit(&conn, "B001", "banana").unwrap_err();
        assert!(err.to_string().contains("banana"));
        let (media_type, ..) = stored_row(&conn, "B001");
        assert_eq!(
            media_type, "LTO-6",
            "a rejected edit must not touch the row"
        );
    }

    #[test]
    fn edit_generation_on_an_unknown_barcode_is_not_found() {
        let conn = crate::db::open_memory().unwrap();
        let err = edit(&conn, "NOPE", "LTO-6").unwrap_err();
        assert!(err.to_string().contains("\"NOPE\" not found"));
    }

    #[test]
    fn edit_generation_same_value_writes_no_event() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, None).unwrap();
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        // Same value, different spelling -- must still be recognised as a
        // no-op once canonicalised.
        edit(&conn, "B001", "lto6").unwrap();
        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after, "a same-value edit must log no event");
    }

    #[test]
    fn edit_generation_logs_a_field_change_event() {
        let conn = crate::db::open_memory().unwrap();

        // An explicit --capacity is never the table figure by construction,
        // so only the media_type event fires.
        register(&conn, "B001", "LTO-10", Some("40000G"), None).unwrap();
        edit(&conn, "B001", "LTO-9").unwrap();
        let updated_b001: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events
                 WHERE entity_type = 'cartridge' AND action = 'updated' AND entity_label = 'B001'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(updated_b001, 1, "only media_type changed, no re-default");

        // A table-valued capacity re-defaults: two event rows.
        register(&conn, "B002", "LTO-6", None, None).unwrap();
        edit(&conn, "B002", "LTO-5").unwrap();
        let updated_b002: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events
                 WHERE entity_type = 'cartridge' AND action = 'updated' AND entity_label = 'B002'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(updated_b002, 2, "media_type + nominal_capacity");

        let (field, old_value, new_value): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT field, old_value, new_value FROM events
                 WHERE entity_type = 'cartridge' AND action = 'updated' AND entity_label = 'B001'
                 ORDER BY id LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(field, "media_type");
        assert_eq!(old_value.as_deref(), Some("LTO-10"));
        assert_eq!(new_value.as_deref(), Some("LTO-9"));
    }

    /// Proves "the wrong-medium check at the next init still applies"
    /// without a drive: `resolve_media` refuses a row that disagrees with
    /// the detected medium, names the exact repair command, and once that
    /// repair is applied via `cartridge edit`, the same call succeeds.
    #[test]
    fn edit_generation_clears_the_init_refusal() {
        use crate::media::Generation;
        use crate::tape::mam::MamInfo;
        use crate::tape::media_detect::{resolve_media, DetectSource, Detected};

        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, None).unwrap();

        let detected_lto5 = Detected {
            generation: Some(Generation::Lto5),
            code: Some(0x58),
            source: DetectSource::MamMedium,
            mam: MamInfo::default(),
        };

        // The stale row (LTO-6) disagrees with the loaded medium (LTO-5):
        // refused, and the refusal names the exact repair.
        let err = resolve_media(
            &detected_lto5,
            None,
            Some((Generation::Lto6, "B001")),
            Generation::Lto6,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("cartridge edit B001 --generation LTO-5"),
            "{err}"
        );

        edit(&conn, "B001", "LTO-5").unwrap();
        let (media_type, ..) = stored_row(&conn, "B001");
        let corrected = Generation::parse(&media_type).unwrap();
        assert_eq!(corrected, Generation::Lto5);

        // The same call, with the corrected row, now agrees.
        let (gen, source) = resolve_media(
            &detected_lto5,
            None,
            Some((corrected, "B001")),
            Generation::Lto6,
        )
        .unwrap();
        assert_eq!(gen, Generation::Lto5);
        assert!(source.is_detected());
    }

    // ---- ADR-0012 amendment, 2026-09-16: `cartridge edit --serial` (Tier 2,
    // gated per-flag — issue #197) ------------------------------------------

    #[test]
    fn edit_requires_at_least_one_of_generation_or_serial() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, None).unwrap();
        let err = run(
            &conn,
            &CartridgeCommands::Edit {
                barcode: "B001".to_string(),
                generation: None,
                serial: None,
            },
            false,
            true,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("nothing to do"));
    }

    /// The Tier 2 gate is REAL, not decorative: under `cargo test` stdin is
    /// never a terminal, so `run()` with the global `--yes` NOT set must hit
    /// `cli::consent::confirm`'s non-interactive refusal exactly like every
    /// other Tier-2 command in this crate — and the row must be untouched.
    #[test]
    fn edit_serial_without_yes_is_refused_and_writes_nothing() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, None).unwrap();
        let err = edit_serial(&conn, "B001", "SER-1", false).unwrap_err();
        assert!(
            err.to_string().contains("non-interactive session"),
            "got: {err}"
        );
        let (_, _, serial_number, operator_serial) = stored_row(&conn, "B001");
        assert_eq!(serial_number, None);
        assert_eq!(operator_serial, None, "a refused edit must write nothing");
    }

    #[test]
    fn edit_serial_with_yes_writes_the_claim_and_logs_an_event() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, None).unwrap();
        edit_serial(&conn, "B001", "SER-1", true).unwrap();

        let (_, _, serial_number, operator_serial) = stored_row(&conn, "B001");
        assert_eq!(
            serial_number, None,
            "cartridge edit --serial must never write the chip-confirmed column"
        );
        assert_eq!(operator_serial.as_deref(), Some("SER-1"));

        let (field, old_value, new_value): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT field, old_value, new_value FROM events
                 WHERE entity_type = 'cartridge' AND action = 'updated' AND field = 'operator_serial'
                 ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(field, "operator_serial");
        assert_eq!(old_value, None);
        assert_eq!(new_value.as_deref(), Some("SER-1"));
    }

    /// `cartridge edit --serial` NEVER touches `serial_number`, even when
    /// one is already chip-confirmed — it corrects only the assertion.
    #[test]
    fn edit_serial_on_a_chip_confirmed_row_changes_only_the_claim() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, None).unwrap();
        conn.execute(
            "UPDATE cartridges SET serial_number = 'SER-CHIP' WHERE barcode = 'B001'",
            [],
        )
        .unwrap();

        edit_serial(&conn, "B001", "SER-CLAIMED", true).unwrap();

        let (_, _, serial_number, operator_serial) = stored_row(&conn, "B001");
        assert_eq!(
            serial_number.as_deref(),
            Some("SER-CHIP"),
            "the chip-confirmed serial must never be touched by this command"
        );
        assert_eq!(operator_serial.as_deref(), Some("SER-CLAIMED"));
    }

    #[test]
    fn edit_serial_same_value_writes_no_event_and_asks_no_consent() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, Some("SER-1")).unwrap();
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        // yes=false: if this reached the consent gate at all it would be
        // refused (non-interactive), so success here proves the no-op path
        // short-circuits before ever asking.
        edit_serial(&conn, "B001", "SER-1", false).unwrap();
        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn edit_serial_dry_run_changes_nothing() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, None).unwrap();
        run(
            &conn,
            &CartridgeCommands::Edit {
                barcode: "B001".to_string(),
                generation: None,
                serial: Some("SER-1".to_string()),
            },
            false,
            false,
            true,
        )
        .unwrap();
        let (_, _, _, operator_serial) = stored_row(&conn, "B001");
        assert_eq!(
            operator_serial, None,
            "dry-run must write nothing, and must not even ask for consent"
        );
    }

    /// Both flags in one call, each gated independently: `--generation` (Tier
    /// 1) applies unconditionally, `--serial` (Tier 2) still needs consent.
    #[test]
    fn edit_generation_and_serial_together_gate_only_the_serial_half() {
        let conn = crate::db::open_memory().unwrap();
        register(&conn, "B001", "LTO-6", None, None).unwrap();
        run(
            &conn,
            &CartridgeCommands::Edit {
                barcode: "B001".to_string(),
                generation: Some("LTO-5".to_string()),
                serial: Some("SER-1".to_string()),
            },
            false,
            true,
            false,
        )
        .unwrap();
        let (media_type, _, _, operator_serial) = stored_row(&conn, "B001");
        assert_eq!(media_type, "LTO-5");
        assert_eq!(operator_serial.as_deref(), Some("SER-1"));
    }

    // ---- `cartridge info`: both serial fields, clearly labelled -----------

    #[test]
    fn info_renders_both_serial_fields_distinguishably() {
        // Pins the exact text (ADR-0012 amendment's "clearly labelled so an
        // operator can tell a confirmed serial from an unconfirmed claim").
        let (chip_line, operator_line) = serial_info_lines(
            &Some("SER-CHIP".to_string()),
            &Some("SER-CLAIMED".to_string()),
        );
        assert_eq!(chip_line, "  Serial (chip-confirmed):   SER-CHIP");
        assert_eq!(operator_line, "  Serial (operator-claimed): SER-CLAIMED");
    }

    #[test]
    fn info_renders_absent_serials_as_none_not_blank() {
        let (chip_line, operator_line) = serial_info_lines(&None, &None);
        assert_eq!(chip_line, "  Serial (chip-confirmed):   (none)");
        assert_eq!(operator_line, "  Serial (operator-claimed): (none)");
    }
}
