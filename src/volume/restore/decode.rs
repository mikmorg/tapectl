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
}

/// Read `decoder` to its end into `out`, keeping at most `cap` bytes. `true`
/// when it ended cleanly inside the cap.
fn drain(mut decoder: impl Read, cap: usize, out: &mut Vec<u8>) -> bool {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match decoder.read(&mut buf) {
            Ok(0) => return true,
            Ok(n) => {
                // A frame that decodes to more than the slice's recorded
                // size is not that slice's frame (and may be a bomb).
                let room = cap.saturating_sub(out.len());
                out.extend_from_slice(&buf[..n.min(room)]);
                if n > room {
                    return false;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
}

/// Decode `frame` as one frame of `algorithm`, keeping at most `cap` decoded
/// bytes. The bytes produced are returned whether or not the frame decoded
/// to its end: a frame that stops (a hole in it, a bad chunk, a cut-short
/// copy) gives the cut's bytes up to there.
pub(super) fn decode(algorithm: Algorithm, frame: &[u8], cap: u64) -> Decoded {
    let cap = usize::try_from(cap).unwrap_or(usize::MAX);
    let mut bytes = Vec::new();
    let complete = match algorithm {
        Algorithm::Gzip => drain(flate2::read::GzDecoder::new(frame), cap, &mut bytes),
        Algorithm::Xz => drain(liblzma::read::XzDecoder::new(frame), cap, &mut bytes),
        Algorithm::Zstd => match zstd::stream::read::Decoder::new(frame) {
            Ok(d) => drain(d.single_frame(), cap, &mut bytes),
            Err(_) => false,
        },
    };
    Decoded { bytes, complete }
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
}
