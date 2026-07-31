use clap::Subcommand;
use rusqlite::Connection;

use crate::config::{Config, TapectlPaths};
use crate::error::{Result, TapectlError};
use crate::volume;

const DEFAULT_BLOCK_SIZE: usize = 512 * 1024;

#[derive(Subcommand, Debug)]
pub enum RestoreCommands {
    /// Restore a unit from a volume
    Unit {
        /// Unit name
        #[arg(long)]
        unit: String,
        /// Volume label
        #[arg(long)]
        from: String,
        /// Destination directory
        #[arg(long)]
        to: String,
        /// Tape device
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
        /// Show what would be restored without restoring
        #[arg(long)]
        dry_run: bool,
    },

    /// Restore a single file from a unit
    File {
        /// File path within the unit
        #[arg(long)]
        file: String,
        /// Unit name
        #[arg(long)]
        unit: String,
        /// Volume label
        #[arg(long)]
        from: String,
        /// Destination directory
        #[arg(long)]
        to: String,
        /// Tape device
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
    },

    /// Dump every file off a tape verbatim, using only what is on the tape
    /// itself (no database needed) — the emergency/heir path
    RawVolume {
        /// Tape device
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
        /// Destination directory
        #[arg(long = "to")]
        to: String,
        /// Refuse unless the tape's own reported label matches (wrong-tape
        /// guard) — not a database lookup
        #[arg(long = "from")]
        from: Option<String>,
    },
}

pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    command: &RestoreCommands,
    json_output: bool,
) -> Result<()> {
    match command {
        RestoreCommands::Unit {
            unit,
            from,
            to,
            device,
            dry_run,
        } => {
            let report = volume::restore::restore_unit(
                conn,
                paths,
                config,
                unit,
                from,
                to,
                device,
                DEFAULT_BLOCK_SIZE,
                *dry_run,
            )?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "unit": report.unit_name,
                        "volume": report.volume_label,
                        "slices": report.slices,
                        "destination": report.destination,
                        "dry_run": report.dry_run,
                    })
                );
            } else if report.dry_run {
                println!(
                    "would restore \"{}\" from {} ({} slices) to {}",
                    report.unit_name, report.volume_label, report.slices, report.destination,
                );
            } else {
                println!(
                    "restored \"{}\" from {} ({} slices) to {}",
                    report.unit_name, report.volume_label, report.slices, report.destination,
                );
            }
        }

        RestoreCommands::File {
            file,
            unit,
            from,
            to,
            device,
        } => {
            volume::restore::restore_file(
                conn,
                paths,
                config,
                unit,
                file,
                from,
                to,
                device,
                DEFAULT_BLOCK_SIZE,
            )?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"file": file, "unit": unit, "volume": from, "destination": to})
                );
            } else {
                println!("restored \"{file}\" from \"{unit}\" on {from} to {to}");
            }
        }

        RestoreCommands::RawVolume { device, to, from } => {
            let dest = std::path::Path::new(to);
            let report =
                volume::raw::restore_raw(device, DEFAULT_BLOCK_SIZE, dest, from.as_deref())?;

            if json_output {
                let files: Vec<_> = report
                    .files
                    .iter()
                    .map(|f| {
                        serde_json::json!({
                            "position": f.position,
                            "type": f.type_label,
                            "path": f.path.display().to_string(),
                            "bytes_written": f.bytes_written,
                            "verified": f.verified,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({
                        "label": report.label,
                        "uuid": report.uuid,
                        "destination": to,
                        "files_dumped": report.files_dumped,
                        "bytes_written": report.bytes_written,
                        "verified_count": report.verified_count,
                        "mismatched_count": report.mismatched_count,
                        "unverifiable_count": report.unverifiable_count,
                        "all_verified": report.all_verified(),
                        "files": files,
                    })
                );
            } else {
                println!(
                    "dumped {} files ({} bytes) from volume \"{}\" (uuid {}) to {}",
                    report.files_dumped, report.bytes_written, report.label, report.uuid, to
                );
                println!(
                    "  verified: {}  mismatched: {}  unverifiable: {}",
                    report.verified_count, report.mismatched_count, report.unverifiable_count
                );
                if !report.all_verified() {
                    for f in report.files.iter().filter(|f| f.verified == Some(false)) {
                        eprintln!(
                            "  CHECKSUM MISMATCH: position {} ({}) at {}",
                            f.position,
                            f.type_label,
                            f.path.display()
                        );
                    }
                }
            }

            if !report.all_verified() {
                return Err(TapectlError::Other(format!(
                    "raw-volume: {} of {} files failed checksum verification",
                    report.mismatched_count, report.files_dumped
                )));
            }
        }
    }
    Ok(())
}
