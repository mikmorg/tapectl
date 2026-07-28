use std::io::Read;
use std::path::Path;

use rusqlite::{params, Connection};
use tracing::info;

use crate::error::{Result, TapectlError};
use crate::util::HashingReader;

/// Validate source files by computing SHA256 for all files in the snapshot.
/// Returns Vec<(relative_path, sha256_hex)>.
pub fn validate_source(
    conn: &Connection,
    snapshot_id: i64,
    source_path: &str,
) -> Result<Vec<(String, String)>> {
    let base = Path::new(source_path);

    // Get all non-directory files from the manifest
    let mut stmt = conn.prepare(
        "SELECT path, size_bytes FROM files WHERE snapshot_id = ?1 AND is_directory = 0",
    )?;
    let files: Vec<(String, i64)> = stmt
        .query_map(params![snapshot_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let total_files = files.len();
    let total_bytes: i64 = files.iter().map(|(_, s)| s).sum();
    info!(
        files = total_files,
        total_mb = total_bytes / (1024 * 1024),
        "validating source checksums"
    );

    let mut checksums = Vec::new();
    let mut validated = 0;

    for (rel_path, expected_size) in &files {
        let full_path = base.join(rel_path);
        let expected_size = *expected_size;

        // Size check needs no read at all (H9 remainder, issue #84): a
        // metadata() stat is instant and fails fast on a missing or
        // changed file before any I/O is spent hashing it — replaces the
        // `exists()` + whole-file `std::fs::read` this check used to ride
        // along with.
        check_source_size(&full_path, rel_path, expected_size)?;

        // Hash by streaming (H9 remainder, issue #84): reuses
        // `util::HashingReader` in fixed-size chunks exactly as
        // `staging::encrypt_file_streaming` does, so peak RAM for this pass
        // is `VALIDATE_STREAM_BUFFER` alone, never the size of the source
        // file. Before this fix, the whole-slice OOM #35 closed for the
        // *encrypted slice* had simply moved here, keyed to the largest
        // single *source* file instead — per
        // `docs/design/v2-open-questions.md` §7 the media-library workload
        // is folders of 2-15 GB typically dominated by ONE file at ~90%, so
        // this ran 2-13 GB into RAM before #35's streaming loop was ever
        // reached.
        let (hex, streamed) = hash_source_file(&full_path, rel_path)?;

        // TOCTOU guard: `check_source_size` and the streaming read above are
        // two separate passes over the filesystem, so the file could change
        // size in the gap between them. Compare the bytes actually streamed
        // against the same expected size, so a file that changes underneath
        // us mid-validation is still caught rather than silently producing
        // a hash of different content than what the manifest describes.
        if streamed != expected_size {
            return Err(TapectlError::Other(format!(
                "source file size changed: {rel_path} (expected {expected_size}, got {streamed})"
            )));
        }

        checksums.push((rel_path.clone(), hex));

        validated += 1;
        if validated % 100 == 0 {
            info!(
                progress = format!("{validated}/{total_files}"),
                "validating"
            );
        }
    }

    info!(files = validated, "source validation complete");
    Ok(checksums)
}

/// Stat `full_path` and confirm its current size matches `expected_size`
/// (the size recorded in the manifest at snapshot time) — a plain
/// `metadata()` call, no read (H9 remainder, issue #84): fails fast on a
/// missing or already-changed file before any I/O is spent hashing it.
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
            "source file size changed: {rel_path} (expected {expected_size}, got {})",
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
fn hash_source_file(full_path: &Path, rel_path: &str) -> Result<(String, i64)> {
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
        total += n as i64;
    }
    Ok((reader.finalize_hex(), total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::io::Write;
    use tempfile::TempDir;

    fn setup_conn_with_snapshot(files: &[(&str, i64)]) -> (Connection, i64) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        let schema = include_str!("../db/migrations/001_initial.sql");
        conn.execute_batch(schema).unwrap();

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

        for (path, size) in files {
            conn.execute(
                "INSERT INTO files (snapshot_id, path, size_bytes, is_directory)
                 VALUES (?1, ?2, ?3, 0)",
                params![sid, path, size],
            )
            .unwrap();
        }
        (conn, sid)
    }

    #[test]
    fn validate_source_happy_path() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(tmp.path().join("b.bin"), b"world!!").unwrap();

        let (conn, sid) = setup_conn_with_snapshot(&[("a.txt", 5), ("b.bin", 7)]);
        let result = validate_source(&conn, sid, tmp.path().to_str().unwrap()).unwrap();
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
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("present.txt"), b"ok").unwrap();

        let (conn, sid) = setup_conn_with_snapshot(&[("present.txt", 2), ("missing.txt", 10)]);
        let err = validate_source(&conn, sid, tmp.path().to_str().unwrap())
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

        let (conn, sid) = setup_conn_with_snapshot(&[("growing.txt", 3)]);
        let err = validate_source(&conn, sid, tmp.path().to_str().unwrap())
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

    #[test]
    fn validate_source_skips_directories() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();
        std::fs::write(tmp.path().join("subdir/f.txt"), b"x").unwrap();

        // Insert a directory row alongside the file — validate_source
        // must filter it out and not try to read it as a file.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        let schema = include_str!("../db/migrations/001_initial.sql");
        conn.execute_batch(schema).unwrap();
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
        conn.execute(
            "INSERT INTO files (snapshot_id, path, size_bytes, is_directory)
             VALUES (?1, 'subdir', 0, 1)",
            [sid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO files (snapshot_id, path, size_bytes, is_directory)
             VALUES (?1, 'subdir/f.txt', 1, 0)",
            [sid],
        )
        .unwrap();

        let result = validate_source(&conn, sid, tmp.path().to_str().unwrap()).unwrap();
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
}
