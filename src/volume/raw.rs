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
use crate::store::Store;
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
    /// `Some(K)` when the tape sealed short (#420): its seal, at K+1, says
    /// the tape ended after file K (`volume-format-v2.md` §4.2).
    pub ended_after: Option<u32>,
    /// The planned files after `ended_after` the selection named, as
    /// `(position, front-index type)`: not on this tape, so not read and not
    /// dumped (R3). Not a failure.
    pub not_on_tape: Vec<(i32, String)>,
}

/// What `restore raw-volume` says of a tape that ended early (#420): where,
/// and each planned file it did not read, as RESTORE.sh's `--verify` says it
/// (`NOT ON TAPE`, not a failure). Empty for any other tape.
pub fn ending_lines(report: &RawRestoreReport) -> Vec<String> {
    let Some(k) = report.ended_after else {
        return Vec::new();
    };
    let mut lines = vec![format!(
        "  the tape ended early: its seal marker, file {}, says it ended after file {k}; \
         the {} planned file(s) after that are not on it (not a failure)",
        k + 1,
        report.not_on_tape.len()
    )];
    lines.extend(
        report
            .not_on_tape
            .iter()
            .map(|(position, kind)| format!("  NOT ON TAPE  file {position:>3}  {kind}")),
    );
    lines
}

impl RawRestoreReport {
    /// True iff every checksum-bearing entry verified — the command's exit
    /// status must reflect this, never report success over a mismatch.
    pub fn all_verified(&self) -> bool {
        self.mismatched_count == 0
    }
}

/// Which files a `restore raw-volume` dumps (issue #417). Empty = every
/// file, the command's original behaviour; otherwise a file is dumped when
/// its position is listed OR its front-index type is (a union, so `--only
/// restore_sh --positions 5` takes both). A full dump of a full cartridge is
/// a terabyte; the heir who wanted RESTORE.sh needed 50 KB of it.
#[derive(Debug, Clone, Default)]
pub struct RawSelection {
    pub positions: Vec<i32>,
    pub types: Vec<String>,
}

/// The type labels a front index uses (`layout_model::ZoneKind::type_label`),
/// the closed set `--only` accepts.
pub const RAW_TYPES: &[&str] = &[
    "id_thunk",
    "system_guide",
    "restore_sh",
    "front_index",
    "tenant_envelope",
    "operator_envelope",
    "operator_envelope_backup",
    "tenant_envelope_backup",
    "data_slice",
    "seal_marker",
];

impl RawSelection {
    /// Every file.
    pub fn all() -> Self {
        Self::default()
    }

    fn is_all(&self) -> bool {
        self.positions.is_empty() && self.types.is_empty()
    }

    fn selects(&self, entry: &format::ParsedIndexEntry) -> bool {
        self.is_all()
            || self.positions.contains(&entry.position)
            || self.types.contains(&entry.type_label)
    }

    /// Refuse a type outside [`RAW_TYPES`] by name, before the tape is
    /// touched: a misspelt `--only` would otherwise dump nothing and pass.
    pub fn check_types(&self) -> Result<()> {
        for t in &self.types {
            if crate::volume::layout_model::ZoneKind::from_type_label(t).is_none() {
                return Err(TapectlError::Other(format!(
                    "--only {t}: not a file type a front index uses; one of {}",
                    RAW_TYPES.join(", ")
                )));
            }
        }
        Ok(())
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
/// memory is a fixed number of blocks — `TapeStore`'s read queue
/// (`pipeline::QUEUE_BYTES`, issue #390) — never the file size.
///
/// Takes an already-open `store` (ADR-0006) rather than a device path — the
/// caller opens `TapeStore::open_read` (or, in tests, hands in a `MemStore`
/// built by [`tests::build_synthetic_tape`]), so this function's own logic is
/// the test surface directly, with no private twin re-implementing the walk.
pub fn restore_raw(
    store: &mut dyn Store,
    dest: &Path,
    expect_label: Option<&str>,
) -> Result<RawRestoreReport> {
    restore_raw_selected(store, dest, expect_label, &RawSelection::all())
}

/// [`restore_raw`], dumping only the files `selection` names (issue #417).
/// A listed position the front index does not have is refused after File 3
/// is read and before anything is written.
pub fn restore_raw_selected(
    store: &mut dyn Store,
    dest: &Path,
    expect_label: Option<&str>,
    selection: &RawSelection,
) -> Result<RawRestoreReport> {
    selection.check_types()?;
    // File 0: the ID thunk. Small and bounded (plain TOML text), so buffering
    // it into a Vec here (like `volume_identify` does) is fine — only the
    // per-content-file loop below needs to stream.
    // Bounded (issue #400): read no further than `SMALL_FILE_CAP`.
    let thunk_bytes = crate::store::read_small_bytes(store, 0, "ID thunk")?;
    let thunk_text = String::from_utf8_lossy(&thunk_bytes).to_string();
    // Before anything else of it is interpreted (ADR-0012 item 15).
    format::check_id_thunk_readable(&thunk_text)?;
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
    let fi_bytes =
        crate::store::read_small_bytes(store, pointers.front_index as u32, "front index")?;
    let fi_text = String::from_utf8_lossy(&fi_bytes).to_string();
    let entries = format::parse_front_index(&fi_text)?;
    for p in &selection.positions {
        if !entries.iter().any(|e| e.position == *p) {
            return Err(TapectlError::Other(format!(
                "--positions {p}: this tape's front index has no file at position {p} \
                 (it lists {} files, positions {}..={})",
                entries.len(),
                entries.first().map_or(0, |e| e.position),
                entries.last().map_or(0, |e| e.position),
            )));
        }
    }

    // #420 (`volume-format-v2.md` §4.2 R1): on a tape that may end early,
    // where it ended is learnt from its seal before any data slice is read,
    // and nothing after that is read (R3). File 3 is the plan; the seal,
    // found by §1.2 rule 6, says which of its files are on the tape. Only a
    // selection that reaches the slices needs it: every file before the
    // first data slice is on every sealed tape (§4.3 W4).
    let ending = end_of_tape(
        store,
        &thunk_text,
        &fi_bytes,
        &pointers,
        &entries,
        selection,
    )?;
    let (entries, not_on_tape) = match ending {
        None => (entries, Vec::new()),
        Some(k) => {
            let (on, off): (Vec<_>, Vec<_>) = entries
                .into_iter()
                .partition(|e| i64::from(e.position) <= i64::from(k));
            // The seal itself lies at K+1, where the plan put a data slice:
            // it is dumped as the seal marker it is, unverifiable like every
            // seal's own entry.
            let seal = format::ParsedIndexEntry {
                position: (k + 1) as i32,
                type_label: "seal_marker".to_string(),
                size_bytes: None,
                sha256_encrypted: None,
            };
            for p in &selection.positions {
                if i64::from(*p) > i64::from(k) + 1 {
                    return Err(TapectlError::Other(format!(
                        "--positions {p}: this tape ended early, after file {k} (its seal \
                         marker, file {}, says so): file {p} of its plan is not on it. \
                         Nothing was dumped",
                        k + 1
                    )));
                }
            }
            let mut on = on;
            on.push(seal);
            let not_on_tape = off
                .into_iter()
                .filter(|e| selection.selects(e))
                .map(|e| (e.position, e.type_label))
                .collect();
            (on, not_on_tape)
        }
    };
    let entries: Vec<_> = entries
        .into_iter()
        .filter(|e| selection.selects(e))
        .collect();

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
        ended_after: ending,
        not_on_tape,
    })
}

/// Where a tape that may end early ended (#420): `Some(K)` when its seal,
/// found by `volume-format-v2.md` §1.2 rule 6 and accepted there, is a short
/// one; `None` for a full seal, for a tape that does not require
/// `short-seal` (it cannot end early, and its seal is read in the dump's own
/// pass, as before 1.6.0), and for a selection that reads no data slice. A
/// seal this tapectl must not interpret refuses the dump by name (§1.2
/// rule 1). With no seal it accepts, the dump reads on as for an unsealed
/// tape (R2), as before 1.6.0.
fn end_of_tape(
    store: &mut dyn Store,
    thunk_text: &str,
    fi_bytes: &[u8],
    pointers: &format::IdThunkLayoutPointers,
    entries: &[format::ParsedIndexEntry],
    selection: &RawSelection,
) -> Result<Option<u32>> {
    let may_end_early = format::parse_id_thunk_claims(thunk_text)
        .is_ok_and(|c| c.requires.iter().any(|f| f == format::SHORT_SEAL));
    let first_slice = entries
        .iter()
        .filter(|e| e.type_label == "data_slice")
        .map(|e| e.position)
        .min();
    let reaches_slices = first_slice.is_some_and(|first| {
        entries
            .iter()
            .any(|e| e.position >= first && selection.selects(e))
    });
    if !may_end_early || !reaches_slices {
        return Ok(None);
    }
    let (Ok(fi_position), Ok(planned)) = (
        u32::try_from(pointers.front_index),
        u32::try_from(pointers.seal_marker),
    ) else {
        return Ok(None);
    };
    let located = crate::volume::rebuild::locate_and_judge_seal(
        store,
        thunk_text,
        format::without_nul_padding(fi_bytes),
        fi_position,
        planned,
    );
    match located.verdict {
        format::SealVerdict::Refused(why) => Err(TapectlError::Other(why)),
        format::SealVerdict::Sealed(_) => Ok(located.ended_after),
        format::SealVerdict::Unsealed(why) => {
            tracing::warn!(
                "raw-volume: no seal marker this tapectl accepts ({why}); dumping the files the \
                 front index plans, as for an unsealed tape"
            );
            Ok(None)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
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
    pub(crate) fn build_synthetic_tape(label: &str, data_bytes: &[u8]) -> MemStore {
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
            cartridge_identity_source: None,
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

    #[test]
    fn dumps_every_file_byte_identical_with_position_type_names() {
        let data = b"hello world, this is the plaintext of a synthetic data slice".to_vec();
        let mut store = build_synthetic_tape("RAW01", &data);
        let tmp = TempDir::new().unwrap();

        let report = restore_raw(&mut store, tmp.path(), None).unwrap();

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

    /// Issue #417: `--only`/`--positions` dump only what they name — an heir
    /// fetching RESTORE.sh no longer dumps every slice of a full cartridge.
    #[test]
    fn a_selection_dumps_only_the_files_it_names() {
        let data = b"slice bytes".to_vec();
        let tmp = TempDir::new().unwrap();
        let mut store = build_synthetic_tape("RAWSEL", &data);
        let only_script = RawSelection {
            positions: vec![],
            types: vec!["restore_sh".into()],
        };
        let report = restore_raw_selected(&mut store, tmp.path(), None, &only_script).unwrap();
        assert_eq!(report.files_dumped, 1);
        assert_eq!(report.files[0].position, 2);
        assert!(report.all_verified());
        let names: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["0002_restore_sh.bin"]);

        // A union: a type and a position.
        let tmp = TempDir::new().unwrap();
        let mut store = build_synthetic_tape("RAWSEL", &data);
        let both = RawSelection {
            positions: vec![0],
            types: vec!["data_slice".into()],
        };
        let report = restore_raw_selected(&mut store, tmp.path(), None, &both).unwrap();
        let got: Vec<(i32, &str)> = report
            .files
            .iter()
            .map(|f| (f.position, f.type_label.as_str()))
            .collect();
        assert_eq!(got, [(0, "id_thunk"), (4, "data_slice")]);
        assert_eq!(
            fs::read(tmp.path().join("0004_data_slice.bin")).unwrap(),
            data
        );
    }

    #[test]
    fn a_selection_naming_nothing_on_the_tape_is_refused_before_dumping() {
        let data = b"slice bytes".to_vec();
        let tmp = TempDir::new().unwrap();
        let mut store = build_synthetic_tape("RAWSEL", &data);
        let err = restore_raw_selected(
            &mut store,
            tmp.path(),
            None,
            &RawSelection {
                positions: vec![99],
                types: vec![],
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no file at position 99"), "{err}");
        assert!(
            fs::read_dir(tmp.path()).unwrap().next().is_none(),
            "nothing dumped"
        );

        let typo = RawSelection {
            positions: vec![],
            types: vec!["restore_script".into()],
        };
        let err = typo.check_types().unwrap_err().to_string();
        assert!(
            err.contains("restore_script") && err.contains("restore_sh"),
            "{err}"
        );
        // The closed set is the front index's own vocabulary.
        for t in RAW_TYPES {
            assert!(
                crate::volume::layout_model::ZoneKind::from_type_label(t).is_some(),
                "{t}"
            );
        }
    }

    #[test]
    fn checksum_mismatch_is_reported_loudly_not_as_success() {
        let data = b"original plaintext bytes for the data slice".to_vec();
        let mut store = build_synthetic_tape("RAW02", &data);
        // Corrupt the on-tape bytes at position 4 after the fact, so the
        // front index's recorded hash no longer matches what gets read back.
        store.files[4] = b"corrupted!! bytes replacing the original slice content".to_vec();
        let tmp = TempDir::new().unwrap();

        let report = restore_raw(&mut store, tmp.path(), None).unwrap();

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

    /// ADR-0012 amendment item 15: a tape whose File 0 requires a feature
    /// this tapectl does not know is refused by name before anything is
    /// dumped — and a key it does not know, not required, is ignored.
    #[test]
    fn a_file_0_requiring_an_unknown_feature_is_refused_before_dumping() {
        let data = b"some plaintext".to_vec();
        let mut store = build_synthetic_tape("RAWFC", &data);
        let f0 = String::from_utf8(store.files[0].clone()).unwrap();
        // From 1.6.0 File 0 states `requires = ["short-seal"]` (ADR-0012
        // item 111): the fixture is checked to say so before it is edited.
        assert!(f0.contains("requires = [\"short-seal\"]"), "{f0}");
        store.files[0] = f0
            .replace(
                "requires = [\"short-seal\"]",
                "requires = [\"unknown-feature\"]",
            )
            .into_bytes();
        let tmp = TempDir::new().unwrap();
        let err = restore_raw(&mut store, tmp.path(), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("\"unknown-feature\""), "{err}");
        assert_eq!(
            fs::read_dir(tmp.path()).unwrap().count(),
            0,
            "nothing dumped"
        );

        // Control: an unknown key nobody requires is read past.
        let mut store = build_synthetic_tape("RAWFC", &data);
        store.files[0] = f0
            .replace(
                "requires = [\"short-seal\"]",
                "requires = [\"short-seal\"]\nfuture_key = 1",
            )
            .into_bytes();
        let tmp = TempDir::new().unwrap();
        restore_raw(&mut store, tmp.path(), None).unwrap();
    }

    #[test]
    fn wrong_label_refuses_before_dumping_anything() {
        let data = b"some plaintext".to_vec();
        let mut store = build_synthetic_tape("REAL-LABEL", &data);
        let tmp = TempDir::new().unwrap();

        let err = restore_raw(&mut store, tmp.path(), Some("WRONG-LABEL"))
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
            store: &mut dyn Store,
            dest: &Path,
            expect_label: Option<&str>,
        ) -> Result<RawRestoreReport> {
            restore_raw(store, dest, expect_label)
        }
    }

    /// #420: `restore raw-volume` of a tape that ended early, on the shared
    /// fixture (`volume::restore::short_tape`): the seal is found first (R1),
    /// nothing after it is read (R3), and the planned files after it are
    /// named as not on the tape.
    mod short_ending {
        use super::*;
        use crate::volume::restore::short_tape::{self, LABEL};

        #[test]
        fn a_raw_dump_of_a_short_tape_stops_at_its_seal_and_names_the_rest() {
            let mut t = short_tape::short_tape();
            let k = t.ended_after;
            let dest = TempDir::new().unwrap();
            let report = restore_raw(&mut t.store, dest.path(), Some(LABEL)).unwrap();
            assert_eq!(report.ended_after, Some(k));
            assert!(report.all_verified(), "{:?}", report.files);
            assert_eq!(
                report.files_dumped,
                k as usize + 2,
                "files 0..=K and the seal"
            );
            let last = report.files.last().unwrap();
            assert_eq!(
                (last.position, last.type_label.as_str()),
                ((k + 1) as i32, "seal_marker")
            );
            assert!(dest
                .path()
                .join(format!("{:04}_seal_marker.bin", k + 1))
                .exists());
            assert_eq!(last.verified, None, "a seal's own entry is unverifiable");
            // Every planned file after K is named, the planned seal among
            // them, and none was dumped.
            let planned: Vec<i32> = ((k + 1)..=t.planned_seal).map(|p| p as i32).collect();
            assert_eq!(
                report
                    .not_on_tape
                    .iter()
                    .map(|(p, _)| *p)
                    .collect::<Vec<_>>(),
                planned
            );
            assert_eq!(
                report.not_on_tape.last().unwrap().1,
                "seal_marker",
                "the plan's seal"
            );
            assert!(!dest
                .path()
                .join(format!("{:04}_data_slice.bin", k + 1))
                .exists());
            let text = ending_lines(&report).join("\n");
            assert!(text.contains(&format!("ended after file {k}")), "{text}");
            assert!(text.contains("NOT ON TAPE"), "{text}");
        }

        #[test]
        fn a_position_past_the_end_is_refused_and_one_before_it_is_dumped() {
            let mut t = short_tape::short_tape();
            let k = t.ended_after;
            let dest = TempDir::new().unwrap();
            let past = RawSelection {
                positions: vec![(k + 2) as i32],
                types: Vec::new(),
            };
            let err = restore_raw_selected(&mut t.store, dest.path(), Some(LABEL), &past)
                .unwrap_err()
                .to_string();
            assert!(err.contains("ended early"), "{err}");
            assert!(err.contains("not on it"), "{err}");
            assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0);

            // K itself (the cut unit's slice that IS on the tape) dumps and
            // verifies: raw-volume is the keyless dump of what is there.
            let at_k = RawSelection {
                positions: vec![k as i32],
                types: Vec::new(),
            };
            let report =
                restore_raw_selected(&mut t.store, dest.path(), Some(LABEL), &at_k).unwrap();
            assert_eq!(report.files_dumped, 1);
            assert_eq!(report.verified_count, 1);
            assert!(report.not_on_tape.is_empty());

            // A selection that reads no data slice does not look for the end.
            let dest = TempDir::new().unwrap();
            let script = RawSelection {
                positions: Vec::new(),
                types: vec!["restore_sh".to_string()],
            };
            let report =
                restore_raw_selected(&mut t.store, dest.path(), Some(LABEL), &script).unwrap();
            assert_eq!(report.files_dumped, 1);
            assert_eq!(report.ended_after, None);
        }

        /// The control: the same write sealed full dumps every planned file,
        /// as before 1.6.0.
        #[test]
        fn a_raw_dump_of_a_full_tape_is_unchanged() {
            let mut t = short_tape::full_tape();
            let dest = TempDir::new().unwrap();
            let report = restore_raw(&mut t.store, dest.path(), Some(LABEL)).unwrap();
            assert_eq!(report.ended_after, None);
            assert!(report.not_on_tape.is_empty());
            assert_eq!(report.files_dumped, t.planned_seal as usize + 1);
            assert!(report.all_verified());
            assert!(ending_lines(&report).is_empty());
        }
    }
}
