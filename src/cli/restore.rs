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
        /// does
        #[arg(long)]
        version: Option<i64>,
        /// Where the restore's scratch directory is made (removed when the
        /// restore ends); defaults to inside --to, never the system temp
        /// directory. A unit whose isolated catalogue from `stage create`
        /// is on disk streams its slices into dar and puts only named
        /// pipes there; otherwise (a rebuilt catalog, dar older than 2.7.9)
        /// every decrypted slice waits there, as large as the unit
        #[arg(long, value_name = "DIR")]
        scratch: Option<String>,
        /// Restore into a destination that already holds files, replacing
        /// any that collide. Without it a destination that is not empty is
        /// refused before the tape is touched
        #[arg(long)]
        overwrite: bool,
        /// Skip the free-space check (the unit's size when it streams;
        /// about twice that when its slices are spooled, with the scratch
        /// directory and --to on one disk), for a filesystem that holds
        /// more than it reports free, such as a compressed or
        /// thin-provisioned one
        #[arg(long)]
        no_space_check: bool,
        /// Show what would be restored without restoring
        #[arg(long)]
        dry_run: bool,
    },

    /// Restore a single file from a unit
    File {
        /// File path within the unit, relative to its root, as `catalog ls`
        /// prints it. Checked against the catalog's file list for the
        /// version before the tape is touched; a directory is refused
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
        /// does
        #[arg(long)]
        version: Option<i64>,
        /// Where the decrypted slices wait for dar: a scratch directory is
        /// made inside DIR and removed when the restore ends. Defaults to
        /// inside --to, never the system temp directory. With the unit's
        /// isolated catalogue from `stage create` on disk only the slices
        /// holding the file (and the last) are read; otherwise every slice
        #[arg(long, value_name = "DIR")]
        scratch: Option<String>,
        /// Replace a file of the same name already in --to. Without it one
        /// is refused before the tape is touched
        #[arg(long)]
        overwrite: bool,
        /// Skip the free-space check (the decrypted slices read, plus the
        /// file, with the scratch directory and --to on one disk), for a
        /// filesystem that holds more than it reports free, such as a
        /// compressed or thin-provisioned one
        #[arg(long)]
        no_space_check: bool,
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
            scratch,
            overwrite,
            no_space_check,
            dry_run,
        } => {
            let device = crate::cli::read_device(config, device.as_deref())?;
            let options = volume::restore::RestoreOptions {
                scratch: scratch.as_ref().map(std::path::PathBuf::from),
                overwrite: *overwrite,
                no_space_check: *no_space_check,
            };
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
                &options,
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
            scratch,
            overwrite,
            no_space_check,
        } => {
            // Issue #241: unlike `restore unit`, this has no preview. The
            // catalog check of `--file` runs before any tape contact (issue
            // #406), but proving the entry is really in the archive still
            // means reading its slices off the tape (every slice, without
            // the unit's isolated catalogue on disk; issue #411).
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "restore file",
                    "there is no cheap preview — the file is checked against the catalog \
                     before the tape is touched, but proving it is in the archive means \
                     reading its slices off the tape. `restore unit --dry-run` previews the \
                     containing unit at no cost.",
                ));
            }
            let device = crate::cli::read_device(config, device.as_deref())?;
            let options = volume::restore::RestoreOptions {
                scratch: scratch.as_ref().map(std::path::PathBuf::from),
                overwrite: *overwrite,
                no_space_check: *no_space_check,
            };
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
                &options,
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
