use clap::Subcommand;
use rusqlite::Connection;

use crate::config::{Config, TapectlPaths};
use crate::error::Result;
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
    },

    /// Write staged data to volume
    Write {
        /// Volume label
        label: String,
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
    },

    /// Verify volume contents against checksums
    Verify {
        /// Volume label
        label: String,
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
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

    /// Clone encrypted slices from one volume to another (no decryption)
    CloneSlices {
        /// Source volume label
        #[arg(long)]
        from: String,
        /// Destination volume label
        #[arg(long)]
        to: String,
        /// Unit name to clone
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
}

pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    command: &VolumeCommands,
    json_output: bool,
) -> Result<()> {
    match command {
        VolumeCommands::Init { label, device } => {
            let vol_id = write::volume_init(conn, config, label, device, DEFAULT_BLOCK_SIZE)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"volume_id": vol_id, "label": label, "status": "initialized"})
                );
            } else {
                println!("volume \"{label}\" initialized (id={vol_id})");
            }
        }

        VolumeCommands::Write { label, device } => {
            write::volume_write(conn, paths, config, label, device, DEFAULT_BLOCK_SIZE)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"label": label, "status": "completed"})
                );
            } else {
                println!("volume \"{label}\" write completed");
            }
        }

        VolumeCommands::Verify { label, device } => {
            let report = write::volume_verify(conn, label, device, DEFAULT_BLOCK_SIZE)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "label": label,
                        "checked": report.checked,
                        "passed": report.passed,
                        "failed": report.failed,
                    })
                );
            } else {
                println!(
                    "verify {label}: {} checked, {} passed, {} failed",
                    report.checked, report.passed, report.failed,
                );
            }
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
            crate::cli::operations::volume_retire(conn, label, json_output)?;
        }

        VolumeCommands::CloneSlices {
            from,
            to,
            unit,
            device,
        } => {
            let report = write::clone_slices(
                conn,
                paths,
                config,
                from,
                to,
                unit,
                device,
                DEFAULT_BLOCK_SIZE,
            )?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "from": from, "to": to, "unit": unit,
                        "slices_cloned": report.slices_cloned,
                        "bytes_cloned": report.bytes_cloned,
                    })
                );
            } else {
                println!(
                    "cloned {} slices ({} MB) from \"{}\" to \"{}\"",
                    report.slices_cloned,
                    report.bytes_cloned / (1024 * 1024),
                    from,
                    to,
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
                    // Estimate tapes needed
                    let tape_cap = config
                        .backends
                        .lto
                        .first()
                        .map(|b| crate::staging::parse_size_to_bytes(&b.nominal_capacity))
                        .unwrap_or(2_500_000_000_000);
                    let usable = (tape_cap as f64 * 0.92) as i64;
                    let tapes_needed = ((total_bytes * copies) + usable - 1) / usable;
                    println!("estimated tapes: {tapes_needed} (at 92% usable capacity)");
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
            write::compact_finish(conn, label)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"label": label, "status": "retired"})
                );
            } else {
                println!("compact-finish \"{label}\": volume retired");
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
            write::compact_finish(conn, label)?;
            println!("  Volume \"{label}\" retired");

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"source": label, "destination": dest_label, "status": "completed"})
                );
            } else {
                println!("\ncompaction complete: {label} → {dest_label}");
            }
        }
    }
    Ok(())
}
