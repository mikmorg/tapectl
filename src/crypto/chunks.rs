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
//!
//! **What a chunk's tag proves, and what it does not.** It proves the chunk
//! was encrypted under the file key this header unwraps, at this index. It
//! does not prove the file is the one the caller expected: any other age
//! file to the same recipient authenticates in full. Two things stand
//! between a caller and that mistake. [`ChunkReader::header`] is the bytes
//! that identify the file (the header with its fresh ephemeral share, and
//! the payload nonce), so chunks of two reads can be required to come from
//! one file; and the caller holds the catalog's checksums, which a chunk
//! cannot stand in for.
//!
//! **Key material** is held in `SecretBox`es and wiped when dropped: the
//! file key (copied out of age's own wrapper into one) and the payload key
//! (which only builds the AEAD, itself wiped on drop). The plaintext of a
//! chunk is decrypted in place in the caller's [`Zeroizing`] buffer, so no
//! other copy of it is made, and the buffer is wiped before each reuse.

use std::cell::RefCell;
use std::io::{self, Read};

use age::secrecy::zeroize::{Zeroize, Zeroizing};
use age::secrecy::{ExposeSecret, SecretBox};
use age::DecryptError;
use age_core::format::{FileKey, Stanza};
use chacha20poly1305::aead::{AeadInPlace, KeyInit};
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
    /// The noted key, wiped on drop like age's own wrapper it was copied from.
    key: RefCell<Option<SecretBox<[u8; 16]>>>,
}

impl KeyGrab<'_> {
    fn note(
        &self,
        unwrapped: std::result::Result<FileKey, DecryptError>,
    ) -> std::result::Result<FileKey, DecryptError> {
        unwrapped.inspect(|k| {
            *self.key.borrow_mut() = Some(SecretBox::init_with_mut(|noted: &mut [u8; 16]| {
                noted.copy_from_slice(k.expose_secret())
            }))
        })
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
    /// The header and the payload nonce, as they lay in the file: what
    /// identifies this age file among all others.
    header: Vec<u8>,
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
    // `init_with` wipes the stack copy of the derived key once it is boxed.
    let payload_key = SecretBox::<[u8; 32]>::init_with(|| {
        age_core::primitives::hkdf(
            &tap.seen[nonce_at..],
            b"payload",
            file_key.expose_secret().as_slice(),
        )
    });
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
        aead: ChaCha20Poly1305::new(payload_key.expose_secret().into()),
        header: tap.seen,
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

    /// The bytes that identify this age file: its header (recipient stanzas
    /// with a fresh ephemeral share each, and the header MAC) and the payload
    /// nonce. Equal for every read of one file, however damaged elsewhere;
    /// different for any other file, even of the same plaintext to the same
    /// recipient. Chunks of two reads with different headers are never chunks
    /// of one file. Public bytes, not key material.
    pub fn header(&self) -> &[u8] {
        &self.header
    }

    /// The plaintext length of the whole file, from its length alone.
    pub fn plain_len(&self) -> u64 {
        (self.chunks - 1) * CHUNK as u64 + (self.last_len - TAG) as u64
    }
}

impl<R: Read> ChunkReader<R> {
    /// Read the next chunk. On [`Next::Authentic`] its plaintext is in `out`,
    /// which is wiped before each read and on every other outcome.
    ///
    /// An `Err` is the source failing to read (a tape error) — the chunks
    /// not yet read are then the caller's to account for, by [`Self::index`]
    /// and [`Self::plain_len`]. A source that ends early is not an error: the
    /// chunk it cut short is [`Next::Damaged`], as is every one after it.
    pub fn next(&mut self, out: &mut Zeroizing<Vec<u8>>) -> io::Result<Next> {
        out.zeroize();
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
        out.extend_from_slice(&self.scratch[..want]);
        match self.aead.decrypt_in_place((&nonce).into(), b"", &mut **out) {
            // age rejects an empty last chunk after a non-empty file.
            Ok(()) if out.is_empty() && index > 0 => {
                out.zeroize();
                Ok(Next::Damaged { plain_len })
            }
            Ok(()) => Ok(Next::Authentic),
            Err(_) => {
                out.zeroize();
                Ok(Next::Damaged { plain_len })
            }
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
    use crate::staging::ONE_RECIPIENT_HEADER_LEN;
    use chacha20poly1305::aead::Aead;
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
        let (mut plain, mut damaged, mut buf) = (Vec::new(), Vec::new(), Zeroizing::default());
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
        let mut buf = Zeroizing::default();
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

    // ── the adversarial review's cases (#480) ─────────────────────────

    fn pattern_seeded(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| ((i * 31 + seed as usize) % 251) as u8)
            .collect()
    }

    fn enc(plain: &[u8], recipients: &[String]) -> Vec<u8> {
        crate::staging::encrypt_data(plain, recipients).unwrap()
    }

    /// `plain` encrypted to `public` with an age header of `header_len` bytes.
    fn enc_header(plain: &[u8], public: &str, header_len: usize) -> Vec<u8> {
        crate::staging::encrypt_data_with_header_len(plain, &[public.to_string()], header_len)
    }

    /// Every chunk of `cipher` read as a file of `len` bytes: the outcomes,
    /// and the plaintext with zeros where a chunk was damaged. A damaged
    /// chunk leaves the buffer empty, always.
    fn outcomes(cipher: &[u8], ids: &[age::x25519::Identity], len: u64) -> (Vec<Next>, Vec<u8>) {
        let mut r = open(Cursor::new(cipher.to_vec()), ids, len).unwrap();
        let (mut v, mut plain, mut buf) = (Vec::new(), Vec::new(), Zeroizing::default());
        loop {
            match r.next(&mut buf).unwrap() {
                Next::End => break,
                Next::Authentic => {
                    plain.extend_from_slice(&buf);
                    v.push(Next::Authentic)
                }
                Next::Damaged { plain_len } => {
                    assert!(buf.is_empty(), "a damaged chunk leaves nothing behind");
                    plain.extend(std::iter::repeat_n(0u8, plain_len));
                    v.push(Next::Damaged { plain_len })
                }
            }
        }
        (v, plain)
    }

    fn header_len(cipher: &[u8], payload_plain: usize) -> usize {
        let chunks = payload_plain.div_ceil(CHUNK).max(1);
        cipher.len() - payload_plain - chunks * TAG
    }

    /// A file cut at a chunk boundary, whose recorded length is the cut's
    /// length: the cut chunk was a non-last chunk, so under the last-chunk
    /// nonce it does not authenticate — a truncation is never "complete".
    #[test]
    fn a_file_truncated_at_a_chunk_boundary_with_a_matching_length_is_not_complete() {
        let (id, public) = keypair();
        for plain_len in [3 * CHUNK + 500, 3 * CHUNK] {
            let plain = pattern_seeded(plain_len, 1);
            let cipher = enc(&plain, std::slice::from_ref(&public));
            let h = header_len(&cipher, plain_len);
            let cut = &cipher[..h + 2 * ENCRYPTED_CHUNK];
            let (v, _) = outcomes(cut, std::slice::from_ref(&id), cut.len() as u64);
            assert_eq!(
                v,
                vec![Next::Authentic, Next::Damaged { plain_len: CHUNK }],
                "{plain_len}"
            );
        }
    }

    /// A file extended by a copied chunk: with its own length it is damaged
    /// at the seam, and with the catalog's length the extra bytes are ignored.
    #[test]
    fn a_file_extended_by_an_appended_chunk_is_not_authentic() {
        let (id, public) = keypair();
        let plain = pattern_seeded(2 * CHUNK, 2);
        let cipher = enc(&plain, std::slice::from_ref(&public));
        let h = header_len(&cipher, 2 * CHUNK);
        let mut ext = cipher.clone();
        ext.extend_from_slice(&cipher[h..h + ENCRYPTED_CHUNK]);
        let (v, _) = outcomes(&ext, std::slice::from_ref(&id), ext.len() as u64);
        assert_eq!(
            v,
            vec![
                Next::Authentic,
                Next::Damaged { plain_len: CHUNK },
                Next::Damaged { plain_len: CHUNK }
            ]
        );
        let (v, got) = outcomes(&ext, std::slice::from_ref(&id), cipher.len() as u64);
        assert_eq!(v, vec![Next::Authentic, Next::Authentic]);
        assert_eq!(got, plain);
    }

    /// The first chunk, the full-length last chunk, a one-chunk file and an
    /// empty file, each damaged: a hole of exactly its own length.
    #[test]
    fn a_damaged_first_chunk_and_a_full_length_last_chunk_are_holes_of_their_own_length() {
        let (id, public) = keypair();
        let plain = pattern_seeded(2 * CHUNK, 3);
        let cipher = enc(&plain, std::slice::from_ref(&public));
        let h = header_len(&cipher, 2 * CHUNK);
        let mut c0 = cipher.clone();
        c0[h + 3] ^= 0x80;
        let (v, got) = outcomes(&c0, std::slice::from_ref(&id), c0.len() as u64);
        assert_eq!(v, vec![Next::Damaged { plain_len: CHUNK }, Next::Authentic]);
        assert!(got[..CHUNK].iter().all(|&b| b == 0));
        assert_eq!(&got[CHUNK..], &plain[CHUNK..]);
        let mut c1 = cipher.clone();
        let n = c1.len();
        c1[n - 1] ^= 0x01; // the last chunk's tag
        let (v, got) = outcomes(&c1, std::slice::from_ref(&id), c1.len() as u64);
        assert_eq!(v, vec![Next::Authentic, Next::Damaged { plain_len: CHUNK }]);
        assert_eq!(got.len(), plain.len());
        // A one-chunk file damaged: the only chunk is first and last.
        let plain = pattern_seeded(10, 4);
        let mut c = enc(&plain, std::slice::from_ref(&public));
        let n = c.len();
        c[n - 20] ^= 0x01;
        let (v, _) = outcomes(&c, std::slice::from_ref(&id), c.len() as u64);
        assert_eq!(v, vec![Next::Damaged { plain_len: 10 }]);
        // An empty file damaged.
        let mut c = enc(b"", std::slice::from_ref(&public));
        let n = c.len();
        c[n - 1] ^= 0x01;
        let (v, _) = outcomes(&c, std::slice::from_ref(&id), c.len() as u64);
        assert_eq!(v, vec![Next::Damaged { plain_len: 0 }]);
    }

    /// The header is age's to refuse (its MAC covers the stanzas, the grease
    /// among them); the payload nonce lies outside the MAC, so changing it
    /// fails every chunk and releases nothing.
    #[test]
    fn a_tampered_header_is_refused_and_a_tampered_nonce_releases_nothing() {
        let (id, public) = keypair();
        let cipher = enc(&pattern_seeded(1000, 5), std::slice::from_ref(&public));
        let mac_at = cipher.windows(4).position(|w| w == b"--- ").unwrap() + 4;
        // A MAC character swapped for another base64 character.
        let mut c = cipher.clone();
        c[mac_at + 5] = if c[mac_at + 5] == b'A' { b'B' } else { b'A' };
        assert!(open(
            Cursor::new(c.clone()),
            std::slice::from_ref(&id),
            c.len() as u64
        )
        .is_err());
        // A byte of a stanza body (inside the MAC'd region), base64-valid.
        let mut c = cipher.clone();
        let nl = c.iter().position(|&b| b == b'\n').unwrap(); // end of version line
        let nl2 = nl + 1 + c[nl + 1..].iter().position(|&b| b == b'\n').unwrap(); // first stanza line
        let at = nl2 + 3;
        c[at] = if c[at] == b'A' { b'B' } else { b'A' };
        assert!(open(
            Cursor::new(c.clone()),
            std::slice::from_ref(&id),
            c.len() as u64
        )
        .is_err());
        // The grease stanza's tag changed: the key still unwraps, only the
        // MAC can refuse.
        let mut c = cipher.clone();
        let g = c.windows(7).position(|w| w == b"-grease").unwrap() - 1;
        c[g] = if c[g] == b'!' { b'#' } else { b'!' };
        assert!(
            open(
                Cursor::new(c.clone()),
                std::slice::from_ref(&id),
                c.len() as u64
            )
            .is_err(),
            "the MAC refuses a changed grease stanza"
        );
        // The payload nonce changed: every chunk fails, nothing is emitted.
        let mut c = cipher.clone();
        let nonce_at = mac_at + 43 + 1;
        c[nonce_at] ^= 0x01;
        let (v, got) = outcomes(&c, std::slice::from_ref(&id), c.len() as u64);
        assert_eq!(v, vec![Next::Damaged { plain_len: 1000 }]);
        assert!(got.iter().all(|&b| b == 0));
    }

    /// Several identities, several recipients: the one that unwraps is used,
    /// in any order, and an identity that matches nothing is not an error.
    #[test]
    fn several_identities_and_several_recipients_open_the_same_file() {
        let (id, public) = keypair();
        let (other, other_pub) = keypair();
        let plain = pattern_seeded(CHUNK + 7, 6);
        let cipher = enc(&plain, &[other_pub.clone(), public.clone()]);
        for ids in [
            vec![id.clone()],
            vec![other.clone()],
            vec![other.clone(), id.clone()],
        ] {
            let (v, got) = outcomes(&cipher, &ids, cipher.len() as u64);
            assert_eq!(v, vec![Next::Authentic, Next::Authentic]);
            assert_eq!(got, plain);
        }
        let only = enc(&plain, std::slice::from_ref(&public));
        let (third, _) = keypair();
        let (v, got) = outcomes(&only, &[third, id.clone()], only.len() as u64);
        assert_eq!(v, vec![Next::Authentic, Next::Authentic]);
        assert_eq!(got, plain);
    }

    /// A length the file cannot have is refused at open or reads as damage,
    /// and a claimed length of u64::MAX allocates nothing.
    #[test]
    fn inconsistent_lengths_are_refused_or_read_as_damage() {
        let (id, public) = keypair();
        let plain = pattern_seeded(CHUNK + 1000, 7);
        let mut cipher = enc(&plain, std::slice::from_ref(&public));
        let real = cipher.len() as u64;
        cipher.extend_from_slice(&[0u8; 100]); // tape block padding
        let (v, _) = outcomes(&cipher, std::slice::from_ref(&id), real + 5);
        assert_eq!(v, vec![Next::Authentic, Next::Damaged { plain_len: 1005 }]);
        let (v, _) = outcomes(&cipher, std::slice::from_ref(&id), real - 5);
        assert_eq!(v, vec![Next::Authentic, Next::Damaged { plain_len: 995 }]);
        let h = header_len(&cipher[..real as usize], CHUNK + 1000) as u64;
        // A length leaving 1..15 bytes for the last chunk is refused at open.
        let refused =
            |len: u64| open(Cursor::new(cipher.clone()), std::slice::from_ref(&id), len).is_err();
        assert!(refused(h + ENCRYPTED_CHUNK as u64 + 15));
        assert!(refused(h + 15));
        assert!(refused(h - 1));
        assert!(refused(0));
        // A huge claimed length: the source ends, so it is damage.
        let mut r = open(
            Cursor::new(cipher.clone()),
            std::slice::from_ref(&id),
            u64::MAX,
        )
        .unwrap();
        let mut buf = Zeroizing::default();
        assert_eq!(r.next(&mut buf).unwrap(), Next::Authentic);
        assert_eq!(
            r.next(&mut buf).unwrap(),
            Next::Damaged { plain_len: CHUNK }
        );
        assert!(r.plain_len() > 0);
    }

    /// age refuses an empty last chunk after a non-empty one, and so does
    /// this module — as damage of zero length, never as data.
    #[test]
    fn an_empty_last_chunk_after_a_non_empty_one_is_damage_of_zero_length() {
        let (id, public) = keypair();
        let plain = pattern_seeded(CHUNK, 8);
        let cipher = enc(&plain, std::slice::from_ref(&public));
        let h = header_len(&cipher, CHUNK);
        let r = open(
            Cursor::new(cipher.clone()),
            std::slice::from_ref(&id),
            cipher.len() as u64,
        )
        .unwrap();
        let mut crafted = cipher[..h].to_vec();
        let n0 = [0u8; 12];
        let mut n1 = [0u8; 12];
        n1[10] = 1;
        n1[11] = 1;
        crafted.extend(r.aead.encrypt((&n0).into(), &plain[..]).unwrap());
        crafted.extend(r.aead.encrypt((&n1).into(), &b""[..]).unwrap());
        let (v, got) = outcomes(&crafted, std::slice::from_ref(&id), crafted.len() as u64);
        assert_eq!(v, vec![Next::Authentic, Next::Damaged { plain_len: 0 }]);
        assert_eq!(got, plain);
        // age itself refuses it.
        let mut a = age::Decryptor::new(Cursor::new(&crafted))
            .unwrap()
            .decrypt(std::iter::once(&id as &dyn age::Identity))
            .unwrap();
        let mut sink = Vec::new();
        assert!(Read::read_to_end(&mut a, &mut sink).is_err());
    }

    /// THE finding behind #480's second review: a DIFFERENT age file to the
    /// same recipient, of the length the catalog recorded, authenticates in
    /// full — every chunk's tag proves only "encrypted under some key this
    /// identity unwraps". What tells it from the expected file is its header:
    /// the caller is given those bytes to compare, and the plaintext to hash.
    #[test]
    fn a_substituted_file_authenticates_in_full_and_is_told_apart_by_its_header() {
        let (id, public) = keypair();
        let a_plain = pattern_seeded(3 * CHUNK + 123, 9);
        let a = enc_header(&a_plain, &public, ONE_RECIPIENT_HEADER_LEN);
        let b_plain = pattern_seeded(3 * CHUNK + 123, 77);
        let b = enc_header(&b_plain, &public, ONE_RECIPIENT_HEADER_LEN);
        assert_eq!(b.len(), a.len());
        let (v, got) = outcomes(&b, std::slice::from_ref(&id), a.len() as u64);
        assert!(v.iter().all(|n| *n == Next::Authentic), "{v:?}");
        assert_eq!(got, b_plain);
        assert_ne!(got, a_plain);
        let header_of = |cipher: &[u8]| {
            open(
                Cursor::new(cipher.to_vec()),
                std::slice::from_ref(&id),
                a.len() as u64,
            )
            .unwrap()
            .header()
            .to_vec()
        };
        assert_ne!(
            header_of(&a),
            header_of(&b),
            "two files to one recipient are told apart by their headers"
        );
        // A header 4 bytes longer, read at `a`'s length: the last chunk comes
        // up 4 bytes short and only it fails. (A file drawn at random could
        // have a header so much longer that the last chunk ended inside its
        // tag and the open itself refused, #489.)
        let c = enc_header(&b_plain, &public, ONE_RECIPIENT_HEADER_LEN + 4);
        let mut padded = c.clone();
        padded.extend_from_slice(&[0u8; 4096]);
        let (v, _) = outcomes(&padded, std::slice::from_ref(&id), a.len() as u64);
        assert_eq!(
            &v[..3],
            &[Next::Authentic, Next::Authentic, Next::Authentic]
        );
        assert!(matches!(v[3], Next::Damaged { .. }));
    }

    /// The identifying header is the same bytes on every read of one file,
    /// damaged elsewhere or not, and covers the whole header and the nonce.
    #[test]
    fn the_header_is_the_same_on_every_read_of_one_file() {
        let (id, public) = keypair();
        let plain = pattern_seeded(3 * CHUNK, 10);
        let cipher = enc(&plain, std::slice::from_ref(&public));
        let h = header_len(&cipher, 3 * CHUNK);
        let mut hurt = cipher.clone();
        hurt[h + ENCRYPTED_CHUNK + 4] ^= 0xFF;
        let read = |bytes: &[u8]| {
            open(
                Cursor::new(bytes.to_vec()),
                std::slice::from_ref(&id),
                bytes.len() as u64,
            )
            .unwrap()
            .header()
            .to_vec()
        };
        assert_eq!(read(&cipher), read(&hurt));
        assert_eq!(
            read(&cipher),
            &cipher[..h],
            "it is the header and the nonce, exactly as they lay in the file"
        );
    }
}
