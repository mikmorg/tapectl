//! An age file's payload, one authenticated chunk at a time (ADR-0012 item
//! 101, issue #480).
//!
//! age's STREAM construction encrypts a file in 64 KiB chunks, each under
//! the nonce `counter ‖ last-flag`, so every chunk authenticates alone. The
//! `age` crate's `StreamReader` cannot use that: after one chunk fails it
//! cannot be driven on. Its failed `decrypt_chunk` returns before
//! `encrypted_pos` is reset, so the stale ciphertext stays in its buffer, and
//! a retry as a last chunk sets the nonce's last flag before the AEAD fails,
//! so every later call dies at `set_last`. `Seek` re-sets the counter but
//! never the buffer. (Measured against age 0.11.2, `stream.rs`; the test
//! `age_stream_reader_cannot_continue_past_a_bad_chunk` pins it.) So one bad
//! 64 KiB chunk of a 1 GiB slice would lose the rest of the slice.
//!
//! This module reads the same format itself. It opens the file with the
//! `age` crate, so the header MAC and the recipient stanzas are age's own
//! checks, and borrows only two facts from that: the file key (an identity
//! wrapper notes the key it unwraps) and the payload nonce (the 16 bytes the
//! header parse ends on). The payload key and the chunking are the age v1
//! spec's: `HKDF-SHA256(salt = nonce, ikm = file key, info = "payload")`, and
//! ChaCha20-Poly1305 over 64 KiB chunks under an 11-byte big-endian counter
//! and a last-chunk byte. The tests prove it byte for byte against what the
//! `age` crate writes and reads, at every chunk boundary.
//!
//! A chunk that fails authentication is reported, never decrypted around: no
//! unauthenticated byte is ever returned. How long it was is known from the
//! file's length, which the catalog records: the payload is
//! `encrypted_len - header - nonce` bytes, cut into 65 552-byte chunks with
//! a shorter last one, so a bad chunk's plaintext span is arithmetic and
//! needs none of its bytes.

use std::cell::RefCell;
use std::io::{self, Read};

use age::secrecy::ExposeSecret;
use age::DecryptError;
use age_core::format::{FileKey, Stanza};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::ChaCha20Poly1305;

use crate::error::{Result, TapectlError};

/// age's plaintext chunk size.
pub const CHUNK: usize = 64 * 1024;
const TAG: usize = 16;
/// A full chunk as it lies in the file: the plaintext and its tag.
pub const ENCRYPTED_CHUNK: usize = CHUNK + TAG;
/// The payload nonce that follows the header.
const PAYLOAD_NONCE: usize = 16;

/// Counts and records the bytes read through it while `recording`.
struct Tap<R> {
    inner: R,
    seen: Vec<u8>,
    count: u64,
    recording: bool,
}

impl<R: Read> Read for Tap<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count += n as u64;
        if self.recording {
            self.seen.extend_from_slice(&buf[..n]);
        }
        Ok(n)
    }
}

/// An identity that tries `identities` as age would, and notes the file key
/// the one that matched unwrapped.
struct KeyGrab<'a> {
    identities: &'a [age::x25519::Identity],
    key: RefCell<Option<[u8; 16]>>,
}

impl KeyGrab<'_> {
    fn note(
        &self,
        unwrapped: std::result::Result<FileKey, DecryptError>,
    ) -> std::result::Result<FileKey, DecryptError> {
        unwrapped.inspect(|k| *self.key.borrow_mut() = Some(*k.expose_secret()))
    }
}

impl age::Identity for KeyGrab<'_> {
    fn unwrap_stanza(&self, stanza: &Stanza) -> Option<std::result::Result<FileKey, DecryptError>> {
        self.identities
            .iter()
            .find_map(|id| id.unwrap_stanza(stanza))
            .map(|r| self.note(r))
    }

    fn unwrap_stanzas(
        &self,
        stanzas: &[Stanza],
    ) -> Option<std::result::Result<FileKey, DecryptError>> {
        self.identities
            .iter()
            .find_map(|id| id.unwrap_stanzas(stanzas))
            .map(|r| self.note(r))
    }
}

/// What reading one chunk gave.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// The chunk authenticated; its plaintext is in the buffer given to
    /// [`ChunkReader::next`].
    Authentic,
    /// The chunk did not authenticate (or was cut short). Its plaintext
    /// would have been `plain_len` bytes. The buffer holds nothing of it.
    Damaged { plain_len: usize },
    /// Every chunk has been read.
    End,
}

/// An age file opened for chunk-by-chunk reading: positioned at the first
/// payload chunk, knowing how many there are and how long the last is.
pub struct ChunkReader<R> {
    src: R,
    aead: ChaCha20Poly1305,
    /// The index of the next chunk.
    index: u64,
    chunks: u64,
    /// The last chunk's length as it lies in the file, tag included.
    last_len: usize,
    scratch: Vec<u8>,
}

/// Open the age file `src` (`encrypted_len` bytes in all, header included)
/// with `identities`. The header's MAC and the recipient match are age's
/// own; the errors are worded as `read_slice` has always worded them.
pub fn open<R: Read>(
    src: R,
    identities: &[age::x25519::Identity],
    encrypted_len: u64,
) -> Result<ChunkReader<R>> {
    let mut tap = Tap {
        inner: src,
        seen: Vec::new(),
        count: 0,
        recording: true,
    };
    let grab = KeyGrab {
        identities,
        key: RefCell::new(None),
    };
    {
        let decryptor = age::Decryptor::new(&mut tap)
            .map_err(|e| TapectlError::Encryption(format!("decryptor: {e}")))?;
        // Nothing of the payload is read here: the reader age hands back is
        // dropped unread, and only its header work (the MAC, the key) is used.
        let _unread = decryptor
            .decrypt(std::iter::once(&grab as &dyn age::Identity))
            .map_err(|e| TapectlError::Encryption(format!("decrypt: {e}")))?;
    }
    tap.recording = false;
    let file_key = grab
        .key
        .into_inner()
        .ok_or_else(|| TapectlError::Encryption("decrypt: no file key was unwrapped".into()))?;
    let nonce_at =
        tap.seen.len().checked_sub(PAYLOAD_NONCE).ok_or_else(|| {
            TapectlError::Encryption("decryptor: the age header is too short".into())
        })?;
    let payload_key =
        age_core::primitives::hkdf(&tap.seen[nonce_at..], b"payload", file_key.as_slice());
    let payload = encrypted_len.checked_sub(tap.count).ok_or_else(|| {
        TapectlError::Encryption(format!(
            "the age file is shorter ({encrypted_len} bytes) than its own header ({})",
            tap.count
        ))
    })?;
    // One chunk at least: an empty plaintext is one chunk of a bare tag.
    if payload < TAG as u64 {
        return Err(TapectlError::Encryption(format!(
            "the age file ends inside its first chunk (payload {payload} bytes)"
        )));
    }
    let chunks = payload.div_ceil(ENCRYPTED_CHUNK as u64);
    let last_len = (payload - (chunks - 1) * ENCRYPTED_CHUNK as u64) as usize;
    if last_len < TAG {
        return Err(TapectlError::Encryption(
            "the age file ends inside its last chunk's tag".into(),
        ));
    }
    Ok(ChunkReader {
        src: tap.inner,
        aead: ChaCha20Poly1305::new((&payload_key).into()),
        index: 0,
        chunks,
        last_len,
        scratch: vec![0; ENCRYPTED_CHUNK],
    })
}

impl<R> ChunkReader<R> {
    /// The number of chunks in the file.
    pub fn chunks(&self) -> u64 {
        self.chunks
    }

    /// The plaintext length of the whole file, from its length alone.
    pub fn plain_len(&self) -> u64 {
        (self.chunks - 1) * CHUNK as u64 + (self.last_len - TAG) as u64
    }
}

impl<R: Read> ChunkReader<R> {
    /// Read the next chunk. On [`Next::Authentic`] its plaintext replaces the
    /// contents of `out`; otherwise `out` is emptied.
    ///
    /// An `Err` is the source failing to read (a tape error) — the chunks
    /// not yet read are then the caller's to account for, by [`Self::index`]
    /// and [`Self::plain_len`]. A source that ends early is not an error: the
    /// chunk it cut short is [`Next::Damaged`], as is every one after it.
    pub fn next(&mut self, out: &mut Vec<u8>) -> io::Result<Next> {
        out.clear();
        if self.index >= self.chunks {
            return Ok(Next::End);
        }
        let last = self.index + 1 == self.chunks;
        let want = if last { self.last_len } else { ENCRYPTED_CHUNK };
        let plain_len = want - TAG;
        let index = self.index;
        self.index += 1;
        let mut got = 0;
        while got < want {
            match self.src.read(&mut self.scratch[got..want]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    // The chunk is lost with the read; the caller decides
                    // what the rest of the file is.
                    self.index = index;
                    return Err(e);
                }
            }
        }
        if got < want {
            return Ok(Next::Damaged { plain_len });
        }
        let mut nonce = [0u8; 12];
        nonce[3..11].copy_from_slice(&index.to_be_bytes());
        nonce[11] = u8::from(last);
        match self.aead.decrypt((&nonce).into(), &self.scratch[..want]) {
            Ok(plain) => {
                // age rejects an empty last chunk after a non-empty file.
                if plain.is_empty() && index > 0 {
                    return Ok(Next::Damaged { plain_len });
                }
                *out = plain;
                Ok(Next::Authentic)
            }
            Err(_) => Ok(Next::Damaged { plain_len }),
        }
    }

    /// The index of the next chunk [`Self::next`] will read.
    pub fn index(&self) -> u64 {
        self.index
    }

    /// The plaintext length of the chunks from `index` on — what a source
    /// that failed outright leaves unread.
    pub fn plain_len_from(&self, index: u64) -> u64 {
        if index >= self.chunks {
            return 0;
        }
        self.plain_len() - index * CHUNK as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Seek, SeekFrom};

    fn keypair() -> (age::x25519::Identity, String) {
        let id = age::x25519::Identity::generate();
        let public = id.to_public().to_string();
        (id, public)
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    /// Every chunk of `cipher` through [`ChunkReader`], collected, with the
    /// damaged spans.
    fn read_all(cipher: &[u8], id: &age::x25519::Identity) -> (Vec<u8>, Vec<(u64, usize)>, u64) {
        let mut reader = open(
            Cursor::new(cipher.to_vec()),
            std::slice::from_ref(id),
            cipher.len() as u64,
        )
        .unwrap();
        let (mut plain, mut damaged, mut buf) = (Vec::new(), Vec::new(), Vec::new());
        loop {
            let at = plain.len() as u64;
            match reader.next(&mut buf).unwrap() {
                Next::Authentic => plain.extend_from_slice(&buf),
                Next::Damaged { plain_len } => {
                    damaged.push((at, plain_len));
                    plain.extend(std::iter::repeat_n(0u8, plain_len));
                }
                Next::End => break,
            }
        }
        (plain, damaged, reader.chunks())
    }

    /// The proof against the `age` crate itself, at every size around a
    /// chunk boundary: what age wrote reads back byte for byte, and the
    /// chunk count and the plaintext length are the file length's arithmetic.
    #[test]
    fn it_reads_what_the_age_crate_writes_at_every_chunk_boundary() {
        let (id, public) = keypair();
        for len in [
            0,
            1,
            100,
            CHUNK - 1,
            CHUNK,
            CHUNK + 1,
            2 * CHUNK - 1,
            2 * CHUNK,
            2 * CHUNK + 1,
            3 * CHUNK,
            200_000,
        ] {
            let plain = pattern(len);
            let cipher =
                crate::staging::encrypt_data(&plain, std::slice::from_ref(&public)).unwrap();
            let (got, damaged, chunks) = read_all(&cipher, &id);
            assert_eq!(got, plain, "plaintext of {len} bytes");
            assert!(damaged.is_empty(), "{len}: {damaged:?}");
            assert_eq!(
                chunks,
                len.div_ceil(CHUNK).max(1) as u64,
                "chunks of {len} bytes"
            );
            let reader = open(
                Cursor::new(cipher.clone()),
                std::slice::from_ref(&id),
                cipher.len() as u64,
            )
            .unwrap();
            assert_eq!(reader.plain_len(), len as u64, "plain_len of {len} bytes");
        }
    }

    /// And the other direction: what this module cannot decrypt, age cannot
    /// either — one flipped byte in the middle chunk is damaged here and an
    /// error in age, and the chunks around it read here are the ones age
    /// would have read up to the failure.
    #[test]
    fn a_flipped_byte_costs_exactly_its_own_chunk() {
        let (id, public) = keypair();
        let plain = pattern(3 * CHUNK + 1000);
        let mut cipher =
            crate::staging::encrypt_data(&plain, std::slice::from_ref(&public)).unwrap();
        let header = cipher.len() - (3 * ENCRYPTED_CHUNK + 1000 + TAG);
        // A byte in chunk 1's ciphertext.
        cipher[header + ENCRYPTED_CHUNK + 777] ^= 0x01;
        let (got, damaged, _) = read_all(&cipher, &id);
        assert_eq!(damaged, vec![(CHUNK as u64, CHUNK)]);
        assert_eq!(&got[..CHUNK], &plain[..CHUNK], "the chunk before is whole");
        assert!(
            got[CHUNK..2 * CHUNK].iter().all(|&b| b == 0),
            "the hole is zeros, never the unauthenticated plaintext"
        );
        assert_eq!(
            &got[2 * CHUNK..],
            &plain[2 * CHUNK..],
            "the chunks after are whole: they decrypt on their own"
        );
        // age's own reader stops at the bad chunk.
        let mut r = age::Decryptor::new(Cursor::new(&cipher))
            .unwrap()
            .decrypt(std::iter::once(&id as &dyn age::Identity))
            .unwrap();
        let mut sink = Vec::new();
        assert!(io::copy(&mut r, &mut sink).is_err());
        assert_eq!(sink.len(), CHUNK, "age delivered only the chunks before");
    }

    /// The short last chunk, damaged: its hole is its own length, not 64 KiB.
    #[test]
    fn a_damaged_last_chunk_is_a_hole_of_its_own_length() {
        let (id, public) = keypair();
        let plain = pattern(CHUNK + 1000);
        let mut cipher =
            crate::staging::encrypt_data(&plain, std::slice::from_ref(&public)).unwrap();
        let n = cipher.len();
        cipher[n - 5] ^= 0xFF;
        let (got, damaged, _) = read_all(&cipher, &id);
        assert_eq!(damaged, vec![(CHUNK as u64, 1000)]);
        assert_eq!(got.len(), plain.len());
        assert_eq!(&got[..CHUNK], &plain[..CHUNK]);
    }

    /// A source that ends early: the chunk it cut and every chunk after are
    /// damaged, and the span still adds up to the whole file.
    #[test]
    fn a_source_that_ends_early_leaves_the_rest_damaged() {
        let (id, public) = keypair();
        let plain = pattern(4 * CHUNK);
        let cipher = crate::staging::encrypt_data(&plain, std::slice::from_ref(&public)).unwrap();
        let header = cipher.len() - 4 * ENCRYPTED_CHUNK;
        let cut = &cipher[..header + 2 * ENCRYPTED_CHUNK + 100];
        let mut reader = open(
            Cursor::new(cut.to_vec()),
            std::slice::from_ref(&id),
            cipher.len() as u64,
        )
        .unwrap();
        let mut buf = Vec::new();
        let mut outcomes = Vec::new();
        loop {
            match reader.next(&mut buf).unwrap() {
                Next::End => break,
                other => outcomes.push(other),
            }
        }
        assert_eq!(
            outcomes,
            vec![
                Next::Authentic,
                Next::Authentic,
                Next::Damaged { plain_len: CHUNK },
                Next::Damaged { plain_len: CHUNK },
            ]
        );
    }

    /// The wrong key is age's refusal, worded as `read_slice` always has.
    #[test]
    fn the_wrong_key_is_refused_before_any_chunk() {
        let (_, public) = keypair();
        let (other, _) = keypair();
        let cipher =
            crate::staging::encrypt_data(b"secret", std::slice::from_ref(&public)).unwrap();
        let err = open(
            Cursor::new(cipher.clone()),
            std::slice::from_ref(&other),
            cipher.len() as u64,
        )
        .err()
        .expect("the wrong key opens nothing");
        assert!(err.to_string().contains("decrypt"), "{err}");
    }

    /// The finding behind this module, pinned so a future `age` that fixes it
    /// is noticed: after a bad chunk `StreamReader` cannot be driven to the
    /// next one by `Seek`, because the stale ciphertext stays in its buffer.
    #[test]
    fn age_stream_reader_cannot_continue_past_a_bad_chunk() {
        let (id, public) = keypair();
        let plain = pattern(3 * CHUNK);
        let mut cipher =
            crate::staging::encrypt_data(&plain, std::slice::from_ref(&public)).unwrap();
        let header = cipher.len() - 3 * ENCRYPTED_CHUNK;
        cipher[header + ENCRYPTED_CHUNK + 5] ^= 0x01;
        let mut r = age::Decryptor::new(Cursor::new(cipher))
            .unwrap()
            .decrypt(std::iter::once(&id as &dyn age::Identity))
            .unwrap();
        let mut buf = vec![0u8; CHUNK];
        r.read_exact(&mut buf).unwrap();
        assert!(r.read(&mut buf).is_err(), "chunk 1 fails");
        // Skip to chunk 2 (a mid-chunk target, which avoids the length probe).
        r.seek(SeekFrom::Start(2 * CHUNK as u64 + 1)).ok();
        let mut rest = Vec::new();
        let outcome = r.read_to_end(&mut rest);
        // Measured on age 0.11.2: "last chunk has been processed".
        assert!(
            outcome.is_err() && rest.is_empty(),
            "if this passes age can continue past a bad chunk and this module is no longer \
             needed: {outcome:?}, {} bytes",
            rest.len()
        );
    }
}
