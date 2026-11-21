use clap::Subcommand;
use rusqlite::Connection;

use crate::config::{Config, TapectlPaths};
use crate::error::{Result, TapectlError};
use crate::store::TapeStore;
use crate::volume;

const DEFAULT_BLOCK_SIZE: usize = 512 * 1024;

/// `restore unit --help`'s examples (ADR-0012 item 211).
const RESTORE_UNIT_EXAMPLES: &str = "\
Examples:
  Restore the newest version of a unit from the loaded tape:
    tapectl restore unit photos --to /srv/restore/photos
  An older version, from a named volume:
    tapectl restore unit photos --from L6-0001 --version 2 --to /srv/restore/photos
  An heir with only the escrow key, after catalog rebuild --from-volume --key:
    tapectl restore unit --unit photos --device /dev/tape/by-id/YOUR-DRIVE-nst --to /srv/restore/photos --key escrow.key";

/// `restore file --help`'s examples (ADR-0012 item 211).
const RESTORE_FILE_EXAMPLES: &str = "\
Examples:
  Restore one file of a unit from the loaded tape:
    tapectl restore file photos 2024/beach.jpg --to /srv/restore
  The same, from a named volume, as an heir who learned --unit:
    tapectl restore file --unit photos 2024/beach.jpg --from L6-0001 --to /srv/restore";

#[derive(Subcommand, Debug)]
pub enum RestoreCommands {
    /// Restore a unit from a volume
    ///
    /// Exit status: 0 = restored; 2 = an error or a refusal; 75 = the catalog was
    /// busy.
    #[command(after_long_help = RESTORE_UNIT_EXAMPLES)]
    Unit {
        #[command(flatten)]
        unit: UnitArg,
        /// Volume label.
        ///
        /// Without it the loaded tape names itself: File 0 is read and the
        /// volume is found in this catalog by its uuid, and the one chosen is
        /// printed. A label that is given is checked against File 0 as ever. A
        /// tape this catalog does not know is refused, naming what File 0 says.
        /// Without it, --dry-run (which opens no drive) previews from the one
        /// volume this catalog has holding the unit, and names it; when several
        /// hold it they are listed, to name one here. For a version held across
        /// cartridges (a range on this tape) the restore may start on any tape
        /// of it: the others are asked for in slice order, by label, sticker
        /// and chip serial, and the answer "skip" (I do not have it), or nobody
        /// at the terminal, restores the files this tape holds whole and lists
        /// the rest
        #[arg(long)]
        from: Option<String>,
        /// Destination directory
        #[arg(long)]
        to: String,
        /// Tape device (by-id path).
        ///
        /// Defaults to the only configured drive; required when more than one
        /// is configured.
        #[arg(long)]
        device: Option<String>,
        /// Snapshot version to restore.
        ///
        /// Defaults to the newest version of the unit on this volume (the same
        /// rule RESTORE.sh applies); a version the volume does not carry is
        /// refused, naming the ones it does
        #[arg(long)]
        version: Option<i64>,
        /// Not used by a restore of a unit: only `restore file` extracts into a scratch directory.
        ///
        /// A restore of a unit writes no slice to disk: each member goes to
        /// tar as its slice is read. A repair of a damaged file from another
        /// copy of the version builds in a hidden directory inside --to,
        /// on the destination's own filesystem so that the file keeps its
        /// owner, times, extended attributes and ACL, and removes it when it
        /// ends. Only `restore file` puts anything in --scratch
        #[arg(long, value_name = "DIR")]
        scratch: Option<String>,
        /// Restore into a destination that already holds files, replacing any that collide.
        ///
        /// Without it a destination that is not empty is refused before the
        /// tape is touched
        #[arg(long)]
        overwrite: bool,
        /// Skip the free-space check, for a filesystem that holds more than it reports.
        ///
        /// Skip the free-space check (the unit's size in --to), for a
        /// filesystem that holds more than it reports free, such as a
        /// compressed or thin-provisioned one
        #[arg(long)]
        no_space_check: bool,
        /// Do not hash every restored file on disk once tar has ended.
        ///
        /// Every member is still hashed against the file table as it is
        /// handed to tar. The check on disk is a second read of what was
        /// restored: it is what proves tar made each file whole, since tar's
        /// exit status is never the verdict
        #[arg(long)]
        no_file_check: bool,
        /// An age identity file (a secret key) to decrypt with, in this command only.
        ///
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
        #[arg(long, short = 'n')]
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
    ///
    /// Exit status: 0 = every unit restored; 2 = a unit not restored, an error or a
    /// refusal; 75 = the catalog was busy.
    Volume {
        /// Volume label.
        ///
        /// Without it the loaded tape names itself: File 0 is read and the
        /// volume is found in this catalog by its uuid, and the one chosen is
        /// printed. A label that is given is checked against File 0 as ever. A
        /// tape this catalog does not know is refused, naming what File 0 says.
        /// Not with --dry-run, which opens no drive
        label: Option<String>,
        /// Destination directory: each unit is restored into `DIR/<unit name>`.
        ///
        /// Destination directory: each unit is restored into `DIR/<unit
        /// name>`, which must be empty or new
        #[arg(long, value_name = "DIR")]
        to: String,
        /// A unit to restore; repeat for several.
        ///
        /// Without it, every unit the volume holds a written slice of: a unit
        /// of which the tape holds only a range (the one a short seal ended
        /// inside, say) is read across cartridges. A unit named that the tape
        /// holds nothing of is refused before the drive is opened
        #[arg(long = "unit", value_name = "NAME")]
        units: Vec<String>,
        /// Tape device (by-id path).
        ///
        /// Defaults to the only configured drive; required when more than one
        /// is configured.
        #[arg(long)]
        device: Option<String>,
        /// Not used by a restore of units: only `restore file` extracts into a scratch directory.
        ///
        /// A repair from another copy builds in a hidden directory inside
        /// each unit's own directory, on its filesystem, and removes it when
        /// that unit ends. See `restore unit --scratch`
        #[arg(long, value_name = "DIR")]
        scratch: Option<String>,
        /// Restore into unit directories that already hold files, replacing any that collide.
        ///
        /// Without it a unit directory that is not empty is refused before the
        /// tape is touched
        #[arg(long)]
        overwrite: bool,
        /// Skip the free-space check of the whole set.
        ///
        /// Skip the free-space check of the whole set (every restored unit)
        #[arg(long)]
        no_space_check: bool,
        /// Do not hash every restored file on disk once tar has ended.
        ///
        /// See `restore unit --no-file-check`
        #[arg(long)]
        no_file_check: bool,
        /// Stop at the first unit that fails; the units after it are not attempted.
        ///
        /// Without it every unit is tried
        #[arg(long)]
        fail_fast: bool,
        /// An age identity file (a secret key) to decrypt with, in this command only.
        ///
        /// An age identity file (the SECRET, `AGE-SECRET-KEY-1…`) to decrypt
        /// with in this command only; repeat for several. It is added to
        /// the home's own keys and nothing more: never copied into the
        /// home, registered as any key, or logged. Lets an heir who holds
        /// only the escrow key restore through tapectl after `catalog
        /// rebuild --from-volume --key`. A file that is not an identity is
        /// refused, naming it, before the drive is opened
        #[arg(long, value_name = "FILE")]
        key: Vec<std::path::PathBuf>,
        /// Show what would be restored, unit by unit in tape order, without opening the drive
        #[arg(long, short = 'n')]
        dry_run: bool,
    },

    /// Restore a single file from a unit
    ///
    /// Exit status: 0 = restored; 2 = an error or a refusal; 75 = the catalog
    /// was busy.
    // The derived usage would show `[UNIT] [PATH]`, both optional, since the
    // two slots are filled by order (`UnitAndFile`); this says what is asked.
    #[command(
        override_usage = "tapectl restore file [OPTIONS] --to <TO> <UNIT|--unit <UNIT>> <PATH|--file <PATH>>",
        after_long_help = RESTORE_FILE_EXAMPLES
    )]
    File {
        // The unit and the file: `UNIT PATH`, or either as a flag (`--unit
        // UNIT PATH`, `UNIT --file PATH`, `--unit UNIT --file PATH`). The
        // field keeps the name `unit`, as `restore unit`'s does.
        #[command(flatten)]
        unit: UnitAndFile,
        /// Volume label.
        ///
        /// Without it the loaded tape names itself: File 0 is read and the
        /// volume is found in this catalog by its uuid, and the one chosen is
        /// printed. A label that is given is checked against File 0 as ever. A
        /// tape this catalog does not know is refused, naming what File 0 says.
        /// For a version held across cartridges (a range on this tape), this
        /// tape (or the loaded one) must hold its slice 1, as for `restore
        /// unit`; the others are asked for in slice order
        #[arg(long)]
        from: Option<String>,
        /// Destination directory
        #[arg(long)]
        to: String,
        /// Tape device (by-id path).
        ///
        /// Defaults to the only configured drive; required when more than one
        /// is configured.
        #[arg(long)]
        device: Option<String>,
        /// Snapshot version to restore.
        ///
        /// Defaults to the newest version of the unit on this volume (the same
        /// rule RESTORE.sh applies); a version the volume does not carry is
        /// refused, naming the ones it does
        #[arg(long)]
        version: Option<i64>,
        /// Where the file is extracted before it is placed (default: inside --to).
        ///
        /// Where the file is extracted before it is placed: a scratch
        /// directory is made inside DIR and removed when the restore ends.
        /// Defaults to inside --to, never the system temp directory. Only the
        /// slices that hold the file (and a hard link's target) are read
        #[arg(long, value_name = "DIR")]
        scratch: Option<String>,
        /// Replace a file of the same name already in --to.
        ///
        /// Without it one is refused before the tape is touched
        #[arg(long)]
        overwrite: bool,
        /// Skip the free-space check, for a filesystem that holds more than it reports.
        ///
        /// Skip the free-space check (the file's size), for a filesystem
        /// that holds more than it reports free, such as a compressed or
        /// thin-provisioned one
        #[arg(long)]
        no_space_check: bool,
        /// Do not hash the restored file on disk once tar has ended.
        ///
        /// The member is still hashed against the file table as it is handed
        /// to tar. The check on disk is a second read of what was restored
        #[arg(long)]
        no_file_check: bool,
        /// An age identity file (a secret key) to decrypt with, in this command only.
        ///
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

    /// Dump every file off a tape verbatim, using only the tape itself (no catalog).
    ///
    /// Dump every file off a tape verbatim, using only what is on the tape
    /// itself (no database needed): the files the heir path reads, without
    /// RESTORE.sh. --positions and --only narrow it to the files named (a
    /// full cartridge is a terabyte; RESTORE.sh alone is `--only restore_sh`)
    ///
    /// Exit status: 0 = every file dumped and checked; 2 = a file failed its
    /// checksum, or an error; 75 = the catalog was busy.
    RawVolume {
        /// Tape device (by-id path).
        ///
        /// Defaults to the only configured drive; required when more than one
        /// is configured.
        #[arg(long)]
        device: Option<String>,
        /// Destination directory
        #[arg(long = "to")]
        to: String,
        /// Refuse unless the tape's own label matches (a wrong-tape guard).
        ///
        /// Refuse unless the tape's own reported label matches (wrong-tape
        /// guard) — not a database lookup
        #[arg(long = "from")]
        from: Option<String>,
        /// Dump only the files at these tape positions (comma-separated or repeated).
        ///
        /// Dump only the files at these tape positions (comma-separated or
        /// repeated; File 0 is the ID thunk, 2 RESTORE.sh, 3 the front
        /// index). With --only, a file either names is dumped
        #[arg(long, value_delimiter = ',', value_name = "N")]
        positions: Vec<i32>,
        /// Dump only the files of these front-index types (comma-separated or repeated).
        ///
        /// Dump only the files of these front-index types (comma-separated
        /// or repeated): id_thunk, system_guide, restore_sh, front_index,
        /// tenant_envelope, operator_envelope, operator_envelope_backup,
        /// tenant_envelope_backup, data_slice, seal_marker
        #[arg(long, value_delimiter = ',', value_name = "TYPE")]
        only: Vec<String>,
    },
}

/// The unit a restore reads: the positional UNIT, or `--unit UNIT`, the
/// spelling every tape's File 1 prints and an heir may type on any restore
/// (ADR-0012 items 203 and 210). Exactly one of the two, which the usage
/// line shows as `<UNIT|--unit <UNIT>>`.
#[derive(clap::Args, Debug, Clone, PartialEq, Eq)]
#[group(required = true, multiple = false)]
pub struct UnitArg {
    /// Unit name
    #[arg(value_name = "UNIT")]
    name: Option<String>,
    /// The unit, as a flag: the same as UNIT
    #[arg(long = "unit", value_name = "UNIT")]
    unit: Option<String>,
}

impl UnitArg {
    /// The unit named, however it was given.
    pub fn name(&self) -> &str {
        self.name
            .as_deref()
            .or(self.unit.as_deref())
            .expect("clap requires UNIT or --unit")
    }
}

impl From<&str> for UnitArg {
    fn from(name: &str) -> Self {
        UnitArg {
            name: Some(name.to_string()),
            unit: None,
        }
    }
}

/// `restore file`'s unit and file (OQ2): `UNIT PATH`, or either one as a
/// flag — `--unit UNIT PATH`, `UNIT --file PATH`, `--unit UNIT --file PATH`.
/// The mixed `--unit UNIT PATH` is the spelling an heir who learned `--unit`
/// from File 1's `restore unit --unit` types here (ADR-0012 item 210 (a)).
///
/// Clap fills positionals in order, so after `--unit` the one positional
/// given lands in the first slot, UNIT's: [`Self::resolve`] reads it there as
/// the file. Two unit/file groups (each "exactly one of the two") cannot say
/// that: they refused `restore file --unit U PATH` as UNIT given twice.
#[derive(clap::Args, Debug, Clone, PartialEq, Eq)]
pub struct UnitAndFile {
    /// Unit name. After --unit, the file's PATH instead
    #[arg(value_name = "UNIT", required_unless_present = "unit")]
    first: Option<std::ffi::OsString>,
    /// File path within the unit, relative to its root, as `catalog list` prints it.
    ///
    /// Typed as the shell has it: the name's own bytes, so a name that is not UTF-8
    /// and one with a backslash are typed as they are. `catalog list` prints them
    /// escaped (`\\` for a backslash, `\xHH` for a byte outside UTF-8).
    ///
    /// Checked against the catalog's file list for the version before the tape
    /// is touched; a directory is refused
    #[arg(
        value_name = "PATH",
        required_unless_present_any = ["unit", "file"],
        conflicts_with_all = ["unit", "file"]
    )]
    second: Option<std::ffi::OsString>,
    /// The unit, as a flag: the same as UNIT
    #[arg(long = "unit", value_name = "UNIT")]
    unit: Option<String>,
    /// The file, as a flag: the same as PATH
    #[arg(long = "file", value_name = "PATH")]
    file: Option<std::ffi::OsString>,
}

impl UnitAndFile {
    /// The unit named, however it was given. A unit's name is UTF-8 text:
    /// one that is not is "" here, and [`Self::resolve`] refuses it by name.
    pub fn name(&self) -> &str {
        self.unit
            .as_deref()
            .or(self.first.as_deref().and_then(|f| f.to_str()))
            .unwrap_or("")
    }

    /// The unit and the file, however they were given. The two shapes clap
    /// cannot refuse on its own are refused here: `--unit` with no file, and
    /// `--unit UNIT --file PATH` with a positional besides. The file is
    /// returned in the escaped form (§1.5; ADR-0012 item 297): the operator
    /// types a name's bytes, and the catalog and the file table spell them so.
    pub fn resolve(&self) -> Result<(&str, String)> {
        use crate::volume::index::escape_os;
        let file = match (&self.unit, &self.first, &self.second, &self.file) {
            (None, Some(_), Some(path), None) => path,
            (None, Some(_), None, Some(path)) => path,
            (Some(_), Some(path), None, None) => path,
            (Some(_), None, None, Some(path)) => path,
            (Some(unit), None, None, None) => {
                return Err(TapectlError::Other(format!(
                    "restore file: no file named for unit \"{unit}\"; give it after the \
                     unit, as `tapectl restore file --unit {unit} PATH --to DIR`"
                )))
            }
            (Some(_), Some(extra), None, Some(path)) => {
                return Err(TapectlError::Other(format!(
                    "restore file: \"{}\" is one argument too many: --unit names the \
                     unit and --file names \"{}\"",
                    escape_os(extra),
                    escape_os(path)
                )))
            }
            _ => {
                return Err(TapectlError::Other(
                    "restore file: name the unit and the file, as `tapectl restore file UNIT \
                     PATH --to DIR`"
                        .into(),
                ))
            }
        };
        if self.unit.is_none() && self.first.as_deref().is_some_and(|f| f.to_str().is_none()) {
            return Err(TapectlError::Other(format!(
                "restore file: the unit name \"{}\" is not UTF-8; unit names are text",
                escape_os(self.first.as_deref().expect("checked"))
            )));
        }
        Ok((self.name(), escape_os(file).into_owned()))
    }
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
            no_file_check,
            key,
            dry_run,
        } => {
            // Item 107: a `--key` that is not an identity is refused first,
            // by name, before the device is resolved or the drive opened.
            let keys = crate::crypto::keys::KeyFile::read_all(key)?;
            let unit = unit.name();
            // Issue #435: no `--from` restores from the loaded tape, which
            // File 0 names. A dry run opens no drive, so it asks the catalog
            // instead, and previews from the one volume holding the unit,
            // naming it; several are listed for --from (ADR-0012 item 220).
            let only_holder = match (from, *dry_run) {
                (None, true) => {
                    let label = volume::restore::sole_holder(conn, unit, *version)?;
                    eprintln!(
                        "no --from given: previewing from {label}, the one volume this \
                         catalog has holding \"{unit}\""
                    );
                    Some(label)
                }
                _ => None,
            };
            let from = only_holder.as_ref().or(from.as_ref());
            let device = crate::cli::read_device(config, device.as_deref())?;
            let options = volume::restore::RestoreOptions {
                scratch: scratch.as_ref().map(std::path::PathBuf::from),
                overwrite: *overwrite,
                no_space_check: *no_space_check,
                no_file_check: *no_file_check,
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
                crate::render::print_json(&unit_report_json(&report));
            } else if report.dry_run {
                for line in unit_dry_run_lines(&report) {
                    println!("{line}");
                }
            } else {
                println!(
                    "restored \"{}\" v{} from {} ({}) to {}",
                    report.unit_name,
                    report.version,
                    report.volume_label,
                    crate::render::plural(report.slices as i64, "slice", "slices"),
                    report.destination,
                );
                for line in repair_lines(&report.substitutions, &report.filled) {
                    println!("{line}");
                }
                for line in tar_lines(&report.tar_said) {
                    eprintln!("{line}");
                }
                for line in id_map_lines(&report.id_map) {
                    println!("{line}");
                }
            }
        }

        RestoreCommands::File {
            unit,
            from,
            to,
            device,
            version,
            scratch,
            overwrite,
            no_space_check,
            no_file_check,
            key,
        } => {
            // Item 107: refused by name before anything else, as `restore
            // unit` does.
            let keys = crate::crypto::keys::KeyFile::read_all(key)?;
            let (unit, file) = unit.resolve()?;
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
                no_file_check: *no_file_check,
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
                    &file,
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
                    &file,
                    to,
                    &device,
                    DEFAULT_BLOCK_SIZE,
                    *version,
                    &options,
                )?,
            };
            let from = report.volume_label.clone();

            if json_output {
                crate::render::print_json(&serde_json::json!({
                    "file": file,
                    "unit": unit,
                    "volume": from,
                    "destination": to,
                    "substitutions": substitutions_json(&report.substitutions),
                    "filled": filled_json(&report.filled),
                    "id_map": id_map_json(&report.id_map),
                    "tar_said": report.tar_said,
                }));
            } else {
                println!("restored \"{file}\" from \"{unit}\" on {from} to {to}");
                for line in repair_lines(&report.substitutions, &report.filled) {
                    println!("{line}");
                }
                for line in tar_lines(&report.tar_said) {
                    eprintln!("{line}");
                }
                for line in id_map_lines(&report.id_map) {
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
            no_file_check,
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
                no_file_check: *no_file_check,
                keys,
            };
            let report = match label {
                Some(label) => {
                    let device = crate::cli::read_device(config, device.as_deref())?;
                    if units.is_empty() {
                        // Every unit the volume holds (#573): one that cannot
                        // be planned is listed as failed, the rest restored.
                        volume::restore::restore_every_unit(
                            conn,
                            paths,
                            config,
                            label,
                            to,
                            &options,
                            *fail_fast,
                            &device,
                            DEFAULT_BLOCK_SIZE,
                            *dry_run,
                        )?
                    } else {
                        let requests = volume::restore::unit_requests(units, to)?;
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

            // ADR-0012 items 266 and 288: with no unit named, the set is every
            // unit the volume holds a written slice of, a unit of which the
            // tape holds only a range included (it is read across
            // cartridges); none is named and left out.
            if json_output {
                crate::render::print_json(&units_report_json(&report));
            } else {
                for line in units_report_lines(&report) {
                    println!("{line}");
                }
            }
            let failed = report.failed();
            if failed > 0 {
                return Err(TapectlError::Other(format!(
                    "{failed} of {} not restored from {}",
                    crate::render::plural(report.units.len() as i64, "unit", "units"),
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
            // Issue #241: the tape's own files, as the heir path reads
            // them — there is no catalog
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
                crate::render::print_json(&serde_json::json!({
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
                }));
            } else {
                println!(
                    "dumped {} ({}) from volume \"{}\" (uuid {}) to {}",
                    crate::render::plural(report.files_dumped as i64, "file", "files"),
                    crate::util::format_bytes_binary(report.bytes_written as i64),
                    report.label,
                    report.uuid,
                    to
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
                    "raw-volume: {} of {} failed checksum verification",
                    report.mismatched_count,
                    crate::render::plural(report.files_dumped as i64, "file", "files")
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
            "  {} came back damaged or missing and were filled from another copy of this \
             version, each checked against its recorded checksum:",
            crate::render::plural(filled.len() as i64, "file", "files")
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

/// What tar said while it extracted (invariant 6: its exit status is never
/// the verdict, and what it said is not dropped), as lines for the terminal.
fn tar_lines(said: &[String]) -> Vec<String> {
    said.iter().map(|l| format!("tar: {l}")).collect()
}

/// The staging's id map (item 278), as lines (see
/// [`volume::restore::id_map_lines`]).
fn id_map_lines(map: &[crate::idmap::IdRow]) -> Vec<String> {
    volume::restore::id_map_lines(map)
}

/// `--json`'s `id_map`: one object for each id.
fn id_map_json(map: &[crate::idmap::IdRow]) -> serde_json::Value {
    map.iter()
        .map(|row| {
            serde_json::json!({
                "kind": row.kind.letter(),
                "id": row.id,
                "name": row.name,
            })
        })
        .collect()
}

/// The refusal for `restore volume --dry-run` with no label (issue #435): a
/// dry run opens no drive, and without a label the volume is whatever File 0
/// on the loaded tape says. (`restore unit` asks the catalog for the unit's
/// one holder instead, [`volume::restore::sole_holder`].)
fn no_volume_dry_run(command: &str) -> TapectlError {
    crate::cli::refuse_dry_run(
        command,
        "with no volume named the tape that is loaded names it, which means reading its \
         File 0, and a dry run opens no drive. Name the volume (the label) to preview.",
    )
}

/// `restore unit --json`: what was restored, or what a dry run would.
fn unit_report_json(report: &volume::restore::RestoreReport) -> serde_json::Value {
    serde_json::json!({
        "unit": report.unit_name,
        "volume": report.volume_label,
        "version": report.version,
        "slices": report.slices,
        "destination": report.destination,
        "dry_run": report.dry_run,
        // A dry run of a version with slices on no live volume would restore
        // it in part: the files in those slices would be left out and
        // listed (D17, item 266).
        "outcome": if !report.dry_run {
            "restored"
        } else if report.left_out_slices.is_empty() {
            "dry-run"
        } else {
            "dry-run-partial"
        },
        "left_out_slices": report.left_out_slices,
        "left_out": report.left_out,
        "substitutions": substitutions_json(&report.substitutions),
        "filled": filled_json(&report.filled),
        "id_map": id_map_json(&report.id_map),
        "tar_said": report.tar_said,
    })
}

/// `restore unit --dry-run`'s text: what would be restored, and, when slices
/// are on no live volume, that it would be in part and what is left out.
fn unit_dry_run_lines(report: &volume::restore::RestoreReport) -> Vec<String> {
    let mut lines = vec![format!(
        "would restore{} \"{}\" v{} from {} ({}) to {}",
        if report.left_out.is_some() {
            " in part"
        } else {
            ""
        },
        report.unit_name,
        report.version,
        report.volume_label,
        crate::render::plural(report.slices as i64, "slice", "slices"),
        report.destination,
    )];
    if let Some(why) = &report.left_out {
        lines.push(format!("  not whole: {why}"));
    }
    lines
}

/// `restore volume`'s report, one line per unit in the order the units lie
/// on the tape (the order they were restored in), then a summary.
pub(crate) fn units_report_lines(report: &volume::restore::UnitsReport) -> Vec<String> {
    let mut lines = Vec::with_capacity(report.units.len() + 1);
    for u in &report.units {
        // A unit that could not be planned has no version (0) and no slices.
        let what = if u.version > 0 {
            format!(
                "\"{}\" v{} ({}) to {}",
                u.unit_name,
                u.version,
                crate::render::plural(u.slices as i64, "slice", "slices"),
                u.destination
            )
        } else {
            format!("\"{}\" to {}", u.unit_name, u.destination)
        };
        lines.push(match (&u.error, u.attempted, report.dry_run) {
            (Some(e), false, true) => format!("would not restore {what}: {e}"),
            (_, _, true) if u.left_out.is_some() => format!(
                "would restore {what} in part: {}",
                u.left_out.as_deref().unwrap_or_default()
            ),
            (_, _, true) => format!("would restore {what}"),
            (None, true, false) => format!("restored {what}"),
            (Some(e), _, false) => format!("FAILED {what}: {e}"),
            (None, false, false) => format!("not attempted {what} (--fail-fast)"),
        });
        lines.extend(repair_lines(&u.substitutions, &u.filled));
        lines.extend(id_map_lines(&u.id_map));
    }
    let total = report.units.len();
    lines.push(if report.dry_run && report.failed() > 0 {
        format!(
            "would restore {} of {} from {} in one pass; {} could not be planned",
            total - report.failed(),
            crate::render::plural(total as i64, "unit", "units"),
            report.volume_label,
            report.failed()
        )
    } else if report.dry_run {
        format!(
            "would restore {} from {} in one pass",
            crate::render::plural(total as i64, "unit", "units"),
            report.volume_label
        )
    } else {
        format!(
            "{} of {} restored from {}",
            total - report.failed(),
            crate::render::plural(total as i64, "unit", "units"),
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
            let outcome = if u.error.is_some() {
                "failed"
            } else if report.dry_run && !u.left_out_slices.is_empty() {
                "dry-run-partial"
            } else if report.dry_run {
                "dry-run"
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
                "id_map": id_map_json(&u.id_map),
                "tar_said": u.tar_said,
                "left_out_slices": u.left_out_slices,
                "left_out": u.left_out,
            })
        })
        .collect();
    serde_json::json!({
        "volume": report.volume_label,
        "dry_run": report.dry_run,
        "failed": report.failed(),
        "units": units,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> std::result::Result<UnitAndFile, clap::Error> {
        let argv = ["tapectl", "restore", "file"]
            .iter()
            .chain(args)
            .chain(&["--to", "/d"]);
        match crate::cli::Cli::try_parse_from(argv)?.command {
            crate::cli::Commands::Restore {
                command: RestoreCommands::File { unit, .. },
            } => Ok(unit),
            other => panic!("not restore file: {other:?}"),
        }
    }

    /// OQ2 and ADR-0012 item 210 (a): every spelling of the unit and the
    /// file parses to the same two names, the mixed `--unit U PATH` an heir
    /// types after File 1's `restore unit --unit` included.
    #[test]
    fn restore_file_takes_the_unit_and_the_file_in_every_spelling() {
        for args in [
            &["photos", "a/b.txt"][..],
            &["--unit", "photos", "a/b.txt"],
            &["a/b.txt", "--unit", "photos"],
            &["photos", "--file", "a/b.txt"],
            &["--unit", "photos", "--file", "a/b.txt"],
            &["--file", "a/b.txt", "--unit", "photos"],
        ] {
            let target = parse(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            assert_eq!(
                target.resolve().unwrap(),
                ("photos", "a/b.txt".to_string()),
                "{args:?}"
            );
            assert_eq!(target.name(), "photos", "{args:?}");
        }
    }

    /// ADR-0012 item 297 (#569): the operator types a name's bytes, and the
    /// file is looked up by its escaped form. clap takes the bytes whole (a
    /// `String` argument refused a name that is not UTF-8).
    #[test]
    fn restore_file_takes_the_typed_path_as_bytes_and_escapes_it() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let typed: &[(&[u8], &str)] = &[
            (b"caf\xe9.txt", "caf\\xe9.txt"),
            (b"back\\slash.txt", "back\\\\slash.txt"),
            (b"plain/\xff", "plain/\\xff"),
            (b"tab\there", "tab\\there"),
        ];
        for (bytes, want) in typed {
            let file = OsString::from_vec(bytes.to_vec());
            for shape in 0..3 {
                let argv: Vec<OsString> = ["tapectl", "restore", "file"]
                    .iter()
                    .map(OsString::from)
                    .chain(match shape {
                        0 => vec!["photos".into(), file.clone()],
                        1 => vec!["--unit".into(), "photos".into(), file.clone()],
                        _ => vec!["photos".into(), "--file".into(), file.clone()],
                    })
                    .chain(["--to".into(), "/d".into()])
                    .collect();
                let target = match crate::cli::Cli::try_parse_from(argv).unwrap().command {
                    crate::cli::Commands::Restore {
                        command: RestoreCommands::File { unit, .. },
                    } => unit,
                    other => panic!("not restore file: {other:?}"),
                };
                let (unit, file) = target.resolve().unwrap();
                assert_eq!((unit, file.as_str()), ("photos", *want), "shape {shape}");
            }
        }
    }

    /// The shapes that name too little or too much are refused: by clap
    /// where it can say so, by [`UnitAndFile::resolve`] where it cannot.
    #[test]
    fn restore_file_refuses_a_missing_or_doubled_name() {
        for args in [
            &["photos"][..],
            &["--file", "a/b.txt"],
            &["photos", "a/b.txt", "--unit", "photos"],
            &["photos", "a/b.txt", "--file", "a/b.txt"],
            &[],
        ] {
            assert!(parse(args).is_err(), "{args:?} must not parse");
        }
        let lone = parse(&["--unit", "photos"]).expect("clap leaves this to resolve");
        let err = lone.resolve().unwrap_err().to_string();
        assert!(err.contains("no file named for unit \"photos\""), "{err}");
        let doubled = parse(&["--unit", "photos", "x", "--file", "a/b.txt"])
            .expect("clap leaves this to resolve");
        let err = doubled.resolve().unwrap_err().to_string();
        assert!(err.contains("\"x\" is one argument too many"), "{err}");
    }

    fn dry_report(left_out: Option<&str>, slices: &[i64]) -> volume::restore::RestoreReport {
        volume::restore::RestoreReport {
            unit_name: "u".into(),
            volume_label: "L".into(),
            version: 1,
            slices: 3,
            destination: "/d".into(),
            dry_run: true,
            success: true,
            substitutions: Vec::new(),
            filled: Vec::new(),
            id_map: Vec::new(),
            tar_said: Vec::new(),
            left_out_slices: slices.to_vec(),
            left_out: left_out.map(str::to_string),
        }
    }

    /// restore-3: a dry run that would leave files out says so, in words and
    /// in the JSON, and the outcome is not the whole restore's.
    #[test]
    fn a_dry_run_with_slices_on_no_live_volume_says_it_would_restore_in_part() {
        let why = "unit \"u\" version 1: slices 4 to 6 of it are on no live volume";
        let part = dry_report(Some(why), &[4, 5, 6]);
        let lines = unit_dry_run_lines(&part);
        assert!(
            lines[0].starts_with("would restore in part \"u\""),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains(why)), "{lines:?}");
        let json = unit_report_json(&part);
        assert_eq!(json["outcome"], "dry-run-partial");
        assert_eq!(json["left_out_slices"], serde_json::json!([4, 5, 6]));
        assert_eq!(json["left_out"], why);
        assert_eq!(json["dry_run"], true, "the existing field is stable");

        let whole = dry_report(None, &[]);
        assert!(unit_dry_run_lines(&whole)[0].starts_with("would restore \"u\""));
        assert_eq!(unit_report_json(&whole)["outcome"], "dry-run");
    }

    /// restore-3 and restore-5 for `restore volume --dry-run`: a unit with
    /// slices on no live volume would be restored in part, one that cannot be
    /// planned would not be restored, and the summary and the JSON agree with
    /// the exit status.
    #[test]
    fn a_volume_dry_run_counts_what_it_would_not_restore() {
        use crate::volume::restore::short_tape::{short_tape, LABEL};
        let t = short_tape();
        let dest = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dest.path().join("alpha")).unwrap();
        std::fs::write(dest.path().join("alpha").join("stray"), b"x").unwrap();
        let report = volume::restore::restore_every_unit(
            &t.conn,
            &t.paths,
            &crate::config::Config::default(),
            LABEL,
            &dest.path().to_string_lossy(),
            &volume::restore::RestoreOptions::default(),
            false,
            "/nonexistent/nst",
            4096,
            true,
        )
        .unwrap();
        assert_eq!(report.failed(), 1, "{report:?}");
        let lines = units_report_lines(&report);
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("would not restore \"alpha\"")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("would restore \"bravo\"")
                    && l.contains(" in part: ")
                    && l.contains("slice 2 of it is on no live volume")),
            "{lines:?}"
        );
        let last = lines.last().unwrap();
        assert!(
            last.contains("1 of 2 units") && last.contains("1 could not be planned"),
            "{last}"
        );
        let json = units_report_json(&report);
        assert_eq!(json["failed"], 1);
        let outcome = |n: &str| {
            json["units"]
                .as_array()
                .unwrap()
                .iter()
                .find(|u| u["unit"] == n)
                .unwrap()["outcome"]
                .clone()
        };
        assert_eq!(outcome("alpha"), "failed");
        assert_eq!(outcome("bravo"), "dry-run-partial");
    }
}
