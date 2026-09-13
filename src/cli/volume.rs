use clap::Subcommand;
use rusqlite::Connection;

use crate::cli::{read_device, write_device};
use crate::config::{Config, TapectlPaths};
use crate::error::Result;
use crate::store::{TapeStore, Tier};
use crate::volume::write;

pub(crate) const DEFAULT_BLOCK_SIZE: usize = 512 * 1024; // 512 KB

#[derive(Subcommand, Debug)]
pub enum VolumeCommands {
    /// Initialize a new volume (write ID thunk to tape)
    Init {
        /// Volume label (e.g., L6-0001)
        label: String,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
        /// Overwrite a cartridge whose File 0 already identifies a
        /// DIFFERENT volume (e.g. a mislabeled or stale tape). Refused by
        /// default (issue #27) — loading the wrong cartridge would
        /// otherwise silently overwrite it. Never overrides a cartridge
        /// that is already SEALED (ADR-0003): bulk-erase the physical tape
        /// and run `cartridge mark-erased` first for that case. It also
        /// never overrides the drive/media compatibility refusal, which is
        /// a physical fact rather than a risk judgement (ADR-0010).
        #[arg(long)]
        force: bool,
        /// Declare the loaded medium's generation (e.g. LTO-6, LTO-7-M8).
        ///
        /// Normally unnecessary and normally ignored: ADR-0010 DETECTS the
        /// generation from the drive (MAM medium density code, else MAM
        /// format density code, else the st driver's density register), and
        /// a detected code is a fact about the tape that this flag cannot
        /// override — a `--media` contradicting one is an error, not a hint.
        /// It is consulted only when no source reports a recognised code.
        #[arg(long)]
        media: Option<String>,
        /// Bind this volume to an already-registered cartridge by barcode.
        ///
        /// Normally unnecessary: `volume init` matches the loaded medium's
        /// MAM serial to `cartridges.serial_number`, and auto-registers a
        /// cartridge (barcode = that serial) when nothing matches. Use this
        /// to attach the volume to a cartridge YOU labelled — the barcode on
        /// the physical sticker — instead. A serial match wins over this
        /// flag, since the serial was read off the medium.
        #[arg(long)]
        cartridge: Option<String>,
    },

    /// Write staged data to volume
    Write {
        /// Volume label
        label: String,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
        /// See `volume init --force` — same override, same limits.
        #[arg(long)]
        force: bool,
        /// Seal slices that were staged before the escrow recipient existed
        /// (ADR-0005). Only reachable for tapes copied forward via
        /// `read-slices`/`compact-read`, since `stage create` refuses to
        /// stage without escrow; use it to migrate a dying pre-escrow
        /// cartridge, knowing the copy stays unrecoverable by the escrow key.
        #[arg(long)]
        allow_missing_escrow: bool,
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
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
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
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
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
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
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
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
    },

    /// Read live encrypted slices from a volume to staging (compaction step 1)
    CompactRead {
        /// Source volume label
        label: String,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
    },

    /// Write compaction slices from staging to destination (compaction step 2)
    CompactWrite {
        /// Destination volume label
        #[arg(long)]
        destination: String,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
        /// See `volume write --allow-missing-escrow`. A compaction whose
        /// source volume predates the escrow recipient needs this to proceed.
        #[arg(long)]
        allow_missing_escrow: bool,
    },

    /// Show bin-packing plan for pending staged data
    Plan {
        /// Number of copies to plan
        #[arg(long, default_value = "1")]
        copies: i64,
        /// Estimate against this media generation rather than the drive's
        /// own (ADR-0010) — e.g. counting LTO-5 cartridges for an LTO-6
        /// drive. No cartridge need be loaded; this is an estimate, and the
        /// authoritative figure is each volume's own `capacity_bytes` once
        /// `volume init` has detected the medium it is actually on.
        #[arg(long)]
        media: Option<String>,
    },

    /// Retire source volume after compaction (compaction step 3)
    CompactFinish {
        /// Source volume label to retire
        label: String,
        /// Proceed even when a unit is left with no other copy (ADR-0008
        /// Tier 2, issue #147 — see cli::consent). It does NOT defeat the
        /// Tier-3 refusal: a live slice with no copy anywhere still stops
        /// the retirement outright, and nothing waives that.
        #[arg(long)]
        force: bool,
    },

    /// Interactive compaction: read + write + finish in one flow
    ///
    /// Step 3 applies `compact-finish`'s ADR-0008 Tier-2 gate and may
    /// refuse non-interactively without `--force`. When it does, the
    /// destination tape is already written and sealed and nothing is lost:
    /// `volume compact-finish <SOURCE> --force` completes the flow without
    /// re-reading or re-writing anything.
    Compact {
        /// Source volume label
        label: String,
        /// Destination volume label. Without it the flow PROMPTS for one,
        /// which needs a terminal — pass this to run compaction
        /// non-interactively (issue #146, ADR-0008's non-hanging rule).
        #[arg(long)]
        to: Option<String>,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
        /// See `volume write --allow-missing-escrow`.
        #[arg(long)]
        allow_missing_escrow: bool,
        /// See `volume compact-finish --force` — step 3's ADR-0008 Tier-2
        /// gate. With `--to` and this (or the global `--yes`), the whole
        /// three-step flow runs non-interactively.
        #[arg(long)]
        force: bool,
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
    /// Un-record a deposit whose bytes are gone from the warehouse.
    ///
    /// ADR-0006 names the reason this must exist: "a warehouse copy dies
    /// weeks after payment stops". A lapsed bill or a provider lifecycle
    /// rule deletes the object without telling tapectl, and a deposit row
    /// that outlives its bytes keeps counting as a copy at exactly the
    /// gates that decide whether local data may be deleted.
    Remove {
        /// Volume label whose deposit is gone
        label: String,
        /// Warehouse location the deposit was recorded at
        #[arg(long)]
        from: String,
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
            media,
            cartridge,
        } => {
            let device = write_device(config, device.as_deref())?;
            let vol_id = write::volume_init(
                conn,
                config,
                label,
                &device,
                DEFAULT_BLOCK_SIZE,
                *force,
                media.as_deref(),
                cartridge.as_deref(),
            )?;
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
            allow_missing_escrow,
        } => {
            let device = write_device(config, device.as_deref())?;
            write::volume_write(
                conn,
                paths,
                config,
                label,
                &device,
                DEFAULT_BLOCK_SIZE,
                *force,
                *allow_missing_escrow,
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
            let device = write_device(config, device.as_deref())?;
            write::volume_resume(conn, paths, config, label, &device, DEFAULT_BLOCK_SIZE)?;
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
            let device = read_device(config, device.as_deref())?;
            let report =
                write::volume_verify(conn, config, label, &device, DEFAULT_BLOCK_SIZE, tier)?;
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
            let device = read_device(config, device.as_deref())?;
            let mut store = TapeStore::open_read(&device, DEFAULT_BLOCK_SIZE)?;
            let id = write::volume_identify(&mut store)?;
            println!("{id}");
        }

        VolumeCommands::Move { label, to } => {
            let outcome = crate::cli::location::move_volume(conn, label, to)?;
            if json_output {
                // `cartridge`/`volumes_moved` are ADDITIVE (ADR-0011): the
                // move now carries the cartridge and any other volume on it,
                // and a consumer that only reads `label`/`location` sees
                // exactly what it saw before.
                println!(
                    "{}",
                    serde_json::json!({
                        "label": label,
                        "location": to,
                        "cartridge": outcome.cartridge,
                        "volumes_moved": outcome.volumes,
                    })
                );
            } else {
                println!("volume \"{label}\" moved to \"{to}\"");
                if let Some(barcode) = &outcome.cartridge {
                    println!("  cartridge \"{barcode}\" moved with it");
                    let others: Vec<&String> =
                        outcome.volumes.iter().filter(|l| *l != label).collect();
                    if !others.is_empty() {
                        println!(
                            "  {} other volume(s) on that cartridge moved too: {}",
                            others.len(),
                            others
                                .iter()
                                .map(|s| s.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                }
            }
        }

        VolumeCommands::Retire { label } => {
            crate::cli::operations::volume_retire(conn, label, yes, dry_run, json_output)?;
        }

        VolumeCommands::ReadSlices { from, unit, device } => {
            let device = read_device(config, device.as_deref())?;
            let mut store = TapeStore::open_read(&device, DEFAULT_BLOCK_SIZE)?;
            let report = write::read_slices(conn, config, from, unit, &mut store)?;
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

        VolumeCommands::Plan { copies, media } => {
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
                    // Estimate tapes needed from the configured LTO backend.
                    // ADR-0010: the figure follows the GENERATION being
                    // planned for (`--media`, else the drive's own), not a
                    // capacity declared on the drive.
                    let backend = crate::config::resolve_lto_backend(config, None)?;
                    let tape_cap = backend.planning_capacity_bytes(media.as_deref())? as i64;
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
            let device = read_device(config, device.as_deref())?;
            let mut store = TapeStore::open_read(&device, DEFAULT_BLOCK_SIZE)?;
            let report = write::compact_read(conn, config, label, &mut store)?;
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
            allow_missing_escrow,
        } => {
            let device = write_device(config, device.as_deref())?;
            write::compact_write(
                conn,
                paths,
                config,
                destination,
                &device,
                DEFAULT_BLOCK_SIZE,
                *allow_missing_escrow,
            )?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"destination": destination, "status": "completed"})
                );
            } else {
                println!("compact-write to \"{destination}\" completed");
            }
        }

        VolumeCommands::CompactFinish { label, force } => {
            let report = write::compact_finish(conn, label, *force || yes)?;
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

        VolumeCommands::Compact {
            label,
            to,
            device,
            allow_missing_escrow,
            force,
        } => {
            // Interactive: run all 3 steps. Strict resolution (ADR-0010):
            // step 2 writes, so this needs a real backend even though step 1
            // only reads.
            let device = write_device(config, device.as_deref())?;
            println!("=== Step 1: Reading live slices from \"{label}\" ===");
            // Scoped so the read-only store (and its device fd) closes
            // before step 2 opens the same device for writing — the st
            // driver refuses a second concurrent open (EBUSY).
            let report = {
                let mut store = TapeStore::open_read(&device, DEFAULT_BLOCK_SIZE)?;
                write::compact_read(conn, config, label, &mut store)?
            };
            println!(
                "  Read {} slices ({} MB)",
                report.slices_read,
                report.bytes_read / (1024 * 1024),
            );

            let dest_label = resolve_compact_destination(to.as_deref())?;
            let dest_label = dest_label.as_str();

            println!("=== Step 2: Writing compaction slices to \"{dest_label}\" ===");
            write::compact_write(
                conn,
                paths,
                config,
                dest_label,
                &device,
                DEFAULT_BLOCK_SIZE,
                *allow_missing_escrow,
            )?;
            println!("  Write completed");

            println!("=== Step 3: Retiring source volume \"{label}\" ===");
            // Step 2 has already written and sealed the destination by now,
            // so a step-3 consent refusal must not read as "the compaction
            // failed" — it read as that in every earlier draft, and the
            // operator's rational response would have been to redo a
            // multi-hour write that had already succeeded.
            //
            // A PRE-FLIGHT gate was considered and rejected: before step 2
            // the destination holds nothing, so `retire_impacts` reports
            // ZERO other copies for every LIVE unit on the source (its only
            // copy IS the source, which is what compaction is about to fix).
            // Gating there would demand `--force` for ordinary compaction
            // and invert the gate's meaning. Only after step 2 does the
            // at-risk set narrow to units whose content was not carried
            // forward — exactly the issue #147 case that should gate.
            let report = write::compact_finish(conn, label, *force || yes).inspect_err(|e| {
                // ONLY the consent refusal. `compact_finish`'s other
                // failure is the Tier-3 refusal, and after a successful
                // compact-write that means a live slice was not carried
                // forward — a bug, not a `--force` situation. Naming
                // `--force` as the recovery for it is precisely the
                // confusion ADR-0008 warns about.
                let msg = e.to_string();
                if msg.contains("refused: non-interactive session")
                    || msg.contains("aborted, not confirmed")
                {
                    eprintln!(
                        "\nNothing was lost: destination \"{dest_label}\" is written and \
                         sealed, and source \"{label}\" is simply not retired yet.\n\
                         To complete step 3 without re-reading or re-writing anything:\n    \
                         tapectl volume compact-finish {label} --force"
                    );
                }
            })?;
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

        DepositCommands::Remove { label, from } => {
            let deleted = conn.execute(
                "DELETE FROM volume_deposits
                  WHERE volume_id = (SELECT id FROM volumes WHERE label = ?1)
                    AND location_id = (SELECT id FROM locations WHERE name = ?2)",
                params![label, from],
            )?;
            if deleted == 0 {
                return Err(TapectlError::Other(format!(
                    "no recorded deposit of \"{label}\" at \"{from}\""
                )));
            }
            crate::db::events::log_event(
                conn,
                "volume_deposit",
                0,
                Some(label),
                "deleted",
                None,
                None,
                None,
                Some(&format!("deposit at {from} un-recorded")),
                None,
            )?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"volume": label, "location": from, "removed": deleted})
                );
            } else {
                println!(
                    "removed the recorded deposit of \"{label}\" at \"{from}\"; \
                     it no longer counts as a copy"
                );
            }
        }
    }
    Ok(())
}

/// ADR-0004 Tier 1: print the remaining-coverage evidence for every unit
/// `compact_finish` retired coverage for. Display-only, matching
/// `cli::operations::print_retire_impact`'s evidence lines -- compaction
/// retires the source volume exactly like `volume retire` does.
/// The compaction destination label, resolved WITHOUT ever blocking on a
/// prompt nobody can answer (issue #146).
///
/// `volume compact`'s step-2 destination used to be a bare
/// `stdin().read_line()` with no terminal check, and the global `--yes` did
/// not skip it — so a cron job, the mhvtl verify gate, or any redirected
/// run hung here forever. That is precisely the failure shape ADR-0008
/// names ("blocking forever on a handle that will never produce input",
/// the issue #33 class), and its rule is absolute: refuse with a non-zero
/// exit rather than wait.
///
/// This is NOT routed through `cli::consent::confirm`. That is the Tier-2
/// y/N gate; this is a VALUE the operator has to supply, and there is no
/// safe default to assume — `--yes` cannot invent a label. So the terminal
/// check is the same (`std::io::IsTerminal`, no new dependency) and the
/// override is `--to`.
fn resolve_compact_destination(to: Option<&str>) -> Result<String> {
    use std::io::IsTerminal;
    resolve_compact_destination_with(to, std::io::stdin().is_terminal(), || {
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        Ok(input)
    })
}

/// [`resolve_compact_destination`] with the TTY answer and the read
/// injected, so the non-interactive branch can be proven never to attempt
/// the read that would hang — the same testing discipline as
/// `cli::consent::confirm_with`.
fn resolve_compact_destination_with(
    to: Option<&str>,
    is_tty: bool,
    read_answer: impl FnOnce() -> Result<String>,
) -> Result<String> {
    if let Some(label) = to {
        let label = label.trim();
        if label.is_empty() {
            return Err(crate::error::TapectlError::Other(
                "--to was given an empty destination label".into(),
            ));
        }
        return Ok(label.to_string());
    }

    if !is_tty {
        return Err(crate::error::TapectlError::Other(
            "volume compact refused: a destination label is required and stdin is not a \
             terminal, so there is nobody to prompt — re-run with `--to <LABEL>` (the \
             destination tape must already be initialised)"
                .into(),
        ));
    }

    println!("\nInsert destination tape and enter volume label:");
    let input = read_answer()?;
    let label = input.trim();
    if label.is_empty() {
        return Err(crate::error::TapectlError::Other(
            "no destination label provided".into(),
        ));
    }
    Ok(label.to_string())
}

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
            // `status`/`remaining_copies` are ADDITIVE (issue #147), and
            // named exactly as `operations::retire_impacts_json` names the
            // same two facts — the shapes come from one derivation now, so
            // they should read alike.
            serde_json::json!({
                "unit": unit.unit_name,
                "status": unit.unit_status,
                "remaining_copies": unit.other_copies,
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

    /// Issue #146: `volume compact`'s destination-label prompt had no
    /// terminal check, so a non-interactive run blocked forever on a handle
    /// nobody would ever write to — the issue #33 failure shape ADR-0008
    /// names by name. Every test here drives `resolve_compact_destination_with`
    /// so both the TTY answer and the read are injected; the real
    /// `resolve_compact_destination` is never called, exactly as
    /// `cli::consent`'s own tests avoid the ambient terminal.
    mod compact_destination {
        use super::*;

        /// The crux: the refusal path must not merely return an error, it
        /// must never ATTEMPT the read that would hang.
        #[test]
        fn non_tty_without_to_refuses_and_never_reads_stdin() {
            let err = resolve_compact_destination_with(None, false, || {
                panic!("must never attempt to read stdin when non-TTY and --to was not given")
            })
            .expect_err("a non-interactive compaction with no --to must refuse");
            let msg = err.to_string();
            assert!(msg.contains("refused"), "got: {msg}");
            assert!(
                msg.contains("--to"),
                "the refusal must name the override; got: {msg}"
            );
        }

        /// `--to` short-circuits before any stdin interaction, on a TTY or
        /// not — that is what makes compaction scriptable.
        #[test]
        fn to_short_circuits_before_any_read() {
            for is_tty in [true, false] {
                let label = resolve_compact_destination_with(Some("L6-DST"), is_tty, || {
                    panic!("--to must short-circuit before any stdin read")
                })
                .unwrap();
                assert_eq!(label, "L6-DST");
            }
        }

        #[test]
        fn to_is_trimmed_and_an_empty_one_is_rejected() {
            assert_eq!(
                resolve_compact_destination_with(Some("  L6-DST\n"), false, || unreachable!())
                    .unwrap(),
                "L6-DST"
            );
            assert!(
                resolve_compact_destination_with(Some("   "), false, || unreachable!()).is_err(),
                "an empty --to is a mistake, not a request to prompt"
            );
        }

        /// The interactive path still works — the fix is a terminal check,
        /// not the removal of the prompt.
        #[test]
        fn a_tty_still_prompts_and_trims_the_answer() {
            let label =
                resolve_compact_destination_with(None, true, || Ok("  L6-DST \n".to_string()))
                    .unwrap();
            assert_eq!(label, "L6-DST");
        }

        /// A bare Enter is not a destination. It used to be caught after
        /// the read; it still is.
        #[test]
        fn an_empty_answer_on_a_tty_is_refused() {
            assert!(resolve_compact_destination_with(None, true, || Ok("\n".to_string())).is_err());
        }
    }

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

        /// ADR-0006: "a warehouse copy dies weeks after payment stops."
        /// When it does, the row must be removable -- a deposit that
        /// outlives its bytes keeps counting as a copy at `unit
        /// mark-tape-only` and `snapshot mark-reclaimable`, the two gates
        /// that decide whether local data may be deleted.
        #[test]
        fn a_deposit_can_be_un_recorded_and_stops_counting_as_a_copy() {
            let conn = fixture();
            add(&conn, "L6-0003", "glacier").unwrap();
            let before: i64 = conn
                .query_row("SELECT COUNT(*) FROM volume_deposits", [], |r| r.get(0))
                .unwrap();
            assert_eq!(before, 1);

            run_deposit(
                &conn,
                &DepositCommands::Remove {
                    label: "L6-0003".into(),
                    from: "glacier".into(),
                },
                true,
            )
            .expect("a recorded deposit must be removable");

            let after: i64 = conn
                .query_row("SELECT COUNT(*) FROM volume_deposits", [], |r| r.get(0))
                .unwrap();
            assert_eq!(after, 0, "the row must be gone, not merely flagged");
        }

        /// Removing something that was never recorded is an error, not a
        /// silent success -- a quiet no-op would let an operator believe
        /// they had corrected the catalog when they had typoed a label.
        #[test]
        fn removing_a_deposit_that_was_never_recorded_errors() {
            let conn = fixture();
            let err = run_deposit(
                &conn,
                &DepositCommands::Remove {
                    label: "L6-0003".into(),
                    from: "glacier".into(),
                },
                true,
            )
            .expect_err("nothing was recorded there");
            assert!(err.to_string().contains("no recorded deposit"), "{err}");
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
