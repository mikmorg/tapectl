//! The storage interface (ADR-0006). A `Store` executes a Layout's entries at
//! contact and reports back — write via `execute`, read via `read_file`, a
//! pre-flight `capacity` oracle, and `confirm`'s keyless integrity chain walk
//! (`docs/design/volume-format-v2.md` §5). `TapeStore` is the first
//! implementation (LTO via the kernel st driver); `WarehouseStore`/
//! `ExportStore` are peers landing later (#72/#73).
//!
//! The trait is deliberately medium-agnostic — the anti-tape-ism test is that
//! a warehouse upload must fit `execute` without violence, and a deposit
//! receipt must fit `confirm` the same way. `MemStore` is the in-memory peer
//! that exercises the *exact same* `confirm` algorithm as `TapeStore`: the
//! chain walk is factored into one shared function, [`chain_walk`], so it is
//! the real algorithm — not a description of it — that the unit tests below
//! (and later, the T7 synthetic-heir harness) exercise with no tape anywhere.

use std::io::{Read, Write};

use sha2::{Digest, Sha256};

use crate::error::{Result, TapectlError};
use crate::tape::ioctl::TapeDevice;
use crate::volume::format;
use crate::volume::layout_model::{pad_to_blocks, Layout, ZoneKind};

/// How thoroughly `confirm` checked the tape
/// (`docs/design/volume-format-v2.md` §5). `Navigable` diffs the front index
/// against the Layout only; `Integrity` additionally hashes every content
/// file's on-tape bytes against the front index's `sha256_encrypted`.
/// Integrity is the seal default (ratified 2026-07-22, §1.2); `--quick` opts
/// down to Navigable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Navigable,
    Integrity,
}

/// What kind of disagreement a [`Mismatch`] reports. Each variant maps to a
/// distinct step of the `volume-format-v2.md` §5 chain walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MismatchKind {
    /// The seal marker (last file) is absent, unreadable, or fails to parse.
    /// Per the fail-safe reader precedence (`v2-open-questions.md` §2.5)
    /// this is the *normal* signal for an unsealed tape, never an error.
    SealUnreadable,
    /// The front index (File 3) is unreadable or fails to parse.
    FrontIndexUnreadable,
    /// File 3's own §2.5 self-consistency check found a violation.
    FrontIndexInconsistent,
    /// sha256(File 3's true bytes) != the seal marker's `front_index_sha256`
    /// — the tape's two ends disagree (quarantine-grade).
    FrontIndexDivergesFromSeal,
    /// A front-index entry's `{position, type, size_bytes}` disagrees with
    /// the Layout, or a Layout entry is missing from the front index.
    NavigationDisagreement,
    /// (Integrity tier) a content file's on-tape bytes, truncated to the
    /// front index's claimed size, hash to something other than the front
    /// index's `sha256_encrypted` for that position.
    ContentHashMismatch,
}

/// One disagreement `confirm` found, at a specific tape position. Kept
/// minimal and `Debug`-printable — a report structure, not a control type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    /// The tape file position the disagreement concerns.
    pub position: u32,
    pub kind: MismatchKind,
    /// What was expected (a hash, a size, a count, or a plain description).
    pub expected: String,
    /// What was actually found.
    pub actual: String,
}

/// What `confirm` found.
///
/// `tier` is the tier that was *requested*, not necessarily achieved — it is
/// what `verification_sessions.verify_type` records (Integrity -> `full`,
/// Navigable -> `quick`, ADR-0001). Whether that tier was actually achieved
/// is read off `mismatches`: empty means a clean pass; any entry means the
/// tape failed at (or before) the step that entry describes — including a
/// wholly absent seal marker, reported as a `SealUnreadable` mismatch rather
/// than as an `Err`. Callers (the T6 write session) decide pass/quarantine
/// from `mismatches`, never from `tier` alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    pub tier: Tier,
    pub files_checked: u32,
    pub mismatches: Vec<Mismatch>,
}

/// The validate-time capacity oracle (`layout-session.md` validation point 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityReport {
    pub usable_bytes: u64,
}

/// A medium that executes a Layout's entries at contact and can attest to
/// what it holds afterward.
pub trait Store {
    /// The usable capacity available for a Layout to fit inside
    /// (`Layout::validate`'s capacity oracle).
    fn capacity(&mut self) -> Result<CapacityReport>;

    /// Stream `len` bytes from `src`, followed by a file mark. `sync`
    /// requests a synchronous (durable) file mark — v2 uses this only for
    /// the seal marker; every other v2 entry uses `sync=false` (the final
    /// flush covers everything written before it; v1's op-envelope sync
    /// marks are a caller choice unrelated to this trait). Returns the
    /// number of bytes committed to the medium, including any block padding.
    /// A full medium is an `Err` — there is no salvage path (ADR-0007); the
    /// caller turns that into a clean abort to an unsealed tape.
    fn execute(&mut self, src: &mut dyn Read, len: u64, sync: bool) -> Result<u64>;

    /// Run the keyless integrity chain walk (`volume-format-v2.md` §5)
    /// against `layout` at the requested `tier`. Never fails with `Err` over
    /// tape *content* — every disagreement, including a wholly absent or
    /// unparseable seal marker, is recorded in the returned [`Evidence`]
    /// rather than propagated (fail-safe precedence, `v2-open-questions.md`
    /// §2.5). An `Err` here means `layout` itself is malformed (no
    /// front-index or seal-marker entry) — a caller bug, not a tape
    /// condition.
    fn confirm(&mut self, layout: &Layout, tier: Tier) -> Result<Evidence>;

    /// Read the tape file at `position` (0-indexed), streaming its bytes
    /// into `sink` as they are read rather than buffering the whole file.
    /// Returns the total bytes read (the on-tape length, padding included —
    /// trimming to the true size is the caller's job).
    fn read_file(&mut self, position: u32, sink: &mut dyn Write) -> Result<u64>;
}

/// The §5 chain walk, shared by every `Store` impl's `confirm` so the exact
/// algorithm — not a re-description of it — is what both `TapeStore` (via
/// hardware/mhvtl) and `MemStore` (via the unit tests below and the future
/// T7 synthetic-heir harness) run. `read` fetches one tape file's on-tape
/// (still block-padded) bytes by position; any `Err` it returns is folded
/// into the walk's fail-safe verdict rather than propagated — reading past
/// what a store actually holds is exactly the "absent" case for the seal
/// marker, and an ordinary read failure for anything else.
fn chain_walk<F>(layout: &Layout, tier: Tier, mut read: F) -> Result<Evidence>
where
    F: FnMut(u32) -> Result<Vec<u8>>,
{
    let seal_entry = layout
        .entries
        .iter()
        .find(|e| matches!(e.kind, ZoneKind::SealMarker))
        .ok_or_else(|| TapectlError::Other("layout has no seal_marker entry".into()))?;
    let fi_entry = layout
        .entries
        .iter()
        .find(|e| matches!(e.kind, ZoneKind::FrontIndex))
        .ok_or_else(|| TapectlError::Other("layout has no front_index entry".into()))?;
    let seal_pos = seal_entry.position as u32;
    let fi_pos = fi_entry.position as u32;
    let Some(fi_true_len) = fi_entry.size_bytes else {
        return Err(TapectlError::Other(
            "layout's front_index entry has no size_bytes".into(),
        ));
    };
    let fi_true_len = fi_true_len as usize;

    let mut mismatches: Vec<Mismatch> = Vec::new();
    let mut files_checked: u32 = 0;

    let evidence = 'walk: {
        // Step 1 (§5.1): read + parse the seal marker (the last file).
        // Absent or unparseable is the normal unsealed signal, never an Err.
        let seal_bytes = match read(seal_pos) {
            Ok(b) => b,
            Err(e) => {
                mismatches.push(Mismatch {
                    position: seal_pos,
                    kind: MismatchKind::SealUnreadable,
                    expected: "seal marker present and readable".to_string(),
                    actual: format!("read failed: {e}"),
                });
                break 'walk Evidence {
                    tier,
                    files_checked,
                    mismatches,
                };
            }
        };
        files_checked += 1;
        let seal_str = String::from_utf8_lossy(&seal_bytes);
        let seal = match format::parse_seal_marker(&seal_str) {
            Ok(s) => s,
            Err(e) => {
                mismatches.push(Mismatch {
                    position: seal_pos,
                    kind: MismatchKind::SealUnreadable,
                    expected: "seal marker parses".to_string(),
                    actual: format!("parse failed: {e}"),
                });
                break 'walk Evidence {
                    tier,
                    files_checked,
                    mismatches,
                };
            }
        };

        // Step 2 (§5.2): hash File 3's TRUE bytes; compare to the seal's binding.
        let fi_bytes = match read(fi_pos) {
            Ok(b) => b,
            Err(e) => {
                mismatches.push(Mismatch {
                    position: fi_pos,
                    kind: MismatchKind::FrontIndexUnreadable,
                    expected: "front index present and readable".to_string(),
                    actual: format!("read failed: {e}"),
                });
                break 'walk Evidence {
                    tier,
                    files_checked,
                    mismatches,
                };
            }
        };
        files_checked += 1;
        if fi_true_len > fi_bytes.len() {
            mismatches.push(Mismatch {
                position: fi_pos,
                kind: MismatchKind::FrontIndexUnreadable,
                expected: format!("{fi_true_len} on-tape bytes"),
                actual: format!("only {} bytes read back", fi_bytes.len()),
            });
            break 'walk Evidence {
                tier,
                files_checked,
                mismatches,
            };
        }
        let fi_true_bytes = &fi_bytes[..fi_true_len];
        let fi_hash = sha256_hex(fi_true_bytes);
        if fi_hash != seal.front_index_sha256 {
            mismatches.push(Mismatch {
                position: fi_pos,
                kind: MismatchKind::FrontIndexDivergesFromSeal,
                expected: seal.front_index_sha256.clone(),
                actual: fi_hash,
            });
            // Divergence is quarantine-grade (§2.5), but the walk continues
            // so one confirm call surfaces every disagreement rather than
            // stopping at the first (report, not fail-fast).
        }

        // Step 3 (§5.3): parse File 3, run the §2.5 self-consistency checks,
        // then diff every entry against the Layout = Navigable tier.
        let fi_str = String::from_utf8_lossy(fi_true_bytes);
        let parsed_fi = match format::parse_front_index(&fi_str) {
            Ok(v) => v,
            Err(e) => {
                mismatches.push(Mismatch {
                    position: fi_pos,
                    kind: MismatchKind::FrontIndexUnreadable,
                    expected: "front index parses".to_string(),
                    actual: format!("parse failed: {e}"),
                });
                break 'walk Evidence {
                    tier,
                    files_checked,
                    mismatches,
                };
            }
        };

        for violation in format::validate_consistency(&parsed_fi) {
            mismatches.push(Mismatch {
                position: fi_pos,
                kind: MismatchKind::FrontIndexInconsistent,
                expected: "front index entries are self-consistent (§2.5)".to_string(),
                actual: format!("{violation:?}"),
            });
        }

        for entry in &layout.entries {
            let position = entry.position as u32;
            let Some(claim) = parsed_fi.iter().find(|p| p.position == entry.position) else {
                mismatches.push(Mismatch {
                    position,
                    kind: MismatchKind::NavigationDisagreement,
                    expected: format!("front index lists position {}", entry.position),
                    actual: "missing from front index".to_string(),
                });
                continue;
            };
            if claim.type_label != entry.kind.type_label() {
                mismatches.push(Mismatch {
                    position,
                    kind: MismatchKind::NavigationDisagreement,
                    expected: entry.kind.type_label().to_string(),
                    actual: claim.type_label.clone(),
                });
            }
            // Both File 3's own entry and the seal marker's entry may
            // legitimately omit size_bytes (self-reference / not-yet-known
            // at File-3-build-time — an exclusion rule that may evolve;
            // this diff only flags an outright disagreement, never a bare
            // omission on either side).
            if let (Some(a), Some(b)) = (claim.size_bytes, entry.size_bytes) {
                if a != b {
                    mismatches.push(Mismatch {
                        position,
                        kind: MismatchKind::NavigationDisagreement,
                        expected: format!("size_bytes {b}"),
                        actual: format!("size_bytes {a}"),
                    });
                }
            }
        }

        if tier == Tier::Navigable {
            break 'walk Evidence {
                tier,
                files_checked,
                mismatches,
            };
        }

        // Step 4 (§5.4, Integrity tier only): every file except File 3 and
        // the seal marker, truncated to the front index's claimed size,
        // hashed and compared to the front index's sha256_encrypted.
        for claim in &parsed_fi {
            let position = claim.position as u32;
            if position == fi_pos || position == seal_pos {
                continue;
            }
            let (Some(want_hash), Some(want_size)) = (&claim.sha256_encrypted, claim.size_bytes)
            else {
                mismatches.push(Mismatch {
                    position,
                    kind: MismatchKind::NavigationDisagreement,
                    expected: "front index carries size_bytes + sha256_encrypted".to_string(),
                    actual: "one or both missing for a content file".to_string(),
                });
                continue;
            };

            let bytes = match read(position) {
                Ok(b) => b,
                Err(e) => {
                    mismatches.push(Mismatch {
                        position,
                        kind: MismatchKind::ContentHashMismatch,
                        expected: "file readable".to_string(),
                        actual: format!("read failed: {e}"),
                    });
                    continue;
                }
            };
            files_checked += 1;

            if want_size as usize > bytes.len() {
                mismatches.push(Mismatch {
                    position,
                    kind: MismatchKind::ContentHashMismatch,
                    expected: format!("{want_size} on-tape bytes"),
                    actual: format!("only {} bytes read back", bytes.len()),
                });
                continue;
            }
            let actual_hash = sha256_hex(&bytes[..want_size as usize]);
            if &actual_hash != want_hash {
                mismatches.push(Mismatch {
                    position,
                    kind: MismatchKind::ContentHashMismatch,
                    expected: want_hash.clone(),
                    actual: actual_hash,
                });
            }
        }

        Evidence {
            tier,
            files_checked,
            mismatches,
        }
    };

    Ok(evidence)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// LTO tape via the kernel st driver (fixed 512KB blocks).
pub struct TapeStore {
    dev: TapeDevice,
    usable_bytes: u64,
}

impl TapeStore {
    /// Open the drive and rewind to BOT, ready to write File 0. Hardware
    /// compression is disabled best-effort (encrypted data is incompressible;
    /// §2.8) — a drive that rejects the op is only logged, not failed.
    /// `usable_bytes` is the pre-flight capacity oracle's answer (nominal
    /// capacity × the configured usable-capacity factor); the caller
    /// computes it, since only the caller has the config in scope.
    pub fn open(device: &str, block_size: usize, usable_bytes: u64) -> Result<Self> {
        let dev = TapeDevice::open(device, block_size)?;
        dev.rewind()?;
        if let Err(e) = dev.disable_compression() {
            tracing::warn!(err = %e, "could not disable hardware compression (continuing)");
        }
        Ok(Self { dev, usable_bytes })
    }
}

impl Store for TapeStore {
    fn capacity(&mut self) -> Result<CapacityReport> {
        Ok(CapacityReport {
            usable_bytes: self.usable_bytes,
        })
    }

    fn execute(&mut self, src: &mut dyn Read, len: u64, sync: bool) -> Result<u64> {
        self.dev.write_stream(src, len, sync)
    }

    fn confirm(&mut self, layout: &Layout, tier: Tier) -> Result<Evidence> {
        chain_walk(layout, tier, |position| {
            let mut buf = Vec::new();
            self.read_file(position, &mut buf)?;
            Ok(buf)
        })
    }

    fn read_file(&mut self, position: u32, sink: &mut dyn Write) -> Result<u64> {
        self.dev.rewind()?;
        if position > 0 {
            self.dev.forward_space_file(position as i32)?;
        }
        self.dev.read_file_streaming(sink)
    }
}

/// An in-memory store: proves the interface is medium-agnostic (the "second
/// store implementable without touching Layout code" acceptance) and lets
/// `confirm` and the write session be unit-tested without a tape. Stores
/// PADDED bytes (zero-filled to `block_size`, mirroring tape semantics) so
/// the truncate-then-hash logic in [`chain_walk`] is exercised identically
/// to `TapeStore`.
pub struct MemStore {
    /// Every file's on-tape (padded) bytes, in write order == position.
    pub files: Vec<Vec<u8>>,
    /// Whether each corresponding file used a synchronous filemark.
    pub syncs: Vec<bool>,
    block_size: usize,
    usable_bytes: u64,
}

impl MemStore {
    /// A fresh store with the given block size and an effectively unlimited
    /// capacity; chain with [`Self::with_usable_bytes`] to exercise
    /// capacity-gated tests.
    pub fn new(block_size: usize) -> Self {
        Self {
            files: Vec::new(),
            syncs: Vec::new(),
            block_size,
            usable_bytes: u64::MAX,
        }
    }

    /// Override the capacity `capacity()` reports.
    pub fn with_usable_bytes(mut self, usable_bytes: u64) -> Self {
        self.usable_bytes = usable_bytes;
        self
    }
}

impl Store for MemStore {
    fn capacity(&mut self) -> Result<CapacityReport> {
        Ok(CapacityReport {
            usable_bytes: self.usable_bytes,
        })
    }

    fn execute(&mut self, src: &mut dyn Read, len: u64, sync: bool) -> Result<u64> {
        let mut buf = Vec::with_capacity(len as usize);
        src.take(len)
            .read_to_end(&mut buf)
            .map_err(|e| TapectlError::Other(format!("read source: {e}")))?;
        if (buf.len() as u64) < len {
            return Err(TapectlError::Other(format!(
                "source exhausted after {} of {len} declared bytes",
                buf.len()
            )));
        }
        let padded_len = pad_to_blocks(len, self.block_size as u64);
        buf.resize(padded_len as usize, 0);
        self.files.push(buf);
        self.syncs.push(sync);
        Ok(padded_len)
    }

    fn confirm(&mut self, layout: &Layout, tier: Tier) -> Result<Evidence> {
        chain_walk(layout, tier, |position| {
            let mut buf = Vec::new();
            self.read_file(position, &mut buf)?;
            Ok(buf)
        })
    }

    fn read_file(&mut self, position: u32, sink: &mut dyn Write) -> Result<u64> {
        let bytes = self.files.get(position as usize).ok_or_else(|| {
            TapectlError::Other(format!("no file recorded at position {position}"))
        })?;
        sink.write_all(bytes)
            .map_err(|e| TapectlError::Other(format!("sink write: {e}")))?;
        Ok(bytes.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::layout::{generate_front_index, generate_seal_marker, FrontIndexFile};
    use crate::volume::layout_model::{CapacityBudget, ContentSource, Layout, LayoutEntry};
    use std::io::Cursor;
    use std::path::PathBuf;

    const BS: u64 = 512 * 1024;

    // --- basic Store mechanics ------------------------------------------

    #[test]
    fn memstore_records_entries_in_order() {
        let mut s = MemStore::new(BS as usize);
        s.execute(&mut Cursor::new(b"id-thunk".to_vec()), 8, false)
            .unwrap();
        s.execute(&mut Cursor::new(b"slice".to_vec()), 5, false)
            .unwrap();
        s.execute(&mut Cursor::new(b"op-envelope".to_vec()), 11, true)
            .unwrap();
        assert_eq!(s.files.len(), 3);
        assert_eq!(&s.files[0][..8], b"id-thunk");
        assert_eq!(s.syncs, vec![false, false, true]);
    }

    #[test]
    fn memstore_execute_pads_to_block_boundary_and_reports_padded_length() {
        let bs = 4096u64;
        let mut store = MemStore::new(bs as usize);
        let data = vec![7u8; 5000]; // not a multiple of 4096
        let committed = store
            .execute(&mut Cursor::new(data.clone()), data.len() as u64, false)
            .unwrap();
        let expected_padded = pad_to_blocks(data.len() as u64, bs);
        assert_eq!(committed, expected_padded);
        assert_eq!(store.files[0].len(), expected_padded as usize);
        assert_eq!(&store.files[0][..data.len()], &data[..]);
        assert!(store.files[0][data.len()..].iter().all(|&b| b == 0));
    }

    #[test]
    fn memstore_execute_errors_if_source_is_shorter_than_declared_len() {
        let mut store = MemStore::new(4096);
        let short = b"only four".to_vec();
        assert!(store.execute(&mut Cursor::new(short), 100, false).is_err());
    }

    #[test]
    fn memstore_capacity_reports_configured_usable_bytes() {
        let mut store = MemStore::new(4096).with_usable_bytes(123_456);
        assert_eq!(store.capacity().unwrap().usable_bytes, 123_456);
    }

    #[test]
    fn read_file_errors_on_unknown_position() {
        let mut store = MemStore::new(4096);
        let mut sink = Vec::new();
        assert!(store.read_file(0, &mut sink).is_err());
    }

    #[test]
    fn read_file_round_trips_padded_bytes() {
        let mut store = MemStore::new(4096);
        store
            .execute(&mut Cursor::new(b"hello".to_vec()), 5, false)
            .unwrap();
        let mut sink = Vec::new();
        let n = store.read_file(0, &mut sink).unwrap();
        assert_eq!(n, 4096);
        assert_eq!(sink.len(), 4096);
        assert_eq!(&sink[..5], b"hello");
    }

    // --- confirm / chain_walk, via MemStore -----------------------------

    /// Build a small, self-consistent 6-file fixture (id_thunk, guide,
    /// restore_sh, front_index, one data slice, seal_marker) written into a
    /// `MemStore` by hand — no write session exists yet (T6). Returns the
    /// Layout `confirm` checks against and the store holding the matching
    /// bytes. `seal_hash_override` lets a test bind the seal marker to a
    /// deliberately wrong front-index hash.
    fn build_confirm_fixture(seal_hash_override: Option<&str>) -> (Layout, MemStore) {
        let id_thunk = b"ID THUNK CONTENT".to_vec();
        let guide = b"SYSTEM GUIDE CONTENT".to_vec();
        let restore_sh = b"#!/bin/sh\n# RESTORE.sh CONTENT\n".to_vec();
        let slice = vec![0xABu8; 300_000]; // deliberately not block-aligned

        let id_hash = sha256_hex(&id_thunk);
        let guide_hash = sha256_hex(&guide);
        let restore_hash = sha256_hex(&restore_sh);
        let slice_hash = sha256_hex(&slice);

        // File 3's own content: the front-index and seal-marker entries
        // stay size/hash-less (self-reference / not-yet-written); every
        // other entry carries its real size + on-tape hash.
        let fi_files = vec![
            FrontIndexFile {
                position: 0,
                type_label: "id_thunk",
                size_bytes: Some(id_thunk.len() as u64),
                sha256_encrypted: Some(id_hash.clone()),
            },
            FrontIndexFile {
                position: 1,
                type_label: "system_guide",
                size_bytes: Some(guide.len() as u64),
                sha256_encrypted: Some(guide_hash.clone()),
            },
            FrontIndexFile {
                position: 2,
                type_label: "restore_sh",
                size_bytes: Some(restore_sh.len() as u64),
                sha256_encrypted: Some(restore_hash.clone()),
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
                size_bytes: Some(slice.len() as u64),
                sha256_encrypted: Some(slice_hash.clone()),
            },
            FrontIndexFile {
                position: 5,
                type_label: "seal_marker",
                size_bytes: None,
                sha256_encrypted: None,
            },
        ];

        let fi_bytes = generate_front_index("FIXT01", &fi_files).into_bytes();
        let fi_hash = sha256_hex(&fi_bytes);
        let bound_hash = seal_hash_override.unwrap_or(&fi_hash);

        // The seal marker's embedded copy is MORE complete: fill in File 3's
        // own size + hash (known now, at seal time) before embedding.
        let mut seal_files = fi_files.clone();
        if let Some(e) = seal_files.iter_mut().find(|f| f.position == 3) {
            e.size_bytes = Some(fi_bytes.len() as u64);
            e.sha256_encrypted = Some(fi_hash.clone());
        }
        let seal_bytes = generate_seal_marker("FIXT01", 6, bound_hash, &seal_files).into_bytes();

        let mut store = MemStore::new(BS as usize);
        store
            .execute(
                &mut Cursor::new(id_thunk.clone()),
                id_thunk.len() as u64,
                false,
            )
            .unwrap();
        store
            .execute(&mut Cursor::new(guide.clone()), guide.len() as u64, false)
            .unwrap();
        store
            .execute(
                &mut Cursor::new(restore_sh.clone()),
                restore_sh.len() as u64,
                false,
            )
            .unwrap();
        store
            .execute(
                &mut Cursor::new(fi_bytes.clone()),
                fi_bytes.len() as u64,
                false,
            )
            .unwrap();
        store
            .execute(&mut Cursor::new(slice.clone()), slice.len() as u64, false)
            .unwrap();
        store
            .execute(
                &mut Cursor::new(seal_bytes.clone()),
                seal_bytes.len() as u64,
                true,
            )
            .unwrap();

        let layout = Layout {
            label: "FIXT01".into(),
            volume_uuid: "uuid-fixt".into(),
            media_type: "LTO-6".into(),
            block_size: BS,
            budget: CapacityBudget {
                available_bytes: 1000 * BS,
                reserve_bytes: BS,
            },
            entries: vec![
                LayoutEntry {
                    position: 0,
                    kind: ZoneKind::IdThunk,
                    size_bytes: Some(id_thunk.len() as u64),
                    sha256: Some(id_hash),
                    source: ContentSource::Generated,
                },
                LayoutEntry {
                    position: 1,
                    kind: ZoneKind::SystemGuide,
                    size_bytes: Some(guide.len() as u64),
                    sha256: Some(guide_hash),
                    source: ContentSource::Generated,
                },
                LayoutEntry {
                    position: 2,
                    kind: ZoneKind::RestoreSh,
                    size_bytes: Some(restore_sh.len() as u64),
                    sha256: Some(restore_hash),
                    source: ContentSource::Generated,
                },
                LayoutEntry {
                    position: 3,
                    kind: ZoneKind::FrontIndex,
                    size_bytes: Some(fi_bytes.len() as u64),
                    sha256: Some(fi_hash),
                    source: ContentSource::Generated,
                },
                LayoutEntry {
                    position: 4,
                    kind: ZoneKind::Slice { stage_slice_id: 1 },
                    size_bytes: Some(slice.len() as u64),
                    sha256: Some(slice_hash),
                    source: ContentSource::Staged(PathBuf::from("/fixture/slice.age")),
                },
                LayoutEntry {
                    position: 5,
                    kind: ZoneKind::SealMarker,
                    size_bytes: Some(seal_bytes.len() as u64),
                    sha256: None,
                    source: ContentSource::Generated,
                },
            ],
        };

        (layout, store)
    }

    #[test]
    fn confirm_happy_path_has_zero_mismatches_at_integrity_tier() {
        let (layout, mut store) = build_confirm_fixture(None);
        let evidence = store.confirm(&layout, Tier::Integrity).unwrap();
        assert_eq!(evidence.tier, Tier::Integrity);
        assert_eq!(evidence.mismatches, Vec::new(), "{:?}", evidence.mismatches);
        // Seal + File 3 + the 4 remaining content files (id_thunk, guide,
        // restore_sh, slice) are all read during an Integrity pass.
        assert_eq!(evidence.files_checked, 6);
    }

    #[test]
    fn confirm_happy_path_has_zero_mismatches_at_navigable_tier() {
        let (layout, mut store) = build_confirm_fixture(None);
        let evidence = store.confirm(&layout, Tier::Navigable).unwrap();
        assert_eq!(evidence.tier, Tier::Navigable);
        assert_eq!(evidence.mismatches, Vec::new(), "{:?}", evidence.mismatches);
        // Navigable only reads the seal marker and File 3.
        assert_eq!(evidence.files_checked, 2);
    }

    #[test]
    fn confirm_detects_content_hash_mismatch_only_at_integrity_tier() {
        let (layout, mut store) = build_confirm_fixture(None);
        // Flip a byte well within the slice's true (unpadded) region.
        store.files[4][100] ^= 0xFF;

        let nav = store.confirm(&layout, Tier::Navigable).unwrap();
        assert_eq!(
            nav.mismatches,
            Vec::new(),
            "navigable tier must not hash content: {:?}",
            nav.mismatches
        );

        let full = store.confirm(&layout, Tier::Integrity).unwrap();
        assert_eq!(full.mismatches.len(), 1);
        assert_eq!(full.mismatches[0].position, 4);
        assert_eq!(full.mismatches[0].kind, MismatchKind::ContentHashMismatch);
    }

    #[test]
    fn confirm_reports_unsealed_when_seal_file_is_absent() {
        let (layout, mut store) = build_confirm_fixture(None);
        store.files.pop(); // drop the seal marker entirely (position 5 gone)

        let evidence = store.confirm(&layout, Tier::Integrity).unwrap();
        assert_eq!(evidence.files_checked, 0);
        assert_eq!(evidence.mismatches.len(), 1);
        assert_eq!(evidence.mismatches[0].kind, MismatchKind::SealUnreadable);
        assert_eq!(evidence.mismatches[0].position, 5);
    }

    #[test]
    fn confirm_reports_unsealed_when_seal_file_is_garbage() {
        let (layout, mut store) = build_confirm_fixture(None);
        store.files[5] = vec![0xFFu8; 100]; // present, but not parseable TOML

        let evidence = store.confirm(&layout, Tier::Integrity).unwrap();
        assert_eq!(evidence.files_checked, 1, "the seal read itself succeeded");
        assert_eq!(evidence.mismatches.len(), 1);
        assert_eq!(evidence.mismatches[0].kind, MismatchKind::SealUnreadable);
    }

    #[test]
    fn confirm_detects_front_index_divergence_from_seal_binding() {
        let (layout, mut store) = build_confirm_fixture(Some(&"0".repeat(64)));
        let evidence = store.confirm(&layout, Tier::Navigable).unwrap();
        assert!(evidence
            .mismatches
            .iter()
            .any(|m| m.kind == MismatchKind::FrontIndexDivergesFromSeal));
    }

    #[test]
    fn confirm_surfaces_front_index_consistency_violations() {
        let (mut layout, mut store) = build_confirm_fixture(None);

        // Work from File 3's TRUE (unpadded) bytes — store.files[3] is the
        // block-padded tape buffer, and splicing text into the padded NUL
        // tail would corrupt the document instead of the intended entry
        // list. The Layout's own front-index entry carries the true length.
        let fi_true_len = layout
            .entries
            .iter()
            .find(|e| matches!(e.kind, ZoneKind::FrontIndex))
            .unwrap()
            .size_bytes
            .unwrap() as usize;
        let mut s = String::from_utf8(store.files[3][..fi_true_len].to_vec()).unwrap();

        // Duplicate the id_thunk [[files]] block: the document still parses
        // as valid TOML, but now claims position 0 twice — a §2.5 violation
        // the chain walk must surface as a mismatch, not a parse crash.
        let dup_start = s.find("[[files]]").unwrap();
        let dup_end = dup_start + s[dup_start..].find("\n\n[[files]]").unwrap();
        let block = s[dup_start..dup_end].to_string();
        s.push('\n');
        s.push_str(&block);
        let corrupted_true_bytes = s.into_bytes();
        let corrupted_len = corrupted_true_bytes.len() as u64;

        let mut padded = corrupted_true_bytes.clone();
        padded.resize(pad_to_blocks(corrupted_len, BS) as usize, 0);
        store.files[3] = padded;
        if let Some(e) = layout
            .entries
            .iter_mut()
            .find(|e| matches!(e.kind, ZoneKind::FrontIndex))
        {
            e.size_bytes = Some(corrupted_len);
        }

        // Rebuild + re-store the seal marker bound to the corrupted File 3
        // so step 2 (the binding hash) passes and the walk actually reaches
        // the consistency check in step 3.
        let new_hash = sha256_hex(&corrupted_true_bytes);
        let seal_files = vec![FrontIndexFile {
            position: 3,
            type_label: "front_index",
            size_bytes: Some(corrupted_len),
            sha256_encrypted: Some(new_hash.clone()),
        }];
        let seal_true_bytes =
            generate_seal_marker("FIXT01", 6, &new_hash, &seal_files).into_bytes();
        let seal_len = seal_true_bytes.len() as u64;
        let mut seal_padded = seal_true_bytes;
        seal_padded.resize(pad_to_blocks(seal_len, BS) as usize, 0);
        store.files[5] = seal_padded;
        if let Some(e) = layout
            .entries
            .iter_mut()
            .find(|e| matches!(e.kind, ZoneKind::SealMarker))
        {
            e.size_bytes = Some(seal_len);
        }

        let evidence = store.confirm(&layout, Tier::Navigable).unwrap();
        assert!(
            evidence
                .mismatches
                .iter()
                .any(|m| m.kind == MismatchKind::FrontIndexInconsistent),
            "{:?}",
            evidence.mismatches
        );
    }

    #[test]
    fn confirm_errors_if_layout_lacks_a_seal_marker_entry() {
        let mut store = MemStore::new(4096);
        let layout = Layout {
            label: "X".into(),
            volume_uuid: "u".into(),
            media_type: "LTO-6".into(),
            block_size: 4096,
            budget: CapacityBudget {
                available_bytes: 1,
                reserve_bytes: 0,
            },
            entries: vec![LayoutEntry {
                position: 0,
                kind: ZoneKind::IdThunk,
                size_bytes: Some(1),
                sha256: None,
                source: ContentSource::Generated,
            }],
        };
        assert!(store.confirm(&layout, Tier::Navigable).is_err());
    }

    #[test]
    fn confirm_errors_if_layout_lacks_a_front_index_entry() {
        let mut store = MemStore::new(4096);
        let layout = Layout {
            label: "X".into(),
            volume_uuid: "u".into(),
            media_type: "LTO-6".into(),
            block_size: 4096,
            budget: CapacityBudget {
                available_bytes: 1,
                reserve_bytes: 0,
            },
            entries: vec![LayoutEntry {
                position: 0,
                kind: ZoneKind::SealMarker,
                size_bytes: Some(1),
                sha256: None,
                source: ContentSource::Generated,
            }],
        };
        assert!(store.confirm(&layout, Tier::Navigable).is_err());
    }
}
