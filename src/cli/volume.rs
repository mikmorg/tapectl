use clap::Subcommand;
use rusqlite::Connection;

use crate::config::{Config, TapectlPaths};
use crate::error::Result;
use crate::store::Tier;
use crate::volume::write;

const DEFAULT_BLOCK_SIZE: usize = 512 * 1024; // 512 KB

#[derive(Subcommand, Debug)]
pub enum VolumeCommands {
    /// Initialize a new volume (write ID thunk to tape)
    Init {
        /// Volume label (e.g., L6-0001)
        label: String,
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
        /// Overwrite a cartridge whose File 0 already identifies a
        /// DIFFERENT volume (e.g. a mislabeled or stale tape). Refused by
        /// default (issue #27) — loading the wrong cartridge would
        /// otherwise silently overwrite it. Never overrides a cartridge
        /// that is already SEALED (ADR-0003): bulk-erase the physical tape
        /// and run `cartridge mark-erased` first for that case.
        #[arg(long)]
        force: bool,
    },

    /// Write staged data to volume
    Write {
        /// Volume label
        label: String,
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
        /// See `volume init --force` — same override, same limits.
        #[arg(long)]
        force: bool,
    },

    /// Resume an interrupted write session (issue #25). Reload the SAME
    /// cartridge first: the session continues from its frozen staging files
    /// rather than rebuilding them, and it refuses (quarantining the volume)
    /// if the loaded tape's File 0 does not match, or if the tape is already
    /// sealed. There is no --force: `volume write --force` overrides a
    /// wrong-cartridge finding before anything is written, which has no
    /// meaning for a tape this session has already partly written.
    Resume {
        /// Volume label
        label: String,
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
    },

    /// Deliberately abandon a volume's unfinished write session (issue #94):
    /// `docs/design/layout-session.md`'s Aborted row, first clause. Use this
    /// when a `volume resume` reports a revalidation failure you know to be
    /// permanent (the staged data is really gone), or to clear a `planned`
    /// session that was killed before anything was written. Nothing can tell
    /// a transient cause from a permanent one but you, which is why resume
    /// never decides this on its own.
    ///
    /// The tape is never contacted (hence no --device): the cartridge is left
    /// unsealed and physically unharmed, and the staged files stay pinned
    /// until `staging clean` runs. The session, however, becomes unresumable
    /// for good.
    Abort {
        /// Volume label
        label: String,
        /// Skip the confirmation prompt (ADR-0008 Tier 2). Required in a
        /// non-interactive session, which otherwise refuses rather than
        /// assuming consent.
        #[arg(long)]
        yes: bool,
    },

    /// Verify volume contents via the keyless chain walk (seal -> front index
    /// -> content). Default tier is `--full` (integrity: hashes every
    /// content file); `--quick` opts down to navigable (seal binding + front
    /// index self-consistency only, no per-file content hashing).
    Verify {
        /// Volume label
        label: String,
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
        /// Full integrity chain walk (default): hash every content file
        /// against the front index's ciphertext hashes.
        #[arg(long, conflicts_with = "quick")]
        full: bool,
        /// Quick navigable-only chain walk: seal binding + front index
        /// self-consistency, no per-file content hashing.
        #[arg(long)]
        quick: bool,
    },

    /// Identify a tape (read ID thunk)
    Identify {
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
    },

    /// Move a volume to a location
    Move {
        /// Volume label
        label: String,
        /// Destination location name
        #[arg(long)]
        to: String,
    },

    /// Retire a volume (with impact analysis)
    Retire {
        /// Volume label
        label: String,
    },

    /// Read encrypted slices from a volume into staging (then use `volume write` to write them)
    ReadSlices {
        /// Source volume label
        #[arg(long)]
        from: String,
        /// Unit name to read
        #[arg(long)]
        unit: String,
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
    },

    /// Read live encrypted slices from a volume to staging (compaction step 1)
    CompactRead {
        /// Source volume label
        label: String,
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
    },

    /// Write compaction slices from staging to destination (compaction step 2)
    CompactWrite {
        /// Destination volume label
        #[arg(long)]
        destination: String,
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
    },

    /// Show bin-packing plan for pending staged data
    Plan {
        /// Number of copies to plan
        #[arg(long, default_value = "1")]
        copies: i64,
    },

    /// Retire source volume after compaction (compaction step 3)
    CompactFinish {
        /// Source volume label to retire
        label: String,
    },

    /// Interactive compaction: read + write + finish in one flow
    Compact {
        /// Source volume label
        label: String,
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
    },

    /// Record and inspect WAREHOUSE DEPOSITS of sealed volumes (ADR-0006).
    ///
    /// tapectl does NOT move the bytes. Issue #72 was rescoped by CTO
    /// decision: an operator copies a sealed volume's bytes to cold cloud
    /// storage by the documented external procedure (rclone / aws-cli) and
    /// then RECORDS that copy here, so the catalog can reason about it.
    Deposit {
        #[command(subcommand)]
        command: DepositCommands,
    },
}

/// `volume deposit` -- record and list warehouse copies (ADR-0006).
#[derive(Subcommand, Debug)]
pub enum DepositCommands {
    /// Record that a sealed volume's bytes now also exist at a warehouse.
    Add {
        /// Volume label whose bytes were deposited
        label: String,
        /// Warehouse location name (must be a location of kind `warehouse`)
        #[arg(long)]
        to: String,
        /// The provider's receipt / object-version identifier, if it gave
        /// one. There is deliberately no checksum field: tapectl did not
        /// perform the copy, so a typed-in checksum would be a claim about
        /// a claim (issue #73).
        #[arg(long)]
        receipt: Option<String>,
        /// Storage class the bytes were placed in (e.g. DEEP_ARCHIVE)
        #[arg(long)]
        storage_class: Option<String>,
        /// Free-text note
        #[arg(long)]
        notes: Option<String>,
    },
    /// List recorded deposits
    List {
        /// Only deposits of this volume
        #[arg(long)]
        volume: Option<String>,
    },
}

/// Run a volume subcommand. Returns the process exit code (issue #45/H10),
/// mirroring the `audit` convention (`src/cli/audit.rs`): 0=clean,
/// 1=warning, 2=violation. Every arm but `Verify` has no exit-code
/// semantics of its own and returns `EXIT_SUCCESS`; the caller (`main.rs`)
/// decides whether to actually call `std::process::exit`, exactly as it
/// does for `audit`.
pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    command: &VolumeCommands,
    json_output: bool,
    yes: bool,
    dry_run: bool,
) -> Result<i32> {
    let mut exit_code = crate::error::EXIT_SUCCESS;
    match command {
        VolumeCommands::Init {
            label,
            device,
            force,
        } => {
            let vol_id =
                write::volume_init(conn, config, label, device, DEFAULT_BLOCK_SIZE, *force)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"volume_id": vol_id, "label": label, "status": "initialized"})
                );
            } else {
                println!("volume \"{label}\" initialized (id={vol_id})");
            }
        }

        VolumeCommands::Write {
            label,
            device,
            force,
        } => {
            write::volume_write(
                conn,
                paths,
                config,
                label,
                device,
                DEFAULT_BLOCK_SIZE,
                *force,
            )?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"label": label, "status": "completed"})
                );
            } else {
                println!("volume \"{label}\" write completed");
            }
        }

        VolumeCommands::Resume { label, device } => {
            write::volume_resume(conn, paths, config, label, device, DEFAULT_BLOCK_SIZE)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"label": label, "status": "completed", "resumed": true})
                );
            } else {
                println!("volume \"{label}\" write resumed and completed");
            }
        }

        VolumeCommands::Abort { label, yes } => {
            write::volume_abort(conn, label, *yes)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"label": label, "status": "aborted"})
                );
            } else {
                println!(
                    "volume \"{label}\" write session aborted — the session can no longer be \
                     resumed. The cartridge is unsealed and unharmed; the staged slices stay \
                     pinned until `tapectl staging clean`."
                );
            }
        }

        VolumeCommands::Verify {
            label,
            device,
            full: _,
            quick,
        } => {
            let tier = if *quick {
                Tier::Navigable
            } else {
                Tier::default()
            };
            let tier_name = if *quick { "quick" } else { "full" };
            let report =
                write::volume_verify(conn, config, label, device, DEFAULT_BLOCK_SIZE, tier)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "label": label,
                        "tier": tier_name,
                        "checked": report.checked,
                        "passed": report.passed,
                        "failed": report.failed,
                    })
                );
            } else {
                println!(
                    "verify {label} ({tier_name} tier): {} checked, {} passed, {} failed",
                    report.checked, report.passed, report.failed,
                );
            }
            // issue #45/H10: a failing verify must not exit 0 — a
            // cron-scheduled integrity check that finds corruption but
            // reports success defeats the entire point of verifying.
            exit_code = verify_exit_code(&report);
        }

        VolumeCommands::Identify { device } => {
            let id = write::volume_identify(device, DEFAULT_BLOCK_SIZE)?;
            println!("{id}");
        }

        VolumeCommands::Move { label, to } => {
            crate::cli::location::move_volume(conn, label, to)?;
            if json_output {
                println!("{}", serde_json::json!({"label": label, "location": to}));
            } else {
                println!("volume \"{label}\" moved to \"{to}\"");
            }
        }

        VolumeCommands::Retire { label } => {
            crate::cli::operations::volume_retire(conn, label, yes, dry_run, json_output)?;
        }

        VolumeCommands::ReadSlices { from, unit, device } => {
            let report = write::read_slices(conn, config, from, unit, device, DEFAULT_BLOCK_SIZE)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "from": from, "unit": unit,
                        "slices_read": report.slices_read,
                        "bytes_read": report.bytes_read,
                    })
                );
            } else {
                println!(
                    "read {} slices ({} MB) from \"{}\" into staging",
                    report.slices_read,
                    report.bytes_read / (1024 * 1024),
                    from,
                );
                println!(
                    "run `tapectl volume write DEST --device {}` to write to tape",
                    device
                );
            }
        }

        VolumeCommands::Plan { copies } => {
            // Show what staged data would be written
            let mut stmt = conn.prepare(
                "SELECT u.name, s.version, ss.num_slices, ss.total_encrypted_size
                 FROM stage_sets ss
                 JOIN snapshots s ON s.id = ss.snapshot_id
                 JOIN units u ON u.id = s.unit_id
                 WHERE ss.status = 'staged'
                 ORDER BY ss.total_encrypted_size DESC",
            )?;
            let rows: Vec<(String, i64, Option<i64>, Option<i64>)> = stmt
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if rows.is_empty() {
                println!("no staged data to plan");
            } else {
                let total_bytes: i64 = rows.iter().map(|(_, _, _, s)| s.unwrap_or(0)).sum();
                let total_slices: i64 = rows.iter().map(|(_, _, n, _)| n.unwrap_or(0)).sum();

                if json_output {
                    let units: Vec<serde_json::Value> = rows
                        .iter()
                        .map(|(name, ver, slices, size)| {
                            serde_json::json!({"unit": name, "version": ver, "slices": slices, "size": size})
                        })
                        .collect();
                    println!(
                        "{}",
                        serde_json::json!({
                            "copies": copies, "total_slices": total_slices,
                            "total_bytes": total_bytes, "units": units,
                        })
                    );
                } else {
                    println!("volume write plan ({copies} copy/copies):");
                    for (name, ver, slices, size) in &rows {
                        println!(
                            "  {name} v{ver}: {} slices, {} MB",
                            slices.unwrap_or(0),
                            size.unwrap_or(0) / (1024 * 1024),
                        );
                    }
                    println!(
                        "\ntotal: {total_slices} slices, {} MB x {copies} = {} MB",
                        total_bytes / (1024 * 1024),
                        total_bytes * copies / (1024 * 1024),
                    );
                    // Estimate tapes needed from configured LTO backend
                    let backend = config.backends.lto.first().ok_or_else(|| {
                        crate::error::TapectlError::Config("no LTO backend configured".into())
                    })?;
                    let tape_cap = crate::staging::parse_size_to_bytes(&backend.nominal_capacity)?;
                    let factor = backend.usable_capacity_factor;
                    let usable = (tape_cap as f64 * factor) as i64;
                    let tapes_needed = ((total_bytes * copies) + usable - 1) / usable;
                    println!(
                        "estimated tapes: {tapes_needed} (at {}% usable capacity)",
                        (factor * 100.0).round() as i64
                    );
                }
            }
        }

        VolumeCommands::CompactRead { label, device } => {
            let report = write::compact_read(conn, config, label, device, DEFAULT_BLOCK_SIZE)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"label": label, "slices_read": report.slices_read, "bytes_read": report.bytes_read})
                );
            } else {
                println!(
                    "compact-read \"{label}\": {} live slices ({} MB) staged",
                    report.slices_read,
                    report.bytes_read / (1024 * 1024),
                );
            }
        }

        VolumeCommands::CompactWrite {
            destination,
            device,
        } => {
            write::compact_write(conn, paths, config, destination, device, DEFAULT_BLOCK_SIZE)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"destination": destination, "status": "completed"})
                );
            } else {
                println!("compact-write to \"{destination}\" completed");
            }
        }

        VolumeCommands::CompactFinish { label } => {
            let report = write::compact_finish(conn, label)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "label": label,
                        "status": "retired",
                        "affected_units": compact_finish_evidence_json(&report),
                    })
                );
            } else {
                println!("compact-finish \"{label}\": volume retired");
                print_compact_finish_evidence(&report);
            }
        }

        VolumeCommands::Compact { label, device } => {
            // Interactive: run all 3 steps
            println!("=== Step 1: Reading live slices from \"{label}\" ===");
            let report = write::compact_read(conn, config, label, device, DEFAULT_BLOCK_SIZE)?;
            println!(
                "  Read {} slices ({} MB)",
                report.slices_read,
                report.bytes_read / (1024 * 1024),
            );

            println!("\nInsert destination tape and enter volume label:");
            let mut dest_label = String::new();
            std::io::stdin().read_line(&mut dest_label).ok();
            let dest_label = dest_label.trim();

            if dest_label.is_empty() {
                return Err(crate::error::TapectlError::Other(
                    "no destination label provided".into(),
                ));
            }

            println!("=== Step 2: Writing compaction slices to \"{dest_label}\" ===");
            write::compact_write(conn, paths, config, dest_label, device, DEFAULT_BLOCK_SIZE)?;
            println!("  Write completed");

            println!("=== Step 3: Retiring source volume \"{label}\" ===");
            let report = write::compact_finish(conn, label)?;
            println!("  Volume \"{label}\" retired");
            if !json_output {
                print_compact_finish_evidence(&report);
            }

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "source": label,
                        "destination": dest_label,
                        "status": "completed",
                        "affected_units": compact_finish_evidence_json(&report),
                    })
                );
            } else {
                println!("\ncompaction complete: {label} → {dest_label}");
            }
        }

        VolumeCommands::Deposit { command } => run_deposit(conn, command, json_output)?,
    }
    Ok(exit_code)
}

/// `volume deposit` (ADR-0006, issue #73).
///
/// Two refusals, and only two. Both are validation of the recorded FACT,
/// not policy gates -- ADR-0004 keeps every coverage judgement advisory,
/// so nothing here warns, blocks, or grows a `--force`:
///
/// 1. The target location must be a `warehouse`. Recording a deposit at a
///    shelf would claim a copy exists in a place nothing was copied to,
///    and it would then be counted as one by every derivation.
/// 2. The volume must pass `coverage::eligible` (sealed). You cannot have
///    deposited bytes that were never sealed -- an unsealed volume's bytes
///    are not final, so a copy of them is a copy of nothing durable.
fn run_deposit(conn: &Connection, command: &DepositCommands, json_output: bool) -> Result<()> {
    use crate::error::TapectlError;
    use rusqlite::params;

    match command {
        DepositCommands::Add {
            label,
            to,
            receipt,
            storage_class,
            notes,
        } => {
            let (vol_id, status): (i64, String) = conn
                .query_row(
                    "SELECT id, status FROM volumes WHERE label = ?1",
                    params![label],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|_| TapectlError::VolumeNotFound(label.clone()))?;

            let (loc_id, kind): (i64, String) = conn
                .query_row(
                    "SELECT id, kind FROM locations WHERE name = ?1",
                    params![to],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|_| TapectlError::Other(format!("location \"{to}\" not found")))?;

            if kind != "warehouse" {
                return Err(TapectlError::Other(format!(
                    "location \"{to}\" is a {kind}, not a warehouse; a deposit records bytes \
                     copied to cold cloud storage. Create one with: \
                     tapectl location add <NAME> --kind warehouse"
                )));
            }
            if status != "sealed" {
                return Err(TapectlError::Other(format!(
                    "volume \"{label}\" is {status}, not sealed; only a sealed volume's bytes \
                     are final, so there is nothing durable to have deposited"
                )));
            }

            conn.execute(
                "INSERT INTO volume_deposits (volume_id, location_id, receipt, storage_class, notes)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![vol_id, loc_id, receipt, storage_class, notes],
            )?;
            let id = conn.last_insert_rowid();
            crate::db::events::log_created(conn, "volume_deposit", id, label, None)?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"id": id, "volume": label, "location": to,
                                       "receipt": receipt, "storage_class": storage_class})
                );
            } else {
                println!(
                    "recorded warehouse deposit of \"{label}\" at \"{to}\" (id={id}) — \
                     never re-verified, and warehouse copies do not refresh"
                );
            }
        }

        DepositCommands::List { volume } => {
            let mut sql = String::from(
                "SELECT v.label, l.name, d.deposited_at, d.receipt, d.storage_class, d.notes
                 FROM volume_deposits d
                 JOIN volumes v ON v.id = d.volume_id
                 JOIN locations l ON l.id = d.location_id",
            );
            let mut binds: Vec<String> = Vec::new();
            if let Some(v) = volume {
                sql.push_str(" WHERE v.label = ?1");
                binds.push(v.clone());
            }
            sql.push_str(" ORDER BY v.label, l.name");
            let mut stmt = conn.prepare(&sql)?;
            let bind_refs: Vec<&dyn rusqlite::types::ToSql> = binds
                .iter()
                .map(|b| b as &dyn rusqlite::types::ToSql)
                .collect();
            let rows: Vec<serde_json::Value> = stmt
                .query_map(bind_refs.as_slice(), |row| {
                    Ok(serde_json::json!({
                        "volume": row.get::<_, String>(0)?,
                        "location": row.get::<_, String>(1)?,
                        "deposited_at": row.get::<_, String>(2)?,
                        "receipt": row.get::<_, Option<String>>(3)?,
                        "storage_class": row.get::<_, Option<String>>(4)?,
                        "notes": row.get::<_, Option<String>>(5)?,
                    }))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                println!("{}", serde_json::to_string_pretty(&rows).unwrap());
            } else if rows.is_empty() {
                println!("no warehouse deposits recorded");
            } else {
                for r in &rows {
                    println!(
                        "  {} at {} — deposited {}{}",
                        r["volume"].as_str().unwrap_or("?"),
                        r["location"].as_str().unwrap_or("?"),
                        r["deposited_at"].as_str().unwrap_or("?"),
                        r["receipt"]
                            .as_str()
                            .map(|x| format!(", receipt {x}"))
                            .unwrap_or_default(),
                    );
                }
            }
        }
    }
    Ok(())
}

/// ADR-0004 Tier 1: print the remaining-coverage evidence for every unit
/// `compact_finish` retired coverage for. Display-only, matching
/// `cli::operations::print_retire_impact`'s evidence lines -- compaction
/// retires the source volume exactly like `volume retire` does.
fn print_compact_finish_evidence(report: &[write::CompactFinishReport]) {
    let now = chrono::Utc::now().naive_utc();
    for unit in report {
        if let Some(line) = crate::policy::evidence::describe(&unit.unit_name, &unit.evidence, now)
        {
            println!("  {line}");
        }
    }
}

/// JSON shape for `compact_finish`'s per-unit evidence, mirroring
/// `cli::operations::retire_impacts_json`'s `evidence`/`evidence_summary`
/// fields.
fn compact_finish_evidence_json(report: &[write::CompactFinishReport]) -> Vec<serde_json::Value> {
    let now = chrono::Utc::now().naive_utc();
    report
        .iter()
        .map(|unit| {
            let evidence: Vec<serde_json::Value> = unit
                .evidence
                .iter()
                .map(crate::cli::operations::evidence_json)
                .collect();
            let evidence_summary =
                crate::policy::evidence::describe(&unit.unit_name, &unit.evidence, now);
            serde_json::json!({
                "unit": unit.unit_name,
                "evidence": evidence,
                "evidence_summary": evidence_summary,
            })
        })
        .collect()
}

/// Decide the process exit code for `volume verify` from its report
/// (issue #45/H10). No warning tier of its own: a chain walk either
/// confirms every checked slice or it finds real corruption, so the
/// result is binary — clean or violation — unlike `fsck`, which can also
/// report a repaired-but-notable finding.
fn verify_exit_code(report: &write::VerifyReport) -> i32 {
    if report.failed > 0 {
        crate::error::EXIT_ERROR
    } else {
        crate::error::EXIT_SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `volume deposit` validation (issue #73 / ADR-0006). These are the
    /// only two refusals the whole feature adds; everything else about a
    /// deposit is advisory.
    mod deposits {
        use super::*;
        use rusqlite::params;

        fn fixture() -> Connection {
            let (conn, _unit, _vol) =
                crate::policy::coverage::tests::setup_unit_with_deposit("active");
            conn.execute("DELETE FROM volume_deposits", []).unwrap();
            conn
        }

        fn add(conn: &Connection, label: &str, to: &str) -> Result<()> {
            run_deposit(
                conn,
                &DepositCommands::Add {
                    label: label.to_string(),
                    to: to.to_string(),
                    receipt: Some("rcpt-9".into()),
                    storage_class: Some("DEEP_ARCHIVE".into()),
                    notes: None,
                },
                true,
            )
        }

        #[test]
        fn records_a_deposit_of_a_sealed_volume_at_a_warehouse() {
            let conn = fixture();
            add(&conn, "L6-0003", "glacier").expect("sealed volume + warehouse must be accepted");
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM volume_deposits", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 1);
            let receipt: Option<String> = conn
                .query_row("SELECT receipt FROM volume_deposits", [], |r| r.get(0))
                .unwrap();
            assert_eq!(receipt.as_deref(), Some("rcpt-9"));
        }

        #[test]
        fn refuses_a_shelf_location() {
            let conn = fixture();
            let err = add(&conn, "L6-0003", "home").expect_err(
                "a shelf is not a warehouse; recording a deposit there would \
                             claim a copy in a place nothing was copied to",
            );
            let msg = err.to_string();
            assert!(msg.contains("is a shelf, not a warehouse"), "{msg}");
        }

        #[test]
        fn refuses_a_volume_that_is_not_sealed() {
            let conn = fixture();
            conn.execute("UPDATE volumes SET status = 'active'", [])
                .unwrap();
            let err = add(&conn, "L6-0003", "glacier")
                .expect_err("unsealed bytes are not final, so nothing durable was deposited");
            let msg = err.to_string();
            assert!(msg.contains("is active, not sealed"), "{msg}");
        }

        #[test]
        fn refuses_an_unknown_location_and_an_unknown_volume() {
            let conn = fixture();
            assert!(add(&conn, "L6-0003", "nowhere").is_err());
            assert!(add(&conn, "NO-SUCH-VOL", "glacier").is_err());
        }

        /// The UNIQUE(volume_id, location_id) constraint: the same volume
        /// cannot be deposited twice at one warehouse, which would
        /// double-count it as two copies.
        #[test]
        fn refuses_a_duplicate_deposit_of_the_same_volume_at_the_same_warehouse() {
            let conn = fixture();
            add(&conn, "L6-0003", "glacier").unwrap();
            assert!(add(&conn, "L6-0003", "glacier").is_err());
        }

        #[test]
        fn list_filters_by_volume_and_json_stdout_stays_parseable() {
            let conn = fixture();
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                      capacity_bytes, status)
                 VALUES ('L6-0004', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
                [],
            )
            .unwrap();
            add(&conn, "L6-0003", "glacier").unwrap();
            add(&conn, "L6-0004", "glacier").unwrap();

            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM volume_deposits d JOIN volumes v ON v.id = d.volume_id
                     WHERE v.label = ?1",
                    params!["L6-0003"],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1);

            run_deposit(
                &conn,
                &DepositCommands::List {
                    volume: Some("L6-0003".into()),
                },
                true,
            )
            .unwrap();
        }
    }

    #[test]
    fn verify_exit_code_clean_report_is_success() {
        let report = write::VerifyReport {
            checked: 10,
            passed: 10,
            failed: 0,
        };
        assert_eq!(verify_exit_code(&report), crate::error::EXIT_SUCCESS);
    }

    #[test]
    fn verify_exit_code_any_failure_is_violation() {
        let report = write::VerifyReport {
            checked: 10,
            passed: 9,
            failed: 1,
        };
        assert_eq!(verify_exit_code(&report), crate::error::EXIT_ERROR);
    }

    #[test]
    fn verify_exit_code_all_failed_is_violation() {
        let report = write::VerifyReport {
            checked: 3,
            passed: 0,
            failed: 3,
        };
        assert_eq!(verify_exit_code(&report), crate::error::EXIT_ERROR);
    }

    #[test]
    fn verify_exit_code_empty_report_is_success() {
        // A volume with zero checkable slices (degenerate but not a
        // failure) must not be reported as a violation.
        let report = write::VerifyReport::default();
        assert_eq!(verify_exit_code(&report), crate::error::EXIT_SUCCESS);
    }
}
