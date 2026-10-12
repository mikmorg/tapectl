//! How many bytes went through sha256, and how many were read back from
//! staging: the counts `tests/pass_budget.rs` holds each operation to
//! (issue #416 item 4).
//!
//! **Test-only, by ruling** (ADR-0012, 2026-10-12 item 45): nothing in the
//! crate reads these counts and nothing records them, so they never reach
//! the catalog and need no migration. They exist so a budget test can see
//! that a write hashes every byte twice (once on the way to the medium, once
//! on the readback) and reads each staged byte once, and fail when a change
//! adds a pass, as 1.0.3's second pre-write hash did.
//!
//! **One counter for the whole process, off until a test turns it on.**
//! Production hashes on worker threads (the write pipeline's hash stage,
//! staging's hash threads, the source hasher beside dar), so a per-thread
//! count would miss most of it. A counted byte is one relaxed atomic add,
//! and only while a [`Probe`] is alive; otherwise a relaxed load of a flag
//! nobody writes. A probe holds a process-wide lock, so two budget tests in
//! one binary take turns; a test that hashes WITHOUT a probe, beside one that
//! holds it, would be counted too, which is why the budget tests live in a
//! test binary of their own.
//!
//! **The seam is a constructor.** [`Sha256`] is `sha2::Sha256` with every
//! update counted, and implements the same `Digest` traits, so a call site
//! switches by its `use` line. Staging reads are counted where the write
//! path reads a staged file: [`crate::util::DropBehind`] (the write itself)
//! and [`StagedRead`] (`--prewrite-hash`'s pass before it).

use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use sha2::digest::{consts::U32, FixedOutput, HashMarker, Output, OutputSizeUser, Reset, Update};

static COUNTING: AtomicBool = AtomicBool::new(false);
static HASHED: AtomicU64 = AtomicU64::new(0);
static STAGING_READ: AtomicU64 = AtomicU64::new(0);
static PROBE: Mutex<()> = Mutex::new(());

fn add(counter: &AtomicU64, n: usize) {
    if COUNTING.load(Ordering::Relaxed) {
        counter.fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// Note `n` bytes read from a staged file.
pub fn note_staging_read(n: usize) {
    add(&STAGING_READ, n);
}

/// `sha2::Sha256`, with every byte it hashes counted. A drop-in: the
/// `Digest` methods (`new`, `update`, `finalize`, `digest`) come from the
/// same blanket impl `sha2::Sha256` gets them from.
#[derive(Clone, Default)]
pub struct Sha256(sha2::Sha256);

impl HashMarker for Sha256 {}

impl OutputSizeUser for Sha256 {
    type OutputSize = U32;
}

impl Update for Sha256 {
    fn update(&mut self, data: &[u8]) {
        add(&HASHED, data.len());
        Update::update(&mut self.0, data);
    }
}

impl FixedOutput for Sha256 {
    fn finalize_into(self, out: &mut Output<Self>) {
        FixedOutput::finalize_into(self.0, out);
    }
}

impl Reset for Sha256 {
    fn reset(&mut self) {
        Reset::reset(&mut self.0);
    }
}

/// A staged file opened for a read that is not the write itself
/// (`--prewrite-hash`'s pass), its bytes counted as staging reads.
pub struct StagedRead<R>(pub R);

impl StagedRead<std::fs::File> {
    pub fn open(path: &std::path::Path) -> io::Result<Self> {
        std::fs::File::open(path).map(StagedRead)
    }
}

impl<R: Read> Read for StagedRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.0.read(buf)?;
        note_staging_read(n);
        Ok(n)
    }
}

/// What was counted between two points.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Passes {
    /// Bytes put through sha256.
    pub hashed: u64,
    /// Bytes read from staged files.
    pub staging_read: u64,
}

impl std::ops::Sub for Passes {
    type Output = Passes;
    fn sub(self, earlier: Passes) -> Passes {
        Passes {
            hashed: self.hashed - earlier.hashed,
            staging_read: self.staging_read - earlier.staging_read,
        }
    }
}

/// Counting turned on, for as long as this lives. Holds the process-wide
/// probe lock, so probes never overlap.
pub struct Probe {
    _turn: MutexGuard<'static, ()>,
}

/// Turn counting on, waiting for any other probe to finish first.
pub fn probe() -> Probe {
    let turn = PROBE.lock().unwrap_or_else(|e| e.into_inner());
    COUNTING.store(true, Ordering::SeqCst);
    Probe { _turn: turn }
}

impl Probe {
    /// Everything counted since the process started counting: difference
    /// two readings to get what one stretch of work cost.
    pub fn read(&self) -> Passes {
        Passes {
            hashed: HASHED.load(Ordering::SeqCst),
            staging_read: STAGING_READ.load(Ordering::SeqCst),
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        COUNTING.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::Sha256;
    use sha2::Digest;

    /// The counted hasher is sha256: same digest as `sha2`'s, through
    /// `new`/`update`/`finalize`, `digest`, and a clone mid-stream.
    #[test]
    fn the_counted_hasher_is_sha256() {
        let data = b"tapectl pass count";
        let want = sha2::Sha256::digest(data);
        assert_eq!(Sha256::digest(data), want);
        let mut h = Sha256::new();
        h.update(&data[..5]);
        let mid = h.clone();
        h.update(&data[5..]);
        assert_eq!(h.finalize(), want);
        let mut m = mid;
        m.update(&data[5..]);
        assert_eq!(m.finalize(), want);
    }
}
