use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use tracing::{info, warn};

use crate::config::{Config, TapectlPaths};
use crate::crypto::keys;
use crate::dar;
use crate::dar::restore::DarReport;
use crate::db::queries;
use crate::error::{Result, TapectlError};
use crate::progress;
use crate::store::Store;
use crate::tape::contact::{self, ContactSite, Operation};
use crate::volume::restore_record::{self, RestoreRecord};

mod fill;
#[cfg(test)]
pub(crate) mod short_tape;
mod span;
mod supply;
#[cfg(test)]
use supply::FixedBay;
pub use supply::Substitution;
use supply::{Bay, DeviceBay, SliceSink, Supply};

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
    /// One entry: `dar -x -g file_path` extracts just it into the scratch
    /// directory (issue #406; it used to be the whole unit, in $TMPDIR),
    /// and it is then placed into `dest_dir`, the directory the operator
    /// named.
    File {
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
    /// Where the restore's `.tapectl-restore-tmp` directory is made.
    /// `None`: inside the destination. A streamed restore (issue #411)
    /// puts only its named pipes there; a spooled one its decrypted slices.
    /// Never `$TMPDIR` — a unit's slices can be hundreds of GiB, and a
    /// system temp directory is often RAM or the small root filesystem.
    pub scratch: Option<PathBuf>,
    /// Restore into a destination that already holds files: `restore unit`
    /// replaces any that collide (dar `-w`), `restore file` replaces the
    /// one file. Without it either refuses before the tape is touched.
    pub overwrite: bool,
    /// Skip the free-space check, for a filesystem that holds more than
    /// `statvfs` reports (a compressed or thin-provisioned one) — the same
    /// escape RESTORE.sh's `--no-space-check` is.
    pub no_space_check: bool,
    /// Never stream (`--spool`, ADR-0012 2026-10-06 item 23): decrypt every
    /// slice to scratch and let dar extract from the files, even when the
    /// unit's isolated catalogue is on disk and streaming would be chosen.
    /// The way round a streaming problem, and the way to reproduce the
    /// path a rebuilt catalog takes. `restore file` always spools anyway.
    pub spool: bool,
    /// Identity files given with `--key` (ADR-0012 2026-10-19 item 107),
    /// read before the drive was opened. They are used for decryption in
    /// this restore only and ADD to the home's own keys: never copied into
    /// the home, never registered as the operator's or the escrow's
    /// identity, never logged (`KeyFile`'s `Debug` shows a path only).
    pub keys: Vec<keys::KeyFile>,
}

/// The scratch directory's name, inside the destination or `--scratch`.
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
/// three, and one that failed inside dar still carries dar's report.
#[derive(Debug, Default)]
struct RestoreTrace {
    /// Slices decrypted off the tape so far.
    slices_read: i64,
    /// Their plaintext byte total, as measured through the hashing writer.
    bytes_decrypted: i64,
    /// dar's report, whenever dar ran.
    dar: Option<DarReport>,
    /// `dar --version`, read once dar has run; `None` if it could not be.
    dar_version: Option<String>,
    /// [`RestoreTarget::File`] only: the one entry was placed.
    placed: bool,
    /// Files that came back damaged or missing and were filled from another
    /// copy of the same version: the path, and the volume and check.
    filled: Vec<(String, String)>,
}

/// Removes the restore scratch directory when it goes out of scope, on every
/// path out of [`restore_unit`] — success, `?`, panic (issue #102).
///
/// The scratch directory holds **decrypted** dar slices. Before this guard,
/// cleanup lived at the end of the happy path only, so any failure between
/// creating the directory and finishing the extract — a checksum mismatch, a
/// dar error, a full disk, a missing key, a tape read error — left plaintext
/// archive content sitting in the destination directory the operator chose,
/// with nothing said about it. Everywhere else in this tool plaintext exists
/// only transiently inside staging; this was the one place it could be left
/// behind outside it, and it was on the failure path.
///
/// The directory deliberately lives *under the destination* (or under
/// `--scratch DIR`, [`scratch_dir`]) rather than in `std::env::temp_dir()`:
/// dar extracts from it into the destination, and a slice set can be
/// hundreds of gigabytes, so a system temp dir on a small tmpfs (or the small
/// root filesystem) is the wrong home for it. That is why this is a
/// hand-written guard and not `tempfile::tempdir()`. `restore file` used to
/// extract the whole unit into a `tempfile` directory in `$TMPDIR`; since
/// issue #406 it uses this scratch directory too.
///
/// A guard runs only if the process does: a SIGKILL or a power loss leaves
/// the directory behind. So the next restore refuses to start while it
/// exists, naming it, rather than mixing its slices with a new set
/// ([`preflight`]).
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
        });
    }

    // Issue #166: refuse before the store is opened if this drive cannot
    // read the loaded medium. Proceeds silently with no configured backend
    // or nothing detected — the DR machine with keys and no `backend add`
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
    config: &Config,
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
    // Issue #411: stream or spool, and which slices — decided here, so the
    // space check asks for what this restore will actually write.
    let plan = plan_restore(conn, config, &selection, target, &scratch, options.spool)?;
    preflight(
        conn, unit_name, &selection, &plan, target, &scratch, options,
    )?;
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

/// The restore's checks that need no tape (issue #406), in the order an
/// operator would want them named: the requested file, a scratch directory
/// a killed restore left behind, the destination, then the space.
fn preflight(
    conn: &Connection,
    unit_name: &str,
    selection: &RestoreSelection,
    plan: &RestorePlan,
    target: RestoreTarget<'_>,
    scratch: &Path,
    options: &RestoreOptions,
) -> Result<()> {
    let file_size = preflight_checks(conn, unit_name, selection, target, scratch, options)?;
    if options.no_space_check {
        info!("disk space not checked (--no-space-check)");
        return Ok(());
    }
    check_restore_space(Path::new(target.destination()), scratch, plan, file_size)
}

/// [`preflight`] without the space check — the file, a leftover scratch
/// directory, the destination — so a multi-unit restore can check the
/// space of the whole set at once (issue #398). Returns the size of the
/// `restore file` file (`None` for a unit).
fn preflight_checks(
    conn: &Connection,
    unit_name: &str,
    selection: &RestoreSelection,
    target: RestoreTarget<'_>,
    scratch: &Path,
    options: &RestoreOptions,
) -> Result<Option<i64>> {
    let destination = Path::new(target.destination());

    // `restore file`: the path must be one this version archived. The
    // catalog's `files` table is the record of what went into the archive;
    // a typo used to read every slice off the tape and then fail.
    let file_size = match target {
        RestoreTarget::File { file_path, .. } => {
            Some(catalog_file_size(conn, unit_name, selection, file_path)?)
        }
        RestoreTarget::Unit { .. } => None,
    };

    // A scratch directory that already exists is a killed restore's: its
    // guard never ran, and it may hold decrypted slices. Never mixed with a
    // new set, never silently removed.
    if fs::symlink_metadata(scratch).is_ok() {
        return Err(TapectlError::Other(format!(
            "refusing to restore: the scratch directory {} already exists. A restore that was \
             killed (or lost power) leaves it behind, and it may hold DECRYPTED archive slices. \
             Remove it (`rm -rf {}`) and run the restore again. Nothing was read from tape.",
            scratch.display(),
            scratch.display()
        )));
    }

    // The destination. `restore unit` wants an empty (or new) directory: dar
    // keeps an existing file rather than overwrite it, which used to be found
    // only after the whole unit was read and extracted. `restore file`
    // places one file, so only that one name matters.
    if !options.overwrite {
        match target {
            RestoreTarget::Unit { .. } => {
                if let Some(entry) = first_entry(destination)? {
                    return Err(TapectlError::Other(format!(
                        "refusing to restore into {}: it is not empty (it holds \"{entry}\", \
                         and maybe more). dar would keep every existing file that collides with \
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
                        placed.display()
                    )));
                }
            }
        }
    }
    Ok(file_size)
}

/// The size `files` records for `file_path` in the selected version, after
/// refusing a path that version did not archive, or a directory — `restore
/// file` places one file (`restore unit` restores a tree). `Ok(0)` when the
/// catalog holds no file list for the version at all (a catalog that never
/// had one): then nothing can be checked, and the restore says so and goes
/// on, as it always did.
fn catalog_file_size(
    conn: &Connection,
    unit_name: &str,
    selection: &RestoreSelection,
    file_path: &str,
) -> Result<i64> {
    let snapshot_id: i64 = conn.query_row(
        "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
        params![selection.stage_set_id],
        |r| r.get(0),
    )?;
    let known: i64 = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM file_versions WHERE snapshot_id = ?1)",
        params![snapshot_id],
        |r| r.get(0),
    )?;
    if known == 0 {
        warn!(
            unit = unit_name,
            version = selection.version,
            "the catalog holds no file list for this version, so --file cannot be checked \
             before the tape is read"
        );
        return Ok(0);
    }
    let row: Option<(bool, Option<i64>)> = conn
        .query_row(
            "SELECT fv.kind = 0, fv.size_bytes FROM file_versions fv
             WHERE fv.snapshot_id = ?1
               AND fv.path_id = (SELECT id FROM paths
                                  WHERE unit_id = (SELECT unit_id FROM snapshots WHERE id = ?1)
                                    AND path = ?2)",
            params![snapshot_id, file_path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match row {
        None => Err(TapectlError::Other(format!(
            "unit \"{unit_name}\" version {} has no file \"{file_path}\" in the catalog's record \
             of what it archived, so the tape was not touched. A path is relative to the unit's \
             root and matched exactly; `tapectl catalog search \"<words of the name>\" \
             --all-versions` finds one and names the versions that hold it (without \
             `--all-versions` it searches each unit's newest version only), and `tapectl \
             catalog ls {unit_name}` lists the newest version's files.",
            selection.version
        ))),
        Some((true, _)) => Err(TapectlError::Other(format!(
            "\"{file_path}\" is a directory in unit \"{unit_name}\" version {}; `restore file` \
             restores one file. Restore the unit (`tapectl restore unit`) and take the \
             directory from it. Nothing was read from tape.",
            selection.version
        ))),
        Some((false, size)) => Ok(size.unwrap_or(0).max(0)),
    }
}

/// The first entry of `dir`, or `None` when it is empty or does not exist
/// yet. A path that exists and is not a directory is refused.
fn first_entry(dir: &Path) -> Result<Option<String>> {
    match fs::read_dir(dir) {
        Ok(mut entries) => Ok(entries
            .next()
            .transpose()?
            .map(|e| e.file_name().to_string_lossy().into_owned())),
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

/// Where `restore file` puts `file_path`: its last component, directly in
/// the destination (the directory the operator named).
fn placed_path(destination: &Path, file_path: &str) -> PathBuf {
    destination.join(
        Path::new(file_path)
            .file_name()
            .unwrap_or(std::ffi::OsStr::new(file_path)),
    )
}

/// Refuse a restore the disk cannot hold, before the first slice is read
/// (issue #406), for what THIS restore writes ([`RestorePlan`], issue
/// #411):
///
/// - **Streamed** (`restore unit` with its catalogue): nothing in scratch —
///   each slice goes through a named pipe straight into dar — and about the
///   archive's size in the destination.
/// - **Spooled**: every slice read, decrypted, in scratch (a decrypted slice
///   is no larger than its ciphertext, and since #411 no ciphertext copy
///   waits beside it), then about the archive's size again in the
///   destination. On one filesystem: about twice the unit.
/// - **`restore file`**: the slices it reads (only those dar needs, when
///   the catalogue is on disk) plus the one file, extracted in scratch and
///   then moved into place.
///
/// RESTORE.sh's `check_space` (the heir's path, which always spools) asks
/// for the spooled figure plus one slice.
fn check_restore_space(
    destination: &Path,
    scratch: &Path,
    plan: &RestorePlan,
    file_size: Option<i64>,
) -> Result<()> {
    let why = if plan.stream.is_some() {
        "A restore streams its slices into dar, so it needs about the unit's size in --to."
    } else if file_size.is_some() {
        "A restore file decrypts the slices it reads to disk before dar extracts the file \
         from them, so it needs those slices and the file."
    } else {
        "This restore decrypts every slice of the unit to disk before dar extracts them (it \
         streams only when the unit's isolated catalogue from `stage create` is on disk, \
         dar is 2.7.9 or newer and --spool was not given), so with the scratch space and --to \
         on one disk it needs the unit's size about twice over."
    };
    match space_needs(destination, scratch, plan, file_size) {
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

/// What one restore of `plan` needs, per filesystem ([`check_restore_space`]
/// says what and why). `None` when the filesystems cannot be found: then
/// the check is skipped, with a warning.
fn space_needs(
    destination: &Path,
    scratch: &Path,
    plan: &RestorePlan,
    file_size: Option<i64>,
) -> Option<Vec<SpaceNeed>> {
    let read: i64 = plan
        .positions
        .iter()
        .map(|wp| wp.encrypted_bytes.max(0))
        .sum();
    let spooled = if plan.stream.is_some() { 0 } else { read };
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
        (Some(file), true) => vec![need(&dest_fs, dest_dev, 0, spooled + file)],
        // ...and is a copy across two.
        (Some(file), false) => vec![
            need(&scratch_fs, scratch_dev, 0, spooled + file),
            need(&dest_fs, dest_dev, file, 0),
        ],
        (None, true) => vec![need(&dest_fs, dest_dev, read, spooled)],
        (None, false) => vec![
            need(&scratch_fs, scratch_dev, 0, spooled),
            need(&dest_fs, dest_dev, read, 0),
        ],
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
            dar: None,
            dar_version: None,
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
/// corroboration through the `dar` extract.
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
    // decided it before the drive opened: whether the slices stream into dar
    // or spool to scratch, and which slices a `restore file` reads.
    let scratch = scratch_dir(Path::new(target.destination()), options.scratch.as_deref());
    let plan = plan_restore(conn, config, &selection, target, &scratch, options.spool)?;

    // #478: a version read across cartridges needs a person to change them.
    // Refused here, before a key is loaded or a slice read, rather than at
    // the first swap with part of the unit already restored.
    if !supply.attended() {
        let runs = span::runs(&plan.positions);
        if runs
            .iter()
            .any(|r| Some(r.volume_id) != supply.here_volume())
        {
            return Err(span::unattended_refusal(
                conn,
                &format!("unit \"{unit_name}\" version {version}"),
                &runs,
            ));
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
/// dar's report verbatim. Best-effort, like the contact row — the result is
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
        RestoreTarget::Unit { .. } => trace.dar.as_ref().and_then(DarReport::inodes_restored),
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
            dar: trace.dar.as_ref(),
            dar_version: trace.dar_version.as_deref(),
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

/// How one restore gets its slices to dar, and which slices it reads —
/// decided before the drive is opened ([`plan_restore`], issue #411).
#[derive(Debug, Clone)]
pub(crate) struct RestorePlan {
    /// `Some(catalogue)`: stream — each slice is decrypted off the tape into
    /// a named pipe `dar -x --sequential-read -A <catalogue>` reads, and
    /// nothing of the unit is ever on disk outside the destination.
    /// `None`: spool — every slice read is decrypted to a file in scratch,
    /// then dar extracts from those files.
    pub(crate) stream: Option<PathBuf>,
    /// The slices to read, in tape order: every slice of the version, or
    /// for a `restore file` with an isolated catalogue only those dar needs
    /// for the one entry.
    pub(crate) positions: Vec<WritePositionInfo>,
}

/// Decide how a restore of `selection` runs (issue #411), with no tape:
///
/// - **`restore unit` streams** when all of these hold, and spools
///   otherwise: the stage set's own isolated catalogue is on disk
///   ([`own_catalogue`]); the dar is new enough to read a sliced archive
///   sequentially with it ([`dar::restore::STREAMING_MIN_VERSION`]); the
///   slices lie on tape in slice order (dar asks for them in that order,
///   and the tape is read forward); and the scratch directory's filesystem
///   takes a named pipe.
/// - **`restore file` reads only the slices dar needs** for the entry when
///   the catalogue is on disk: the entry's own and its directories'
///   ([`dar::restore::entry_slices`]), plus the archive's last, where dar
///   reads its catalogue. Those are spooled and extracted in dar's direct
///   mode — sequential mode cannot skip a slice. Without a catalogue every
///   slice is read, as before.
/// - **`spool`** (`--spool`, ADR-0012 2026-10-06 item 23) forces a
///   `restore unit` to spool though it could stream. It changes nothing for
///   `restore file`, which never streams.
pub(crate) fn plan_restore(
    conn: &Connection,
    config: &Config,
    selection: &RestoreSelection,
    target: RestoreTarget<'_>,
    scratch: &Path,
    spool: bool,
) -> Result<RestorePlan> {
    let all = selection.positions.clone();
    let Some(catalogue) = own_catalogue(conn, selection.stage_set_id)? else {
        return Ok(RestorePlan {
            stream: None,
            positions: all,
        });
    };
    match target {
        RestoreTarget::Unit { .. } => {
            let stream = if spool {
                info!("--spool: the slices are spooled, not streamed");
                None
            } else {
                streamable(config, &all, scratch).then_some(catalogue)
            };
            Ok(RestorePlan {
                stream,
                positions: all,
            })
        }
        RestoreTarget::File { file_path, .. } => {
            let needed = dar::restore::entry_slices(&config.dar.binary, &catalogue, file_path)
                .unwrap_or_else(|e| {
                    warn!(error = %e, "cannot list the isolated catalogue; reading every slice");
                    None
                });
            let positions = match needed {
                Some(mut needed) => {
                    if let Some(last) = all.iter().map(|wp| wp.slice_number).max() {
                        needed.insert(last);
                    }
                    let subset: Vec<WritePositionInfo> = all
                        .iter()
                        .filter(|wp| needed.contains(&wp.slice_number))
                        .cloned()
                        .collect();
                    // Every slice dar will ask for must be one this volume
                    // carries; otherwise read them all, as before.
                    if subset.len() == needed.len() {
                        subset
                    } else {
                        all
                    }
                }
                None => all,
            };
            Ok(RestorePlan {
                stream: None,
                positions,
            })
        }
    }
}

/// The isolated dar catalogue `stage create` made for this stage set's OWN
/// archive, when it is still on disk — `None` otherwise.
///
/// `stage create` isolates a catalogue once per snapshot and records the
/// same `catalog_path` on every later stage set of it (issue #419, ruled
/// to become one per stage set). A later stage set is a later `dar -c` run,
/// and dar refuses (FATAL, measured on 2.7.13) to read an archive with a
/// catalogue isolated from a different run, even of the same content — its
/// slicing may differ too. So a catalogue counts only for the stage set
/// that made it: the FIRST stage set of the snapshot to record that path.
/// Once #419 gives each stage set its own path, each is its own first. A
/// catalog rebuilt from a tape records the catalogue the tape carried, when
/// it carried one (#473).
fn own_catalogue(conn: &Connection, stage_set_id: i64) -> Result<Option<PathBuf>> {
    let row: Option<(Option<String>, Option<i64>)> = conn
        .query_row(
            "SELECT ss.catalog_path,
                    (SELECT MIN(o.id) FROM stage_sets o
                     WHERE o.snapshot_id = ss.snapshot_id AND o.catalog_path = ss.catalog_path)
             FROM stage_sets ss WHERE ss.id = ?1",
            params![stage_set_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((Some(base), Some(owner))) = row else {
        return Ok(None);
    };
    if owner != stage_set_id {
        info!(
            stage_set = stage_set_id,
            catalogue_of = owner,
            "the isolated catalogue on disk belongs to another staging of this version; not used"
        );
        return Ok(None);
    }
    let base = PathBuf::from(base);
    let first = PathBuf::from(format!("{}.1.dar", base.display()));
    Ok(first.is_file().then_some(base))
}

/// Whether a `restore unit` with its catalogue can stream (see
/// [`plan_restore`]), each refusal said once in the log.
fn streamable(config: &Config, positions: &[WritePositionInfo], scratch: &Path) -> bool {
    match dar::version::check(&config.dar.binary) {
        Ok(v) if dar::restore::supports_streaming(&v) => {}
        Ok(v) => {
            info!(
                dar = %v.full_string,
                "this dar cannot read a sliced archive sequentially; the slices are spooled"
            );
            return false;
        }
        Err(_) => return false,
    }
    // In slice order, and on each cartridge in tape order: a version read
    // across cartridges (#478) starts again near the front of the next one.
    let in_slice_order = positions.windows(2).all(|w| {
        w[0].slice_number < w[1].slice_number
            && (w[0].volume_id != w[1].volume_id
                || w[0].position.parse::<u32>().ok() < w[1].position.parse::<u32>().ok())
    });
    if !in_slice_order {
        info!("the slices are not on tape in slice order; they are spooled");
        return false;
    }
    if !takes_a_named_pipe(scratch) {
        info!(
            scratch = %scratch.display(),
            "the scratch directory's filesystem does not take a named pipe; the slices are spooled"
        );
        return false;
    }
    true
}

/// Whether the filesystem the scratch directory will be made on takes a
/// named pipe: one is made beside where it will be, and removed.
fn takes_a_named_pipe(scratch: &Path) -> bool {
    let Some(dir) = scratch.parent().and_then(existing) else {
        return false;
    };
    let probe = dir.join(format!(".tapectl-fifo-probe-{}", std::process::id()));
    let made = nix::unistd::mkfifo(&probe, nix::sys::stat::Mode::S_IRUSR).is_ok();
    let _ = fs::remove_file(&probe);
    made
}

/// Run `plan` against `store`: make the scratch directory (removed on every
/// way out — [`RestoreScratch`]), get the slices to dar — streamed or
/// spooled — and, for `restore file`, place the one entry.
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
    // Scratch: inside the destination or `--scratch`, never $TMPDIR
    // ([`scratch_dir`], issue #406). Named in the log first: if this process
    // is killed, that line says where decrypted data may be left.
    fs::create_dir_all(scratch).map_err(|e| {
        TapectlError::Other(format!(
            "cannot create the restore scratch directory {}: {e}",
            scratch.display()
        ))
    })?;
    let _scratch = RestoreScratch(scratch.to_path_buf());
    let archive_base = scratch.join("restore");
    // The holes this restore writes are the supply's from here on.
    let damage_before = supply.damage.len();
    // Which cartridge each slice is read from, for the prompt that asks for
    // the next one of a version read across cartridges (#478).
    supply.set_route(&plan.positions);
    let extracted = scratch.join("extract");

    let streamed =
        if let (Some(catalogue), RestoreTarget::Unit { dest_dir }) = (&plan.stream, target) {
            info!(scratch = %scratch.display(), "slices stream to dar through named pipes here");
            Some((
                stream_into_dar(
                    config,
                    unit_name,
                    &plan.positions,
                    catalogue,
                    &archive_base,
                    Path::new(dest_dir),
                    options,
                    identities,
                    supply,
                    trace,
                )?,
                PathBuf::from(dest_dir),
            ))
        } else {
            None
        };

    let (verdict, root) = match streamed {
        Some(streamed) => streamed,
        None => spool_and_extract(
            config,
            unit_name,
            plan,
            target,
            options,
            scratch,
            &archive_base,
            &extracted,
            identities,
            supply,
            trace,
        )?,
    };

    // What dar left, what it could not make of the holes, and — when a copy
    // of the same version holds the rest — the files filled from it
    // (issue #480, ADR-0012 items 101 and 102).
    let holes = supply.damage[damage_before..].to_vec();
    let ex = fill::judge(trace, verdict, root, holes)?;
    let ctx = fill::Finish {
        conn,
        config,
        identities,
        unit_name,
        version,
        stage_set_id: plan
            .positions
            .first()
            .map(|wp| wp.stage_set_id)
            .unwrap_or_default(),
        target,
        scratch,
    };
    trace.filled = fill::finish(&ctx, supply, &ex)?;

    // `restore file`: place the one requested entry, inside the recorded
    // span (see `RestoreTarget`).
    if let RestoreTarget::File {
        file_path,
        dest_dir: file_dest,
    } = target
    {
        place_one_entry(&extracted, file_path, Path::new(file_dest))?;
        trace.placed = true;
    }
    Ok(())
}

/// The spooled half of [`restore_planned`]: every slice to a file in
/// scratch, then dar's extract from them. Returns dar's verdict and where it
/// wrote.
#[allow(clippy::too_many_arguments)]
fn spool_and_extract(
    config: &Config,
    unit_name: &str,
    plan: &RestorePlan,
    target: RestoreTarget<'_>,
    options: &RestoreOptions,
    scratch: &Path,
    archive_base: &Path,
    extracted: &Path,
    identities: &[age::x25519::Identity],
    supply: &mut Supply<'_, '_, '_>,
    trace: &mut RestoreTrace,
) -> Result<(Result<()>, PathBuf)> {
    info!(scratch = %scratch.display(), "decrypted slices wait for dar here");
    // Issue #386: the tape read and decrypt, counted as ciphertext off the
    // tape, then dar's extract as a phase of its own.
    let phase = progress::phase("read", Some(ciphertext_bytes(&plan.positions)));
    for (i, wp) in plan.positions.iter().enumerate() {
        note_slice(&phase, unit_name, i, plan.positions.len(), wp);
        // Issue #404: between slices — nothing has been extracted into the
        // destination yet.
        crate::signal::check(|| {
            format!(
                "restore of \"{unit_name}\" stopped after reading {i} of {} slices, before \
                 anything was extracted; run the restore again",
                plan.positions.len()
            )
        })?;
        // dar expects: basename.N.dar
        let slice_path = scratch.join(format!("restore.{}.dar", wp.slice_number));
        let read = supply.read_slice(wp, identities, SliceSink::File(&slice_path))?;
        trace.slices_read += 1;
        trace.bytes_decrypted += read.authentic_bytes as i64;
    }
    phase.done();

    // Run dar extract. Its report is kept whenever it ran, on both verdicts
    // (issue #306); the version is read only once dar has actually run, so
    // a restore that never reached dar records no version either.
    let phase = progress::phase("extract", None);
    phase.item(unit_name.to_string());
    // `restore unit` extracts the whole archive into the destination;
    // `restore file` asks dar for its one entry (`-g`, issue #406) into
    // scratch, and places it after the fill.
    let (report, verdict) = match target {
        RestoreTarget::Unit { dest_dir } => {
            info!("extracting dar archive to {dest_dir}");
            dar::restore::extract_reported(
                &config.dar.binary,
                archive_base,
                Path::new(dest_dir),
                options.overwrite,
            )
        }
        RestoreTarget::File { file_path, .. } => {
            info!("extracting \"{file_path}\" from the dar archive");
            dar::restore::extract_file_reported(
                &config.dar.binary,
                archive_base,
                file_path,
                extracted,
            )
        }
    };
    record_dar(config, trace, report);
    phase.done();
    let root = match target {
        RestoreTarget::Unit { dest_dir } => PathBuf::from(dest_dir),
        RestoreTarget::File { .. } => extracted.to_path_buf(),
    };
    Ok((verdict, root))
}

fn ciphertext_bytes(positions: &[WritePositionInfo]) -> u64 {
    positions
        .iter()
        .map(|wp| wp.encrypted_bytes.max(0) as u64)
        .sum()
}

fn note_slice(
    phase: &progress::Phase,
    unit_name: &str,
    i: usize,
    total: usize,
    wp: &WritePositionInfo,
) {
    phase.item(format!(
        "{unit_name} slice {} of {total} (file {})",
        i + 1,
        wp.position
    ));
    info!(
        slice = wp.slice_number,
        n = i + 1,
        total,
        tape_pos = %wp.position,
        "reading slice from tape"
    );
}

/// Keep dar's report, and its version once it has run (issue #306).
fn record_dar(config: &Config, trace: &mut RestoreTrace, report: Option<DarReport>) {
    if report.is_some() {
        trace.dar_version = dar::version::check(&config.dar.binary)
            .ok()
            .map(|v| v.full_string);
    }
    trace.dar = report;
}

/// The streamed restore (issue #411): a named pipe per slice in scratch,
/// `dar -x --sequential-read -A <catalogue>` reading them in slice order,
/// and each slice decrypted off the tape into its pipe as the tape is read
/// — no decrypted byte is ever on disk outside the destination, and the
/// drive streams while dar extracts (the tape read runs ahead of the
/// decrypt by the read pipeline's queue, issue #390).
///
/// dar extracts AS the slices arrive, so a failure partway — a slice that
/// fails its checksum, a tape read error — leaves what dar had extracted
/// by then in the destination. The error says so.
///
/// Given the catalogue, dar stops reading where the file data ends and exits
/// 0, leaving the archive's own trailing catalogue unread (#497,
/// [`dar::restore::SequentialExtract`]). Every slice is read and
/// authenticated to its end all the same, what dar does not take is
/// dropped, and dar's exit status, not the pipe it closed, says whether
/// the restore failed.
#[allow(clippy::too_many_arguments)]
fn stream_into_dar(
    config: &Config,
    unit_name: &str,
    positions: &[WritePositionInfo],
    catalogue: &Path,
    archive_base: &Path,
    dest: &Path,
    options: &RestoreOptions,
    identities: &[age::x25519::Identity],
    supply: &mut Supply<'_, '_, '_>,
    trace: &mut RestoreTrace,
) -> Result<Result<()>> {
    // Every pipe exists before dar starts: dar opens slice 1, then each
    // next slice as it finishes the one before.
    let pipes: Vec<PathBuf> = positions
        .iter()
        .map(|wp| {
            PathBuf::from(format!(
                "{}.{}.dar",
                archive_base.display(),
                wp.slice_number
            ))
        })
        .collect();
    for pipe in &pipes {
        nix::unistd::mkfifo(
            pipe,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .map_err(|e| {
            TapectlError::Other(format!(
                "cannot make the named pipe {}: {e}",
                pipe.display()
            ))
        })?;
    }
    info!(
        "extracting dar archive to {} as its slices are read",
        dest.display()
    );
    let darx = dar::restore::SequentialExtract::spawn(
        &config.dar.binary,
        archive_base,
        catalogue,
        dest,
        options.overwrite,
    )?;

    let phase = progress::phase("read", Some(ciphertext_bytes(positions)));
    let mut failure = None;
    let damage_before = supply.damage.len();
    // The first slice dar stopped reading before its end (or had ended
    // before it opened), with no hole to blame. Given the catalogue, dar
    // stops where the file data ends and exits 0 (#497): that is dar done,
    // and every slice is still read and checked. Any other exit is dar
    // failed, named at this slice as ever.
    let mut cut_at: Option<i64> = None;
    for (i, (wp, pipe)) in positions.iter().zip(&pipes).enumerate() {
        note_slice(&phase, unit_name, i, positions.len(), wp);
        // Issue #404: between slices. dar has been extracting as the slices
        // arrived, so a stop here leaves a partial restore — said below.
        if let Err(e) = crate::signal::check(|| {
            format!(
                "restore of \"{unit_name}\" stopped after reading {i} of {} slices",
                positions.len()
            )
        }) {
            failure = Some(e);
            break;
        }
        let sink = SliceSink::Pipe {
            path: pipe,
            dar: &darx,
        };
        match supply.read_slice(wp, identities, sink) {
            Ok(read) => {
                trace.slices_read += 1;
                trace.bytes_decrypted += read.authentic_bytes as i64;
                if read.dar_gone {
                    // dar stopped reading. If a hole is why (a slice nobody
                    // could give whole, written as zeros over something dar
                    // needs), the later slices are of no use to it, and the
                    // restore is judged by what it left.
                    if supply.damage.len() > damage_before {
                        break;
                    }
                    // Otherwise dar's exit status says why: 0 is dar done,
                    // and the slices left are read and checked, into
                    // nothing; any other is dar dying of its own accord, and
                    // that is the failure. Still running, the next slice's
                    // pipe or the end of the stream will tell.
                    let cut = *cut_at.get_or_insert(wp.slice_number);
                    if darx.status().is_some_and(|s| !s.success()) {
                        failure = Some(TapectlError::Dar(format!(
                            "dar stopped reading slice {cut}"
                        )));
                        break;
                    }
                }
            }
            Err(e) => {
                // A dar that stopped reading a slice and then failed is
                // named at that slice.
                failure = Some(match (cut_at, e) {
                    (Some(cut), TapectlError::Dar(_)) => {
                        TapectlError::Dar(format!("dar stopped reading slice {cut}"))
                    }
                    (_, e) => e,
                });
                break;
            }
        }
    }
    if failure.is_some() {
        // dar may be waiting to open the next slice's pipe; it would wait
        // forever.
        darx.kill();
    } else {
        phase.done();
        if supply.damage.len() > damage_before {
            settle_dar(&darx, &pipes, Some(SETTLE_PATIENCE));
        } else if cut_at.is_some() {
            // dar stopped reading before the end and may still be running:
            // it is let end, for as long as that takes (a pipe it waits on
            // is closed under it), and its exit status decides.
            settle_dar(&darx, &pipes, None);
        }
    }
    let (report, verdict) = darx.finish();
    if let (None, Some(cut)) = (&failure, cut_at) {
        if supply.damage.len() == damage_before
            && report.as_ref().is_some_and(|r| r.exit_code != Some(0))
        {
            failure = Some(TapectlError::Dar(format!(
                "dar stopped reading slice {cut}"
            )));
        }
    }
    record_dar(config, trace, report);
    if let Some(e) = failure {
        let partial = |what: String| {
            format!(
                "{what}. dar was extracting the unit into {} as its slices arrived, so that \
                 directory may now hold a partial restore: empty it (or restore again with \
                 --overwrite) before trusting what is there",
                dest.display()
            )
        };
        // A stop by signal (#404) stays one, so it exits as one.
        return Err(match e {
            TapectlError::Interrupted(what) => TapectlError::Interrupted(partial(what)),
            other => TapectlError::Other(partial(other.to_string())),
        });
    }
    Ok(verdict)
}

/// How long dar is given to end once every slice has been fed to it and a
/// hole was written into one.
const SETTLE_PATIENCE: std::time::Duration = std::time::Duration::from_secs(30);

/// Let a dar that is still running after a restore with holes end. A hole
/// over something dar needs (a slice's header) can leave it waiting to
/// re-open a slice's pipe that nobody is feeding; a pipe held open for it
/// and closed at once gives it the end of file it needs to fail and exit.
/// A dar that is still there after `patience` ([`SETTLE_PATIENCE`]) is
/// stopped: what it left is judged as ever, by the files. With no patience
/// it is waited for as long as it runs: a dar that stopped reading a slice
/// early with no hole to blame (#497) is judged by its own exit status.
fn settle_dar(
    darx: &dar::restore::SequentialExtract,
    pipes: &[PathBuf],
    patience: Option<std::time::Duration>,
) {
    use std::os::unix::fs::OpenOptionsExt;
    let started = std::time::Instant::now();
    while !darx.exited() {
        if patience.is_some_and(|p| started.elapsed() > p) {
            warn!("dar did not end after the holes; stopping it");
            darx.kill();
            return;
        }
        for pipe in pipes {
            // Succeeds only while dar is waiting to open it; dropped at once.
            let _ = fs::OpenOptions::new()
                .write(true)
                .custom_flags(nix::libc::O_NONBLOCK)
                .open(pipe);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Open the named pipe dar will read a slice from, for writing — once dar
/// has opened it for reading. A plain blocking open would wait forever on a
/// dar that has already given up, so this opens non-blocking (which fails
/// with ENXIO while no reader is there), asking between tries whether dar
/// is still running, then makes the descriptor blocking for the copy.
fn open_pipe_for_dar(
    path: &Path,
    darx: &dar::restore::SequentialExtract,
    slice_number: i64,
) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    loop {
        match fs::OpenOptions::new()
            .write(true)
            .custom_flags(nix::libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => {
                nix::fcntl::fcntl(
                    file.as_raw_fd(),
                    nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::empty()),
                )
                .map_err(|e| TapectlError::Other(format!("named pipe {}: {e}", path.display())))?;
                return Ok(file);
            }
            Err(e) if e.raw_os_error() == Some(nix::libc::ENXIO) => {
                if darx.exited() {
                    return Err(TapectlError::Dar(format!(
                        "dar stopped before it read slice {slice_number}"
                    )));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => {
                return Err(TapectlError::Other(format!(
                    "cannot open the named pipe {}: {e}",
                    path.display()
                )))
            }
        }
    }
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
    // `dar -x -g` of the one entry into the restore's scratch directory, then
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
        }
    }
}

/// [`restore_units`]'s half before the tape: every unit planned and
/// checked, the set's space checked, the units sorted into tape order.
fn plan_units(
    conn: &Connection,
    config: &Config,
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
        // A version this tape holds only a range of is refused by name
        // (#478, ADR-0012 item 89): `restore volume` restores what one tape
        // holds whole.
        let selection = select(conn, &req.unit, volume_label, req.version, Across::Refuse)?;
        let target = RestoreTarget::Unit {
            dest_dir: &req.dest_dir,
        };
        let scratch = scratch_dir(Path::new(&req.dest_dir), options.scratch.as_deref());
        let plan = plan_restore(conn, config, &selection, target, &scratch, options.spool)?;
        preflight_checks(conn, &req.unit, &selection, target, &scratch, options)?;
        if let Some(n) = space_needs(Path::new(&req.dest_dir), &scratch, &plan, None) {
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
            "Restoring these units one after another keeps every restored unit and the largest \
             unit's spooled slices (a unit whose isolated catalogue is on disk streams and \
             spools nothing, unless --spool).",
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
/// fact been restored correctly by dar.
///
/// A rename when the scratch directory and `dest_dir` share a filesystem —
/// the default, scratch inside `--to` — so a large file is never written
/// twice (issue #406's space check counts on it); a copy otherwise.
fn place_one_entry(extracted: &Path, file_path: &str, dest_dir: &Path) -> Result<()> {
    let source_file = extracted.join(file_path);
    let meta = fs::symlink_metadata(&source_file).map_err(|_| {
        TapectlError::Other(format!("file \"{file_path}\" not found in restored unit"))
    })?;

    let dest = placed_path(dest_dir, file_path);
    fs::create_dir_all(dest_dir)?;
    if meta.is_dir() || fs::rename(&source_file, &dest).is_err() {
        place_restored_entry(&source_file, &meta, &dest)?;
    }

    info!(file = file_path, dest = %dest.display(), "file restored");
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
/// which lets dar do the extraction — preserved it. Same archive, same entry,
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
}

/// One slice of the selected version, as restore reads it off the volume.
#[derive(Debug, Clone)]
pub struct WritePositionInfo {
    /// The stage set this slice belongs to — one stage set is one snapshot
    /// version's complete dar archive (issue #315).
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

/// What [`select`] does with a version the volume holds only a range of
/// (#478, `volume-format-v2.md` §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Across {
    /// Plan its restore across cartridges ([`span::plan_across`]): `restore
    /// unit` and `restore file` (ADR-0012 item 131).
    Plan,
    /// Refuse it by name: `restore volume` restores the units one tape holds
    /// whole (item 89).
    Refuse,
}

/// Select the ONE snapshot version of `unit_name` to restore from
/// `volume_label`, and return its slices (issue #315).
///
/// A volume can carry the same unit in several snapshot versions — every
/// staged stage set rides the same write — and the query this replaced had
/// no version filter at all: it returned every version's slices, each was
/// decrypted to `restore.{slice_number}.dar`, and a later version's slice N
/// overwrote an earlier one's, handing `dar` a mix. RESTORE.sh had already
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
    select(conn, unit_name, volume_label, want, Across::Plan)
}

/// [`select_write_positions`], with what to do about a range ([`Across`]).
fn select(
    conn: &Connection,
    unit_name: &str,
    volume_label: &str,
    want: Option<i64>,
    across: Across,
) -> Result<RestoreSelection> {
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;

    // Every (version, stage set) of this unit on this volume — newest
    // version first, later stage set first within one — whose write is a
    // copy here (`completed`, with written slices), a range of a version
    // held across cartridges (#478, `range`, with written slices), or was
    // cut by the early end of the tape (#420, `cut`). A cut version is
    // picked as RESTORE.sh picks it (the newest on the tape, or `--version`)
    // and then refused by name: it is not on this tape whole
    // (`volume-format-v2.md` §4.2 R4). A range is picked the same way and
    // read across cartridges (§4.4).
    let mut stmt = conn.prepare(
        "SELECT DISTINCT s.version, ss.id, w.status, v.ended_after
         FROM writes w
         JOIN stage_sets ss ON ss.id = w.stage_set_id
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN volumes v ON v.id = w.volume_id
         WHERE s.unit_id = ?1 AND v.label = ?2
           AND (w.status = 'cut'
                OR (w.status IN ('completed', 'range')
                    AND EXISTS (SELECT 1 FROM write_positions wp
                                 WHERE wp.write_id = w.id AND wp.status = 'written')))
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
    if status == "cut" {
        return Err(cut_refusal(
            conn,
            unit_name,
            unit.id,
            volume_label,
            version,
            stage_set_id,
            &versions_on_volume,
        )?);
    }

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
        match across {
            Across::Refuse => {
                return Err(range_refusal(
                    conn,
                    unit_name,
                    unit.id,
                    volume_label,
                    version,
                    stage_set_id,
                    &positions,
                )?)
            }
            Across::Plan => span::plan_across(
                conn,
                unit_name,
                version,
                stage_set_id,
                volume_label,
                positions,
            )?,
        }
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

/// Where a version held across cartridges is restored from (#478): a volume
/// in service that holds it whole, or else the live holder of its slice 1,
/// which may be `volume_id` itself. `None` when no live volume holds slice 1.
fn restore_from(
    conn: &Connection,
    unit_id: i64,
    version: i64,
    stage_set_id: i64,
    volume_id: Option<i64>,
) -> Result<Option<String>> {
    if let Some(whole) = crate::policy::coverage::in_service_copy_of_version_excluding(
        conn, unit_id, version, volume_id,
    )? {
        return Ok(Some(whole));
    }
    slice_one_holder(conn, stage_set_id)
}

/// The first live holder of `stage_set_id`'s slice 1, in-service first
/// ([`crate::volume::slice_holders::holders_of`]): where a restore of it
/// across cartridges starts (#478).
fn slice_one_holder(conn: &Connection, stage_set_id: i64) -> Result<Option<String>> {
    let first: Option<String> = conn
        .query_row(
            "SELECT sha256_encrypted FROM stage_slices
              WHERE stage_set_id = ?1 AND slice_number = 1",
            params![stage_set_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(match first {
        Some(sha) => crate::volume::slice_holders::holders_of(conn, &sha)?
            .into_iter()
            .next()
            .map(|h| h.label),
        None => None,
    })
}

/// The refusal `restore volume` gives a version `volume_label` holds only a
/// range of (#478, ADR-0012 item 89): it restores the units a tape holds
/// whole, and a range restores only with the version's other slices
/// (`volume-format-v2.md` §4.4 R7). Names the run this tape holds and the
/// `restore unit` that reads the version across cartridges.
fn range_refusal(
    conn: &Connection,
    unit_name: &str,
    unit_id: i64,
    volume_label: &str,
    version: i64,
    stage_set_id: i64,
    here: &[WritePositionInfo],
) -> Result<TapectlError> {
    let volume_id = here.first().map(|wp| wp.volume_id);
    let run = match (
        here.iter().map(|wp| wp.slice_number).min(),
        here.iter().map(|wp| wp.slice_number).max(),
    ) {
        (Some(first), Some(last)) => span::slices_phrase(first, last),
        _ => "some slices".to_string(),
    };
    let how = match restore_from(conn, unit_id, version, stage_set_id, volume_id)? {
        Some(from) => format!(
            "restore it across cartridges with `tapectl restore unit --unit {unit_name} --from \
             {from} --version {version} --to <DIR>`"
        ),
        None => "no live volume holds its slice 1, so it cannot be restored from this \
                 catalog's tapes yet"
            .to_string(),
    };
    Ok(TapectlError::Other(format!(
        "unit \"{unit_name}\" version {version} is not on volume \"{volume_label}\" whole: this \
         tape holds only {run} of it, a range of a version held across cartridges \
         (volume-format-v2.md §4.4). `restore volume` restores the units a tape holds whole; \
         {how}. Nothing was read from tape."
    )))
}

/// `volumes.ended_after` of `volume_label`: where its tape ended, when it
/// sealed short (#420). `None` for a full seal and for an unknown label.
fn ended_after_of(conn: &Connection, volume_label: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT ended_after FROM volumes WHERE label = ?1",
            params![volume_label],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten())
}

/// The refusal for a version the early end of `volume_label`'s tape cut
/// (#420; `volume-format-v2.md` §4.2 R4: name it, never restore it from this
/// tape, say where its copies are). Where: a volume in service that holds
/// the same version whole, from the catalog
/// ([`crate::policy::coverage::in_service_copy_of_version_excluding`], this
/// volume excluded), or, when none does, that it is still to be written.
fn cut_refusal(
    conn: &Connection,
    unit_name: &str,
    unit_id: i64,
    volume_label: &str,
    version: i64,
    stage_set_id: i64,
    whole_here: &[i64],
) -> Result<TapectlError> {
    let volume_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            params![volume_label],
            |r| r.get(0),
        )
        .optional()?;
    let ended_after = ended_after_of(conn, volume_label)?;
    let last: Option<i64> = conn.query_row(
        "SELECT MAX(CAST(wp.position AS INTEGER)) FROM write_positions wp
           JOIN writes w ON w.id = wp.write_id
          WHERE w.stage_set_id = ?1 AND w.volume_id = ?2",
        params![stage_set_id, volume_id],
        |r| r.get(0),
    )?;
    let holder = crate::policy::coverage::in_service_copy_of_version_excluding(
        conn, unit_id, version, volume_id,
    )?;
    let ended = match (ended_after, last) {
        (Some(k), Some(last)) => format!(
            "the tape ended early, after tape file {k} (its seal says so), and the slices of \
             this version run to tape file {last}"
        ),
        (Some(k), None) => format!("the tape ended early, after tape file {k}"),
        _ => "the tape ended early".to_string(),
    };
    let elsewhere = match holder {
        Some(label) => format!(
            "volume \"{label}\" holds it whole: `tapectl restore unit --unit {unit_name} --from \
             {label} --version {version} --to <DIR>`"
        ),
        None => "no volume this catalog knows holds it whole yet: it is written whole by an \
                 ordinary `tapectl volume write` onto another cartridge while it is still \
                 staged, or `tapectl catalog rebuild --from-volume` of the tape that holds it \
                 brings that tape into the catalog"
            .to_string(),
    };
    let older = if whole_here.is_empty() {
        String::new()
    } else {
        format!(
            " Version(s) {} of it are whole on \"{volume_label}\": pass one with --version.",
            whole_here
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Ok(TapectlError::Other(format!(
        "unit \"{unit_name}\" version {version} is not on volume \"{volume_label}\" whole: \
         {ended}. It was cut by the end of the tape and is never restored from it \
         (volume-format-v2.md §4.2); {elsewhere}.{older} Nothing was read from tape."
    )))
}

/// One unit a volume does not hold whole though its tape carries some of it:
/// one the early end of the tape cut (#420), or one of which the tape holds
/// only a range, a version held across cartridges (#478). What `restore
/// volume` names instead of restoring it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CutUnit {
    pub unit_name: String,
    pub version: i64,
    /// A volume in service that holds this version whole, if the catalog
    /// knows one.
    pub whole_on: Option<String>,
    /// For a range (#478): the first and last slice of the version this tape
    /// holds. `None` for a unit the end of the tape cut.
    pub range: Option<(i64, i64)>,
    /// For a range: a live volume holding the version's slice 1, where a
    /// restore across cartridges starts (`volume-format-v2.md` §4.4), this
    /// one included. `None` for a cut unit, or when no live volume holds it.
    pub starts_on: Option<String>,
}

impl CutUnit {
    /// `"bravo" v1, whole on "L6-0002"`, or where none is known, that it is
    /// still to be written; for a range, the slices this tape holds and where
    /// the version is whole or starts.
    pub fn describe(&self) -> String {
        let whole = match &self.whole_on {
            Some(label) => format!("whole on \"{label}\""),
            None => "whole on no volume this catalog knows yet".to_string(),
        };
        match self.range {
            None => format!("\"{}\" v{}, {whole}", self.unit_name, self.version),
            Some((first, last)) => format!(
                "\"{}\" v{}, {} of it on this tape, {}",
                self.unit_name,
                self.version,
                span::slices_phrase(first, last),
                match (&self.whole_on, &self.starts_on) {
                    (Some(_), _) => whole,
                    (None, Some(label)) =>
                        format!("held across cartridges from its slice 1 on \"{label}\""),
                    (None, None) => "and its slice 1 is on no live volume".to_string(),
                }
            ),
        }
    }

    /// The volume `restore unit --from` reads this version from: a whole
    /// copy, or for a range the holder of its slice 1.
    pub fn restore_from(&self) -> Option<&str> {
        self.whole_on.as_deref().or(self.starts_on.as_deref())
    }
}

/// Every unit version recorded `cut` on `volume_label` (#420), or held
/// there only as a range of a version held across cartridges (#478), in the
/// order it was planned on the tape, each with a volume that holds it whole
/// when the catalog knows one, and for a range the slices this tape holds
/// and the live holder of its slice 1. Empty for a tape that sealed full
/// with nothing of a span on it. `restore volume` names these: the tape does
/// not hold them whole, so they are not in [`units_on_volume`]'s set
/// (`volume-format-v2.md` §4.2, "`restore volume` names the cut units", and
/// §4.4, ADR-0012 item 89).
pub fn cut_units_on_volume(conn: &Connection, volume_label: &str) -> Result<Vec<CutUnit>> {
    let mut stmt = conn.prepare(
        "SELECT u.id, u.name, s.version, v.id, w.status = 'range', ss.id,
                (SELECT MIN(CAST(wp.position AS INTEGER)) FROM write_positions wp
                  WHERE wp.write_id = w.id) AS first,
                (SELECT MIN(sl.slice_number) FROM write_positions wp
                   JOIN stage_slices sl ON sl.id = wp.stage_slice_id
                  WHERE wp.write_id = w.id AND wp.status = 'written'),
                (SELECT MAX(sl.slice_number) FROM write_positions wp
                   JOIN stage_slices sl ON sl.id = wp.stage_slice_id
                  WHERE wp.write_id = w.id AND wp.status = 'written')
         FROM writes w
         JOIN stage_sets ss ON ss.id = w.stage_set_id
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN units u ON u.id = s.unit_id
         JOIN volumes v ON v.id = w.volume_id
         WHERE v.label = ?1
           AND (w.status = 'cut'
                OR (w.status = 'range'
                    AND EXISTS (SELECT 1 FROM write_positions wp
                                 WHERE wp.write_id = w.id AND wp.status = 'written')))
         ORDER BY first, u.name, s.version",
    )?;
    type Row = (i64, String, i64, i64, bool, i64, Option<i64>, Option<i64>);
    let rows = stmt
        .query_map(params![volume_label], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, bool>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Option<i64>>(7)?,
                r.get::<_, Option<i64>>(8)?,
            ))
        })?
        .collect::<std::result::Result<Vec<Row>, _>>()?;
    rows.into_iter()
        .map(
            |(unit_id, unit_name, version, volume_id, range, stage_set_id, first, last)| {
                let whole_on = crate::policy::coverage::in_service_copy_of_version_excluding(
                    conn,
                    unit_id,
                    version,
                    Some(volume_id),
                )?;
                let (range, starts_on) = match (range, first, last) {
                    (true, Some(first), Some(last)) => {
                        (Some((first, last)), slice_one_holder(conn, stage_set_id)?)
                    }
                    _ => (None, None),
                };
                Ok(CutUnit {
                    whole_on,
                    range,
                    starts_on,
                    unit_name,
                    version,
                })
            },
        )
        .collect()
}

/// Every unit with a live copy on `volume_label` — written slices of a
/// completed write, the predicate [`select_write_positions`] reads by, so
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
         WHERE v.label = ?1 AND w.status = 'completed' AND wp.status = 'written'
         GROUP BY u.id
         ORDER BY first, u.name",
    )?;
    let names = stmt
        .query_map(params![volume_label], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if names.is_empty() {
        // #420: a tape that ended inside its first unit holds none whole.
        // #478: nor does a tape that holds only ranges of versions held
        // across cartridges.
        let cut = cut_units_on_volume(conn, volume_label)?;
        let list = |units: &[&CutUnit]| {
            units
                .iter()
                .map(|c| c.describe())
                .collect::<Vec<_>>()
                .join("; ")
        };
        let (ranges, cuts): (Vec<&CutUnit>, Vec<&CutUnit>) =
            cut.iter().partition(|c| c.range.is_some());
        if !cut.is_empty() && ranges.is_empty() {
            return Err(TapectlError::Other(format!(
                "volume \"{volume_label}\" holds no unit whole: its tape ended early, and every \
                 unit on it was cut by the end of the tape ({}). Nothing was read from tape.",
                list(&cuts)
            )));
        }
        if !ranges.is_empty() {
            let cut_part = if cuts.is_empty() {
                String::new()
            } else {
                format!(
                    "; its tape ended early, and the end of the tape cut the others ({})",
                    list(&cuts)
                )
            };
            return Err(TapectlError::Other(format!(
                "volume \"{volume_label}\" holds no unit whole: it holds only a range of \
                 versions held across cartridges ({}), each restored across cartridges with \
                 `tapectl restore unit`{cut_part}. Nothing was read from tape.",
                list(&ranges)
            )));
        }
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
/// to a file ahead of the head and rewinds for one behind it. dar does not
/// care — each slice is named `restore.{slice_number}.dar` whatever order
/// it arrives in.
fn get_write_positions(
    conn: &Connection,
    stage_set_id: i64,
    volume_label: &str,
) -> Result<Vec<WritePositionInfo>> {
    let mut stmt = conn.prepare(
        "SELECT sl.slice_number, wp.position, sl.sha256_plain, sl.sha256_encrypted, sl.encrypted_bytes,
                sl.stage_set_id, v.id
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
        sink: SliceSink<'_>,
    ) -> Result<u64> {
        let conn = crate::db::open_memory().unwrap();
        let site = site(Operation::RestoreUnit);
        let guard = site.open(&conn, None);
        let mut bay = FixedBay(store);
        let mut supply = Supply::start(&conn, &mut bay, guard, None, "TEST", site);
        let r = supply
            .read_slice(wp, identities, sink)
            .map(|read| read.authentic_bytes);
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
        fs::write(scratch.join("restore.1.dar"), b"decrypted archive bytes").unwrap();

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
            fs::write(scratch.join("restore.1.dar"), b"decrypted archive bytes")?;
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

        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("out.dar");

        let plain_size = read_slice(&mut store, &wp, &[identity], SliceSink::File(&out)).unwrap();

        assert_eq!(plain_size, plaintext.len() as u64);
        let restored = fs::read(&out).unwrap();
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

        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("out.dar");

        let err = read_slice(&mut store, &wp, &[identity], SliceSink::File(&out)).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("checksum mismatch"), "got: {msg}");
        assert!(
            msg.contains(&wp.slice_number.to_string()),
            "error must name the slice, got: {msg}"
        );
        assert!(
            !out.exists(),
            "no partial plaintext should be left behind on a failed restore"
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

        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("out.dar");

        // The right identity is SECOND in the list, proving the single
        // `decrypt()` call tries every identity (age's `obtain_payload_key`
        // does `find_map` over the header internally) rather than only ever
        // succeeding when the match happens to come first.
        let identities = vec![wrong_identity, right_identity];
        let plain_size = read_slice(&mut store, &wp, &identities, SliceSink::File(&out)).unwrap();

        assert_eq!(plain_size, plaintext.len() as u64);
        assert_eq!(fs::read(&out).unwrap(), plaintext);
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

        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("out.dar");

        let plain_size = read_slice(&mut store, &wp, &[identity], SliceSink::File(&out)).unwrap();
        assert_eq!(plain_size, plaintext.len() as u64);
        assert_eq!(fs::read(&out).unwrap(), plaintext);
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
    /// hat: dar had restored it correctly and the command denied it was there.
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
        /// the scratch-dir naming (`restore.{slice_number}.dar`) collapsed
        /// into v2's slice 1 followed by v1's slices 2..3.
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

        /// The wiring: `restore_unit --dry-run` counts the SELECTED version's
        /// slices — 1 for the default (v2), 3 for `--version 1` — where the
        /// unfiltered query counted all 4. A refused version fails the dry
        /// run too, before any drive is touched.
        #[test]
        fn restore_unit_dry_run_counts_only_the_selected_version() {
            let f = two_versions_on_one_volume();
            let home = TempDir::new().unwrap();
            let paths = TapectlPaths::new(home.path().join(".tapectl"));
            let run = |v: Option<i64>| {
                restore_unit(
                    &f.conn,
                    &paths,
                    &Config::default(),
                    "big",
                    "VOL-PM4",
                    "/nonexistent/tapectl-dry-run-dest",
                    "/nonexistent/tapectl-dry-run-nst",
                    524288,
                    v,
                    true,
                    &RestoreOptions::default(),
                )
            };
            let newest = run(None).unwrap();
            assert_eq!((newest.version, newest.slices), (2, 1));
            let v1 = run(Some(1)).unwrap();
            assert_eq!((v1.version, v1.slices), (1, 3));
            let err = run(Some(3)).unwrap_err().to_string();
            assert!(err.contains("no version 3"), "{err}");
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

        /// `restore volume` with no label: the units are listed from the
        /// volume File 0 names, planned and restored from it; the report
        /// names it.
        #[test]
        fn restore_volume_with_no_label_restores_the_units_of_the_loaded_volume() {
            let conn = crate::db::open_memory().unwrap();
            seed_loaded(&conn, "RL-MANY", "rl-unit");
            let tape = tape_labelled("RL-MANY");
            let dest = TempDir::new().unwrap();
            let paths = TapectlPaths::new(dest.path().to_path_buf());
            let (_fake, mut store) = on_fake_tape(&tape);
            let to = dest.path().join("out");

            let report = restore_units_from_loaded_store(
                &conn,
                &paths,
                &Config::default(),
                &[],
                &to.to_string_lossy(),
                &RestoreOptions::default(),
                false,
                &mut store,
                site(Operation::RestoreVolume),
            )
            .expect("the contact check passes; each unit's own failure is its outcome");

            assert_eq!(report.volume_label, "RL-MANY");
            assert_eq!(
                report
                    .units
                    .iter()
                    .map(|u| u.unit_name.as_str())
                    .collect::<Vec<_>>(),
                ["rl-unit"],
                "every unit written to the loaded volume"
            );
            assert!(report.units[0].attempted);
            let (operation, outcome) = only_contact(&conn);
            assert_eq!(operation, "restore volume");
            assert!(outcome.is_some());
            // ONE contact for the whole run, naming the volume (issue #442).
            let contacts = contact_rows(&conn);
            assert_eq!(contacts.len(), 1, "{contacts:?}");
            assert!(contacts[0].1.is_some(), "{contacts:?}");
        }

        /// The same for a volume named by a tape that is another volume's:
        /// refused, nothing planned past the volume lookup.
        #[test]
        fn restore_volume_with_no_label_refuses_a_tape_the_catalog_does_not_know() {
            let conn = crate::db::open_memory().unwrap();
            seed(&conn, "RL-OTHER", "rl-unit");
            let mut store = tape_labelled("RL-STRANGER");
            let dest = TempDir::new().unwrap();
            let paths = TapectlPaths::new(dest.path().to_path_buf());

            let err = restore_units_from_loaded_store(
                &conn,
                &paths,
                &Config::default(),
                &[],
                &dest.path().join("out").to_string_lossy(),
                &RestoreOptions::default(),
                false,
                &mut store,
                site(Operation::RestoreVolume),
            )
            .unwrap_err()
            .to_string();

            assert!(err.contains("RL-STRANGER"), "{err}");
            assert!(err.contains("no volume with that uuid"), "{err}");
            assert!(!dest.path().join("out").exists());
            // The drive was opened and File 0 read: one `failed` contact
            // with the reason, naming no volume (issue #442).
            let contacts = contact_rows(&conn);
            assert_eq!(contacts.len(), 1, "{contacts:?}");
            assert_eq!(contacts[0].0, "restore volume");
            assert_eq!(contacts[0].1, None);
            assert_eq!(contacts[0].2.as_deref(), Some("failed"));
            assert!(
                contacts[0]
                    .3
                    .as_deref()
                    .is_some_and(|d| d.contains("no volume with that uuid")),
                "{contacts:?}"
            );
        }

        /// `restore volume` with no label, asked for a unit the catalog does
        /// not have on the volume: refused by the set's planning, after
        /// File 0 — one `failed` contact naming the volume (issue #442).
        #[test]
        fn restore_volume_with_no_label_records_a_plan_refusal_as_a_failed_contact() {
            let conn = crate::db::open_memory().unwrap();
            let volume_id = seed_loaded(&conn, "RL-SETPLAN", "rl-unit");
            let tape = tape_labelled("RL-SETPLAN");
            let dest = TempDir::new().unwrap();
            let paths = TapectlPaths::new(dest.path().to_path_buf());
            let (_fake, mut store) = on_fake_tape(&tape);

            let err = restore_units_from_loaded_store(
                &conn,
                &paths,
                &Config::default(),
                &["no-such-unit".to_string()],
                &dest.path().join("out").to_string_lossy(),
                &RestoreOptions::default(),
                false,
                &mut store,
                site(Operation::RestoreVolume),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("no-such-unit"), "{err}");

            let contacts = contact_rows(&conn);
            assert_eq!(contacts.len(), 1, "{contacts:?}");
            assert_eq!(contacts[0].0, "restore volume");
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

        // ── issue #306: the RESTORE's own record ──

        mod record {
            use super::*;
            use crate::volume::restore_record::{rows, RestoreRow};

            /// A real restore fixture: `seed`'s catalog, a real dar archive
            /// of two files (one slice) encrypted to a key saved in
            /// `paths.keys_dir`, and a MemStore whose File 0 names `label`
            /// and whose File 1 is that ciphertext. The seeded slice row and
            /// position are rewritten to the real values, so the restore
            /// runs end to end: tape read, sha256, decrypt, `dar -x`.
            ///
            /// Returns the store and the plaintext slice length.
            pub(super) fn real_fixture(
                conn: &Connection,
                paths: &TapectlPaths,
                label: &str,
                unit: &str,
            ) -> (MemStore, u64) {
                real_fixture_keyed(conn, paths, label, unit, "t1", "primary")
            }

            /// [`real_fixture`] with the slice encrypted to key file
            /// `{key_tenant}-{key_alias}.age.key` instead of `t1-primary`.
            fn real_fixture_keyed(
                conn: &Connection,
                paths: &TapectlPaths,
                label: &str,
                unit: &str,
                key_tenant: &str,
                key_alias: &str,
            ) -> (MemStore, u64) {
                seed(conn, label, unit);
                paths.ensure_dirs().unwrap();
                let kp = keys::generate_and_save(&paths.keys_dir, key_tenant, key_alias).unwrap();

                let work = TempDir::new().unwrap();
                let src = work.path().join("src");
                fs::create_dir_all(&src).unwrap();
                fs::write(src.join("a.txt"), b"alpha").unwrap();
                fs::write(src.join("b.txt"), b"bravo").unwrap();
                let base = work.path().join("arch");
                let created = std::process::Command::new("dar")
                    .arg("-c")
                    .arg(&base)
                    .arg("-R")
                    .arg(&src)
                    .arg("-Q")
                    .output()
                    .unwrap();
                assert!(created.status.success(), "dar -c failed in test setup");
                let plain = fs::read(work.path().join("arch.1.dar")).unwrap();
                let cipher = encrypt_to(&plain, std::slice::from_ref(&kp.public_key));

                let mut store = tape_labelled(label);
                store
                    .execute(&mut Cursor::new(cipher.clone()), cipher.len() as u64, false)
                    .unwrap();
                conn.execute(
                    "UPDATE stage_slices SET size_bytes = ?1, encrypted_bytes = ?2,
                            sha256_plain = ?3, sha256_encrypted = ?4",
                    params![
                        plain.len() as i64,
                        cipher.len() as i64,
                        direct_hash(&plain),
                        direct_hash(&cipher)
                    ],
                )
                .unwrap();
                conn.execute(
                    "UPDATE write_positions SET position = '1', sha256_on_volume = ?1",
                    params![direct_hash(&cipher)],
                )
                .unwrap();
                (store, plain.len() as u64)
            }

            pub(super) fn only_row(conn: &Connection) -> RestoreRow {
                let mut all = rows(conn).unwrap();
                assert_eq!(all.len(), 1, "one restore is one row: {all:?}");
                all.remove(0)
            }

            fn id_of(conn: &Connection, sql: &str) -> i64 {
                conn.query_row(sql, [], |r| r.get(0)).unwrap()
            }

            /// The success row, and the POSITIVE CONTROL for every failure
            /// test below: a clean restore records a row too, with dar's
            /// report present and non-empty and the counts measured — so
            /// these tests distinguish "records restores" from "records
            /// failures".
            #[test]
            fn a_clean_restore_unit_records_one_ok_row_with_dar_report_and_counts() {
                let conn = crate::db::open_memory().unwrap();
                let home = TempDir::new().unwrap();
                let paths = TapectlPaths::new(home.path().join(".tapectl"));
                let (mut store, plain_len) = real_fixture(&conn, &paths, "RR-OK", "rr-unit");
                let dest = TempDir::new().unwrap();
                let dest_str = dest.path().to_string_lossy().to_string();

                let report = restore_unit_from_store(
                    &conn,
                    &paths,
                    &Config::default(),
                    "rr-unit",
                    "RR-OK",
                    1,
                    RestoreTarget::Unit {
                        dest_dir: &dest_str,
                    },
                    &RestoreOptions::default(),
                    &mut store,
                    site(Operation::RestoreUnit),
                )
                .expect("a clean restore");
                assert_eq!(report.slices, 1);
                // The row's `ok` is backed by a real extract.
                assert_eq!(fs::read(dest.path().join("a.txt")).unwrap(), b"alpha");
                assert_eq!(fs::read(dest.path().join("b.txt")).unwrap(), b"bravo");

                let r = only_row(&conn);
                assert_eq!(r.kind, "unit");
                assert_eq!(r.outcome, "ok");
                assert_eq!(r.error, None);
                assert_eq!(
                    r.contact_id,
                    Some(id_of(&conn, "SELECT id FROM cartridge_contacts")),
                    "the contact is the spine"
                );
                assert_eq!(
                    r.volume_id,
                    Some(id_of(&conn, "SELECT id FROM volumes WHERE label = 'RR-OK'"))
                );
                assert_eq!(r.volume_label.as_deref(), Some("RR-OK"));
                assert_eq!(
                    r.unit_id,
                    Some(id_of(&conn, "SELECT id FROM units WHERE name = 'rr-unit'"))
                );
                assert_eq!(r.unit_name.as_deref(), Some("rr-unit"));
                assert_eq!(r.version, Some(1));
                assert_eq!(r.file_path, None);
                assert_eq!(r.destination, dest_str);
                assert!(r.finished_at >= r.started_at);
                assert_eq!(r.slices_read, Some(1));
                assert_eq!(r.bytes_restored, Some(plain_len as i64));
                assert_eq!(r.files_restored, Some(2), "dar's own inode count");
                assert_eq!(r.dar_exit_code, Some(0));
                assert!(r
                    .dar_argv
                    .as_deref()
                    .unwrap()
                    .starts_with(r#"["dar","-x","#));
                let stdout = r.dar_stdout.expect("dar ran: its report is kept");
                assert!(
                    !stdout.is_empty(),
                    "positive control: the report is non-empty"
                );
                assert!(stdout.contains("2 inode(s) restored"), "{stdout}");
                assert!(r.dar_stderr.is_some(), "stderr kept too, even if empty");
                // `dar::version::check`'s parsed spelling, e.g. `2.7.13`.
                let ver = r.dar_version.expect("dar ran, so its version was read");
                assert!(
                    ver.split('.').count() == 3 && ver.split('.').all(|p| p.parse::<u32>().is_ok()),
                    "{ver}"
                );
                assert_eq!(r.tapectl_version, crate::build_info::VERSION);
            }

            /// Issue #350(d) at the restore path: tenant `t1` must not
            /// decrypt with tenant `t1-old`'s key just because `t1-` prefixes
            /// its file name. The slice is encrypted ONLY to
            /// `t1-old-primary.age.key`. Positive control first: with no key
            /// row (the `catalog rebuild` case) the file name cannot say
            /// whose it is, so `t1` gets it and the restore succeeds — the
            /// loader never withholds a key it cannot prove is another
            /// tenant's. Then the catalog's row says it is `t1-old`'s, and
            /// the same restore of `t1` finds no key of its own.
            #[test]
            fn restore_loads_only_the_keys_the_catalog_says_are_the_tenants() {
                let restore = |conn: &Connection, paths: &TapectlPaths, store: &mut MemStore| {
                    let dest = TempDir::new().unwrap();
                    let dest_str = dest.path().to_string_lossy().to_string();
                    restore_unit_from_store(
                        conn,
                        paths,
                        &Config::default(),
                        "kk-unit",
                        "KK-1",
                        1,
                        RestoreTarget::Unit {
                            dest_dir: &dest_str,
                        },
                        &RestoreOptions::default(),
                        store,
                        site(Operation::RestoreUnit),
                    )
                };

                // Control: no key rows.
                let conn = crate::db::open_memory().unwrap();
                let home = TempDir::new().unwrap();
                let paths = TapectlPaths::new(home.path().join(".tapectl"));
                let (mut store, _) =
                    real_fixture_keyed(&conn, &paths, "KK-1", "kk-unit", "t1-old", "primary");
                restore(&conn, &paths, &mut store)
                    .expect("control: with no key row, t1 may own t1-old-primary");

                // The catalog says the key is t1-old's.
                let conn = crate::db::open_memory().unwrap();
                let home = TempDir::new().unwrap();
                let paths = TapectlPaths::new(home.path().join(".tapectl"));
                let (mut store, _) =
                    real_fixture_keyed(&conn, &paths, "KK-1", "kk-unit", "t1-old", "primary");
                let old = crate::db::queries::insert_tenant(&conn, "t1-old", None, false).unwrap();
                let public = fs::read_to_string(paths.keys_dir.join("t1-old-primary.age.pub"))
                    .unwrap()
                    .trim()
                    .to_string();
                crate::db::queries::insert_key(
                    &conn,
                    old,
                    "t1-old-primary",
                    &public,
                    &public,
                    "primary",
                    None,
                )
                .unwrap();
                let err = restore(&conn, &paths, &mut store)
                    .expect_err("t1 decrypted with t1-old's key")
                    .to_string();
                assert!(
                    err.contains("no secret keys found for tenant \"t1\""),
                    "{err}"
                );
            }

            /// `restore file` is ONE row of kind `file` under `restore
            /// unit`'s one contact, naming the directory the operator gave
            /// — never the scratch directory the file was extracted into.
            #[test]
            fn a_restore_file_records_one_file_row_naming_the_operators_destination() {
                let conn = crate::db::open_memory().unwrap();
                let home = TempDir::new().unwrap();
                let paths = TapectlPaths::new(home.path().join(".tapectl"));
                let (mut store, _) = real_fixture(&conn, &paths, "RF-OK", "rf-unit");
                let dest = TempDir::new().unwrap();
                let dest_str = dest.path().to_string_lossy().to_string();

                restore_unit_from_store(
                    &conn,
                    &paths,
                    &Config::default(),
                    "rf-unit",
                    "RF-OK",
                    1,
                    RestoreTarget::File {
                        file_path: "b.txt",
                        dest_dir: &dest_str,
                    },
                    &RestoreOptions::default(),
                    &mut store,
                    site(Operation::RestoreUnit),
                )
                .expect("a clean file restore");
                assert_eq!(fs::read(dest.path().join("b.txt")).unwrap(), b"bravo");
                assert!(
                    !dest.path().join("a.txt").exists(),
                    "only the one file lands in the operator's destination"
                );

                assert_eq!(
                    only_contact(&conn),
                    ("restore unit".to_string(), Some("ok".to_string()))
                );
                let r = only_row(&conn);
                assert_eq!(r.kind, "file");
                assert_eq!(r.outcome, "ok");
                assert_eq!(r.file_path.as_deref(), Some("b.txt"));
                assert_eq!(r.destination, dest_str);
                assert_eq!(r.files_restored, Some(1));
                assert!(!r.dar_stdout.unwrap().is_empty());
            }

            /// The placing step is inside the recorded span: a file the unit
            /// does not contain is a `failed` row (and a failed contact),
            /// not an `ok` row beside a non-zero exit — and dar's report is
            /// still there, because dar did run.
            #[test]
            fn a_restore_file_whose_entry_is_missing_records_a_failed_row() {
                let conn = crate::db::open_memory().unwrap();
                let home = TempDir::new().unwrap();
                let paths = TapectlPaths::new(home.path().join(".tapectl"));
                let (mut store, _) = real_fixture(&conn, &paths, "RF-MISS", "rf-unit");
                let dest = TempDir::new().unwrap();

                let err = restore_unit_from_store(
                    &conn,
                    &paths,
                    &Config::default(),
                    "rf-unit",
                    "RF-MISS",
                    1,
                    RestoreTarget::File {
                        file_path: "nope.txt",
                        dest_dir: &dest.path().to_string_lossy(),
                    },
                    &RestoreOptions::default(),
                    &mut store,
                    site(Operation::RestoreUnit),
                )
                .unwrap_err()
                .to_string();
                assert!(err.contains("not found in restored unit"), "{err}");

                assert_eq!(only_contact(&conn).1.as_deref(), Some("failed"));
                let r = only_row(&conn);
                assert_eq!(r.kind, "file");
                assert_eq!(r.outcome, "failed");
                assert!(
                    r.error
                        .as_deref()
                        .unwrap()
                        .contains("not found in restored unit"),
                    "{:?}",
                    r.error
                );
                assert_eq!(r.files_restored, None, "nothing was placed");
                assert_eq!(r.slices_read, Some(1), "the tape WAS read");
                assert!(!r.dar_stdout.unwrap().is_empty(), "dar ran and said so");
            }

            /// A restore that fails AFTER its contact opened — here on the
            /// key load, the injected error — records a `failed` row naming
            /// the error, with NULL for dar's report because dar never ran.
            #[test]
            fn a_restore_failing_after_the_contact_opened_records_a_failed_row() {
                let conn = crate::db::open_memory().unwrap();
                seed(&conn, "RR-NOKEY", "rr-unit");
                let mut store = tape_labelled("RR-NOKEY");
                let dest = TempDir::new().unwrap();
                let paths = TapectlPaths::new(dest.path().to_path_buf());

                let err = restore_unit_from_store(
                    &conn,
                    &paths,
                    &Config::default(),
                    "rr-unit",
                    "RR-NOKEY",
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
                assert!(err.contains("no secret keys"), "{err}");

                let r = only_row(&conn);
                assert_eq!(r.kind, "unit");
                assert_eq!(r.outcome, "failed");
                assert_eq!(r.error.as_deref(), Some(err.as_str()));
                assert_eq!(
                    r.contact_id,
                    Some(id_of(&conn, "SELECT id FROM cartridge_contacts"))
                );
                assert_eq!(r.slices_read, Some(0), "measured: none were read");
                assert_eq!(r.dar_stdout, None, "dar never ran: NULL, not empty");
                assert_eq!(r.dar_stderr, None);
                assert_eq!(r.dar_exit_code, None);
                assert_eq!(r.dar_version, None);
                assert_eq!(r.files_restored, None);
            }

            /// A wrong-tape refusal happens at the contact, so it is
            /// recorded as a failed restore too.
            #[test]
            fn a_restore_refused_for_the_wrong_tape_records_a_failed_row() {
                let conn = crate::db::open_memory().unwrap();
                seed(&conn, "RR-WANT", "rr-unit");
                let mut store = tape_labelled("RR-LOADED");
                let dest = TempDir::new().unwrap();
                let paths = TapectlPaths::new(dest.path().to_path_buf());
                let _ = restore_unit_from_store(
                    &conn,
                    &paths,
                    &Config::default(),
                    "rr-unit",
                    "RR-WANT",
                    1,
                    RestoreTarget::Unit {
                        dest_dir: &dest.path().to_string_lossy(),
                    },
                    &RestoreOptions::default(),
                    &mut store,
                    site(Operation::RestoreUnit),
                );
                let r = only_row(&conn);
                assert_eq!(r.outcome, "failed");
                assert!(r.error.as_deref().unwrap().contains("wrong tape"));
                assert_eq!(r.volume_label.as_deref(), Some("RR-WANT"));
            }

            /// A refusal BEFORE the contact — a dry run, here — is not a
            /// restore and writes no row. The positive control is every
            /// test above.
            #[test]
            fn a_dry_run_writes_no_row() {
                let conn = crate::db::open_memory().unwrap();
                seed(&conn, "RR-DRY", "rr-unit");
                let dest = TempDir::new().unwrap();
                let paths = TapectlPaths::new(dest.path().to_path_buf());
                restore_unit(
                    &conn,
                    &paths,
                    &Config::default(),
                    "rr-unit",
                    "RR-DRY",
                    &dest.path().to_string_lossy(),
                    "/nonexistent/tapectl-dry-run",
                    4096,
                    None,
                    true,
                    &RestoreOptions::default(),
                )
                .unwrap();
                assert!(rows(&conn).unwrap().is_empty());
            }

            fn raw_volume(conn: &Connection, store: &mut MemStore, dest: &Path) -> bool {
                restore_raw_volume(conn, store, dest, None, site(Operation::RestoreRawVolume))
                    .is_ok()
            }

            /// `restore raw-volume`: a clean dump is an `ok` row of kind
            /// `raw-volume`, carrying the tape's own label and the measured
            /// dump, and no dar report (a raw dump runs no dar).
            #[test]
            fn a_clean_raw_volume_dump_records_an_ok_row() {
                let conn = crate::db::open_memory().unwrap();
                let data = b"raw-volume slice bytes".to_vec();
                let mut store = crate::volume::raw::tests::build_synthetic_tape("RAW-OK", &data);
                let dest = TempDir::new().unwrap();
                assert!(raw_volume(&conn, &mut store, dest.path()));

                let r = only_row(&conn);
                assert_eq!(r.kind, "raw-volume");
                assert_eq!(r.outcome, "ok");
                assert_eq!(
                    r.volume_label.as_deref(),
                    Some("RAW-OK"),
                    "the tape's own claim"
                );
                assert_eq!(r.volume_id, None, "raw-volume names no catalog row");
                assert_eq!(r.unit_name, None);
                assert_eq!(r.files_restored, Some(6));
                assert!(r.bytes_restored.unwrap() > data.len() as i64);
                assert_eq!(r.slices_read, None);
                assert_eq!(r.dar_stdout, None);
                assert_eq!(r.destination, dest.path().to_string_lossy());
                assert_eq!(
                    r.contact_id,
                    Some(id_of(&conn, "SELECT id FROM cartridge_contacts"))
                );
            }

            /// A raw dump that fails (File 0 only, no front index) records a
            /// `failed` row with the error.
            #[test]
            fn a_failed_raw_volume_dump_records_a_failed_row() {
                let conn = crate::db::open_memory().unwrap();
                let mut store = tape_labelled("RAW-BAD");
                let dest = TempDir::new().unwrap();
                assert!(!raw_volume(&conn, &mut store, dest.path()));

                let r = only_row(&conn);
                assert_eq!(r.kind, "raw-volume");
                assert_eq!(r.outcome, "failed");
                assert!(r.error.is_some());
                assert_eq!(r.files_restored, None, "not known: the dump never finished");
                assert_eq!(r.bytes_restored, None);
            }
        }

        // ── issue #406: restore checks before it touches the tape ──

        mod preflight {
            use super::record::{only_row, real_fixture};
            use super::*;
            use crate::store::{injected::InjectedDrive, OpenMode};
            use crate::tape::fake::FakeTape;

            const DEVICE: &str = "/nonexistent/tapectl-restore-preflight-nst";

            /// `real_fixture`'s catalog and tape (a real dar archive of
            /// `a.txt` and `b.txt`), the version's `files` rows as `snapshot
            /// create` writes them, and the tape behind an injected drive —
            /// so the REAL `restore_unit`/`restore_file` run end to end, and
            /// the fake says whether, and how, the drive was opened.
            struct Rig {
                conn: Connection,
                paths: TapectlPaths,
                fake: FakeTape,
                _home: TempDir,
                _drive: InjectedDrive,
            }

            fn rig(label: &str) -> Rig {
                let conn = crate::db::open_memory().unwrap();
                let home = TempDir::new().unwrap();
                let paths = TapectlPaths::new(home.path().join(".tapectl"));
                let (store, _) = real_fixture(&conn, &paths, label, "pf-unit");
                let snapshot_id: i64 = conn
                    .query_row("SELECT id FROM snapshots", [], |r| r.get(0))
                    .unwrap();
                for (path, size, kind) in [
                    ("a.txt", 5, "regular"),
                    ("b.txt", 5, "regular"),
                    ("docs", 0, "dir"),
                ] {
                    crate::db::files::fixture::insert(&conn, snapshot_id, path, size, kind, None);
                }
                let fake = FakeTape::with_files(store.files.clone(), 4096);
                let drive = InjectedDrive::install(&fake);
                Rig {
                    conn,
                    paths,
                    fake,
                    _home: home,
                    _drive: drive,
                }
            }

            fn unit(r: &Rig, label: &str, dest: &Path, options: &RestoreOptions) -> Result<()> {
                restore_unit(
                    &r.conn,
                    &r.paths,
                    &Config::default(),
                    "pf-unit",
                    label,
                    &dest.to_string_lossy(),
                    DEVICE,
                    4096,
                    None,
                    false,
                    options,
                )
                .map(|_| ())
            }

            fn file(
                r: &Rig,
                label: &str,
                path: &str,
                dest: &Path,
                options: &RestoreOptions,
            ) -> Result<()> {
                restore_file(
                    &r.conn,
                    &r.paths,
                    &Config::default(),
                    "pf-unit",
                    path,
                    label,
                    &dest.to_string_lossy(),
                    DEVICE,
                    4096,
                    None,
                    options,
                )
                .map(|_| ())
            }

            /// A refusal before contact: the drive never opened, and so no
            /// contact, MAM journal or `restores` row exists.
            fn assert_no_tape_contact(r: &Rig) {
                assert_eq!(r.fake.opens(), vec![], "the drive was opened");
                let count = |sql: &str| -> i64 { r.conn.query_row(sql, [], |x| x.get(0)).unwrap() };
                assert_eq!(count("SELECT COUNT(*) FROM cartridge_contacts"), 0);
                assert_eq!(count("SELECT COUNT(*) FROM mam_journal"), 0);
                assert_eq!(count("SELECT COUNT(*) FROM restores"), 0);
            }

            fn names_in(dir: &Path) -> Vec<String> {
                let mut names: Vec<String> = fs::read_dir(dir)
                    .unwrap()
                    .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                names
            }

            /// The positive control for every refusal below: the same rig,
            /// a clean destination, and the restore runs — read-only, with
            /// its decrypted slices in a scratch directory inside `--to`
            /// WHILE the tape is read, and nothing of it left after.
            #[test]
            fn a_restore_unit_into_an_empty_destination_runs_with_scratch_inside_it() {
                let r = rig("PF-OK");
                let dest = TempDir::new().unwrap();
                r.fake.watch(&dest.path().join(SCRATCH_NAME));
                unit(&r, "PF-OK", dest.path(), &RestoreOptions::default()).unwrap();

                assert_eq!(names_in(dest.path()), ["a.txt", "b.txt"]);
                assert_eq!(r.fake.opens(), vec![OpenMode::ReadOnly]);
                assert!(
                    r.fake.watched().contains(&(1, true)),
                    "the scratch directory was inside --to while the slice was read: {:?}",
                    r.fake.watched()
                );
            }

            /// Issue #406: `restore file` used to extract the WHOLE unit into
            /// `tempfile::tempdir()` — $TMPDIR, the small root filesystem on
            /// home2 — slices and all. Now its scratch is inside `--to` while
            /// the tape is read, dar is asked for the one entry (`-g`), and
            /// only that file lands.
            #[test]
            fn restore_file_extracts_only_its_file_with_dar_g_and_scratch_inside_to() {
                let r = rig("PF-FILE");
                let dest = TempDir::new().unwrap();
                r.fake.watch(&dest.path().join(SCRATCH_NAME));
                file(
                    &r,
                    "PF-FILE",
                    "b.txt",
                    dest.path(),
                    &RestoreOptions::default(),
                )
                .unwrap();

                assert_eq!(fs::read(dest.path().join("b.txt")).unwrap(), b"bravo");
                assert_eq!(names_in(dest.path()), ["b.txt"], "no scratch left behind");
                assert!(
                    r.fake.watched().contains(&(1, true)),
                    "the scratch directory was inside --to while the slice was read, not in \
                     $TMPDIR: {:?}",
                    r.fake.watched()
                );
                let row = only_row(&r.conn);
                let argv = row.dar_argv.expect("dar ran");
                assert!(argv.contains(r#""-g","b.txt""#), "{argv}");
            }

            /// The production half never asks the system for a temp
            /// directory — the deterministic form of "never touches $TMPDIR".
            #[test]
            fn the_restore_code_never_uses_a_system_temp_directory() {
                const SRC: &str = include_str!("restore.rs");
                let prod = SRC.split("#[cfg(test)]\nmod tests").next().unwrap();
                assert!(prod.len() < SRC.len(), "positive control: tests split off");
                assert!(prod.contains("fn restore_file("), "positive control");
                for needle in ["tempfile::", "temp_dir(", "TMPDIR"] {
                    assert!(
                        !prod
                            .lines()
                            .filter(|l| !l.trim_start().starts_with("//"))
                            .any(|l| l.contains(needle)),
                        "production restore code uses {needle}"
                    );
                }
            }

            #[test]
            fn the_scratch_directory_is_inside_the_destination_or_scratch() {
                assert_eq!(
                    scratch_dir(Path::new("/restore/here"), None),
                    Path::new("/restore/here/.tapectl-restore-tmp")
                );
                assert_eq!(
                    scratch_dir(Path::new("/restore/here"), Some(Path::new("/big/disk"))),
                    Path::new("/big/disk/.tapectl-restore-tmp")
                );
            }

            /// Too little space is refused before the drive is opened —
            /// it used to fill the disk hours in.
            #[test]
            fn a_short_space_destination_is_refused_before_the_drive_is_opened() {
                let r = rig("PF-SPACE");
                let dest = TempDir::new().unwrap();
                let _free = RestoreFreeOverride::set(10);
                let err = unit(&r, "PF-SPACE", dest.path(), &RestoreOptions::default())
                    .unwrap_err()
                    .to_string();
                assert!(err.contains("not enough disk space"), "{err}");
                assert!(err.contains("Nothing was read from tape"), "{err}");
                assert!(err.contains("--scratch DIR"), "{err}");
                assert_no_tape_contact(&r);

                // --no-space-check is the escape, as in RESTORE.sh.
                let options = RestoreOptions {
                    no_space_check: true,
                    ..RestoreOptions::default()
                };
                unit(&r, "PF-SPACE", dest.path(), &options).unwrap();
                assert_eq!(names_in(dest.path()), ["a.txt", "b.txt"]);
            }

            /// A destination that does not exist yet is measured on its
            /// nearest existing ancestor — and a RELATIVE one (`--to
            /// restored`) on the working directory's, never skipped: its
            /// ancestors as written end in "", which exists nowhere.
            #[test]
            fn a_missing_destination_is_measured_on_its_nearest_existing_ancestor() {
                let cwd = std::env::current_dir().unwrap();
                assert_eq!(
                    existing(Path::new("tapectl-no-such-relative-dir/deeper")),
                    Some(cwd)
                );
                let tmp = TempDir::new().unwrap();
                assert_eq!(
                    existing(&tmp.path().join("not/yet/made")),
                    Some(tmp.path().to_path_buf())
                );
                assert_eq!(existing(tmp.path()), Some(tmp.path().to_path_buf()));
            }

            /// A spooled restore (no isolated catalogue on disk, as here)
            /// needs about twice the unit on one filesystem: its decrypted
            /// slices, then the extract. Since #411 no ciphertext copy waits
            /// beside a slice, so the "plus one slice" RESTORE.sh still asks
            /// for is gone — refused one byte short of twice, accepted at it.
            #[test]
            fn the_space_needed_to_spool_is_twice_the_unit() {
                let r = rig("PF-MATH");
                let slice: i64 = r
                    .conn
                    .query_row("SELECT encrypted_bytes FROM stage_slices", [], |x| x.get(0))
                    .unwrap();
                let dest = TempDir::new().unwrap();
                {
                    let _free = RestoreFreeOverride::set(2 * slice - 1);
                    let err = unit(&r, "PF-MATH", dest.path(), &RestoreOptions::default())
                        .unwrap_err()
                        .to_string();
                    assert!(err.contains("not enough disk space"), "{err}");
                    assert!(err.contains("twice"), "{err}");
                }
                let _free = RestoreFreeOverride::set(2 * slice);
                unit(&r, "PF-MATH", dest.path(), &RestoreOptions::default()).unwrap();
            }

            /// A destination that is not empty is refused before the drive
            /// is opened (dar would keep each colliding file, found only
            /// after the whole unit was read); `--overwrite` replaces them.
            #[test]
            fn a_non_empty_destination_is_refused_unless_overwrite() {
                let r = rig("PF-FULL");
                let dest = TempDir::new().unwrap();
                fs::write(dest.path().join("a.txt"), b"STALE").unwrap();
                let err = unit(&r, "PF-FULL", dest.path(), &RestoreOptions::default())
                    .unwrap_err()
                    .to_string();
                assert!(err.contains("is not empty"), "{err}");
                assert!(err.contains("--overwrite"), "{err}");
                assert_no_tape_contact(&r);

                // A dry run that says "would restore" means it.
                let dry = restore_unit(
                    &r.conn,
                    &r.paths,
                    &Config::default(),
                    "pf-unit",
                    "PF-FULL",
                    &dest.path().to_string_lossy(),
                    DEVICE,
                    4096,
                    None,
                    true,
                    &RestoreOptions::default(),
                );
                assert!(dry.is_err(), "a dry run takes the same refusal");

                let options = RestoreOptions {
                    overwrite: true,
                    ..RestoreOptions::default()
                };
                unit(&r, "PF-FULL", dest.path(), &options).unwrap();
                assert_eq!(fs::read(dest.path().join("a.txt")).unwrap(), b"alpha");
                let argv = only_row(&r.conn).dar_argv.expect("dar ran");
                assert!(argv.contains(r#""-w""#), "{argv}");
            }

            /// `restore file` places one name, so only that name collides.
            #[test]
            fn restore_file_refuses_an_existing_file_of_that_name_unless_overwrite() {
                let r = rig("PF-FCOL");
                let dest = TempDir::new().unwrap();
                fs::write(dest.path().join("unrelated.txt"), b"mine").unwrap();
                fs::write(dest.path().join("b.txt"), b"STALE").unwrap();
                let err = file(
                    &r,
                    "PF-FCOL",
                    "b.txt",
                    dest.path(),
                    &RestoreOptions::default(),
                )
                .unwrap_err()
                .to_string();
                assert!(err.contains("already exists"), "{err}");
                assert_no_tape_contact(&r);

                let options = RestoreOptions {
                    overwrite: true,
                    ..RestoreOptions::default()
                };
                file(&r, "PF-FCOL", "b.txt", dest.path(), &options).unwrap();
                assert_eq!(fs::read(dest.path().join("b.txt")).unwrap(), b"bravo");
                assert_eq!(
                    fs::read(dest.path().join("unrelated.txt")).unwrap(),
                    b"mine"
                );
            }

            /// `--file` is checked against the version's `files` rows before
            /// the drive is opened: a typo used to read every slice (~5 h on
            /// a large unit) and then fail. A directory is refused too.
            #[test]
            fn an_unknown_or_directory_file_is_refused_before_the_drive_is_opened() {
                let r = rig("PF-NOFILE");
                let dest = TempDir::new().unwrap();
                let err = file(
                    &r,
                    "PF-NOFILE",
                    "nope.txt",
                    dest.path(),
                    &RestoreOptions::default(),
                )
                .unwrap_err()
                .to_string();
                assert!(err.contains("has no file \"nope.txt\""), "{err}");
                assert!(err.contains("tape was not touched"), "{err}");
                assert!(err.contains("tapectl catalog search"), "{err}");
                // `catalog search` looks in each unit's NEWEST version only;
                // the version restored here may be an older one.
                assert!(
                    err.contains("tapectl catalog search \"<words of the name>\" --all-versions"),
                    "{err}"
                );

                let err = file(
                    &r,
                    "PF-NOFILE",
                    "docs",
                    dest.path(),
                    &RestoreOptions::default(),
                )
                .unwrap_err()
                .to_string();
                assert!(err.contains("is a directory"), "{err}");
                assert_no_tape_contact(&r);
            }

            /// A scratch directory a killed restore left (its guard never
            /// ran) may hold decrypted slices: refused, named, never reused —
            /// inside `--to`, and inside `--scratch`.
            #[test]
            fn a_leftover_scratch_directory_is_refused_before_the_drive_is_opened() {
                let r = rig("PF-LEFT");
                let dest = TempDir::new().unwrap();
                fs::create_dir(dest.path().join(SCRATCH_NAME)).unwrap();
                let err = unit(&r, "PF-LEFT", dest.path(), &RestoreOptions::default())
                    .unwrap_err()
                    .to_string();
                assert!(err.contains("already exists"), "{err}");
                assert!(err.contains("DECRYPTED"), "{err}");
                assert!(err.contains(SCRATCH_NAME), "{err}");

                let other = TempDir::new().unwrap();
                fs::create_dir(other.path().join(SCRATCH_NAME)).unwrap();
                let fresh = TempDir::new().unwrap();
                let options = RestoreOptions {
                    scratch: Some(other.path().to_path_buf()),
                    ..RestoreOptions::default()
                };
                let err = unit(&r, "PF-LEFT", fresh.path(), &options)
                    .unwrap_err()
                    .to_string();
                assert!(
                    err.contains(&other.path().join(SCRATCH_NAME).display().to_string()),
                    "{err}"
                );
                assert_no_tape_contact(&r);
            }

            /// `--scratch DIR`: the decrypted slices wait there while the
            /// tape is read, not in `--to`, and nothing is left in either.
            #[test]
            fn scratch_dir_puts_the_decrypted_slices_on_another_disk() {
                let r = rig("PF-SCR");
                let dest = TempDir::new().unwrap();
                let other = TempDir::new().unwrap();
                r.fake.watch(&other.path().join(SCRATCH_NAME));
                let options = RestoreOptions {
                    scratch: Some(other.path().to_path_buf()),
                    ..RestoreOptions::default()
                };
                unit(&r, "PF-SCR", dest.path(), &options).unwrap();
                assert!(
                    r.fake.watched().contains(&(1, true)),
                    "{:?}",
                    r.fake.watched()
                );
                assert_eq!(names_in(dest.path()), ["a.txt", "b.txt"]);
                assert!(names_in(other.path()).is_empty(), "scratch removed");
            }
        }

        // ── issue #389: restore reads the tape forward ──

        mod forward_reads {
            use super::*;
            use crate::tape::fake::{FakeTape, Op};

            /// THE acceptance test for restore: a unit of K contiguous slices
            /// comes off a tape with ONE rewind — the open's — then File 0
            /// (the contact check), one forward space to the first slice and
            /// every slice in turn. Before #389 it was a rewind and a locate
            /// per slice.
            ///
            /// Driven through the real `restore_unit_from_store` over a
            /// `TapeStore` on the in-memory tape, with a real multi-slice dar
            /// archive, so the extract proves the slices that came back were
            /// the right ones.
            #[test]
            fn a_multi_slice_restore_is_one_rewind_and_one_forward_pass() {
                const FIRST: u32 = 4;
                let conn = crate::db::open_memory().unwrap();
                let home = TempDir::new().unwrap();
                let paths = TapectlPaths::new(home.path().join(".tapectl"));
                seed(&conn, "FWD-1", "fwd-unit");
                paths.ensure_dirs().unwrap();
                let kp = keys::generate_and_save(&paths.keys_dir, "t1", "primary").unwrap();

                // A real dar archive, cut into several slices.
                let work = TempDir::new().unwrap();
                let src = work.path().join("src");
                fs::create_dir_all(&src).unwrap();
                let blob: Vec<u8> = (0..200_000u32)
                    .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
                    .collect();
                fs::write(src.join("big.bin"), &blob).unwrap();
                fs::write(src.join("small.txt"), b"small").unwrap();
                let base = work.path().join("arch");
                let created = std::process::Command::new("dar")
                    .arg("-c")
                    .arg(&base)
                    .arg("-R")
                    .arg(&src)
                    .arg("-s")
                    .arg("64k")
                    .arg("-Q")
                    .output()
                    .unwrap();
                assert!(
                    created.status.success(),
                    "dar -c failed in test setup: {}",
                    String::from_utf8_lossy(&created.stderr)
                );
                let plains: Vec<Vec<u8>> = (1..)
                    .map(|n| work.path().join(format!("arch.{n}.dar")))
                    .take_while(|p| p.exists())
                    .map(|p| fs::read(p).unwrap())
                    .collect();
                assert!(
                    plains.len() >= 3,
                    "want a multi-slice archive, got {} slice(s)",
                    plains.len()
                );

                // The tape: File 0 names the volume, Files 1..3 stand in for
                // the rest of the front zone, then the slices from File 4.
                let mut mem = tape_labelled("FWD-1");
                for filler in 1..FIRST {
                    mem.execute(&mut Cursor::new(vec![filler as u8; 100]), 100, false)
                        .unwrap();
                }
                // The catalog: `seed` made slice 1 at position 4; give it
                // its real values and add the rest after it.
                let ss_id: i64 = conn
                    .query_row("SELECT id FROM stage_sets", [], |r| r.get(0))
                    .unwrap();
                let write_id: i64 = conn
                    .query_row("SELECT id FROM writes", [], |r| r.get(0))
                    .unwrap();
                for (i, plain) in plains.iter().enumerate() {
                    let cipher = encrypt_to(plain, std::slice::from_ref(&kp.public_key));
                    mem.execute(&mut Cursor::new(cipher.clone()), cipher.len() as u64, false)
                        .unwrap();
                    let number = i as i64 + 1;
                    let position = (FIRST + i as u32).to_string();
                    if number == 1 {
                        conn.execute(
                            "UPDATE stage_slices SET size_bytes = ?1, encrypted_bytes = ?2,
                                    sha256_plain = ?3, sha256_encrypted = ?4",
                            params![
                                plain.len() as i64,
                                cipher.len() as i64,
                                direct_hash(plain),
                                direct_hash(&cipher)
                            ],
                        )
                        .unwrap();
                        conn.execute(
                            "UPDATE write_positions SET position = ?1, sha256_on_volume = ?2",
                            params![position, direct_hash(&cipher)],
                        )
                        .unwrap();
                    } else {
                        conn.execute(
                            "INSERT INTO stage_slices
                                (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                 sha256_plain, sha256_encrypted)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            params![
                                ss_id,
                                number,
                                plain.len() as i64,
                                cipher.len() as i64,
                                direct_hash(plain),
                                direct_hash(&cipher)
                            ],
                        )
                        .unwrap();
                        let slice_id = conn.last_insert_rowid();
                        conn.execute(
                            "INSERT INTO write_positions
                                (write_id, stage_slice_id, position, status, sha256_on_volume)
                             VALUES (?1, ?2, ?3, 'written', ?4)",
                            params![write_id, slice_id, position, direct_hash(&cipher)],
                        )
                        .unwrap();
                    }
                }

                let fake = FakeTape::with_files(mem.files.clone(), 4096);
                let mut store = TapeStore::from_ops(fake.boxed(), 0).unwrap();
                let dest = TempDir::new().unwrap();
                let dest_str = dest.path().to_string_lossy().to_string();
                let report = restore_unit_from_store(
                    &conn,
                    &paths,
                    &Config::default(),
                    "fwd-unit",
                    "FWD-1",
                    1,
                    RestoreTarget::Unit {
                        dest_dir: &dest_str,
                    },
                    &RestoreOptions::default(),
                    &mut store,
                    site(Operation::RestoreUnit),
                )
                .expect("a clean multi-slice restore");
                assert_eq!(report.slices, plains.len());
                assert_eq!(fs::read(dest.path().join("big.bin")).unwrap(), blob);
                assert_eq!(fs::read(dest.path().join("small.txt")).unwrap(), b"small");

                // File 0 is a bounded head read (issue #400).
                let mut expected = vec![Op::Rewind, Op::ReadHead(0), Op::Space(FIRST - 1)];
                expected.extend((0..plains.len() as u32).map(|i| Op::Read(FIRST + i)));
                assert_eq!(fake.ops(), expected);
                assert_eq!(fake.rewinds(), 1);
            }
        }

        // ── issue #411: stream into dar; read only what a file needs ──

        /// A unit of several REAL dar slices — optionally with the isolated
        /// catalogue `stage create` makes, at `stage_sets.catalog_path` — on
        /// a fake tape behind an injected drive, so the real `restore_unit`
        /// / `restore_file` run end to end with real dar.
        pub(super) mod multi {
            use super::*;
            use crate::store::injected::InjectedDrive;
            use crate::tape::fake::FakeTape;

            /// Tape position of slice 1 (Files 1..3 stand in for the rest
            /// of the front zone).
            pub(crate) const FIRST: u32 = 4;
            pub(crate) const DEVICE: &str = "/nonexistent/tapectl-restore-multi-nst";

            pub(crate) struct Multi {
                pub conn: Connection,
                pub paths: TapectlPaths,
                pub fake: FakeTape,
                /// The tree archived, `(path relative to the root, bytes)`.
                pub files: Vec<(String, Vec<u8>)>,
                /// Each slice's ciphertext length, slice 1 first.
                pub cipher_lens: Vec<i64>,
                /// Every unit on the volume, in tape order.
                pub units: Vec<UnitFixture>,
                pub _home: TempDir,
                pub _drive: InjectedDrive,
            }

            /// Where [`Multi::damage`] hits a slice's age file.
            #[derive(Clone, Copy)]
            pub(crate) enum Spot {
                /// Inside the header's MAC line: the file does not open.
                HeaderMac,
                /// Inside the 16-byte payload nonce: the file opens and no
                /// chunk authenticates.
                Nonce,
            }

            /// `(unit name, its files as (path, bytes), dar slice size)`.
            pub(crate) type UnitSpec<'a> = (&'a str, &'a [(&'a str, Vec<u8>)], &'a str);

            pub(crate) struct UnitFixture {
                pub name: String,
                pub files: Vec<(String, Vec<u8>)>,
                /// Tape position of the unit's slice 1.
                pub first_position: u32,
                pub cipher_lens: Vec<i64>,
            }

            impl Multi {
                pub(crate) fn slices(&self) -> usize {
                    self.cipher_lens.len()
                }

                /// The slice reads the tape saw, as slice numbers (File 0,
                /// the contact check, left out).
                pub(crate) fn slice_reads(&self) -> Vec<u32> {
                    self.fake
                        .ops()
                        .iter()
                        .filter_map(|op| match op {
                            crate::tape::fake::Op::Read(n) if *n >= FIRST => Some(n - FIRST + 1),
                            _ => None,
                        })
                        .collect()
                }

                /// Flip one byte of slice `slice` (1-based) as the tape holds
                /// it, at a landmark of its age file found in the bytes
                /// themselves. NEVER an absolute offset: age puts a grease
                /// stanza of random length (a tag of 1-8 characters, up to
                /// four arguments, a body of up to 99 bytes) before the MAC
                /// line, so the header ends anywhere from about byte 195 to
                /// byte 370 and a fixed offset lands in the header on one
                /// encryption and in the payload nonce on the next (#484).
                pub(crate) fn damage(&self, slice: usize, spot: Spot) {
                    let mut tape = self.fake.state();
                    let file = &mut tape.files[FIRST as usize + slice - 1];
                    // The MAC line is the only one that starts with `---`:
                    // stanza lines start `->` and their bodies are base64.
                    let mac = file
                        .windows(5)
                        .position(|w| w == b"\n--- ")
                        .expect("an age header's MAC line")
                        + 5;
                    let at = match spot {
                        // A character of the MAC itself: age refuses the header.
                        Spot::HeaderMac => mac + 5,
                        // The payload nonce follows the MAC's 43 characters and
                        // its newline. It lies outside the MAC, so the header
                        // opens and the key unwraps, but every chunk then fails
                        // to authenticate.
                        Spot::Nonce => mac + 43 + 1 + 3,
                    };
                    file[at] ^= 0xFF;
                }

                /// Forget the catalogue, as a catalog rebuilt from tape
                /// does: the restore must then spool.
                pub(crate) fn drop_catalogue(&self) {
                    self.conn
                        .execute("UPDATE stage_sets SET catalog_path = NULL", [])
                        .unwrap();
                }

                pub(crate) fn unit(
                    &self,
                    label: &str,
                    unit: &str,
                    dest: &Path,
                    options: &RestoreOptions,
                ) -> Result<RestoreReport> {
                    restore_unit(
                        &self.conn,
                        &self.paths,
                        &Config::default(),
                        unit,
                        label,
                        &dest.to_string_lossy(),
                        DEVICE,
                        4096,
                        None,
                        false,
                        options,
                    )
                }

                pub(crate) fn file(
                    &self,
                    label: &str,
                    unit: &str,
                    path: &str,
                    dest: &Path,
                    options: &RestoreOptions,
                ) -> Result<()> {
                    restore_file(
                        &self.conn,
                        &self.paths,
                        &Config::default(),
                        unit,
                        path,
                        label,
                        &dest.to_string_lossy(),
                        DEVICE,
                        4096,
                        None,
                        options,
                    )
                    .map(|_| ())
                }

                /// Every archived file is in `dest`, byte for byte.
                pub(crate) fn assert_restored(&self, dest: &Path) {
                    for (path, bytes) in &self.files {
                        assert_eq!(
                            &fs::read(dest.join(path)).unwrap_or_default(),
                            bytes,
                            "{path} restored wrong"
                        );
                    }
                }
            }

            /// Pseudo-random bytes (incompressible), seeded so each file
            /// differs.
            pub(crate) fn noise(len: usize, seed: u32) -> Vec<u8> {
                (0..len as u32)
                    .map(|i| (i.wrapping_add(seed).wrapping_mul(2_654_435_761) >> 13) as u8)
                    .collect()
            }

            pub(crate) fn multi(
                label: &str,
                unit: &str,
                files: &[(&str, Vec<u8>)],
                slice_size: &str,
                catalogue: bool,
            ) -> Multi {
                volume_of(label, &[(unit, files, slice_size)], catalogue)
            }

            /// [`multi`] with `dar_args` added to its `dar -c`.
            pub(crate) fn multi_with(
                label: &str,
                unit: &str,
                files: &[(&str, Vec<u8>)],
                slice_size: &str,
                dar_args: &[&str],
            ) -> Multi {
                volume_with(label, &[(unit, files, slice_size)], true, dar_args)
            }

            /// [`multi`] for several units, one after another on the tape
            /// in the order given (slices of the first from [`FIRST`]),
            /// each its own unit, snapshot v1, stage set and completed
            /// write on the one volume — `stage create` and `volume write`
            /// in miniature. [`Multi::files`] / [`Multi::cipher_lens`]
            /// describe the FIRST unit; [`Multi::units`] every unit.
            pub(crate) fn volume_of(label: &str, units: &[UnitSpec<'_>], catalogue: bool) -> Multi {
                volume_with(label, units, catalogue, &[])
            }

            /// [`volume_of`], with `dar_args` added to every unit's `dar -c`.
            fn volume_with(
                label: &str,
                units: &[UnitSpec<'_>],
                catalogue: bool,
                dar_args: &[&str],
            ) -> Multi {
                let conn = crate::db::open_memory().unwrap();
                let home = TempDir::new().unwrap();
                let paths = TapectlPaths::new(home.path().join(".tapectl"));
                paths.ensure_dirs().unwrap();
                let kp = keys::generate_and_save(&paths.keys_dir, "t1", "primary").unwrap();
                conn.execute(
                    "INSERT INTO tenants (name, is_operator, status) VALUES ('t1', 0, 'active')",
                    [],
                )
                .unwrap();
                let tenant_id = conn.last_insert_rowid();
                conn.execute(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
                     VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
                    params![label],
                )
                .unwrap();
                let volume_id = conn.last_insert_rowid();

                let mut mem = tape_labelled(label);
                for filler in 1..FIRST {
                    mem.execute(&mut Cursor::new(vec![filler as u8; 100]), 100, false)
                        .unwrap();
                }
                let mut fixtures = Vec::new();
                for (k, (name, files, slice_size)) in units.iter().enumerate() {
                    conn.execute(
                        "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                         VALUES (?1, ?2, ?3, 'mtime_size', 1, 'active')",
                        params![format!("unit{k:04}-uuid"), name, tenant_id],
                    )
                    .unwrap();
                    let unit_id = conn.last_insert_rowid();
                    conn.execute(
                        "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
                         VALUES (?1, 1, 'staged', '/tmp', 1, 16)",
                        params![unit_id],
                    )
                    .unwrap();
                    let snapshot_id = conn.last_insert_rowid();
                    conn.execute(
                        "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
                        params![snapshot_id],
                    )
                    .unwrap();
                    let ss_id = conn.last_insert_rowid();
                    conn.execute(
                        "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                         VALUES (?1, ?2, ?3, 'completed')",
                        params![ss_id, snapshot_id, volume_id],
                    )
                    .unwrap();
                    let write_id = conn.last_insert_rowid();

                    let work = TempDir::new().unwrap();
                    let src = work.path().join("src");
                    fs::create_dir_all(&src).unwrap();
                    let mut dirs = std::collections::BTreeSet::new();
                    for (path, bytes) in files.iter() {
                        let at = src.join(path);
                        fs::create_dir_all(at.parent().unwrap()).unwrap();
                        fs::write(&at, bytes).unwrap();
                        crate::db::files::fixture::insert(
                            &conn,
                            snapshot_id,
                            path,
                            bytes.len() as i64,
                            "regular",
                            None,
                        );
                        let mut parent = Path::new(path).parent();
                        while let Some(p) = parent.filter(|p| !p.as_os_str().is_empty()) {
                            dirs.insert(p.to_string_lossy().into_owned());
                            parent = p.parent();
                        }
                    }
                    for dir in dirs {
                        crate::db::files::fixture::insert(&conn, snapshot_id, &dir, 0, "dir", None);
                    }
                    let base = work.path().join("arch");
                    let created = std::process::Command::new("dar")
                        .arg("-c")
                        .arg(&base)
                        .arg("-R")
                        .arg(&src)
                        .arg("-s")
                        .arg(slice_size)
                        .arg("-Q")
                        .args(dar_args)
                        .output()
                        .unwrap();
                    assert!(
                        created.status.success(),
                        "dar -c failed in test setup: {}",
                        String::from_utf8_lossy(&created.stderr)
                    );
                    if catalogue {
                        // Where and how `stage create` isolates it.
                        let dir = paths.catalogs_dir.join(format!("unit{k:04}"));
                        fs::create_dir_all(&dir).unwrap();
                        let catalogue = dir.join(format!("unit{k:04}_v1"));
                        crate::dar::create::extract_catalog("dar", &base, &catalogue).unwrap();
                        conn.execute(
                            "UPDATE stage_sets SET catalog_path = ?1 WHERE id = ?2",
                            params![catalogue.to_string_lossy(), ss_id],
                        )
                        .unwrap();
                    }
                    let plains: Vec<Vec<u8>> = (1..)
                        .map(|n| work.path().join(format!("arch.{n}.dar")))
                        .take_while(|p| p.exists())
                        .map(|p| fs::read(p).unwrap())
                        .collect();
                    let first_position = mem.files.len() as u32;
                    let mut cipher_lens = Vec::new();
                    for (i, plain) in plains.iter().enumerate() {
                        let cipher = encrypt_to(plain, std::slice::from_ref(&kp.public_key));
                        let position = mem.files.len().to_string();
                        mem.execute(&mut Cursor::new(cipher.clone()), cipher.len() as u64, false)
                            .unwrap();
                        cipher_lens.push(cipher.len() as i64);
                        conn.execute(
                            "INSERT INTO stage_slices
                                (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                 sha256_plain, sha256_encrypted)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            params![
                                ss_id,
                                i as i64 + 1,
                                plain.len() as i64,
                                cipher.len() as i64,
                                direct_hash(plain),
                                direct_hash(&cipher)
                            ],
                        )
                        .unwrap();
                        let slice_id = conn.last_insert_rowid();
                        conn.execute(
                            "INSERT INTO write_positions
                                (write_id, stage_slice_id, position, status, sha256_on_volume)
                             VALUES (?1, ?2, ?3, 'written', ?4)",
                            params![write_id, slice_id, position, direct_hash(&cipher)],
                        )
                        .unwrap();
                    }
                    fixtures.push(UnitFixture {
                        name: name.to_string(),
                        files: files
                            .iter()
                            .map(|(p, b)| (p.to_string(), b.clone()))
                            .collect(),
                        first_position,
                        cipher_lens,
                    });
                }
                let fake = FakeTape::with_files(mem.files.clone(), 4096);
                let drive = InjectedDrive::install(&fake);
                Multi {
                    conn,
                    paths,
                    fake,
                    files: fixtures[0].files.clone(),
                    cipher_lens: fixtures[0].cipher_lens.clone(),
                    units: fixtures,
                    _home: home,
                    _drive: drive,
                }
            }
        }

        mod streaming {
            use super::multi::{multi, multi_with, noise, Multi, Spot, FIRST};
            use super::record::only_row;
            use super::*;
            use crate::tape::fake::Op;

            fn tree() -> Vec<(&'static str, Vec<u8>)> {
                vec![
                    ("big.bin", noise(300_000, 1)),
                    ("mid.bin", noise(150_000, 2)),
                    ("small.txt", b"small".to_vec()),
                    ("sub/deep.txt", b"deep".to_vec()),
                ]
            }

            /// THE acceptance test for #411's unit half: with the isolated
            /// catalogue `stage create` keeps, a multi-slice unit restores
            /// with free space for the DESTINATION ONLY — each slice is
            /// decrypted off the tape straight into a named pipe that `dar
            /// --sequential-read -A <catalogue>` reads, so nothing of it is
            /// ever on disk in scratch, in one forward pass.
            ///
            /// The tree is the shape that breaks dar 2.7.13's sequential
            /// read WITHOUT the catalogue (a large file running into the
            /// last slice): the restore must be identical anyway.
            #[test]
            fn a_unit_with_its_catalogue_streams_with_no_scratch_space() {
                let m = multi("ST-1", "st-unit", &tree(), "150k", true);
                assert!(m.slices() >= 3, "want a multi-slice archive");
                let dest = TempDir::new().unwrap();
                let need: i64 = m.cipher_lens.iter().sum();
                let _free = RestoreFreeOverride::set(need);
                m.fake.watch(&dest.path().join(SCRATCH_NAME));

                m.unit("ST-1", "st-unit", dest.path(), &RestoreOptions::default())
                    .expect("a streamed restore needs only the destination's space");
                m.assert_restored(dest.path());

                let argv = only_row(&m.conn).dar_argv.expect("dar ran");
                assert!(argv.contains(r#""--sequential-read""#), "{argv}");
                assert!(argv.contains(r#""-A""#), "{argv}");
                let bytes = m.fake.watched_bytes();
                assert_eq!(bytes.len(), m.slices() + 1, "{bytes:?}");
                assert!(
                    bytes.iter().all(|(_, b)| *b == 0),
                    "a decrypted slice was on disk while the tape was read: {bytes:?}"
                );
                let mut expected = vec![Op::Rewind, Op::ReadHead(0), Op::Space(FIRST - 1)];
                expected.extend((0..m.slices() as u32).map(|i| Op::Read(FIRST + i)));
                assert_eq!(m.fake.ops(), expected, "one forward pass");
            }

            /// The positive control: the same unit with no catalogue (a
            /// catalog rebuilt from tape has none) spools — its decrypted
            /// slices ARE on disk in scratch while the tape is read, and the
            /// space check asks for them.
            #[test]
            fn without_its_catalogue_a_unit_spools_and_the_space_check_says_so() {
                let m = multi("ST-2", "st-unit", &tree(), "150k", true);
                m.drop_catalogue();
                let dest = TempDir::new().unwrap();
                let need: i64 = m.cipher_lens.iter().sum();
                {
                    let _free = RestoreFreeOverride::set(need);
                    let err = m
                        .unit("ST-2", "st-unit", dest.path(), &RestoreOptions::default())
                        .unwrap_err()
                        .to_string();
                    assert!(err.contains("not enough disk space"), "{err}");
                }
                m.fake.watch(&dest.path().join(SCRATCH_NAME));
                m.unit("ST-2", "st-unit", dest.path(), &RestoreOptions::default())
                    .expect("a spooled restore");
                m.assert_restored(dest.path());
                assert!(
                    m.fake.watched_bytes().iter().any(|(_, b)| *b > 0),
                    "spooled slices are on disk: {:?}",
                    m.fake.watched_bytes()
                );
                let argv = only_row(&m.conn).dar_argv.expect("dar ran");
                assert!(!argv.contains("--sequential-read"), "{argv}");
            }

            // ── #497: dar, given the catalogue, ends before the archive's own ──
            //
            // With `-A` dar knows where the file data ends and stops reading
            // there, before the archive's trailing catalogue, and exits 0. A
            // tail bigger than a pipe's buffer used to fail the restore ("dar
            // stopped reading slice N"), and a slice dar never opened ("dar
            // stopped before it read slice N"), over a unit restored whole. A
            // unit needs many files for that: 2000 here, with long names.
            //
            // The archives here carry no filesystem-specific attributes
            // (`--fsa-scope none`): dar 2.7.13 reading some archives through
            // FIFOs misreads a file's FSA record ("invalid FSA nature flag",
            // exit 5, every byte restored), an archive the same dar reads
            // from regular files without a word. That is a defect of its own
            // (measured for #497), and it is kept out of these tests.
            const NO_FSA: &[&str] = &["--fsa-scope", "none"];

            /// The restore of `label`, failing with dar's own report.
            fn restored(m: &Multi, label: &str, dest: &Path, options: &RestoreOptions) {
                if let Err(e) = m.unit(label, "bc-unit", dest, options) {
                    let row = only_row(&m.conn);
                    panic!(
                        "the unit is whole, yet: {e}\ndar exit {:?}\n{}{}",
                        row.dar_exit_code,
                        row.dar_stdout.unwrap_or_default(),
                        row.dar_stderr.unwrap_or_default()
                    );
                }
            }

            /// 2000 names of about 200 characters: what makes dar's
            /// catalogue large with few files.
            fn many_names() -> Vec<String> {
                (0..2000)
                    .map(|i| format!("{i:04}-{}.bin", "n".repeat(190)))
                    .collect()
            }

            fn many_tree(names: &[String], file_len: usize) -> Vec<(&str, Vec<u8>)> {
                names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| (n.as_str(), noise(file_len, i as u32 + 7)))
                    .collect()
            }

            /// The fixture's own control: the isolated catalogue dar gets as
            /// `-A` is far bigger than a pipe's buffer (64 KiB), so the tail
            /// dar leaves unread cannot hide in one.
            fn assert_big_catalogue(m: &Multi) -> u64 {
                let base: String = m
                    .conn
                    .query_row("SELECT catalog_path FROM stage_sets", [], |r| {
                        r.get::<_, String>(0)
                    })
                    .unwrap();
                let len = fs::metadata(format!("{base}.1.dar")).unwrap().len();
                assert!(
                    len > 512 * 1024,
                    "the catalogue is {len} bytes: the test needs one above 512 KiB"
                );
                len
            }

            /// The shape home2 met: 2 MiB slices, the catalogue inside the
            /// last one after the end of the data. dar stops there and exits
            /// 0, the restore is whole, and every slice is read once, forward
            /// (the last to its end).
            #[test]
            fn a_unit_whose_catalogue_dar_leaves_unread_streams_whole() {
                let names = many_names();
                let m = multi_with("BC-1", "bc-unit", &many_tree(&names, 2000), "2M", NO_FSA);
                let cat = assert_big_catalogue(&m);
                assert!(m.slices() >= 2, "want several slices");
                assert!(
                    cat < *m.cipher_lens.last().unwrap() as u64,
                    "the catalogue fits in the last slice, after the end of the data"
                );
                let dest = TempDir::new().unwrap();
                restored(&m, "BC-1", dest.path(), &RestoreOptions::default());
                m.assert_restored(dest.path());
                let argv = only_row(&m.conn).dar_argv.expect("dar ran");
                assert!(argv.contains(r#""--sequential-read""#), "{argv}");
                assert!(argv.contains(r#""-A""#), "{argv}");
                let mut expected = vec![Op::Rewind, Op::ReadHead(0), Op::Space(FIRST - 1)];
                expected.extend((0..m.slices() as u32).map(|i| Op::Read(FIRST + i)));
                assert_eq!(m.fake.ops(), expected, "one forward pass");
            }

            /// The other shape: 100 KiB slices, the catalogue filling the
            /// last few, which dar exits 0 without opening. They are still
            /// read off the tape and authenticated, into nothing.
            #[test]
            fn a_unit_whose_last_slices_dar_never_opens_streams_whole() {
                let names = many_names();
                let m = multi_with("BC-2", "bc-unit", &many_tree(&names, 300), "100k", NO_FSA);
                assert_big_catalogue(&m);
                assert!(m.slices() >= 10, "want many slices");
                let dest = TempDir::new().unwrap();
                restored(&m, "BC-2", dest.path(), &RestoreOptions::default());
                m.assert_restored(dest.path());
                let argv = only_row(&m.conn).dar_argv.expect("dar ran");
                assert!(argv.contains(r#""--sequential-read""#), "{argv}");
                let mut expected = vec![Op::Rewind, Op::ReadHead(0), Op::Space(FIRST - 1)];
                expected.extend((0..m.slices() as u32).map(|i| Op::Read(FIRST + i)));
                assert_eq!(m.fake.ops(), expected, "every slice read, forward");
            }

            /// The way round #497 on a tapectl without the fix (ADR-0012 item
            /// 178): `--spool` decrypts every slice to scratch and dar reads
            /// them directly, the trailing catalogue too. It held before the
            /// fix and must go on holding.
            #[test]
            fn spool_restores_a_unit_whose_catalogue_dar_would_leave_unread() {
                let names = many_names();
                let m = multi_with("BC-3", "bc-unit", &many_tree(&names, 2000), "2M", NO_FSA);
                assert_big_catalogue(&m);
                let dest = TempDir::new().unwrap();
                let spool = RestoreOptions {
                    spool: true,
                    ..RestoreOptions::default()
                };
                restored(&m, "BC-3", dest.path(), &spool);
                m.assert_restored(dest.path());
                let argv = only_row(&m.conn).dar_argv.expect("dar ran");
                assert!(!argv.contains("--sequential-read"), "{argv}");
            }

            /// A slice whose age header does not open cannot be read around:
            /// no chunk of it can be told from another, so nothing can be a
            /// hole. The restore fails naming the slice and saying dar may
            /// have written part of the unit into --to; the slice is read to
            /// its end (no rewind), and no later slice is read.
            ///
            /// This is the case where a bad slice fails a streamed restore
            /// outright. A slice whose header opens but whose chunks do not
            /// authenticate is a hole instead and the restore goes on past it
            /// (`a_slice_whose_every_chunk_fails_is_a_hole_and_the_restore_is_incomplete`,
            /// and `repair_tests` for one bad chunk).
            ///
            /// The byte is a landmark of the age file, not an offset: this
            /// test used to flip byte 200, which was in the header on 93
            /// encryptions of 100 and in the nonce on the other 7, because
            /// age's grease stanza makes the header's length random (#484).
            #[test]
            fn a_corrupt_slice_mid_stream_names_the_slice_and_the_partial_extract() {
                let m = multi("ST-3", "st-unit", &tree(), "150k", true);
                m.damage(2, Spot::HeaderMac);
                let dest = TempDir::new().unwrap();
                let err = m
                    .unit("ST-3", "st-unit", dest.path(), &RestoreOptions::default())
                    .unwrap_err()
                    .to_string();
                assert!(err.contains("slice 2 checksum mismatch"), "{err}");
                assert!(err.contains("partial"), "{err}");
                assert!(err.contains("cannot be read around"), "{err}");
                assert_eq!(m.slice_reads(), vec![1, 2], "{:?}", m.fake.ops());
                assert_eq!(m.fake.rewinds(), 1);
                assert!(
                    !dest.path().join(SCRATCH_NAME).exists(),
                    "the pipes are cleaned up"
                );
            }

            /// The other half of #484, which the test above used to hit one
            /// time in fourteen: a byte of the payload nonce, outside the
            /// header's MAC. The header opens and the key unwraps, but no
            /// chunk of the slice authenticates, and the only copy has
            /// nothing to substitute, so slice 2 is delivered as a hole of
            /// zeros from end to end and dar is fed on past it.
            ///
            /// dar, reading on past a slice of zeros, exits 0 having
            /// skipped files it had begun itself in the same run (its
            /// "not restored (user choice)" lines: with no terminal it
            /// answers its own overwrite question no). It is ONE dar into a
            /// destination that was empty; no tapectl path re-feeds a slice
            /// or runs a second extract. The restore must end INCOMPLETE,
            /// name the slice, its holes and the files, write no byte that
            /// did not authenticate, and not call dar's own files stale
            /// copies.
            #[test]
            fn a_slice_whose_every_chunk_fails_is_a_hole_and_the_restore_is_incomplete() {
                let m = multi("ST-4", "st-unit", &tree(), "150k", true);
                m.damage(2, Spot::Nonce);
                let dest = TempDir::new().unwrap();
                let err = m
                    .unit("ST-4", "st-unit", dest.path(), &RestoreOptions::default())
                    .unwrap_err()
                    .to_string();
                assert!(err.contains("INCOMPLETE"), "{err}");
                assert!(err.contains("slice 2 on ST-4"), "{err}");
                assert!(err.contains("hole(s) of zeros"), "{err}");
                assert!(err.contains("0..65536"), "every chunk: {err}");
                assert!(
                    !err.contains("partial restore"),
                    "a hole is not a stream that failed: {err}"
                );
                assert!(err.contains("big.bin"), "the file that spans it: {err}");
                assert!(
                    !err.contains("stale") && !err.contains("remove those files"),
                    "dar's own files are not stale copies, and the destination was empty: {err}"
                );
                // No unauthenticated byte: whatever dar left is the original or
                // the zeros of a hole.
                for (path, original) in &m.files {
                    let Ok(got) = fs::read(dest.path().join(path)) else {
                        continue;
                    };
                    assert!(got.len() <= original.len(), "{path}: {} bytes", got.len());
                    assert!(
                        got.iter().zip(original).all(|(g, o)| g == o || *g == 0),
                        "{path} carries a byte that is neither the original nor a hole"
                    );
                }
                assert_eq!(m.fake.rewinds(), 1);
                assert!(
                    !dest.path().join(SCRATCH_NAME).exists(),
                    "the pipes are cleaned up"
                );
            }

            /// THE acceptance test for #411's file half: `restore file`
            /// reads only the slices dar needs for the one file — its own,
            /// the directories' above it, and the last (dar's catalogue) —
            /// in one forward pass, never the whole unit.
            #[test]
            fn restore_file_reads_only_the_slices_that_hold_it() {
                let m = multi("SF-1", "sf-unit", &tree(), "64k", true);
                let all = m.slices() as u32;
                assert!(all >= 5, "want a many-slice archive, got {all}");
                let dest = TempDir::new().unwrap();
                m.file(
                    "SF-1",
                    "sf-unit",
                    "small.txt",
                    dest.path(),
                    &RestoreOptions::default(),
                )
                .unwrap();
                assert_eq!(fs::read(dest.path().join("small.txt")).unwrap(), b"small");

                let reads = m.slice_reads();
                assert!(
                    !reads.is_empty() && reads.len() <= 3,
                    "a one-slice file reads at most its slice(s) and the last: {reads:?}"
                );
                assert!(
                    reads.windows(2).all(|w| w[0] < w[1]),
                    "ascending: {reads:?}"
                );
                assert_eq!(reads.last(), Some(&all), "dar's catalogue is in the last");
                assert_eq!(m.fake.rewinds(), 1);
                let row = only_row(&m.conn);
                assert_eq!(row.slices_read, Some(reads.len() as i64));
            }

            /// A file below directories needs their slices too (their
            /// attributes are restored with them) — and still not the whole
            /// unit; the file comes back identical.
            #[test]
            fn restore_file_of_a_nested_file_reads_its_ancestors_slices_too() {
                let m = multi("SF-2", "sf-unit", &tree(), "64k", true);
                let all = m.slices() as u32;
                let dest = TempDir::new().unwrap();
                m.file(
                    "SF-2",
                    "sf-unit",
                    "sub/deep.txt",
                    dest.path(),
                    &RestoreOptions::default(),
                )
                .unwrap();
                assert_eq!(fs::read(dest.path().join("deep.txt")).unwrap(), b"deep");
                let reads = m.slice_reads();
                assert!((reads.len() as u32) < all, "{reads:?} of {all}");
                assert_eq!(reads.last(), Some(&all));
            }

            /// The catalogue `stage create` isolated belongs to the dar run
            /// that made it — the snapshot's first staging. A later staging
            /// of the same version (another `dar -c`) records the same
            /// path, but dar refuses that pairing FATAL, so it must not be
            /// used: the later stage set spools.
            #[test]
            fn a_catalogue_from_another_staging_of_the_version_is_not_used() {
                let m = multi("ST-OWN", "st-unit", &tree(), "150k", true);
                let first: i64 = m
                    .conn
                    .query_row("SELECT id FROM stage_sets", [], |r| r.get(0))
                    .unwrap();
                let base = own_catalogue(&m.conn, first).unwrap();
                assert!(
                    base.is_some(),
                    "positive control: the first staging owns it"
                );
                m.conn
                    .execute(
                        "INSERT INTO stage_sets (snapshot_id, status, slice_size, catalog_path)
                         SELECT snapshot_id, 'staged', slice_size, catalog_path
                         FROM stage_sets WHERE id = ?1",
                        params![first],
                    )
                    .unwrap();
                let later = m.conn.last_insert_rowid();
                assert_eq!(own_catalogue(&m.conn, later).unwrap(), None);
                assert!(own_catalogue(&m.conn, first).unwrap().is_some());

                // Once each staging records its OWN catalogue (#419), the
                // later one is its own first, and is used.
                let base = base.unwrap();
                let own = base.with_file_name("later_v1");
                fs::copy(
                    format!("{}.1.dar", base.display()),
                    format!("{}.1.dar", own.display()),
                )
                .unwrap();
                m.conn
                    .execute(
                        "UPDATE stage_sets SET catalog_path = ?1 WHERE id = ?2",
                        params![own.to_string_lossy(), later],
                    )
                    .unwrap();
                assert_eq!(own_catalogue(&m.conn, later).unwrap(), Some(own));
            }

            /// Without a catalogue there is nothing to say which slices hold
            /// the file: every slice is read, as before.
            #[test]
            fn restore_file_without_a_catalogue_reads_every_slice() {
                let m = multi("SF-3", "sf-unit", &tree(), "64k", true);
                m.drop_catalogue();
                let dest = TempDir::new().unwrap();
                m.file(
                    "SF-3",
                    "sf-unit",
                    "small.txt",
                    dest.path(),
                    &RestoreOptions::default(),
                )
                .unwrap();
                assert_eq!(fs::read(dest.path().join("small.txt")).unwrap(), b"small");
                assert_eq!(m.slice_reads(), (1..=m.slices() as u32).collect::<Vec<_>>());
            }
        }

        // ── issue #398: several units from one volume, one tape pass ──

        mod several_units {
            use super::multi::{noise, volume_of, Multi, DEVICE};
            use super::*;
            use crate::tape::fake::Op;
            use crate::volume::restore_record::rows;

            fn three(catalogue: bool) -> Multi {
                let a = [("a.bin", noise(90_000, 1)), ("a.txt", b"alpha".to_vec())];
                let b = [("b.bin", noise(120_000, 2)), ("d/b.txt", b"bravo".to_vec())];
                let c = [("c.txt", b"charlie".to_vec())];
                volume_of(
                    "MU-1",
                    &[("ua", &a, "64k"), ("ub", &b, "64k"), ("uc", &c, "64k")],
                    catalogue,
                )
            }

            fn requests(m: &Multi, root: &Path) -> Vec<UnitRequest> {
                m.units
                    .iter()
                    .map(|u| UnitRequest {
                        unit: u.name.clone(),
                        version: None,
                        dest_dir: root.join(&u.name).to_string_lossy().into_owned(),
                    })
                    .collect()
            }

            fn run(m: &Multi, reqs: &[UnitRequest], fail_fast: bool) -> Result<UnitsReport> {
                restore_units(
                    &m.conn,
                    &m.paths,
                    &Config::default(),
                    "MU-1",
                    reqs,
                    &RestoreOptions::default(),
                    fail_fast,
                    DEVICE,
                    4096,
                    false,
                )
            }

            fn reads(m: &Multi) -> Vec<u32> {
                m.fake
                    .ops()
                    .iter()
                    .filter_map(|op| match op {
                        // File 0's bounded read is a `ReadHead` (#400).
                        Op::Read(n) | Op::ReadHead(n) => Some(*n),
                        _ => None,
                    })
                    .collect()
            }

            fn count(m: &Multi, sql: &str) -> i64 {
                m.conn.query_row(sql, [], |r| r.get(0)).unwrap()
            }

            fn assert_unit_restored(m: &Multi, k: usize, root: &Path) {
                for (path, bytes) in &m.units[k].files {
                    assert_eq!(
                        &fs::read(root.join(&m.units[k].name).join(path)).unwrap_or_default(),
                        bytes,
                        "{}: {path}",
                        m.units[k].name
                    );
                }
            }

            /// THE acceptance test for #398: three units restored in one
            /// session open the drive once and rewind once, and read the
            /// tape strictly forward — File 0 once, then every slice of
            /// every unit in ascending position. One contact, one
            /// `restores` row per unit. Asked for in NON-tape order, so the
            /// order is the restore's own doing. Spooled (no catalogues),
            /// streamed below.
            #[test]
            fn three_units_are_one_rewind_and_one_forward_pass() {
                for catalogue in [false, true] {
                    let m = three(catalogue);
                    let root = TempDir::new().unwrap();
                    let mut reqs = requests(&m, root.path());
                    reqs.reverse();
                    let report = run(&m, &reqs, false).unwrap();
                    assert_eq!(report.failed(), 0, "{report:?}");
                    for k in 0..3 {
                        assert_unit_restored(&m, k, root.path());
                    }
                    let names: Vec<&str> =
                        report.units.iter().map(|u| u.unit_name.as_str()).collect();
                    assert_eq!(names, ["ua", "ub", "uc"], "outcomes in tape order");

                    assert_eq!(m.fake.opens().len(), 1, "the drive is opened once");
                    assert_eq!(m.fake.rewinds(), 1, "{:?}", m.fake.ops());
                    let reads = reads(&m);
                    assert_eq!(reads[0], 0, "File 0, once: {reads:?}");
                    assert!(
                        reads.windows(2).all(|w| w[0] < w[1]),
                        "strictly ascending: {reads:?}"
                    );
                    let slices: usize = m.units.iter().map(|u| u.cipher_lens.len()).sum();
                    assert_eq!(reads.len(), slices + 1, "{reads:?}");
                    assert_eq!(count(&m, "SELECT COUNT(*) FROM cartridge_contacts"), 1);
                    let rows = rows(&m.conn).unwrap();
                    assert_eq!(rows.len(), 3);
                    assert!(rows.iter().all(|r| r.outcome == "ok" && r.kind == "unit"));
                    let contact: i64 = m
                        .conn
                        .query_row("SELECT id FROM cartridge_contacts", [], |r| r.get(0))
                        .unwrap();
                    assert!(rows.iter().all(|r| r.contact_id == Some(contact)));
                }
            }

            /// One unit's failure does not stop the others: B's slice is
            /// corrupt, A and C restore, B is reported (and recorded)
            /// failed — and the pass is still one rewind, forward only.
            #[test]
            fn a_failing_unit_does_not_stop_the_others() {
                let m = three(false);
                let b_first = m.units[1].first_position as usize;
                m.fake.state().files[b_first][300] ^= 0xFF;
                let root = TempDir::new().unwrap();
                let report = run(&m, &requests(&m, root.path()), false).unwrap();
                assert_eq!(report.failed(), 1, "{report:?}");
                let b = &report.units[1];
                assert!(
                    b.error
                        .as_deref()
                        .unwrap_or("")
                        .contains("checksum mismatch"),
                    "{b:?}"
                );
                assert_unit_restored(&m, 0, root.path());
                assert_unit_restored(&m, 2, root.path());
                assert_eq!(m.fake.rewinds(), 1, "{:?}", m.fake.ops());
                assert!(reads(&m).windows(2).all(|w| w[0] < w[1]));
                let outcomes: Vec<String> = rows(&m.conn)
                    .unwrap()
                    .into_iter()
                    .map(|r| r.outcome)
                    .collect();
                assert_eq!(outcomes, ["ok", "failed", "ok"]);
            }

            /// `fail_fast`: the first failure ends the session; the units
            /// after it are reported not attempted, and have no row.
            #[test]
            fn fail_fast_stops_at_the_first_failing_unit() {
                let m = three(false);
                let b_first = m.units[1].first_position as usize;
                m.fake.state().files[b_first][300] ^= 0xFF;
                let root = TempDir::new().unwrap();
                let report = run(&m, &requests(&m, root.path()), true).unwrap();
                assert_eq!(report.failed(), 2, "{report:?}");
                assert!(report.units[0].error.is_none());
                assert!(report.units[1].error.is_some());
                assert!(!report.units[2].attempted, "{report:?}");
                assert_eq!(rows(&m.conn).unwrap().len(), 2);
                assert!(!root.path().join("uc").exists());
            }

            /// Everything that can be refused without the tape is refused
            /// for the WHOLE set before the drive opens: an unknown unit,
            /// a unit asked for twice, two units into one directory, a
            /// destination that is not empty, and a set that does not fit
            /// on the disk though each unit alone would.
            #[test]
            fn the_whole_set_is_checked_before_the_drive_opens() {
                let m = three(false);
                let root = TempDir::new().unwrap();
                let refused = |reqs: &[UnitRequest], want: &str| {
                    let err = run(&m, reqs, false).unwrap_err().to_string();
                    assert!(err.contains(want), "{want}: {err}");
                    assert_eq!(m.fake.opens(), vec![], "the drive was opened: {err}");
                };

                let mut reqs = requests(&m, root.path());
                reqs[2].unit = "nope".into();
                refused(&reqs, "nope");

                let mut reqs = requests(&m, root.path());
                reqs[2].unit = "ua".into();
                refused(&reqs, "more than once");

                let mut reqs = requests(&m, root.path());
                reqs[2].dest_dir = reqs[0].dest_dir.clone();
                refused(&reqs, "same destination");

                let reqs = requests(&m, root.path());
                fs::create_dir_all(&reqs[1].dest_dir).unwrap();
                fs::write(Path::new(&reqs[1].dest_dir).join("x"), b"x").unwrap();
                refused(&reqs, "is not empty");
                fs::remove_dir_all(&reqs[1].dest_dir).unwrap();

                // Spooled, each unit needs about twice itself; one at a
                // time the scratch is reused, the restored units add up.
                let sizes: Vec<i64> = m.units.iter().map(|u| u.cipher_lens.iter().sum()).collect();
                let largest = *sizes.iter().max().unwrap();
                let total: i64 = sizes.iter().sum();
                {
                    let _free = RestoreFreeOverride::set(2 * largest);
                    refused(&reqs, "not enough disk space");
                }
                let _free = RestoreFreeOverride::set(total + largest);
                let report = run(&m, &reqs, false).unwrap();
                assert_eq!(report.failed(), 0, "{report:?}");
            }

            /// The wrong tape in the drive refuses the whole set at the one
            /// corroboration, before any slice is read: one contact, and
            /// every unit's row failed, naming the tape.
            #[test]
            fn the_wrong_tape_refuses_the_set_after_one_file0_read() {
                let m = three(false);
                let other = super::tape_labelled("MU-OTHER").files[0].clone();
                m.fake.state().files[0] = other;
                let root = TempDir::new().unwrap();
                let err = run(&m, &requests(&m, root.path()), false)
                    .unwrap_err()
                    .to_string();
                assert!(err.contains("wrong tape"), "{err}");
                assert_eq!(reads(&m), vec![0], "only File 0 was read");
                assert_eq!(count(&m, "SELECT COUNT(*) FROM cartridge_contacts"), 1);
                let rows = rows(&m.conn).unwrap();
                assert_eq!(rows.len(), 3);
                assert!(rows.iter().all(|r| r.outcome == "failed"));
            }

            /// A dry run plans the whole set — the version and slices of
            /// each unit, in tape order — and touches nothing.
            #[test]
            fn a_dry_run_plans_the_set_without_the_drive() {
                let m = three(false);
                let root = TempDir::new().unwrap();
                let report = restore_units(
                    &m.conn,
                    &m.paths,
                    &Config::default(),
                    "MU-1",
                    &requests(&m, root.path()),
                    &RestoreOptions::default(),
                    false,
                    DEVICE,
                    4096,
                    true,
                )
                .unwrap();
                assert!(report.dry_run);
                let slices: Vec<usize> = report.units.iter().map(|u| u.slices).collect();
                let want: Vec<usize> = m.units.iter().map(|u| u.cipher_lens.len()).collect();
                assert_eq!(slices, want);
                assert_eq!(m.fake.opens(), vec![]);
                assert_eq!(rows(&m.conn).unwrap().len(), 0);
            }
        }

        // ── ADR-0012 2026-10-06 item 23: `restore volume` and `--spool`,
        // driven through the CLI's own `run` over the injected drive ──

        mod restore_volume_cli {
            use super::multi::{multi, noise, volume_of, Multi, DEVICE};
            use super::*;
            use crate::cli::restore::{run, units_report_lines, RestoreCommands};
            use crate::tape::fake::Op;
            use crate::volume::restore_record::rows;

            fn three(label: &str, catalogue: bool) -> Multi {
                let a = [("a.bin", noise(90_000, 1)), ("a.txt", b"alpha".to_vec())];
                let b = [("b.bin", noise(120_000, 2)), ("d/b.txt", b"bravo".to_vec())];
                let c = [("c.txt", b"charlie".to_vec())];
                volume_of(
                    label,
                    &[("ua", &a, "64k"), ("ub", &b, "64k"), ("uc", &c, "64k")],
                    catalogue,
                )
            }

            /// `restore volume <label> --to <to> [--unit …]`.
            fn volume(label: &str, to: &Path, units: &[&str]) -> RestoreCommands {
                RestoreCommands::Volume {
                    label: Some(label.to_string()),
                    to: to.to_string_lossy().into_owned(),
                    units: units.iter().map(|u| u.to_string()).collect(),
                    device: Some(DEVICE.to_string()),
                    scratch: None,
                    overwrite: false,
                    no_space_check: false,
                    spool: false,
                    fail_fast: false,
                    key: Vec::new(),
                    dry_run: false,
                }
            }

            fn with_spool(mut cmd: RestoreCommands) -> RestoreCommands {
                match &mut cmd {
                    RestoreCommands::Volume { spool, .. } | RestoreCommands::Unit { spool, .. } => {
                        *spool = true
                    }
                    _ => unreachable!(),
                }
                cmd
            }

            fn cli(m: &Multi, cmd: &RestoreCommands) -> Result<()> {
                run(&m.conn, &m.paths, &Config::default(), cmd, false, false)
            }

            fn reads(m: &Multi) -> Vec<u32> {
                m.fake
                    .ops()
                    .iter()
                    .filter_map(|op| match op {
                        // File 0's bounded read is a `ReadHead` (#400).
                        Op::Read(n) | Op::ReadHead(n) => Some(*n),
                        _ => None,
                    })
                    .collect()
            }

            fn assert_unit_restored(m: &Multi, k: usize, root: &Path) {
                for (path, bytes) in &m.units[k].files {
                    assert_eq!(
                        &fs::read(root.join(&m.units[k].name).join(path)).unwrap_or_default(),
                        bytes,
                        "{}: {path}",
                        m.units[k].name
                    );
                }
            }

            /// Every row's dar argv, oldest first.
            fn argvs(m: &Multi) -> Vec<String> {
                rows(&m.conn)
                    .unwrap()
                    .into_iter()
                    .map(|r| r.dar_argv.expect("dar ran"))
                    .collect()
            }

            /// THE acceptance test for the CLI half of #398: with no
            /// `--unit`, every unit on the volume is restored, each into
            /// `--to/<unit>`, in ONE pass — one drive open, one rewind, File
            /// 0 once, every slice in ascending position — under ONE contact
            /// recorded as `restore volume`, with a `unit` row per unit.
            #[test]
            fn restore_volume_restores_every_unit_into_its_own_directory_in_one_pass() {
                let m = three("RV-1", false);
                let root = TempDir::new().unwrap();
                let to = root.path().join("out");
                cli(&m, &volume("RV-1", &to, &[])).expect("restore volume");
                for k in 0..3 {
                    assert_unit_restored(&m, k, &to);
                }
                let mut entries: Vec<String> = fs::read_dir(&to)
                    .unwrap()
                    .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect();
                entries.sort();
                assert_eq!(entries, ["ua", "ub", "uc"], "one directory per unit");

                assert_eq!(m.fake.opens().len(), 1, "the drive is opened once");
                assert_eq!(m.fake.rewinds(), 1, "{:?}", m.fake.ops());
                let reads = reads(&m);
                assert_eq!(reads[0], 0, "File 0 first: {reads:?}");
                assert!(
                    reads.windows(2).all(|w| w[0] < w[1]),
                    "strictly ascending: {reads:?}"
                );
                let slices: usize = m.units.iter().map(|u| u.cipher_lens.len()).sum();
                assert_eq!(reads.len(), slices + 1, "{reads:?}");

                let ops: Vec<String> = {
                    let mut stmt = m
                        .conn
                        .prepare("SELECT operation FROM cartridge_contacts")
                        .unwrap();
                    stmt.query_map([], |r| r.get(0))
                        .unwrap()
                        .collect::<rusqlite::Result<_>>()
                        .unwrap()
                };
                assert_eq!(ops, ["restore volume"], "one contact, under its own name");
                let rows = rows(&m.conn).unwrap();
                let dests: Vec<String> = rows.iter().map(|r| r.destination.clone()).collect();
                let want: Vec<String> = ["ua", "ub", "uc"]
                    .iter()
                    .map(|u| to.join(u).to_string_lossy().into_owned())
                    .collect();
                assert_eq!(dests, want);
                assert!(rows.iter().all(|r| r.kind == "unit" && r.outcome == "ok"));
            }

            /// `--unit` names the set: only those units are read and
            /// restored, the others neither read nor created. A dry run of
            /// the same set opens no drive.
            #[test]
            fn restore_volume_unit_restores_only_the_units_named() {
                let m = three("RV-2", false);
                let root = TempDir::new().unwrap();
                let to = root.path().join("out");

                let mut dry = volume("RV-2", &to, &["uc", "ua"]);
                if let RestoreCommands::Volume { dry_run, .. } = &mut dry {
                    *dry_run = true;
                }
                cli(&m, &dry).expect("dry run");
                assert_eq!(m.fake.opens(), vec![], "a dry run opens no drive");

                cli(&m, &volume("RV-2", &to, &["uc", "ua"])).expect("restore volume");
                assert_unit_restored(&m, 0, &to);
                assert_unit_restored(&m, 2, &to);
                assert!(!to.join("ub").exists(), "ub was not asked for");
                let b_first = m.units[1].first_position;
                let b_last = b_first + m.units[1].cipher_lens.len() as u32;
                assert!(
                    reads(&m).iter().all(|n| !(b_first..b_last).contains(n)),
                    "ub's slices were read: {:?}",
                    reads(&m)
                );
                let names: Vec<String> = rows(&m.conn)
                    .unwrap()
                    .into_iter()
                    .map(|r| r.unit_name.unwrap())
                    .collect();
                assert_eq!(names, ["ua", "uc"], "tape order, not the order asked");
            }

            /// A unit that fails makes the command fail — non-zero exit,
            /// naming how many — while the others are still restored; with
            /// `--fail-fast` the units after it are not attempted.
            #[test]
            fn restore_volume_fails_when_any_unit_fails() {
                let m = three("RV-3", false);
                let b_first = m.units[1].first_position as usize;
                m.fake.state().files[b_first][300] ^= 0xFF;
                let root = TempDir::new().unwrap();
                let to = root.path().join("out");
                let err = cli(&m, &volume("RV-3", &to, &[]))
                    .expect_err("a failed unit fails the command")
                    .to_string();
                assert!(err.contains("1 of 3 units not restored from RV-3"), "{err}");
                assert_unit_restored(&m, 0, &to);
                assert_unit_restored(&m, 2, &to);

                let to = root.path().join("fast");
                let mut cmd = volume("RV-3", &to, &[]);
                if let RestoreCommands::Volume { fail_fast, .. } = &mut cmd {
                    *fail_fast = true;
                }
                let err = cli(&m, &cmd).unwrap_err().to_string();
                assert!(err.contains("2 of 3 units not restored"), "{err}");
                assert!(!to.join("uc").exists(), "--fail-fast stopped before uc");
            }

            /// The report prints one line per unit in TAPE order — asked
            /// for backwards here — saying how each ended, then a summary.
            #[test]
            fn the_report_is_in_tape_order_and_names_each_outcome() {
                let m = three("RV-4", false);
                let b_first = m.units[1].first_position as usize;
                m.fake.state().files[b_first][300] ^= 0xFF;
                let root = TempDir::new().unwrap();
                let reqs: Vec<UnitRequest> = ["uc", "ub", "ua"]
                    .iter()
                    .map(|u| UnitRequest {
                        unit: u.to_string(),
                        version: None,
                        dest_dir: root.path().join(u).to_string_lossy().into_owned(),
                    })
                    .collect();
                let report = restore_units(
                    &m.conn,
                    &m.paths,
                    &Config::default(),
                    "RV-4",
                    &reqs,
                    &RestoreOptions::default(),
                    false,
                    DEVICE,
                    4096,
                    false,
                )
                .unwrap();
                let lines = units_report_lines(&report);
                assert_eq!(lines.len(), 4, "{lines:#?}");
                assert!(lines[0].starts_with("restored \"ua\" v1"), "{lines:#?}");
                assert!(lines[1].starts_with("FAILED \"ub\" v1"), "{lines:#?}");
                assert!(lines[1].contains("checksum mismatch"), "{lines:#?}");
                assert!(lines[2].starts_with("restored \"uc\" v1"), "{lines:#?}");
                assert_eq!(lines[3], "2 of 3 unit(s) restored from RV-4");
            }

            /// No volume of that label, or two units of which one would
            /// land inside the other's directory: refused before the drive.
            #[test]
            fn restore_volume_refuses_before_the_drive() {
                let a = [("a.txt", b"alpha".to_vec())];
                let b = [("b.txt", b"bravo".to_vec())];
                let m = volume_of(
                    "RV-5",
                    &[("nest", &a, "64k"), ("nest/inner", &b, "64k")],
                    false,
                );
                let root = TempDir::new().unwrap();
                let err = cli(&m, &volume("NOPE", root.path(), &[]))
                    .unwrap_err()
                    .to_string();
                assert!(err.contains("no volume \"NOPE\""), "{err}");
                let err = cli(&m, &volume("RV-5", root.path(), &[]))
                    .unwrap_err()
                    .to_string();
                assert!(err.contains("inside"), "{err}");
                assert!(err.contains("Nothing was read from tape"), "{err}");
                assert_eq!(m.fake.opens(), vec![], "the drive was opened");
            }

            /// `restore volume --spool`: units whose catalogues are on disk
            /// (they would stream) are spooled instead — dar reads files in
            /// direct mode, no `--sequential-read`. The same set without the
            /// flag streams: the positive control.
            #[test]
            fn restore_volume_spool_forces_the_spooled_path() {
                let m = three("RV-6", true);
                let root = TempDir::new().unwrap();
                let streamed = root.path().join("streamed");
                cli(&m, &volume("RV-6", &streamed, &[])).unwrap();
                let argv = argvs(&m);
                assert_eq!(argv.len(), 3);
                assert!(
                    argv.iter().all(|a| a.contains(r#""--sequential-read""#)),
                    "positive control: without --spool these stream: {argv:#?}"
                );

                let spooled = root.path().join("spooled");
                cli(&m, &with_spool(volume("RV-6", &spooled, &[]))).unwrap();
                for k in 0..3 {
                    assert_unit_restored(&m, k, &spooled);
                }
                let argv = argvs(&m);
                assert_eq!(argv.len(), 6);
                assert!(
                    argv[3..].iter().all(|a| !a.contains("--sequential-read")),
                    "--spool must not stream: {argv:#?}"
                );
            }

            /// `restore unit --spool`, the same: a unit that would stream
            /// spools, and its decrypted slices are on disk in scratch while
            /// the tape is read.
            #[test]
            fn restore_unit_spool_forces_the_spooled_path() {
                let tree = [
                    ("big.bin", noise(300_000, 1)),
                    ("small.txt", b"small".to_vec()),
                ];
                let m = multi("RU-SP", "sp-unit", &tree, "150k", true);
                let unit = |to: &Path| RestoreCommands::Unit {
                    unit: "sp-unit".into(),
                    from: Some("RU-SP".into()),
                    to: to.to_string_lossy().into_owned(),
                    device: Some(DEVICE.into()),
                    version: None,
                    scratch: None,
                    overwrite: false,
                    no_space_check: false,
                    spool: false,
                    key: Vec::new(),
                    dry_run: false,
                };
                let root = TempDir::new().unwrap();
                cli(&m, &unit(&root.path().join("streamed"))).unwrap();
                assert!(
                    argvs(&m)[0].contains(r#""--sequential-read""#),
                    "positive control: without --spool it streams"
                );

                let spooled = root.path().join("spooled");
                m.fake.watch(&spooled.join(SCRATCH_NAME));
                cli(&m, &with_spool(unit(&spooled))).unwrap();
                m.assert_restored(&spooled);
                let argv = &argvs(&m)[1];
                assert!(!argv.contains("--sequential-read"), "{argv}");
                assert!(
                    m.fake.watched_bytes().iter().any(|(_, b)| *b > 0),
                    "spooled slices are on disk: {:?}",
                    m.fake.watched_bytes()
                );
            }
        }

        // ── issue #476: a tape's `requires` is checked before its slices are read ──

        /// ADR-0012 2026-10-06 item 15: every reader checks File 0's
        /// `layout_version`, `magic` and `requires` and refuses a tape needing
        /// a feature it does not know. `restore raw` and `catalog rebuild`
        /// always did; restore of slices read File 0 for identity only, so a
        /// tape fenced with `requires = ["parts"]` (#409) would have restored
        /// one part of a divided version as the whole of it. The refusal rides
        /// the contact's one File 0 read, so these tests also pin that nothing
        /// but that read touched the tape.
        mod requires_fence {
            use super::multi::{multi, noise, volume_of, Multi, DEVICE};
            use super::*;
            use crate::tape::fake::Op;
            use crate::volume::restore_record::rows;

            /// How a 1.6.0 File 0 states what it requires (ADR-0012 item 111):
            /// one feature, which this tapectl knows.
            const STATED: &str = "\nrequires = [\"short-seal\"]\n";
            const UNKNOWN: &str = "\nrequires = [\"unknown-feature\"]\n";

            /// [`crate::volume::format::rewrite_file0`], by the name this
            /// module's tests use.
            fn rewritten(file0: &[u8], from: &str, to: &str) -> Vec<u8> {
                crate::volume::format::rewrite_file0(file0, from, to)
            }

            /// [`rewritten`], on the File 0 of the tape in the drive.
            fn rewrite(m: &Multi, from: &str, to: &str) {
                let old = m.fake.state().files[0].clone();
                m.fake.state().files[0] = rewritten(&old, from, to);
            }

            fn one() -> Multi {
                multi(
                    "RQ-1",
                    "rq-unit",
                    &[("a.txt", b"alpha".to_vec()), ("b.bin", noise(100_000, 3))],
                    "64k",
                    true,
                )
            }

            fn three() -> Multi {
                let a = [("a.bin", noise(90_000, 1)), ("a.txt", b"alpha".to_vec())];
                let b = [("b.bin", noise(120_000, 2))];
                let c = [("c.txt", b"charlie".to_vec())];
                volume_of(
                    "RQ-3",
                    &[("ua", &a, "64k"), ("ub", &b, "64k"), ("uc", &c, "64k")],
                    true,
                )
            }

            fn volume(m: &Multi, root: &Path) -> Result<UnitsReport> {
                let reqs: Vec<UnitRequest> = m
                    .units
                    .iter()
                    .map(|u| UnitRequest {
                        unit: u.name.clone(),
                        version: None,
                        dest_dir: root.join(&u.name).to_string_lossy().into_owned(),
                    })
                    .collect();
                restore_units(
                    &m.conn,
                    &m.paths,
                    &Config::default(),
                    "RQ-3",
                    &reqs,
                    &RestoreOptions::default(),
                    false,
                    DEVICE,
                    4096,
                    false,
                )
            }

            /// File 0 was read once and nothing else was: no slice, no
            /// second look at File 0.
            fn assert_only_file0_was_read(ops: &[Op]) {
                assert_eq!(
                    ops.iter().filter(|o| **o == Op::ReadHead(0)).count(),
                    1,
                    "File 0, once: {ops:?}"
                );
                assert!(
                    !ops.iter().any(|o| matches!(o, Op::Read(_))),
                    "a slice was read: {ops:?}"
                );
                assert!(
                    !ops.iter().any(|o| matches!(o, Op::Space(_))),
                    "the head moved toward a slice: {ops:?}"
                );
            }

            /// The refusal is the one `restore raw` and `catalog rebuild` give.
            fn assert_unknown_feature_refusal(err: &str) {
                assert!(err.contains("\"unknown-feature\""), "{err}");
                assert!(err.contains("newer tapectl"), "{err}");
                assert!(err.contains("RESTORE.sh"), "{err}");
                assert!(err.contains("Nothing has been read past it"), "{err}");
                let raw = crate::volume::format::check_id_thunk_readable(
                    &String::from_utf8(
                        tape_labelled("RQ-1").files[0]
                            .iter()
                            .copied()
                            .filter(|b| *b != 0)
                            .collect(),
                    )
                    .unwrap()
                    .replacen(STATED, UNKNOWN, 1),
                )
                .unwrap_err()
                .to_string();
                assert_eq!(err, raw, "the same text raw and rebuild give");
            }

            /// THE acceptance test for `restore unit` and `restore file`
            /// (#476): a tape whose File 0 requires a feature this tapectl
            /// does not know is refused by name at the contact, whether the
            /// restore would stream or spool, before a scratch directory is
            /// made, a key loaded or a slice read. The contact and the
            /// `restores` row record the refusal, as they do a wrong tape.
            #[test]
            fn restore_unit_and_file_refuse_a_tape_requiring_an_unknown_feature() {
                for spool in [false, true] {
                    for as_file in [false, true] {
                        let m = one();
                        rewrite(&m, STATED, UNKNOWN);
                        let dest = TempDir::new().unwrap();
                        let options = RestoreOptions {
                            spool,
                            ..RestoreOptions::default()
                        };
                        let err = if as_file {
                            m.file("RQ-1", "rq-unit", "a.txt", dest.path(), &options)
                                .unwrap_err()
                        } else {
                            m.unit("RQ-1", "rq-unit", dest.path(), &options)
                                .unwrap_err()
                        }
                        .to_string();
                        let what = format!("spool={spool} file={as_file}");
                        assert_unknown_feature_refusal(&err);
                        assert_only_file0_was_read(&m.fake.ops());
                        assert!(
                            !dest.path().join(SCRATCH_NAME).exists(),
                            "{what}: no scratch directory"
                        );
                        assert_eq!(
                            fs::read_dir(dest.path()).unwrap().count(),
                            0,
                            "{what}: nothing was made in the destination"
                        );
                        let all = rows(&m.conn).unwrap();
                        assert_eq!(all.len(), 1, "{what}: {all:?}");
                        assert_eq!(all[0].outcome, "failed", "{what}");
                        assert!(
                            all[0]
                                .error
                                .as_deref()
                                .is_some_and(|e| e.contains("\"unknown-feature\"")),
                            "{what}: {all:?}"
                        );
                        assert_eq!(all[0].slices_read, Some(0), "{what}");
                        let (_, outcome) = only_contact(&m.conn);
                        assert_eq!(outcome.as_deref(), Some("failed"), "{what}");
                    }
                }
            }

            /// `restore volume`: one corroboration refuses the whole set, so
            /// every unit's row is `failed` with the reason and no unit's
            /// destination is made, on one contact with one File 0 read.
            #[test]
            fn restore_volume_refuses_a_tape_requiring_an_unknown_feature() {
                let m = three();
                rewrite(&m, STATED, UNKNOWN);
                let root = TempDir::new().unwrap();
                let err = volume(&m, root.path()).unwrap_err().to_string();
                assert_unknown_feature_refusal(&err);
                assert_only_file0_was_read(&m.fake.ops());
                let all = rows(&m.conn).unwrap();
                assert_eq!(all.len(), 3, "{all:?}");
                for r in &all {
                    assert_eq!(r.outcome, "failed", "{r:?}");
                    assert!(
                        r.error
                            .as_deref()
                            .is_some_and(|e| e.contains("\"unknown-feature\"")),
                        "{r:?}"
                    );
                }
                for u in &m.units {
                    assert!(!root.path().join(&u.name).exists(), "{} was made", u.name);
                }
                let (operation, outcome) = only_contact(&m.conn);
                assert_eq!(operation, "restore volume");
                assert_eq!(outcome.as_deref(), Some("failed"));
            }

            /// A `requires` that is not a list of strings cannot be shown to
            /// be readable (`check_id_thunk_readable`): refused on both paths
            /// with the parse error, never restored as if it required nothing.
            #[test]
            fn a_requires_that_is_not_a_list_is_refused() {
                let m = one();
                rewrite(&m, STATED, "\nrequires = \"parts\"\n");
                let dest = TempDir::new().unwrap();
                let err = m
                    .unit("RQ-1", "rq-unit", dest.path(), &RestoreOptions::default())
                    .unwrap_err()
                    .to_string();
                assert!(err.contains("TOML parse failed"), "{err}");
                assert!(err.contains("requires"), "{err}");
                assert_only_file0_was_read(&m.fake.ops());
                assert!(!dest.path().join(SCRATCH_NAME).exists());

                let m = three();
                rewrite(&m, STATED, "\nrequires = \"parts\"\n");
                let root = TempDir::new().unwrap();
                let err = volume(&m, root.path()).unwrap_err().to_string();
                assert!(err.contains("TOML parse failed"), "{err}");
                assert_only_file0_was_read(&m.fake.ops());
                assert!(rows(&m.conn).unwrap().iter().all(|r| r.outcome == "failed"));
            }

            /// Another layout or magic is the same refusal (the rest of
            /// `FormatClaims::refusal`, which the restore path now reaches).
            #[test]
            fn another_layout_is_refused_on_restore() {
                let m = one();
                rewrite(&m, "\nlayout_version = 2\n", "\nlayout_version = 3\n");
                let dest = TempDir::new().unwrap();
                let err = m
                    .unit("RQ-1", "rq-unit", dest.path(), &RestoreOptions::default())
                    .unwrap_err()
                    .to_string();
                assert!(err.contains("layout_version = 3"), "{err}");
                assert_only_file0_was_read(&m.fake.ops());
            }

            /// The positive controls: a File 0 that states 1.6.0's
            /// `requires = ["short-seal"]`, states `requires = []` (every tape
            /// from 1.1.0 to 1.5.0), states none (every tape before it) or
            /// carries a key nobody requires restores exactly as before — on
            /// the unit path and the volume path — and the slices ARE read,
            /// so the refusals above are the fence and not a tape that
            /// cannot be restored at all.
            #[test]
            fn a_tape_that_requires_nothing_restores_as_before() {
                for (what, to) in [
                    ("states requires = [\"short-seal\"]", STATED),
                    (
                        "states requires = [], as 1.1.0 to 1.5.0",
                        "\nrequires = []\n",
                    ),
                    ("states no requires, as before 1.1.0", "\n"),
                    (
                        "carries a key nobody requires",
                        "\nrequires = [\"short-seal\"]\nfuture_key = \"x\"\n",
                    ),
                ] {
                    let m = one();
                    rewrite(&m, STATED, to);
                    let dest = TempDir::new().unwrap();
                    m.unit("RQ-1", "rq-unit", dest.path(), &RestoreOptions::default())
                        .unwrap_or_else(|e| panic!("{what}: {e}"));
                    m.assert_restored(dest.path());
                    assert!(
                        m.fake.ops().iter().any(|o| matches!(o, Op::Read(_))),
                        "{what}: the slices were read"
                    );
                    assert_eq!(rows(&m.conn).unwrap()[0].outcome, "ok", "{what}");

                    let m = three();
                    rewrite(&m, STATED, to);
                    let root = TempDir::new().unwrap();
                    let report = volume(&m, root.path()).unwrap_or_else(|e| panic!("{what}: {e}"));
                    assert_eq!(report.failed(), 0, "{what}: {report:?}");
                    assert!(
                        m.fake.ops().iter().any(|o| matches!(o, Op::Read(_))),
                        "{what}: the slices were read"
                    );
                }
            }

            /// A File 0 that cannot be read at all is an absence, and proceeds
            /// as it did: the fence turns away a tape that SAYS it needs
            /// something, not one that said nothing. (A blank File 0 reads as
            /// empty, so the restore goes on to fail later, on its own terms
            /// — never on `requires`.)
            #[test]
            fn an_unreadable_file0_is_not_a_new_refusal() {
                let m = one();
                m.fake.state().files.truncate(0);
                let dest = TempDir::new().unwrap();
                let err = m
                    .unit("RQ-1", "rq-unit", dest.path(), &RestoreOptions::default())
                    .unwrap_err()
                    .to_string();
                assert!(!err.contains("requires"), "{err}");
                assert!(!err.contains("newer tapectl"), "{err}");
            }

            // ── the tape that is loaded, named by no label (#435) ──

            /// `restore unit` with no `--from`: File 0 is read ONCE (by
            /// `identify`) and the contact check is served from those bytes,
            /// so the fence adds no read; it refuses ahead of the scratch
            /// directory, the key and every slice, on the one contact.
            #[test]
            fn restore_with_no_from_refuses_a_tape_requiring_an_unknown_feature() {
                let conn = crate::db::open_memory().unwrap();
                let volume_id = seed_loaded(&conn, "RL-FENCE", "rl-unit");
                let mut tape = tape_labelled("RL-FENCE");
                tape.files[0] = rewritten(&tape.files[0], STATED, UNKNOWN);
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

                assert!(err.contains("\"unknown-feature\""), "{err}");
                assert!(err.contains("newer tapectl"), "{err}");
                assert_only_file0_was_read(&fake.ops());
                assert!(!dest.path().join(SCRATCH_NAME).exists());
                let contacts = contact_rows(&conn);
                assert_eq!(contacts.len(), 1, "{contacts:?}");
                assert_eq!(contacts[0].1, Some(volume_id));
                assert_eq!(contacts[0].2.as_deref(), Some("failed"));
                assert!(
                    contacts[0]
                        .3
                        .as_deref()
                        .is_some_and(|d| d.contains("unknown-feature")),
                    "{contacts:?}"
                );
            }

            /// `restore volume` with no label: the same, for the set.
            #[test]
            fn restore_volume_with_no_label_refuses_a_tape_requiring_an_unknown_feature() {
                let conn = crate::db::open_memory().unwrap();
                seed_loaded(&conn, "RL-FENCE3", "rl-unit");
                let mut tape = tape_labelled("RL-FENCE3");
                tape.files[0] = rewritten(&tape.files[0], STATED, UNKNOWN);
                let dest = TempDir::new().unwrap();
                let paths = TapectlPaths::new(dest.path().to_path_buf());
                let to = dest.path().join("out");
                let (fake, mut store) = on_fake_tape(&tape);

                let err = restore_units_from_loaded_store(
                    &conn,
                    &paths,
                    &Config::default(),
                    &[],
                    &to.to_string_lossy(),
                    &RestoreOptions::default(),
                    false,
                    &mut store,
                    site(Operation::RestoreVolume),
                )
                .unwrap_err()
                .to_string();

                assert!(err.contains("\"unknown-feature\""), "{err}");
                assert_only_file0_was_read(&fake.ops());
                assert!(!to.join("rl-unit").exists());
                let (operation, outcome) = only_contact(&conn);
                assert_eq!(operation, "restore volume");
                assert_eq!(outcome.as_deref(), Some("failed"));
                let restores = rows(&conn).unwrap();
                assert_eq!(restores.len(), 1, "{restores:?}");
                assert_eq!(restores[0].outcome, "failed");
            }
        }
    }

    /// #420: restore through tapectl from a tape that ended early
    /// (`volume-format-v2.md` §4.2 R3-R5), on the shared fixture: `alpha`
    /// whole, `bravo` cut with its first slice on the tape, `charlie` cut.
    mod short_ending {
        use super::*;
        use crate::volume::restore::short_tape::{self, ShortTape, LABEL};

        fn files_under(root: &Path) -> Vec<(String, Vec<u8>)> {
            let mut out: Vec<(String, Vec<u8>)> = fs::read_dir(root)
                .unwrap()
                .flatten()
                .filter(|e| e.path().is_file())
                .map(|e| {
                    (
                        e.file_name().to_string_lossy().into_owned(),
                        fs::read(e.path()).unwrap(),
                    )
                })
                .collect();
            out.sort();
            out
        }

        fn expected(t: &ShortTape, unit: &str) -> Vec<(String, Vec<u8>)> {
            let mut files = t.files.iter().find(|(u, _)| u == unit).unwrap().1.clone();
            files.sort();
            files
        }

        /// A second volume in service holding `unit`'s version 1 whole: the
        /// place a refusal sends the operator.
        fn whole_elsewhere(conn: &Connection, unit: &str, label: &str) {
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                      capacity_bytes, status, sealed_at)
                 VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed', datetime('now'))",
                params![label],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 SELECT ss.id, s.id, (SELECT id FROM volumes WHERE label = ?2), 'completed'
                   FROM stage_sets ss JOIN snapshots s ON s.id = ss.snapshot_id
                   JOIN units u ON u.id = s.unit_id WHERE u.name = ?1",
                params![unit, label],
            )
            .unwrap();
        }

        /// A whole unit restores from a short tape as from any other, every
        /// file back as it was.
        #[test]
        fn a_whole_unit_restores_from_a_short_tape() {
            let mut t = short_tape::short_tape();
            let dest = TempDir::new().unwrap();
            let dest_str = dest.path().to_string_lossy().to_string();
            let report = restore_unit_from_store(
                &t.conn,
                &t.paths,
                &Config::default(),
                "alpha",
                LABEL,
                1,
                RestoreTarget::Unit {
                    dest_dir: &dest_str,
                },
                &RestoreOptions::default(),
                &mut t.store,
                site(Operation::RestoreUnit),
            )
            .expect("a whole unit restores");
            assert_eq!(report.slices, 2);
            assert_eq!(files_under(dest.path()), expected(&t, "alpha"));
        }

        /// R4: a cut unit is refused by name before any slice is read, says
        /// where the tape ended and where its version is whole — from the
        /// catalog: a volume in service with a completed write of the same
        /// version (`coverage::in_service_copy_of_version_excluding`), or,
        /// with none, that it is still to be written.
        #[test]
        fn a_cut_unit_is_refused_by_name_and_points_at_the_volume_that_holds_it_whole() {
            let mut t = short_tape::short_tape();
            let k = t.ended_after;
            let last = t.positions_of("bravo")[1];
            let err = select_write_positions(&t.conn, "bravo", LABEL, None)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(&format!(
                    "unit \"bravo\" version 1 is not on volume \"{LABEL}\" whole"
                )),
                "{err}"
            );
            assert!(err.contains(&format!("after tape file {k}")), "{err}");
            assert!(err.contains(&format!("run to tape file {last}")), "{err}");
            assert!(
                err.contains("no volume this catalog knows holds it whole yet"),
                "{err}"
            );
            assert!(err.contains("Nothing was read from tape"), "{err}");
            // --version names it too, and charlie (nothing of it on the
            // tape) is refused the same way.
            let err = select_write_positions(&t.conn, "bravo", LABEL, Some(1))
                .unwrap_err()
                .to_string();
            assert!(err.contains("is not on volume"), "{err}");
            let err = select_write_positions(&t.conn, "charlie", LABEL, None)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("unit \"charlie\" version 1 is not on volume"),
                "{err}"
            );

            whole_elsewhere(&t.conn, "bravo", "WHOLE02");
            let err = select_write_positions(&t.conn, "bravo", LABEL, None)
                .unwrap_err()
                .to_string();
            assert!(err.contains("volume \"WHOLE02\" holds it whole"), "{err}");
            assert!(
                err.contains("tapectl restore unit --unit bravo --from WHOLE02 --version 1"),
                "{err}"
            );

            // Through the store seam: refused, and nothing restored.
            let dest = TempDir::new().unwrap();
            let dest_str = dest.path().to_string_lossy().to_string();
            let err = restore_unit_from_store(
                &t.conn,
                &t.paths,
                &Config::default(),
                "bravo",
                LABEL,
                1,
                RestoreTarget::Unit {
                    dest_dir: &dest_str,
                },
                &RestoreOptions::default(),
                &mut t.store,
                site(Operation::RestoreUnit),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("is not on volume"), "{err}");
            assert!(files_under(dest.path()).is_empty());

            // The positive control: the version restores from where it IS
            // whole once the catalog knows nothing cut of it here.
            assert!(select_write_positions(&t.conn, "alpha", LABEL, None).is_ok());
        }

        /// `restore volume` with no unit named restores every unit the tape
        /// holds whole and names the cut ones; a tape that ended inside its
        /// first unit holds none whole and says so.
        #[test]
        fn restore_volume_restores_the_whole_units_and_names_the_cut_ones() {
            let mut t = short_tape::short_tape();
            assert_eq!(units_on_volume(&t.conn, LABEL).unwrap(), vec!["alpha"]);
            let cut = cut_units_on_volume(&t.conn, LABEL).unwrap();
            assert_eq!(
                cut.iter()
                    .map(|c| (c.unit_name.as_str(), c.version, c.whole_on.clone()))
                    .collect::<Vec<_>>(),
                vec![("bravo", 1, None), ("charlie", 1, None)]
            );
            let lines = crate::cli::restore::cut_units_lines(&cut, LABEL).join("\n");
            assert!(
                lines.contains("not on this tape whole: \"bravo\" v1"),
                "{lines}"
            );
            assert!(lines.contains("2 unit(s) left out"), "{lines}");

            let dest = TempDir::new().unwrap();
            let report = restore_units_from_loaded_store(
                &t.conn,
                &t.paths,
                &Config::default(),
                &[],
                &dest.path().to_string_lossy(),
                &RestoreOptions::default(),
                false,
                &mut t.store,
                site(Operation::RestoreVolume),
            )
            .unwrap();
            assert_eq!(report.failed(), 0);
            assert_eq!(
                report
                    .units
                    .iter()
                    .map(|u| u.unit_name.as_str())
                    .collect::<Vec<_>>(),
                vec!["alpha"]
            );
            assert_eq!(
                files_under(&dest.path().join("alpha")),
                expected(&t, "alpha")
            );
            assert!(!dest.path().join("bravo").exists());

            // Every unit cut: the volume holds none whole, and says which.
            t.conn
                .execute("UPDATE writes SET status = 'cut'", [])
                .unwrap();
            let err = units_on_volume(&t.conn, LABEL).unwrap_err().to_string();
            assert!(err.contains("holds no unit whole"), "{err}");
            assert!(err.contains("\"alpha\" v1"), "{err}");
        }

        /// The repair source (#480, §4.2 R5): a cut write's slices are never
        /// one, even its slice at `ended_after` that is on the tape and hashes
        /// right; and a completed write's slice recorded past its own tape's
        /// `ended_after` (a catalog `audit` names) is no source either.
        #[test]
        fn a_cut_writes_slices_are_never_a_repair_source() {
            let t = short_tape::short_tape();
            let k = t.ended_after;
            let sha_at = |position: u32| -> String {
                t.conn
                    .query_row(
                        "SELECT sl.sha256_encrypted FROM write_positions wp
                           JOIN stage_slices sl ON sl.id = wp.stage_slice_id
                          WHERE CAST(wp.position AS INTEGER) = ?1",
                        params![position],
                        |r| r.get(0),
                    )
                    .unwrap()
            };
            // bravo's first slice is on the tape at K; its write is cut.
            assert_eq!(t.positions_of("bravo")[0], k);
            let bravo_first = sha_at(k);
            assert!(
                crate::volume::slice_holders::holders_of(&t.conn, &bravo_first)
                    .unwrap()
                    .is_empty()
            );
            // alpha's slices are a source on this tape (the positive control).
            let alpha_first = sha_at(t.positions_of("alpha")[0]);
            let holders = crate::volume::slice_holders::holders_of(&t.conn, &alpha_first).unwrap();
            assert_eq!(holders.len(), 1);
            assert_eq!(holders[0].label, LABEL);

            // A completed row whose position the catalog records past its
            // volume's end: not a source.
            t.conn
                .execute(
                    "UPDATE volumes SET ended_after = ?1 WHERE label = ?2",
                    params![t.positions_of("alpha")[0] - 1, LABEL],
                )
                .unwrap();
            assert!(
                crate::volume::slice_holders::holders_of(&t.conn, &alpha_first)
                    .unwrap()
                    .is_empty()
            );
        }
    }
}

// Declared last: the source-scan tests split this file at its first `#[cfg(test)]
// mod`, and the production code must all lie before it.
#[cfg(test)]
mod heir_tests;
#[cfg(test)]
pub(crate) mod repair_tests;
