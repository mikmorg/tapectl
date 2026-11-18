//! The source against its snapshot (issue #32/H6, #364): by metadata
//! before anything is staged ([`plan`]), and in full, for a caller with no
//! archiver beside it ([`validate_source`]). `stage create` takes each
//! file's sha256 from the tar stream as it passes (ADR-0012 item 253) and
//! compares it with the baselines [`plan`] gathers.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rusqlite::{params, Connection};
use tracing::info;

use crate::db::files::FileKind;
use crate::error::{Result, TapectlError};
use crate::util::HashingReader;

/// What [`validate_source`] establishes.
#[derive(Debug)]
pub struct SourceValidation {
    /// `(relative_path, sha256_hex)` for every regular file in the snapshot.
    pub checksums: Vec<(String, String)>,
}

/// One regular file of the snapshot, as the source walk found it.
#[derive(Debug, Clone)]
pub(crate) struct PlannedFile {
    rel_path: String,
    expected_size: i64,
    /// `files.sha256` from an earlier stage of this snapshot, if any.
    baseline: Option<String>,
}

/// The regular files [`hash_files`] reads, checked against the snapshot by
/// metadata alone ([`plan`]).
#[derive(Debug)]
pub(crate) struct SourcePlan {
    files: Vec<PlannedFile>,
}

impl SourcePlan {
    /// Each regular file's recorded sha256 (hex), where the version has one:
    /// what a stream hash is compared with (`volume-format-v3.md` §3.2).
    pub(crate) fn baselines(&self) -> HashMap<&str, &str> {
        self.files
            .iter()
            .filter_map(|f| f.baseline.as_deref().map(|b| (f.rel_path.as_str(), b)))
            .collect()
    }

    /// The bytes [`hash_files`] will read.
    pub(crate) fn total_bytes(&self) -> u64 {
        self.files
            .iter()
            .map(|f| f.expected_size.max(0) as u64)
            .sum()
    }
}

/// Check the source against the snapshot's file list by metadata alone
/// (issue #364). This is what refuses before tar starts: a manifest file
/// gone from disk (MISSING), at another size (DIRTY), or excluded since the
/// snapshot (issue #464). Content (BITROT, and a file changing while
/// staging reads it) is checked as the stream passes and after tar.
///
/// A manifest file the patterns exclude since the snapshot is refused
/// (issue #464): the snapshot no longer describes the unit as it is staged,
/// and `snapshot create` is the remedy.
///
/// Also diffs the on-disk file set against the manifest for NEW files (issue
/// #32/H6): a file the catalog has never seen is a warning, not a refusal.
/// tar is given the snapshot's own list (`volume-format-v3.md` §3.1), so a
/// NEW file is not archived until a snapshot records it.
///
/// `global_excludes` is `config.defaults.global_excludes` (issue #49) —
/// passed through to `walk_directory`, so a globally-excluded file appearing
/// after `snapshot create` is not falsely reported NEW.
pub(crate) fn plan(
    conn: &Connection,
    snapshot_id: i64,
    source_path: &str,
    global_excludes: &[String],
) -> Result<SourcePlan> {
    let base = Path::new(source_path);

    // Every non-directory entry of the manifest, with any established sha256
    // baseline and its recorded file_type (issue #33/H7: 'regular' /
    // 'symlink' / 'special'). NEW detection needs every non-directory path,
    // symlink/special included, or a staged symlink would reappear as NEW
    // on every re-stage.
    let mut stmt = conn.prepare(&format!(
        "SELECT p.path, fv.size_bytes, fv.sha256, fv.kind FROM {}
         WHERE fv.snapshot_id = ?1 AND fv.kind <> 0
         ORDER BY fv.path_id",
        crate::db::files::VERSION_FILES
    ))?;
    let all_entries: Vec<(String, i64, Option<String>, FileKind)> = stmt
        .query_map(params![snapshot_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                crate::db::files::sha256_column(row.get(2)?),
                row.get::<_, FileKind>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    // ADR-0012, 2026-10-07 item 24 (#378): the unit's own dotfile is not
    // content. `unit tag`/`unit rename`/a `[policy]` edit rewrite it, and
    // the same Version must still stage, so it is neither size-checked
    // against the snapshot, compared with a sha256 baseline, nor reported
    // NEW, and its absence is not MISSING. tar still archives it, so its
    // hash (from the stream, at the size it has now) is recorded by the first
    // stage of a Version as for any file.
    let is_dotfile = crate::unit::dotfile::is_unit_dotfile;
    let all_entries: Vec<_> = all_entries
        .into_iter()
        .filter(|(p, ..)| !is_dotfile(p))
        .collect();

    // The source as it is now, with the stage's patterns: the NEW check
    // below.
    let (_, _, disk_entries) = super::walk_directory(source_path, global_excludes)?;
    let manifest_paths: HashSet<&str> = all_entries.iter().map(|(p, ..)| p.as_str()).collect();
    let mut new_entries: Vec<_> = disk_entries
        .iter()
        .filter(|e| !e.is_dir && !is_dotfile(&e.path) && !manifest_paths.contains(e.path.as_str()))
        .collect();
    new_entries.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    let new_files: Vec<&str> = new_entries.iter().map(|e| e.path.as_str()).collect();
    if !new_files.is_empty() {
        // WARN, not error — deliberately weaker than BITROT/MISSING/DIRTY.
        //
        // The design doc (§2.13) files NEW as a `check-integrity` REPORT
        // status, not a stage-time gate, and a gate whose false positives
        // halt legitimate work is worse than no gate. tar is given the
        // snapshot's list, so the file is simply not archived; the warning
        // says so.
        tracing::warn!(
            files = %new_files.join(", "),
            "NEW: file(s) on disk are absent from the snapshot manifest and are not \
             archived by this stage. Re-run `snapshot create` before staging if these \
             should be archived (design §2.13)"
        );
    }

    // Content validation applies to regular files only (issue #33/H7). A
    // symlink's recorded size is its target string's length, and opening a
    // FIFO with no writer blocks forever: both are recorded, never hashed.
    let mut regular: HashMap<&str, (i64, Option<&String>)> = all_entries
        .iter()
        .filter(|(_, _, _, kind)| *kind == FileKind::Regular)
        .map(|(path, size, sha, _)| (path.as_str(), (*size, sha.as_ref())))
        .collect();

    let mut files = Vec::with_capacity(regular.len());
    for entry in disk_entries.iter().filter(|e| !e.is_dir) {
        if is_dotfile(&entry.path) {
            if entry.file_type == "regular" {
                files.push(PlannedFile {
                    rel_path: entry.path.clone(),
                    expected_size: entry.size,
                    baseline: None,
                });
            }
            continue;
        }
        if let Some((size, baseline)) = regular.remove(entry.path.as_str()) {
            check_source_size(&base.join(&entry.path), &entry.path, size)?;
            files.push(PlannedFile {
                rel_path: entry.path.clone(),
                expected_size: size,
                baseline: baseline.cloned(),
            });
        }
    }
    // What the walk did not find, in manifest order: MISSING (refused here),
    // or on disk but excluded since the snapshot. tar is given the
    // snapshot's list, which holds such a file; but the unit's patterns now
    // say it is not part of the unit, so the snapshot no longer describes
    // the unit (issue #464): refuse, and a new snapshot is the remedy.
    let mut excluded_since: Vec<&str> = Vec::new();
    for (path, _, _, kind) in &all_entries {
        if *kind != FileKind::Regular {
            continue;
        }
        if let Some((size, _)) = regular.remove(path.as_str()) {
            check_source_size(&base.join(path), path, size)?;
            excluded_since.push(path);
        }
    }
    if !excluded_since.is_empty() {
        return Err(TapectlError::Other(format!(
            "the snapshot lists {} the unit's exclude patterns now exclude: {}. The snapshot \
             no longer describes the unit as its patterns say: take a new snapshot (`tapectl snapshot create \
             <unit>`) and stage that, or remove the exclude.",
            count_of_files(excluded_since.len()),
            named_files(&excluded_since),
        )));
    }
    Ok(SourcePlan { files })
}

/// `1 file`, `3 files`.
fn count_of_files(n: usize) -> String {
    format!("{n} file{}", if n == 1 { "" } else { "s" })
}

/// How many file names a refusal spells out before summarising the rest.
const NAMED_FILES: usize = 10;

/// The first [`NAMED_FILES`] of `paths`, comma-separated, and a count of the
/// rest.
fn named_files<S: AsRef<str>>(paths: &[S]) -> String {
    let shown: Vec<&str> = paths.iter().take(NAMED_FILES).map(AsRef::as_ref).collect();
    let mut text = shown.join(", ");
    if paths.len() > NAMED_FILES {
        text.push_str(&format!(" and {} more", paths.len() - NAMED_FILES));
    }
    text
}

/// What a file's metadata said: what a recheck compares afterwards. A
/// write moves the change time, which no unprivileged writer can set back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Seen {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) size: u64,
    pub(crate) mtime_ns: i128,
    pub(crate) ctime_ns: i128,
}

impl Seen {
    pub(crate) fn of(m: &std::fs::Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            size: m.len(),
            mtime_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
            ctime_ns: i128::from(m.ctime()) * 1_000_000_000 + i128::from(m.ctime_nsec()),
        }
    }
}

/// One hashed file.
#[derive(Debug, Clone)]
struct HashedFile {
    rel_path: String,
    sha256: String,
    seen: Seen,
    /// The wall-clock time the hash began, in ns since the epoch — on the
    /// clock file times are stamped with.
    started_ns: i128,
}

/// What [`hash_files`] read.
#[derive(Debug)]
pub(crate) struct Hashed {
    files: Vec<HashedFile>,
}

impl Hashed {
    /// `(relative_path, sha256_hex)` for every hashed file, for
    /// `staging::backfill_checksums`.
    pub(crate) fn checksums(&self) -> Vec<(String, String)> {
        self.files
            .iter()
            .map(|f| (f.rel_path.clone(), f.sha256.clone()))
            .collect()
    }
}

/// The DIRTY refusal for a file that changed while staging read it.
pub(crate) fn changed_while_staging(rel_path: &str, what: &str) -> TapectlError {
    TapectlError::Other(format!(
        "DIRTY: source file changed while staging read it: {rel_path} ({what}). Its \
         recorded sha256 would not be of the bytes archived, so nothing was staged. \
         Stage again once the source is quiet; if the change is real, take a new \
         snapshot (`tapectl snapshot create <unit>`) and stage that."
    ))
}

pub(crate) fn now_ns() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

/// Hash one planned file (see [`hash_files`]).
fn hash_one(full_path: &Path, planned: &PlannedFile) -> Result<HashedFile> {
    let rel_path = planned.rel_path.as_str();
    let expected_size = planned.expected_size;
    let started_ns = now_ns();
    // Never follow, never open a non-regular file (issue #33/H7: opening a
    // FIFO with no writer blocks forever).
    let path_meta = std::fs::symlink_metadata(full_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            TapectlError::Other(format!("source file missing: {rel_path}"))
        } else {
            TapectlError::Other(format!("cannot stat source file: {rel_path} ({e})"))
        }
    })?;
    if !path_meta.is_file() {
        return Err(TapectlError::Other(format!(
            "refusing to read non-regular file: {rel_path} — symlinks/FIFOs/sockets/devices \
             are never content-validated"
        )));
    }
    let file = std::fs::File::open(full_path)
        .map_err(|e| TapectlError::Other(format!("cannot open source file: {rel_path} ({e})")))?;
    let before = file.metadata()?;
    if before.ino() != path_meta.ino() || before.dev() != path_meta.dev() {
        return Err(changed_while_staging(
            rel_path,
            "it was replaced as it was opened",
        ));
    }
    if before.len() as i64 != expected_size {
        return Err(TapectlError::Other(format!(
            "DIRTY: source file size changed: {rel_path} (expected {expected_size} bytes, \
             found {} bytes) — a real edit (size and content both differ) since the \
             snapshot was taken. Take a new snapshot (`tapectl snapshot create <unit>`) \
             and stage that instead.",
            before.len()
        )));
    }
    // Read no further than the planned size; growth is refused below: the
    // second `fstat` sees the new size and change time.
    let mut reader = HashingReader::new(file.take(expected_size as u64));
    let mut buf = vec![0u8; VALIDATE_STREAM_BUFFER];
    let mut streamed: i64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        streamed += n as i64;
    }
    let hex = reader.finalize_hex();
    let after = reader.into_inner().into_inner().metadata()?;
    let seen = Seen::of(&after);
    if Seen::of(&before) != seen || streamed != expected_size {
        return Err(changed_while_staging(
            rel_path,
            "it changed while it was hashed",
        ));
    }

    // Commitment-point comparison (issue #32/H6): the size equals the
    // snapshot's, so a hash that differs from the baseline is content drift
    // at a constant size — bitrot, not an edit.
    if let Some(baseline) = planned.baseline.as_deref() {
        if baseline != hex {
            return Err(TapectlError::Other(format!(
                "BITROT suspected: {rel_path} — sha256 differs at an unchanged \
                 size ({expected_size} bytes): baseline={baseline}, current={hex}. \
                 Refusing to stage; investigate before re-staging \
                 (`tapectl unit check-integrity <unit>` checks every file against \
                 its recorded baseline)."
            )));
        }
    }
    Ok(HashedFile {
        rel_path: planned.rel_path.clone(),
        sha256: hex,
        seen,
        started_ns,
    })
}

/// Hash every planned file in turn, in the plan's order. For each: stat,
/// open, `fstat`, hash its planned size and no more, `fstat` again. A file
/// at another size than the snapshot's is DIRTY; one whose size, times or
/// inode moved while it was read is DIRTY; a file whose hash differs from
/// its baseline at the same size is BITROT suspected. What it saw is kept
/// for [`recheck`].
pub(crate) fn hash_files(base: &Path, plan: &SourcePlan) -> Result<Hashed> {
    let total = plan.files.len();
    info!(
        files = total,
        total_mb = plan.total_bytes() / (1024 * 1024),
        "hashing source files"
    );
    let mut files = Vec::with_capacity(total);
    for (done, planned) in plan.files.iter().enumerate() {
        // Issue #404: hours on a large unit, so a signal stops it between
        // files.
        crate::signal::check(|| {
            format!("stopped while checking the source ({done} of {total} files hashed)")
        })?;
        files.push(hash_one(&base.join(&planned.rel_path), planned)?);
    }
    info!(files = files.len(), "source files hashed");
    Ok(Hashed { files })
}

/// A file whose change time is this close to (or after) the moment its
/// hash began may have been written again within the same timestamp tick
/// without its times moving — git's "racy" case. Two seconds covers the
/// coarsest timestamps a source filesystem plausibly has.
pub(crate) const RACY_WINDOW_NS: i128 = 2_000_000_000;

/// After the hash: every hashed file must still be the file, at the size and
/// change time, that was hashed (issue #364). A file whose change time was
/// within [`RACY_WINDOW_NS`] of its hash is hashed again, and must hash the
/// same.
pub(crate) fn recheck(base: &Path, hashed: &Hashed) -> Result<()> {
    let total = hashed.files.len();
    for (done, f) in hashed.files.iter().enumerate() {
        crate::signal::check(|| {
            format!("stopped while re-checking the source ({done} of {total} files)")
        })?;
        let full_path = base.join(&f.rel_path);
        let meta = std::fs::symlink_metadata(&full_path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                changed_while_staging(&f.rel_path, "it was removed after it was hashed")
            } else {
                TapectlError::Other(format!("cannot stat source file: {} ({e})", f.rel_path))
            }
        })?;
        let now = Seen::of(&meta);
        if now.dev != f.seen.dev
            || now.ino != f.seen.ino
            || now.size != f.seen.size
            || now.ctime_ns != f.seen.ctime_ns
        {
            return Err(changed_while_staging(
                &f.rel_path,
                "it changed after it was hashed",
            ));
        }
        if f.seen.ctime_ns >= f.started_ns - RACY_WINDOW_NS {
            let (again, _) = hash_source_file(&full_path, &f.rel_path)?;
            if again != f.sha256 {
                return Err(changed_while_staging(
                    &f.rel_path,
                    "it hashed differently a second time",
                ));
            }
        }
    }
    Ok(())
}

/// The whole source check with no archiver beside it: [`plan`],
/// [`hash_files`] and [`recheck`] in turn — the archival **commitment
/// point** (issue #32/H6; design §2.13: "staging is the archival commitment
/// point — full sha256 every time"), the same verdicts `stage create` gives
/// from the stream's hashes:
///
///   - no baseline yet                  -> establish it (normal, not an error)
///   - baseline matches                 -> fine
///   - baseline differs, SAME size      -> BITROT suspected: refuse to stage
///   - baseline differs, size DIFFERS   -> DIRTY: a real edit (issue #36's
///     scope, not bitrot — see `check_source_size`)
///   - a manifest file absent from disk -> MISSING
///
/// Returns every regular file's `(relative_path, sha256_hex)`;
/// `backfill_checksums`'s own `sha256 IS NULL` guard is what prevents
/// overwriting an existing baseline.
pub fn validate_source(
    conn: &Connection,
    snapshot_id: i64,
    source_path: &str,
    global_excludes: &[String],
) -> Result<SourceValidation> {
    let plan = plan(conn, snapshot_id, source_path, global_excludes)?;
    crate::progress::set_total(plan.total_bytes());
    let base = Path::new(source_path);
    let hashed = hash_files(base, &plan)?;
    recheck(base, &hashed)?;
    Ok(SourceValidation {
        checksums: hashed.checksums(),
    })
}

/// Stat `full_path` and confirm its current size matches `expected_size`
/// (the size recorded in the manifest at snapshot time) — a plain
/// `metadata()` call, no read (H9 remainder, issue #84): fails fast on a
/// missing or already-changed file before any I/O is spent hashing it.
///
/// Doubles as two of the issue #32/H6 classifications, both unconditional
/// on whether a sha256 baseline exists (they only need a size comparison):
///   - NotFound             -> MISSING (unchanged wording/behavior — this
///     already errored before #32; not touched here).
///   - size mismatch        -> DIRTY: a real edit changed both size and
///     content. Deliberately kept out of `validate_source`'s BITROT
///     wording (and vice versa) so the two outcomes can never be
///     conflated — full dirty-detection machinery (`unit info --dirty`,
///     `mark-tape-only`'s guard) is issue #36's scope, not this one's.
fn check_source_size(full_path: &Path, rel_path: &str, expected_size: i64) -> Result<()> {
    let metadata = std::fs::metadata(full_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            TapectlError::Other(format!("source file missing: {rel_path}"))
        } else {
            TapectlError::Other(format!("cannot access source file: {rel_path} ({e})"))
        }
    })?;

    if metadata.len() as i64 != expected_size {
        return Err(TapectlError::Other(format!(
            "DIRTY: source file size changed: {rel_path} (expected {expected_size} bytes, \
             found {} bytes) — a real edit (size and content both differ) since the \
             snapshot was taken. Take a new snapshot (`tapectl snapshot create <unit>`) \
             and stage that instead.",
            metadata.len()
        )));
    }
    Ok(())
}

/// Fixed-size buffer for streaming source-file validation (H9 remainder,
/// issue #84) — same 128 KiB convention as `encrypt_file_streaming`'s
/// `STREAM_COPY_BUFFER` (`src/staging/mod.rs`) and
/// `volume::layout_model::hash_file`. Peak RAM for `hash_source_file` is
/// this buffer alone, never the size of the file being validated.
const VALIDATE_STREAM_BUFFER: usize = 128 * 1024;

/// Stream-hash `full_path`, returning `(sha256_hex, bytes_read)` — reuses
/// `util::HashingReader` in a fixed-size buffer loop exactly as
/// `encrypt_file_streaming` does (H9 remainder, issue #84), so this never
/// holds more than `VALIDATE_STREAM_BUFFER` of the file in RAM regardless of
/// its size. The byte count is returned alongside the hash (not just the
/// hash) so the caller can detect a file that changed size during this very
/// read — see the TOCTOU guard in `validate_source`.
///
/// `pub(crate)` (issue #32/H6): `cli::operations::unit_check_integrity` was
/// the last whole-file `fs::read` site (H9-class); it now streams through
/// this exact function instead of growing a second implementation, so the
/// two call sites can never disagree about what a file's sha256 is.
///
/// Defense in depth (issue #33/H7): `validate_source`'s own `file_type`
/// filter is the primary guard, but this function refuses to `File::open`
/// anything that isn't confirmed a regular file, independent of any
/// caller's filtering. `symlink_metadata` (never follows) runs first and
/// unconditionally — a FIFO with no writer blocks `File::open` forever
/// with no timeout, so that call must never be reached for anything else.
pub(crate) fn hash_source_file(full_path: &Path, rel_path: &str) -> Result<(String, i64)> {
    let (hex, total, _) = stream_source_file(full_path, rel_path, &mut |_| {})?;
    Ok((hex, total))
}

/// [`hash_source_file`]'s read loop, handing every chunk read to `chunk` as
/// well, and returning the `symlink_metadata` it checked before opening —
/// how `validate_source` counts non-zero bytes and recognises a hard-linked
/// inode in the one read it already makes (issue #354).
fn stream_source_file(
    full_path: &Path,
    rel_path: &str,
    chunk: &mut dyn FnMut(&[u8]),
) -> Result<(String, i64, std::fs::Metadata)> {
    let meta = std::fs::symlink_metadata(full_path)
        .map_err(|e| TapectlError::Other(format!("cannot stat source file: {rel_path} ({e})")))?;
    if !meta.is_file() {
        return Err(TapectlError::Other(format!(
            "refusing to read non-regular file: {rel_path} — symlinks/FIFOs/sockets/devices \
             are never content-validated"
        )));
    }

    let file = std::fs::File::open(full_path)
        .map_err(|e| TapectlError::Other(format!("cannot open source file: {rel_path} ({e})")))?;
    let mut reader = HashingReader::new(file);
    let mut buf = [0u8; VALIDATE_STREAM_BUFFER];
    let mut total: i64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        chunk(&buf[..n]);
        total += n as i64;
    }
    Ok((reader.finalize_hex(), total, meta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::io::Write;
    use tempfile::TempDir;

    /// `files` is `(path, size_bytes, sha256_baseline)` — the third element
    /// seeds `files.sha256` as it would stand after
    /// a *previous* successful stage (issue #32/H6): `None` simulates a
    /// snapshot that has never been staged (no baseline yet — the
    /// commitment point hasn't happened); `Some(hex)` simulates a re-stage
    /// with an already-established baseline to compare against.
    fn setup_conn_with_snapshot(files: &[(&str, i64, Option<&str>)]) -> (Connection, i64) {
        // Full ordered migration chain (issue #44) — this used to hand-apply
        // 001 + 005 directly, skipping 002-004. That was non-contiguous: any
        // schema fact 002-004 establish (e.g. the FTS5 `files_fts` shadow
        // table + triggers from 002) was silently absent here while every
        // other real code path already went through the full chain.
        let conn = crate::db::open_memory().unwrap();

        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('u1', 'u', ?1, 'mtime_size', 1, 'active')",
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
        let sid = conn.last_insert_rowid();

        for (path, size, sha) in files {
            // file_type = 'regular' unconditionally: every existing caller of
            // this helper plants a genuine regular-file scenario. Symlink/
            // special rows for the issue #33/H7 tests go through
            // `insert_nonregular_file` instead, which takes file_type
            // explicitly.
            crate::db::files::fixture::insert(&conn, sid, path, *size, "regular", *sha);
        }
        (conn, sid)
    }

    /// Issue #404: the sha256 pass reads every byte of the unit, so a
    /// signal stops it between files.
    #[test]
    fn validate_source_stops_between_files_on_a_signal() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        let (conn, sid) = setup_conn_with_snapshot(&[("a.txt", 5, None)]);
        crate::signal::interrupt_this_thread(true);
        let r = validate_source(&conn, sid, tmp.path().to_str().unwrap(), &[]);
        crate::signal::interrupt_this_thread(false);
        match r {
            Err(TapectlError::Interrupted(at)) => {
                assert_eq!(
                    at,
                    "stopped while checking the source (0 of 1 files hashed)"
                )
            }
            Err(e) => panic!("expected an interruption, got {e:?}"),
            Ok(_) => panic!("expected an interruption, got Ok"),
        }
    }

    /// Issue #364: a file written again after it was hashed — its change
    /// time moves — fails the recheck that ties the hash to what was archived.
    #[test]
    fn recheck_refuses_a_file_changed_after_its_hash() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        let (conn, sid) = setup_conn_with_snapshot(&[("a.txt", 5, None)]);
        let base = tmp.path();
        let plan = plan(&conn, sid, base.to_str().unwrap(), &[]).unwrap();
        let hashed = hash_files(base, &plan).unwrap();
        recheck(base, &hashed).expect("unchanged: passes");

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(base.join("a.txt"), b"HELLO").unwrap();
        let err = recheck(base, &hashed).unwrap_err().to_string();
        assert!(err.contains("DIRTY") && err.contains("a.txt"), "{err}");
    }

    #[test]
    fn validate_source_happy_path() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(tmp.path().join("b.bin"), b"world!!").unwrap();

        let (conn, sid) = setup_conn_with_snapshot(&[("a.txt", 5, None), ("b.bin", 7, None)]);
        let result = validate_source(&conn, sid, tmp.path().to_str().unwrap(), &[])
            .unwrap()
            .checksums;
        assert_eq!(result.len(), 2);
        // Sha256 of "hello" is 2cf24d...
        let hello = result
            .iter()
            .find(|(p, _)| p == "a.txt")
            .expect("a.txt in results");
        assert_eq!(
            hello.1,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn validate_source_missing_file_errors() {
        // MISSING classification (issue #32): a manifest file absent from
        // disk. This already errored before #32 — preserved verbatim.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("present.txt"), b"ok").unwrap();

        let (conn, sid) =
            setup_conn_with_snapshot(&[("present.txt", 2, None), ("missing.txt", 10, None)]);
        let err = validate_source(&conn, sid, tmp.path().to_str().unwrap(), &[])
            .err()
            .unwrap();
        let msg = format!("{err}");
        assert!(
            msg.contains("missing.txt"),
            "expected error to mention missing file, got: {msg}"
        );
    }

    #[test]
    fn validate_source_size_mismatch_errors() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("growing.txt"), b"actually longer").unwrap();

        let (conn, sid) = setup_conn_with_snapshot(&[("growing.txt", 3, None)]);
        let err = validate_source(&conn, sid, tmp.path().to_str().unwrap(), &[])
            .err()
            .unwrap();
        let msg = format!("{err}");
        assert!(msg.contains("size changed"), "got: {msg}");
        // H9 remainder (issue #84): the size check moved from a whole-file
        // `fs::read` to a `metadata()` stat, but the error must still name
        // the file — this is operator-facing.
        assert!(
            msg.contains("growing.txt"),
            "error must name the file, got: {msg}"
        );
    }

    // --- Bitrot commitment point (issue #32/H6) ---------------------------
    //
    // `validate_source` used to compute a fresh sha256 for every file and
    // compare it against *nothing* — the query never even selected
    // `sha256`. These tests drive the full classification the fix adds:
    // baseline-absent (commitment point), baseline-matches, baseline-differs
    // at the SAME size (bitrot — refuse to stage), baseline-differs at a
    // DIFFERENT size (dirty — #36's scope, not bitrot), and on-disk files
    // the manifest never saw (NEW — refuse to stage, per the issue's own
    // remediation text: "diff walked set vs manifest for NEW/MISSING").

    #[test]
    fn first_stage_establishes_a_baseline_where_none_existed() {
        // The commitment point itself: no `files.sha256` recorded yet ⇒
        // this is normal, not an error, and the hash computed here is what
        // `backfill_checksums` will use to establish the baseline.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();

        let (conn, sid) = setup_conn_with_snapshot(&[("a.txt", 5, None)]);
        let checksums = validate_source(&conn, sid, tmp.path().to_str().unwrap(), &[])
            .unwrap()
            .checksums;
        assert_eq!(checksums.len(), 1);
        let (path, hex) = &checksums[0];
        assert_eq!(path, "a.txt");
        assert_eq!(
            hex,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );

        // Simulate stage_create's backfill step and confirm it actually
        // lands — there is nothing to protect yet, so this must write.
        crate::staging::backfill_checksums(&conn, sid, &checksums).unwrap();
        let stored: Option<String> = crate::db::files::fixture::sha256(&conn, sid, "a.txt");
        assert_eq!(stored.as_deref(), Some(hex.as_str()));
    }

    #[test]
    fn restage_with_unchanged_content_passes_and_baseline_is_untouched() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        let baseline = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

        let (conn, sid) = setup_conn_with_snapshot(&[("a.txt", 5, Some(baseline))]);
        let checksums = validate_source(&conn, sid, tmp.path().to_str().unwrap(), &[])
            .unwrap()
            .checksums;
        assert_eq!(checksums[0].1, baseline);

        crate::staging::backfill_checksums(&conn, sid, &checksums).unwrap();
        let stored: Option<String> = crate::db::files::fixture::sha256(&conn, sid, "a.txt");
        assert_eq!(stored.as_deref(), Some(baseline));
    }

    #[test]
    fn same_size_different_content_is_bitrot_suspected_and_baseline_not_overwritten() {
        // The whole reason issue #32/H6 exists: content changed at a
        // constant size. Must refuse to stage, must name the file and BOTH
        // hashes and the shared size, and must never let the corrupt
        // content's hash reach the baseline.
        let tmp = TempDir::new().unwrap();
        let actual_content = b"HELLO"; // same size as "hello", different bytes
        std::fs::write(tmp.path().join("a.txt"), actual_content).unwrap();
        let stale_baseline = direct_hash(b"hello");
        let current_hex = direct_hash(actual_content);
        assert_ne!(
            stale_baseline, current_hex,
            "test setup must actually differ"
        );

        let (conn, sid) = setup_conn_with_snapshot(&[("a.txt", 5, Some(stale_baseline.as_str()))]);
        let err = validate_source(&conn, sid, tmp.path().to_str().unwrap(), &[])
            .err()
            .unwrap();
        let msg = format!("{err}");

        assert!(msg.contains("BITROT"), "must name it BITROT, got: {msg}");
        assert!(msg.contains("a.txt"), "must name the file, got: {msg}");
        // Issue #357: the next step, not an issue number.
        assert!(
            msg.contains("tapectl unit check-integrity") && !msg.contains('#'),
            "must name the command that investigates, got: {msg}"
        );
        assert!(
            msg.contains(stale_baseline.as_str()),
            "must show the baseline hash, got: {msg}"
        );
        assert!(
            msg.contains(current_hex.as_str()),
            "must show the current hash, got: {msg}"
        );
        assert!(
            msg.contains("5 bytes"),
            "must show the shared size, got: {msg}"
        );

        // The baseline must survive completely untouched — validate_source
        // itself never writes, but assert directly against the DB so this
        // test also guards against a future refactor that calls backfill
        // unconditionally before checking the result.
        let stored: Option<String> = crate::db::files::fixture::sha256(&conn, sid, "a.txt");
        assert_eq!(stored.as_deref(), Some(stale_baseline.as_str()));
    }

    #[test]
    fn different_size_different_content_is_classified_dirty_not_bitrot() {
        // A real edit (both size and content differ) is DIRTY (#36's
        // scope), never BITROT — the two outcomes must stay distinctly
        // named so they can never be conflated.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"a much longer edited body").unwrap();
        let baseline = direct_hash(b"hello");

        let (conn, sid) = setup_conn_with_snapshot(&[("a.txt", 5, Some(baseline.as_str()))]);
        let err = validate_source(&conn, sid, tmp.path().to_str().unwrap(), &[])
            .err()
            .unwrap();
        let msg = format!("{err}");

        assert!(msg.contains("DIRTY"), "must name it DIRTY, got: {msg}");
        assert!(
            !msg.contains("BITROT"),
            "dirty and bitrot must be mutually exclusive outcomes, got: {msg}"
        );
        assert!(msg.contains("a.txt"), "must name the file, got: {msg}");
        // Issue #357: the remedy, not "tracked separately under issue #36".
        assert!(
            msg.contains("tapectl snapshot create") && !msg.contains('#'),
            "must name the remedy, got: {msg}"
        );
    }

    #[test]
    fn new_file_on_disk_not_in_manifest_warns_but_does_not_refuse() {
        // NEW is a WARNING, not a gate — see the rationale at the check
        // itself. The design doc (§2.13) files NEW as a check-integrity
        // report status, so blocking staging on it would halt legitimate
        // work for a file that is genuinely new (not excluded by anything
        // — `stray.tmp` matches neither a dotfile nor a global pattern
        // here), which must still surface as a WARNing, not an error.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(tmp.path().join("stray.tmp"), b"appeared after snapshot").unwrap();
        let baseline = direct_hash(b"hello");
        let (conn, sid) = setup_conn_with_snapshot(&[("a.txt", 5, Some(baseline.as_str()))]);

        let out = validate_source(&conn, sid, tmp.path().to_str().unwrap(), &[]);
        assert!(
            out.is_ok(),
            "a NEW file must warn, not refuse staging: {out:?}"
        );
        let checksums = out.unwrap().checksums;
        assert!(
            checksums.iter().any(|(p, _)| p == "a.txt"),
            "the manifest's own file must still be hashed and returned: {checksums:?}"
        );
    }

    #[test]
    fn backfill_checksums_sql_guard_refuses_to_overwrite_an_existing_baseline() {
        // Defense in depth: even called directly with a hash that
        // disagrees with an existing baseline — bypassing
        // `validate_source`'s own refusal entirely — the UPDATE's own
        // `sha256 IS NULL` guard must still refuse the write. This is what
        // makes the "(first stage only)" comment literally true rather
        // than a promise nothing enforces.
        let original = "0a".repeat(32);
        let (conn, sid) = setup_conn_with_snapshot(&[("a.txt", 5, Some(original.as_str()))]);

        crate::staging::backfill_checksums(&conn, sid, &[("a.txt".to_string(), "0b".repeat(32))])
            .unwrap();

        let stored: Option<String> = crate::db::files::fixture::sha256(&conn, sid, "a.txt");
        assert_eq!(
            stored,
            Some(original),
            "backfill must never overwrite an existing sha256 baseline"
        );
    }

    #[test]
    fn backfill_checksums_establishes_a_baseline_when_absent() {
        let (conn, sid) = setup_conn_with_snapshot(&[("a.txt", 5, None)]);
        let fresh = "0c".repeat(32);
        crate::staging::backfill_checksums(&conn, sid, &[("a.txt".to_string(), fresh.clone())])
            .unwrap();

        let stored: Option<String> = crate::db::files::fixture::sha256(&conn, sid, "a.txt");
        assert_eq!(stored, Some(fresh));
    }

    // --- issue #354: the staging-space lower bound the read yields ---

    #[test]
    fn validate_source_skips_directories() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();
        std::fs::write(tmp.path().join("subdir/f.txt"), b"x").unwrap();

        // Insert a directory row alongside the file — validate_source
        // must filter it out and not try to read it as a file.
        // Full ordered migration chain (issue #44) — see setup_conn_with_snapshot.
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('u1', 'u', ?1, 'mtime_size', 1, 'active')",
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
        let sid = conn.last_insert_rowid();
        crate::db::files::fixture::insert(&conn, sid, "subdir", 0, "dir", None);
        crate::db::files::fixture::insert(&conn, sid, "subdir/f.txt", 1, "regular", None);

        let result = validate_source(&conn, sid, tmp.path().to_str().unwrap(), &[])
            .unwrap()
            .checksums;
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "subdir/f.txt");
    }

    // --- H9 remainder (issue #84): validate_source's per-file read used to
    // be a whole-file `std::fs::read`, so the OOM #35 closed for the
    // *encrypted slice* simply moved here, keyed to the largest single
    // *source* file — per `v2-open-questions.md` §7 the media-library
    // workload is folders of 2-15 GB typically dominated by ONE file at
    // ~90%. The fix streams the size check (metadata() only, no read) and
    // the hash (via `util::HashingReader` in fixed chunks, mirroring
    // `encrypt_file_streaming` in `src/staging/mod.rs`), plus a byte-count
    // guard for the TOCTOU window the two-pass split introduces. The tests
    // below exercise the new `check_source_size`/`hash_source_file`/
    // `VALIDATE_STREAM_BUFFER` pieces directly.

    fn direct_hash(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        format!("{:x}", h.finalize())
    }

    #[test]
    fn hash_source_file_matches_direct_sha256_digest_for_content_larger_than_one_buffer() {
        // Content must exceed VALIDATE_STREAM_BUFFER (128 KiB) to prove
        // this isn't a single-`read()` toy case — varied per-line content,
        // not one repeated byte, so the hash reflects the whole input.
        let mut content = Vec::new();
        for i in 0..5000u32 {
            content.extend_from_slice(format!("line {i} of varied source content\n").as_bytes());
        }
        assert!(
            content.len() > VALIDATE_STREAM_BUFFER,
            "test content must exceed one buffer to be meaningful, got {} bytes",
            content.len()
        );

        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("big.bin");
        std::fs::write(&path, &content).unwrap();

        let expected_hex = direct_hash(&content);
        let (hex, streamed) = hash_source_file(&path, "big.bin").unwrap();

        assert_eq!(streamed, content.len() as i64);
        assert_eq!(
            hex, expected_hex,
            "streaming hash must equal Sha256::digest of the same bytes \
             (this feeds files.sha256 / the bitrot baseline, issue #32)"
        );

        // Reproduce the *exact* old code verbatim — `Sha256::digest(&data)`
        // followed by the same byte-iteration hex formatting the pre-#84
        // code used (`hash.iter().map(|b| format!("{b:02x}")).collect()`),
        // not just `HashingReader::finalize_hex`'s `{:x}` compared against
        // itself. This is the literal equivalence the fix must preserve:
        // the sha256 hex feeds `files.sha256` and the bitrot baseline
        // (#32), so a formatting drift here would be silently wrong.
        let old_style_hex: String = Sha256::digest(&content)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            hex, old_style_hex,
            "must match the exact byte-iteration hex the old code produced"
        );
    }

    #[test]
    fn hash_source_file_handles_several_chunks_of_varied_content() {
        // 640 KiB = 5x VALIDATE_STREAM_BUFFER (128 KiB): forces multiple
        // read() loop iterations without staging anything close to a real
        // multi-GB source file in a unit test. Content varies per block
        // (not N copies of one block) so a bug that only reads the first
        // buffer's worth would produce a detectably wrong hash.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("multi.bin");

        let mut f = std::fs::File::create(&path).unwrap();
        let mut expected_hasher = Sha256::new();
        let mut total_len: u64 = 0;
        for i in 0..20u64 {
            let mut block = vec![0xABu8; 32 * 1024];
            block[0] = (i % 256) as u8;
            block[1] = ((i / 256) % 256) as u8;
            f.write_all(&block).unwrap();
            expected_hasher.update(&block);
            total_len += block.len() as u64;
        }
        drop(f);
        let expected_hex = format!("{:x}", expected_hasher.finalize());

        let (hex, streamed) = hash_source_file(&path, "multi.bin").unwrap();
        assert_eq!(streamed, total_len as i64);
        assert_eq!(hex, expected_hex);
    }

    #[test]
    fn check_source_size_is_instant_and_needs_no_read() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("f.bin");
        std::fs::write(&path, vec![0u8; 4096]).unwrap();

        assert!(check_source_size(&path, "f.bin", 4096).is_ok());

        let err = check_source_size(&path, "f.bin", 9999).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("size changed"), "got: {msg}");
        assert!(msg.contains("f.bin"), "got: {msg}");
    }

    #[test]
    fn check_source_size_missing_file_names_it() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nope.bin");

        let err = check_source_size(&path, "nope.bin", 10).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("missing"), "got: {msg}");
        assert!(msg.contains("nope.bin"), "got: {msg}");
    }

    #[test]
    fn byte_count_guard_catches_a_file_that_changes_size_after_the_metadata_check() {
        // `check_source_size` and `hash_source_file` are the exact two
        // operations `validate_source` calls in order, with a gap between
        // them — the TOCTOU window the byte-count guard exists for. A real
        // wall-clock race between two threads landing precisely in that
        // gap would be inherently timing-dependent (flaky); instead this
        // drives the two calls directly with a real mutation performed in
        // the gap, which is deterministic and exercises the exact same two
        // functions in the exact same order `validate_source` uses.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("shifting.bin");
        std::fs::write(&path, vec![0xCDu8; 4096]).unwrap();

        // Metadata check passes: on-disk size matches the recorded size.
        check_source_size(&path, "shifting.bin", 4096).unwrap();

        // File changes size in the gap before the streaming pass runs.
        std::fs::write(&path, vec![0xCDu8; 2048]).unwrap();

        let (_hex, streamed) = hash_source_file(&path, "shifting.bin").unwrap();

        // The byte count actually streamed no longer matches what
        // `check_source_size` verified moments before — exactly the
        // condition `validate_source`'s guard checks after calling both
        // functions, and it must not go unnoticed.
        assert_ne!(
            streamed, 4096,
            "the guard's premise: streamed count must diverge from the pre-checked size"
        );
        assert_eq!(streamed, 2048);
    }

    // --- Symlinks and special files (issue #33/H7) -------------------------
    //
    // `walk_directory` and `validate_source` used to disagree about
    // link-following: the walk recorded a symlink's target-string length as
    // its "size" (never following), while `check_source_size` used
    // `std::fs::metadata` (which DOES follow) to compare against the
    // target's real content size. Any symlink whose name-target length
    // differed from the target's content size produced a false DIRTY (the
    // gate's exact fixture: a 10-character target name pointing at 7 bytes
    // of content). A broken symlink was reported as a missing source file.
    // Opening a FIFO with no writer via `File::open` blocked forever with
    // no timeout. The fix: content validation (size check + sha256) applies
    // to regular files only — symlinks/specials are recorded (file_type +
    // link_target) but excluded from the validation set entirely.

    /// Plants one additional non-regular `files` row
    /// alongside whatever `setup_conn_with_snapshot` already inserted — that
    /// helper hardcodes `file_type = 'regular'` (every existing caller is a
    /// genuine regular-file scenario), so symlink/special rows need their
    /// own insert with an explicit `file_type`/`link_target`.
    fn insert_nonregular_file(
        conn: &Connection,
        snapshot_id: i64,
        path: &str,
        size: i64,
        file_type: &str,
        link_target: Option<&str>,
    ) {
        crate::db::files::fixture::insert_entry(
            conn,
            snapshot_id,
            crate::db::files::FileEntry {
                path: path.to_string(),
                kind: FileKind::from_name(file_type).unwrap(),
                size_bytes: size,
                mtime_ns: None,
                sha256: None,
                link_target: link_target.map(str::to_string),
            },
        );
    }

    #[test]
    fn good_symlink_with_mismatched_target_length_does_not_false_positive_dirty() {
        // Reproduces the mhvtl gate's exact fixture shape: target.txt holds
        // 7 bytes of content, link-ok's target-string "target.txt" is 10
        // characters. Pre-fix, `check_source_size` compared 10
        // (walk_directory's recorded "size" for the symlink) against
        // fs::metadata's followed 7 and raised "DIRTY: source file size
        // changed" — a false positive with nothing actually dirty.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("target.txt"), b"target\n").unwrap();
        std::os::unix::fs::symlink("target.txt", tmp.path().join("link-ok")).unwrap();

        let (conn, sid) = setup_conn_with_snapshot(&[("target.txt", 7, None)]);
        // Matches what walk_directory records for this symlink: size_bytes
        // = meta.len() = len("target.txt") = 10 (lstat's own size for the
        // symlink object — unchanged by this fix; only content
        // *validation* is skipped, not the recorded size, see the commit
        // message for why).
        insert_nonregular_file(&conn, sid, "link-ok", 10, "symlink", Some("target.txt"));

        let result = validate_source(&conn, sid, tmp.path().to_str().unwrap(), &[]);
        assert!(
            result.is_ok(),
            "a mismatched-length symlink must not produce a false DIRTY: {result:?}"
        );
        let checksums = result.unwrap().checksums;
        assert!(
            checksums.iter().any(|(p, _)| p == "target.txt"),
            "the real regular file must still be validated: {checksums:?}"
        );
        assert!(
            !checksums.iter().any(|(p, _)| p == "link-ok"),
            "the symlink must be excluded from the validation set: {checksums:?}"
        );
    }

    #[test]
    fn broken_symlink_does_not_error_as_missing() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("target.txt"), b"target\n").unwrap();
        std::os::unix::fs::symlink("does-not-exist.txt", tmp.path().join("dangling")).unwrap();

        let (conn, sid) = setup_conn_with_snapshot(&[("target.txt", 7, None)]);
        insert_nonregular_file(
            &conn,
            sid,
            "dangling",
            15,
            "symlink",
            Some("does-not-exist.txt"),
        );

        let result = validate_source(&conn, sid, tmp.path().to_str().unwrap(), &[]);
        assert!(
            result.is_ok(),
            "a broken symlink must not error at all — excluded from validation: {result:?}"
        );
        let checksums = result.unwrap().checksums;
        assert!(
            !checksums.iter().any(|(p, _)| p == "dangling"),
            "the broken symlink must be excluded from the validation set: {checksums:?}"
        );
    }

    #[test]
    fn special_file_excluded_from_validation_set_without_needing_a_live_fifo_on_disk() {
        // Deliberately does NOT create a real FIFO on disk at this path: if
        // the exclusion filter (the `file_type = 'regular'` restriction on
        // validate_source's SELECT) ever regresses on its own,
        // hash_source_file's independent defense-in-depth guard would still
        // convert a reintroduced special-file lookup into a clean `Err`
        // (nothing exists at this path) — but this test's job is to prove
        // the exclusion itself, not to depend on that second layer, so it
        // never puts a live, writer-less FIFO anywhere a regression could
        // reach `File::open` on it and hang the suite (see the commit
        // message and `hash_source_file_refuses_a_fifo_instead_of_blocking`
        // for the one place a real FIFO is used, which is safe only because
        // it is itself a direct, deterministic call to the guarded
        // function).
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();

        let (conn, sid) = setup_conn_with_snapshot(&[("a.txt", 5, None)]);
        insert_nonregular_file(&conn, sid, "a.fifo", 0, "special", None);

        let checksums = validate_source(&conn, sid, tmp.path().to_str().unwrap(), &[])
            .unwrap()
            .checksums;
        assert_eq!(
            checksums.len(),
            1,
            "the special file must be excluded from the validation set: {checksums:?}"
        );
        assert_eq!(checksums[0].0, "a.txt");
    }

    #[test]
    fn hash_source_file_refuses_a_fifo_instead_of_blocking() {
        // Direct, deterministic call — not a thread race against a hang.
        // Given the fix, hash_source_file's symlink_metadata check runs
        // BEFORE any File::open, so this returns Err immediately by
        // construction; it never reaches the open() call that would
        // otherwise block forever waiting for a writer that will never
        // come. (Do not run this test against the pre-fix code — with no
        // writer ever connecting, it hangs rather than failing; see the
        // commit message.)
        let tmp = TempDir::new().unwrap();
        let fifo_path = tmp.path().join("myfifo");
        nix::unistd::mkfifo(
            &fifo_path,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();

        let result = hash_source_file(&fifo_path, "myfifo");
        assert!(
            result.is_err(),
            "hash_source_file must refuse a FIFO, not block trying to read it: {result:?}"
        );
    }
}
