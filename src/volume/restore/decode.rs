//! A compressed slice's frame, decoded as far as it will go
//! (`volume-format-v3.md` §3.5; ADR-0012 items 284, 286; invariants 6 and
//! 11).
//!
//! A compressed staging stores each slice's cut as one complete frame of its
//! algorithm (gzip, xz or zstd), and age-encrypts the frame. What the supply
//! hands the reader is the frame. This module gives back the cut: the same
//! libraries staging packs with ([`crate::staging::pack`]), read to their end
//! or to the point they stop, so that what a damaged or truncated frame DOES
//! decode is kept. In a damaged frame nothing after the bad bytes decodes
//! (item 284), but the bytes before them are the cut's own bytes and are
//! still used.
//!
//! **A decoder's own verdict is never the verdict** (invariant 6): BusyBox
//! `unxz` exits 0 after writing wrong bytes. Whatever this module returns is
//! judged by the caller against the slice's `size_bytes` and `sha256_plain`
//! and, member by member, against the file table's hashes.

use std::io::Read;

use crate::staging::pack::Algorithm;

/// What decoding gave.
#[derive(Debug)]
pub(super) struct Decoded {
    /// Every byte the decoder produced, in order. When `complete` is false
    /// these are the bytes before the point where the frame stopped
    /// decoding: a prefix of the cut, never more than the cut.
    pub bytes: Vec<u8>,
    /// The frame decoded to its end without a complaint from the decoder.
    /// Not a verdict by itself.
    pub complete: bool,
    /// Set when the buffer for the cut could not be allocated (a recorded
    /// size no host could hold: a corrupt catalog row). Nothing was decoded;
    /// the slice is bad, and this says why.
    pub unallocatable: Option<String>,
}

/// Read `decoder` to its end, handing what it produces to `sink` and keeping
/// at most `cap` bytes. `true` when it ended cleanly inside the cap.
fn drain(mut decoder: impl Read, cap: usize, mut sink: impl FnMut(&[u8])) -> bool {
    let mut buf = vec![0u8; 64 * 1024];
    let mut kept = 0usize;
    loop {
        match decoder.read(&mut buf) {
            Ok(0) => return true,
            Ok(n) => {
                // A frame that decodes to more than the slice's recorded
                // size is not that slice's frame (and may be a bomb).
                let room = cap.saturating_sub(kept);
                sink(&buf[..n.min(room)]);
                kept += n.min(room);
                if n > room {
                    return false;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
}

/// Decode `frame` as one frame of `algorithm` through `sink`, keeping at most
/// `cap` bytes. `true` when it ended cleanly inside the cap.
fn run(algorithm: Algorithm, frame: &[u8], cap: usize, sink: impl FnMut(&[u8])) -> bool {
    match algorithm {
        Algorithm::Gzip => drain(flate2::read::GzDecoder::new(frame), cap, sink),
        Algorithm::Xz => drain(liblzma::read::XzDecoder::new(frame), cap, sink),
        Algorithm::Zstd => match zstd::stream::read::Decoder::new(frame) {
            Ok(d) => drain(d.single_frame(), cap, sink),
            Err(_) => false,
        },
    }
}

/// Decode `frame` as one frame of `algorithm`, keeping at most `cap` decoded
/// bytes. The bytes produced are returned whether or not the frame decoded
/// to its end: a frame that stops (a hole in it, a bad chunk, a cut-short
/// copy) gives the cut's bytes up to there.
///
/// The buffer is made once, at `cap` (the slice's recorded size, which a
/// whole frame decodes to exactly), never grown by doubling (#571, ADR-0012
/// item 298); a `cap` that cannot be allocated is reported in
/// [`Decoded::unallocatable`] and aborts nothing.
pub(super) fn decode(algorithm: Algorithm, frame: &[u8], cap: u64) -> Decoded {
    let cap = usize::try_from(cap).unwrap_or(usize::MAX);
    let mut bytes = Vec::new();
    if let Err(e) = bytes.try_reserve_exact(cap) {
        return Decoded {
            bytes: Vec::new(),
            complete: false,
            unallocatable: Some(format!(
                "the {cap} bytes the catalog records for it cannot be held in memory ({e})"
            )),
        };
    }
    let complete = run(algorithm, frame, cap, |b| bytes.extend_from_slice(b));
    Decoded {
        bytes,
        complete,
        unallocatable: None,
    }
}

/// sha256 of what `frame` decodes to, taken as the decoder produces it (a
/// 64 KiB buffer, never the cut whole), when it decodes to its end inside
/// `cap` and to exactly `cap` bytes; `None` otherwise. The same bytes, the
/// same completeness and length conditions as hashing [`decode`]'s output.
pub(super) fn digest(algorithm: Algorithm, frame: &[u8], cap: u64) -> Option<[u8; 32]> {
    use sha2::Digest;
    let cap = usize::try_from(cap).unwrap_or(usize::MAX);
    let mut hasher = crate::pass_count::Sha256::new();
    let mut total = 0usize;
    let complete = run(algorithm, frame, cap, |b| {
        hasher.update(b);
        total += b.len();
    });
    (complete && total == cap).then(|| hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::staging::pack::{Packer, Packing};
    use std::io::Write;

    fn mixed(n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n);
        let mut x: u32 = 11;
        while v.len() < n {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            if (x >> 20).is_multiple_of(3) {
                v.extend_from_slice(b"words that repeat, words that repeat ");
            } else {
                v.extend_from_slice(&x.to_le_bytes());
            }
        }
        v.truncate(n);
        v
    }

    fn frame(algorithm: Algorithm, cut: &[u8]) -> Vec<u8> {
        let level = algorithm.default_level();
        let mut packer = Packer::new(Packing { algorithm, level }, Vec::new()).unwrap();
        packer.write_all(cut).unwrap();
        packer.finish().unwrap().0
    }

    const ALL: [Algorithm; 3] = [Algorithm::Gzip, Algorithm::Xz, Algorithm::Zstd];

    /// Each carried algorithm's frame decodes to exactly the cut.
    #[test]
    fn a_whole_frame_decodes_to_the_cut_for_each_algorithm() {
        let cut = mixed(300_000);
        for a in ALL {
            let d = decode(a, &frame(a, &cut), cut.len() as u64);
            assert!(d.complete, "{}", a.name());
            assert_eq!(d.bytes, cut, "{}", a.name());
        }
    }

    /// A frame cut short gives a prefix of the cut (never a byte that is not
    /// the cut's) and says it is not whole.
    #[test]
    fn a_truncated_frame_gives_a_prefix_of_the_cut_and_says_it_is_not_whole() {
        let cut = mixed(400_000);
        for a in ALL {
            let f = frame(a, &cut);
            let d = decode(a, &f[..f.len() * 2 / 3], cut.len() as u64);
            assert!(!d.complete, "{}", a.name());
            assert!(!d.bytes.is_empty(), "{}: something decoded", a.name());
            assert!(d.bytes.len() < cut.len(), "{}", a.name());
            assert_eq!(d.bytes, cut[..d.bytes.len()], "{}", a.name());
        }
    }

    /// A frame that decodes to more than the slice's recorded size is not
    /// that slice's frame: it is cut off at the cap and not whole.
    #[test]
    fn a_frame_larger_than_its_cap_is_refused() {
        let cut = mixed(100_000);
        for a in ALL {
            let d = decode(a, &frame(a, &cut), 50_000);
            assert!(!d.complete, "{}", a.name());
            assert!(d.bytes.len() <= 50_000, "{}", a.name());
        }
    }

    /// The streamed digest is the sha256 of the decoded cut, and `None` in
    /// every case the decoded bytes would not have been the whole cut.
    #[test]
    fn the_streamed_digest_is_of_the_decoded_cut_and_of_nothing_short_of_it() {
        use sha2::Digest;
        let cut = mixed(300_000);
        let want: [u8; 32] = sha2::Sha256::digest(&cut).into();
        for a in ALL {
            let f = frame(a, &cut);
            let n = cut.len() as u64;
            assert_eq!(digest(a, &f, n), Some(want), "{}", a.name());
            assert_eq!(
                digest(a, &f, n + 1),
                None,
                "{}: not the recorded size",
                a.name()
            );
            assert_eq!(digest(a, &f, n - 1), None, "{}: over the cap", a.name());
            assert_eq!(
                digest(a, &f[..f.len() / 2], n),
                None,
                "{}: half a frame",
                a.name()
            );
        }
    }

    /// The buffer is made once, at the recorded size: it is never bigger.
    #[test]
    fn the_decoded_buffer_is_exactly_the_recorded_size() {
        let cut = mixed(300_007);
        for a in ALL {
            let d = decode(a, &frame(a, &cut), cut.len() as u64);
            assert!(d.complete && d.unallocatable.is_none(), "{}", a.name());
            assert_eq!(d.bytes.capacity(), cut.len(), "{}", a.name());
        }
    }

    /// A recorded size no host can hold (a corrupt catalog row) is refused by
    /// name, with nothing decoded, and the process goes on.
    #[test]
    fn a_size_that_cannot_be_allocated_is_a_bad_slice_and_not_an_abort() {
        let cut = mixed(10_000);
        for a in ALL {
            let d = decode(a, &frame(a, &cut), u64::MAX / 2);
            assert!(!d.complete, "{}", a.name());
            assert!(d.bytes.is_empty(), "{}", a.name());
            let why = d.unallocatable.expect("says why");
            assert!(why.contains("cannot be held in memory"), "{why}");
        }
    }
}
