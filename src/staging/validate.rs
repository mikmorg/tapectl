use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

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

/// One regular file of the snapshot to hash: in the order dar reads it.
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
    /// The bytes [`hash_files`] will read.
    pub(crate) fn total_bytes(&self) -> u64 {
        self.files
            .iter()
            .map(|f| f.expected_size.max(0) as u64)
            .sum()
    }
}

/// Check the source against the snapshot's file list by metadata alone, and
/// list the regular files to hash in the order dar will read them (issue
/// #364). This is what still refuses before dar starts: a manifest file
/// gone from disk (MISSING) or at another size (DIRTY). Content — BITROT,
/// and a file changing while staging reads it — is checked by
/// [`hash_files`] and [`recheck`], while dar runs.
///
/// dar reads a directory tree in readdir order, depth first (measured,
/// docs/research/2026-10-06-plaintext-free-staging.md §4.2), and
/// `staging::walk_directory` walks the same way with the same exclude
/// patterns, so the hasher reads each file just before or just after dar
/// does, and the second read comes from the page cache. A manifest file the
/// patterns exclude since the snapshot is still hashed (it is in the
/// snapshot), at the end; dar skips it.
///
/// Also diffs the on-disk file set against the manifest for NEW files (issue
/// #32/H6): a file dar will archive that the catalog has never seen is a
/// warning, not a refusal (see the comment at the check).
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
    // NEW, and its absence is not MISSING. dar still archives it, so it is
    // still hashed (at the size it has now, below), and the first stage of a
    // Version records that hash as for any file.
    let is_dotfile = crate::unit::dotfile::is_unit_dotfile;
    let all_entries: Vec<_> = all_entries
        .into_iter()
        .filter(|(p, ..)| !is_dotfile(p))
        .collect();

    // The walk dar's own walk matches (same order, same patterns): the NEW
    // check below, and the hashing order.
    let (_, _, disk_entries) = super::walk_directory(source_path, global_excludes)?;
    let manifest_paths: HashSet<&str> = all_entries.iter().map(|(p, ..)| p.as_str()).collect();
    let mut new_files: Vec<&str> = disk_entries
        .iter()
        .filter(|e| !e.is_dir && !is_dotfile(&e.path) && !manifest_paths.contains(e.path.as_str()))
        .map(|e| e.path.as_str())
        .collect();
    new_files.sort_unstable();
    if !new_files.is_empty() {
        // WARN, not error — deliberately weaker than BITROT/MISSING/DIRTY.
        //
        // The design doc (§2.13) files NEW as a `check-integrity` REPORT
        // status, not a stage-time gate, and a gate whose false positives
        // halt legitimate work is worse than no gate: operators learn to
        // bypass it. The real cost of NEW is a catalog that under-reports a
        // file dar did archive — a wart, not data loss. (The exclusion wiring
        // of issue #49 closed the false-positive sources: the same effective
        // excludes reach dar, the walks and this check.)
        tracing::warn!(
            files = %new_files.join(", "),
            "NEW: file(s) on disk are absent from the snapshot manifest; dar will \
             archive content the catalog has not recorded. Re-run `snapshot create` \
             before staging if these should be catalogued (design §2.13)"
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
    // What the walk did not find, in manifest order: MISSING, or excluded
    // since the snapshot (still hashed; dar skips it).
    for (path, _, _, kind) in &all_entries {
        if *kind != FileKind::Regular {
            continue;
        }
        if let Some((size, baseline)) = regular.remove(path.as_str()) {
            check_source_size(&base.join(path), path, size)?;
            files.push(PlannedFile {
                rel_path: path.clone(),
                expected_size: size,
                baseline: baseline.cloned(),
            });
        }
    }
    Ok(SourcePlan { files })
}

/// What a file's metadata said when it was hashed — what [`recheck`]
/// compares after dar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seen {
    dev: u64,
    ino: u64,
    size: u64,
    mtime_ns: i128,
    ctime_ns: i128,
}

impl Seen {
    fn of(m: &std::fs::Metadata) -> Self {
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

/// How far ahead of dar the hasher may read, or dar of the hasher (issue
/// #364): the two reads of a file must both find it in the page cache, so
/// the source leaves its disk once. Well under the page cache of any host
/// that stages.
pub(crate) const READ_AHEAD_BYTES: u64 = 1 << 30;

/// The hasher's and dar's read positions, shared between the hasher thread
/// and the thread that consumes dar's archive, each holding the other to
/// [`ReadAhead::lead`] bytes (issue #364). Neither ever waits on a side that
/// is waiting on it: the hasher waits only while it is ahead, dar's consumer
/// only while dar is.
///
/// Positions are offsets in dar's read order — the unit's files end to end,
/// in [`plan`]'s order. With several hasher threads (issue #366) each one
/// is held to the lead by where it reads ([`ReadAhead::hasher_may_read`]),
/// and dar by the thread furthest behind ([`Lanes`]), so every byte of the
/// source is read by both within the lead of each other, whatever the
/// number of threads.
#[derive(Debug)]
pub(crate) struct ReadAhead {
    lead: u64,
    /// The low-water mark: every byte before it in dar's order is hashed
    /// or being hashed past it ([`Lanes`] keeps it).
    hashed: AtomicU64,
    dar_read: AtomicU64,
    dar_done: AtomicBool,
    hasher_done: AtomicBool,
    hasher_failed: AtomicBool,
    stop: AtomicBool,
    /// dar's process id, once it runs (0 before): the hasher reads dar's
    /// own read count itself while it waits, so a file dar reads but barely
    /// writes out (zeros it stores as holes, data that compresses well)
    /// does not leave the hasher waiting on a stale position.
    dar_pid: AtomicU64,
}

impl ReadAhead {
    pub(crate) fn new(lead: u64) -> Self {
        Self {
            lead,
            hashed: AtomicU64::new(0),
            dar_read: AtomicU64::new(0),
            dar_done: AtomicBool::new(false),
            hasher_done: AtomicBool::new(false),
            hasher_failed: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            dar_pid: AtomicU64::new(0),
        }
    }

    /// dar is running as `pid`.
    pub(crate) fn dar_started(&self, pid: u32) {
        self.dar_pid.store(u64::from(pid), Ordering::Release);
    }

    /// No dar to keep pace with: the hasher reads freely.
    fn alone() -> Self {
        let ahead = Self::new(u64::MAX);
        ahead.dar_done.store(true, Ordering::Relaxed);
        ahead
    }

    /// Whether the hasher refused the stage or failed; its error is what
    /// [`hash_files`] returns.
    pub(crate) fn hasher_failed(&self) -> bool {
        self.hasher_failed.load(Ordering::Acquire)
    }

    /// dar has read `bytes` of the source. Waits while that is more than
    /// the lead ahead of the hasher (dar, its pipe full, waits too), and
    /// returns early on a stop or a failed hasher.
    pub(crate) fn dar_has_read(&self, bytes: u64) {
        self.dar_read.fetch_max(bytes, Ordering::AcqRel);
        while bytes
            > self
                .hashed
                .load(Ordering::Acquire)
                .saturating_add(self.lead)
            && !self.hasher_done.load(Ordering::Acquire)
            && !self.hasher_failed()
            && !self.stop.load(Ordering::Acquire)
            && !crate::signal::is_interrupted()
        {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// dar has read everything it will: the hasher need not wait for it.
    pub(crate) fn dar_finished(&self) {
        self.dar_done.store(true, Ordering::Release);
    }

    /// Stop the hasher at its next check.
    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// Wait while `at` — where a hasher thread is about to read, in dar's
    /// order — is more than the lead ahead of dar, or more than the lead
    /// past the hasher thread furthest behind (the low-water mark), so the
    /// source held between the two reads is one lead at any thread count.
    /// One thread is always at the low-water mark itself. False if the
    /// hasher was told to stop meanwhile.
    fn hasher_may_read(&self, at: u64) -> bool {
        loop {
            if self.stopped() {
                return false;
            }
            let within =
                |pos: &AtomicU64| at <= pos.load(Ordering::Acquire).saturating_add(self.lead);
            if self.dar_done.load(Ordering::Acquire)
                || (within(&self.dar_read) && within(&self.hashed))
            {
                return true;
            }
            let pid = self.dar_pid.load(Ordering::Acquire);
            if let Some(read) = u32::try_from(pid)
                .ok()
                .filter(|&p| p != 0)
                .and_then(crate::dar::create::bytes_read)
            {
                self.dar_read.fetch_max(read, Ordering::AcqRel);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Where each hasher thread reads, in dar's order (issue #366), and the
/// low-water mark they publish to [`ReadAhead`] for dar to be paced by.
///
/// Files are handed out in dar's order ([`Lanes::take`]); a file handed out
/// is read from its offset onward until it is finished. The low-water mark
/// is the least of the positions being read and the offset of the next file
/// not yet handed out: every byte before it is hashed. Pacing dar by the
/// threads' total instead let one thread fall behind dar on a large file
/// without bound while the others ran ahead on later ones.
#[derive(Debug)]
pub(crate) struct Lanes {
    /// Each file's offset in dar's order, and the unit's total at the end.
    starts: Vec<u64>,
    state: std::sync::Mutex<LaneState>,
}

#[derive(Debug, Default)]
struct LaneState {
    /// The next file to hand out.
    next: usize,
    /// Each file being read: where its thread reads now.
    reading: std::collections::BTreeMap<usize, u64>,
}

impl Lanes {
    /// Lanes over files of `sizes` bytes, in dar's order.
    pub(crate) fn new(sizes: &[u64]) -> Self {
        let mut starts = Vec::with_capacity(sizes.len() + 1);
        let mut at = 0u64;
        for &size in sizes {
            starts.push(at);
            at = at.saturating_add(size);
        }
        starts.push(at);
        Self {
            starts,
            state: std::sync::Mutex::new(LaneState::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LaneState> {
        // A thread that panicked mid-update leaves a state that is still
        // whole: every write here is a single insert, update or remove.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn publish(&self, state: &LaneState, ahead: &ReadAhead) {
        let next = self.starts[state.next.min(self.starts.len() - 1)];
        let low = state.reading.values().copied().fold(next, u64::min);
        ahead.hashed.store(low, Ordering::Release);
    }

    /// The next file in dar's order and its offset, now being read; `None`
    /// once every file is handed out.
    pub(crate) fn take(&self, ahead: &ReadAhead) -> Option<(usize, u64)> {
        let mut state = self.lock();
        let i = state.next;
        if i + 1 >= self.starts.len() {
            return None;
        }
        state.next += 1;
        state.reading.insert(i, self.starts[i]);
        self.publish(&state, ahead);
        Some((i, self.starts[i]))
    }

    /// File `i`'s thread has read up to `at`.
    pub(crate) fn advance(&self, ahead: &ReadAhead, i: usize, at: u64) {
        let mut state = self.lock();
        if let Some(pos) = state.reading.get_mut(&i) {
            *pos = at;
        }
        self.publish(&state, ahead);
    }

    /// File `i` is read to its end, or will not be read further.
    pub(crate) fn finish(&self, ahead: &ReadAhead, i: usize) {
        let mut state = self.lock();
        state.reading.remove(&i);
        self.publish(&state, ahead);
    }
}

/// One file's place among the [`Lanes`]: what [`hash_one`] reports its
/// progress through.
struct Lane<'a> {
    lanes: &'a Lanes,
    index: usize,
    start: u64,
}

/// How many source files the hasher reads at once when nothing says
/// otherwise (issue #366): `[staging] hash_threads`' default, before the
/// clamp to the cores the host has ([`hash_threads`]). 1, the
/// one-file-at-a-time pass, until home2's measurements at 1/2/4/8 threads
/// are in (ADR-0012, 2026-10-07 amendment, item 12).
pub const DEFAULT_HASH_THREADS: usize = 1;

/// The most `[staging] hash_threads` may say (issue #366).
pub const MAX_HASH_THREADS: usize = 64;

/// The number of hasher threads to run for `requested`: at least 1, and no
/// more than the host's cores (issue #366: sha256 is CPU-bound, so threads
/// past the cores only add contention).
pub(crate) fn hash_threads(requested: usize) -> usize {
    let cores = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    requested.clamp(1, MAX_HASH_THREADS).min(cores).max(1)
}

/// Hash every file of `plan` under `base`, keeping pace with dar through
/// `ahead` (issue #364: the source is read once from disk — dar and this
/// hasher each read a file within `ahead`'s lead of the other, so the
/// second read comes from the page cache).
///
/// `threads` files are hashed at once (issue #366: one core's sha256 was
/// the ceiling of the whole stage). Files are handed out in `plan`'s order —
/// dar's read order — and each thread reads only within the lead of dar at
/// its own place in that order, while dar is held to the thread furthest
/// behind ([`Lanes`]), so the single read holds at any thread count. Several
/// files are read at once only where several fit in the lead: many small
/// files, not a few large ones. The results are collected in `plan`'s
/// order, so every recorded sha256 and every refusal is the serial pass's.
/// The refusal reported is the first in that order, never whichever thread
/// happened to fail first: a failure at file `i` stops files after `i` from
/// being started, while every file before it is still read to the end, so a
/// refusal earlier in the order still wins. One thread is the serial pass
/// exactly, on the calling thread.
///
/// For each file: stat, open, `fstat`, hash its planned size and no more,
/// `fstat` again. A file at another size than the snapshot's is DIRTY; one
/// whose size, times or inode moved while it was read (grown past its
/// planned size too) is DIRTY; a file whose hash differs from
/// its baseline at the same size is BITROT suspected. What it saw is kept
/// for [`recheck`], which ties this read to dar's.
///
/// On a refusal or an error it marks `ahead` failed, so dar's consumer stops
/// dar, and returns the error.
pub(crate) fn hash_files(
    base: &Path,
    plan: &SourcePlan,
    ahead: &ReadAhead,
    threads: usize,
) -> Result<Hashed> {
    let result = hash_all(base, plan, ahead, threads);
    match &result {
        Ok(_) => ahead.hasher_done.store(true, Ordering::Release),
        Err(_) => ahead.hasher_failed.store(true, Ordering::Release),
    }
    result
}

fn hash_all(base: &Path, plan: &SourcePlan, ahead: &ReadAhead, threads: usize) -> Result<Hashed> {
    let total_files = plan.files.len();
    let threads = threads.clamp(1, total_files.max(1));
    info!(
        files = total_files,
        total_mb = plan.total_bytes() / (1024 * 1024),
        threads,
        "hashing source files"
    );
    let sizes: Vec<u64> = plan
        .files
        .iter()
        .map(|f| f.expected_size.max(0) as u64)
        .collect();
    let lanes = Lanes::new(&sizes);
    let done = AtomicUsize::new(0);
    // The lowest index that failed so far; no file after it is started.
    let first_failure = AtomicUsize::new(usize::MAX);
    let slots: Vec<std::sync::OnceLock<Result<HashedFile>>> = (0..total_files)
        .map(|_| std::sync::OnceLock::new())
        .collect();
    let worker = || loop {
        let Some((i, start)) = lanes.take(ahead) else {
            break;
        };
        if i > first_failure.load(Ordering::Acquire) {
            lanes.finish(ahead, i);
            break;
        }
        let lane = Lane {
            lanes: &lanes,
            index: i,
            start,
        };
        let planned = &plan.files[i];
        let result = (|| {
            // Issue #404: hours on a large unit, so a signal stops it
            // between files.
            crate::signal::check(|| {
                format!(
                    "stopped while checking the source ({} of {total_files} files hashed)",
                    done.load(Ordering::Acquire)
                )
            })?;
            // Not opened until its first byte is inside the lead.
            if !ahead.hasher_may_read(start) {
                return Err(TapectlError::Other("the source check was stopped".into()));
            }
            let full_path = base.join(&planned.rel_path);
            #[cfg(test)]
            let _active = hash_hook::Active::enter(&full_path);
            let hashed = hash_one(&full_path, planned, ahead, &lane)?;
            #[cfg(test)]
            hash_hook::fire(&full_path);
            Ok(hashed)
        })();
        lanes.finish(ahead, i);
        if result.is_err() {
            first_failure.fetch_min(i, Ordering::AcqRel);
        } else {
            done.fetch_add(1, Ordering::AcqRel);
        }
        let _ = slots[i].set(result);
    };
    std::thread::scope(|s| {
        for n in 1..threads {
            let spawned = std::thread::Builder::new()
                .name(format!("tapectl-source-hash-{n}"))
                .spawn_scoped(s, worker);
            if spawned.is_err() {
                // Fewer threads is slower, never wrong: the calling thread
                // below hashes whatever the others do not.
                break;
            }
        }
        worker();
    });

    let mut files = Vec::with_capacity(total_files);
    for slot in slots {
        match slot.into_inner() {
            Some(Ok(hashed)) => files.push(hashed),
            Some(Err(e)) => return Err(e),
            // Not started: an earlier file failed, and is reported first.
            None => break,
        }
    }
    if files.len() != total_files {
        return Err(TapectlError::Other(
            "the source check stopped before every file was hashed".into(),
        ));
    }
    info!(files = files.len(), "source files hashed");
    Ok(Hashed { files })
}

/// The DIRTY refusal for a file that changed while staging read it.
fn changed_while_staging(rel_path: &str, what: &str) -> TapectlError {
    TapectlError::Other(format!(
        "DIRTY: source file changed while staging read it: {rel_path} ({what}). Its \
         recorded sha256 would not be of the bytes dar archived, so nothing was staged. \
         Stage again once the source is quiet; if the change is real, take a new \
         snapshot (`tapectl snapshot create <unit>`) and stage that."
    ))
}

fn now_ns() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

/// Hash one planned file (see [`hash_files`]).
fn hash_one(
    full_path: &Path,
    planned: &PlannedFile,
    ahead: &ReadAhead,
    lane: &Lane,
) -> Result<HashedFile> {
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
    // Read no further than the planned size: a lane never passes its own
    // file's end in dar's order. Read to EOF, a file growing while it was
    // hashed ran its lane past the next file's offset, which the low-water
    // mark never passes, and the hasher and dar then waited on each other
    // for good. Growth is refused below: the second `fstat` sees the new
    // size and change time.
    let mut reader = HashingReader::new(file.take(expected_size as u64));
    let mut buf = vec![0u8; VALIDATE_STREAM_BUFFER];
    let mut streamed: i64 = 0;
    loop {
        // Paced per buffer, not per file (issue #364): a unit is often one
        // large file, and per-file pacing would let the hasher read all of
        // it alone and dar then read it again from disk.
        let at = lane.start.saturating_add(streamed as u64);
        if !ahead.hasher_may_read(at) {
            return Err(TapectlError::Other("the source check was stopped".into()));
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        streamed += n as i64;
        lane.lanes.advance(
            ahead,
            lane.index,
            lane.start.saturating_add(streamed as u64),
        );
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

/// [`hash_files`] on its own thread, beside dar: what `stage create` runs
/// (issue #364). Dropping it unfinished stops the hasher and waits for it,
/// so no thread outlives a failed stage.
pub(crate) struct ConcurrentHash {
    ahead: std::sync::Arc<ReadAhead>,
    handle: Option<std::thread::JoinHandle<Result<Hashed>>>,
}

impl ConcurrentHash {
    /// Start hashing `plan`'s files under `base`, `threads` at a time.
    pub(crate) fn spawn(
        base: std::path::PathBuf,
        plan: SourcePlan,
        threads: usize,
    ) -> Result<Self> {
        let ahead = std::sync::Arc::new(ReadAhead::new(READ_AHEAD_BYTES));
        let shared = ahead.clone();
        let handle = std::thread::Builder::new()
            .name("tapectl-source-hash".into())
            .spawn(move || hash_files(&base, &plan, &shared, threads))?;
        Ok(Self {
            ahead,
            handle: Some(handle),
        })
    }

    /// The read positions dar's consumer reports to and is paced by.
    pub(crate) fn ahead(&self) -> &ReadAhead {
        &self.ahead
    }

    /// Wait for the hasher and take what it read, or its refusal.
    pub(crate) fn finish(mut self) -> Result<Hashed> {
        let handle = self.handle.take().expect("finished once");
        handle
            .join()
            .unwrap_or_else(|_| Err(TapectlError::Other("the source hasher panicked".into())))
    }
}

impl Drop for ConcurrentHash {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.ahead.stop();
            let _ = handle.join();
        }
    }
}

/// A file whose change time is this close to (or after) the moment its
/// hash began may have been written again within the same timestamp tick
/// without its times moving — git's "racy" case. Two seconds covers the
/// coarsest timestamps a source filesystem plausibly has.
const RACY_WINDOW_NS: i128 = 2_000_000_000;

/// After dar has read the source: every hashed file must still be the file,
/// at the size and change time, that was hashed (issue #364). A write
/// between the hash and dar's read moves the change time, which no
/// unprivileged writer can set back; dar's own `--retry-on-change 0` covers
/// a write during dar's read. A file whose change time was within
/// [`RACY_WINDOW_NS`] of its hash is hashed again, and must hash the same.
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

/// The whole source check with no dar beside it: [`plan`], [`hash_files`]
/// and [`recheck`] in turn — the archival **commitment point** (issue
/// #32/H6; design §2.13: "staging is the archival commitment point — full
/// sha256 every time"), as `stage create` runs it around dar:
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
    let hashed = hash_files(
        base,
        &plan,
        &ReadAhead::alone(),
        hash_threads(DEFAULT_HASH_THREADS),
    )?;
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
///     conflated — full dirty-detection machinery (`unit status --dirty`,
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

/// Test-only: run a callback right after a file under a given source root
/// is hashed — how a test changes a file between its hash and dar's read.
/// Keyed by source root, so tests running in parallel never see each
/// other's hooks; the hasher may run on its own thread.
#[cfg(test)]
pub(crate) mod hash_hook {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    type Hook = Box<dyn Fn(&Path) + Send>;
    static HOOKS: Mutex<Option<HashMap<PathBuf, Hook>>> = Mutex::new(None);

    /// Call `hook` with each file's path after it is hashed, for every
    /// source under `root`, until [`clear`].
    pub(crate) fn set(root: &Path, hook: impl Fn(&Path) + Send + 'static) {
        HOOKS
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(root.to_path_buf(), Box::new(hook));
    }

    pub(crate) fn clear(root: &Path) {
        if let Some(map) = HOOKS.lock().unwrap().as_mut() {
            map.remove(root);
        }
    }

    pub(crate) fn fire(path: &Path) {
        let hooks = HOOKS.lock().unwrap();
        if let Some(map) = hooks.as_ref() {
            for (root, hook) in map {
                if path.starts_with(root) {
                    hook(path);
                }
            }
        }
    }

    /// `(files being hashed now, the most at once)` per tracked source root
    /// (issue #366): how a test sees that files are hashed in parallel.
    static ACTIVE: Mutex<Option<HashMap<PathBuf, (usize, usize)>>> = Mutex::new(None);

    /// Count how many files under `root` are hashed at once, until
    /// [`untrack`].
    pub(crate) fn track(root: &Path) {
        ACTIVE
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(root.to_path_buf(), (0, 0));
    }

    /// The most files under `root` that were hashed at once; stops tracking.
    pub(crate) fn untrack(root: &Path) -> usize {
        ACTIVE
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|m| m.remove(root))
            .map_or(0, |(_, most)| most)
    }

    /// One file being hashed, for the length of the guard.
    pub(crate) struct Active(Option<PathBuf>);

    impl Active {
        pub(crate) fn enter(path: &Path) -> Self {
            let mut active = ACTIVE.lock().unwrap();
            let root = active.as_mut().and_then(|m| {
                m.iter_mut()
                    .find(|(root, _)| path.starts_with(root))
                    .map(|(root, (now, most))| {
                        *now += 1;
                        *most = (*most).max(*now);
                        root.clone()
                    })
            });
            Self(root)
        }
    }

    impl Drop for Active {
        fn drop(&mut self) {
            if let Some(root) = self.0.take() {
                if let Some((now, _)) = ACTIVE
                    .lock()
                    .unwrap()
                    .as_mut()
                    .and_then(|m| m.get_mut(&root))
                {
                    *now -= 1;
                }
            }
        }
    }
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

    /// Issue #364: dar's consumer waits while dar is more than the lead
    /// ahead of the hasher, and goes on once the hasher catches up.
    #[test]
    fn dar_waits_while_it_is_more_than_the_lead_ahead_of_the_hasher() {
        let ahead = std::sync::Arc::new(ReadAhead::new(10));
        let shared = ahead.clone();
        let dar = std::thread::spawn(move || shared.dar_has_read(100));
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !dar.is_finished(),
            "90 bytes ahead with a lead of 10: waits"
        );
        ahead.hashed.store(95, Ordering::Release);
        dar.join().unwrap();
    }

    /// The other side: the hasher waits while it is more than the lead ahead
    /// of dar, and reads freely once dar has finished.
    #[test]
    fn the_hasher_waits_while_it_is_more_than_the_lead_ahead_of_dar() {
        let ahead = std::sync::Arc::new(ReadAhead::new(10));
        let shared = ahead.clone();
        let hasher = std::thread::spawn(move || shared.hasher_may_read(50));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!hasher.is_finished(), "40 bytes ahead of dar: waits");
        ahead.dar_finished();
        assert!(hasher.join().unwrap(), "dar done: read on");

        // Told to stop while waiting, it says so.
        let ahead = std::sync::Arc::new(ReadAhead::new(10));
        let shared = ahead.clone();
        let hasher = std::thread::spawn(move || shared.hasher_may_read(50));
        ahead.stop();
        assert!(!hasher.join().unwrap());
    }

    /// Issue #364: the lead holds inside a file, not only between files. A
    /// unit is often one large file; paced per file, the hasher would read
    /// all of it alone while dar waited, and dar then read it again from
    /// disk once it outgrew the page cache — the double read this exists to
    /// remove.
    #[test]
    fn the_hasher_keeps_its_lead_inside_one_large_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("big.bin"), vec![7u8; 1 << 20]).unwrap();
        let (conn, sid) = setup_conn_with_snapshot(&[("big.bin", 1 << 20, None)]);
        let base = tmp.path().to_path_buf();
        let plan = plan(&conn, sid, base.to_str().unwrap(), &[]).unwrap();
        let ahead = std::sync::Arc::new(ReadAhead::new(64 << 10));
        let shared = ahead.clone();
        let hasher = std::thread::spawn(move || hash_files(&base, &plan, &shared, 1).map(|_| ()));
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !hasher.is_finished(),
            "dar has read nothing: the hasher may not read the whole file alone"
        );
        let hashed = ahead.hashed.load(Ordering::Acquire);
        assert!(
            hashed <= (64 << 10) + VALIDATE_STREAM_BUFFER as u64,
            "the hasher stays within one buffer of its lead: {hashed}"
        );
        ahead.dar_finished();
        hasher.join().unwrap().unwrap();
    }

    /// A source file that grows past the lead after the hasher opened it is
    /// refused as changed while it was hashed, never a stage that hangs.
    /// Read to EOF, the grown file's lane ran past the next file's offset,
    /// the low-water mark stayed at that offset, and the hasher and dar
    /// then each waited on the other for good — with one thread too.
    #[test]
    fn a_file_growing_past_the_lead_while_hashed_is_refused_not_a_hang() {
        const LEAD: u64 = 64 << 10;
        const PLANNED: usize = 256 << 10;
        const GROWTH: usize = 512 << 10;
        for threads in [1, 4] {
            let tmp = TempDir::new().unwrap();
            let mut rows = vec![("a0.bin".to_string(), PLANNED as i64)];
            std::fs::write(tmp.path().join("a0.bin"), vec![1u8; PLANNED]).unwrap();
            for i in 1..4 {
                let name = format!("a{i}.bin");
                std::fs::write(tmp.path().join(&name), vec![i as u8; 32 << 10]).unwrap();
                rows.push((name, 32 << 10));
            }
            let borrowed: Vec<(&str, i64, Option<&str>)> =
                rows.iter().map(|(n, s)| (n.as_str(), *s, None)).collect();
            let (conn, sid) = setup_conn_with_snapshot(&borrowed);
            let base = tmp.path().to_path_buf();
            let plan = plan(&conn, sid, base.to_str().unwrap(), &[]).unwrap();
            assert_eq!(plan.files[0].rel_path, "a0.bin", "fixture: dar's first");
            let total_after: u64 = plan.total_bytes() + GROWTH as u64;
            let ahead = std::sync::Arc::new(ReadAhead::new(LEAD));

            let shared = ahead.clone();
            let root = base.clone();
            let hasher = std::thread::spawn(move || hash_files(&root, &plan, &shared, threads));
            // Opened and read from: a0.bin is open, its size checked.
            let t0 = std::time::Instant::now();
            while ahead.hashed.load(Ordering::Acquire) == 0 {
                assert!(t0.elapsed() < Duration::from_secs(10), "hasher never read");
                std::thread::sleep(Duration::from_millis(1));
            }
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(base.join("a0.bin"))
                .unwrap();
            f.write_all(&vec![9u8; GROWTH]).unwrap();
            drop(f);

            // dar reads the grown source, as it would.
            let shared = ahead.clone();
            let dar = std::thread::spawn(move || {
                let mut pos = 0u64;
                while pos < total_after && !shared.hasher_failed() && !shared.stopped() {
                    pos = (pos + (16 << 10)).min(total_after);
                    shared.dar_has_read(pos);
                }
                shared.dar_finished();
            });

            let t0 = std::time::Instant::now();
            while !hasher.is_finished() && t0.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(10));
            }
            let hung = !hasher.is_finished();
            let (hashed, dar_read) = (
                ahead.hashed.load(Ordering::Acquire),
                ahead.dar_read.load(Ordering::Acquire),
            );
            // Release both sides either way, so a failure does not hang.
            ahead.stop();
            let result = hasher.join().unwrap();
            dar.join().unwrap();
            assert!(
                !hung,
                "{threads} thread(s): the hasher and dar wait on each other \
                 (hashed={hashed}, dar_read={dar_read})"
            );
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("it changed while it was hashed") && err.contains("a0.bin"),
                "{threads} thread(s): {err}"
            );
        }
    }

    /// Issue #366 with #364: several hasher threads keep the lead by where
    /// each one reads in dar's order, not by what they read between them.
    /// Four large files and four threads, dar not started: only the first
    /// file is inside the lead, so only it is opened, and nothing is read
    /// past the lead. Paced by the threads' sum, all four files were opened
    /// at once and read in step, so with dar running the first file's
    /// hasher fell behind dar by more and more of that file while the
    /// others ran ahead of it — each file read twice from disk once it
    /// outgrew the page cache.
    #[test]
    fn hasher_threads_keep_the_lead_by_their_place_in_dars_order() {
        let tmp = TempDir::new().unwrap();
        let mut rows = Vec::new();
        for i in 0..4 {
            let name = format!("big{i}.bin");
            std::fs::write(tmp.path().join(&name), vec![i as u8; 1 << 20]).unwrap();
            rows.push((name, 1i64 << 20));
        }
        let borrowed: Vec<(&str, i64, Option<&str>)> =
            rows.iter().map(|(n, s)| (n.as_str(), *s, None)).collect();
        let (conn, sid) = setup_conn_with_snapshot(&borrowed);
        let base = tmp.path().to_path_buf();
        let plan = plan(&conn, sid, base.to_str().unwrap(), &[]).unwrap();
        let ahead = std::sync::Arc::new(ReadAhead::new(64 << 10));
        let shared = ahead.clone();
        hash_hook::track(&base);
        let root = base.clone();
        let hasher = std::thread::spawn(move || hash_files(&root, &plan, &shared, 4).map(|_| ()));
        std::thread::sleep(Duration::from_millis(300));
        let most = hash_hook::untrack(&base);
        let finished = hasher.is_finished();
        let low_water = ahead.hashed.load(Ordering::Acquire);
        ahead.dar_finished();
        hasher.join().unwrap().unwrap();
        assert!(
            !finished,
            "dar has read nothing: the hasher may not run ahead"
        );
        assert_eq!(most, 1, "only the file inside the lead was opened");
        assert!(
            low_water <= (64 << 10) + VALIDATE_STREAM_BUFFER as u64,
            "nothing read past one buffer beyond the lead: {low_water}"
        );
    }

    /// The other half: dar waits on the hasher thread furthest behind in its
    /// order (issue #366), so a slow file early in the unit holds dar to the
    /// lead even while other threads have hashed far more between them.
    #[test]
    fn dar_waits_on_the_hasher_thread_furthest_behind() {
        let lanes = Lanes::new(&[1000; 4]);
        let ahead = std::sync::Arc::new(ReadAhead::new(10));
        // Files 0..3 handed out; file 0's hasher is at byte 5, the others
        // have read most of theirs.
        for _ in 0..4 {
            lanes.take(&ahead).unwrap();
        }
        lanes.advance(&ahead, 0, 5);
        lanes.advance(&ahead, 1, 1900);
        lanes.advance(&ahead, 2, 2900);
        lanes.advance(&ahead, 3, 3900);
        assert_eq!(ahead.hashed.load(Ordering::Acquire), 5);
        let shared = ahead.clone();
        let dar = std::thread::spawn(move || shared.dar_has_read(100));
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !dar.is_finished(),
            "95 bytes past the slowest hasher: waits"
        );
        lanes.advance(&ahead, 0, 95);
        dar.join().unwrap();
        // A file done leaves the low-water mark to the next one behind.
        lanes.finish(&ahead, 0);
        assert_eq!(ahead.hashed.load(Ordering::Acquire), 1900);
    }

    /// And no hasher thread reads more than the lead past the one furthest
    /// behind (issue #366), even with dar far ahead: what one stage holds
    /// between the two reads stays one lead of the source, as it was with
    /// one thread, not one lead each side of dar.
    #[test]
    fn no_hasher_thread_reads_more_than_the_lead_past_the_slowest() {
        let lanes = Lanes::new(&[1000; 2]);
        let ahead = std::sync::Arc::new(ReadAhead::new(10));
        ahead.dar_read.store(5000, Ordering::Release);
        lanes.take(&ahead).unwrap();
        lanes.advance(&ahead, 0, 5);
        let (_, start) = lanes.take(&ahead).unwrap();
        let shared = ahead.clone();
        let hasher = std::thread::spawn(move || shared.hasher_may_read(start));
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !hasher.is_finished(),
            "file 1 starts 995 bytes past the slowest thread: waits"
        );
        lanes.advance(&ahead, 0, 990);
        assert!(hasher.join().unwrap());
    }

    /// A waiting hasher reads dar's own read count, so a stretch dar reads
    /// but barely writes out (holes, compressible data) does not leave it
    /// waiting on a stale position. This process stands in for dar: it has
    /// read far more than 60 bytes.
    #[test]
    fn a_waiting_hasher_reads_how_far_dar_has_read_itself() {
        let ahead = ReadAhead::new(10);
        // The one hasher thread, at 50: the low-water mark is itself.
        ahead.hashed.store(50, Ordering::Release);
        ahead.dar_started(std::process::id());
        assert!(
            crate::dar::create::bytes_read(std::process::id()).is_some(),
            "fixture: the kernel reports a read count"
        );
        assert!(
            ahead.hasher_may_read(50),
            "the hasher goes on without a tick"
        );
    }

    /// Issue #364: a file written again after it was hashed — its change
    /// time moves — fails the recheck that ties the hash to dar's read.
    #[test]
    fn recheck_refuses_a_file_changed_after_its_hash() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        let (conn, sid) = setup_conn_with_snapshot(&[("a.txt", 5, None)]);
        let base = tmp.path();
        let plan = plan(&conn, sid, base.to_str().unwrap(), &[]).unwrap();
        let hashed = hash_files(base, &plan, &ReadAhead::alone(), 1).unwrap();
        recheck(base, &hashed).expect("unchanged: passes");

        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(base.join("a.txt"), b"HELLO").unwrap();
        let err = recheck(base, &hashed).unwrap_err().to_string();
        assert!(err.contains("DIRTY") && err.contains("a.txt"), "{err}");
    }

    /// A tree of `n` files in nested directories, sizes varied, every one
    /// in the snapshot with `baseline` (by file index) as its recorded
    /// sha256. Returns the tree, the catalog and the snapshot id.
    fn tree_of(
        n: usize,
        size: impl Fn(usize) -> usize,
        baseline: impl Fn(usize, &[u8]) -> Option<String>,
    ) -> (TempDir, Connection, i64) {
        let tmp = TempDir::new().unwrap();
        let mut rows: Vec<(String, i64, Option<String>)> = Vec::new();
        for i in 0..n {
            let rel = format!("d{}/e{}/f{i:03}.bin", i % 3, i % 5);
            let full = tmp.path().join(&rel);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            let data: Vec<u8> = (0..size(i))
                .map(|j| ((i * 31 + j * 7) % 251) as u8)
                .collect();
            std::fs::write(&full, &data).unwrap();
            rows.push((rel, data.len() as i64, baseline(i, &data)));
        }
        let borrowed: Vec<(&str, i64, Option<&str>)> = rows
            .iter()
            .map(|(p, s, b)| (p.as_str(), *s, b.as_deref()))
            .collect();
        let (conn, sid) = setup_conn_with_snapshot(&borrowed);
        (tmp, conn, sid)
    }

    fn hash_with(base: &Path, plan: &SourcePlan, threads: usize) -> Result<Vec<(String, String)>> {
        hash_files(base, plan, &ReadAhead::alone(), threads).map(|h| h.checksums())
    }

    /// Issue #366 acceptance: hashing several files at once records exactly
    /// what the serial pass records — every sha256, in dar's order — and
    /// refuses exactly what it refuses, with the same text: a file changed
    /// at its recorded size (BITROT against its baseline) and a file gone
    /// since the plan (missing).
    #[test]
    fn parallel_hashing_records_and_refuses_exactly_what_the_serial_pass_does() {
        let (tmp, conn, sid) = tree_of(60, |i| (i * 7919) % 300_000, |_, _| None);
        let base = tmp.path();
        let planned = plan(&conn, sid, base.to_str().unwrap(), &[]).unwrap();
        let serial = hash_with(base, &planned, 1).unwrap();
        assert_eq!(serial.len(), 60);
        for threads in [2, 4, 8, 64] {
            assert_eq!(
                hash_with(base, &planned, threads).unwrap(),
                serial,
                "{threads}"
            );
        }
        let walk: Vec<&str> = planned.files.iter().map(|f| f.rel_path.as_str()).collect();
        let recorded: Vec<&str> = serial.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(recorded, walk, "recorded in dar's read order");

        // A changed file (same size, other bytes, against a baseline) and a
        // missing one.
        let (tmp, conn, sid) = tree_of(
            40,
            |i| 1000 + i * 50,
            |_, data| Some(format!("{:x}", Sha256::digest(data))),
        );
        let base = tmp.path();
        let planned2 = plan(&conn, sid, base.to_str().unwrap(), &[]).unwrap();
        let changed = planned2.files[25].rel_path.clone();
        let missing = planned2.files[11].rel_path.clone();
        let len = std::fs::metadata(base.join(&changed)).unwrap().len() as usize;
        std::fs::write(base.join(&changed), vec![0xEEu8; len]).unwrap();
        let only_changed = hash_with(base, &planned2, 1).unwrap_err().to_string();
        assert!(
            only_changed.contains("BITROT suspected") || only_changed.contains("DIRTY"),
            "{only_changed}"
        );
        std::fs::remove_file(base.join(&missing)).unwrap();
        let serial = hash_with(base, &planned2, 1).unwrap_err().to_string();
        assert!(
            serial.contains("source file missing") && serial.contains(&missing),
            "the earlier file in dar's order is the one reported: {serial}"
        );
        for threads in [2, 4, 8] {
            assert_eq!(
                hash_with(base, &planned2, threads).unwrap_err().to_string(),
                serial,
                "{threads} threads"
            );
        }
    }

    /// Issue #366: files really are hashed at once — `threads` of them in
    /// flight, not one after another.
    #[test]
    fn several_files_are_hashed_at_once() {
        let (tmp, conn, sid) = tree_of(24, |_| 4096, |_, _| None);
        let base = tmp.path();
        let plan = plan(&conn, sid, base.to_str().unwrap(), &[]).unwrap();
        hash_hook::track(base);
        // Each file stays "in flight" a little after its hash, so threads
        // that run at all overlap.
        hash_hook::set(base, |_| std::thread::sleep(Duration::from_millis(15)));
        let result = hash_with(base, &plan, 4);
        hash_hook::clear(base);
        let most = hash_hook::untrack(base);
        result.unwrap();
        assert!(most >= 2, "at most {most} file(s) hashed at once");
        assert!(most <= 4, "never more than the threads: {most}");
    }

    /// Issue #366: the refusal reported is the first in dar's order, not
    /// whichever thread failed first. The early file is large and only
    /// fails once its whole hash is read (BITROT); a later file fails at
    /// once (its size moved since the plan). Threads that raced would report
    /// the later one.
    #[test]
    fn the_first_refusal_in_dar_order_is_the_one_reported() {
        let (tmp, conn, sid) = tree_of(
            30,
            |_| 2048,
            |_, data| Some(format!("{:x}", Sha256::digest(data))),
        );
        let base = tmp.path();
        // The first file in dar's order becomes large, with a baseline of
        // its own content, then rots by one byte at the end.
        let first = plan(&conn, sid, base.to_str().unwrap(), &[]).unwrap().files[0]
            .rel_path
            .clone();
        let mut bytes: Vec<u8> = (0..24u32 << 20).map(|j| (j % 253) as u8).collect();
        conn.execute(
            "UPDATE file_versions SET size_bytes = ?1, sha256 = ?2
             WHERE snapshot_id = ?3
               AND path_id = (SELECT id FROM paths
                               WHERE unit_id = (SELECT unit_id FROM snapshots WHERE id = ?3)
                                 AND path = ?4)",
            params![
                bytes.len() as i64,
                Sha256::digest(&bytes).as_slice(),
                sid,
                first
            ],
        )
        .unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        std::fs::write(base.join(&first), &bytes).unwrap();
        let planned = plan(&conn, sid, base.to_str().unwrap(), &[]).unwrap();
        assert_eq!(planned.files[0].rel_path, first);
        let later = planned.files.last().unwrap().rel_path.clone();
        std::fs::write(base.join(&later), b"grown longer").unwrap();
        let big_path = first;
        for threads in [1, 4] {
            let err = hash_with(base, &planned, threads).unwrap_err().to_string();
            assert!(
                err.contains("BITROT suspected") && err.contains(&big_path),
                "{threads} threads: {err}"
            );
        }
    }

    #[test]
    fn hash_threads_are_at_least_one_and_at_most_the_cores() {
        let cores = std::thread::available_parallelism().unwrap().get();
        assert_eq!(hash_threads(0), 1);
        assert_eq!(hash_threads(1), 1);
        assert_eq!(hash_threads(10_000), cores.min(MAX_HASH_THREADS));
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
