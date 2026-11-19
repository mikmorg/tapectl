use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use tracing::{info, warn};

use crate::config::{Config, TapectlPaths};
use crate::crypto::keys;
use crate::db::{queries, stage_files, stage_ids};
use crate::error::{Result, TapectlError};
use crate::idmap::IdRow;
use crate::progress;
use crate::staging::pack::Algorithm;
use crate::store::Store;
use crate::tape::contact::{self, ContactSite, Operation};
use crate::volume::filetable::{self, Kind};
use crate::volume::restore_record::{self, RestoreRecord, TarReport};

mod decode;
mod fill;
mod reader;
#[cfg(test)]
pub(crate) mod short_tape;
mod span;
mod supply;
#[cfg(test)]
use supply::FixedBay;
pub use supply::Substitution;
use supply::{Bay, DeviceBay, Supply};

/// What a restore through `restore unit`'s one drive path is FOR — the whole
/// unit, or one file out of it (issue #306).
///
/// `restore file` reaches the drive through the same seam as `restore unit`
/// (one contact, one health reading, `Operation::RestoreUnit`), and the
/// `restores` row it writes says `kind = 'file'`. Carrying the target through
/// the seam, rather than having `restore_file` copy the one entry out
/// AFTER the seam returns, puts the placing of that entry INSIDE the
/// recorded span: a "file not found in restored unit" is then a `failed`
/// row, not an `ok` row beside a non-zero exit.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RestoreTarget<'a> {
    /// The whole unit, extracted straight into `dest_dir`.
    Unit { dest_dir: &'a str },
    /// One entry: tar extracts just it (and a hard link's target) into the
    /// scratch directory (issue #406; it used to be the whole unit, in
    /// $TMPDIR), and it is then placed into `dest_dir`, the directory the
    /// operator named.
    File {
        /// In the escaped form of §1.5 (ADR-0012 item 297): the file table's
        /// own spelling of the path, so a lookup compares text and a name
        /// that is not UTF-8 is carried by its bytes.
        file_path: &'a str,
        dest_dir: &'a str,
    },
}

impl<'a> RestoreTarget<'a> {
    /// The directory the operator named — what `restores.destination`
    /// records and what the report prints.
    fn destination(&self) -> &'a str {
        match *self {
            RestoreTarget::Unit { dest_dir } | RestoreTarget::File { dest_dir, .. } => dest_dir,
        }
    }

    fn file_path(&self) -> Option<&'a str> {
        match *self {
            RestoreTarget::Unit { .. } => None,
            RestoreTarget::File { file_path, .. } => Some(file_path),
        }
    }

    /// `restores.kind`.
    fn kind(&self) -> &'static str {
        match self {
            RestoreTarget::Unit { .. } => restore_record::KIND_UNIT,
            RestoreTarget::File { .. } => restore_record::KIND_FILE,
        }
    }
}

/// How `restore unit` and `restore file` may use the disk (issue #406) —
/// their `--scratch`, `--overwrite` and `--no-space-check` flags.
#[derive(Debug, Clone, Default)]
pub struct RestoreOptions {
    /// Where `restore file`'s `.tapectl-restore-tmp` directory is made: the
    /// one file is extracted there and then placed. `None`: inside the
    /// destination. A restore of a unit puts nothing there: the members go
    /// to tar as they are read, and no slice is ever written to disk.
    /// Never `$TMPDIR` — a system temp directory is often RAM or the small
    /// root filesystem.
    pub scratch: Option<PathBuf>,
    /// Restore into a destination that already holds files: `restore unit`
    /// replaces any that collide (`tar --overwrite`), `restore file`
    /// replaces the one file. Without it either refuses before the tape is
    /// touched.
    pub overwrite: bool,
    /// Skip the free-space check, for a filesystem that holds more than
    /// `statvfs` reports (a compressed or thin-provisioned one) — the same
    /// escape RESTORE.sh's `--no-space-check` is.
    pub no_space_check: bool,
    /// Do not hash every restored file on disk once tar has ended (D16,
    /// `--no-file-check`). Every member is still hashed as it is fed; the
    /// check on disk costs one more read of what was restored, and is
    /// RESTORE.sh's too (invariant 6).
    pub no_file_check: bool,
    /// Identity files given with `--key` (ADR-0012 2026-10-19 item 107),
    /// read before the drive was opened. They are used for decryption in
    /// this restore only and ADD to the home's own keys: never copied into
    /// the home, never registered as the operator's or the escrow's
    /// identity, never logged (`KeyFile`'s `Debug` shows a path only).
    pub keys: Vec<keys::KeyFile>,
}

/// The scratch directory's name, inside the destination or `--scratch`. Only
/// `restore file` makes one: it extracts the one file there, then places it.
pub(crate) const SCRATCH_NAME: &str = ".tapectl-restore-tmp";

/// Where a restore keeps its decrypted slices (issue #406):
/// [`SCRATCH_NAME`] inside `--scratch DIR` when given, else inside the
/// destination — never `$TMPDIR`. The directory is the restore's own: it is
/// refused if it already exists, and removed when the restore ends.
pub(crate) fn scratch_dir(destination: &Path, scratch: Option<&Path>) -> PathBuf {
    scratch.unwrap_or(destination).join(SCRATCH_NAME)
}

/// What the contacted half of a restore measured on the way, whether or not
/// it got to the end — the figures the `restores` row records (issue #306).
///
/// Filled in as the restore proceeds and read by the seam after the
/// contact closes, so a restore that failed after three slices still says
/// three, and one that failed inside tar still carries tar's report.
#[derive(Debug, Default)]
struct RestoreTrace {
    /// Slices decrypted off the tape so far.
    slices_read: i64,
    /// Their plaintext byte total, as measured through the hashing writer.
    bytes_decrypted: i64,
    /// What the last tar run did, whenever tar ran.
    tar: Option<TarReport>,
    /// `tar --version`'s first line, read once tar has run; `None` if it
    /// could not be.
    tar_version: Option<String>,
    /// Members restored (fed and standing after every check).
    files_restored: Option<i64>,
    /// tar's own complaints, repeated for the operator (invariant 6: tar's
    /// exit status is never the verdict, but what it said is not dropped).
    tar_said: Vec<String>,
    /// [`RestoreTarget::File`] only: the one entry was placed.
    placed: bool,
    /// Files that came back damaged or missing and were filled from another
    /// copy of the same version: the path, and the volume and check.
    filled: Vec<(String, String)>,
}

/// Removes the restore scratch directory when it goes out of scope, on every
/// path out of [`restore_unit`] — success, `?`, panic (issue #102).
///
/// The scratch directory holds **decrypted** plaintext: the one file `restore
/// file` extracts before it places it, and what a repair from another copy
/// reads. Cleanup that lived at the end of the happy path only would leave
/// that plaintext sitting in the destination directory the operator chose,
/// with nothing said about it, after any failure between creating the
/// directory and finishing the restore — a checksum mismatch, a full disk, a
/// missing key, a tape read error. Everywhere else in this tool plaintext
/// exists only transiently inside staging; this is the one place it could be
/// left behind outside it, and it would be on the failure path.
///
/// The directory deliberately lives *under the destination* (or under
/// `--scratch DIR`, [`scratch_dir`]) rather than in `std::env::temp_dir()`:
/// a system temp dir on a small tmpfs (or the small root filesystem) is the
/// wrong home for a file that may be hundreds of gigabytes. That is why this
/// is a hand-written guard and not `tempfile::tempdir()`.
///
/// A guard runs only if the process does: a SIGKILL or a power loss leaves
/// the directory behind. So the next restore refuses to start while it
/// exists, naming it ([`preflight`]).
///
/// A removal failure is reported at `warn!` naming the path, never swallowed
/// and never escalated: the operator needs to know plaintext remains, but a
/// cleanup error must not mask the original failure that triggered it — and
/// `Drop` cannot return one anyway.
struct RestoreScratch(PathBuf);

impl Drop for RestoreScratch {
    fn drop(&mut self) {
        if !self.0.exists() {
            return;
        }
        if let Err(e) = fs::remove_dir_all(&self.0) {
            warn!(
                path = %self.0.display(),
                error = %e,
                "could not remove the restore scratch directory — it may still \
                 contain DECRYPTED archive slices; remove it by hand",
            );
        }
    }
}

/// Restore a unit from a volume to a destination directory.
// 11 args reflects the CLI's flat shape (unit/volume/dest/device/block_size/
// version/dry_run/options alongside conn/paths/config); interim allow. This used to carry a
// comment blaming the count on "the store read seam in #71 (epic #20)" —
// wrong: #71 was closed and scoped only to the write-side execute/confirm
// seam. The read seam migrated here directly (issue #85): per-slice tape
// access below now goes through `Store::read_file` via `read_slice`,
// not a bespoke `TapeDevice` call.
#[allow(clippy::too_many_arguments)]
pub fn restore_unit(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    unit_name: &str,
    volume_label: &str,
    dest_dir: &str,
    device: &str,
    block_size: usize,
    version: Option<i64>,
    dry_run: bool,
    options: &RestoreOptions,
) -> Result<RestoreReport> {
    restore_through_drive(
        conn,
        paths,
        config,
        unit_name,
        volume_label,
        RestoreTarget::Unit { dest_dir },
        options,
        device,
        block_size,
        version,
        dry_run,
    )
}

/// [`restore_unit`] and [`restore_file`]'s one path to the drive: resolve
/// the version, run the [`preflight`] that may refuse with no tape contact,
/// take the two MAM reads, open the store, and hand off to the store seam
/// with the [`RestoreTarget`] that says which of the two this is.
#[allow(clippy::too_many_arguments)]
fn restore_through_drive(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    unit_name: &str,
    volume_label: &str,
    target: RestoreTarget<'_>,
    options: &RestoreOptions,
    device: &str,
    block_size: usize,
    version: Option<i64>,
    dry_run: bool,
) -> Result<RestoreReport> {
    let (selection, plan) = plan_before_tape(
        conn,
        config,
        unit_name,
        volume_label,
        target,
        options,
        version,
    )?;

    if dry_run {
        return Ok(RestoreReport {
            unit_name: unit_name.to_string(),
            volume_label: volume_label.to_string(),
            version: selection.version,
            slices: plan.positions.len(),
            destination: target.destination().to_string(),
            dry_run: true,
            success: true,
            substitutions: Vec::new(),
            filled: Vec::new(),
            id_map: id_map_of(conn, plan.stage_set_id),
            tar_said: Vec::new(),
        });
    }

    // Issue #166: refuse before the store is opened if this drive cannot
    // read the loaded medium. Proceeds silently with no configured backend
    // or nothing detected — the DR machine with keys and no `drive add`
    // yet (ADR-0005), same leniency as the MAM read just below.
    //
    // Both MAM reads are held and journalled when the contact opens (issue
    // #297). `DeviceBay::load` takes them, then opens the store read-only,
    // positioned at BOT: reading the MAM opens the device read-only and drops
    // the fd, and the st driver refuses a second concurrent open, so they
    // come first. LENIENT — no configured backend yields an absence, which is
    // the DR machine.
    let mut bay = DeviceBay {
        device,
        block_size,
        operation: Operation::RestoreUnit,
        store: None,
    };
    let arrival = bay.load(conn, config)?;
    arrival.with_site(config, Operation::RestoreUnit, device, |site| {
        restore_unit_from_bay(
            conn,
            paths,
            config,
            unit_name,
            volume_label,
            selection.version,
            target,
            options,
            &mut bay,
            *site,
        )
    })
}

/// Everything a `restore unit` / `restore file` settles from the catalog
/// before the tape is read: the unit and its tenant, the one snapshot
/// version's slices on the volume, the stream-or-spool plan and the
/// preflight that may refuse. [`restore_through_drive`] runs it before the
/// drive opens; the loaded-tape path ([`restore_unit_from_loaded_store`])
/// cannot know the volume until it has read File 0, so it runs the same
/// code right after.
fn plan_before_tape(
    conn: &Connection,
    _config: &Config,
    unit_name: &str,
    volume_label: &str,
    target: RestoreTarget<'_>,
    options: &RestoreOptions,
    version: Option<i64>,
) -> Result<(RestoreSelection, RestorePlan)> {
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;

    // Validated here as well as in `restore_unit_from_store`, so a dry run —
    // which never reaches the store half — still refuses a unit whose tenant
    // has gone.
    queries::get_tenant_by_id(conn, unit.tenant_id)?
        .ok_or_else(|| TapectlError::Other("tenant not found".into()))?;

    // Exactly one snapshot version's slices on this volume (issue #315) —
    // resolved before the drive is touched, so a `--version` the volume does
    // not carry is refused with no tape I/O, and a dry run counts only the
    // selected version's slices.
    let selection = select_write_positions(conn, unit_name, volume_label, version)?;

    // Issue #406: everything that can be refused without the tape is refused
    // here, before the first contact — a typo in `--file`, a destination
    // that is not empty, a leftover scratch directory, a disk too small.
    // Each of these used to surface only after every slice was read (~5 h
    // for a large unit), or never. A dry run takes the same checks, so a
    // dry run that says "would restore" means it.
    let scratch = scratch_dir(Path::new(target.destination()), options.scratch.as_deref());
    preflight_checks(conn, unit_name, &selection, target, &scratch, options)?;
    // The file table decides which members, and so which slices, this
    // restore reads: decided here, so the space check asks for what this
    // restore will actually write.
    let plan = plan_restore(conn, &selection, target)?;
    if options.no_space_check {
        info!("disk space not checked (--no-space-check)");
    } else {
        check_restore_space(Path::new(target.destination()), &scratch, &plan)?;
    }
    Ok((selection, plan))
}

/// Open the drive for a restore that names no volume (issue #435) and hand
/// `body` the bay and the contact site: the same four steps, in the same
/// order, as [`restore_through_drive`] and [`restore_units`] take for a
/// named volume — the no-medium check first, both MAM reads held for the
/// contact, then the read-only store. One function so `restore unit`,
/// `restore file` and `restore volume` cannot open the drive three ways.
fn with_loaded_drive<R>(
    conn: &Connection,
    config: &Config,
    device: &str,
    block_size: usize,
    operation: Operation,
    body: impl FnOnce(&mut dyn Bay, ContactSite<'_>) -> Result<R>,
) -> Result<R> {
    let mut bay = DeviceBay {
        device,
        block_size,
        operation,
        store: None,
    };
    let arrival = bay.load(conn, config)?;
    arrival.with_site(config, operation, device, |site| body(&mut bay, *site))
}

/// [`restore_unit`] for the tape that is loaded, named by no `--from` (issue
/// #435): File 0 identifies the volume. Never a dry run — a preview that
/// opens no drive cannot know the volume — so the CLI refuses that before
/// it gets here.
#[allow(clippy::too_many_arguments)]
pub fn restore_unit_from_loaded(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    unit_name: &str,
    dest_dir: &str,
    device: &str,
    block_size: usize,
    version: Option<i64>,
    options: &RestoreOptions,
) -> Result<RestoreReport> {
    with_loaded_drive(
        conn,
        config,
        device,
        block_size,
        Operation::RestoreUnit,
        |bay, site| {
            restore_unit_from_loaded_bay(
                conn,
                paths,
                config,
                unit_name,
                RestoreTarget::Unit { dest_dir },
                options,
                version,
                bay,
                site,
            )
        },
    )
}

/// [`restore_file`] for the tape that is loaded; see
/// [`restore_unit_from_loaded`]. Returns the report so the caller can name
/// the volume that was chosen.
#[allow(clippy::too_many_arguments)]
pub fn restore_file_from_loaded(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    unit_name: &str,
    file_path: &str,
    dest_dir: &str,
    device: &str,
    block_size: usize,
    version: Option<i64>,
    options: &RestoreOptions,
) -> Result<RestoreReport> {
    with_loaded_drive(
        conn,
        config,
        device,
        block_size,
        Operation::RestoreUnit,
        |bay, site| {
            restore_unit_from_loaded_bay(
                conn,
                paths,
                config,
                unit_name,
                RestoreTarget::File {
                    file_path,
                    dest_dir,
                },
                options,
                version,
                bay,
                site,
            )
        },
    )
}

/// The loaded-tape restore, minus the drive: read File 0 ONCE off `store`,
/// find the volume in the catalog, then run the checks a named volume runs
/// before the tape ([`plan_before_tape`]) and restore through
/// [`restore_unit_in_contact`] on a store that already holds that File 0 —
/// its contact check finds it where it looks and reads nothing again.
///
/// The order a named volume has — every catalog refusal before the drive
/// opens (#406) — cannot hold here: the catalog questions are about a
/// volume, and the tape is what names it. So the drive is open and File 0
/// read first, then the same refusals, and still no slice is read, no
/// scratch directory made and no key loaded before them.
///
/// **The contact opens first** (issue #442), before File 0 is read, and the
/// volume is attached to it once the catalog has resolved it by uuid. The
/// drive is open from the first step, so a refusal at any later one — a tape
/// the catalog does not know, a destination that is not empty — is a contact
/// closed `failed` with its reason ([`refuse_loaded_contact`]), as a named
/// volume's wrong tape is; and the restore that follows is the SAME contact
/// ([`restore_unit_in_contact`]), not a second one. A refusal writes no
/// `restores` row: the restore never began, as a named volume's refusals
/// before the drive write none.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn restore_unit_from_loaded_store(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    unit_name: &str,
    target: RestoreTarget<'_>,
    options: &RestoreOptions,
    version: Option<i64>,
    store: &mut dyn Store,
    site: ContactSite<'_>,
) -> Result<RestoreReport> {
    restore_unit_from_loaded_bay(
        conn,
        paths,
        config,
        unit_name,
        target,
        options,
        version,
        &mut FixedBay(store),
        site,
    )
}

/// [`restore_unit_from_loaded_store`] over a [`Bay`]: the drive that can
/// also be asked for another cartridge.
#[allow(clippy::too_many_arguments)]
pub(crate) fn restore_unit_from_loaded_bay(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    unit_name: &str,
    target: RestoreTarget<'_>,
    options: &RestoreOptions,
    version: Option<i64>,
    bay: &mut dyn Bay,
    site: ContactSite<'_>,
) -> Result<RestoreReport> {
    let started_at = restore_record::now_sqlite();
    let phase = progress::phase("contact-open", None);
    let guard = site.open(conn, None);
    phase.done();
    let kind = target.kind();
    let (found, primed) = match crate::volume::loaded::identify(conn, bay.store()) {
        Ok((found, peeked)) => (found, peeked.into_file0()),
        Err(e) => return refuse_loaded_contact(conn, guard, &site, None, kind, e),
    };
    guard.record_volume(found.volume_id);
    crate::volume::loaded::announce("restoring from", &found);
    let (selection, _plan) = match plan_before_tape(
        conn,
        config,
        unit_name,
        &found.label,
        target,
        options,
        version,
    ) {
        Ok(planned) => planned,
        Err(e) => return refuse_loaded_contact(conn, guard, &site, Some(found.volume_id), kind, e),
    };
    restore_unit_in_contact(
        conn,
        paths,
        config,
        guard,
        Some(found.volume_id),
        &started_at,
        unit_name,
        &found.label,
        selection.version,
        target,
        options,
        bay,
        primed,
        &site,
    )
}

/// Close the contact of a no-label restore (issue #435) that was refused
/// after its drive was opened and File 0 read (issue #442): `failed`, with
/// the reason, and the one post-command reading every contact takes. The
/// refusal goes out unchanged.
///
/// `volume_id` is the volume the catalog resolved, `None` when it resolved
/// none (a tape it does not know). `kind` names the restore for its phase
/// timings, which have no volume to be recorded against in that case.
fn refuse_loaded_contact<T>(
    conn: &Connection,
    guard: contact::ContactGuard<'_>,
    site: &ContactSite<'_>,
    volume_id: Option<i64>,
    kind: &str,
    e: TapectlError,
) -> Result<T> {
    let contact_id = guard.id();
    guard.finish(contact::OUTCOME_FAILED, Some(&e.to_string()));
    let phase = progress::phase("health-sweep", None);
    crate::volume::write::health_after_read_contact(conn, site, volume_id, contact_id);
    phase.done();
    match volume_id {
        Some(id) => crate::db::phase_timings::record_drained(
            conn,
            &format!("restore {kind}"),
            crate::db::phase_timings::Subject::Volume(id),
        ),
        None => {
            let _ = progress::drain();
        }
    }
    Err(e)
}

/// The restore's checks that need no tape and no file table (issue #406), in
/// the order an operator would want them named: a leftover scratch
/// directory, the destination. (The requested file is checked against the
/// file table by [`plan_restore`], and the space after it.)
fn preflight_checks(
    _conn: &Connection,
    _unit_name: &str,
    _selection: &RestoreSelection,
    target: RestoreTarget<'_>,
    scratch: &Path,
    options: &RestoreOptions,
) -> Result<()> {
    let destination = Path::new(target.destination());

    // `restore file` extracts into a scratch directory first. One that
    // already exists is a killed restore's: its guard never ran, and it may
    // hold a restored file. Never mixed with a new one, never silently
    // removed. (A restore of a unit makes none.)
    if matches!(target, RestoreTarget::File { .. }) && fs::symlink_metadata(scratch).is_ok() {
        return Err(TapectlError::Other(format!(
            "refusing to restore: the scratch directory {} already exists. A restore that was \
             killed (or lost power) leaves it behind, and it may hold the restored file. \
             Remove it (`rm -rf {}`) and run the restore again. Nothing was read from tape.",
            scratch.display(),
            scratch.display()
        )));
    }

    // The destination. `restore unit` wants an empty (or new) directory: tar
    // keeps an existing file rather than overwrite it, which would be found
    // only after the whole unit was read and extracted. `restore file`
    // places one file, so only that one name matters.
    if !options.overwrite {
        match target {
            RestoreTarget::Unit { .. } => {
                if let Some(entry) = first_entry(destination)? {
                    return Err(TapectlError::Other(format!(
                        "refusing to restore into {}: it is not empty (it holds \"{entry}\", \
                         and maybe more). tar would keep every existing file that collides with \
                         one from the tape, so the result would not be what is on tape. Restore \
                         into an empty or new directory, or pass --overwrite to replace what \
                         collides. Nothing was read from tape.",
                        destination.display()
                    )));
                }
            }
            RestoreTarget::File { file_path, .. } => {
                let placed = placed_path(destination, file_path);
                if fs::symlink_metadata(&placed).is_ok() {
                    return Err(TapectlError::Other(format!(
                        "refusing to restore \"{file_path}\": {} already exists. Choose another \
                         --to, move that file away, or pass --overwrite to replace it. Nothing \
                         was read from tape.",
                        crate::volume::index::escape_os(placed.as_os_str())
                    )));
                }
            }
        }
    }
    Ok(())
}

/// The first entry of `dir`, or `None` when it is empty or does not exist
/// yet. A path that exists and is not a directory is refused.
fn first_entry(dir: &Path) -> Result<Option<String>> {
    match fs::read_dir(dir) {
        Ok(mut entries) => Ok(entries
            .next()
            .transpose()?
            .map(|e| crate::volume::index::escape_os(&e.file_name()).into_owned())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => Err(TapectlError::Other(
            format!("--to {} exists and is not a directory", dir.display()),
        )),
        Err(e) => Err(TapectlError::Other(format!(
            "cannot read the destination {}: {e}",
            dir.display()
        ))),
    }
}

/// Where `restore file` puts `file_path` (in the escaped form, §1.5): its
/// last component, as the bytes it is, directly in the destination (the
/// directory the operator named).
fn placed_path(destination: &Path, file_path: &str) -> PathBuf {
    let name = crate::volume::index::on_disk(file_path);
    destination.join(name.file_name().unwrap_or(name.as_os_str()))
}

/// Refuse a restore the disk cannot hold, before the first slice is read
/// (issue #406), for what THIS restore writes ([`RestorePlan`]):
///
/// - **`restore unit`**: the members' own length in the destination, about
///   the unit's size. Nothing of a slice is ever on disk: each is held in
///   memory while its members are cut out and handed to tar.
/// - **`restore file`**: the one file (and the target of a hard link) is
///   extracted in scratch and then moved into place, which costs nothing
///   more on one filesystem.
///
/// RESTORE.sh's `check_space` (the heir's path) asks for its own figure.
fn check_restore_space(destination: &Path, scratch: &Path, plan: &RestorePlan) -> Result<()> {
    let why = if plan.file_size.is_some() {
        "A restore file extracts the file in the scratch directory and then moves it into place, \
         so it needs the file's size."
    } else {
        "A restore feeds each member to tar as its slice is read, and so needs about the unit's \
         size in --to; no slice is written to disk."
    };
    match space_needs(destination, scratch, plan.bytes, plan.file_size) {
        Some(needs) => require_space(&needs, why),
        None => Ok(()),
    }
}

/// Disk a restore takes on one filesystem: `kept` stays (the restored
/// files), `transient` is gone when the restore ends (its scratch).
#[derive(Debug, Clone)]
struct SpaceNeed {
    /// An existing directory on the filesystem — where free space is asked.
    dir: PathBuf,
    /// Its `st_dev`; `None` when it cannot be read (then counted with every
    /// other unknown: over-ask, never under-ask).
    dev: Option<u64>,
    kept: i64,
    transient: i64,
}

/// What one restore needs, per filesystem ([`check_restore_space`] says what
/// and why): `bytes` is what is extracted, `file_size` the one file a
/// `restore file` places. `None` when the filesystems cannot be found: then
/// the check is skipped, with a warning.
fn space_needs(
    destination: &Path,
    scratch: &Path,
    bytes: i64,
    file_size: Option<i64>,
) -> Option<Vec<SpaceNeed>> {
    let scratch_root = scratch.parent().unwrap_or(scratch);
    let (Some(scratch_fs), Some(dest_fs)) = (existing(scratch_root), existing(destination)) else {
        warn!(
            destination = %destination.display(),
            scratch = %scratch_root.display(),
            "cannot find the filesystem a restore would write to; continuing without the space \
             check"
        );
        return None;
    };
    let dev = |dir: &Path| {
        fs::metadata(dir)
            .ok()
            .map(|m| std::os::unix::fs::MetadataExt::dev(&m))
    };
    let (scratch_dev, dest_dev) = (dev(&scratch_fs), dev(&dest_fs));
    // One filesystem unless proven otherwise: over-ask, never under-ask.
    let same_fs = scratch_dev.is_none() || dest_dev.is_none() || scratch_dev == dest_dev;
    let need = |dir: &PathBuf, dev: Option<u64>, kept: i64, transient: i64| SpaceNeed {
        dir: dir.clone(),
        dev,
        kept,
        transient,
    };
    Some(match (file_size, same_fs) {
        // `restore file` extracts into scratch, then renames into place,
        // which costs nothing more on one filesystem.
        (Some(file), true) => vec![need(&dest_fs, dest_dev, 0, bytes.max(file))],
        // ...and is a copy across two.
        (Some(file), false) => vec![
            need(&scratch_fs, scratch_dev, 0, bytes.max(file)),
            need(&dest_fs, dest_dev, file, 0),
        ],
        (None, _) => vec![need(&dest_fs, dest_dev, bytes, 0)],
    })
}

/// Refuse when a filesystem cannot hold `needs`: per filesystem, everything
/// kept plus the largest transient — restores of a set run one after
/// another, each removing its scratch before the next (issue #398).
fn require_space(needs: &[SpaceNeed], why: &str) -> Result<()> {
    let mut by_fs: Vec<(Option<u64>, PathBuf, i64, i64)> = Vec::new();
    for n in needs {
        match by_fs.iter_mut().find(|(dev, ..)| *dev == n.dev) {
            Some((_, _, kept, transient)) => {
                *kept += n.kept;
                *transient = (*transient).max(n.transient);
            }
            None => by_fs.push((n.dev, n.dir.clone(), n.kept, n.transient)),
        }
    }
    for (_, dir, kept, transient) in by_fs {
        let bytes = kept + transient;
        let Some(free) = restore_free_bytes(&dir) else {
            warn!(
                path = %dir.display(),
                "cannot measure free disk space; continuing without the check"
            );
            continue;
        };
        info!(
            path = %dir.display(),
            needs = %crate::util::format_bytes_binary(bytes),
            free = %crate::util::format_bytes_binary(free),
            "restore disk space"
        );
        if free < bytes {
            return Err(TapectlError::Other(format!(
                "not enough disk space in {} for this restore: it needs about {}, and {} is \
                 free. Nothing was read from tape. {why} Free space there, choose a larger disk \
                 with --to, or put any decrypted slices on another disk with --scratch DIR. \
                 (--no-space-check skips this check, for a filesystem that holds more than it \
                 reports free, such as a compressed or thin-provisioned one.)",
                dir.display(),
                crate::util::format_bytes_binary(bytes),
                crate::util::format_bytes_binary(free),
            )));
        }
    }
    Ok(())
}

/// `path` itself if it exists, else its nearest existing ancestor — where
/// a directory the restore will create gets its space from. A relative path
/// is taken from the working directory, as every later step takes it, so a
/// bare `--to restored` that does not exist yet is measured on the working
/// directory's filesystem rather than not at all.
fn existing(path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_relative() {
        std::env::current_dir().ok()?.join(path)
    } else {
        path.to_path_buf()
    };
    absolute
        .ancestors()
        .find(|p| p.exists())
        .map(Path::to_path_buf)
}

#[cfg(test)]
thread_local! {
    /// Test-only: the free space [`restore_free_bytes`] reports on this
    /// thread, so the space refusal can be driven without filling a disk.
    static RESTORE_FREE_OVERRIDE: std::cell::Cell<Option<i64>> =
        const { std::cell::Cell::new(None) };
}

/// Test-only: pretend every filesystem a restore checks has `bytes` free, on
/// this test's thread, until dropped.
#[cfg(test)]
pub(crate) struct RestoreFreeOverride;
#[cfg(test)]
impl RestoreFreeOverride {
    pub(crate) fn set(bytes: i64) -> Self {
        RESTORE_FREE_OVERRIDE.with(|c| c.set(Some(bytes)));
        Self
    }
}
#[cfg(test)]
impl Drop for RestoreFreeOverride {
    fn drop(&mut self) {
        RESTORE_FREE_OVERRIDE.with(|c| c.set(None));
    }
}

/// Bytes an unprivileged process can still write under `dir` (`f_bavail` x
/// `f_frsize`, the same arithmetic as `stage create`'s space check), or
/// `None` when `statvfs` cannot say.
fn restore_free_bytes(dir: &Path) -> Option<i64> {
    #[cfg(test)]
    if let Some(free) = RESTORE_FREE_OVERRIDE.with(|c| c.get()) {
        return Some(free);
    }
    let stat = nix::sys::statvfs::statvfs(dir).ok()?;
    let free = (stat.blocks_available() as u64).saturating_mul(stat.fragment_size());
    Some(i64::try_from(free).unwrap_or(i64::MAX))
}

/// `restore raw-volume` over an already-open store — the heir/DR dump
/// (ADR-0005), with its contact recorded.
///
/// A thin wrapper around [`crate::volume::raw::restore_raw`], which stays
/// `Connection`-free on purpose: it is the function an heir runs with no
/// catalog at all, and giving it a database would be giving it the thing the
/// whole path exists to do without. The contact is bookkeeping ABOUT that
/// dump, not part of it, so it belongs here.
///
/// **`site`'s medium is what the CLI read off the cartridge's MAM** (issue
/// #316) — the same one-parameter shape as [`restore_unit_from_store`].
/// This used to be hard-coded `Medium::NotAttempted`, so every raw-volume
/// contact said "no MAM read is attempted on this path" (both since removed
/// as vocabulary with no writer, issue #318) while the CLI arm had in fact read the MAM in `check_read_contact`
/// (issue #166) and the journal recorded that read (issue #297) — the
/// contact denied a read its own journal rows proved. The CLI now takes the
/// same two reads every other read path takes (ADR-0013 §5):
/// `check_read_contact`, then [`crate::volume::binding::loaded_medium`],
/// whose [`MamInfo`](crate::tape::mam::MamInfo) is what the site's
/// `Medium` carries.
///
/// Taking the second read rather than justifying one: the first read hands
/// back only a verdict and a raw capture, not the parsed `MamInfo` a contact
/// records, so without the second there is no serial or load count to put
/// on the row. It does not compromise ADR-0005: the chip is part of the
/// CARTRIDGE, not the catalog, and nothing here corroborates or refuses on
/// it — `ContactGuard::open` only records. A DR machine with no backend gets
/// `Medium::NoBackend` (no read happened, `REASON_NO_BACKEND_CONFIGURED`);
/// one whose catalog never saw this cartridge gets
/// `REASON_SERIAL_UNREGISTERED`, which is the honest answer. `volume_id` is
/// NULL: this path runs against whatever tape is loaded and names none.
///
/// The site carries both MAM captures the CLI took before the store opened
/// (`ContactSite::with_mam_reads`, issue #297), journalled against this
/// contact when it opens.
pub fn restore_raw_volume(
    conn: &Connection,
    store: &mut dyn Store,
    dest: &Path,
    expect_label: Option<&str>,
    site: ContactSite<'_>,
) -> Result<crate::volume::raw::RawRestoreReport> {
    restore_raw_volume_selected(
        conn,
        store,
        dest,
        expect_label,
        &crate::volume::raw::RawSelection::all(),
        site,
    )
}

/// [`restore_raw_volume`] dumping only the files `selection` names
/// (`--positions`, `--only`; issue #417).
pub fn restore_raw_volume_selected(
    conn: &Connection,
    store: &mut dyn Store,
    dest: &Path,
    expect_label: Option<&str>,
    selection: &crate::volume::raw::RawSelection,
    site: ContactSite<'_>,
) -> Result<crate::volume::raw::RawRestoreReport> {
    let started_at = restore_record::now_sqlite();
    let phase = progress::phase("contact-open", None);
    let guard = site.open(conn, None);
    phase.done();
    let contact_id = guard.id();
    let phase = progress::phase("dump", None);
    let r = crate::volume::raw::restore_raw_selected(store, dest, expect_label, selection);
    if r.is_ok() {
        phase.done();
    } else {
        drop(phase);
    }
    // A dump whose checksums did not all verify is how this contact ENDED,
    // even though the function returns `Ok` — the CLI's exit status says the
    // same thing (`RawRestoreReport::all_verified`).
    let (outcome, error) = match &r {
        Ok(report) if !report.all_verified() => (
            contact::OUTCOME_FAILED,
            Some(format!(
                "{} of {} files mismatched",
                report.mismatched_count, report.files_dumped
            )),
        ),
        Ok(_) => (contact::OUTCOME_OK, None),
        Err(e) => (contact::OUTCOME_FAILED, Some(e.to_string())),
    };
    guard.finish(outcome, error.as_deref());
    // The restore's own record (issue #306), the same outcome as its
    // contact. `volume_label` is what the TAPE said, or what was asked for
    // when the dump failed before File 0 could say; `volume_id` is NULL,
    // as on the contact: this path names no catalog row (ADR-0005).
    let report = r.as_ref().ok();
    restore_record::record(
        conn,
        &RestoreRecord {
            contact_id,
            volume_id: None,
            volume_label: report.map(|rep| rep.label.as_str()).or(expect_label),
            unit_id: None,
            unit_name: None,
            version: None,
            kind: restore_record::KIND_RAW_VOLUME,
            file_path: None,
            destination: &dest.to_string_lossy(),
            started_at: &started_at,
            outcome,
            error: error.as_deref(),
            slices_read: None,
            bytes_restored: report.map(|rep| rep.bytes_written as i64),
            files_restored: report.map(|rep| rep.files_dumped as i64),
            tar: None,
            tar_version: None,
        },
    );
    // The post-command health reading (issue #320), on every outcome — a
    // dump that failed its checksums is exactly when the read-error
    // counters matter. The heir's dump itself stays `Connection`-free.
    let phase = progress::phase("health-sweep", None);
    crate::volume::write::health_after_read_contact(conn, &site, None, contact_id);
    phase.done();
    // Issue #386: the phases are in the session log; a raw dump names no
    // catalog volume to record them against, so they go no further.
    let _ = progress::drain();
    r
}

/// [`restore_unit`] minus the tape device — everything from the contact
/// corroboration through the tar extract.
///
/// Split at the store seam for the reason ADR-0006 gives generally and
/// [`crate::volume::write::volume_verify_with_store`] already demonstrates:
/// with a `&mut dyn Store` the contact discipline is exercisable against a
/// `MemStore` with no hardware, which is the only way to prove that restore
/// actually corroborates (issue #193: "a contact that skips corroboration is
/// the defect returning"). `restore_unit` keeps the drive-only parts.
///
/// Re-runs the unit/tenant/position lookups rather than taking them as
/// arguments: they are three indexed reads against an open connection, and a
/// function that cannot be called on its own is not a seam.
///
/// **Writes the `restores` row** (issue #306) once the contact has closed,
/// on every outcome — this seam is where the contact opens, so it is where
/// "after the contact opened" begins. Nothing before `site.open` records a
/// restore: a refusal upstream of the drive is not one.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn restore_unit_from_store(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    unit_name: &str,
    volume_label: &str,
    version: i64,
    target: RestoreTarget<'_>,
    options: &RestoreOptions,
    store: &mut dyn Store,
    site: ContactSite<'_>,
) -> Result<RestoreReport> {
    restore_unit_from_bay(
        conn,
        paths,
        config,
        unit_name,
        volume_label,
        version,
        target,
        options,
        &mut FixedBay(store),
        site,
    )
}

/// [`restore_unit_from_store`] over a [`Bay`] — the drive that can also be
/// asked for another cartridge when a slice is damaged (issue #480).
#[allow(clippy::too_many_arguments)]
pub(crate) fn restore_unit_from_bay(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    unit_name: &str,
    volume_label: &str,
    version: i64,
    target: RestoreTarget<'_>,
    options: &RestoreOptions,
    bay: &mut dyn Bay,
    site: ContactSite<'_>,
) -> Result<RestoreReport> {
    // The volume the restore names, looked up here only so the contact can
    // reference it. An absent row is not an absent contact: the tape in the
    // drive was still read (File 0, below), which is exactly the situation a
    // `volume_id` this command cannot resolve describes.
    let volume_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            rusqlite::params![volume_label],
            |r| r.get(0),
        )
        .optional()?;
    let started_at = restore_record::now_sqlite();
    let phase = progress::phase("contact-open", None);
    let guard = site.open(conn, volume_id);
    phase.done();
    restore_unit_in_contact(
        conn,
        paths,
        config,
        guard,
        volume_id,
        &started_at,
        unit_name,
        volume_label,
        version,
        target,
        options,
        bay,
        None,
        &site,
    )
}

/// [`restore_unit_from_store`] from the moment its contact is open: the
/// restore itself, the `restores` row, the one post-command reading and the
/// phase timings. Split out for issue #442 — the loaded-tape restore opens
/// the same contact before it can know the volume, and ends in this.
///
/// `primed` is File 0's head, when the loaded-tape path has already read it
/// to identify the volume: lent to the first contact check so the tape is
/// asked for File 0 once.
#[allow(clippy::too_many_arguments)]
fn restore_unit_in_contact(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    guard: contact::ContactGuard<'_>,
    volume_id: Option<i64>,
    started_at: &str,
    unit_name: &str,
    volume_label: &str,
    version: i64,
    target: RestoreTarget<'_>,
    options: &RestoreOptions,
    bay: &mut dyn Bay,
    primed: Option<Vec<u8>>,
    site: &ContactSite<'_>,
) -> Result<RestoreReport> {
    let contact_id = guard.id();
    let mut trace = RestoreTrace::default();
    let mut supply = Supply::start(conn, bay, guard, volume_id, volume_label, *site);
    supply.prime(primed);
    supply.set_subject(format!("unit \"{unit_name}\" v{version}"));
    let r = restore_unit_contacted(
        conn,
        paths,
        config,
        unit_name,
        volume_label,
        version,
        target,
        options,
        &mut supply,
        &mut trace,
    );
    // The last cartridge's contact closes with the outcome and takes its one
    // post-command health reading (issue #320): a restore that failed on a
    // read error is precisely the contact whose counters matter. A swap
    // closed the earlier cartridges' contacts the same way, each with its own
    // reading, so no contact has two and none has none.
    supply.finish(r.as_ref().err());
    record_restore(
        conn,
        RecordAt {
            contact_id,
            volume_id,
            volume_label,
            started_at,
        },
        unit_name,
        version,
        target,
        r.as_ref().err(),
        &trace,
    );
    // Issue #386: this restore's phases, on every outcome.
    if let Some(id) = volume_id {
        crate::db::phase_timings::record_drained(
            conn,
            &format!("restore {}", target.kind()),
            crate::db::phase_timings::Subject::Volume(id),
        );
    }
    r
}

/// [`restore_unit_from_store`] minus the contact bookkeeping. Fills
/// `trace` as it goes, so the seam can record what was measured whether or
/// not this returns `Ok`.
#[allow(clippy::too_many_arguments)]
fn restore_unit_contacted(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    unit_name: &str,
    volume_label: &str,
    version: i64,
    target: RestoreTarget<'_>,
    options: &RestoreOptions,
    supply: &mut Supply<'_, '_, '_>,
    trace: &mut RestoreTrace,
) -> Result<RestoreReport> {
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;
    let tenant = queries::get_tenant_by_id(conn, unit.tenant_id)?
        .ok_or_else(|| TapectlError::Other("tenant not found".into()))?;
    // ONE version's slices (issue #315). The version is resolved by the
    // caller — `restore_unit` before it opens the drive — and passed here
    // concretely, so the version counted and the version read are the same.
    let selection = select_write_positions(conn, unit_name, volume_label, Some(version))?;

    supply.corroborate_first(volume_label)?;

    // ONE plan, decided again exactly as `restore_through_drive`'s preflight
    // decided it before the drive opened: which members, and so which
    // slices, this restore reads.
    let scratch = scratch_dir(Path::new(target.destination()), options.scratch.as_deref());
    let plan = plan_restore(conn, &selection, target)?;

    // #478, item 266: a version read across cartridges needs a person to
    // change them. With nobody at the terminal the other cartridges are not
    // offered: the restore takes the files this tape holds whole and lists
    // the rest, which is on the other cartridges (R6, R7). It is said first.
    if !supply.attended() {
        let runs = span::runs(&plan.positions);
        if runs
            .iter()
            .any(|r| Some(r.volume_id) != supply.here_volume())
        {
            warn!(
                "{}",
                span::unattended_note(
                    conn,
                    &format!("unit \"{unit_name}\" version {version}"),
                    &runs,
                )
            );
        }
    }

    let identities = load_identities(conn, paths, &tenant, &options.keys)?;
    let substitutions_before = supply.substitutions.len();
    restore_planned(
        conn,
        config,
        unit_name,
        version,
        &plan,
        target,
        options,
        &scratch,
        &identities,
        supply,
        trace,
    )
    .map_err(|e| name_keys_given(e, &options.keys))?;

    info!(unit = unit_name, volume = volume_label, "restore complete");

    Ok(RestoreReport {
        unit_name: unit_name.to_string(),
        volume_label: volume_label.to_string(),
        version,
        slices: plan.positions.len(),
        destination: target.destination().to_string(),
        dry_run: false,
        success: true,
        substitutions: supply.substitutions[substitutions_before..].to_vec(),
        filled: trace.filled.clone(),
        id_map: id_map_of(conn, plan.stage_set_id),
        tar_said: trace.tar_said.clone(),
    })
}

/// Corroborate at contact (ADR-0012, issue #193), before a scratch
/// directory is made, before a key is loaded and before a single slice is
/// read. Restoring from the wrong tape used to surface as a per-slice
/// sha256 failure with no word about why. Reads File 0.
///
/// The same File 0 is also checked for whether this tapectl can read the
/// tape at all (ADR-0012 2026-10-06 item 15, #476): a `requires` naming a
/// feature it does not know, another magic or layout, or claims that do not
/// parse refuse with the text `volume::raw` and `volume::rebuild` give, ahead
/// of the identity check. It rides the one File 0 read the contact already
/// takes ([`crate::volume::binding::File0Facts::unreadable`]), so it costs the tape nothing; a File 0
/// that could not be read at all is an absence and proceeds, as before. This
/// is where every restore of slices (`restore unit`, `restore file`,
/// `restore volume`, with or without a label, streamed or `--spool`ed)
/// meets the tape; `restore raw` has its own check in `volume::raw`.
fn corroborate_at_contact(
    conn: &Connection,
    volume_label: &str,
    store: &mut dyn Store,
    medium_serial: Option<&str>,
) -> Result<()> {
    let volume_id: i64 = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            rusqlite::params![volume_label],
            |r| r.get(0),
        )
        .map_err(|_| TapectlError::VolumeNotFound(volume_label.to_string()))?;
    let phase = progress::phase("identify", None);
    let file0 = crate::volume::binding::read_file0_facts(store).readable()?;
    let medium = crate::volume::binding::MediumFacts::new(medium_serial.map(str::to_string), file0);
    crate::volume::binding::corroborate_volume(conn, volume_id, volume_label, &medium)?;
    phase.done();
    Ok(())
}

/// The contact a `restores` row hangs under (issue #306).
#[derive(Clone, Copy)]
struct RecordAt<'a> {
    contact_id: Option<i64>,
    volume_id: Option<i64>,
    volume_label: &'a str,
    started_at: &'a str,
}

/// The restore's own record: what came back, where to, how it ended, and
/// tar's report verbatim. Best-effort, like the contact row — the result is
/// decided and this cannot change it.
fn record_restore(
    conn: &Connection,
    at: RecordAt<'_>,
    unit_name: &str,
    version: i64,
    target: RestoreTarget<'_>,
    error: Option<&TapectlError>,
    trace: &RestoreTrace,
) {
    let (outcome, error) = match error {
        None => (contact::OUTCOME_OK, None),
        Some(e) => (contact::OUTCOME_FAILED, Some(e.to_string())),
    };
    let files_restored = match target {
        RestoreTarget::Unit { .. } => trace.files_restored,
        RestoreTarget::File { .. } => trace.placed.then_some(1),
    };
    let unit_id = queries::get_unit_by_name(conn, unit_name)
        .ok()
        .flatten()
        .map(|u| u.id);
    restore_record::record(
        conn,
        &RestoreRecord {
            contact_id: at.contact_id,
            volume_id: at.volume_id,
            volume_label: Some(at.volume_label),
            unit_id,
            unit_name: Some(unit_name),
            version: Some(version),
            kind: target.kind(),
            file_path: target.file_path(),
            destination: target.destination(),
            started_at: at.started_at,
            outcome,
            error: error.as_deref(),
            slices_read: Some(trace.slices_read),
            bytes_restored: Some(trace.bytes_decrypted),
            files_restored,
            tar: trace.tar.as_ref(),
            tar_version: trace.tar_version.as_deref(),
        },
    );
}

/// Every secret key the tenant owns, and the operator's, for trial
/// decryption — plus the identities given with `--key` (item 107), which
/// add to the home's and replace nothing. Ownership comes from the
/// catalog's key rows, so tenant `family` no longer also loads tenant
/// `family-old`'s keys; with no row (after `catalog rebuild`) a file goes to
/// every tenant it may belong to, so a tenant's own key is never withheld
/// (issue #350, keys::KeyOwners).
fn load_identities(
    conn: &Connection,
    paths: &TapectlPaths,
    tenant: &crate::db::models::Tenant,
    given: &[keys::KeyFile],
) -> Result<Vec<age::x25519::Identity>> {
    let mut identities = keys::load_tenant_identities(conn, &paths.keys_dir, &tenant.name)?;
    if !tenant.is_operator {
        if let Some(operator) = queries::get_operator_tenant(conn)? {
            identities.extend(keys::load_tenant_identities(
                conn,
                &paths.keys_dir,
                &operator.name,
            )?);
        }
    }
    identities.extend(keys::KeyFile::all_identities(given));
    if identities.is_empty() {
        return Err(TapectlError::Encryption(format!(
            "no secret keys found for tenant \"{}\"",
            tenant.name,
        )));
    }
    Ok(identities)
}

/// The refusal a restore gives when no identity it holds opens a slice —
/// the decrypt failure `chunks::open` reports for age's `NoMatchingKeys`,
/// whose own wording is the needle so a change of it cannot desynchronise.
fn is_no_matching_key(e: &TapectlError) -> bool {
    e.to_string()
        .contains(&age::DecryptError::NoMatchingKeys.to_string())
}

/// A restore that found no identity to open a slice, told which `--key`
/// files were given (item 107): the same refusal as without `--key`, with
/// the files the operator tried named after it. Any other error, and any
/// restore with no `--key`, passes through untouched.
fn name_keys_given(e: TapectlError, given: &[keys::KeyFile]) -> TapectlError {
    if given.is_empty() || !is_no_matching_key(&e) {
        return e;
    }
    TapectlError::Encryption(format!(
        "{e} (none of the identities in the --key file(s) given — {} — nor the home's own \
         keys opens this slice)",
        keys::KeyFile::describe(given),
    ))
}

/// Which slices one restore reads, and what it writes — decided from the
/// catalog before the drive is opened ([`plan_restore`]).
#[derive(Debug, Clone)]
pub(crate) struct RestorePlan {
    pub(crate) stage_set_id: i64,
    /// The slices to read, in slice order, each with the volume it is read
    /// from: those that hold the members asked for (`restore unit`: every
    /// member, so every slice that holds one; `restore file`: only the
    /// slices of the file, and of a hard link's target).
    pub(crate) positions: Vec<WritePositionInfo>,
    /// What the restore extracts: the members' length in the stream.
    pub(crate) bytes: i64,
    /// `restore file`: the size of the file asked for.
    pub(crate) file_size: Option<i64>,
}

/// What the catalog records of one staging that a read needs: its slice
/// size, how its slices are packed, and where each slice begins in the
/// stream.
pub(crate) struct StageFacts {
    pub(crate) slice_size: u64,
    pub(crate) compression: Option<Algorithm>,
    pub(crate) offsets: std::collections::BTreeMap<i64, u64>,
}

/// The facts of stage set `stage_set_id`, checked against its file table:
/// each slice's offset is the sum of the sizes of the slices before it
/// (invariant 13: the slices joined are the stream), and the table says the
/// same wherever it says anything. A catalog and a table that disagree are
/// refused here, by name, before any tape is touched.
pub(crate) fn stage_facts(
    conn: &Connection,
    stage_set_id: i64,
    rows: &[filetable::Row],
) -> Result<StageFacts> {
    let (slice_size, compression): (i64, Option<String>) = conn.query_row(
        "SELECT slice_size, compression FROM stage_sets WHERE id = ?1",
        params![stage_set_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let slice_size = u64::try_from(slice_size)
        .ok()
        .filter(|s| *s > 0)
        .ok_or_else(|| {
            TapectlError::Other(format!(
                "stage set {stage_set_id} records a slice size of {slice_size}: no read can be \
                 planned from it. Nothing was read from tape."
            ))
        })?;
    let compression = match compression.as_deref() {
        None => None,
        Some(name) => crate::staging::pack::packing(name, None)?.map(|p| p.algorithm),
    };
    let mut stmt = conn.prepare(
        "SELECT slice_number, size_bytes FROM stage_slices
          WHERE stage_set_id = ?1 ORDER BY slice_number",
    )?;
    let sizes = stmt
        .query_map(params![stage_set_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut offsets = std::collections::BTreeMap::new();
    let mut at = 0u64;
    for (n, size) in sizes {
        offsets.insert(n, at);
        at += u64::try_from(size).unwrap_or(0);
    }
    for (&n, &at) in &offsets {
        if let Some(start) = filetable::slice_start(rows, slice_size, n as u64) {
            if start != at {
                return Err(TapectlError::Other(format!(
                    "the catalog and the file table of stage set {stage_set_id} disagree about \
                     slice {n}: the slices before it hold {at} bytes of the stream, and the \
                     table begins its first member at {start}. Nothing was read from tape; \
                     `tapectl audit` names the rows."
                )));
            }
        }
    }
    Ok(StageFacts {
        slice_size,
        compression,
        offsets,
    })
}

/// The members a restore of `target` feeds, in stream order: the whole
/// unit, or the one file (and a hard link's target before it). A path the
/// version did not archive, or a folder, is refused by name.
pub(crate) fn members_of_target<'r>(
    rows: &'r [filetable::Row],
    unit_name: &str,
    version: i64,
    target: RestoreTarget<'_>,
) -> Result<Vec<&'r filetable::Row>> {
    match target {
        RestoreTarget::Unit { .. } => Ok(filetable::select(rows, &["."])),
        RestoreTarget::File { file_path, .. } => {
            let members = filetable::select(rows, &[file_path]);
            let asked = file_path.strip_prefix("./").unwrap_or(file_path);
            let asked = asked.trim_end_matches('/');
            match rows.iter().find(|r| r.path == asked) {
                None => Err(TapectlError::Other(format!(
                    "unit \"{unit_name}\" version {version} has no file \"{file_path}\" in its \
                     file table, so the tape was not touched. A path is relative to the unit's \
                     root and matched exactly; `tapectl catalog search \"<words of the name>\" \
                     --all-versions` finds one and names the versions that hold it (without \
                     `--all-versions` it searches each unit's newest version only), and \
                     `tapectl catalog list {unit_name}` lists the newest version's files."
                ))),
                Some(row) if row.kind == Kind::Folder => Err(TapectlError::Other(format!(
                    "\"{file_path}\" is a directory in unit \"{unit_name}\" version {version}; \
                     `restore file` restores one file. Restore the unit (`tapectl restore unit`) \
                     and take the directory from it. Nothing was read from tape."
                ))),
                Some(_) => Ok(members),
            }
        }
    }
}

/// Decide what a restore of `selection` reads (issue #411), with no tape:
/// the staging's file table gives the members, and the members give the
/// slices ([`filetable::slices_of`]). `restore file` reads only the slices of
/// its file; `restore unit` those that hold a member. A staging with no file
/// table cannot be read by tapectl: `catalog rebuild --from-volume` brings
/// the table back from a tape that carries it.
pub(crate) fn plan_restore(
    conn: &Connection,
    selection: &RestoreSelection,
    target: RestoreTarget<'_>,
) -> Result<RestorePlan> {
    let unit_name: String = conn.query_row(
        "SELECT u.name FROM stage_sets ss
           JOIN snapshots s ON s.id = ss.snapshot_id
           JOIN units u ON u.id = s.unit_id
          WHERE ss.id = ?1",
        params![selection.stage_set_id],
        |r| r.get(0),
    )?;
    let rows = stage_files::load(conn, selection.stage_set_id)?;
    if rows.is_empty() {
        return Err(TapectlError::Other(format!(
            "this catalog holds no file table for unit \"{unit_name}\" version {}, and a restore \
             is cut from the tape's slices by it: `tapectl catalog rebuild --from-volume` of a \
             tape that holds this version brings it back. Nothing was read from tape.",
            selection.version
        )));
    }
    let facts = stage_facts(conn, selection.stage_set_id, &rows)?;
    let members = members_of_target(&rows, &unit_name, selection.version, target)?;
    let needed = filetable::slices_of(members.iter().copied(), facts.slice_size);
    let positions: Vec<WritePositionInfo> = selection
        .positions
        .iter()
        .filter(|wp| u64::try_from(wp.slice_number).is_ok_and(|n| needed.contains(&n)))
        .cloned()
        .collect();
    let bytes: u64 = members.iter().map(|m| m.len()).sum();
    let file_size = match target {
        RestoreTarget::Unit { .. } => None,
        RestoreTarget::File { file_path, .. } => {
            let asked = file_path.strip_prefix("./").unwrap_or(file_path);
            let asked = asked.trim_end_matches('/');
            rows.iter()
                .find(|r| r.path == asked)
                .map(|r| i64::try_from(r.size).unwrap_or(i64::MAX))
        }
    };
    Ok(RestorePlan {
        stage_set_id: selection.stage_set_id,
        positions,
        bytes: i64::try_from(bytes).unwrap_or(i64::MAX),
        file_size,
    })
}

/// The id map of `stage_set_id`, to be shown with the restore (item 278): a
/// restore is by number, and this is what the numbers were called when the
/// stream was written.
fn id_map_of(conn: &Connection, stage_set_id: i64) -> Vec<IdRow> {
    stage_ids::load(conn, stage_set_id).unwrap_or_else(|e| {
        warn!(error = %e, "the staging's id map could not be read");
        Vec::new()
    })
}

/// `tar --version`'s first line, for the `restores` row.
fn tar_version_of(config: &Config) -> Option<String> {
    let out = crate::tar::command(&config.tar.binary)
        .arg("--version")
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(str::to_string)
}

/// Run `plan` against `supply`: read the slices, feed tar the members each
/// holds ([`reader`]), repair what is lost from another copy ([`fill`]),
/// and, for `restore file`, place the one entry.
#[allow(clippy::too_many_arguments)]
fn restore_planned(
    conn: &Connection,
    config: &Config,
    unit_name: &str,
    version: i64,
    plan: &RestorePlan,
    target: RestoreTarget<'_>,
    options: &RestoreOptions,
    scratch: &Path,
    identities: &[age::x25519::Identity],
    supply: &mut Supply<'_, '_, '_>,
    trace: &mut RestoreTrace,
) -> Result<()> {
    // `restore file` extracts into the scratch directory ([`scratch_dir`],
    // inside the destination or `--scratch`, never $TMPDIR, issue #406) and
    // places the one file. Named in the log first: if this process is
    // killed, that line says where the file may be left.
    let (extract, _scratch) = match target {
        RestoreTarget::Unit { dest_dir } => (PathBuf::from(dest_dir), None),
        RestoreTarget::File { .. } => {
            fs::create_dir_all(scratch).map_err(|e| {
                TapectlError::Other(format!(
                    "cannot create the restore scratch directory {}: {e}",
                    scratch.display()
                ))
            })?;
            let guard = RestoreScratch(scratch.to_path_buf());
            let extract = scratch.join("extract");
            info!(scratch = %scratch.display(), "the file is extracted here, then placed");
            (extract, Some(guard))
        }
    };
    let rows = stage_files::load(conn, plan.stage_set_id)?;
    let facts = stage_facts(conn, plan.stage_set_id, &rows)?;
    let members = members_of_target(&rows, unit_name, version, target)?;
    // The holes this restore records are the supply's from here on.
    let damage_before = supply.damage.len();
    let job = reader::Job {
        unit_name,
        members: &members,
        slice_size: facts.slice_size,
        compression: facts.compression,
        positions: &plan.positions,
        offsets: &facts.offsets,
        identities,
        tar_binary: &config.tar.binary,
        dest: &extract,
        overwrite: options.overwrite,
        file_check: !options.no_file_check,
    };
    let phase = progress::phase("extract", None);
    phase.item(unit_name.to_string());
    let outcome = reader::run(&job, supply, trace);
    phase.done();
    trace.tar_version = trace
        .tar
        .is_some()
        .then(|| tar_version_of(config))
        .flatten();
    let mut outcome = outcome?;
    trace.files_restored = Some(outcome.restored as i64);
    trace.tar_said = std::mem::take(&mut outcome.tar_said);
    if let RestoreTarget::File { file_path, .. } = target {
        // The one file asked for is the verdict; a hard link's target is
        // there to make it, and is not the operator's.
        let asked = file_path
            .strip_prefix("./")
            .unwrap_or(file_path)
            .trim_end_matches('/');
        outcome.lost.retain(|path, _| path == asked);
    }

    // What was lost, and — when a copy of the same version holds the rest —
    // the files filled from it (issue #480, ADR-0012 items 101, 102, 255).
    let holes = supply.damage[damage_before..].to_vec();
    let ex = fill::Extracted {
        root: extract.clone(),
        lost: outcome.lost,
        holes,
    };
    let ctx = fill::Finish {
        conn,
        config,
        identities,
        unit_name,
        version,
        stage_set_id: plan.stage_set_id,
        target,
        rows: &rows,
        options,
    };
    trace.filled = fill::finish(&ctx, supply, &ex)?;

    // `restore file`: place the one requested entry, inside the recorded
    // span (see `RestoreTarget`).
    if let RestoreTarget::File {
        file_path,
        dest_dir: file_dest,
    } = target
    {
        place_one_entry(&extract, file_path, Path::new(file_dest))?;
        trace.placed = true;
    }
    Ok(())
}

/// A writer that, once its inner writer fails, discards everything after
/// and claims it written — so [`read_slice`] keeps reading and hashing a
/// slice to its filemark after its decrypt has stopped.
struct Detachable<W> {
    inner: Option<W>,
}

impl<W: Write> Detachable<W> {
    fn new(inner: W) -> Self {
        Self { inner: Some(inner) }
    }
}

impl<W: Write> Write for Detachable<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(inner) = self.inner.as_mut() {
            if inner.write_all(buf).is_err() {
                self.inner = None;
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(inner) = self.inner.as_mut() {
            if inner.flush().is_err() {
                self.inner = None;
            }
        }
        Ok(())
    }
}

/// Restore a single file from a unit on a volume.
// Same too-many-args shape as `restore_unit` (which this wraps); interim
// allow.
#[allow(clippy::too_many_arguments)]
pub fn restore_file(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    unit_name: &str,
    file_path: &str,
    volume_label: &str,
    dest_dir: &str,
    device: &str,
    block_size: usize,
    version: Option<i64>,
    options: &RestoreOptions,
) -> Result<RestoreReport> {
    // tar's extract of the one entry into the restore's scratch directory, then
    // placed into `dest_dir`, inside the same recorded span
    // (`RestoreTarget::File`, issue #306) — through the one drive path
    // `restore unit` uses, so this is one contact and one reading, never two.
    // It used to extract the WHOLE unit into a `tempfile` directory in
    // $TMPDIR (issue #406).
    restore_through_drive(
        conn,
        paths,
        config,
        unit_name,
        volume_label,
        RestoreTarget::File {
            file_path,
            dest_dir,
        },
        options,
        device,
        block_size,
        version,
        false,
    )
}

/// One unit of a multi-unit restore (issue #398).
#[derive(Debug, Clone)]
pub struct UnitRequest {
    pub unit: String,
    /// The snapshot version; `None` is the newest on the volume.
    pub version: Option<i64>,
    /// Where this unit is restored — its own directory.
    pub dest_dir: String,
}

/// How one unit of a multi-unit restore ended (issue #398).
#[derive(Debug, Clone)]
pub struct UnitOutcome {
    pub unit_name: String,
    pub version: i64,
    pub slices: usize,
    pub destination: String,
    /// `None`: restored (or, in a dry run, would be). `Some`: why not.
    pub error: Option<String>,
    /// `false`: never started — an earlier unit failed under `fail_fast`,
    /// or this is a dry run.
    pub attempted: bool,
    /// Slices of this unit read from another copy (issue #480).
    pub substitutions: Vec<supply::Substitution>,
    /// Files of this unit filled from another copy of its version.
    pub filled: Vec<(String, String)>,
    /// The unit's id map (item 278), as [`RestoreReport::id_map`].
    pub id_map: Vec<IdRow>,
    /// What tar said, repeated.
    pub tar_said: Vec<String>,
}

/// What a multi-unit restore did, unit by unit, in tape order.
#[derive(Debug)]
pub struct UnitsReport {
    pub volume_label: String,
    pub dry_run: bool,
    pub units: Vec<UnitOutcome>,
}

impl UnitsReport {
    /// Units that failed or never started.
    pub fn failed(&self) -> usize {
        self.units
            .iter()
            .filter(|u| u.error.is_some() || (!self.dry_run && !u.attempted))
            .count()
    }
}

/// Restore several units from one volume in ONE pass over the tape (issue
/// #398) — the disaster-recovery shape, where restoring a whole volume unit
/// by unit paid a drive open, a rewind and a locate per unit.
///
/// Everything that can be refused without the tape is refused for the
/// WHOLE set before the drive is opened: each unit's version, plan and
/// [`preflight`] checks, a unit asked for twice, two units into one
/// directory, and the disk space of the set (per filesystem, the restored
/// units add up and the largest scratch counts once — the units restore one
/// after another, each removing its scratch). Then the drive is opened ONCE,
/// File 0 is corroborated ONCE, and the units are restored in the order
/// their slices lie on the tape, so the head only moves forward ([`TapeStore`]'s
/// cursor, #389). Each unit streams or spools as [`plan_restore`] decides.
///
/// One unit's failure does not stop the others unless `fail_fast`; every
/// unit that was attempted gets its own `restores` row (kind `unit`) under
/// the one contact, whose operation is `restore volume` — the CLI command
/// this is (ADR-0012 2026-10-06 item 23), not N `restore unit`s. The report lists every requested unit in tape order. `Err` is
/// for a refusal before the tape, a drive that will not open, or a tape
/// that is not the volume named.
#[allow(clippy::too_many_arguments)]
pub fn restore_units(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    volume_label: &str,
    requests: &[UnitRequest],
    options: &RestoreOptions,
    fail_fast: bool,
    device: &str,
    block_size: usize,
    dry_run: bool,
) -> Result<UnitsReport> {
    let planned = plan_units(conn, config, volume_label, requests, options)?;
    if dry_run {
        return Ok(UnitsReport {
            volume_label: volume_label.to_string(),
            dry_run: true,
            units: planned.iter().map(|p| p.outcome(None, false)).collect(),
        });
    }
    // The drive, as `restore_through_drive` opens it: both MAM reads, then
    // the store, once for the whole set.
    let mut bay = DeviceBay {
        device,
        block_size,
        operation: Operation::RestoreVolume,
        store: None,
    };
    let arrival = bay.load(conn, config)?;
    arrival.with_site(config, Operation::RestoreVolume, device, |site| {
        restore_units_from_bay(
            conn,
            paths,
            config,
            volume_label,
            &planned,
            options,
            fail_fast,
            &mut bay,
            *site,
        )
    })
}

/// The [`UnitRequest`]s for `names`, each restored to `<dest_root>/<name>`
/// — `restore volume`'s layout. The name becomes a path under `dest_root`,
/// so it is held to the unit-name rules (no `..`, no leading `/`) even for
/// a row a rebuilt catalog brought in from tape.
pub fn unit_requests(names: &[String], dest_root: &str) -> Result<Vec<UnitRequest>> {
    names
        .iter()
        .map(|name| {
            crate::naming::validate_unit_name(name)?;
            Ok(UnitRequest {
                unit: name.clone(),
                version: None,
                dest_dir: Path::new(dest_root)
                    .join(name)
                    .to_string_lossy()
                    .into_owned(),
            })
        })
        .collect()
}

/// [`restore_units`] for the tape that is loaded, named by no label (issue
/// #435). `unit_names` empty restores every unit held whole (#420) on the
/// volume File 0 names. Never a dry run, as [`restore_unit_from_loaded`].
#[allow(clippy::too_many_arguments)]
pub fn restore_units_from_loaded(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    unit_names: &[String],
    dest_root: &str,
    options: &RestoreOptions,
    fail_fast: bool,
    device: &str,
    block_size: usize,
) -> Result<UnitsReport> {
    with_loaded_drive(
        conn,
        config,
        device,
        block_size,
        Operation::RestoreVolume,
        |bay, site| {
            restore_units_from_loaded_bay(
                conn, paths, config, unit_names, dest_root, options, fail_fast, bay, site,
            )
        },
    )
}

/// [`restore_units_from_loaded`] minus the drive; the multi-unit twin of
/// [`restore_unit_from_loaded_store`], with the same order: File 0 once, the
/// volume found, then the whole set planned and checked
/// ([`plan_units`]), then the one pass over the tape. And the same contact
/// (issue #442): opened before File 0 is read, the volume attached once
/// resolved, closed `failed` by [`refuse_loaded_contact`] on any refusal
/// before the set is restored, and handed on to
/// [`restore_units_in_contact`] when there is none.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn restore_units_from_loaded_store(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    unit_names: &[String],
    dest_root: &str,
    options: &RestoreOptions,
    fail_fast: bool,
    store: &mut dyn Store,
    site: ContactSite<'_>,
) -> Result<UnitsReport> {
    restore_units_from_loaded_bay(
        conn,
        paths,
        config,
        unit_names,
        dest_root,
        options,
        fail_fast,
        &mut FixedBay(store),
        site,
    )
}

/// [`restore_units_from_loaded_store`] over a [`Bay`].
#[allow(clippy::too_many_arguments)]
fn restore_units_from_loaded_bay(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    unit_names: &[String],
    dest_root: &str,
    options: &RestoreOptions,
    fail_fast: bool,
    bay: &mut dyn Bay,
    site: ContactSite<'_>,
) -> Result<UnitsReport> {
    let phase = progress::phase("contact-open", None);
    let guard = site.open(conn, None);
    phase.done();
    let (found, primed) = match crate::volume::loaded::identify(conn, bay.store()) {
        Ok((found, peeked)) => (found, peeked.into_file0()),
        Err(e) => return refuse_loaded_contact(conn, guard, &site, None, "volume", e),
    };
    guard.record_volume(found.volume_id);
    crate::volume::loaded::announce("restoring from", &found);
    let planned = (|| {
        let names = if unit_names.is_empty() {
            units_on_volume(conn, &found.label)?
        } else {
            unit_names.to_vec()
        };
        let requests = unit_requests(&names, dest_root)?;
        plan_units(conn, config, &found.label, &requests, options)
    })();
    let planned = match planned {
        Ok(planned) => planned,
        Err(e) => {
            return refuse_loaded_contact(conn, guard, &site, Some(found.volume_id), "volume", e)
        }
    };
    restore_units_in_contact(
        conn,
        paths,
        config,
        guard,
        Some(found.volume_id),
        &found.label,
        &planned,
        options,
        fail_fast,
        bay,
        primed,
        &site,
    )
}

/// One unit of a set, planned before the tape ([`plan_units`]).
struct PlannedUnit {
    unit: String,
    version: i64,
    dest_dir: String,
    scratch: PathBuf,
    plan: RestorePlan,
}

impl PlannedUnit {
    fn target(&self) -> RestoreTarget<'_> {
        RestoreTarget::Unit {
            dest_dir: &self.dest_dir,
        }
    }

    fn first_position(&self) -> u32 {
        self.plan
            .positions
            .iter()
            .filter_map(|wp| wp.position.parse().ok())
            .min()
            .unwrap_or(u32::MAX)
    }

    fn outcome(&self, error: Option<String>, attempted: bool) -> UnitOutcome {
        UnitOutcome {
            unit_name: self.unit.clone(),
            version: self.version,
            slices: self.plan.positions.len(),
            destination: self.dest_dir.clone(),
            error,
            attempted,
            substitutions: Vec::new(),
            filled: Vec::new(),
            id_map: Vec::new(),
            tar_said: Vec::new(),
        }
    }
}

/// [`restore_units`]'s half before the tape: every unit planned and
/// checked, the set's space checked, the units sorted into tape order.
fn plan_units(
    conn: &Connection,
    _config: &Config,
    volume_label: &str,
    requests: &[UnitRequest],
    options: &RestoreOptions,
) -> Result<Vec<PlannedUnit>> {
    if requests.is_empty() {
        return Err(TapectlError::Other("no unit to restore".into()));
    }
    let mut seen_units = std::collections::HashSet::new();
    let mut seen_dests = std::collections::HashSet::new();
    for req in requests {
        if !seen_units.insert(req.unit.as_str()) {
            return Err(TapectlError::Other(format!(
                "unit \"{}\" is asked for more than once. Nothing was read from tape.",
                req.unit
            )));
        }
        if !seen_dests.insert(Path::new(&req.dest_dir)) {
            return Err(TapectlError::Other(format!(
                "two units would be restored into the same destination {}: each unit needs its \
                 own. Nothing was read from tape.",
                req.dest_dir
            )));
        }
    }
    // One unit inside another's destination (units `a` and `a/b` restored
    // to DIR/a and DIR/a/b): both are empty when checked, but whichever is
    // restored second would land in a directory the first has filled.
    for outer in requests {
        for inner in requests {
            let (o, i) = (Path::new(&outer.dest_dir), Path::new(&inner.dest_dir));
            if o != i && i.starts_with(o) {
                return Err(TapectlError::Other(format!(
                    "unit \"{}\" would be restored inside unit \"{}\"'s destination ({} is \
                     inside {}). Restore them separately, each to its own --to. Nothing was \
                     read from tape.",
                    inner.unit, outer.unit, inner.dest_dir, outer.dest_dir
                )));
            }
        }
    }

    let mut planned = Vec::with_capacity(requests.len());
    let mut needs = Vec::new();
    for req in requests {
        let unit = queries::get_unit_by_name(conn, &req.unit)?
            .ok_or_else(|| TapectlError::UnitNotFound(req.unit.clone()))?;
        queries::get_tenant_by_id(conn, unit.tenant_id)?
            .ok_or_else(|| TapectlError::Other("tenant not found".into()))?;
        // A version this tape holds only a range of is planned across the
        // cartridges as `restore unit` plans it (item 266, D17): with the
        // others offered it goes on across them, and without them it takes
        // the files this tape holds whole and lists the rest.
        let selection = select(conn, &req.unit, volume_label, req.version)?;
        let target = RestoreTarget::Unit {
            dest_dir: &req.dest_dir,
        };
        let scratch = scratch_dir(Path::new(&req.dest_dir), options.scratch.as_deref());
        let plan = plan_restore(conn, &selection, target)?;
        preflight_checks(conn, &req.unit, &selection, target, &scratch, options)?;
        if let Some(n) = space_needs(Path::new(&req.dest_dir), &scratch, plan.bytes, None) {
            needs.extend(n);
        }
        planned.push(PlannedUnit {
            unit: req.unit.clone(),
            version: selection.version,
            dest_dir: req.dest_dir.clone(),
            scratch,
            plan,
        });
    }
    if options.no_space_check {
        info!("disk space not checked (--no-space-check)");
    } else {
        require_space(
            &needs,
            "Restoring these units one after another keeps every restored unit; no slice is \
             written to disk.",
        )?;
    }
    planned.sort_by_key(PlannedUnit::first_position);
    Ok(planned)
}

/// [`restore_units`] minus the tape device: ONE contact, ONE corroboration
/// of File 0, then each planned unit in turn over the same bay, each with its
/// own `restores` row, then ONE health reading.
#[allow(clippy::too_many_arguments)]
fn restore_units_from_bay(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    volume_label: &str,
    planned: &[PlannedUnit],
    options: &RestoreOptions,
    fail_fast: bool,
    bay: &mut dyn Bay,
    site: ContactSite<'_>,
) -> Result<UnitsReport> {
    let volume_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            rusqlite::params![volume_label],
            |r| r.get(0),
        )
        .optional()?;
    let phase = progress::phase("contact-open", None);
    let guard = site.open(conn, volume_id);
    phase.done();
    restore_units_in_contact(
        conn,
        paths,
        config,
        guard,
        volume_id,
        volume_label,
        planned,
        options,
        fail_fast,
        bay,
        None,
        &site,
    )
}

/// [`restore_units_from_bay`] from the moment its contact is open (issue
/// #442): the one corroboration, each planned unit in turn, the contact's
/// close, the one reading and the phase timings.
#[allow(clippy::too_many_arguments)]
fn restore_units_in_contact(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    guard: contact::ContactGuard<'_>,
    volume_id: Option<i64>,
    volume_label: &str,
    planned: &[PlannedUnit],
    options: &RestoreOptions,
    fail_fast: bool,
    bay: &mut dyn Bay,
    primed: Option<Vec<u8>>,
    site: &ContactSite<'_>,
) -> Result<UnitsReport> {
    let contact_id = guard.id();

    let mut supply = Supply::start(conn, bay, guard, volume_id, volume_label, *site);
    supply.prime(primed);
    let corroborated = supply.corroborate_first(volume_label);
    let mut units = Vec::with_capacity(planned.len());
    let mut stopped = false;
    for p in planned {
        if stopped {
            units.push(p.outcome(None, false));
            continue;
        }
        let started_at = restore_record::now_sqlite();
        let mut trace = RestoreTrace::default();
        let substitutions_before = supply.substitutions.len();
        let r = match &corroborated {
            // The wrong tape: every unit is refused for the same reason.
            Err(e) => Err(TapectlError::Other(e.to_string())),
            Ok(()) => {
                supply.set_subject(format!("unit \"{}\" v{}", p.unit, p.version));
                restore_planned_unit(conn, paths, config, p, options, &mut supply, &mut trace)
            }
        };
        record_restore(
            conn,
            RecordAt {
                contact_id,
                volume_id,
                volume_label,
                started_at: &started_at,
            },
            &p.unit,
            p.version,
            p.target(),
            r.as_ref().err(),
            &trace,
        );
        match &r {
            Ok(()) => info!(unit = %p.unit, volume = volume_label, "unit restored"),
            Err(e) => warn!(unit = %p.unit, error = %e, "unit not restored"),
        }
        if r.is_err() && fail_fast {
            stopped = true;
        }
        let mut outcome = p.outcome(r.err().map(|e| e.to_string()), true);
        outcome.substitutions = supply.substitutions[substitutions_before..].to_vec();
        outcome.filled = trace.filled.clone();
        outcome.tar_said = trace.tar_said.clone();
        outcome.id_map = id_map_of(conn, p.plan.stage_set_id);
        units.push(outcome);
    }

    let failed = units
        .iter()
        .filter(|u| u.error.is_some() || !u.attempted)
        .count();
    let detail = (failed > 0)
        .then(|| TapectlError::Other(format!("{failed} of {} units not restored", units.len())));
    // The last cartridge's contact closes with the set's outcome and takes
    // its one health reading; a swap closed the earlier ones.
    supply.finish(detail.as_ref());
    if let Some(id) = volume_id {
        crate::db::phase_timings::record_drained(
            conn,
            "restore volume",
            crate::db::phase_timings::Subject::Volume(id),
        );
    }
    corroborated?;
    Ok(UnitsReport {
        volume_label: volume_label.to_string(),
        dry_run: false,
        units,
    })
}

/// One unit of a set, after the contact was corroborated.
fn restore_planned_unit(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    p: &PlannedUnit,
    options: &RestoreOptions,
    supply: &mut Supply<'_, '_, '_>,
    trace: &mut RestoreTrace,
) -> Result<()> {
    let unit = queries::get_unit_by_name(conn, &p.unit)?
        .ok_or_else(|| TapectlError::UnitNotFound(p.unit.clone()))?;
    let tenant = queries::get_tenant_by_id(conn, unit.tenant_id)?
        .ok_or_else(|| TapectlError::Other("tenant not found".into()))?;
    let identities = load_identities(conn, paths, &tenant, &options.keys)?;
    restore_planned(
        conn,
        config,
        &p.unit,
        p.version,
        &p.plan,
        p.target(),
        options,
        &p.scratch,
        &identities,
        supply,
        trace,
    )
    .map_err(|e| name_keys_given(e, &options.keys))
}

/// Move the one entry `file_path` out of the extract at `extracted` into
/// `dest_dir` — `restore file`'s placing step.
///
/// `symlink_metadata`, not `exists()`: a unit may legitimately contain a
/// symlink pointing outside itself, and `exists()` follows the link, so a
/// dangling one was reported as "not found in restored unit" when it had in
/// fact been restored correctly by tar.
///
/// A rename when the scratch directory and `dest_dir` share a filesystem —
/// the default, scratch inside `--to` — so a large file is never written
/// twice (issue #406's space check counts on it); a copy otherwise.
fn place_one_entry(extracted: &Path, file_path: &str, dest_dir: &Path) -> Result<()> {
    let source_file = extracted.join(crate::volume::index::on_disk(file_path));
    let meta = fs::symlink_metadata(&source_file).map_err(|_| {
        TapectlError::Other(format!("file \"{file_path}\" not found in restored unit"))
    })?;

    let dest = placed_path(dest_dir, file_path);
    fs::create_dir_all(dest_dir)?;
    if meta.is_dir() || fs::rename(&source_file, &dest).is_err() {
        place_restored_entry(&source_file, &meta, &dest)?;
    }

    info!(file = file_path, dest = %crate::volume::index::escape_os(dest.as_os_str()), "file restored");
    Ok(())
}

/// Put one restored entry at `dest`, preserving what it *is*.
///
/// Split out of `restore_file` so the symlink rule is reachable without a tape:
/// everything around it needs a full `restore_unit` first, which made this the
/// one restore behaviour no test could exercise.
///
/// `fs::copy` follows symlinks and writes the *target's* bytes, so
/// `restore file` turned a symlink into a plain file while `restore unit` —
/// which lets tar do the extraction — preserved it. Same archive, same entry,
/// two different results depending on which command the operator reached for.
fn place_restored_entry(source: &Path, meta: &fs::Metadata, dest: &Path) -> Result<()> {
    if meta.is_symlink() {
        let target = fs::read_link(source)?;
        // `symlink()` refuses an existing path, and `fs::copy` (the non-symlink
        // arm) silently overwrites — so remove first to keep both arms behaving
        // the same way rather than making symlinks the one case that errors.
        if fs::symlink_metadata(dest).is_ok() {
            fs::remove_file(dest)?;
        }
        std::os::unix::fs::symlink(&target, dest)?;
    } else {
        fs::copy(source, dest)?;
    }
    Ok(())
}

#[derive(Debug)]
pub struct RestoreReport {
    pub unit_name: String,
    pub volume_label: String,
    /// The snapshot version restored (or that would be) — the newest on the
    /// volume unless `--version` named one (issue #315).
    pub version: i64,
    pub slices: usize,
    pub destination: String,
    pub dry_run: bool,
    #[allow(dead_code)]
    pub success: bool,
    /// Slices read from another copy because the volume's own was damaged
    /// (ADR-0012 item 101, issue #480).
    pub substitutions: Vec<supply::Substitution>,
    /// Files that came back damaged or missing and were filled from another
    /// copy of the same version (item 102): the path, and where from.
    pub filled: Vec<(String, String)>,
    /// The staging's id map (item 278): what each user and group number in
    /// the unit was called by the host that wrote it. Shown, never acted on:
    /// a restore is by number and no owner is changed by name.
    pub id_map: Vec<IdRow>,
    /// What tar said, repeated (it is never the verdict, invariant 6).
    pub tar_said: Vec<String>,
}

/// One slice of the selected version, as restore reads it off the volume.
#[derive(Debug, Clone)]
pub struct WritePositionInfo {
    /// The stage set this slice belongs to — one stage set is one snapshot
    /// version's complete tar stream, cut into slices (issue #315).
    pub stage_set_id: i64,
    /// The volume `position` is a tape file of.
    pub volume_id: i64,
    pub slice_number: i64,
    /// `write_positions.position` — the tape file number, not the slice
    /// number.
    pub position: String,
    pub sha256_plain: String,
    pub sha256_encrypted: String,
    pub encrypted_bytes: i64,
    /// The slice's cut: its length before compression and encryption.
    pub size_bytes: i64,
}

/// Exactly ONE snapshot version of a unit on one volume, and its slices —
/// what `restore unit` / `restore file` read (issue #315).
#[derive(Debug, Clone)]
pub struct RestoreSelection {
    /// `snapshots.version` of the selected stage set.
    pub version: i64,
    pub stage_set_id: i64,
    /// Every version of this unit with written slices on this volume,
    /// ascending — what a refusal names, and what an operator can pass to
    /// `--version`.
    pub versions_on_volume: Vec<i64>,
    /// The selected stage set's slices, in slice order. For a version this
    /// volume holds whole, all on it, in tape order. For one it holds a
    /// range of (#478), every slice of the version, each with the volume it
    /// is read from ([`WritePositionInfo::volume_id`]): this volume's range
    /// first, then the other cartridges in slice order ([`span`]).
    pub positions: Vec<WritePositionInfo>,
}

/// Select the ONE snapshot version of `unit_name` to restore from
/// `volume_label`, and return its slices (issue #315).
///
/// A volume can carry the same unit in several snapshot versions — every
/// staged stage set rides the same write — and the query this replaced had
/// no version filter at all: it returned every version's slices, each was
/// decrypted into the one stream, and a later version's slice N
/// was mixed into an earlier one's. RESTORE.sh had already
/// been fixed for the same defect (`AWK_SELECT_VERSION`, issue #131); this
/// applies the SAME rule so the two restore paths cannot disagree:
///
/// - **`version: None`** — the highest `snapshots.version` of the unit that
///   has written slices on this volume (AWK: the `[[units]]` block with the
///   greatest `snapshot_version`).
/// - **`version: Some(n)`** — exactly version `n` (AWK: `want`). A version
///   not on the volume is REFUSED, naming the versions that are; AWK prints
///   nothing in that case and RESTORE.sh reports it.
/// - No snapshot-status filter, as AWK has none: a superseded or reclaimable
///   version that is physically on the tape stays restorable.
///
/// **Two stage sets of one version on one volume** is not reachable through
/// the CLI: `writes` is `UNIQUE(stage_set_id, volume_id)`, and `stage create
/// --version` refuses while any stage set of that version still has live
/// slices, so two sets of one version are never staged together into one
/// write session. It is resolved deterministically anyway, by the highest
/// `stage_sets.id` (the later staging): each stage set is a complete archive
/// of the same snapshot, so either is a correct restore, and mixing them is
/// the one wrong answer. AWK breaks the same tie by manifest order (`>=`
/// keeps the last block), which is equally arbitrary; any deterministic
/// choice of ONE set matches it in what matters.
///
/// **A version this volume holds a range of** (#478, `volume-format-v2.md`
/// §4.4) is named on the tape as a cut one is, and is then planned across
/// cartridges ([`span::plan_across`]): this volume must hold its slice 1,
/// every slice must be on a live volume, and the positions returned are
/// every slice of the version, from the fewest cartridges in slice order.
/// Either refusal comes before any cartridge is asked for. A version held
/// whole costs the statements it did in 1.6.0 (`catalog_budget`'s
/// `RestoreSelect`).
pub fn select_write_positions(
    conn: &Connection,
    unit_name: &str,
    volume_label: &str,
    want: Option<i64>,
) -> Result<RestoreSelection> {
    select(conn, unit_name, volume_label, want)
}

/// The volume `restore unit --dry-run` previews when no `--from` names one
/// (ADR-0012 item 220). The real restore reads the loaded tape's File 0 to
/// learn its volume; a dry run opens no drive, so it asks the catalog, and
/// the one in-service volume (`coverage::in_service`, where audit's
/// `read-slices --from` also looks) that holds the unit is the answer: one
/// holding a version whole, or slice 1 of a version held across cartridges
/// (where a restore of it starts). `version` narrows it to that version.
///
/// Several holders are refused, each named with its versions, for `--from`
/// to pick one; none is refused too. One statement after the unit's own
/// lookup, and it is not on any real restore's path.
pub fn sole_holder(conn: &Connection, unit_name: &str, version: Option<i64>) -> Result<String> {
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;
    let mut stmt = conn.prepare(&format!(
        "SELECT v.label, s.version FROM writes w
           JOIN stage_sets ss ON ss.id = w.stage_set_id
           JOIN snapshots s ON s.id = ss.snapshot_id
           JOIN volumes v ON v.id = w.volume_id
          WHERE s.unit_id = ?1 AND (?2 IS NULL OR s.version = ?2) AND {}
            AND w.status IN ('completed', 'range')
            AND EXISTS (SELECT 1 FROM write_positions wp
                          JOIN stage_slices sl ON sl.id = wp.stage_slice_id
                         WHERE wp.write_id = w.id AND wp.status = 'written'
                           AND (w.status = 'completed' OR sl.slice_number = 1))
          ORDER BY v.label, s.version",
        crate::policy::coverage::in_service("v")
    ))?;
    let rows = stmt
        .query_map(params![unit.id, version], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut holders: std::collections::BTreeMap<String, Vec<i64>> = Default::default();
    for (label, v) in rows {
        let versions = holders.entry(label).or_default();
        if !versions.contains(&v) {
            versions.push(v);
        }
    }
    let which = match version {
        Some(v) => format!("\"{unit_name}\" v{v}"),
        None => format!("\"{unit_name}\""),
    };
    match holders.len() {
        1 => Ok(holders.into_keys().next().expect("one holder")),
        0 => Err(TapectlError::Other(format!(
            "no volume in service holds {which}, so there is nothing to preview; `tapectl \
             catalog locate {unit_name}` lists every copy"
        ))),
        n => {
            let listed: Vec<String> = holders
                .iter()
                .map(|(label, versions)| {
                    let vs: Vec<String> = versions.iter().map(|v| format!("v{v}")).collect();
                    format!("{label} ({})", vs.join(", "))
                })
                .collect();
            Err(TapectlError::Other(format!(
                "{n} volumes hold {which}: {}. A dry run opens no drive, so it cannot read \
                 which one is loaded: name one with --from to preview the restore from it",
                listed.join(", ")
            )))
        }
    }
}

/// [`select_write_positions`].
fn select(
    conn: &Connection,
    unit_name: &str,
    volume_label: &str,
    want: Option<i64>,
) -> Result<RestoreSelection> {
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;

    // Every (version, stage set) of this unit on this volume — newest
    // version first, later stage set first within one — whose write is a
    // copy here (`completed`, with written slices), a range of a version
    // held across cartridges (#478, `range`, with written slices). A range is
    // picked as RESTORE.sh picks it (the newest on the tape, or `--version`)
    // and read across cartridges (`volume-format-v3.md` §5.3). A set the tape
    // was planned to take and did not (ADR-0012 item 288) has no row.
    let mut stmt = conn.prepare(
        "SELECT DISTINCT s.version, ss.id, w.status, v.ended_after
         FROM writes w
         JOIN stage_sets ss ON ss.id = w.stage_set_id
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN volumes v ON v.id = w.volume_id
         WHERE s.unit_id = ?1 AND v.label = ?2
           AND w.status IN ('completed', 'range')
           AND EXISTS (SELECT 1 FROM write_positions wp
                        WHERE wp.write_id = w.id AND wp.status = 'written')
         ORDER BY s.version DESC, ss.id DESC",
    )?;
    let candidates = stmt
        .query_map(params![unit.id, volume_label], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    if candidates.is_empty() {
        return Err(TapectlError::Other(format!(
            "no data for unit \"{unit_name}\" on volume \"{volume_label}\""
        )));
    }

    // The versions this volume holds whole: what `--version` may name.
    let mut versions_on_volume: Vec<i64> = candidates
        .iter()
        .filter(|(_, _, status, _)| status == "completed")
        .map(|(v, _, _, _)| *v)
        .collect();
    versions_on_volume.sort_unstable();
    versions_on_volume.dedup();

    // `candidates` is ordered newest-first, so the first match is the pick.
    let picked = match want {
        None => candidates.first(),
        Some(want) => candidates.iter().find(|(v, _, _, _)| *v == want),
    };
    let Some((version, stage_set_id, status, ended_after)) = picked else {
        let listed: Vec<String> = versions_on_volume.iter().map(i64::to_string).collect();
        let mut ranges: Vec<String> = candidates
            .iter()
            .filter(|(_, _, status, _)| status == "range")
            .map(|(v, _, _, _)| v.to_string())
            .collect();
        ranges.dedup();
        return Err(TapectlError::Other(format!(
            "unit \"{unit_name}\" has no version {} on volume \"{volume_label}\"; \
             version(s) on it: {}{} — pass one of those with --version",
            want.unwrap_or_default(),
            if listed.is_empty() {
                "none whole".to_string()
            } else {
                listed.join(", ")
            },
            if ranges.is_empty() {
                String::new()
            } else {
                format!(
                    "; a range of version(s) {}, held across cartridges",
                    ranges.join(", ")
                )
            },
        )));
    };
    let (version, stage_set_id, ended_after) = (*version, *stage_set_id, *ended_after);
    let positions = get_write_positions(conn, stage_set_id, volume_label)?;
    // R3 (`volume-format-v2.md` §4.2): nothing past where the tape ended is
    // read. A completed write holds every slice at or before it by
    // construction (confirm, rebuild and adoption all split the rows so, and
    // `audit` names a catalog that does not); refused here rather than read
    // past the seal if one ever does not.
    if let Some((k, wp)) = ended_after.and_then(|k| {
        positions
            .iter()
            .find(|wp| wp.position.parse::<i64>().map_or(true, |p| p > k))
            .map(|wp| (k, wp))
    }) {
        return Err(TapectlError::Other(format!(
            "unit \"{unit_name}\" version {version}: the catalog records its slice {} at tape \
             file {} of volume \"{volume_label}\", but that tape ended after file {k} (its \
             seal says so). The catalog disagrees with the tape; `tapectl audit` names the \
             rows. Nothing was read from tape.",
            wp.slice_number, wp.position
        )));
    }
    let positions = if status == "range" {
        span::plan_across(
            conn,
            unit_name,
            version,
            stage_set_id,
            volume_label,
            positions,
        )?
    } else {
        positions
    };
    Ok(RestoreSelection {
        version,
        stage_set_id,
        versions_on_volume,
        positions,
    })
}

/// Every unit with a live copy on `volume_label` — written slices of a
/// completed write or a range (item 266), the predicate
/// [`select_write_positions`] reads by, so
/// each unit named is one `restore unit --from <label>` would accept — in
/// the order its first slice lies on the tape. `restore volume`'s default
/// set (ADR-0012 2026-10-06 item 23, issue #398).
///
/// Not gated on the volume's status or condition: a quarantined or retired
/// tape can be the only copy left, and reading it is the operator's call,
/// exactly as with `restore unit`. Refused when the catalog has no such
/// volume or nothing on it, so an empty set never reaches the drive.
pub fn units_on_volume(conn: &Connection, volume_label: &str) -> Result<Vec<String>> {
    let known: Option<i64> = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            params![volume_label],
            |r| r.get(0),
        )
        .optional()?;
    if known.is_none() {
        return Err(TapectlError::Other(format!(
            "no volume \"{volume_label}\" in the catalog. Nothing was read from tape."
        )));
    }
    let mut stmt = conn.prepare(
        "SELECT u.name, MIN(CAST(wp.position AS INTEGER)) AS first
         FROM write_positions wp
         JOIN writes w ON w.id = wp.write_id
         JOIN stage_slices sl ON sl.id = wp.stage_slice_id
         JOIN stage_sets ss ON ss.id = sl.stage_set_id
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN units u ON u.id = s.unit_id
         JOIN volumes v ON v.id = w.volume_id
         WHERE v.label = ?1 AND w.status IN ('completed', 'range') AND wp.status = 'written'
         GROUP BY u.id
         ORDER BY first, u.name",
    )?;
    let names = stmt
        .query_map(params![volume_label], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if names.is_empty() {
        return Err(TapectlError::Other(format!(
            "volume \"{volume_label}\" holds no unit with written slices. Nothing was read \
             from tape."
        )));
    }
    Ok(names)
}

/// The written slices of ONE stage set on one volume, in tape order: of its
/// completed write, or of its range (#478; a volume holds at most one write
/// of a stage set, `UNIQUE(stage_set_id, volume_id)`).
///
/// Tape order is slice order on every tape tapectl writes (a unit's slices
/// are laid out contiguously, in `slice_number` order), so this is the order
/// restore always read them in. It is spelled as tape order anyway, with
/// `CAST` because `position` is TEXT, because that is what keeps restore
/// one forward pass over the tape (issue #389): `TapeStore` spaces forward
/// to a file ahead of the head and rewinds for one behind it.
fn get_write_positions(
    conn: &Connection,
    stage_set_id: i64,
    volume_label: &str,
) -> Result<Vec<WritePositionInfo>> {
    let mut stmt = conn.prepare(
        "SELECT sl.slice_number, wp.position, sl.sha256_plain, sl.sha256_encrypted, sl.encrypted_bytes,
                sl.stage_set_id, v.id, sl.size_bytes
         FROM write_positions wp
         JOIN writes w ON w.id = wp.write_id
         JOIN stage_slices sl ON sl.id = wp.stage_slice_id
         JOIN volumes v ON v.id = w.volume_id
         WHERE sl.stage_set_id = ?1 AND v.label = ?2 AND w.status IN ('completed', 'range')
           AND wp.status = 'written'
         ORDER BY CAST(wp.position AS INTEGER), sl.slice_number",
    )?;

    let rows = stmt
        .query_map(params![stage_set_id, volume_label], |row| {
            Ok(WritePositionInfo {
                slice_number: row.get(0)?,
                position: row.get(1)?,
                sha256_plain: row.get(2)?,
                sha256_encrypted: row.get(3)?,
                encrypted_bytes: row.get(4)?,
                stage_set_id: row.get(5)?,
                volume_id: row.get(6)?,
                size_bytes: row.get(7)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(rows)
}

#[cfg(test)]
mod tests {
    //! Tests for the H9 fix (issue #85): `read_slice` (once `restore_one_slice`) must behave
    //! equivalently to the old whole-buffer `tape.read_file()` +
    //! `read_to_end` pair it replaces in `restore_unit`'s slice loop, while
    //! never holding a whole encrypted slice or its decrypted plaintext in
    //! RAM. Mirrors the #35/#84 test suites' shape (`src/staging/mod.rs`,
    //! `src/staging/validate.rs`): round-trip, corruption detection,
    //! multi-chunk streaming, plus (specific to restore) trial-decryption
    //! order-independence. `MemStore::read_file` does a single whole-buffer
    //! `write_all`, so the restore-level tests alone never exercise a
    //! padding boundary that falls mid-write across several pushes the way
    //! a real tape read (`read_file_streaming`) does; `TruncatingWriter`'s
    //! own boundary-crossing tests (moved to `crate::util`, issue #86 —
    //! it's now shared with `store.rs` and `volume/write.rs` too) cover that
    //! directly.
    use super::*;
    use crate::store::{MemStore, TapeStore};
    use crate::tape::contact::Medium;
    use sha2::{Digest, Sha256};
    use std::io::Cursor;
    use tempfile::TempDir;

    /// The `ContactSite` a `MemStore` test has: no configured backend, the
    /// honest description of a machine with no drive at all (ADR-0005's DR
    /// shape). Nothing here opens the device path: with no backend the
    /// contact asks no drive who it is (issue #314).
    fn site(operation: Operation) -> ContactSite<'static> {
        static CFG: std::sync::OnceLock<Config> = std::sync::OnceLock::new();
        ContactSite::new(
            CFG.get_or_init(Config::default),
            operation,
            "/nonexistent/tapectl-contact-test-nst",
            Medium::NoBackend,
        )
    }

    /// `(operation, outcome)` of the one contact row, asserted BY VALUE.
    fn only_contact(conn: &Connection) -> (String, Option<String>) {
        conn.query_row(
            "SELECT operation, outcome FROM cartridge_contacts",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    fn direct_hash(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        format!("{:x}", h.finalize())
    }

    /// Encrypt `plaintext` to every key in `pubkeys` and return the raw
    /// ciphertext — a small buffered test-only helper (production code
    /// never buffers a whole ciphertext; see `read_slice`).
    fn encrypt_to(plaintext: &[u8], pubkeys: &[String]) -> Vec<u8> {
        crate::staging::encrypt_data(plaintext, pubkeys).unwrap()
    }

    /// Build a one-slice `MemStore` (position 0) plus the matching
    /// `WritePositionInfo` fixture for `plaintext` encrypted to `pubkeys`.
    /// `block_size` drives `MemStore`'s on-tape zero-padding, so even a
    /// small fixture exercises the true/padding trim, not just the
    /// no-padding-needed case.
    fn build_fixture(
        plaintext: &[u8],
        pubkeys: &[String],
        block_size: usize,
    ) -> (MemStore, WritePositionInfo) {
        let ciphertext = encrypt_to(plaintext, pubkeys);
        let sha256_encrypted = direct_hash(&ciphertext);
        let sha256_plain = direct_hash(plaintext);
        let encrypted_bytes = ciphertext.len() as i64;

        let mut store = MemStore::new(block_size);
        store
            .execute(&mut Cursor::new(ciphertext), encrypted_bytes as u64, false)
            .unwrap();

        let wp = WritePositionInfo {
            stage_set_id: 1,
            volume_id: 1,
            slice_number: 1,
            position: "0".to_string(),
            sha256_plain,
            sha256_encrypted,
            encrypted_bytes,
            size_bytes: plaintext.len() as i64,
        };

        (store, wp)
    }

    /// One slice read through a [`Supply`] over a single store: the cartridge
    /// in the drive is the only one there is, so nothing can be substituted
    /// (and this is the path every restore took before issue #480).
    fn read_slice(
        store: &mut dyn Store,
        wp: &WritePositionInfo,
        identities: &[age::x25519::Identity],
    ) -> Result<Vec<u8>> {
        let conn = crate::db::open_memory().unwrap();
        let site = site(Operation::RestoreUnit);
        let guard = site.open(&conn, None);
        let mut bay = FixedBay(store);
        let mut supply = Supply::start(&conn, &mut bay, guard, None, "TEST", site);
        let r = supply.read_slice(wp, identities, None).map(|read| {
            assert!(read.holes.is_empty(), "a clean read has no hole");
            assert!(read.unavailable.is_none());
            assert_eq!(read.authentic_bytes, read.bytes.len() as u64);
            read.bytes
        });
        supply.finish(r.as_ref().err());
        r
    }

    // --- RestoreScratch (issue #102) -------------------------------------

    /// The guard's whole point: the directory goes away when the scope does,
    /// with no explicit cleanup call anywhere.
    #[test]
    fn scratch_dir_is_removed_when_the_guard_drops() {
        let tmp = TempDir::new().unwrap();
        let scratch = tmp.path().join(".tapectl-restore-tmp");
        fs::create_dir_all(&scratch).unwrap();
        fs::write(scratch.join("restore.scratch"), b"decrypted archive bytes").unwrap();

        {
            let _guard = RestoreScratch(scratch.clone());
            assert!(scratch.exists());
        }

        assert!(
            !scratch.exists(),
            "the scratch directory outlived its guard, so decrypted slices \
             would be left in the operator's destination"
        );
    }

    /// The case that motivated the issue: an error propagating out of the
    /// scope with `?` must still clean up. A guard that only cleaned on the
    /// happy path would pass the test above and fail this one.
    #[test]
    fn scratch_dir_is_removed_when_the_scope_exits_via_an_error() {
        let tmp = TempDir::new().unwrap();
        let scratch = tmp.path().join(".tapectl-restore-tmp");

        fn fails_after_creating(scratch: &Path) -> Result<()> {
            fs::create_dir_all(scratch)?;
            let _guard = RestoreScratch(scratch.to_path_buf());
            fs::write(scratch.join("restore.scratch"), b"decrypted archive bytes")?;
            Err(TapectlError::Other("checksum mismatch".into()))
        }

        assert!(fails_after_creating(&scratch).is_err());
        assert!(
            !scratch.exists(),
            "a failure mid-restore left decrypted slices behind"
        );
    }

    /// Removal failure must not panic and must not mask the original error —
    /// `Drop` cannot report one, so it warns and moves on. Simulated by
    /// pointing the guard at a path that no longer exists, which is also the
    /// real double-cleanup case (`restore_file`'s outer `TempDir` can remove
    /// the tree first).
    #[test]
    fn a_missing_scratch_dir_is_not_an_error_on_drop() {
        let tmp = TempDir::new().unwrap();
        drop(RestoreScratch(tmp.path().join("never-created")));
    }

    /// **Wiring test.** The three above prove the guard; this proves
    /// `restore_unit` actually uses it. Without it, all three still pass
    /// while the scratch directory leaks — the exact "test the wiring, not
    /// just the pure function" trap this repo has hit before.
    ///
    /// Drives a REAL failure through `restore_unit`: the fixture has a unit,
    /// a volume and a write position (so the function gets past its early
    /// returns and creates the scratch dir), but the keys directory is empty,
    /// so identity loading fails immediately afterwards. No tape needed.
    #[test]
    fn restore_unit_leaves_no_scratch_dir_when_it_fails_partway() {
        let home = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        let paths = TapectlPaths::new(home.path().join(".tapectl"));
        paths.ensure_dirs().unwrap();

        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('alice', 0, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('u1', 'photos', ?1, 'mtime_size', 1, 'active')",
            [tid],
        )
        .unwrap();
        let uid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
             VALUES (?1, 1, 'full', 'current', '/tmp')",
            [uid],
        )
        .unwrap();
        let snap_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size)
             VALUES (?1, 'staged', 104857600)",
            [snap_id],
        )
        .unwrap();
        let ss_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes,
                                       encrypted_bytes, sha256_plain, sha256_encrypted)
             VALUES (?1, 1, 1000, 1100, 'abc123', 'def456')",
            [ss_id],
        )
        .unwrap();
        let slice_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES ('L6-0001', 'lto', 'primary', 'LTO-6', 2500000000000, 'sealed')",
            [],
        )
        .unwrap();
        let vol_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'completed')",
            params![ss_id, snap_id, vol_id],
        )
        .unwrap();
        let write_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO write_positions (write_id, stage_slice_id, position, status)
             VALUES (?1, ?2, '8', 'written')",
            params![write_id, slice_id],
        )
        .unwrap();

        let dest_str = dest.path().to_string_lossy().to_string();
        let result = restore_unit(
            &conn,
            &paths,
            &Config::default(),
            "photos",
            "L6-0001",
            &dest_str,
            "/dev/null",
            524288,
            None,
            false,
            &RestoreOptions::default(),
        );

        assert!(
            result.is_err(),
            "fixture is supposed to fail (no keys) — if this ever succeeds the \
             test is no longer exercising the failure path"
        );
        let scratch = dest.path().join(".tapectl-restore-tmp");
        assert!(
            !scratch.exists(),
            "restore_unit failed and left {} behind — that directory holds \
             DECRYPTED archive slices in the operator's destination",
            scratch.display()
        );
    }

    // --- read_slice (the 4 required scenarios) --------------------

    #[test]
    fn round_trip_reproduces_the_exact_original_plaintext() {
        let kp = crate::crypto::keys::generate_keypair();
        let pubkeys = vec![kp.public_key.clone()];
        let identity: age::x25519::Identity = kp.secret_key.parse().unwrap();

        let plaintext = b"restore round-trip content, repeated a bit. ".repeat(50);
        let (mut store, wp) = build_fixture(&plaintext, &pubkeys, 4096);

        let restored = read_slice(&mut store, &wp, &[identity]).unwrap();

        assert_eq!(restored.len(), plaintext.len());
        assert_eq!(restored, plaintext);
    }

    #[test]
    fn corruption_is_detected_and_names_the_slice() {
        let kp = crate::crypto::keys::generate_keypair();
        let pubkeys = vec![kp.public_key.clone()];
        let identity: age::x25519::Identity = kp.secret_key.parse().unwrap();

        let plaintext = b"content that will be corrupted on tape".to_vec();
        let (mut store, wp) = build_fixture(&plaintext, &pubkeys, 4096);

        // Flip a byte well within the true (unpadded) ciphertext region —
        // same style as `store::tests::confirm_detects_content_hash_mismatch_
        // only_at_integrity_tier`'s `store.files[4][100] ^= 0xFF`.
        store.files[0][5] ^= 0xFF;

        let err = read_slice(&mut store, &wp, &[identity]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("checksum mismatch"), "got: {msg}");
        assert!(
            msg.contains(&wp.slice_number.to_string()),
            "error must name the slice, got: {msg}"
        );
    }

    #[test]
    fn trial_decryption_succeeds_when_the_correct_identity_is_not_first() {
        let wrong_kp = crate::crypto::keys::generate_keypair();
        let right_kp = crate::crypto::keys::generate_keypair();
        let wrong_identity: age::x25519::Identity = wrong_kp.secret_key.parse().unwrap();
        let right_identity: age::x25519::Identity = right_kp.secret_key.parse().unwrap();

        // Encrypted only to the "right" key — "wrong" cannot decrypt it.
        let pubkeys = vec![right_kp.public_key.clone()];
        let plaintext = b"only the right key can open this".to_vec();
        let (mut store, wp) = build_fixture(&plaintext, &pubkeys, 4096);

        // The right identity is SECOND in the list, proving the single
        // `decrypt()` call tries every identity (age's `obtain_payload_key`
        // does `find_map` over the header internally) rather than only ever
        // succeeding when the match happens to come first.
        let identities = vec![wrong_identity, right_identity];
        let restored = read_slice(&mut store, &wp, &identities).unwrap();

        assert_eq!(restored, plaintext);
    }

    #[test]
    fn multi_chunk_slice_restores_correctly() {
        let kp = crate::crypto::keys::generate_keypair();
        let pubkeys = vec![kp.public_key.clone()];
        let identity: age::x25519::Identity = kp.secret_key.parse().unwrap();

        // Several times RESTORE_STREAM_BUFFER (128 KiB) and age's own 64 KiB
        // STREAM chunk, with a small block_size so the ciphertext also
        // spans many MemStore-recorded on-tape blocks — exercises real
        // multi-chunk streaming on both the tape-read/trim side and the
        // decrypt/copy side, without staging anything close to a real
        // multi-GB slice in a unit test.
        let mut plaintext = Vec::new();
        for i in 0..20_000u32 {
            plaintext
                .extend_from_slice(format!("line {i} of multi-chunk restore content\n").as_bytes());
        }
        assert!(
            plaintext.len() > 512 * 1024,
            "fixture must exceed several buffers to be meaningful, got {} bytes",
            plaintext.len()
        );

        let (mut store, wp) = build_fixture(&plaintext, &pubkeys, 4096);

        let restored = read_slice(&mut store, &wp, &[identity]).unwrap();
        assert_eq!(restored, plaintext);
    }

    #[test]
    fn the_decrypt_buffer_is_one_age_chunk_independent_of_input_length() {
        // Structural guarantee behind the constant-memory claim: the decrypt
        // path holds one age chunk (64 KiB plus its tag) and the chunk it
        // returns, never the slice. Pinning the value means any future drift
        // toward whole-slice buffering is a deliberate, visible edit.
        assert_eq!(crate::crypto::chunks::CHUNK, 64 * 1024);
        assert_eq!(crate::crypto::chunks::ENCRYPTED_CHUNK, 64 * 1024 + 16);
    }

    /// A symlink in a unit must come back as a symlink from `restore file`,
    /// the way it already does from `restore unit`. `fs::copy` follows the
    /// link and writes the target's bytes, so the two commands disagreed
    /// about the same archive entry.
    #[test]
    fn a_symlink_is_restored_as_a_symlink_not_as_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.txt");
        std::fs::write(&target, b"payload").unwrap();
        let link = dir.path().join("link-ok");
        std::os::unix::fs::symlink("real.txt", &link).unwrap();

        let dest = dir.path().join("out").join("link-ok");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        let meta = std::fs::symlink_metadata(&link).unwrap();
        place_restored_entry(&link, &meta, &dest).unwrap();

        assert!(
            std::fs::symlink_metadata(&dest).unwrap().is_symlink(),
            "restored entry must still be a symlink"
        );
        assert_eq!(std::fs::read_link(&dest).unwrap(), Path::new("real.txt"));
    }

    /// A symlink pointing outside the unit is legitimate and restores as a
    /// dangling link. The old `exists()` check followed it and reported the
    /// entry missing, which is the same dereferencing bug wearing a different
    /// hat: tar had restored it correctly and the command denied it was there.
    #[test]
    fn a_dangling_symlink_is_preserved_rather_than_called_missing() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("points-away");
        std::os::unix::fs::symlink("/nowhere/at/all", &link).unwrap();

        assert!(
            !link.exists(),
            "precondition: exists() denies a dangling link"
        );
        let meta = std::fs::symlink_metadata(&link).expect("symlink_metadata still sees it");

        let dest = dir.path().join("out").join("points-away");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        place_restored_entry(&link, &meta, &dest).unwrap();

        assert_eq!(
            std::fs::read_link(&dest).unwrap(),
            Path::new("/nowhere/at/all")
        );
    }

    /// A plain file still overwrites, and a symlink now overwrites too. Before
    /// the split these differed: `symlink()` refuses an existing path while
    /// `fs::copy` replaces one.
    #[test]
    fn both_arms_overwrite_an_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("f.txt");
        std::fs::write(&src, b"new").unwrap();
        let dest = dir.path().join("dest");
        std::fs::write(&dest, b"old").unwrap();

        let meta = std::fs::symlink_metadata(&src).unwrap();
        place_restored_entry(&src, &meta, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");

        let link = dir.path().join("l");
        std::os::unix::fs::symlink("f.txt", &link).unwrap();
        let lmeta = std::fs::symlink_metadata(&link).unwrap();
        place_restored_entry(&link, &lmeta, &dest).unwrap();
        assert!(std::fs::symlink_metadata(&dest).unwrap().is_symlink());
    }

    /// Issue #315: one volume can carry the SAME unit in several snapshot
    /// versions — every staged stage set rides the same write. Restore must
    /// select exactly ONE version's slices, the newest by default (the rule
    /// RESTORE.sh's `AWK_SELECT_VERSION` applies), never the union.
    mod version_selection {
        use super::*;

        pub(super) struct TwoVersions {
            pub conn: Connection,
            /// `stage_sets.id` of v1 (three slices) and v2 (one slice).
            pub v1_set: i64,
            pub v2_set: i64,
        }

        /// The seed-8 shape in miniature: `big` v1 with THREE slices and v2
        /// with ONE, both completed writes on `VOL-PM4`, positions 8..10 and
        /// 11. Different slice counts on purpose — a mix of the two is then
        /// visible in the count as well as in the stage-set ids.
        pub(super) fn two_versions_on_one_volume() -> TwoVersions {
            let conn = crate::db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('t1', 0, 'active')",
                [],
            )
            .unwrap();
            let tenant_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                 VALUES ('big', 'big', ?1, 'mtime_size', 1, 'active')",
                params![tenant_id],
            )
            .unwrap();
            let unit_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
                 VALUES ('VOL-PM4', 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
                [],
            )
            .unwrap();
            let volume_id = conn.last_insert_rowid();

            let mut next_position = 8;
            let mut add_version = |version: i64, slices: i64| -> i64 {
                conn.execute(
                    "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
                     VALUES (?1, ?2, 'staged', '/tmp', 1, 16)",
                    params![unit_id, version],
                )
                .unwrap();
                let snap_id = conn.last_insert_rowid();
                conn.execute(
                    "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
                    params![snap_id],
                )
                .unwrap();
                let ss_id = conn.last_insert_rowid();
                conn.execute(
                    "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                     VALUES (?1, ?2, ?3, 'completed')",
                    params![ss_id, snap_id, volume_id],
                )
                .unwrap();
                let write_id = conn.last_insert_rowid();
                for n in 1..=slices {
                    conn.execute(
                        "INSERT INTO stage_slices
                            (stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted)
                         VALUES (?1, ?2, 16, 16, 'aa', 'bb')",
                        params![ss_id, n],
                    )
                    .unwrap();
                    let slice_id = conn.last_insert_rowid();
                    conn.execute(
                        "INSERT INTO write_positions (write_id, stage_slice_id, position, status, sha256_on_volume)
                         VALUES (?1, ?2, ?3, 'written', 'bb')",
                        params![write_id, slice_id, next_position.to_string()],
                    )
                    .unwrap();
                    next_position += 1;
                }
                ss_id
            };
            let v1_set = add_version(1, 3);
            let v2_set = add_version(2, 1);
            TwoVersions {
                conn,
                v1_set,
                v2_set,
            }
        }

        /// The defect itself: with no version named, the positions returned
        /// are exactly v2's one slice — not v1's three plus v2's one, which
        /// a slice-number-keyed scratch name collapsed into v2's slice 1
        /// followed by v1's slices 2..3.
        #[test]
        fn the_default_selects_only_the_newest_versions_slices() {
            let f = two_versions_on_one_volume();
            // Positive control: the fixture really does carry both versions'
            // positions on the one volume — four written positions in all.
            let all: i64 = f
                .conn
                .query_row("SELECT COUNT(*) FROM write_positions", [], |r| r.get(0))
                .unwrap();
            assert_eq!(all, 4);
            assert_ne!(f.v1_set, f.v2_set);

            let sel = select_write_positions(&f.conn, "big", "VOL-PM4", None).unwrap();
            assert_eq!(sel.version, 2);
            assert_eq!(sel.stage_set_id, f.v2_set);
            assert_eq!(sel.versions_on_volume, vec![1, 2]);
            let sets: Vec<i64> = sel.positions.iter().map(|p| p.stage_set_id).collect();
            assert_eq!(sets, vec![f.v2_set], "only v2's one slice may be read");
            assert_eq!(sel.positions[0].position, "11");
        }

        /// `--version 1` selects exactly v1's three slices, in slice order,
        /// at v1's positions — and none of v2's.
        #[test]
        fn an_explicit_version_selects_exactly_that_versions_slices() {
            let f = two_versions_on_one_volume();
            let sel = select_write_positions(&f.conn, "big", "VOL-PM4", Some(1)).unwrap();
            assert_eq!(sel.version, 1);
            assert_eq!(sel.stage_set_id, f.v1_set);
            let rows: Vec<(i64, i64, &str)> = sel
                .positions
                .iter()
                .map(|p| (p.stage_set_id, p.slice_number, p.position.as_str()))
                .collect();
            assert_eq!(
                rows,
                vec![(f.v1_set, 1, "8"), (f.v1_set, 2, "9"), (f.v1_set, 3, "10")]
            );
        }

        /// A version the volume does not carry is refused, and the refusal
        /// names the versions it DOES carry.
        #[test]
        fn a_version_not_on_the_volume_is_refused_naming_the_ones_that_are() {
            let f = two_versions_on_one_volume();
            let err = select_write_positions(&f.conn, "big", "VOL-PM4", Some(9))
                .unwrap_err()
                .to_string();
            assert!(err.contains("no version 9"), "{err}");
            assert!(err.contains("VOL-PM4"), "{err}");
            assert!(err.contains("1, 2"), "{err}");
        }

        /// A unit with nothing on the volume keeps its old message.
        #[test]
        fn a_unit_with_nothing_on_the_volume_says_so() {
            let f = two_versions_on_one_volume();
            let err = select_write_positions(&f.conn, "big", "ELSEWHERE", None)
                .unwrap_err()
                .to_string();
            assert!(err.contains("no data for unit \"big\""), "{err}");
        }

        /// The unreachable-through-the-CLI tie (two stage sets of ONE version
        /// on one volume) still selects ONE set — the later one — never both.
        #[test]
        fn two_stage_sets_of_one_version_select_the_later_set_only() {
            let f = two_versions_on_one_volume();
            // Re-point v1's stage set at v2's snapshot: now both sets are v2.
            let v2_snap: i64 = f
                .conn
                .query_row(
                    "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
                    params![f.v2_set],
                    |r| r.get(0),
                )
                .unwrap();
            f.conn
                .execute(
                    "UPDATE stage_sets SET snapshot_id = ?1 WHERE id = ?2",
                    params![v2_snap, f.v1_set],
                )
                .unwrap();
            let sel = select_write_positions(&f.conn, "big", "VOL-PM4", None).unwrap();
            assert_eq!(sel.version, 2);
            assert!(f.v2_set > f.v1_set);
            assert_eq!(sel.stage_set_id, f.v2_set);
            assert!(sel.positions.iter().all(|p| p.stage_set_id == f.v2_set));
            assert_eq!(sel.positions.len(), 1);
        }

        /// ADR-0012 item 220: a dry run with no `--from` previews from the
        /// catalog's only holder of the unit, and with several lists them,
        /// each with its versions; `--version` narrows the holders to those
        /// of that version.
        #[test]
        fn a_dry_run_with_no_volume_named_uses_the_only_holder() {
            let f = two_versions_on_one_volume();
            assert_eq!(sole_holder(&f.conn, "big", None).unwrap(), "VOL-PM4");
            assert_eq!(sole_holder(&f.conn, "big", Some(1)).unwrap(), "VOL-PM4");
            let err = sole_holder(&f.conn, "big", Some(3))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("no volume in service holds \"big\" v3"),
                "{err}"
            );
            let err = sole_holder(&f.conn, "nope", None).unwrap_err();
            assert!(matches!(err, TapectlError::UnitNotFound(_)), "{err}");

            // A second volume holding v1: two holders without --version,
            // still one for v2.
            f.conn
                .execute(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                          capacity_bytes, status)
                     VALUES ('VOL-PM5', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
                    [],
                )
                .unwrap();
            let other = f.conn.last_insert_rowid();
            let snap: i64 = f
                .conn
                .query_row(
                    "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
                    params![f.v1_set],
                    |r| r.get(0),
                )
                .unwrap();
            f.conn
                .execute(
                    "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                     VALUES (?1, ?2, ?3, 'completed')",
                    params![f.v1_set, snap, other],
                )
                .unwrap();
            let write = f.conn.last_insert_rowid();
            f.conn
                .execute(
                    "INSERT INTO write_positions (write_id, stage_slice_id, position, status)
                     SELECT ?1, id, 7 + slice_number, 'written' FROM stage_slices
                      WHERE stage_set_id = ?2",
                    params![write, f.v1_set],
                )
                .unwrap();
            let err = sole_holder(&f.conn, "big", None).unwrap_err().to_string();
            assert!(
                err.contains("2 volumes hold \"big\": VOL-PM4 (v1, v2), VOL-PM5 (v1)"),
                "{err}"
            );
            assert!(err.contains("--from"), "{err}");
            assert_eq!(sole_holder(&f.conn, "big", Some(2)).unwrap(), "VOL-PM4");

            // A retired holder is not one.
            f.conn
                .execute(
                    "UPDATE volumes SET status = 'retired' WHERE id = ?1",
                    params![other],
                )
                .unwrap();
            assert_eq!(sole_holder(&f.conn, "big", None).unwrap(), "VOL-PM4");
        }
    }

    /// Restore is a contact (ADR-0012, issue #193) and corroborates before it
    /// makes a scratch directory, loads a key or reads a slice. Proves only
    /// that the CALL happens — the rule's own branches are drilled in
    /// `volume::binding`.
    mod contact {
        use super::*;
        use crate::store::Store;
        use crate::volume::layout;

        /// Enough catalog for `restore_unit_from_store` to reach the contact
        /// check: one tenant, unit, snapshot, stage set, slice, volume,
        /// completed write and written position.
        fn seed(conn: &Connection, volume_label: &str, unit_name: &str) {
            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('t1', 0, 'active')",
                [],
            )
            .unwrap();
            let tenant_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                 VALUES (?1, ?1, ?2, 'mtime_size', 1, 'active')",
                params![unit_name, tenant_id],
            )
            .unwrap();
            let unit_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
                 VALUES (?1, 1, 'staged', '/tmp', 1, 16)",
                params![unit_id],
            )
            .unwrap();
            let snap_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
                params![snap_id],
            )
            .unwrap();
            let ss_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO stage_slices
                    (stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted)
                 VALUES (?1, 1, 16, 16, 'aa', 'bb')",
                params![ss_id],
            )
            .unwrap();
            let slice_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
                 VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
                params![volume_label],
            )
            .unwrap();
            let volume_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, 'completed')",
                params![ss_id, snap_id, volume_id],
            )
            .unwrap();
            let write_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO write_positions (write_id, stage_slice_id, position, status, sha256_on_volume)
                 VALUES (?1, ?2, '4', 'written', 'bb')",
                params![write_id, slice_id],
            )
            .unwrap();
        }

        /// A MemStore whose File 0 is a real v2 ID thunk naming `label`.
        /// Nothing beyond File 0 is needed: the contact refusal must fire
        /// before anything else is read.
        fn tape_labelled(label: &str) -> MemStore {
            let thunk = layout::generate_id_thunk_v2(&layout::IdThunkV2Params {
                label,
                uuid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                media_type: "LTO-6",
                tapectl_version: "0.0.0-test",
                nominal_capacity: 2_500_000_000_000,
                mam_capacity: 2_400_000_000_000,
                total_files: 6,
                mam_manufacturer: "TESTCO",
                mam_serial: "",
                mam_length: 846,
                mam_loads: 1,
                created_at: "2026-09-16T00:00:00Z",
                cartridge_identity_source: None,
            })
            .into_bytes();
            let mut store = MemStore::new(4096);
            store
                .execute(&mut Cursor::new(thunk.clone()), thunk.len() as u64, false)
                .unwrap();
            store
        }

        #[test]
        fn restore_refuses_a_tape_whose_file0_names_another_volume() {
            let conn = crate::db::open_memory().unwrap();
            seed(&conn, "RESTORE-WANT", "r-unit");
            let mut store = tape_labelled("RESTORE-LOADED");
            let dest = TempDir::new().unwrap();
            let paths = TapectlPaths::new(dest.path().to_path_buf());

            let err = restore_unit_from_store(
                &conn,
                &paths,
                &Config::default(),
                "r-unit",
                "RESTORE-WANT",
                1,
                RestoreTarget::Unit {
                    dest_dir: &dest.path().to_string_lossy(),
                },
                &RestoreOptions::default(),
                &mut store,
                site(Operation::RestoreUnit),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("wrong tape"), "{err}");
            assert!(err.contains("RESTORE-WANT"), "{err}");
            assert!(err.contains("RESTORE-LOADED"), "{err}");
            assert!(
                !dest.path().join(".tapectl-restore-tmp").exists(),
                "a refused contact must not have made a scratch directory"
            );
        }

        /// The DR shape: the same restore with the RIGHT tape gets past the
        /// contact check and fails later, on the key load — proving the
        /// refusal above is the contact check and not an unrelated early
        /// error.
        #[test]
        fn restore_with_the_right_tape_gets_past_the_contact_check() {
            let conn = crate::db::open_memory().unwrap();
            seed(&conn, "RESTORE-OK", "r-unit");
            let mut store = tape_labelled("RESTORE-OK");
            let dest = TempDir::new().unwrap();
            let paths = TapectlPaths::new(dest.path().to_path_buf());

            let err = restore_unit_from_store(
                &conn,
                &paths,
                &Config::default(),
                "r-unit",
                "RESTORE-OK",
                1,
                RestoreTarget::Unit {
                    dest_dir: &dest.path().to_string_lossy(),
                },
                &RestoreOptions::default(),
                &mut store,
                site(Operation::RestoreUnit),
            )
            .unwrap_err()
            .to_string();
            assert!(
                !err.contains("wrong tape"),
                "the contact check must have passed; got: {err}"
            );
        }

        // ── issue #435: restore names no volume; the loaded tape does ──

        /// The uuid `tape_labelled` writes into File 0.
        const TAPE_UUID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

        /// The contacts of the catalog, oldest first:
        /// `(operation, volume_id, outcome, detail)`.
        #[allow(clippy::type_complexity)]
        fn contact_rows(
            conn: &Connection,
        ) -> Vec<(String, Option<i64>, Option<String>, Option<String>)> {
            conn.prepare(
                "SELECT operation, volume_id, outcome, detail FROM cartridge_contacts ORDER BY id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
        }

        /// The `detail` of the only contact.
        fn contact_detail(conn: &Connection) -> Option<String> {
            conn.query_row("SELECT detail FROM cartridge_contacts", [], |r| r.get(0))
                .unwrap()
        }

        /// [`seed`], the volume carrying the uuid the loaded tape's File 0
        /// has. Returns the volume's id.
        fn seed_loaded(conn: &Connection, label: &str, unit_name: &str) -> i64 {
            seed(conn, label, unit_name);
            conn.execute(
                "UPDATE volumes SET uuid = ?1 WHERE label = ?2",
                params![TAPE_UUID, label],
            )
            .unwrap();
            conn.query_row(
                "SELECT id FROM volumes WHERE label = ?1",
                params![label],
                |r| r.get(0),
            )
            .unwrap()
        }

        /// A `TapeStore` over a `FakeTape` holding `tape`'s files, so every
        /// head motion is recorded.
        fn on_fake_tape(tape: &MemStore) -> (crate::tape::fake::FakeTape, TapeStore) {
            let fake = crate::tape::fake::FakeTape::with_files(tape.files.clone(), 4096);
            let store = TapeStore::from_ops(fake.boxed(), 0).unwrap();
            (fake, store)
        }

        /// No `--from`: the volume File 0 names is the one restored from —
        /// the restore gets past its contact check on it (and fails later,
        /// on the key load these fixtures do not have), records its contact
        /// against that volume, and moves the head exactly as the same
        /// restore with the label typed does: File 0 is read once.
        #[test]
        fn restore_with_no_from_restores_from_the_volume_file0_names() {
            let conn = crate::db::open_memory().unwrap();
            let volume_id = seed_loaded(&conn, "RL-ONE", "rl-unit");
            let tape = tape_labelled("RL-ONE");
            let dest = TempDir::new().unwrap();
            let paths = TapectlPaths::new(dest.path().to_path_buf());
            let dest_dir = dest.path().to_string_lossy().into_owned();
            let (fake, mut store) = on_fake_tape(&tape);

            let err = restore_unit_from_loaded_store(
                &conn,
                &paths,
                &Config::default(),
                "rl-unit",
                RestoreTarget::Unit {
                    dest_dir: &dest_dir,
                },
                &RestoreOptions::default(),
                None,
                &mut store,
                site(Operation::RestoreUnit),
            )
            .unwrap_err()
            .to_string();
            for refusal in [
                "wrong tape",
                "does not identify",
                "no volume with that uuid",
            ] {
                assert!(
                    !err.contains(refusal),
                    "the loaded volume was found and its contact check passed: {err}"
                );
            }
            assert_eq!(
                contact_rows(&conn),
                vec![(
                    "restore unit".to_string(),
                    Some(volume_id),
                    // The fixture has no key, so the restore fails past the
                    // contact check; what matters here is that it got there
                    // on ONE contact (issue #442), not a second one opened
                    // after the first.
                    Some("failed".to_string()),
                    contact_detail(&conn)
                )],
                "one contact, naming the volume"
            );
            assert!(
                !contact_detail(&conn)
                    .unwrap_or_default()
                    .contains("no volume with that uuid"),
                "the contact failed on the key load, not on finding the volume"
            );

            // The same restore with the label typed, on a fresh copy of the
            // same tape: identical head motion.
            let conn2 = crate::db::open_memory().unwrap();
            seed_loaded(&conn2, "RL-ONE", "rl-unit");
            let (fake2, mut store2) = on_fake_tape(&tape);
            let _ = restore_unit_from_store(
                &conn2,
                &paths,
                &Config::default(),
                "rl-unit",
                "RL-ONE",
                1,
                RestoreTarget::Unit {
                    dest_dir: &dest_dir,
                },
                &RestoreOptions::default(),
                &mut store2,
                site(Operation::RestoreUnit),
            );
            assert_eq!(
                fake.ops(),
                fake2.ops(),
                "no `--from` costs the same head motion as naming the volume"
            );
            assert!(
                fake.ops().contains(&crate::tape::fake::Op::ReadHead(0)),
                "positive control: File 0 was read"
            );
        }

        /// A tape this catalog does not know is refused, naming what File 0
        /// says, before the catalog's refusals, any scratch directory, any
        /// key. The drive was opened and File 0 read, so it is ONE contact,
        /// `failed`, with the reason and no volume (issue #442).
        #[test]
        fn restore_with_no_from_refuses_a_tape_the_catalog_does_not_know() {
            let conn = crate::db::open_memory().unwrap();
            seed(&conn, "RL-KNOWN", "rl-unit");
            conn.execute("UPDATE volumes SET uuid = 'some-other-uuid'", [])
                .unwrap();
            let mut store = tape_labelled("RL-LOADED");
            let dest = TempDir::new().unwrap();
            let paths = TapectlPaths::new(dest.path().to_path_buf());
            let dest_dir = dest.path().to_string_lossy().into_owned();

            let err = restore_unit_from_loaded_store(
                &conn,
                &paths,
                &Config::default(),
                "rl-unit",
                RestoreTarget::Unit {
                    dest_dir: &dest_dir,
                },
                &RestoreOptions::default(),
                None,
                &mut store,
                site(Operation::RestoreUnit),
            )
            .unwrap_err()
            .to_string();

            assert!(
                err.contains("\"RL-LOADED\"") && err.contains(TAPE_UUID),
                "{err}"
            );
            assert!(err.contains("no volume with that uuid"), "{err}");
            let contacts = contact_rows(&conn);
            assert_eq!(contacts.len(), 1, "one contact: {contacts:?}");
            let (operation, volume_id, outcome, detail) = &contacts[0];
            assert_eq!(operation, "restore unit");
            assert_eq!(*volume_id, None, "the catalog has no volume for this tape");
            assert_eq!(outcome.as_deref(), Some("failed"));
            assert!(
                detail
                    .as_deref()
                    .is_some_and(|d| d.contains("no volume with that uuid")),
                "the reason is recorded: {detail:?}"
            );
            let closed: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM cartridge_contacts WHERE closed_at IS NOT NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(closed, 1, "closed, not left to read as a crash");
            let restores: i64 = conn
                .query_row("SELECT COUNT(*) FROM restores", [], |r| r.get(0))
                .unwrap();
            assert_eq!(restores, 0, "no restore began, so no restores row");
            assert!(!dest.path().join(".tapectl-restore-tmp").exists());
        }

        /// A refusal of the checks a named volume makes before the tape
        /// (`plan_before_tape`) comes AFTER File 0 here, with the drive
        /// open: one contact, `failed`, with the reason, naming the volume
        /// the catalog resolved (issue #442). Nothing is restored and no
        /// `restores` row is written, as for a named volume's refusal.
        #[test]
        fn restore_with_no_from_records_a_plan_refusal_as_a_failed_contact() {
            let conn = crate::db::open_memory().unwrap();
            let volume_id = seed_loaded(&conn, "RL-PLAN", "rl-unit");
            let tape = tape_labelled("RL-PLAN");
            let home = TempDir::new().unwrap();
            let paths = TapectlPaths::new(home.path().to_path_buf());
            // A destination that is not empty: refused before any slice is read.
            let dest = TempDir::new().unwrap();
            fs::write(dest.path().join("already-here"), b"x").unwrap();
            let dest_dir = dest.path().to_string_lossy().into_owned();
            let (fake, mut store) = on_fake_tape(&tape);

            let err = restore_unit_from_loaded_store(
                &conn,
                &paths,
                &Config::default(),
                "rl-unit",
                RestoreTarget::Unit {
                    dest_dir: &dest_dir,
                },
                &RestoreOptions::default(),
                None,
                &mut store,
                site(Operation::RestoreUnit),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("it is not empty"), "{err}");

            let contacts = contact_rows(&conn);
            assert_eq!(contacts.len(), 1, "one contact: {contacts:?}");
            let (operation, contact_volume, outcome, detail) = &contacts[0];
            assert_eq!(operation, "restore unit");
            assert_eq!(*contact_volume, Some(volume_id), "the volume File 0 named");
            assert_eq!(outcome.as_deref(), Some("failed"));
            assert!(
                detail
                    .as_deref()
                    .is_some_and(|d| d.contains("it is not empty")),
                "the reason is recorded: {detail:?}"
            );
            let restores: i64 = conn
                .query_row("SELECT COUNT(*) FROM restores", [], |r| r.get(0))
                .unwrap();
            assert_eq!(restores, 0, "the restore never began");
            assert!(
                fake.ops().contains(&crate::tape::fake::Op::ReadHead(0)),
                "positive control: the drive was in use (File 0 was read)"
            );
            assert!(
                !fake
                    .ops()
                    .iter()
                    .any(|o| matches!(o, crate::tape::fake::Op::Read(_))),
                "and no slice was read: {:?}",
                fake.ops()
            );
        }

        /// The same for a unit the catalog does not have on the volume.
        #[test]
        fn restore_with_no_from_records_an_unknown_unit_as_a_failed_contact() {
            let conn = crate::db::open_memory().unwrap();
            let volume_id = seed_loaded(&conn, "RL-NOUNIT", "rl-unit");
            let tape = tape_labelled("RL-NOUNIT");
            let dest = TempDir::new().unwrap();
            let paths = TapectlPaths::new(dest.path().to_path_buf());
            let dest_dir = dest.path().join("out").to_string_lossy().into_owned();
            let (_fake, mut store) = on_fake_tape(&tape);

            let err = restore_unit_from_loaded_store(
                &conn,
                &paths,
                &Config::default(),
                "no-such-unit",
                RestoreTarget::Unit {
                    dest_dir: &dest_dir,
                },
                &RestoreOptions::default(),
                None,
                &mut store,
                site(Operation::RestoreUnit),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("no-such-unit"), "{err}");

            let contacts = contact_rows(&conn);
            assert_eq!(contacts.len(), 1, "{contacts:?}");
            assert_eq!(contacts[0].1, Some(volume_id));
            assert_eq!(contacts[0].2.as_deref(), Some("failed"));
            assert!(
                contacts[0]
                    .3
                    .as_deref()
                    .is_some_and(|d| d.contains("no-such-unit")),
                "{contacts:?}"
            );
        }

        // ── issue #296: the contact ROW ──

        /// `restore unit` records its contact, refusal and all: a cartridge
        /// was in a drive and File 0 was read off it, which is what
        /// `cartridge_contacts` records. `operation` and `outcome` are
        /// asserted BY VALUE — a row exists either way, and "which command,
        /// ending how" is the whole question.
        #[test]
        fn a_restore_refused_for_the_wrong_tape_still_records_its_contact() {
            let conn = crate::db::open_memory().unwrap();
            seed(&conn, "RC-WANT", "rc-unit");
            let mut store = tape_labelled("RC-LOADED");
            let dest = TempDir::new().unwrap();
            let paths = TapectlPaths::new(dest.path().to_path_buf());

            restore_unit_from_store(
                &conn,
                &paths,
                &Config::default(),
                "rc-unit",
                "RC-WANT",
                1,
                RestoreTarget::Unit {
                    dest_dir: &dest.path().to_string_lossy(),
                },
                &RestoreOptions::default(),
                &mut store,
                site(Operation::RestoreUnit),
            )
            .unwrap_err();

            let rows: i64 = conn
                .query_row("SELECT COUNT(*) FROM cartridge_contacts", [], |r| r.get(0))
                .unwrap();
            assert_eq!(rows, 1);
            let (operation, outcome) = only_contact(&conn);
            assert_eq!(operation, "restore unit");
            assert_eq!(outcome.as_deref(), Some("failed"));

            // The contact names the volume the operator asked for — the one
            // this command is about, not the one File 0 turned out to claim.
            let want: i64 = conn
                .query_row("SELECT id FROM volumes WHERE label = 'RC-WANT'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            let vol: Option<i64> = conn
                .query_row("SELECT volume_id FROM cartridge_contacts", [], |r| r.get(0))
                .unwrap();
            assert_eq!(vol, Some(want));
        }

        /// The positive control for the assertion above: with the RIGHT tape
        /// the restore gets past the contact check and fails later, on the
        /// key load — and STILL records exactly one contact, proving the row
        /// above is not an artefact of the refusal path.
        #[test]
        fn a_restore_that_passes_the_contact_check_records_exactly_one_contact() {
            let conn = crate::db::open_memory().unwrap();
            seed(&conn, "RC-OK", "rc-unit");
            let mut store = tape_labelled("RC-OK");
            let dest = TempDir::new().unwrap();
            let paths = TapectlPaths::new(dest.path().to_path_buf());

            let err = restore_unit_from_store(
                &conn,
                &paths,
                &Config::default(),
                "rc-unit",
                "RC-OK",
                1,
                RestoreTarget::Unit {
                    dest_dir: &dest.path().to_string_lossy(),
                },
                &RestoreOptions::default(),
                &mut store,
                site(Operation::RestoreUnit),
            )
            .unwrap_err()
            .to_string();
            assert!(!err.contains("wrong tape"), "{err}");

            let rows: i64 = conn
                .query_row("SELECT COUNT(*) FROM cartridge_contacts", [], |r| r.get(0))
                .unwrap();
            assert_eq!(rows, 1, "one command holding the drive is one contact");
            assert_eq!(only_contact(&conn).0, "restore unit");
        }

        /// `(outcome, cartridge_id, volume_id, backend_name, identity_reason)`.
        type RawRow = (
            Option<String>,
            Option<i64>,
            Option<i64>,
            Option<String>,
            Option<String>,
        );

        /// `restore raw-volume`'s contact row after `restore_raw_volume`
        /// over `store`: `(outcome, cartridge_id, volume_id, backend_name,
        /// identity_reason)`. The dump itself fails — `tape_labelled` writes
        /// File 0 only and `restore_raw` needs the front index at File 3 —
        /// which is beside the point: recording the contact must not depend
        /// on the dump succeeding.
        fn raw_volume_contact(conn: &Connection, config: &Config, medium: Medium<'_>) -> RawRow {
            let mut store = tape_labelled("RAW-VOL");
            let dest = TempDir::new().unwrap();
            let _ = restore_raw_volume(
                conn,
                &mut store,
                dest.path(),
                None,
                ContactSite::new(config, Operation::RestoreRawVolume, "/dev/null", medium),
            );
            assert_eq!(only_contact(conn).0, "restore raw-volume");
            conn.query_row(
                "SELECT outcome, cartridge_id, volume_id, backend_name, identity_reason
                 FROM cartridge_contacts",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap()
        }

        fn lto0() -> crate::config::LtoBackendConfig {
            crate::config::LtoBackendConfig {
                name: "lto0".to_string(),
                device_tape: "/dev/null".to_string(),
                device_sg: "/dev/sg-nonexistent".to_string(),
                generation: "LTO-6".to_string(),
                capacity_override: None,
                fill_ceiling: 0.95,
                enospc_buffer: "1GiB".to_string(),
                post_warning_bytes: None,
            }
        }

        /// Issue #316: where the CLI read the MAM, the contact says what the
        /// read found — not the since-removed "no MAM read is attempted"
        /// reason (issue #318), which denied a read the journal recorded. The serial is on no registered cartridge
        /// (a DR catalog that never saw this tape), so the reason is
        /// `REASON_SERIAL_UNREGISTERED`, and the backend the read went
        /// through is recorded.
        #[test]
        fn restore_raw_volume_contact_reflects_the_mam_read_the_cli_took() {
            let conn = crate::db::open_memory().unwrap();
            let backend = lto0();
            let mut config = Config::default();
            config.backends.lto.push(backend.clone());
            let mam = crate::tape::mam::MamInfo {
                serial: Some("RAWSERIAL01".to_string()),
                load_count: Some(12),
                ..Default::default()
            };

            let (outcome, cartridge, volume, backend_name, reason) = raw_volume_contact(
                &conn,
                &config,
                Medium::Observed {
                    backend: &backend,
                    mam: &mam,
                },
            );
            assert_eq!(outcome.as_deref(), Some("failed"));
            assert_eq!(
                reason.as_deref(),
                Some(crate::tape::contact::REASON_SERIAL_UNREGISTERED)
            );
            assert_eq!(backend_name.as_deref(), Some("lto0"));
            assert_eq!(
                cartridge, None,
                "no registered cartridge carries that serial"
            );
            assert_eq!(
                volume, None,
                "raw-volume runs against whatever tape is loaded"
            );

            // And a registered serial names its cartridge: the read is used.
            let conn = crate::db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, serial_number, nominal_capacity)
                 VALUES ('RAW001L6', 'LTO-6', 'RAWSERIAL01', 2500000000000)",
                [],
            )
            .unwrap();
            let cid = conn.last_insert_rowid();
            let (_, cartridge, _, _, reason) = raw_volume_contact(
                &conn,
                &config,
                Medium::Observed {
                    backend: &backend,
                    mam: &mam,
                },
            );
            assert_eq!(cartridge, Some(cid));
            assert_eq!(reason, None);
        }

        // ── issue #314: every contact names its drive ──

        fn drive(serial: Option<&str>) -> crate::tape::drive_identity::DriveIdentity {
            crate::tape::drive_identity::DriveIdentity {
                serial: serial.map(str::to_string),
                vendor: Some("HP".to_string()),
                model: Some("Ultrium 6-SCSI".to_string()),
                firmware_rev: Some("35GD".to_string()),
            }
        }

        /// The serial of the drive the one contact row names, via the FK —
        /// `None` when `drive_id` is NULL. By VALUE: "some drive id" cannot
        /// pass for "this drive".
        fn contact_drive_serial(conn: &Connection) -> Option<String> {
            conn.query_row(
                "SELECT d.serial FROM cartridge_contacts c LEFT JOIN drives d ON d.id = c.drive_id",
                [],
                |r| r.get(0),
            )
            .unwrap()
        }

        fn drive_rows(conn: &Connection) -> i64 {
            conn.query_row("SELECT COUNT(*) FROM drives", [], |r| r.get(0))
                .unwrap()
        }

        /// `restore unit` over a store, with the backend resolved and the
        /// drive answering `identity`. Returns the contact's drive serial.
        fn restore_unit_drive(
            conn: &Connection,
            identity: &crate::tape::drive_identity::DriveIdentity,
            observed: bool,
        ) -> Option<String> {
            seed(conn, "RD-VOL", "rd-unit");
            let mut store = tape_labelled("RD-VOL");
            let dest = TempDir::new().unwrap();
            let paths = TapectlPaths::new(dest.path().to_path_buf());
            let backend = lto0();
            let mut config = Config::default();
            if observed {
                config.backends.lto.push(backend.clone());
            }
            let mam = crate::tape::mam::MamInfo::default();
            let medium = if observed {
                Medium::Observed {
                    backend: &backend,
                    mam: &mam,
                }
            } else {
                Medium::NoBackend
            };
            let _ = restore_unit_from_store(
                conn,
                &paths,
                &Config::default(),
                "rd-unit",
                "RD-VOL",
                1,
                RestoreTarget::Unit {
                    dest_dir: &dest.path().to_string_lossy(),
                },
                &RestoreOptions::default(),
                &mut store,
                ContactSite::new(&config, Operation::RestoreUnit, "/dev/null", medium)
                    .with_drive_identity(identity),
            );
            assert_eq!(only_contact(conn).0, "restore unit");
            contact_drive_serial(conn)
        }

        /// Issue #314: `restore unit` collected no health then, so before the
        /// fix its contact never named a drive (4 of 4 NULL in the gate). The
        /// read path is exactly where a drive fault shows up — the contact
        /// is attributed at open, from the identity read alone (and, since
        /// #320, again by its post-command reading, to the same drive).
        #[test]
        fn a_restore_unit_contact_names_the_drive_it_was_made_with() {
            let conn = crate::db::open_memory().unwrap();
            let serial = restore_unit_drive(&conn, &drive(Some("HUJ808A5L4")), true);
            assert_eq!(serial.as_deref(), Some("HUJ808A5L4"));
        }

        /// No serial: the drive is unknown, and unknown is recorded by
        /// absence — NULL `drive_id`, and no `drives` row invented from the
        /// vendor/model it did give.
        #[test]
        fn a_restore_unit_contact_with_no_drive_serial_names_no_drive() {
            let conn = crate::db::open_memory().unwrap();
            assert_eq!(restore_unit_drive(&conn, &drive(None), true), None);
            assert_eq!(drive_rows(&conn), 0, "no serial, no drives row");
        }

        /// The DR machine: no backend configured, so nothing to ask — the
        /// identity is never consulted even when one is on offer, and the
        /// contact names no drive. The positive control is the test two
        /// above: the SAME identity with a backend is recorded.
        #[test]
        fn a_restore_unit_contact_with_no_backend_names_no_drive() {
            let conn = crate::db::open_memory().unwrap();
            assert_eq!(
                restore_unit_drive(&conn, &drive(Some("HUJ808A5L4")), false),
                None
            );
            assert_eq!(drive_rows(&conn), 0, "no backend, no identity read");
        }

        /// `restore raw-volume` — a second read path, the heir/DR one —
        /// names its drive the same way, through the same seam.
        #[test]
        fn a_restore_raw_volume_contact_names_the_drive_it_was_made_with() {
            let conn = crate::db::open_memory().unwrap();
            let backend = lto0();
            let mut config = Config::default();
            config.backends.lto.push(backend.clone());
            let mam = crate::tape::mam::MamInfo::default();
            let identity = drive(Some("XYZZY_A1"));
            let mut store = tape_labelled("RAW-DRIVE");
            let dest = TempDir::new().unwrap();
            let _ = restore_raw_volume(
                &conn,
                &mut store,
                dest.path(),
                None,
                ContactSite::new(
                    &config,
                    Operation::RestoreRawVolume,
                    "/dev/null",
                    Medium::Observed {
                        backend: &backend,
                        mam: &mam,
                    },
                )
                .with_drive_identity(&identity),
            );
            assert_eq!(only_contact(&conn).0, "restore raw-volume");
            assert_eq!(contact_drive_serial(&conn).as_deref(), Some("XYZZY_A1"));
        }

        // ── issue #320: a read-path contact takes ONE post-command health
        //    reading — one sweep, one `health_logs` row, both naming it ──

        use crate::tape::log_pages::tests::{
            assert_each_page_read_once, assert_one_reading_for, FixtureSource, LISTED,
        };
        use std::cell::RefCell;

        /// The id of the one contact row.
        fn only_contact_id(conn: &Connection) -> i64 {
            conn.query_row("SELECT id FROM cartridge_contacts", [], |r| r.get(0))
                .unwrap()
        }

        fn journal_rows(conn: &Connection) -> i64 {
            conn.query_row("SELECT COUNT(*) FROM log_page_journal", [], |r| r.get(0))
                .unwrap()
        }

        fn health_rows(conn: &Connection) -> i64 {
            conn.query_row("SELECT COUNT(*) FROM health_logs", [], |r| r.get(0))
                .unwrap()
        }

        /// `restore unit` over a `MemStore` whose File 0 is `loaded`, the
        /// catalog asking for `want`, the drive `device` in `config`, the
        /// log pages answered by `src`. Returns the command's result as a
        /// string, `Ok` or the error.
        #[allow(clippy::too_many_arguments)]
        fn restore_unit_swept(
            conn: &Connection,
            config: &Config,
            device: &str,
            want: &str,
            loaded: &str,
            src: &RefCell<FixtureSource>,
        ) -> std::result::Result<RestoreReport, String> {
            seed(conn, want, "rh-unit");
            let mut store = tape_labelled(loaded);
            let dest = TempDir::new().unwrap();
            let paths = TapectlPaths::new(dest.path().to_path_buf());
            let identity = drive(Some("HUJ808A5L4"));
            let backend = lto0();
            let mam = crate::tape::mam::MamInfo::default();
            restore_unit_from_store(
                conn,
                &paths,
                &Config::default(),
                "rh-unit",
                want,
                1,
                RestoreTarget::Unit {
                    dest_dir: &dest.path().to_string_lossy(),
                },
                &RestoreOptions::default(),
                &mut store,
                ContactSite::new(
                    config,
                    Operation::RestoreUnit,
                    device,
                    Medium::Observed {
                        backend: &backend,
                        mam: &mam,
                    },
                )
                .with_drive_identity(&identity)
                .with_log_source(src),
            )
            .map_err(|e| e.to_string())
        }

        fn swept_config() -> Config {
            let mut config = Config::default();
            config.backends.lto.push(lto0());
            config
        }

        /// A `restore unit` whose contact check passes (it then fails on the
        /// key load, which is beside the point — the sweep is post-command,
        /// on every outcome) takes exactly one sweep and one `restore`
        /// reading, both naming its contact and its volume; the drive the
        /// reading identified is the contact's.
        #[test]
        fn a_restore_unit_contact_takes_exactly_one_health_reading() {
            let conn = crate::db::open_memory().unwrap();
            let src = RefCell::new(FixtureSource::default());
            let err =
                restore_unit_swept(&conn, &swept_config(), "/dev/null", "RH-OK", "RH-OK", &src)
                    .unwrap_err();
            assert!(!err.contains("wrong tape"), "past the contact check: {err}");

            let cid = only_contact_id(&conn);
            let vid: i64 = conn
                .query_row("SELECT id FROM volumes WHERE label = 'RH-OK'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(
                src.borrow().order,
                LISTED.to_vec(),
                "one sweep, every listed page"
            );
            assert_each_page_read_once(&src.borrow().reads);
            assert_one_reading_for(&conn, cid, "restore unit", Some(vid), LISTED.len());
            assert_eq!(contact_drive_serial(&conn).as_deref(), Some("HUJ808A5L4"));
        }

        /// A restore REFUSED at the contact check still took a contact, so
        /// it still takes its reading — a failed read path is the one whose
        /// counters matter most.
        #[test]
        fn a_restore_unit_refused_for_the_wrong_tape_still_takes_its_reading() {
            let conn = crate::db::open_memory().unwrap();
            let src = RefCell::new(FixtureSource::default());
            let err = restore_unit_swept(
                &conn,
                &swept_config(),
                "/dev/null",
                "RH-WANT",
                "RH-LOADED",
                &src,
            )
            .unwrap_err();
            assert!(err.contains("wrong tape"), "{err}");
            let cid = only_contact_id(&conn);
            let vid: i64 = conn
                .query_row("SELECT id FROM volumes WHERE label = 'RH-WANT'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_one_reading_for(&conn, cid, "restore unit", Some(vid), LISTED.len());
        }

        /// Issue #313 on the READ path, ungated: `--device` spelled by-id
        /// (a symlink) while the backend is configured by its node still
        /// finds the backend, so the reading happens. Pairs with the gate's
        /// `health_by_id_restore`.
        #[test]
        fn a_by_id_restore_unit_contact_still_takes_its_reading() {
            let tmp = TempDir::new().unwrap();
            let nst = tmp.path().join("nst1");
            std::fs::File::create(&nst).unwrap();
            let by_id = tmp.path().join("scsi-XYZZY_A1-nst");
            std::os::unix::fs::symlink(&nst, &by_id).unwrap();
            let mut config = Config::default();
            let mut bk = lto0();
            bk.device_tape = nst.display().to_string();
            config.backends.lto.push(bk);

            let conn = crate::db::open_memory().unwrap();
            let src = RefCell::new(FixtureSource::default());
            let _ = restore_unit_swept(
                &conn,
                &config,
                &by_id.display().to_string(),
                "RH-BYID",
                "RH-BYID",
                &src,
            );
            let cid = only_contact_id(&conn);
            let vid: i64 = conn
                .query_row("SELECT id FROM volumes WHERE label = 'RH-BYID'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_one_reading_for(&conn, cid, "restore unit", Some(vid), LISTED.len());
        }

        /// The DR machine: no backend claims the device, so no sweep is
        /// taken — the source is never asked — and no health or journal row
        /// is written; and the restore ends EXACTLY as it does with the
        /// health reading taken (positive control: the same command with a
        /// backend does read).
        #[test]
        fn a_dr_restore_with_no_backend_takes_no_reading_and_ends_the_same() {
            let dr = crate::db::open_memory().unwrap();
            let dr_src = RefCell::new(FixtureSource::default());
            let dr_result = restore_unit_swept(
                &dr,
                &Config::default(),
                "/dev/null",
                "RH-DR",
                "RH-DR",
                &dr_src,
            );
            assert!(dr_src.borrow().reads.is_empty(), "no backend, no sweep");
            assert_eq!(health_rows(&dr), 0);
            assert_eq!(journal_rows(&dr), 0);
            assert_eq!(
                only_contact(&dr).0,
                "restore unit",
                "the contact is still recorded"
            );

            let swept = crate::db::open_memory().unwrap();
            let src = RefCell::new(FixtureSource::default());
            let swept_result =
                restore_unit_swept(&swept, &swept_config(), "/dev/null", "RH-DR", "RH-DR", &src);
            assert_eq!(health_rows(&swept), 1, "positive control: a backend reads");
            assert_eq!(
                dr_result.map(|r| r.slices),
                swept_result.map(|r| r.slices),
                "the health reading changes nothing about how the restore ends"
            );
        }

        /// A sweep that fails outright — every page read fails — fails
        /// nothing: the restore ends as it would have, the failed reads are
        /// journalled, and no health row claims a reading nobody got.
        #[test]
        fn a_failed_sweep_does_not_fail_the_restore() {
            let conn = crate::db::open_memory().unwrap();
            let mut failing = FixtureSource::default();
            failing.fail.extend([0x00, 0x02, 0x03, 0x2e]);
            let src = RefCell::new(failing);
            let err = restore_unit_swept(&conn, &swept_config(), "/dev/null", "RH-F", "RH-F", &src)
                .unwrap_err();
            // The same command with no drive to sweep ends with the SAME
            // error: the failed sweep contributed nothing to the outcome.
            let dr = crate::db::open_memory().unwrap();
            let unused = RefCell::new(FixtureSource::default());
            let dr_err = restore_unit_swept(
                &dr,
                &Config::default(),
                "/dev/null",
                "RH-F",
                "RH-F",
                &unused,
            )
            .unwrap_err();
            assert_eq!(err, dr_err);
            assert_eq!(health_rows(&conn), 0, "no page read, no reading");
            assert_eq!(
                journal_rows(&conn),
                4,
                "0x00 then the three fallback pages, journalled"
            );
        }

        /// `restore raw-volume`: one reading on its contact, which names no
        /// volume — so neither does the reading.
        #[test]
        fn a_restore_raw_volume_contact_takes_exactly_one_health_reading() {
            let conn = crate::db::open_memory().unwrap();
            let config = swept_config();
            let backend = lto0();
            let mam = crate::tape::mam::MamInfo::default();
            let identity = drive(Some("XYZZY_A1"));
            let src = RefCell::new(FixtureSource::default());
            let mut store = tape_labelled("RAW-HEALTH");
            let dest = TempDir::new().unwrap();
            let _ = restore_raw_volume(
                &conn,
                &mut store,
                dest.path(),
                None,
                ContactSite::new(
                    &config,
                    Operation::RestoreRawVolume,
                    "/dev/null",
                    Medium::Observed {
                        backend: &backend,
                        mam: &mam,
                    },
                )
                .with_drive_identity(&identity)
                .with_log_source(&src),
            );
            let cid = only_contact_id(&conn);
            assert_each_page_read_once(&src.borrow().reads);
            assert_one_reading_for(&conn, cid, "restore raw-volume", None, LISTED.len());
        }

        /// `restore file` reaches the drive only through `restore_unit`,
        /// whose store seam is the one place a read-path reading is taken —
        /// so one `restore file` is one contact and ONE sweep, never two.
        /// Pinned by source scan (the entry points need a drive), calibrated
        /// by finding each function first; the behaviour of the seam itself
        /// is `a_restore_unit_contact_takes_exactly_one_health_reading`.
        #[test]
        fn restore_file_takes_one_reading_through_restore_unit_not_a_second() {
            const SRC: &str = include_str!("restore.rs");
            let prod = SRC.split("#[cfg(test)]\nmod tests").next().unwrap();
            assert!(prod.len() < SRC.len(), "positive control: tests split off");
            let body = |f: &str| {
                let start = prod.find(f).unwrap_or_else(|| panic!("no {f}"));
                let end = prod[start..].find("\n}\n").unwrap() + start;
                &prod[start..end]
            };
            // Both entry points reach the drive through ONE path
            // (`restore_through_drive`, issue #306), and that path reaches
            // the store seam exactly once.
            let file = body("pub fn restore_file(");
            assert_eq!(
                file.matches("restore_through_drive(").count(),
                1,
                "positive control"
            );
            let unit = body("pub fn restore_unit(");
            assert_eq!(
                unit.matches("restore_through_drive(").count(),
                1,
                "positive control"
            );
            let drive = body("fn restore_through_drive(");
            assert_eq!(drive.matches("restore_unit_from_bay(").count(), 1);
            // Issue #442: the seam opens the contact and ends in ONE tail,
            // `restore_unit_in_contact`, which the no-label restore ends in
            // too; the tail takes the reading.
            let seam = body("fn restore_unit_in_contact(");
            let opener = body("fn restore_unit_from_bay(");
            assert_eq!(opener.matches(".open(conn").count(), 1);
            assert_eq!(opener.matches("restore_unit_in_contact(").count(), 1);
            assert!(!opener.contains("health_after_read_contact("));
            for (name, b) in [
                ("restore_file", file),
                ("restore_unit", unit),
                ("restore_through_drive", drive),
            ] {
                for forbidden in [
                    "health_after_read_contact(",
                    "collect_health",
                    "log_pages::",
                    ".open(conn",
                ] {
                    assert!(
                        !b.contains(forbidden),
                        "{name} must not take its own contact or reading ({forbidden})"
                    );
                }
            }
            // Issue #480: the reading is taken where a cartridge's contact
            // is closed (`Presence::close` in supply.rs), so a cartridge
            // swapped in mid-restore has its own contact and its own reading
            // and none has two. The tail closes the last one, once.
            assert_eq!(
                seam.matches("supply.finish(").count(),
                1,
                "the tail of the seam that opens the contact closes the last cartridge's \
                 contact, which takes its one reading"
            );
            assert!(!seam.contains("health_after_read_contact("));
            // Issue #398: a multi-unit restore is ONE contact, so its seam
            // closes ONE for the whole set — never one per unit.
            let units = body("fn restore_units_in_contact(");
            assert_eq!(units.matches("supply.finish(").count(), 1);
            assert!(!units.contains("health_after_read_contact("));
            let units_opener = body("fn restore_units_from_bay(");
            assert_eq!(units_opener.matches(".open(conn").count(), 1);
            assert_eq!(units_opener.matches("restore_units_in_contact(").count(), 1);
            assert!(!units_opener.contains("health_after_read_contact("));
            assert!(
                !units.contains("restore_unit_from_store(")
                    && !units.contains("restore_unit_in_contact("),
                "the set must not take a contact per unit"
            );
            // Issue #442: a no-label restore opens its contact before it can
            // know the volume, so it has its own two openers (unit, set) and
            // ONE closer for the refusals that come before a tail; a contact
            // ends in a tail or in the closer, never both.
            for (opener, tail) in [
                (
                    "fn restore_unit_from_loaded_bay(",
                    "restore_unit_in_contact(",
                ),
                (
                    "fn restore_units_from_loaded_bay(",
                    "restore_units_in_contact(",
                ),
            ] {
                let b = body(opener);
                assert_eq!(b.matches(".open(conn").count(), 1, "{opener}");
                assert_eq!(b.matches(tail).count(), 1, "{opener}");
                assert!(!b.contains("health_after_read_contact("), "{opener}");
                assert!(b.contains("refuse_loaded_contact("), "{opener}");
            }
            let closer = body("fn refuse_loaded_contact<T>(");
            assert_eq!(closer.matches("health_after_read_contact(").count(), 1);
            assert_eq!(
                prod.matches("health_after_read_contact(").count(),
                2,
                "restore.rs takes readings in exactly two places: raw-volume's, and the refusal \
                 of a no-label restore's contact; every other contact closes through the \
                 supply"
            );
            const SUPPLY: &str = include_str!("restore/supply.rs");
            let supply = SUPPLY.split("#[cfg(test)]\nmod tests").next().unwrap();
            assert_eq!(
                supply.matches("health_after_read_contact(").count(),
                1,
                "the supply takes a cartridge's reading in one place, where its contact closes"
            );
            assert_eq!(
                supply.matches(".close(").count(),
                3,
                "a contact is closed by the last cartridge's finish, by a cartridge leaving, or \
                 by one that failed its identity check on arrival"
            );
        }

        /// Positive control for
        /// `restore_raw_volume_contact_reflects_the_mam_read_the_cli_took`:
        /// the reason column is live on this path. With no backend
        /// configured no MAM read happens, and the contact says exactly that
        /// — by value, so the reason assertions there cannot pass on a
        /// column that is never written.
        #[test]
        fn restore_raw_volume_contact_with_no_backend_says_no_read_happened() {
            let conn = crate::db::open_memory().unwrap();
            let (outcome, cartridge, volume, backend_name, reason) =
                raw_volume_contact(&conn, &Config::default(), Medium::NoBackend);
            assert_eq!(outcome.as_deref(), Some("failed"));
            assert_eq!(cartridge, None);
            assert_eq!(volume, None);
            assert_eq!(backend_name, None);
            assert_eq!(
                reason.as_deref(),
                Some(crate::tape::contact::REASON_NO_BACKEND_CONFIGURED),
                "a NULL cartridge_id with no reason is the data loss #296 exists to stop"
            );
        }
    }
}

// Declared last: the source-scan tests split this file at its first `#[cfg(test)]
// mod`, and the production code must all lie before it.
#[cfg(test)]
pub(crate) mod fixture;
#[cfg(test)]
mod heir_tests;
#[cfg(test)]
mod reader_tests;
#[cfg(test)]
pub(crate) mod repair_tests;
#[cfg(test)]
mod restore_tests;
#[cfg(test)]
mod restore_volume_tests;
#[cfg(test)]
mod short_ending_tests;
#[cfg(test)]
mod span_restore_tests;
#[cfg(test)]
mod utf8_tests;
