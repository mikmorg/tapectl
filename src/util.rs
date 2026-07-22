//! Small shared utilities that don't belong to any one subsystem.

use std::io::{self, Read};

use sha2::{Digest, Sha256};

/// A `Read` adapter that computes the sha256 of every byte read through it.
///
/// Lets one streaming pass serve two purposes — e.g. feeding a tape write
/// while hashing the same bytes for the tri-layer integrity model's *execute*
/// layer (`docs/design/v2-open-questions.md` §2.4/§9: "execute re-hashes
/// inline on the same streaming read that feeds the tape") — instead of a
/// second read solely to hash. Wrap any `Read` source; drive it to
/// completion through the normal `Read` interface, then call
/// `finalize_hex()`.
pub struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
}

impl<R: Read> HashingReader<R> {
    /// Wrap `inner`, starting from a fresh hash state.
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    /// The hex sha256 of every byte read through this adapter so far.
    ///
    /// Non-consuming (clones the internal hasher state), so it is safe to
    /// call after driving the reader to EOF without giving up ownership —
    /// the typical call site streams to completion, then reads this once.
    pub fn finalize_hex(&self) -> String {
        format!("{:x}", self.hasher.clone().finalize())
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.hasher.update(&buf[..n]);
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn direct_hash(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        format!("{:x}", h.finalize())
    }

    #[test]
    fn hashing_reader_matches_direct_hash_on_read_to_end() {
        let data = b"the quick brown fox jumps over the lazy dog, repeated to exceed one buffer, \
                     the quick brown fox jumps over the lazy dog, repeated to exceed one buffer";
        let mut reader = HashingReader::new(Cursor::new(data.to_vec()));
        let mut sink = Vec::new();
        reader.read_to_end(&mut sink).unwrap();

        assert_eq!(sink, data);
        assert_eq!(reader.finalize_hex(), direct_hash(data));
    }

    #[test]
    fn hashing_reader_accumulates_across_small_partial_reads() {
        // Read in chunks smaller than any real buffer to exercise partial
        // reads landing in the hasher exactly once each.
        let data: Vec<u8> = (0..=255u8).collect();
        let mut reader = HashingReader::new(Cursor::new(data.clone()));
        let mut buf = [0u8; 7];
        let mut total = Vec::new();
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total.extend_from_slice(&buf[..n]);
        }
        assert_eq!(total, data);
        assert_eq!(reader.finalize_hex(), direct_hash(&data));
    }

    #[test]
    fn empty_source_hashes_to_the_empty_digest() {
        let mut reader = HashingReader::new(Cursor::new(Vec::<u8>::new()));
        let mut sink = Vec::new();
        reader.read_to_end(&mut sink).unwrap();
        assert_eq!(reader.finalize_hex(), direct_hash(&[]));
    }

    #[test]
    fn finalize_hex_is_stable_when_called_more_than_once() {
        // Non-consuming: calling it twice must not change state or crash.
        let data = b"stable";
        let mut reader = HashingReader::new(Cursor::new(data.to_vec()));
        let mut sink = Vec::new();
        reader.read_to_end(&mut sink).unwrap();
        assert_eq!(reader.finalize_hex(), reader.finalize_hex());
    }
}
