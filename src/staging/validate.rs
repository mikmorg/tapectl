use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

use rusqlite::{params, Connection};
use tracing::info;

use crate::error::{Result, TapectlError};
use crate::util::HashingReader;

/// Validate source files by computing SHA256 for all files in the snapshot.
///
/// This is the archival **commitment point** (issue #32/H6; design doc
/// §2.13: "staging is the archival commitment point — full sha256 every
/// time"). The first `stage_create` for a snapshot has no recorded
/// `files.sha256` baseline yet; the hash computed here is what the caller's
/// `backfill_checksums` uses to establish it. Every *later* `stage_create`
/// call for the same `snapshot_id` (a re-stage) already has a baseline, and
/// this function now compares against it instead of blindly recomputing and
/// letting the backfill silently clobber it:
///
///   - no baseline yet                  -> establish it (normal, not an error)
///   - baseline matches                 -> fine
///   - baseline differs, SAME size      -> BITROT suspected: refuse to stage
///   - baseline differs, size DIFFERS   -> DIRTY: a real edit (issue #36's
///     scope, not bitrot — see `check_source_size`)
///
/// Also diffs the on-disk file set against the manifest for NEW files
/// (the issue's own remediation text: "diff walked set vs manifest for
/// NEW/MISSING") — a file dar would silently archive that the catalog has
/// never seen would break the "two stage_sets of one snapshot are logically
/// identical" invariant the finding named. MISSING (a manifest file absent
/// from disk) already errored before this fix, via `check_source_size`'s
/// `metadata()` call below — preserved verbatim.
///
/// Returns `Vec<(relative_path, sha256_hex)>` for every file — the return
/// contract is unchanged (callers depend on it): `stage_create` passes this
/// straight to `backfill_checksums`, whose own `sha256 IS NULL` guard —
/// not any filtering here — is what actually prevents overwriting an
/// existing baseline.
pub fn validate_source(
    conn: &Connection,
    snapshot_id: i64,
    source_path: &str,
) -> Result<Vec<(String, String)>> {
    let base = Path::new(source_path);

    // Get all non-directory entries from the manifest, including any
    // already-established sha256 baseline and each entry's recorded
    // file_type (issue #33/H7: 'regular' / 'symlink' / 'special' — 'dir' is
    // already excluded by the is_directory filter). Fetched unfiltered by
    // type here — NEW detection below needs every non-directory path the
    // manifest knows about, symlink/special included, or a previously
    // staged symlink would falsely reappear as NEW on every re-stage.
    let mut stmt = conn.prepare(
        "SELECT path, size_bytes, sha256, file_type FROM files
         WHERE snapshot_id = ?1 AND is_directory = 0",
    )?;
    let all_entries: Vec<(String, i64, Option<String>, Option<String>)> = stmt
        .query_map(params![snapshot_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    // NEW detection (issue #32/H6): diff the walked on-disk set against the
    // manifest's path set, before touching any file's bytes. Reuses
    // `staging::walk_directory`'s exact rel_path derivation (strip_prefix +
    // to_string_lossy, follow_links(false), is_dir filter) via `super`
    // rather than a second walk implementation that could silently
    // disagree with what `snapshot_create` itself considered "the files" —
    // this is an extra O(file count) directory walk every stage, accepted
    // for the walk-parity guarantee.
    let (_, _, disk_entries) = super::walk_directory(source_path)?;
    let manifest_paths: HashSet<&str> = all_entries.iter().map(|(p, ..)| p.as_str()).collect();
    let mut new_files: Vec<&str> = disk_entries
        .iter()
        .filter(|e| !e.is_dir && !manifest_paths.contains(e.path.as_str()))
        .map(|e| e.path.as_str())
        .collect();
    new_files.sort_unstable();
    if !new_files.is_empty() {
        // WARN, not error — deliberately weaker than BITROT/MISSING/DIRTY.
        //
        // Two reasons. (1) The design doc (§2.13) files NEW as a
        // `check-integrity` REPORT status, not a stage-time gate; hard-failing
        // here would be tapectl inventing a stricter contract than its own spec.
        // (2) More decisively, this check has a KNOWN false-positive source:
        // `walk_directory` applies no exclusion filtering at all, while
        // `stage_create` passes `global_excludes` to dar. So an incidentally
        // excluded file appearing between `snapshot create` and `stage create`
        // — an editor swap file, .DS_Store, a cache entry — would block staging
        // outright for content dar was never going to archive.
        //
        // A gate whose false positives halt legitimate work is worse than no
        // gate: operators learn to bypass it. The real cost of NEW is a catalog
        // that under-reports a file dar did archive — a wart, not data loss.
        //
        // Upgrade this to a hard error once #49 wires excludes through
        // `walk_directory`, at which point the false-positive source is gone.
        tracing::warn!(
            files = %new_files.join(", "),
            "NEW: file(s) on disk are absent from the snapshot manifest; dar will \
             archive content the catalog has not recorded. Re-run `snapshot create` \
             before staging if these should be catalogued (design §2.13; hard-fails \
             once #49 makes exclusion-aware detection possible)"
        );
    }

    // Content validation (size check + sha256) applies to regular files
    // only (issue #33/H7). Symlinks have no content of their own to
    // checksum — their recorded `size_bytes` is lstat's target-string
    // length, not payload, and comparing it against the *followed* size
    // (as `check_source_size` does) produced a false DIRTY whenever the two
    // happened to differ. Special files (FIFO/socket/device) are worse:
    // `hash_source_file` opening a FIFO with no writer blocks forever. Both
    // are recorded in the manifest (so restore and `catalog ls` still see
    // them) but are excluded here, before any file in this set is ever
    // stat'd or opened.
    let files: Vec<(String, i64, Option<String>)> = all_entries
        .into_iter()
        .filter(|(_, _, _, file_type)| file_type.as_deref() == Some("regular"))
        .map(|(path, size, sha, _)| (path, size, sha))
        .collect();

    let total_files = files.len();
    let total_bytes: i64 = files.iter().map(|(_, s, _)| s).sum();
    info!(
        files = total_files,
        total_mb = total_bytes / (1024 * 1024),
        "validating source checksums"
    );

    let mut checksums = Vec::new();
    let mut validated = 0;

    for (rel_path, expected_size, baseline_sha) in &files {
        let full_path = base.join(rel_path);
        let expected_size = *expected_size;

        // Size check needs no read at all (H9 remainder, issue #84): a
        // metadata() stat is instant and fails fast on a missing or
        // changed file before any I/O is spent hashing it — replaces the
        // `exists()` + whole-file `std::fs::read` this check used to ride
        // along with. Also doubles as the MISSING and DIRTY-by-size
        // classifications (issue #32) — see its doc comment.
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

        // Commitment-point comparison (issue #32/H6): `baseline_sha` is
        // `files.sha256` as recorded by the last successful backfill (NULL
        // if this snapshot has never been staged before). By this point
        // `check_source_size` and the streamed byte count have both
        // already proven the current on-disk size equals the manifest's
        // recorded size, so a hash mismatch here is content drift at a
        // CONSTANT size — bitrot, not an edit.
        match baseline_sha.as_deref() {
            None => {
                // Commitment point: nothing to compare against yet. Normal,
                // not an error — the caller's guarded backfill establishes
                // the baseline from the hash pushed below.
            }
            Some(baseline) if baseline == hex.as_str() => {
                // Matches — fine.
            }
            Some(baseline) => {
                // BITROT suspected. Per the steer for #32: refuse to stage
                // rather than archive content already suspected corrupt
                // over a baseline that proves it — and never let this
                // hash reach `backfill_checksums` (moot here since
                // returning Err discards `checksums` entirely, but the
                // SQL guard in `backfill_checksums` protects this
                // independent of that).
                return Err(TapectlError::Other(format!(
                    "BITROT suspected: {rel_path} — sha256 differs at an unchanged \
                     size ({expected_size} bytes): baseline={baseline}, current={hex}. \
                     Refusing to stage (see #32); investigate before re-staging."
                )));
            }
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
             found {} bytes) — a real edit (size and content both differ); tracked \
             separately under issue #36",
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
    let meta = std::fs::symlink_metadata(full_path)
        .map_err(|e| TapectlError::Other(format!("cannot stat source file: {rel_path} ({e})")))?;
    if !meta.is_file() {
        return Err(TapectlError::Other(format!(
            "refusing to read non-regular file: {rel_path} — symlinks/FIFOs/sockets/devices \
             are never content-validated (issue #33)"
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

    /// `files` is `(path, size_bytes, sha256_baseline)` — the third element
    /// seeds `files.sha256`/`manifest_entries.sha256` as they'd stand after
    /// a *previous* successful stage (issue #32/H6): `None` simulates a
    /// snapshot that has never been staged (no baseline yet — the
    /// commitment point hasn't happened); `Some(hex)` simulates a re-stage
    /// with an already-established baseline to compare against.
    fn setup_conn_with_snapshot(files: &[(&str, i64, Option<&str>)]) -> (Connection, i64) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        let schema = include_str!("../db/migrations/001_initial.sql");
        conn.execute_batch(schema).unwrap();
        // 005 adds file_type/link_target (issue #33/H7) — applied directly
        // on top of 001 alone (it only touches files/manifest_entries,
        // both already defined there), matching this helper's existing
        // lightweight-schema convention rather than pulling in 002-004.
        conn.execute_batch(include_str!("../db/migrations/005_file_types.sql"))
            .unwrap();

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

        conn.execute("INSERT INTO manifests (snapshot_id) VALUES (?1)", [sid])
            .unwrap();
        let mid = conn.last_insert_rowid();

        for (path, size, sha) in files {
            // file_type = 'regular' unconditionally: every existing caller of
            // this helper plants a genuine regular-file scenario. Symlink/
            // special rows for the issue #33/H7 tests go through
            // `insert_nonregular_file` instead, which takes file_type
            // explicitly.
            conn.execute(
                "INSERT INTO files (snapshot_id, path, size_bytes, sha256, is_directory, file_type)
                 VALUES (?1, ?2, ?3, ?4, 0, 'regular')",
                params![sid, path, size, sha],
            )
            .unwrap();
            // Mirrors the production backfill target: `manifest_entries`
            // carries the same baseline and the same never-overwrite
            // guarantee `backfill_checksums` must provide.
            conn.execute(
                "INSERT INTO manifest_entries
                     (manifest_id, path, size_bytes, mtime, sha256, is_directory, file_type)
                 VALUES (?1, ?2, ?3, '2026-01-01T00:00:00Z', ?4, 0, 'regular')",
                params![mid, path, size, sha],
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

        let (conn, sid) = setup_conn_with_snapshot(&[("a.txt", 5, None), ("b.bin", 7, None)]);
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
        // MISSING classification (issue #32): a manifest file absent from
        // disk. This already errored before #32 — preserved verbatim.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("present.txt"), b"ok").unwrap();

        let (conn, sid) =
            setup_conn_with_snapshot(&[("present.txt", 2, None), ("missing.txt", 10, None)]);
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

        let (conn, sid) = setup_conn_with_snapshot(&[("growing.txt", 3, None)]);
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
        let checksums = validate_source(&conn, sid, tmp.path().to_str().unwrap()).unwrap();
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
        let stored: Option<String> = conn
            .query_row(
                "SELECT sha256 FROM files WHERE snapshot_id = ?1 AND path = 'a.txt'",
                params![sid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored.as_deref(), Some(hex.as_str()));
    }

    #[test]
    fn restage_with_unchanged_content_passes_and_baseline_is_untouched() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        let baseline = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

        let (conn, sid) = setup_conn_with_snapshot(&[("a.txt", 5, Some(baseline))]);
        let checksums = validate_source(&conn, sid, tmp.path().to_str().unwrap()).unwrap();
        assert_eq!(checksums[0].1, baseline);

        crate::staging::backfill_checksums(&conn, sid, &checksums).unwrap();
        let stored: Option<String> = conn
            .query_row(
                "SELECT sha256 FROM files WHERE snapshot_id = ?1 AND path = 'a.txt'",
                params![sid],
                |row| row.get(0),
            )
            .unwrap();
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
        let err = validate_source(&conn, sid, tmp.path().to_str().unwrap())
            .err()
            .unwrap();
        let msg = format!("{err}");

        assert!(msg.contains("BITROT"), "must name it BITROT, got: {msg}");
        assert!(msg.contains("a.txt"), "must name the file, got: {msg}");
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
        let stored: Option<String> = conn
            .query_row(
                "SELECT sha256 FROM files WHERE snapshot_id = ?1 AND path = 'a.txt'",
                params![sid],
                |row| row.get(0),
            )
            .unwrap();
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
        let err = validate_source(&conn, sid, tmp.path().to_str().unwrap())
            .err()
            .unwrap();
        let msg = format!("{err}");

        assert!(msg.contains("DIRTY"), "must name it DIRTY, got: {msg}");
        assert!(
            !msg.contains("BITROT"),
            "dirty and bitrot must be mutually exclusive outcomes, got: {msg}"
        );
        assert!(msg.contains("a.txt"), "must name the file, got: {msg}");
    }

    #[test]
    fn new_file_on_disk_not_in_manifest_warns_but_does_not_refuse() {
        // NEW is a WARNING, not a gate — see the rationale at the check itself.
        // The design doc (§2.13) files NEW as a check-integrity report status,
        // and this detection has a known false-positive source (walk_directory
        // applies no excludes, dar does), so blocking staging on it would halt
        // legitimate work over files dar was never going to archive.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(tmp.path().join("stray.tmp"), b"appeared after snapshot").unwrap();
        let baseline = direct_hash(b"hello");
        let (conn, sid) = setup_conn_with_snapshot(&[("a.txt", 5, Some(baseline.as_str()))]);

        let out = validate_source(&conn, sid, tmp.path().to_str().unwrap());
        assert!(
            out.is_ok(),
            "a NEW file must warn, not refuse staging: {out:?}"
        );
        let checksums = out.unwrap();
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
        let (conn, sid) = setup_conn_with_snapshot(&[("a.txt", 5, Some("original00baseline"))]);

        crate::staging::backfill_checksums(
            &conn,
            sid,
            &[("a.txt".to_string(), "attemptedoverwrite".to_string())],
        )
        .unwrap();

        let stored: Option<String> = conn
            .query_row(
                "SELECT sha256 FROM files WHERE snapshot_id = ?1 AND path = 'a.txt'",
                params![sid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stored.as_deref(),
            Some("original00baseline"),
            "backfill must never overwrite an existing files.sha256 baseline"
        );

        let stored_manifest: Option<String> = conn
            .query_row(
                "SELECT me.sha256 FROM manifest_entries me
                 JOIN manifests m ON m.id = me.manifest_id
                 WHERE m.snapshot_id = ?1 AND me.path = 'a.txt'",
                params![sid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stored_manifest.as_deref(),
            Some("original00baseline"),
            "backfill must never overwrite an existing manifest_entries.sha256 baseline"
        );
    }

    #[test]
    fn backfill_checksums_establishes_a_baseline_when_absent() {
        let (conn, sid) = setup_conn_with_snapshot(&[("a.txt", 5, None)]);
        crate::staging::backfill_checksums(
            &conn,
            sid,
            &[("a.txt".to_string(), "freshbaseline".to_string())],
        )
        .unwrap();

        let stored: Option<String> = conn
            .query_row(
                "SELECT sha256 FROM files WHERE snapshot_id = ?1 AND path = 'a.txt'",
                params![sid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored.as_deref(), Some("freshbaseline"));
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
        conn.execute_batch(include_str!("../db/migrations/005_file_types.sql"))
            .unwrap();
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
            "INSERT INTO files (snapshot_id, path, size_bytes, is_directory, file_type)
             VALUES (?1, 'subdir', 0, 1, 'dir')",
            [sid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO files (snapshot_id, path, size_bytes, is_directory, file_type)
             VALUES (?1, 'subdir/f.txt', 1, 0, 'regular')",
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

    /// Plants one additional non-regular `files`/`manifest_entries` row
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
        conn.execute(
            "INSERT INTO files (snapshot_id, path, size_bytes, is_directory, file_type, link_target)
             VALUES (?1, ?2, ?3, 0, ?4, ?5)",
            params![snapshot_id, path, size, file_type, link_target],
        )
        .unwrap();
        let manifest_id: i64 = conn
            .query_row(
                "SELECT id FROM manifests WHERE snapshot_id = ?1",
                params![snapshot_id],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO manifest_entries
                 (manifest_id, path, size_bytes, mtime, is_directory, file_type, link_target)
             VALUES (?1, ?2, ?3, '2026-01-01T00:00:00Z', 0, ?4, ?5)",
            params![manifest_id, path, size, file_type, link_target],
        )
        .unwrap();
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

        let result = validate_source(&conn, sid, tmp.path().to_str().unwrap());
        assert!(
            result.is_ok(),
            "a mismatched-length symlink must not produce a false DIRTY: {result:?}"
        );
        let checksums = result.unwrap();
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

        let result = validate_source(&conn, sid, tmp.path().to_str().unwrap());
        assert!(
            result.is_ok(),
            "a broken symlink must not error at all — excluded from validation: {result:?}"
        );
        let checksums = result.unwrap();
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

        let checksums = validate_source(&conn, sid, tmp.path().to_str().unwrap()).unwrap();
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
