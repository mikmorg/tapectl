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
        /// Decrypt every slice to the scratch directory before dar extracts
        /// them, even when the unit could stream (its isolated catalogue
        /// on disk). Needs about twice the unit's size with the scratch
        /// directory and --to on one disk
        #[arg(long)]
        spool: bool,
        /// Show what would be restored without restoring
        #[arg(long)]
        dry_run: bool,
    },

    /// Restore several units from one volume in one pass over the tape
    ///
    /// The disaster-recovery path: the drive is opened and rewound once and
    /// the units are read in the order they lie on the tape, each into its
    /// own directory, `DIR/<unit name>`. Each unit's newest version on the
    /// volume is restored (use `restore unit --version` for an older one).
    /// One unit's failure does not stop the others (unless --fail-fast);
    /// the command fails if any unit was not restored
    Volume {
        /// Volume label
        label: String,
        /// Destination directory: each unit is restored into `DIR/<unit
        /// name>`, which must be empty or new
        #[arg(long, value_name = "DIR")]
        to: String,
        /// A unit to restore; repeat for several. Without it, every unit
        /// with written slices on the volume
        #[arg(long = "unit", value_name = "NAME")]
        units: Vec<String>,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
        /// Where each unit's scratch directory is made, one at a time
        /// (removed when that unit ends); defaults to inside the unit's own
        /// directory, never the system temp directory
        #[arg(long, value_name = "DIR")]
        scratch: Option<String>,
        /// Restore into unit directories that already hold files, replacing
        /// any that collide. Without it a unit directory that is not empty
        /// is refused before the tape is touched
        #[arg(long)]
        overwrite: bool,
        /// Skip the free-space check of the whole set (every restored unit,
        /// plus the largest unit's spooled slices)
        #[arg(long)]
        no_space_check: bool,
        /// Spool every unit's slices to the scratch directory instead of
        /// streaming them into dar (see `restore unit --spool`)
        #[arg(long)]
        spool: bool,
        /// Stop at the first unit that fails; the units after it are not
        /// attempted. Without it every unit is tried
        #[arg(long)]
        fail_fast: bool,
        /// Show what would be restored, unit by unit in tape order, without
        /// opening the drive
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
    /// itself (no database needed) — the emergency/heir path. --positions
    /// and --only narrow it to the files named (a full cartridge is a
    /// terabyte; RESTORE.sh alone is `--only restore_sh`)
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
        /// Dump only the files at these tape positions (comma-separated or
        /// repeated; File 0 is the ID thunk, 2 RESTORE.sh, 3 the front
        /// index). With --only, a file either names is dumped
        #[arg(long, value_delimiter = ',', value_name = "N")]
        positions: Vec<i32>,
        /// Dump only the files of these front-index types (comma-separated
        /// or repeated): id_thunk, system_guide, restore_sh, front_index,
        /// tenant_envelope, operator_envelope, operator_envelope_backup,
        /// data_slice, seal_marker
        #[arg(long, value_delimiter = ',', value_name = "TYPE")]
        only: Vec<String>,
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
            spool,
            dry_run,
        } => {
            let device = crate::cli::read_device(config, device.as_deref())?;
            let options = volume::restore::RestoreOptions {
                scratch: scratch.as_ref().map(std::path::PathBuf::from),
                overwrite: *overwrite,
                no_space_check: *no_space_check,
                spool: *spool,
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
                // `restore file` always spools the slices it reads.
                spool: false,
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

        RestoreCommands::Volume {
            label,
            to,
            units,
            device,
            scratch,
            overwrite,
            no_space_check,
            spool,
            fail_fast,
            dry_run,
        } => {
            let names = if units.is_empty() {
                volume::restore::units_on_volume(conn, label)?
            } else {
                units.clone()
            };
            let requests = names
                .iter()
                .map(|name| {
                    // The name becomes a path under --to: hold it to the
                    // unit-name rules (no `..`, no leading `/`) even for a
                    // row a rebuilt catalog brought in from tape.
                    crate::naming::validate_unit_name(name)?;
                    Ok(volume::restore::UnitRequest {
                        unit: name.clone(),
                        version: None,
                        dest_dir: std::path::Path::new(to)
                            .join(name)
                            .to_string_lossy()
                            .into_owned(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let device = crate::cli::read_device(config, device.as_deref())?;
            let options = volume::restore::RestoreOptions {
                scratch: scratch.as_ref().map(std::path::PathBuf::from),
                overwrite: *overwrite,
                no_space_check: *no_space_check,
                spool: *spool,
            };
            let report = volume::restore::restore_units(
                conn,
                paths,
                config,
                label,
                &requests,
                &options,
                *fail_fast,
                &device,
                DEFAULT_BLOCK_SIZE,
                *dry_run,
            )?;

            if json_output {
                println!("{}", units_report_json(&report));
            } else {
                for line in units_report_lines(&report) {
                    println!("{line}");
                }
            }
            let failed = report.failed();
            if failed > 0 {
                return Err(TapectlError::Other(format!(
                    "{failed} of {} units not restored from {}",
                    report.units.len(),
                    report.volume_label
                )));
            }
        }

        RestoreCommands::RawVolume {
            device,
            to,
            from,
            positions,
            only,
        } => {
            let selection = volume::raw::RawSelection {
                positions: positions.clone(),
                types: only.clone(),
            };
            // A misspelt type is refused before the drive is opened.
            selection.check_types()?;
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
            let report = volume::restore::restore_raw_volume_selected(
                conn,
                &mut store,
                dest,
                from.as_deref(),
                &selection,
                site,
            )?;

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

/// `restore volume`'s report, one line per unit in the order the units lie
/// on the tape (the order they were restored in), then a summary.
pub(crate) fn units_report_lines(report: &volume::restore::UnitsReport) -> Vec<String> {
    let mut lines = Vec::with_capacity(report.units.len() + 1);
    for u in &report.units {
        let what = format!(
            "\"{}\" v{} ({} slices) to {}",
            u.unit_name, u.version, u.slices, u.destination
        );
        lines.push(match (&u.error, u.attempted, report.dry_run) {
            (_, _, true) => format!("would restore {what}"),
            (None, true, false) => format!("restored {what}"),
            (Some(e), _, false) => format!("FAILED {what}: {e}"),
            (None, false, false) => format!("not attempted {what} (--fail-fast)"),
        });
    }
    let total = report.units.len();
    lines.push(if report.dry_run {
        format!(
            "would restore {total} unit(s) from {} in one pass",
            report.volume_label
        )
    } else {
        format!(
            "{} of {total} unit(s) restored from {}",
            total - report.failed(),
            report.volume_label
        )
    });
    lines
}

/// `restore volume --json`: the units in tape order, each with its outcome.
fn units_report_json(report: &volume::restore::UnitsReport) -> serde_json::Value {
    let units: Vec<_> = report
        .units
        .iter()
        .map(|u| {
            let outcome = if report.dry_run {
                "dry-run"
            } else if u.error.is_some() {
                "failed"
            } else if u.attempted {
                "restored"
            } else {
                "not-attempted"
            };
            serde_json::json!({
                "unit": u.unit_name,
                "version": u.version,
                "slices": u.slices,
                "destination": u.destination,
                "outcome": outcome,
                "error": u.error,
            })
        })
        .collect();
    serde_json::json!({
        "volume": report.volume_label,
        "dry_run": report.dry_run,
        "failed": if report.dry_run { 0 } else { report.failed() },
        "units": units,
    })
}
