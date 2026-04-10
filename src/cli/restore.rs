use clap::Subcommand;
use rusqlite::Connection;

use crate::config::{Config, TapectlPaths};
use crate::error::Result;
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
    }
    Ok(())
}
