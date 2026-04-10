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
    }
    Ok(())
}
