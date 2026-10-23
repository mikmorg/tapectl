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
        /// Volume label. Without it the loaded tape names itself: File 0 is
        /// read and the volume is found in this catalog by its uuid, and
        /// the one chosen is printed. A label that is given is checked
        /// against File 0 as ever. A tape this catalog does not know is
        /// refused, naming what File 0 says. Not with --dry-run, which
        /// opens no drive
        #[arg(long)]
        from: Option<String>,
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
        /// An age identity file (the SECRET, `AGE-SECRET-KEY-1…`) to decrypt
        /// with in this command only; repeat for several. It is added to
        /// the home's own keys and nothing more: never copied into the
        /// home, registered as any key, or logged. Lets an heir who holds
        /// only the escrow key restore through tapectl after `catalog
        /// rebuild --from-volume --key`. A file that is not an identity is
        /// refused, naming it, before the drive is opened
        #[arg(long, value_name = "FILE")]
        key: Vec<std::path::PathBuf>,
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
        /// Volume label. Without it the loaded tape names itself: File 0 is
        /// read and the volume is found in this catalog by its uuid, and
        /// the one chosen is printed. A label that is given is checked
        /// against File 0 as ever. A tape this catalog does not know is
        /// refused, naming what File 0 says. Not with --dry-run, which
        /// opens no drive
        label: Option<String>,
        /// Destination directory: each unit is restored into `DIR/<unit
        /// name>`, which must be empty or new
        #[arg(long, value_name = "DIR")]
        to: String,
        /// A unit to restore; repeat for several. Without it, every unit
        /// the volume holds whole: on a tape that ended early (a short
        /// seal), the units the end cut are named, with a volume that holds
        /// each whole, and left out
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
        /// An age identity file (the SECRET, `AGE-SECRET-KEY-1…`) to decrypt
        /// with in this command only; repeat for several. It is added to
        /// the home's own keys and nothing more: never copied into the
        /// home, registered as any key, or logged. Lets an heir who holds
        /// only the escrow key restore through tapectl after `catalog
        /// rebuild --from-volume --key`. A file that is not an identity is
        /// refused, naming it, before the drive is opened
        #[arg(long, value_name = "FILE")]
        key: Vec<std::path::PathBuf>,
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
        /// Volume label. Without it the loaded tape names itself: File 0 is
        /// read and the volume is found in this catalog by its uuid, and
        /// the one chosen is printed. A label that is given is checked
        /// against File 0 as ever. A tape this catalog does not know is
        /// refused, naming what File 0 says
        #[arg(long)]
        from: Option<String>,
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
        /// An age identity file (the SECRET, `AGE-SECRET-KEY-1…`) to decrypt
        /// with in this command only; repeat for several. It is added to
        /// the home's own keys and nothing more: never copied into the
        /// home, registered as any key, or logged. Lets an heir who holds
        /// only the escrow key restore through tapectl after `catalog
        /// rebuild --from-volume --key`. A file that is not an identity is
        /// refused, naming it, before the drive is opened
        #[arg(long, value_name = "FILE")]
        key: Vec<std::path::PathBuf>,
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
        /// tenant_envelope_backup, data_slice, seal_marker
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
            key,
            dry_run,
        } => {
            // Item 107: a `--key` that is not an identity is refused first,
            // by name, before the device is resolved or the drive opened.
            let keys = crate::crypto::keys::KeyFile::read_all(key)?;
            // Issue #435: no `--from` restores from the loaded tape, which
            // File 0 names. A dry run opens no drive, so it cannot.
            if from.is_none() && *dry_run {
                return Err(no_volume_dry_run("restore unit"));
            }
            let device = crate::cli::read_device(config, device.as_deref())?;
            let options = volume::restore::RestoreOptions {
                scratch: scratch.as_ref().map(std::path::PathBuf::from),
                overwrite: *overwrite,
                no_space_check: *no_space_check,
                spool: *spool,
                keys,
            };
            let report = match from {
                Some(from) => volume::restore::restore_unit(
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
                )?,
                None => volume::restore::restore_unit_from_loaded(
                    conn,
                    paths,
                    config,
                    unit,
                    to,
                    &device,
                    DEFAULT_BLOCK_SIZE,
                    *version,
                    &options,
                )?,
            };

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
                        "substitutions": substitutions_json(&report.substitutions),
                        "filled": filled_json(&report.filled),
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
                for line in repair_lines(&report.substitutions, &report.filled) {
                    println!("{line}");
                }
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
            key,
        } => {
            // Item 107: refused by name before anything else, as `restore
            // unit` does.
            let keys = crate::crypto::keys::KeyFile::read_all(key)?;
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
                keys,
            };
            // Issue #435: no `--from` restores from the loaded tape; the
            // volume is then the one File 0 named.
            let report = match from {
                Some(from) => volume::restore::restore_file(
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
                )?,
                None => volume::restore::restore_file_from_loaded(
                    conn,
                    paths,
                    config,
                    unit,
                    file,
                    to,
                    &device,
                    DEFAULT_BLOCK_SIZE,
                    *version,
                    &options,
                )?,
            };
            let from = report.volume_label.clone();

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "file": file,
                        "unit": unit,
                        "volume": from,
                        "destination": to,
                        "substitutions": substitutions_json(&report.substitutions),
                        "filled": filled_json(&report.filled),
                    })
                );
            } else {
                println!("restored \"{file}\" from \"{unit}\" on {from} to {to}");
                for line in repair_lines(&report.substitutions, &report.filled) {
                    println!("{line}");
                }
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
            key,
            dry_run,
        } => {
            // Item 107: refused by name before anything else.
            let keys = crate::crypto::keys::KeyFile::read_all(key)?;
            // Issue #435: no label restores from the loaded tape, which File
            // 0 names; the units are then listed from that volume once it is
            // known. A dry run opens no drive, so it cannot.
            if label.is_none() && *dry_run {
                return Err(no_volume_dry_run("restore volume"));
            }
            let options = volume::restore::RestoreOptions {
                scratch: scratch.as_ref().map(std::path::PathBuf::from),
                overwrite: *overwrite,
                no_space_check: *no_space_check,
                spool: *spool,
                keys,
            };
            let report = match label {
                Some(label) => {
                    let names = if units.is_empty() {
                        volume::restore::units_on_volume(conn, label)?
                    } else {
                        units.clone()
                    };
                    let requests = volume::restore::unit_requests(&names, to)?;
                    let device = crate::cli::read_device(config, device.as_deref())?;
                    volume::restore::restore_units(
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
                    )?
                }
                None => {
                    let device = crate::cli::read_device(config, device.as_deref())?;
                    volume::restore::restore_units_from_loaded(
                        conn,
                        paths,
                        config,
                        units,
                        to,
                        &options,
                        *fail_fast,
                        &device,
                        DEFAULT_BLOCK_SIZE,
                    )?
                }
            };

            // #420 (`volume-format-v2.md` §4.2): with no unit named, the set
            // is every unit the volume holds whole; a tape that ended early
            // also names the units it cut, and where each is whole. A unit
            // named that the tape cut was refused, by name, before the drive.
            // #478 (§4.4, ADR-0012 item 89): the same for a unit of which the
            // tape holds only a range, with where its restore starts.
            let cut = if units.is_empty() {
                volume::restore::cut_units_on_volume(conn, &report.volume_label)?
            } else {
                Vec::new()
            };
            if json_output {
                let mut obj = units_report_json(&report);
                obj["cut"] = cut_units_json(&cut);
                obj["ranges"] = range_units_json(&cut);
                println!("{obj}");
            } else {
                for line in units_report_lines(&report) {
                    println!("{line}");
                }
                for line in cut_units_lines(&cut, &report.volume_label) {
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
                        // #420: additive. Where a short tape ended, and the
                        // planned files after it, not read.
                        "ended_after": report.ended_after,
                        "not_on_tape": report
                            .not_on_tape
                            .iter()
                            .map(|(position, kind)| serde_json::json!({
                                "position": position,
                                "type": kind,
                            }))
                            .collect::<Vec<_>>(),
                        // #490: on a tape with no seal it accepts, the
                        // planned file at which END OF DATA came (R2).
                        "end_of_data_at": report.end_of_data_at,
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
                for line in volume::raw::ending_lines(&report) {
                    println!("{line}");
                }
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

/// What was repaired on the way, one line each: slices read from another
/// copy (ADR-0012 item 101) and files filled from another stage set of the
/// same version (item 102).
pub(crate) fn repair_lines(
    substitutions: &[volume::restore::Substitution],
    filled: &[(String, String)],
) -> Vec<String> {
    let mut lines = Vec::new();
    for s in substitutions {
        lines.push(format!(
            "  slice {} was damaged on {} ({}) and was read from {}, another copy of it",
            s.slice_number, s.damaged_on, s.reason, s.supplied_by
        ));
    }
    if !filled.is_empty() {
        lines.push(format!(
            "  {} file(s) came back damaged or missing and were filled from another copy of \
             this version, each checked against its recorded checksum:",
            filled.len()
        ));
        for (path, from) in filled {
            lines.push(format!("    {path} (from {from})"));
        }
    }
    lines
}

fn substitutions_json(substitutions: &[volume::restore::Substitution]) -> serde_json::Value {
    substitutions
        .iter()
        .map(|s| {
            serde_json::json!({
                "slice": s.slice_number,
                "read_from": s.supplied_by,
                "damaged_on": s.damaged_on,
                "reason": s.reason,
            })
        })
        .collect()
}

fn filled_json(filled: &[(String, String)]) -> serde_json::Value {
    filled
        .iter()
        .map(|(path, from)| serde_json::json!({"path": path, "from": from}))
        .collect()
}

/// The refusal for `--dry-run` with no volume named (issue #435): a dry run
/// opens no drive, and without a label the volume is whatever File 0 on the
/// loaded tape says.
fn no_volume_dry_run(command: &str) -> TapectlError {
    crate::cli::refuse_dry_run(
        command,
        "with no volume named the tape that is loaded names it, which means reading its \
         File 0, and a dry run opens no drive. Name the volume (--from for restore unit, the \
         label for restore volume) to preview.",
    )
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
        lines.extend(repair_lines(&u.substitutions, &u.filled));
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

/// The units the early end of `label`'s tape cut (#420), and those of which
/// it holds only a range of a version held across cartridges (#478), one
/// line each, for `restore volume` with no unit named: not restored, and not
/// a failure — the tape does not hold them whole by design
/// (`volume-format-v2.md` §4.2 and §4.4, ADR-0012 item 89).
pub(crate) fn cut_units_lines(cut: &[volume::restore::CutUnit], label: &str) -> Vec<String> {
    let how = |c: &volume::restore::CutUnit| match c.restore_from() {
        Some(from) => format!(
            "; restore it with `tapectl restore unit --unit {} --from {from} --version {} --to \
             <DIR>`",
            c.unit_name, c.version
        ),
        None => String::new(),
    };
    let mut lines: Vec<String> = cut
        .iter()
        .map(|c| match c.range {
            None => format!(
                "not on this tape whole: {} — cut by the early end of \"{label}\"'s tape{}",
                c.describe(),
                how(c)
            ),
            Some(_) => format!(
                "not on this tape whole: {} — a range of a version held across cartridges, read \
                 with its other slices in slice order{}",
                c.describe(),
                how(c)
            ),
        })
        .collect();
    let cuts = cut.iter().filter(|c| c.range.is_none()).count();
    let ranges = cut.len() - cuts;
    if cuts > 0 {
        lines.push(format!(
            "{cuts} unit(s) left out: \"{label}\" ended early and does not hold them whole"
        ));
    }
    if ranges > 0 {
        lines.push(format!(
            "{ranges} unit(s) held across cartridges left out: \"{label}\" holds only a range \
             of each, and `restore volume` restores the units a tape holds whole"
        ));
    }
    lines
}

/// `restore volume --json`'s `cut`: the units the end of the tape cut.
fn cut_units_json(cut: &[volume::restore::CutUnit]) -> serde_json::Value {
    cut.iter()
        .filter(|c| c.range.is_none())
        .map(|c| {
            serde_json::json!({
                "unit": c.unit_name,
                "version": c.version,
                "whole_on": c.whole_on,
            })
        })
        .collect()
}

/// `restore volume --json`'s `ranges` (#478): the units of which the tape
/// holds only a range, the slices it holds, and where the version is whole
/// or its slice 1 is.
fn range_units_json(cut: &[volume::restore::CutUnit]) -> serde_json::Value {
    cut.iter()
        .filter_map(|c| {
            c.range.map(|(first, last)| {
                serde_json::json!({
                    "unit": c.unit_name,
                    "version": c.version,
                    "first_slice": first,
                    "last_slice": last,
                    "whole_on": c.whole_on,
                    "starts_on": c.starts_on,
                })
            })
        })
        .collect()
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
                "substitutions": substitutions_json(&u.substitutions),
                "filled": filled_json(&u.filled),
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
