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
