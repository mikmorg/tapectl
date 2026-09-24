use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use tracing::{info, warn};

use crate::config::{Config, TapectlPaths};
use crate::crypto::keys;
use crate::dar;
use crate::dar::restore::DarReport;
use crate::db::queries;
use crate::error::{Result, TapectlError};
use crate::store::{Store, TapeStore};
use crate::tape::contact::{self, ContactSite, Medium, Operation};
use crate::tape::mam_journal::MamReads;
use crate::util::{HashingWriter, TruncatingWriter};
use crate::volume::restore_record::{self, RestoreRecord};

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
    /// One entry: the unit is extracted into `extract_dir` (a temp
    /// directory the caller owns) and `file_path` is placed into
    /// `dest_dir`, the directory the operator named.
    File {
        file_path: &'a str,
        dest_dir: &'a str,
        extract_dir: &'a str,
    },
}

impl<'a> RestoreTarget<'a> {
    /// Where dar extracts to.
    fn extract_dir(&self) -> &'a str {
        match *self {
            RestoreTarget::Unit { dest_dir } => dest_dir,
            RestoreTarget::File { extract_dir, .. } => extract_dir,
        }
    }

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
/// The directory deliberately lives *under the destination* rather than in
/// `std::env::temp_dir()`: dar extracts from it into the destination, and a
/// slice set can be hundreds of gigabytes, so a system temp dir on a small
/// tmpfs (or a different filesystem) is the wrong home for it. That is why
/// this is a hand-written guard and not `tempfile::tempdir()` — `restore_file`
/// uses `tempfile` correctly for a *different* directory, one it wants placed
/// by the system.
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
// 10 args reflects the CLI's flat shape (unit/volume/dest/device/block_size/
// version/dry_run alongside conn/paths/config); interim allow. This used to carry a
// comment blaming the count on "the store read seam in #71 (epic #20)" —
// wrong: #71 was closed and scoped only to the write-side execute/confirm
// seam. The read seam migrated here directly (issue #85): per-slice tape
// access below now goes through `Store::read_file` via `restore_one_slice`,
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
) -> Result<RestoreReport> {
    restore_through_drive(
        conn,
        paths,
        config,
        unit_name,
        volume_label,
        RestoreTarget::Unit { dest_dir },
        device,
        block_size,
        version,
        dry_run,
    )
}

/// [`restore_unit`] and [`restore_file`]'s one path to the drive: resolve
/// the version, take the two MAM reads, open the store, and hand off to the
/// store seam with the [`RestoreTarget`] that says which of the two this is.
#[allow(clippy::too_many_arguments)]
fn restore_through_drive(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    unit_name: &str,
    volume_label: &str,
    target: RestoreTarget<'_>,
    device: &str,
    block_size: usize,
    version: Option<i64>,
    dry_run: bool,
) -> Result<RestoreReport> {
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

    if dry_run {
        return Ok(RestoreReport {
            unit_name: unit_name.to_string(),
            volume_label: volume_label.to_string(),
            version: selection.version,
            slices: selection.positions.len(),
            destination: target.destination().to_string(),
            dry_run: true,
            success: true,
        });
    }

    // Issue #166: refuse before the store is opened if this drive cannot
    // read the loaded medium. Proceeds silently with no configured backend
    // or nothing detected — the DR machine with keys and no `backend add`
    // yet (ADR-0005), same leniency as the MAM read just below.
    //
    // Both MAM reads are held and journalled when the contact opens (issue
    // #297).
    let reads = MamReads::new(conn, Operation::RestoreUnit);
    reads.check_read_contact(config, device)?;

    // Before `TapeStore::open_read`: reading the MAM opens the device
    // read-only and drops the fd, and the st driver refuses a second
    // concurrent open. LENIENT — no configured backend yields `None`, an
    // absence, which is the DR machine with keys and no `backend add`.
    let observed = crate::volume::binding::loaded_medium(config, device, &reads);
    // Open the store read-only, positioned at BOT.
    let mut store = TapeStore::open_read(device, block_size)?;
    restore_unit_from_store(
        conn,
        paths,
        config,
        unit_name,
        volume_label,
        selection.version,
        target,
        &mut store,
        ContactSite::new(
            config,
            Operation::RestoreUnit,
            device,
            Medium::from_read(observed.as_ref().map(|(b, m)| (*b, m))),
        )
        .with_mam_reads(&reads),
    )
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
    let started_at = restore_record::now_sqlite();
    let guard = site.open(conn, None);
    let contact_id = guard.id();
    let r = crate::volume::raw::restore_raw(store, dest, expect_label);
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
    crate::volume::write::health_after_read_contact(conn, &site, None, contact_id);
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn restore_unit_from_store(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    unit_name: &str,
    volume_label: &str,
    version: i64,
    target: RestoreTarget<'_>,
    store: &mut dyn Store,
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
    let guard = site.open(conn, volume_id);
    let contact_id = guard.id();
    let mut trace = RestoreTrace::default();
    let r = guard.finish_result(restore_unit_contacted(
        conn,
        paths,
        config,
        unit_name,
        volume_label,
        version,
        target,
        store,
        site.medium_serial(),
        &mut trace,
    ));
    // The restore's own record: what came back, where to, how it ended,
    // and dar's report verbatim. Best-effort, like the contact row — the
    // result `r` is decided and this cannot change it.
    let (outcome, error) = match &r {
        Ok(_) => (contact::OUTCOME_OK, None),
        Err(e) => (contact::OUTCOME_FAILED, Some(e.to_string())),
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
            contact_id,
            volume_id,
            volume_label: Some(volume_label),
            unit_id,
            unit_name: Some(unit_name),
            version: Some(version),
            kind: target.kind(),
            file_path: target.file_path(),
            destination: target.destination(),
            started_at: &started_at,
            outcome,
            error: error.as_deref(),
            slices_read: Some(trace.slices_read),
            bytes_restored: Some(trace.bytes_decrypted),
            files_restored,
            dar: trace.dar.as_ref(),
            dar_version: trace.dar_version.as_deref(),
        },
    );
    // ONE post-command health reading for this contact (issue #320), on
    // every outcome, naming the volume the contact names. This seam is the
    // only place `restore unit` — and `restore file`, which reaches the
    // drive through it — takes one, so a contact cannot get two.
    crate::volume::write::health_after_read_contact(conn, &site, volume_id, contact_id);
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
    store: &mut dyn Store,
    medium_serial: Option<&str>,
    trace: &mut RestoreTrace,
) -> Result<RestoreReport> {
    let dest_dir = target.extract_dir();
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;
    let tenant = queries::get_tenant_by_id(conn, unit.tenant_id)?
        .ok_or_else(|| TapectlError::Other("tenant not found".into()))?;
    // ONE version's slices (issue #315). The version is resolved by the
    // caller — `restore_unit` before it opens the drive — and passed here
    // concretely, so the version counted and the version read are the same.
    let positions = select_write_positions(conn, unit_name, volume_label, Some(version))?.positions;

    // Corroborate at contact (ADR-0012, issue #193), before a scratch
    // directory is made, before a key is loaded and before a single slice is
    // read. Restoring from the wrong tape used to surface as a per-slice
    // sha256 failure with no word about why.
    let volume_id: i64 = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            rusqlite::params![volume_label],
            |r| r.get(0),
        )
        .map_err(|_| TapectlError::VolumeNotFound(volume_label.to_string()))?;
    let medium = crate::volume::binding::MediumFacts::new(
        medium_serial.map(str::to_string),
        crate::volume::binding::read_file0_facts(store),
    );
    crate::volume::binding::corroborate_volume(conn, volume_id, volume_label, &medium)?;

    // Scratch dir for decrypted slices. The guard removes it on EVERY path
    // out of this function, not just the happy one — see `RestoreScratch`.
    let restore_tmp = Path::new(dest_dir).join(".tapectl-restore-tmp");
    fs::create_dir_all(&restore_tmp)?;
    let _scratch = RestoreScratch(restore_tmp.clone());

    // Load all secret keys for trial-decryption (tenant + operator)
    let mut identities = keys::load_all_identities(&paths.keys_dir, &tenant.name)?;
    if !tenant.is_operator {
        if let Some(operator) = queries::get_operator_tenant(conn)? {
            identities.extend(keys::load_all_identities(&paths.keys_dir, &operator.name)?);
        }
    }
    if identities.is_empty() {
        return Err(TapectlError::Encryption(format!(
            "no secret keys found for tenant \"{}\"",
            tenant.name,
        )));
    }

    let mut dar_slices: Vec<PathBuf> = Vec::new();

    for (i, wp) in positions.iter().enumerate() {
        let position: u32 = wp.position.parse().unwrap_or(0);

        info!(
            slice = i + 1,
            total = positions.len(),
            tape_pos = position,
            "reading slice from tape"
        );

        // dar expects: basename.N.dar
        let slice_path = restore_tmp.join(format!("restore.{}.dar", wp.slice_number));
        let ciphertext_tmp_path =
            restore_tmp.join(format!("restore.{}.dar.age.tmp", wp.slice_number));

        let plain_size = restore_one_slice(
            store,
            position,
            wp,
            &identities,
            &ciphertext_tmp_path,
            &slice_path,
        )?;
        dar_slices.push(slice_path);
        trace.slices_read += 1;
        trace.bytes_decrypted += plain_size as i64;

        info!(
            slice = i + 1,
            // Binary (issue #204): a decrypted slice is a measured data
            // size, so this stays 1024-based -- only the field name was
            // wrong, not the division.
            mib = plain_size / (1024 * 1024),
            "decrypted slice"
        );
    }

    // Run dar extract. Its report is kept whenever it ran, on both verdicts
    // (issue #306); the version is read only once dar has actually run, so
    // a restore that never reached dar records no version either.
    let archive_base = restore_tmp.join("restore");
    info!("extracting dar archive to {dest_dir}");
    let (report, verdict) =
        dar::restore::extract_reported(&config.dar.binary, &archive_base, Path::new(dest_dir));
    if report.is_some() {
        trace.dar_version = dar::version::check(&config.dar.binary)
            .ok()
            .map(|v| v.full_string);
    }
    trace.dar = report;
    verdict?;

    // No explicit cleanup here on purpose: `_scratch` removes the whole
    // directory on the way out. The hand-rolled version this replaces walked
    // `dar_slices`, then swept the directory for hash files, then removed the
    // directory — three steps that only ran if every `?` above succeeded.
    drop(dar_slices);

    // `restore file`: place the one requested entry, inside the recorded
    // span (see `RestoreTarget`).
    if let RestoreTarget::File {
        file_path,
        dest_dir: file_dest,
        ..
    } = target
    {
        place_one_entry(Path::new(dest_dir), file_path, Path::new(file_dest))?;
        trace.placed = true;
    }

    info!(unit = unit_name, volume = volume_label, "restore complete");

    Ok(RestoreReport {
        unit_name: unit_name.to_string(),
        volume_label: volume_label.to_string(),
        version,
        slices: positions.len(),
        destination: target.destination().to_string(),
        dry_run: false,
        success: true,
    })
}

/// Restore one slice: read it off `store` at `position`, verify its on-tape
/// (true, unpadded) bytes hash to `wp.sha256_encrypted`, decrypt with
/// whichever of `identities` matches, and stream the plaintext to
/// `output_path`, verifying it hashes to `wp.sha256_plain`. Returns the
/// plaintext byte count.
///
/// On any error, both `ciphertext_tmp_path` and a partial `output_path` are
/// best-effort removed rather than left behind — mirrors
/// `encrypt_file_streaming`'s cleanup-on-error convention (`src/staging/
/// mod.rs`). The ciphertext temp file is disposable either way, success or
/// failure, since nothing downstream ever needs it again once this call
/// returns.
fn restore_one_slice(
    store: &mut dyn Store,
    position: u32,
    wp: &WritePositionInfo,
    identities: &[age::x25519::Identity],
    ciphertext_tmp_path: &Path,
    output_path: &Path,
) -> Result<u64> {
    let result = restore_one_slice_inner(
        store,
        position,
        wp,
        identities,
        ciphertext_tmp_path,
        output_path,
    );
    let _ = fs::remove_file(ciphertext_tmp_path);
    if result.is_err() {
        let _ = fs::remove_file(output_path);
    }
    result
}

/// Two passes, one intermediate ciphertext temp file:
///
/// **Pass 1** streams the slice off `store` (`Store::read_file` is
/// push-based — it drives its own read loop and pushes bytes into a
/// `sink: &mut dyn Write`), through a [`TruncatingWriter`] that trims the
/// trailing block padding to `wp.encrypted_bytes` (the DB-recorded true
/// length — `restore_unit` is the DB-catalog restore path and never
/// consults the on-tape front index) as the bytes arrive, wrapping a
/// [`HashingWriter`] so the ciphertext hash is known the moment the pass
/// finishes, with zero extra buffering. That hash is checked against
/// `wp.sha256_encrypted` — the same integrity check the old whole-buffer
/// code ran, just computed incrementally instead of over a fully materialized
/// `Vec`.
///
/// **Pass 2** only runs once pass 1's hash has been verified. It reopens the
/// now-trusted ciphertext temp file, decrypts it, and streams the plaintext
/// straight to `output_path` through a [`HashingWriter`], checking the
/// result against `wp.sha256_plain`.
///
/// Bridging pass 1's push-based source with pass 2's pull-based
/// `age::Decryptor` (`Decryptor::new` needs a `Read`) without a spooled
/// intermediate would mean either buffering the whole ciphertext in RAM
/// again (the bug this fixes) or a reader thread (unwarranted complexity for
/// a restore CLI path) — the two-artifact shape mirrors the write side's own
/// (staged plaintext file -> `encrypt_file_streaming` -> `.age` file ->
/// tape).
///
/// Trial-decryption is ONE `decrypt()` call carrying every identity in
/// `identities` — `age`'s `obtain_payload_key` tries each of them via
/// `find_map` over the header's recipient stanzas internally, before any
/// STREAM body byte is read, so this never needs a per-identity retry loop
/// that would have to re-open an already-consumed reader.
///
/// Peak RAM: pass 1 is bounded by `Store::read_file`'s own block-sized
/// buffer (`block_size`, 512 KiB by default for `TapeStore`); pass 2 is
/// bounded by `RESTORE_STREAM_BUFFER` (128 KiB) plus age's own constant
/// ~64 KiB STREAM chunk buffer. The passes never overlap, so peak RAM for
/// the whole function is `max(block_size, ~192 KiB)` — independent of slice
/// size, where the buffered predecessor was ~2x slice size (issue #85).
fn restore_one_slice_inner(
    store: &mut dyn Store,
    position: u32,
    wp: &WritePositionInfo,
    identities: &[age::x25519::Identity],
    ciphertext_tmp_path: &Path,
    output_path: &Path,
) -> Result<u64> {
    // Pass 1: stream the slice off the store, trimming block padding to the
    // true (DB-recorded) ciphertext length as it arrives, hashing exactly
    // those bytes — never the whole slice in RAM.
    let ct_file = fs::File::create(ciphertext_tmp_path)?;
    let mut bounded = TruncatingWriter::new(HashingWriter::new(ct_file), wp.encrypted_bytes as u64);
    store.read_file(position, &mut bounded)?;
    let hashing_ct = bounded.into_inner();
    let actual_hash = hashing_ct.finalize_hex();
    drop(hashing_ct); // closes ciphertext_tmp_path before pass 2 reopens it

    if actual_hash != wp.sha256_encrypted {
        return Err(TapectlError::Other(format!(
            "slice {} checksum mismatch on tape: expected {}..., got {}...",
            wp.slice_number,
            &wp.sha256_encrypted[..16],
            &actual_hash[..16],
        )));
    }

    // Pass 2: decrypt the now-verified ciphertext, streaming plaintext
    // straight to `output_path`.
    let ct_file = fs::File::open(ciphertext_tmp_path)?;
    let decryptor = age::Decryptor::new(ct_file)
        .map_err(|e| TapectlError::Encryption(format!("decryptor: {e}")))?;
    let mut reader = decryptor
        .decrypt(identities.iter().map(|id| id as &dyn age::Identity))
        .map_err(|e| TapectlError::Encryption(format!("decrypt: {e}")))?;

    let out_file = fs::File::create(output_path)?;
    let mut hashing_out = HashingWriter::new(out_file);
    let plain_size = stream_copy(&mut reader, &mut hashing_out)?;
    let plain_hash = hashing_out.finalize_hex();

    if plain_hash != wp.sha256_plain {
        return Err(TapectlError::Other(format!(
            "slice {} decrypted checksum mismatch",
            wp.slice_number,
        )));
    }

    Ok(plain_size)
}

/// Fixed-size copy buffer for streaming slice decryption (H9 fix, issue
/// #85) — same 128 KiB convention as `staging::encrypt_file_streaming`'s
/// `STREAM_COPY_BUFFER`, `staging::validate`'s `VALIDATE_STREAM_BUFFER`, and
/// `volume::layout_model::hash_file`. Peak RAM for the decrypt pass this
/// feeds is this buffer plus age's own constant ~64 KiB STREAM chunk buffer,
/// never the size of the slice being restored.
const RESTORE_STREAM_BUFFER: usize = 128 * 1024;

/// Copy every byte from `reader` to `writer` through a fixed-size buffer —
/// never allocates more than `RESTORE_STREAM_BUFFER`, regardless of how much
/// data flows through. Returns the total bytes copied. Same shape as
/// `staging::mod`'s private `stream_copy`; kept as an independently-named
/// copy here rather than shared, matching this codebase's existing
/// convention of one streaming-copy helper per site (see
/// `staging::validate`'s `VALIDATE_STREAM_BUFFER` doc comment for the same
/// precedent).
fn stream_copy<R: Read, W: Write>(reader: &mut R, writer: &mut W) -> Result<u64> {
    let mut buf = [0u8; RESTORE_STREAM_BUFFER];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        total += n as u64;
    }
    Ok(total)
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
) -> Result<()> {
    // A full restore into a temp dir, with the one requested entry placed
    // into `dest_dir` inside the same recorded span (`RestoreTarget::File`,
    // issue #306) — through the one drive path `restore unit` uses, so this
    // is one contact and one reading, never two.
    let tmp = tempfile::tempdir().map_err(|e| TapectlError::Other(e.to_string()))?;
    let tmp_path = tmp.path().to_string_lossy().to_string();

    restore_through_drive(
        conn,
        paths,
        config,
        unit_name,
        volume_label,
        RestoreTarget::File {
            file_path,
            dest_dir,
            extract_dir: &tmp_path,
        },
        device,
        block_size,
        version,
        false,
    )?;
    Ok(())
}

/// Copy the one entry `file_path` out of the extracted unit at `extracted`
/// into `dest_dir` — `restore file`'s placing step.
///
/// `symlink_metadata`, not `exists()`: a unit may legitimately contain a
/// symlink pointing outside itself, and `exists()` follows the link, so a
/// dangling one was reported as "not found in restored unit" when it had in
/// fact been restored correctly by dar.
fn place_one_entry(extracted: &Path, file_path: &str, dest_dir: &Path) -> Result<()> {
    let source_file = extracted.join(file_path);
    let meta = fs::symlink_metadata(&source_file).map_err(|_| {
        TapectlError::Other(format!("file \"{file_path}\" not found in restored unit"))
    })?;

    let dest = dest_dir.join(
        Path::new(file_path)
            .file_name()
            .unwrap_or(std::ffi::OsStr::new(file_path)),
    );
    fs::create_dir_all(dest_dir)?;
    place_restored_entry(&source_file, &meta, &dest)?;

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
}

/// One slice of the selected version, as restore reads it off the volume.
#[derive(Debug, Clone)]
pub struct WritePositionInfo {
    /// The stage set this slice belongs to — one stage set is one snapshot
    /// version's complete dar archive (issue #315).
    pub stage_set_id: i64,
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
    /// The selected stage set's slices, in slice order.
    pub positions: Vec<WritePositionInfo>,
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
pub fn select_write_positions(
    conn: &Connection,
    unit_name: &str,
    volume_label: &str,
    want: Option<i64>,
) -> Result<RestoreSelection> {
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;

    // Every (version, stage set) of this unit with written slices on this
    // volume — newest version first, later stage set first within one.
    let mut stmt = conn.prepare(
        "SELECT DISTINCT s.version, ss.id
         FROM write_positions wp
         JOIN writes w ON w.id = wp.write_id
         JOIN stage_slices sl ON sl.id = wp.stage_slice_id
         JOIN stage_sets ss ON ss.id = sl.stage_set_id
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN volumes v ON v.id = w.volume_id
         WHERE s.unit_id = ?1 AND v.label = ?2 AND w.status = 'completed' AND wp.status = 'written'
         ORDER BY s.version DESC, ss.id DESC",
    )?;
    let candidates = stmt
        .query_map(params![unit.id, volume_label], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    if candidates.is_empty() {
        return Err(TapectlError::Other(format!(
            "no data for unit \"{unit_name}\" on volume \"{volume_label}\""
        )));
    }

    let mut versions_on_volume: Vec<i64> = candidates.iter().map(|(v, _)| *v).collect();
    versions_on_volume.sort_unstable();
    versions_on_volume.dedup();

    // `candidates` is ordered newest-first, so the first match is the pick.
    let picked = match want {
        None => candidates.first(),
        Some(want) => candidates.iter().find(|(v, _)| *v == want),
    };
    let Some(&(version, stage_set_id)) = picked else {
        let listed: Vec<String> = versions_on_volume.iter().map(i64::to_string).collect();
        return Err(TapectlError::Other(format!(
            "unit \"{unit_name}\" has no version {} on volume \"{volume_label}\"; \
             version(s) on it: {} — pass one of those with --version",
            want.unwrap_or_default(),
            listed.join(", "),
        )));
    };

    let positions = get_write_positions(conn, stage_set_id, volume_label)?;
    Ok(RestoreSelection {
        version,
        stage_set_id,
        versions_on_volume,
        positions,
    })
}

/// The written slices of ONE stage set on one volume, in slice order.
fn get_write_positions(
    conn: &Connection,
    stage_set_id: i64,
    volume_label: &str,
) -> Result<Vec<WritePositionInfo>> {
    let mut stmt = conn.prepare(
        "SELECT sl.slice_number, wp.position, sl.sha256_plain, sl.sha256_encrypted, sl.encrypted_bytes,
                sl.stage_set_id
         FROM write_positions wp
         JOIN writes w ON w.id = wp.write_id
         JOIN stage_slices sl ON sl.id = wp.stage_slice_id
         JOIN volumes v ON v.id = w.volume_id
         WHERE sl.stage_set_id = ?1 AND v.label = ?2 AND w.status = 'completed' AND wp.status = 'written'
         ORDER BY sl.slice_number",
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
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(rows)
}

#[cfg(test)]
mod tests {
    //! Tests for the H9 fix (issue #85): `restore_one_slice` must behave
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
    use crate::store::MemStore;
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
    /// never buffers a whole ciphertext; see `restore_one_slice`).
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
            slice_number: 1,
            position: "0".to_string(),
            sha256_plain,
            sha256_encrypted,
            encrypted_bytes,
        };

        (store, wp)
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

    // --- restore_one_slice (the 4 required scenarios) --------------------

    #[test]
    fn round_trip_reproduces_the_exact_original_plaintext() {
        let kp = crate::crypto::keys::generate_keypair();
        let pubkeys = vec![kp.public_key.clone()];
        let identity: age::x25519::Identity = kp.secret_key.parse().unwrap();

        let plaintext = b"restore round-trip content, repeated a bit. ".repeat(50);
        let (mut store, wp) = build_fixture(&plaintext, &pubkeys, 4096);

        let tmp = TempDir::new().unwrap();
        let ct_tmp = tmp.path().join("ct.age.tmp");
        let out = tmp.path().join("out.dar");

        let plain_size = restore_one_slice(&mut store, 0, &wp, &[identity], &ct_tmp, &out).unwrap();

        assert_eq!(plain_size, plaintext.len() as u64);
        let restored = fs::read(&out).unwrap();
        assert_eq!(restored, plaintext);
        assert!(
            !ct_tmp.exists(),
            "ciphertext temp file must be cleaned up after a successful restore"
        );
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
        let ct_tmp = tmp.path().join("ct.age.tmp");
        let out = tmp.path().join("out.dar");

        let err = restore_one_slice(&mut store, 0, &wp, &[identity], &ct_tmp, &out).unwrap_err();
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
        assert!(
            !ct_tmp.exists(),
            "ciphertext temp file must be cleaned up even on failure"
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
        let ct_tmp = tmp.path().join("ct.age.tmp");
        let out = tmp.path().join("out.dar");

        // The right identity is SECOND in the list, proving the single
        // `decrypt()` call tries every identity (age's `obtain_payload_key`
        // does `find_map` over the header internally) rather than only ever
        // succeeding when the match happens to come first.
        let identities = vec![wrong_identity, right_identity];
        let plain_size = restore_one_slice(&mut store, 0, &wp, &identities, &ct_tmp, &out).unwrap();

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
        let ct_tmp = tmp.path().join("ct.age.tmp");
        let out = tmp.path().join("out.dar");

        let plain_size = restore_one_slice(&mut store, 0, &wp, &[identity], &ct_tmp, &out).unwrap();
        assert_eq!(plain_size, plaintext.len() as u64);
        assert_eq!(fs::read(&out).unwrap(), plaintext);
    }

    #[test]
    fn copy_buffer_is_a_small_fixed_constant_independent_of_input_length() {
        // Structural guarantee behind the constant-memory claim, matching
        // `staging::mod`'s `copy_buffer_is_a_small_fixed_constant_
        // independent_of_input_length` — pinning the exact value means any
        // future drift back toward whole-slice buffering is a deliberate,
        // visible edit to this test.
        assert_eq!(RESTORE_STREAM_BUFFER, 128 * 1024);
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
                usable_capacity_factor: 0.95,
                enospc_buffer: "1GiB".to_string(),
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
            assert_eq!(file.matches("restore_through_drive(").count(), 1, "positive control");
            let unit = body("pub fn restore_unit(");
            assert_eq!(unit.matches("restore_through_drive(").count(), 1, "positive control");
            let drive = body("fn restore_through_drive(");
            assert_eq!(drive.matches("restore_unit_from_store(").count(), 1);
            let seam = body("pub(crate) fn restore_unit_from_store(");
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
            assert_eq!(
                seam.matches("health_after_read_contact(").count(),
                1,
                "the seam that opens the contact takes its one reading"
            );
            assert_eq!(
                prod.matches("health_after_read_contact(").count(),
                2,
                "restore.rs takes readings in exactly two seams: restore unit's and raw-volume's"
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
            fn real_fixture(
                conn: &Connection,
                paths: &TapectlPaths,
                label: &str,
                unit: &str,
            ) -> (MemStore, u64) {
                seed(conn, label, unit);
                paths.ensure_dirs().unwrap();
                let kp = keys::generate_and_save(&paths.keys_dir, "t1", "primary").unwrap();

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

            fn only_row(conn: &Connection) -> RestoreRow {
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
                assert_eq!(r.tapectl_version, env!("CARGO_PKG_VERSION"));
            }

            /// `restore file` is ONE row of kind `file` under `restore
            /// unit`'s one contact, naming the directory the operator gave
            /// — never the temp directory the unit was extracted into.
            #[test]
            fn a_restore_file_records_one_file_row_naming_the_operators_destination() {
                let conn = crate::db::open_memory().unwrap();
                let home = TempDir::new().unwrap();
                let paths = TapectlPaths::new(home.path().join(".tapectl"));
                let (mut store, _) = real_fixture(&conn, &paths, "RF-OK", "rf-unit");
                let extract = TempDir::new().unwrap();
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
                        extract_dir: &extract.path().to_string_lossy(),
                    },
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
                let extract = TempDir::new().unwrap();
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
                        extract_dir: &extract.path().to_string_lossy(),
                    },
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
                )
                .unwrap();
                assert!(rows(&conn).unwrap().is_empty());
            }

            fn raw_volume(conn: &Connection, store: &mut MemStore, dest: &Path) -> bool {
                restore_raw_volume(
                    conn,
                    store,
                    dest,
                    None,
                    site(Operation::RestoreRawVolume),
                )
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
                assert_eq!(
                    r.files_restored, None,
                    "not known: the dump never finished"
                );
                assert_eq!(r.bytes_restored, None);
            }
        }
    }
}
