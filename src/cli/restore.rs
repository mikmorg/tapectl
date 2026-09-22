use clap::Subcommand;
use rusqlite::Connection;

use crate::config::{Config, TapectlPaths};
use crate::error::{Result, TapectlError};
use crate::store::TapeStore;
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
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
        /// Snapshot version to restore. Defaults to the newest version of
        /// the unit on this volume (the same rule RESTORE.sh applies); a
        /// version the volume does not carry is refused, naming the ones it
        /// does (issue #315)
        #[arg(long)]
        version: Option<i64>,
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
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
        /// Snapshot version to restore. Defaults to the newest version of
        /// the unit on this volume (the same rule RESTORE.sh applies); a
        /// version the volume does not carry is refused, naming the ones it
        /// does (issue #315)
        #[arg(long)]
        version: Option<i64>,
    },

    /// Dump every file off a tape verbatim, using only what is on the tape
    /// itself (no database needed) — the emergency/heir path
    RawVolume {
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
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
    // Issue #241: `Unit` already honours the global flag on its own —
    // its local `dry_run` field shares clap's arg id with the global one
    // (same mechanism as `collection sync`'s characterisation test), so
    // it shadows this parameter inside that arm and this value is unused
    // there. `File`/`RawVolume` have no local field of their own and read
    // this parameter directly to refuse.
    dry_run: bool,
) -> Result<()> {
    match command {
        RestoreCommands::Unit {
            unit,
            from,
            to,
            device,
            version,
            dry_run,
        } => {
            let device = crate::cli::read_device(config, device.as_deref())?;
            let report = volume::restore::restore_unit(
                conn,
                paths,
                config,
                unit,
                from,
                to,
                &device,
                DEFAULT_BLOCK_SIZE,
                *version,
                *dry_run,
            )?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "unit": report.unit_name,
                        "volume": report.volume_label,
                        "version": report.version,
                        "slices": report.slices,
                        "destination": report.destination,
                        "dry_run": report.dry_run,
                    })
                );
            } else if report.dry_run {
                println!(
                    "would restore \"{}\" v{} from {} ({} slices) to {}",
                    report.unit_name,
                    report.version,
                    report.volume_label,
                    report.slices,
                    report.destination,
                );
            } else {
                println!(
                    "restored \"{}\" v{} from {} ({} slices) to {}",
                    report.unit_name,
                    report.version,
                    report.volume_label,
                    report.slices,
                    report.destination,
                );
            }
        }

        RestoreCommands::File {
            file,
            unit,
            from,
            to,
            device,
            version,
        } => {
            // Issue #241: unlike `restore unit`, this has no cheap
            // preview yet — a faithful one would need to confirm the
            // specific file exists inside the unit's archived contents,
            // which `restore_file` currently does by restoring to a temp
            // dir first (see its own doc comment).
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "restore file",
                    "there is no cheap preview yet — confirming the file exists means \
                     restoring the whole unit to a temp directory first. `restore unit \
                     --dry-run` previews the containing unit at no cost.",
                ));
            }
            let device = crate::cli::read_device(config, device.as_deref())?;
            volume::restore::restore_file(
                conn,
                paths,
                config,
                unit,
                file,
                from,
                to,
                &device,
                DEFAULT_BLOCK_SIZE,
                *version,
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
            // Issue #241: the emergency/heir path — there is no catalog
            // to consult, so knowing what would be dumped means opening
            // the drive and reading the tape's own front index, which is
            // most of this command's own work.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "restore raw-volume",
                    "this uses only what is on the tape itself, so a preview would have to \
                     open the drive and read the tape's own front index to know what would \
                     be dumped.",
                ));
            }
            let dest = std::path::Path::new(to);
            let device = crate::cli::read_device(config, device.as_deref())?;
            // Issue #166: refuse before the store is opened if this drive
            // cannot read the loaded medium. Proceeds silently with no
            // configured backend — this is the heir/DR path, ADR-0005.
            // Both MAM captures are held and journalled against the contact
            // `restore_raw_volume` opens (issue #297).
            let reads = crate::tape::mam_journal::MamReads::new(
                conn,
                crate::tape::contact::Operation::RestoreRawVolume,
            );
            reads.check_read_contact(config, &device)?;
            // The second read, as on every read path (ADR-0013 §5): the
            // contact records what the chip said rather than denying a read
            // happened (issue #316). Before `TapeStore::open_read` — the st
            // driver refuses a second concurrent open. LENIENT: no backend
            // yields `None`, recorded as no read at all. Nothing refuses on
            // it; this path corroborates nothing (ADR-0005).
            let observed = crate::volume::binding::loaded_medium(config, &device, &reads);
            let mut store = TapeStore::open_read(&device, DEFAULT_BLOCK_SIZE)?;
            // Through `volume::restore` rather than `volume::raw` directly:
            // `raw::restore_raw` stays `Connection`-free (it is what an heir
            // runs with no catalog at all), and the contact record is
            // bookkeeping ABOUT the dump, not part of it.
            let site = crate::tape::contact::ContactSite::new(
                config,
                crate::tape::contact::Operation::RestoreRawVolume,
                &device,
                crate::tape::contact::Medium::from_read(observed.as_ref().map(|(b, m)| (*b, m))),
            )
            .with_mam_reads(&reads);
            let report =
                volume::restore::restore_raw_volume(conn, &mut store, dest, from.as_deref(), site)?;

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
