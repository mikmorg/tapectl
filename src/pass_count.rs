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
//! **The seam is a constructor.** [`Sha256`] is a SHA-256 with every update
//! counted, and implements `sha2`'s `Digest` traits, so a call site switches
//! by its `use` line. Staging reads are counted where the write path reads a
//! staged file: [`crate::util::DropBehind`] (the write itself) and
//! [`StagedRead`] (`--prewrite-hash`'s pass before it).
//!
//! **Inside it is `ring`'s SHA-256** (issue #488, the write-shaping note's
//! §5.9, approved 2026-10-20). It is OpenSSL's SSSE3 code: on vm-desk1, one
//! core, release, 512 KiB updates, 216 MB/s against `sha2`'s `asm` 163.6
//! (`docs/research/2026-10-20-write-shaping.md` §5.9). On a CPU without
//! SHA-NI (home2's Westmere) the hash had no headroom over the drive (§1); it
//! now has about 18%. The public surface did not change: the `Digest` traits
//! still come from `sha2`, every byte is still counted once in
//! [`Update::update`], whatever does the hashing, and the digest is the same
//! (`the_counted_hasher_is_sha256_at_every_boundary`).

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

/// SHA-256, with every byte it hashes counted. A drop-in for
/// `sha2::Sha256`: the `Digest` methods (`new`, `update`, `finalize`,
/// `digest`) come from the same blanket impl `sha2::Sha256` gets them from.
/// The hashing itself is `ring`'s (see the module documentation).
#[derive(Clone)]
pub struct Sha256(Inner);

/// What does the hashing. One variant in a production build; a `research`
/// build can also run `sha2`'s (`TAPECTL_EXP_HASH=sha2`, `crate::research`).
#[derive(Clone)]
enum Inner {
    Ring(ring::digest::Context),
    #[cfg(feature = "research")]
    Sha2(sha2::Sha256),
}

impl Inner {
    fn fresh() -> Inner {
        #[cfg(feature = "research")]
        if crate::research::knobs().hash == crate::research::HashImpl::Sha2 {
            return Inner::Sha2(sha2::Sha256::default());
        }
        Inner::Ring(ring::digest::Context::new(&ring::digest::SHA256))
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Sha256(Inner::fresh())
    }
}

impl HashMarker for Sha256 {}

impl OutputSizeUser for Sha256 {
    type OutputSize = U32;
}

impl Update for Sha256 {
    fn update(&mut self, data: &[u8]) {
        // Counted here, once, whichever inner hashes it.
        add(&HASHED, data.len());
        match &mut self.0 {
            Inner::Ring(c) => c.update(data),
            #[cfg(feature = "research")]
            Inner::Sha2(h) => Update::update(h, data),
        }
    }
}

impl FixedOutput for Sha256 {
    fn finalize_into(self, out: &mut Output<Self>) {
        match self.0 {
            Inner::Ring(c) => out.copy_from_slice(c.finish().as_ref()),
            #[cfg(feature = "research")]
            Inner::Sha2(h) => FixedOutput::finalize_into(h, out),
        }
    }
}

impl Reset for Sha256 {
    fn reset(&mut self) {
        // `ring`'s context has no reset of its own: a fresh one is the same.
        self.0 = Inner::fresh();
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

    /// Bytes that differ along the stream, so a dropped, repeated or
    /// reordered piece cannot hash the same.
    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 + i / 509) as u8).collect()
    }

    /// Issue #488: `ring` inside gives `sha2`'s digest on every length that
    /// matters to a block hash — empty, one byte, each side of the 64-byte
    /// block and of the 55/56-byte padding edge, and multi-MiB — fed in one
    /// update and in odd-sized pieces that straddle every block boundary.
    #[test]
    fn the_counted_hasher_is_sha256_at_every_boundary() {
        let lens = [
            0usize,
            1,
            55,
            56,
            63,
            64,
            65,
            127,
            128,
            129,
            4095,
            4096,
            4097,
            3 * 1024 * 1024 + 7,
            8 * 1024 * 1024,
        ];
        for len in lens {
            let data = pattern(len);
            let want = sha2::Sha256::digest(&data);
            assert_eq!(Sha256::digest(&data), want, "one update, {len} bytes");
            for piece in [1usize, 3, 7, 63, 65, 4093, 524_287] {
                if piece > len.max(1) * 2 {
                    continue;
                }
                let mut h = Sha256::new();
                for chunk in data.chunks(piece) {
                    h.update(chunk);
                }
                assert_eq!(h.finalize(), want, "{len} bytes in pieces of {piece}");
            }
        }
    }

    /// A `research` build's `TAPECTL_EXP_HASH=sha2` runs `sha2` behind the
    /// same counted type: the same digest, through updates, a clone and a
    /// reset.
    #[cfg(feature = "research")]
    #[test]
    fn the_research_sha2_inner_is_the_same_hash() {
        use sha2::digest::Reset;
        let data = pattern(1_000_003);
        let mut h = Sha256(super::Inner::Sha2(sha2::Sha256::default()));
        for chunk in data.chunks(4093) {
            h.update(chunk);
        }
        let copy = h.clone();
        assert_eq!(h.finalize(), sha2::Sha256::digest(&data));
        assert_eq!(copy.finalize(), sha2::Sha256::digest(&data));
        let mut h = Sha256(super::Inner::Sha2(sha2::Sha256::default()));
        h.update(b"gone");
        Reset::reset(&mut h);
        h.update(b"kept");
        assert_eq!(h.finalize(), sha2::Sha256::digest(b"kept"));
    }

    /// `Reset` gives a fresh hash: what was fed before it is gone.
    #[test]
    fn a_reset_hasher_starts_over() {
        use sha2::digest::Reset;
        let mut h = Sha256::new();
        h.update(b"forgotten");
        Reset::reset(&mut h);
        h.update(b"kept");
        assert_eq!(h.finalize(), sha2::Sha256::digest(b"kept"));
    }

    /// Issue #488's one throughput figure: the counted hasher against plain
    /// `sha2::Sha256` (what it wrapped before) over the same 256 MiB, in
    /// 512 KiB updates as the write pipeline feeds it. A DEBUG build, so
    /// the absolute figures are not production's (the hash loops are
    /// assembly in both crates, the glue is unoptimised); the release
    /// figures are the note's §5.9. Ignored: it is a measurement, not a
    /// check.
    ///
    /// `cargo test --lib pass_count::tests::throughput -- --ignored --nocapture`
    #[test]
    #[ignore = "a measurement: run by hand with --ignored --nocapture"]
    fn throughput_ring_against_sha2() {
        const LEN: usize = 256 * 1024 * 1024;
        const UPDATE: usize = 512 * 1024;
        let data = pattern(LEN);
        let time = |f: &dyn Fn() -> Vec<u8>| {
            let mut best = std::time::Duration::MAX;
            let mut out = Vec::new();
            for _ in 0..3 {
                let started = std::time::Instant::now();
                out = f();
                best = best.min(started.elapsed());
            }
            (out, LEN as f64 / 1e6 / best.as_secs_f64())
        };
        let (ours, ours_mbs) = time(&|| {
            let mut h = Sha256::new();
            for chunk in data.chunks(UPDATE) {
                h.update(chunk);
            }
            h.finalize().to_vec()
        });
        let (old, old_mbs) = time(&|| {
            let mut h = sha2::Sha256::new();
            for chunk in data.chunks(UPDATE) {
                h.update(chunk);
            }
            h.finalize().to_vec()
        });
        assert_eq!(ours, old);
        eprintln!(
            "sha256 over 256 MiB in 512 KiB updates, debug build, best of 3: \
             pass_count::Sha256 (ring) {ours_mbs:.1} MB/s, sha2::Sha256 (asm) {old_mbs:.1} MB/s, \
             ratio {:.2}",
            ours_mbs / old_mbs
        );
    }
}
