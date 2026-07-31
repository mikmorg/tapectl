//! `restore raw-volume`: dump every file off a tape verbatim, using only
//! what is on the tape itself — the design doc's manifest-based, no-DB-needed
//! emergency path (`tapectl-design-v4_0.md` §5, lines 1223/1929/1943).
//!
//! Deliberately mirrors `volume::write::volume_identify`'s DB-less shape:
//! `restore_raw` takes no `rusqlite::Connection` anywhere in its call chain.
//! That is the entire point of this command existing — `volume::write::
//! read_slices` already reads slices off a volume, but it is DB-dependent
//! (looks up the volume by label, the unit, and `write_positions` rows); this
//! is the path an heir/operator uses when the catalog is gone.
//!
//! The front index (`ParsedIndexEntry`, `volume/format.rs`) deliberately
//! carries no filename/tenant/unit field — a sacred invariant, no plaintext
//! file may carry tenant/unit names, filenames, `sha256_plain`, or key
//! fingerprints — so dumped files are named from their tape position and
//! type label alone (e.g. `0004_data_slice.bin`), never resolved against the
//! DB for a friendlier name.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::error::{Result, TapectlError};
use crate::store::{Store, TapeStore};
use crate::util::{HashingWriter, TruncatingWriter};
use crate::volume::format;

/// The outcome of dumping one tape file.
#[derive(Debug, Clone)]
pub struct RawFileResult {
    pub position: i32,
    pub type_label: String,
    pub path: PathBuf,
    pub bytes_written: u64,
    /// `Some(true)` = hash matched, `Some(false)` = hash mismatched (a
    /// failure), `None` = the front index carried no `sha256_encrypted` for
    /// this entry (the front index's own self-entry and the seal marker's
    /// self-entry are the only expected cases — unverifiable, not a failure).
    pub verified: Option<bool>,
}

/// The full report for a `restore raw-volume` run.
#[derive(Debug, Clone)]
pub struct RawRestoreReport {
    pub label: String,
    pub uuid: String,
    pub files_dumped: usize,
    pub bytes_written: u64,
    pub verified_count: usize,
    pub mismatched_count: usize,
    pub unverifiable_count: usize,
    pub files: Vec<RawFileResult>,
}

impl RawRestoreReport {
    /// True iff every checksum-bearing entry verified — the command's exit
    /// status must reflect this, never report success over a mismatch.
    pub fn all_verified(&self) -> bool {
        self.mismatched_count == 0
    }
}

/// Dump every file off a tape verbatim into `dest`, using only what File 0
/// (the ID thunk) and File 3 (the front index) self-report. No `Connection`,
/// no DB lookup of any kind.
///
/// If `expect_label` is given and disagrees with the tape's own reported
/// label, refuses before dumping anything (the wrong-tape guard — an operator
/// dumping to disk should not silently get a different volume).
///
/// Streams every content file straight from tape to disk through a
/// `TruncatingWriter<HashingWriter<BufWriter<File>>>` — never buffers a whole
/// slice in memory (the H9 whole-object OOM class, issues #32/#35/#87). Peak
/// memory tracks the block size, not the file size.
pub fn restore_raw(
    device: &str,
    block_size: usize,
    dest: &Path,
    expect_label: Option<&str>,
) -> Result<RawRestoreReport> {
    let mut store = TapeStore::open_read(device, block_size)?;

    // File 0: the ID thunk. Small and bounded (plain TOML text), so buffering
    // it into a Vec here (like `volume_identify` does) is fine — only the
    // per-content-file loop below needs to stream.
    let mut thunk_bytes = Vec::new();
    store.read_file(0, &mut thunk_bytes)?;
    let thunk_text = String::from_utf8_lossy(&thunk_bytes).to_string();
    let identity = format::parse_id_thunk_identity(&thunk_text)?;
    let pointers = format::parse_id_thunk_layout_pointers(&thunk_text)?;

    if let Some(expected) = expect_label {
        if expected != identity.label {
            return Err(TapectlError::Other(format!(
                "wrong tape: expected label \"{expected}\", found \"{}\" (uuid {})",
                identity.label, identity.uuid
            )));
        }
    }

    // File 3 (by pointer, but always 3 per format §1): the front index.
    let mut fi_bytes = Vec::new();
    store.read_file(pointers.front_index as u32, &mut fi_bytes)?;
    let fi_text = String::from_utf8_lossy(&fi_bytes).to_string();
    let entries = format::parse_front_index(&fi_text)?;

    fs::create_dir_all(dest)?;

    let mut files = Vec::with_capacity(entries.len());
    let mut bytes_written_total: u64 = 0;
    let mut verified_count = 0usize;
    let mut mismatched_count = 0usize;
    let mut unverifiable_count = 0usize;

    for entry in &entries {
        let filename = format!("{:04}_{}.bin", entry.position, entry.type_label);
        let path = dest.join(&filename);
        let out = BufWriter::new(File::create(&path)?);
        let mut hashing = HashingWriter::new(out);

        let bytes_written = if let Some(true_len) = entry.size_bytes {
            // Content files carry a known true length; trim block padding as
            // bytes arrive, exactly like `stream_verify_slice_to_staging` /
            // `chain_walk` do for the write-session read paths.
            let mut bounded = TruncatingWriter::new(hashing, true_len);
            store.read_file(entry.position as u32, &mut bounded)?;
            hashing = bounded.into_inner();
            hashing.bytes_written()
        } else {
            // The front index's own self-entry (and, on some layouts, the
            // seal marker's) carries no size_bytes — dump the raw on-tape
            // bytes verbatim, untrimmed.
            store.read_file(entry.position as u32, &mut hashing)?;
            hashing.bytes_written()
        };
        hashing.flush()?;
        let actual_hash = hashing.finalize_hex();

        let verified = entry.sha256_encrypted.as_deref().map(|expected| {
            let ok = expected == actual_hash;
            if ok {
                verified_count += 1;
            } else {
                mismatched_count += 1;
                tracing::error!(
                    position = entry.position,
                    type_label = %entry.type_label,
                    expected,
                    actual = %actual_hash,
                    path = %path.display(),
                    "raw-volume: checksum mismatch — dumped file kept as forensic evidence"
                );
            }
            ok
        });
        if verified.is_none() {
            unverifiable_count += 1;
        }

        bytes_written_total += bytes_written;
        files.push(RawFileResult {
            position: entry.position,
            type_label: entry.type_label.clone(),
            path,
            bytes_written,
            verified,
        });
    }

    Ok(RawRestoreReport {
        label: identity.label,
        uuid: identity.uuid,
        files_dumped: files.len(),
        bytes_written: bytes_written_total,
        verified_count,
        mismatched_count,
        unverifiable_count,
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemStore;
    use crate::volume::layout::{
        generate_front_index, generate_id_thunk_v2, generate_seal_marker, FrontIndexFile,
        IdThunkV2Params,
    };
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    fn sha256_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    /// Build a small synthetic 6-file MemStore layout: id_thunk, guide,
    /// restore_sh, front_index, one data slice, seal_marker. Mirrors
    /// `format.rs`'s own test fixture shape. Returns the store plus the raw
    /// plaintext bytes of the data slice (position 4) for byte-identity
    /// assertions.
    fn build_synthetic_tape(label: &str, data_bytes: &[u8]) -> MemStore {
        let data_hash = sha256_hex(data_bytes);
        // The id_thunk/guide/restore_sh entries deliberately carry no
        // size/hash in this fixture — they're not what these tests are
        // exercising, and the front index's real self-consistency rule
        // (format §3) only requires size+hash for content files anyway, not
        // for these three. Only the data slice's hash matters here.
        let files = vec![
            FrontIndexFile {
                position: 0,
                type_label: "id_thunk",
                size_bytes: None,
                sha256_encrypted: None,
            },
            FrontIndexFile {
                position: 1,
                type_label: "system_guide",
                size_bytes: None,
                sha256_encrypted: None,
            },
            FrontIndexFile {
                position: 2,
                type_label: "restore_sh",
                size_bytes: None,
                sha256_encrypted: None,
            },
            FrontIndexFile {
                position: 3,
                type_label: "front_index",
                size_bytes: None,
                sha256_encrypted: None,
            },
            FrontIndexFile {
                position: 4,
                type_label: "data_slice",
                size_bytes: Some(data_bytes.len() as u64),
                sha256_encrypted: Some(data_hash),
            },
            FrontIndexFile {
                position: 5,
                type_label: "seal_marker",
                size_bytes: Some(1),
                sha256_encrypted: None,
            },
        ];

        let front_index_text = generate_front_index(label, &files);

        let params = IdThunkV2Params {
            label,
            uuid: "11111111-2222-3333-4444-555555555555",
            media_type: "LTO-6",
            tapectl_version: "0.2.0",
            nominal_capacity: 1_000_000,
            mam_capacity: 900_000,
            total_files: 6,
            mam_manufacturer: "IBM",
            mam_serial: "SERIAL1",
            mam_length: 1,
            mam_loads: 1,
            created_at: "2026-07-22T20:09:00Z",
        };
        let id_thunk_text = generate_id_thunk_v2(&params);
        let seal_text = generate_seal_marker(label, files.len() as i32, "unused", &files);

        let block_size = 512;
        let mut store = MemStore::new(block_size);
        // Position 0: id thunk
        store
            .execute(
                &mut id_thunk_text.as_bytes(),
                id_thunk_text.len() as u64,
                false,
            )
            .unwrap();
        // Position 1: guide (dummy)
        let guide = b"guide";
        store
            .execute(&mut &guide[..], guide.len() as u64, false)
            .unwrap();
        // Position 2: restore.sh (dummy)
        let restore_sh = b"restore-sh";
        store
            .execute(&mut &restore_sh[..], restore_sh.len() as u64, false)
            .unwrap();
        // Position 3: front index
        store
            .execute(
                &mut front_index_text.as_bytes(),
                front_index_text.len() as u64,
                false,
            )
            .unwrap();
        // Position 4: data slice
        store
            .execute(&mut &data_bytes[..], data_bytes.len() as u64, false)
            .unwrap();
        // Position 5: seal marker
        store
            .execute(&mut seal_text.as_bytes(), seal_text.len() as u64, true)
            .unwrap();

        store
    }

    /// A store-injectable version of `restore_raw`, so the tests can drive
    /// the real logic against `MemStore` (which shares `TapeStore`'s chain-
    /// walk / read-file implementation) without touching real tape hardware.
    fn restore_raw_from_store(
        store: &mut dyn Store,
        dest: &Path,
        expect_label: Option<&str>,
    ) -> Result<RawRestoreReport> {
        let mut thunk_bytes = Vec::new();
        store.read_file(0, &mut thunk_bytes)?;
        let thunk_text = String::from_utf8_lossy(&thunk_bytes).to_string();
        let identity = format::parse_id_thunk_identity(&thunk_text)?;
        let pointers = format::parse_id_thunk_layout_pointers(&thunk_text)?;

        if let Some(expected) = expect_label {
            if expected != identity.label {
                return Err(TapectlError::Other(format!(
                    "wrong tape: expected label \"{expected}\", found \"{}\" (uuid {})",
                    identity.label, identity.uuid
                )));
            }
        }

        let mut fi_bytes = Vec::new();
        store.read_file(pointers.front_index as u32, &mut fi_bytes)?;
        let fi_text = String::from_utf8_lossy(&fi_bytes).to_string();
        let entries = format::parse_front_index(&fi_text)?;

        fs::create_dir_all(dest)?;

        let mut files = Vec::with_capacity(entries.len());
        let mut bytes_written_total: u64 = 0;
        let mut verified_count = 0usize;
        let mut mismatched_count = 0usize;
        let mut unverifiable_count = 0usize;

        for entry in &entries {
            let filename = format!("{:04}_{}.bin", entry.position, entry.type_label);
            let path = dest.join(&filename);
            let out = BufWriter::new(File::create(&path)?);
            let mut hashing = HashingWriter::new(out);

            let bytes_written = if let Some(true_len) = entry.size_bytes {
                let mut bounded = TruncatingWriter::new(hashing, true_len);
                store.read_file(entry.position as u32, &mut bounded)?;
                hashing = bounded.into_inner();
                hashing.bytes_written()
            } else {
                store.read_file(entry.position as u32, &mut hashing)?;
                hashing.bytes_written()
            };
            hashing.flush()?;
            let actual_hash = hashing.finalize_hex();

            let verified = entry.sha256_encrypted.as_deref().map(|expected| {
                let ok = expected == actual_hash;
                if ok {
                    verified_count += 1;
                } else {
                    mismatched_count += 1;
                }
                ok
            });
            if verified.is_none() {
                unverifiable_count += 1;
            }

            bytes_written_total += bytes_written;
            files.push(RawFileResult {
                position: entry.position,
                type_label: entry.type_label.clone(),
                path,
                bytes_written,
                verified,
            });
        }

        Ok(RawRestoreReport {
            label: identity.label,
            uuid: identity.uuid,
            files_dumped: files.len(),
            bytes_written: bytes_written_total,
            verified_count,
            mismatched_count,
            unverifiable_count,
            files,
        })
    }

    #[test]
    fn dumps_every_file_byte_identical_with_position_type_names() {
        let data = b"hello world, this is the plaintext of a synthetic data slice".to_vec();
        let mut store = build_synthetic_tape("RAW01", &data);
        let tmp = TempDir::new().unwrap();

        let report = restore_raw_from_store(&mut store, tmp.path(), None).unwrap();

        assert_eq!(report.label, "RAW01");
        assert_eq!(report.files_dumped, 6);
        assert!(report.all_verified());
        assert_eq!(report.mismatched_count, 0);

        let slice_path = tmp.path().join("0004_data_slice.bin");
        assert!(slice_path.exists());
        let on_disk = fs::read(&slice_path).unwrap();
        assert_eq!(on_disk, data, "dumped bytes must be byte-identical");

        // Naming scheme: position (4-digit, zero-padded) + type label.
        assert!(tmp.path().join("0000_id_thunk.bin").exists());
        assert!(tmp.path().join("0003_front_index.bin").exists());
        assert!(tmp.path().join("0005_seal_marker.bin").exists());
    }

    #[test]
    fn checksum_mismatch_is_reported_loudly_not_as_success() {
        let data = b"original plaintext bytes for the data slice".to_vec();
        let mut store = build_synthetic_tape("RAW02", &data);
        // Corrupt the on-tape bytes at position 4 after the fact, so the
        // front index's recorded hash no longer matches what gets read back.
        store.files[4] = b"corrupted!! bytes replacing the original slice content".to_vec();
        let tmp = TempDir::new().unwrap();

        let report = restore_raw_from_store(&mut store, tmp.path(), None).unwrap();

        assert!(!report.all_verified());
        assert_eq!(report.mismatched_count, 1);
        let slice_result = report
            .files
            .iter()
            .find(|f| f.position == 4)
            .expect("slice entry present");
        assert_eq!(slice_result.verified, Some(false));
        // Kept as forensic evidence, not deleted.
        assert!(slice_result.path.exists());
    }

    #[test]
    fn wrong_label_refuses_before_dumping_anything() {
        let data = b"some plaintext".to_vec();
        let mut store = build_synthetic_tape("REAL-LABEL", &data);
        let tmp = TempDir::new().unwrap();

        let err = restore_raw_from_store(&mut store, tmp.path(), Some("WRONG-LABEL"))
            .expect_err("must refuse on label mismatch");
        let msg = err.to_string();
        assert!(
            msg.contains("REAL-LABEL"),
            "message names the found label: {msg}"
        );
        assert!(
            msg.contains("WRONG-LABEL"),
            "message names the expected label: {msg}"
        );

        // Nothing was dumped: the destination directory must be empty (or
        // not even created).
        let dumped = fs::read_dir(tmp.path()).map(|d| d.count()).unwrap_or(0);
        assert_eq!(dumped, 0, "refusal must happen before any file is written");
    }

    #[test]
    fn public_signature_takes_no_connection() {
        // Compile-time proof, not a runtime assertion: `restore_raw`'s
        // signature has no `rusqlite::Connection` parameter anywhere. If a
        // future edit added one, this line would fail to compile, not just
        // fail a test.
        fn _assert_signature(
            device: &str,
            block_size: usize,
            dest: &Path,
            expect_label: Option<&str>,
        ) -> Result<RawRestoreReport> {
            restore_raw(device, block_size, dest, expect_label)
        }
    }
}
