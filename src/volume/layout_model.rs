//! The reified **Layout** (ADR-0002, `docs/design/layout-session.md`).
//!
//! A `Layout` is a value: the complete ordered enumeration of every file a
//! volume will hold, constructed and validated *before the first byte is
//! written*, and the single source from which all on-tape metadata is
//! generated. This module owns the type and the pre-write validation
//! predicate only — it is medium-agnostic and performs no tape I/O. Executing
//! a Layout (the Write Session) is #22; generating each zone's bytes from the
//! Layout is #24; the store seam is #71.
//!
//! The domain term is **Layout** (the type); the module name is incidental.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;

/// Which zone of the 10-file volume layout an entry is. Slice and envelope
/// variants carry the id they map to so metadata generation (#24) and the
/// session cursor (#22) can tie an entry back to its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZoneKind {
    IdThunk,
    SystemGuide,
    RestoreSh,
    PlanningHeader,
    /// An encrypted data slice, keyed by `stage_slices.id`.
    Slice {
        stage_slice_id: i64,
    },
    MiniIndex,
    /// A tenant envelope, keyed by `tenants.id`.
    TenantEnvelope {
        tenant_id: i64,
    },
    OperatorEnvelope,
    OperatorEnvelopeBackup,
}

impl ZoneKind {
    /// The plaintext `type` label written into the mini-index for this zone.
    pub fn type_label(&self) -> &'static str {
        match self {
            ZoneKind::IdThunk => "id_thunk",
            ZoneKind::SystemGuide => "system_guide",
            ZoneKind::RestoreSh => "restore_sh",
            ZoneKind::PlanningHeader => "planning_header",
            ZoneKind::Slice { .. } => "data_slice",
            ZoneKind::MiniIndex => "mini_index",
            ZoneKind::TenantEnvelope { .. } => "tenant_envelope",
            ZoneKind::OperatorEnvelope => "operator_envelope",
            ZoneKind::OperatorEnvelopeBackup => "operator_envelope",
        }
    }

    fn is_slice(&self) -> bool {
        matches!(self, ZoneKind::Slice { .. })
    }
}

/// Where an entry's bytes come from. Staged slices already exist on disk with
/// a recorded checksum; generated zones are produced from the Layout at write
/// time (#24) and carry their computed bytes' size/hash once generated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentSource {
    /// An ephemeral staged slice file (`stage_slices.staging_path`).
    Staged(PathBuf),
    /// Bytes generated from the Layout (ID thunk, guide, RESTORE.sh, planning
    /// header, mini-index, envelopes).
    Generated,
}

/// One file the volume will hold, at a fixed position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutEntry {
    /// Tape file position (File 0 = ID thunk).
    pub position: i32,
    pub kind: ZoneKind,
    /// Exact byte size. `None` only while a generated zone is still
    /// unmaterialized; `validate` requires every entry to be sized.
    pub size_bytes: Option<u64>,
    /// sha256 of the on-tape bytes. Required for slices (checked against the
    /// staged file); optional for generated zones.
    pub sha256: Option<String>,
    pub source: ContentSource,
}

impl LayoutEntry {
    /// On-tape footprint, rounded up to whole `block_size` blocks (fixed-block
    /// mode pads the last block). `None` if the entry is unsized.
    pub fn on_tape_bytes(&self, block_size: u64) -> Option<u64> {
        self.size_bytes.map(|s| pad_to_blocks(s, block_size))
    }
}

/// Round `size` up to a whole number of `block_size` blocks.
pub fn pad_to_blocks(size: u64, block_size: u64) -> u64 {
    debug_assert!(block_size > 0);
    size.div_ceil(block_size) * block_size
}

/// The capacity a Layout must fit inside. `available_bytes` is the usable
/// figure (nominal × usable factor); `reserve_bytes` folds together the
/// manifest reserve and the ENOSPC buffer (design §2.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityBudget {
    pub available_bytes: u64,
    pub reserve_bytes: u64,
}

/// What the key-resolvability check needs, as plain data so the predicate is
/// pure and unit-testable. Callers assemble it from the DB and key store.
#[derive(Debug, Clone)]
pub struct KeyAvailability {
    /// Every tenant that has an envelope on this volume.
    pub tenant_ids: Vec<i64>,
    /// Of those, which have at least one active key.
    pub tenants_with_active_key: HashSet<i64>,
    /// The operator's keys are present in the key store.
    pub operator_key_present: bool,
    /// The escrow recipient (ADR-0005). `None` until #68 lands, in which case
    /// the check is skipped; `Some(false)` fails validation.
    pub escrow_recipient_present: Option<bool>,
}

/// A validation failure. `validate` collects all failures rather than
/// stopping at the first, because this is a pre-flight report.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LayoutError {
    #[error("entry at position {position} ({label}) has no size")]
    Unsized { position: i32, label: &'static str },
    #[error("slice at position {position} has no recorded sha256")]
    SliceMissingChecksum { position: i32 },
    #[error("capacity exceeded: on-tape {needed} + reserve {reserve} > available {available}")]
    CapacityExceeded {
        needed: u64,
        reserve: u64,
        available: u64,
    },
    #[error("staged slice file missing: {0}")]
    SliceFileMissing(PathBuf),
    #[error("staged slice checksum mismatch for {path}: expected {expected}, got {actual}")]
    SliceChecksumMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("tenant {0} has no active key")]
    TenantHasNoActiveKey(i64),
    #[error("operator key missing")]
    OperatorKeyMissing,
    #[error("escrow recipient missing (ADR-0005)")]
    EscrowRecipientMissing,
    #[error("i/o hashing {path}: {message}")]
    Io { path: PathBuf, message: String },
}

/// The complete file plan for one volume.
#[derive(Debug, Clone)]
pub struct Layout {
    pub label: String,
    pub volume_uuid: String,
    pub media_type: String,
    pub block_size: u64,
    pub budget: CapacityBudget,
    /// Ordered by tape position; `entries[i].position == i` is expected but the
    /// predicate does not assume it (position is carried explicitly).
    pub entries: Vec<LayoutEntry>,
}

impl Layout {
    /// Sum of every entry's block-padded on-tape footprint. Errors listing any
    /// unsized entries, since capacity can't be known without them.
    pub fn on_tape_bytes(&self) -> Result<u64, Vec<LayoutError>> {
        let mut total = 0u64;
        let mut errs = Vec::new();
        for e in &self.entries {
            match e.on_tape_bytes(self.block_size) {
                Some(b) => total += b,
                None => errs.push(LayoutError::Unsized {
                    position: e.position,
                    label: e.kind.type_label(),
                }),
            }
        }
        if errs.is_empty() {
            Ok(total)
        } else {
            Err(errs)
        }
    }

    /// The full pre-write predicate (ADR-0002 / design note point 1-3): every
    /// entry sized; on-tape total + reserve fits the budget; every staged
    /// slice exists on disk with a matching sha256; keys resolvable. Points 4
    /// (generated-zone parse) and 5 (padding is enforced structurally here)
    /// land with #24. Returns every failure found.
    pub fn validate(&self, keys: &KeyAvailability) -> Result<(), Vec<LayoutError>> {
        let mut errs = Vec::new();
        self.check_capacity(&mut errs);
        self.check_staged_slices(&mut errs);
        self.check_keys(keys, &mut errs);
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }

    fn check_capacity(&self, errs: &mut Vec<LayoutError>) {
        match self.on_tape_bytes() {
            Ok(needed) => {
                if needed + self.budget.reserve_bytes > self.budget.available_bytes {
                    errs.push(LayoutError::CapacityExceeded {
                        needed,
                        reserve: self.budget.reserve_bytes,
                        available: self.budget.available_bytes,
                    });
                }
            }
            Err(mut unsized_errs) => errs.append(&mut unsized_errs),
        }
    }

    fn check_staged_slices(&self, errs: &mut Vec<LayoutError>) {
        for e in &self.entries {
            let ContentSource::Staged(path) = &e.source else {
                continue;
            };
            let Some(expected) = &e.sha256 else {
                if e.kind.is_slice() {
                    errs.push(LayoutError::SliceMissingChecksum {
                        position: e.position,
                    });
                }
                continue;
            };
            match hash_file(path) {
                Ok(actual) if &actual == expected => {}
                Ok(actual) => errs.push(LayoutError::SliceChecksumMismatch {
                    path: path.clone(),
                    expected: expected.clone(),
                    actual,
                }),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    errs.push(LayoutError::SliceFileMissing(path.clone()))
                }
                Err(source) => errs.push(LayoutError::Io {
                    path: path.clone(),
                    message: source.to_string(),
                }),
            }
        }
    }

    fn check_keys(&self, keys: &KeyAvailability, errs: &mut Vec<LayoutError>) {
        for t in &keys.tenant_ids {
            if !keys.tenants_with_active_key.contains(t) {
                errs.push(LayoutError::TenantHasNoActiveKey(*t));
            }
        }
        if !keys.operator_key_present {
            errs.push(LayoutError::OperatorKeyMissing);
        }
        // Escrow recipient is only checked once #68 makes it a real concept.
        if keys.escrow_recipient_present == Some(false) {
            errs.push(LayoutError::EscrowRecipientMissing);
        }
    }
}

/// Streamed sha256 of a file (never buffers the whole file — respects the H9
/// streaming direction).
fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 128 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const BS: u64 = 512 * 1024;

    fn sha_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }

    fn keys_ok(tenants: &[i64]) -> KeyAvailability {
        KeyAvailability {
            tenant_ids: tenants.to_vec(),
            tenants_with_active_key: tenants.iter().copied().collect(),
            operator_key_present: true,
            escrow_recipient_present: None,
        }
    }

    fn gen_entry(position: i32, kind: ZoneKind, size: u64) -> LayoutEntry {
        LayoutEntry {
            position,
            kind,
            size_bytes: Some(size),
            sha256: None,
            source: ContentSource::Generated,
        }
    }

    fn layout_with(entries: Vec<LayoutEntry>, available: u64, reserve: u64) -> Layout {
        Layout {
            label: "L6-0001".into(),
            volume_uuid: "uuid-1".into(),
            media_type: "LTO-6".into(),
            block_size: BS,
            budget: CapacityBudget {
                available_bytes: available,
                reserve_bytes: reserve,
            },
            entries,
        }
    }

    #[test]
    fn pad_rounds_up_to_whole_blocks() {
        assert_eq!(pad_to_blocks(0, BS), 0);
        assert_eq!(pad_to_blocks(1, BS), BS);
        assert_eq!(pad_to_blocks(BS, BS), BS);
        assert_eq!(pad_to_blocks(BS + 1, BS), 2 * BS);
    }

    #[test]
    fn valid_layout_passes() {
        // Two generated metadata files, comfortably under budget.
        let entries = vec![
            gen_entry(0, ZoneKind::IdThunk, 1000),
            gen_entry(1, ZoneKind::MiniIndex, 1000),
        ];
        let l = layout_with(entries, 10 * BS, BS);
        assert!(l.validate(&keys_ok(&[])).is_ok());
    }

    #[test]
    fn capacity_uses_block_padded_sizes() {
        // Three 1-byte files each pad to one block; 3*BS + reserve BS = 4*BS.
        let entries = vec![
            gen_entry(0, ZoneKind::IdThunk, 1),
            gen_entry(1, ZoneKind::SystemGuide, 1),
            gen_entry(2, ZoneKind::MiniIndex, 1),
        ];
        assert_eq!(
            layout_with(entries.clone(), 100 * BS, BS)
                .on_tape_bytes()
                .unwrap(),
            3 * BS
        );
        // Fits exactly at available = 4*BS, reserve = BS.
        assert!(layout_with(entries.clone(), 4 * BS, BS)
            .validate(&keys_ok(&[]))
            .is_ok());
        // One block short → CapacityExceeded.
        let errs = layout_with(entries, 4 * BS - 1, BS)
            .validate(&keys_ok(&[]))
            .unwrap_err();
        assert!(matches!(errs[0], LayoutError::CapacityExceeded { .. }));
    }

    #[test]
    fn unsized_entry_is_rejected() {
        let mut e = gen_entry(0, ZoneKind::IdThunk, 0);
        e.size_bytes = None;
        let errs = layout_with(vec![e], 10 * BS, 0)
            .validate(&keys_ok(&[]))
            .unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, LayoutError::Unsized { position: 0, .. })));
    }

    #[test]
    fn staged_slice_missing_file_reported() {
        let entry = LayoutEntry {
            position: 4,
            kind: ZoneKind::Slice { stage_slice_id: 1 },
            size_bytes: Some(10),
            sha256: Some("deadbeef".into()),
            source: ContentSource::Staged(PathBuf::from("/nonexistent/tapectl/slice.age")),
        };
        let errs = layout_with(vec![entry], 10 * BS, 0)
            .validate(&keys_ok(&[]))
            .unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, LayoutError::SliceFileMissing(_))));
    }

    #[test]
    fn staged_slice_checksum_is_verified() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("s1.age");
        let bytes = b"encrypted slice bytes";
        std::fs::File::create(&good)
            .unwrap()
            .write_all(bytes)
            .unwrap();

        let ok = LayoutEntry {
            position: 4,
            kind: ZoneKind::Slice { stage_slice_id: 1 },
            size_bytes: Some(bytes.len() as u64),
            sha256: Some(sha_hex(bytes)),
            source: ContentSource::Staged(good.clone()),
        };
        assert!(layout_with(vec![ok], 10 * BS, 0)
            .validate(&keys_ok(&[]))
            .is_ok());

        let bad = LayoutEntry {
            position: 4,
            kind: ZoneKind::Slice { stage_slice_id: 1 },
            size_bytes: Some(bytes.len() as u64),
            sha256: Some(sha_hex(b"different")),
            source: ContentSource::Staged(good),
        };
        let errs = layout_with(vec![bad], 10 * BS, 0)
            .validate(&keys_ok(&[]))
            .unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, LayoutError::SliceChecksumMismatch { .. })));
    }

    #[test]
    fn slice_without_checksum_is_rejected() {
        let entry = LayoutEntry {
            position: 4,
            kind: ZoneKind::Slice { stage_slice_id: 1 },
            size_bytes: Some(10),
            sha256: None,
            source: ContentSource::Staged(PathBuf::from("/tmp/whatever.age")),
        };
        let errs = layout_with(vec![entry], 10 * BS, 0)
            .validate(&keys_ok(&[]))
            .unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, LayoutError::SliceMissingChecksum { position: 4 })));
    }

    #[test]
    fn keyless_tenant_and_missing_operator_reported() {
        let l = layout_with(vec![gen_entry(0, ZoneKind::IdThunk, 10)], 10 * BS, 0);
        let keys = KeyAvailability {
            tenant_ids: vec![7, 8],
            tenants_with_active_key: [7].into_iter().collect(),
            operator_key_present: false,
            escrow_recipient_present: None,
        };
        let errs = l.validate(&keys).unwrap_err();
        assert!(errs.contains(&LayoutError::TenantHasNoActiveKey(8)));
        assert!(!errs.contains(&LayoutError::TenantHasNoActiveKey(7)));
        assert!(errs.contains(&LayoutError::OperatorKeyMissing));
    }

    #[test]
    fn escrow_absent_only_fails_when_concept_exists() {
        let l = layout_with(vec![gen_entry(0, ZoneKind::IdThunk, 10)], 10 * BS, 0);
        // None = pre-#68: skipped.
        let mut k = keys_ok(&[]);
        k.escrow_recipient_present = None;
        assert!(l.validate(&k).is_ok());
        // Some(false) = concept exists but recipient missing: fails.
        k.escrow_recipient_present = Some(false);
        assert!(l
            .validate(&k)
            .unwrap_err()
            .contains(&LayoutError::EscrowRecipientMissing));
        // Some(true): passes.
        k.escrow_recipient_present = Some(true);
        assert!(l.validate(&k).is_ok());
    }

    #[test]
    fn validate_collects_all_failures() {
        // Over capacity AND a keyless tenant AND a missing slice, all at once.
        let entries = vec![
            gen_entry(0, ZoneKind::IdThunk, 10 * BS),
            LayoutEntry {
                position: 4,
                kind: ZoneKind::Slice { stage_slice_id: 1 },
                size_bytes: Some(10),
                sha256: Some("abc".into()),
                source: ContentSource::Staged(PathBuf::from("/nope.age")),
            },
        ];
        let l = layout_with(entries, BS, 0);
        let keys = KeyAvailability {
            tenant_ids: vec![9],
            tenants_with_active_key: HashSet::new(),
            operator_key_present: true,
            escrow_recipient_present: None,
        };
        let errs = l.validate(&keys).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, LayoutError::CapacityExceeded { .. })));
        assert!(errs
            .iter()
            .any(|e| matches!(e, LayoutError::SliceFileMissing(_))));
        assert!(errs.contains(&LayoutError::TenantHasNoActiveKey(9)));
    }
}
