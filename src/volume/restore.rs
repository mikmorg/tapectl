use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use tracing::info;

use crate::config::{Config, TapectlPaths};
use crate::crypto::keys;
use crate::dar;
use crate::db::queries;
use crate::error::{Result, TapectlError};
use crate::store::{Store, TapeStore};
use crate::util::HashingWriter;

/// Restore a unit from a volume to a destination directory.
// 9 args reflects the CLI's flat shape (unit/volume/dest/device/block_size/
// dry_run alongside conn/paths/config); interim allow. This used to carry a
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
    dry_run: bool,
) -> Result<RestoreReport> {
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;

    let tenant = queries::get_tenant_by_id(conn, unit.tenant_id)?
        .ok_or_else(|| TapectlError::Other("tenant not found".into()))?;

    // Find write positions for this unit on this volume
    let positions = get_write_positions(conn, unit.id, volume_label)?;
    if positions.is_empty() {
        return Err(TapectlError::Other(format!(
            "no data for unit \"{unit_name}\" on volume \"{volume_label}\""
        )));
    }

    if dry_run {
        return Ok(RestoreReport {
            unit_name: unit_name.to_string(),
            volume_label: volume_label.to_string(),
            slices: positions.len(),
            destination: dest_dir.to_string(),
            dry_run: true,
            success: true,
        });
    }

    // Create temp dir for decrypted slices
    let restore_tmp = Path::new(dest_dir).join(".tapectl-restore-tmp");
    fs::create_dir_all(&restore_tmp)?;

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

    // Open the store read-only, positioned at BOT.
    let mut store = TapeStore::open_read(device, block_size)?;

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
            &mut store,
            position,
            wp,
            &identities,
            &ciphertext_tmp_path,
            &slice_path,
        )?;
        dar_slices.push(slice_path);

        info!(
            slice = i + 1,
            mb = plain_size / (1024 * 1024),
            "decrypted slice"
        );
    }

    // Run dar extract
    let archive_base = restore_tmp.join("restore");
    info!("extracting dar archive to {dest_dir}");
    dar::restore::extract(&config.dar.binary, &archive_base, Path::new(dest_dir))?;

    // Clean up temp files
    for path in &dar_slices {
        let _ = fs::remove_file(path);
    }
    // Remove hash files too
    if let Ok(entries) = fs::read_dir(&restore_tmp) {
        for entry in entries.flatten() {
            let _ = fs::remove_file(entry.path());
        }
    }
    let _ = fs::remove_dir(&restore_tmp);

    info!(unit = unit_name, volume = volume_label, "restore complete");

    Ok(RestoreReport {
        unit_name: unit_name.to_string(),
        volume_label: volume_label.to_string(),
        slices: positions.len(),
        destination: dest_dir.to_string(),
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

/// A `Write` adapter that forwards at most the first `limit` bytes it
/// receives to `inner`, silently discarding everything after — trims a tape
/// file's trailing block padding as it streams, without knowing in advance
/// which single `write()` call the true/padding boundary falls inside (a
/// real tape read arrives in many `block_size`-sized pushes via
/// `Store::read_file`, not the one whole-buffer write `MemStore::read_file`
/// happens to do). Mirrors, on the read side, the trim `restore_unit` used
/// to do after the fact via `&enc_data[..encrypted_bytes]` once the whole
/// (padded) slice sat in a `Vec` — issue #85.
struct TruncatingWriter<W> {
    inner: W,
    remaining: u64,
}

impl<W: Write> TruncatingWriter<W> {
    fn new(inner: W, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
        }
    }

    /// Reclaim the wrapped writer once streaming is done.
    fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for TruncatingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let take = (buf.len() as u64).min(self.remaining) as usize;
        if take > 0 {
            self.inner.write_all(&buf[..take])?;
            self.remaining -= take as u64;
        }
        // Always claim the WHOLE input as "written", even the silently
        // discarded tail — never `Ok(take)`. `Write::write_all`'s default
        // implementation treats a `write()` that returns `Ok(0)` for a
        // non-empty buffer as `ErrorKind::WriteZero`, which is exactly what
        // an "honest" `Ok(take)` triggers on every push once `remaining`
        // hits zero (proven by the tests below failing against that
        // version).
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
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
) -> Result<()> {
    // First do a full restore to a temp dir, then extract the single file
    let tmp = tempfile::tempdir().map_err(|e| TapectlError::Other(e.to_string()))?;
    let tmp_path = tmp.path().to_string_lossy().to_string();

    restore_unit(
        conn,
        paths,
        config,
        unit_name,
        volume_label,
        &tmp_path,
        device,
        block_size,
        false,
    )?;

    // Copy the requested file to dest_dir
    let source_file = tmp.path().join(file_path);
    if !source_file.exists() {
        return Err(TapectlError::Other(format!(
            "file \"{file_path}\" not found in restored unit"
        )));
    }

    let dest = Path::new(dest_dir).join(
        Path::new(file_path)
            .file_name()
            .unwrap_or(std::ffi::OsStr::new(file_path)),
    );
    fs::create_dir_all(Path::new(dest_dir))?;
    fs::copy(&source_file, &dest)?;

    info!(file = file_path, dest = %dest.display(), "file restored");
    Ok(())
}

#[derive(Debug)]
pub struct RestoreReport {
    pub unit_name: String,
    pub volume_label: String,
    pub slices: usize,
    pub destination: String,
    pub dry_run: bool,
    #[allow(dead_code)]
    pub success: bool,
}

struct WritePositionInfo {
    slice_number: i64,
    position: String,
    sha256_plain: String,
    sha256_encrypted: String,
    encrypted_bytes: i64,
}

fn get_write_positions(
    conn: &Connection,
    unit_id: i64,
    volume_label: &str,
) -> Result<Vec<WritePositionInfo>> {
    let mut stmt = conn.prepare(
        "SELECT sl.slice_number, wp.position, sl.sha256_plain, sl.sha256_encrypted, sl.encrypted_bytes
         FROM write_positions wp
         JOIN writes w ON w.id = wp.write_id
         JOIN stage_slices sl ON sl.id = wp.stage_slice_id
         JOIN stage_sets ss ON ss.id = sl.stage_set_id
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN volumes v ON v.id = w.volume_id
         WHERE s.unit_id = ?1 AND v.label = ?2 AND w.status = 'completed' AND wp.status = 'written'
         ORDER BY sl.slice_number",
    )?;

    let rows = stmt
        .query_map(params![unit_id, volume_label], |row| {
            Ok(WritePositionInfo {
                slice_number: row.get(0)?,
                position: row.get(1)?,
                sha256_plain: row.get(2)?,
                sha256_encrypted: row.get(3)?,
                encrypted_bytes: row.get(4)?,
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
    //! order-independence and `TruncatingWriter`'s own boundary logic —
    //! `MemStore::read_file` does a single whole-buffer `write_all`, so the
    //! restore-level tests alone never exercise a padding boundary that
    //! falls mid-write across several pushes the way a real tape read
    //! (`read_file_streaming`) does; `TruncatingWriter`'s own tests cover
    //! that directly.
    use super::*;
    use crate::store::MemStore;
    use sha2::{Digest, Sha256};
    use std::io::Cursor;
    use tempfile::TempDir;

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
            slice_number: 1,
            position: "0".to_string(),
            sha256_plain,
            sha256_encrypted,
            encrypted_bytes,
        };

        (store, wp)
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

    // --- TruncatingWriter --------------------------------------------------

    #[test]
    fn truncating_writer_passes_bytes_through_up_to_the_limit() {
        let mut w = TruncatingWriter::new(Vec::new(), 5);
        w.write_all(b"hello world").unwrap();
        assert_eq!(w.into_inner(), b"hello");
    }

    #[test]
    fn truncating_writer_forwards_everything_when_limit_exceeds_total_bytes() {
        // Mirrors the original buffered code's fallback branch (declared
        // size >= what's actually there means no trimming happens at all).
        let mut w = TruncatingWriter::new(Vec::new(), 100);
        w.write_all(b"short").unwrap();
        assert_eq!(w.into_inner(), b"short");
    }

    #[test]
    fn truncating_writer_handles_the_boundary_falling_mid_write_across_several_pushes() {
        // Simulates a real tape read arriving in several `block_size`-sized
        // `write_all` pushes (`Store::read_file`/`read_file_streaming`),
        // rather than the one whole-buffer write `MemStore::read_file`
        // happens to do — the limit boundary lands in the MIDDLE of the
        // third push here, not on a push boundary. This is exactly the case
        // the restore-level MemStore tests above cannot exercise.
        let mut w = TruncatingWriter::new(Vec::new(), 10);
        w.write_all(b"AAAA").unwrap(); // remaining 10 -> 6, all 4 land
        w.write_all(b"BBBB").unwrap(); // remaining 6 -> 2, all 4 land
        w.write_all(b"CCCC").unwrap(); // remaining 2 -> 0, only "CC" lands
        w.write_all(b"DDDD").unwrap(); // remaining stays 0, nothing lands
        assert_eq!(w.into_inner(), b"AAAABBBBCC");
    }

    #[test]
    fn truncating_writer_write_all_never_errors_once_the_limit_is_reached() {
        // The load-bearing behavior (issue #85): `write()` must report the
        // caller's full input length as "written" even when it silently
        // drops bytes past the limit, because `Write::write_all`'s default
        // implementation treats a `write()` that returns `Ok(0)` for a
        // non-empty buffer as `ErrorKind::WriteZero` — exactly what a naive
        // "honest" implementation (report only bytes actually forwarded)
        // triggers on every push once `remaining` hits zero.
        let mut w = TruncatingWriter::new(Vec::new(), 0);
        for _ in 0..5 {
            w.write_all(&[1, 2, 3, 4])
                .expect("write_all must not error once the limit is exhausted");
        }
        assert_eq!(w.into_inner(), Vec::<u8>::new());
    }
}
