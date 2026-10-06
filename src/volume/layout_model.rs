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

/// Which zone of the volume layout (v2, ADR-0007) an entry is. Slice and
/// envelope variants carry the id they map to so metadata generation (#24)
/// and the session cursor (#22) can tie an entry back to its source.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ZoneKind {
    IdThunk,
    SystemGuide,
    RestoreSh,
    /// The plaintext front index (File 3, layout v2 — ADR-0007): carries
    /// per-file position/type/size and ciphertext hashes for every file. The
    /// v1 mid-tape mini-index and the standalone planning-header zone are
    /// both gone (`volume-format-v2.md` §8) — the mini-index's facts moved
    /// here, and the planning header survives only as the operator
    /// envelope's `PLAN.toml` tar member (`generate_planning_header`).
    FrontIndex,
    /// An encrypted data slice, keyed by `stage_slices.id`.
    Slice {
        stage_slice_id: i64,
    },
    /// A tenant envelope, keyed by `tenants.id`.
    TenantEnvelope {
        tenant_id: i64,
    },
    OperatorEnvelope,
    OperatorEnvelopeBackup,
    /// The plaintext trailing seal marker (last file, layout v2 — ADR-0007).
    /// Its presence asserts completeness and binds the front index.
    SealMarker,
}

impl ZoneKind {
    /// The plaintext `type` label written into the front index for this zone.
    pub fn type_label(&self) -> &'static str {
        match self {
            ZoneKind::IdThunk => "id_thunk",
            ZoneKind::SystemGuide => "system_guide",
            ZoneKind::RestoreSh => "restore_sh",
            ZoneKind::FrontIndex => "front_index",
            ZoneKind::Slice { .. } => "data_slice",
            ZoneKind::TenantEnvelope { .. } => "tenant_envelope",
            ZoneKind::OperatorEnvelope => "operator_envelope",
            // Distinct from OperatorEnvelope per volume-format-v2.md §3's type
            // enum ("...operator_envelope, operator_envelope_backup..."), and
            // matched literally by RESTORE.sh's v1 *and* v2 --find-envelope
            // awk pattern (layout.rs, `$2=="operator_envelope_backup"`). Fixed
            // here (plan T5b) — previously collapsed to the same label as the
            // primary envelope, which is a bug relative to the normative
            // format, not a deliberate v1/v2 difference; no test pinned the
            // old value (it only affects the plaintext front/mini index).
            ZoneKind::OperatorEnvelopeBackup => "operator_envelope_backup",
            ZoneKind::SealMarker => "seal_marker",
        }
    }

    /// The reverse of [`Self::type_label`]: reconstruct a `ZoneKind` from a
    /// front-index `type` string. `Slice`/`TenantEnvelope` carry an id in a
    /// production `Layout`, but a front index's plaintext `type` field is
    /// only ever the bare label — no id travels with it (isolation
    /// invariant, `volume-format-v2.md` §2) — so a caller reconstructing a
    /// `Layout` from a *parsed* front index alone (rather than from the
    /// original build inputs) has no id to supply and uses `0` as a dummy
    /// payload. `write.rs::volume_verify`'s post-hoc reconstruction is the
    /// motivating caller: it only ever calls `.type_label()` on the result,
    /// never inspects the dummy id. Returns `None` for an unrecognized
    /// label — callers should treat that as a hard error (an unrecognized
    /// type string is a caller/format problem, not a tape-content mismatch
    /// `Store::confirm`'s `Evidence` is designed to report).
    pub fn from_type_label(label: &str) -> Option<ZoneKind> {
        Some(match label {
            "id_thunk" => ZoneKind::IdThunk,
            "system_guide" => ZoneKind::SystemGuide,
            "restore_sh" => ZoneKind::RestoreSh,
            "front_index" => ZoneKind::FrontIndex,
            "data_slice" => ZoneKind::Slice { stage_slice_id: 0 },
            "tenant_envelope" => ZoneKind::TenantEnvelope { tenant_id: 0 },
            "operator_envelope" => ZoneKind::OperatorEnvelope,
            "operator_envelope_backup" => ZoneKind::OperatorEnvelopeBackup,
            "seal_marker" => ZoneKind::SealMarker,
            _ => return None,
        })
    }

    fn is_slice(&self) -> bool {
        matches!(self, ZoneKind::Slice { .. })
    }
}

/// Where an entry's bytes come from.
///
/// v2 (plan T5b, sheet §2.2 "materialize-to-staging"): every generated zone
/// is written to the session's staging directory *once*, at `build()` time,
/// and thereafter is a disk path + size + hash exactly like a staged slice —
/// no `Vec<u8>` ever lives in a `Layout`. This is what lets `execute` stream
/// uniformly from disk in position order, and what fixes resume: the ID
/// thunk and seal marker embed real timestamps, so regenerating them on
/// resume would silently produce different bytes than what is already on
/// tape; materializing once and re-reading the frozen file avoids that.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ContentSource {
    /// An ephemeral staged slice file (`stage_slices.staging_path`). Never
    /// regenerated or re-encrypted by the Layout — it arrives already
    /// encrypted from stage time; the Layout only records its existing size
    /// and `sha256_encrypted` verbatim (no third read of the slice bulk,
    /// `volume-format-v2.md` §3).
    Staged(PathBuf),
    /// A generated zone (ID thunk, guide, RESTORE.sh, front index, envelopes,
    /// seal marker) frozen to a file under the session staging directory at
    /// `build()` time (v2-open-questions.md §2.2). `size_bytes`/`sha256` on
    /// the owning `LayoutEntry` describe these exact on-disk bytes.
    Materialized(PathBuf),
    /// No real backing file at all — used only where a `LayoutEntry` must be
    /// constructed but nothing ever reads `source` for it. `build()` (the v2
    /// path) never produces this for a real write session; every one of its
    /// generated zones is `Materialized`. The one v2 caller today is
    /// `write.rs::volume_verify`'s post-hoc reconstruction: it rebuilds a
    /// synthetic `Layout` directly from a *parsed* front index (there is no
    /// on-disk session to point at, possibly years after the write), and
    /// `Store::confirm`'s chain walk never reads `entry.source` — only
    /// `position`/`kind`/`size_bytes`/`sha256` — so this variant is a safe
    /// "don't care" placeholder there. (Formerly also v1's variant for bytes
    /// generated in memory by the pre-T8 write pipeline; that pipeline is
    /// gone.)
    Generated,
}

/// One file the volume will hold, at a fixed position.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CapacityBudget {
    pub available_bytes: u64,
    pub reserve_bytes: u64,
}

/// How [`Layout::validate`] checks each staged slice before the tape moves —
/// tri-layer L1 (`v2-open-questions.md` §2.4).
///
/// ADR-0012's 2026-09-30 (later) amendment: the default is [`Self::Size`].
/// L2 (execute's inline re-hash of the very bytes streamed to the store,
/// clean abort on mismatch, no seal) already guarantees no rotted slice is
/// ever sealed; a full L1 hash only moves that detection before the tape
/// starts, at the price of reading every staged byte one extra time — hours
/// per copy at production sizes. [`Self::FullHash`] (`--prewrite-hash`)
/// restores it. Only the staged-slice check is affected: materialized
/// (frozen generated) zones are always size- and hash-checked — they are
/// small, and resume's "frozen zones re-hash byte-identical" rule depends on
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SliceCheck {
    /// The staged file exists and its size equals the recorded encrypted
    /// size. Reads metadata only, never the slice's bytes.
    #[default]
    Size,
    /// Size check plus a full sha256 of the file from disk against the
    /// recorded ciphertext hash.
    FullHash,
}

impl SliceCheck {
    /// The CLI's `--prewrite-hash` as a mode.
    pub fn from_prewrite_hash(prewrite_hash: bool) -> Self {
        if prewrite_hash {
            Self::FullHash
        } else {
            Self::Size
        }
    }
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
    /// The escrow recipient (ADR-0005). A production caller assembles this
    /// from `queries::escrow_key_exists`/`escrow_public_key`: `Some(true)`
    /// when a registered escrow row exists, `Some(false)` when it does not
    /// (fails validation, `LayoutError::EscrowRecipientMissing`). `None`
    /// skips the check entirely — meant for a caller that has no escrow
    /// concept in its context, not a routine choice. The real orchestrators
    /// (`write::volume_write` and `write::volume_resume`, both via
    /// `write::assemble_session_keys`) pass `Some(..)`, as should any new one
    /// (ADR-0005).
    pub escrow_recipient_present: Option<bool>,
    /// Per-stage-set escrow verdicts for the stage sets this write is about
    /// to put on tape: `(stage_set_id, unit_name, reason)`, one entry per
    /// stage set whose **recorded** recipient list (`stage_sets.key_
    /// fingerprints`) does not contain the currently registered escrow
    /// public key (issue #115).
    ///
    /// This is a strictly different question from
    /// [`Self::escrow_recipient_present`], which only asks "is an escrow
    /// registered *now*". Slices are encrypted at `stage create` time and
    /// envelopes at `volume write` time, so registering the escrow between
    /// those two steps — the natural order for a new archive, since `volume
    /// write` is the first command that mentions escrow at all — produced a
    /// tape that sealed and verified clean while the escrow key could open
    /// the envelopes and not the data. The recorded recipient list is the
    /// only evidence of who a staged slice was actually encrypted to (an
    /// age X25519 stanza carries an ephemeral share, not a recipient
    /// identity), so it is what this field is computed from.
    ///
    /// Semantics mirror `escrow_recipient_present`: `None` means the caller
    /// did not compute it and the check is skipped entirely — meant for a
    /// caller with no stage-set context, not a routine choice. Production
    /// orchestrators MUST pass `Some(..)`, and always do: the one place this
    /// is computed is `write::assemble_session_keys`, shared by
    /// `write::volume_write` and `write::volume_resume`. `Some(vec![])` —
    /// the pass verdict — is also what a not-yet-registered escrow yields,
    /// since `escrow_recipient_present: Some(false)` already reports that
    /// case and the two must not double-report.
    pub stage_sets_lacking_escrow: Option<Vec<(i64, String, String)>>,
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
    #[error("staged slice size mismatch for {path}: recorded {expected} bytes, on-disk {actual}")]
    SliceSizeMismatch {
        path: PathBuf,
        expected: u64,
        actual: u64,
    },
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
    /// A stage set about to be written was encrypted to a recipient list
    /// that does not include the *current* escrow public key (issue #115).
    /// Deliberately NOT [`Self::EscrowRecipientMissing`]: that one means "no
    /// escrow is registered at all" and is fixed by registering one, while
    /// this one means "an escrow exists but these particular slices predate
    /// it (or predate a deliberate escrow swap, ADR-0005)" and is fixed only
    /// by re-staging — the slices cannot be repaired in place, and must not
    /// be.
    #[error(
        "stage set {stage_set_id} for unit '{unit}' was encrypted without the current escrow \
         recipient ({reason}) — its slices cannot be recovered with the escrow key; re-stage \
         it: tapectl staging clean --force (releases the old set), then \
         tapectl stage create {unit} --version <snapshot version> (ADR-0005)"
    )]
    StageSetLacksEscrow {
        stage_set_id: i64,
        unit: String,
        reason: String,
    },
    #[error("i/o hashing {path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("materialized zone at position {position} missing from disk: {path}")]
    MaterializedZoneMissing { position: i32, path: PathBuf },
    #[error(
        "materialized zone at position {position} size mismatch: recorded {expected}, on-disk {actual}"
    )]
    MaterializedZoneSizeMismatch {
        position: i32,
        expected: u64,
        actual: u64,
    },
    #[error(
        "materialized zone at position {position} hash mismatch: recorded {expected}, on-disk {actual}"
    )]
    MaterializedZoneHashMismatch {
        position: i32,
        expected: String,
        actual: String,
    },
    #[error("generated zone at position {position} ({label}) failed to parse: {message}")]
    GeneratedZoneUnparseable {
        position: i32,
        label: &'static str,
        message: String,
    },
    #[error("generated zone at position {position} is internally inconsistent: {message}")]
    GeneratedZoneInconsistent { position: i32, message: String },
    #[error("RESTORE.sh failed `bash -n`: {0}")]
    RestoreScriptSyntaxError(String),
    /// A signal stopped the full hash of the staged slices (`--prewrite-hash`)
    /// before it finished (#404's follow-up). Nothing was compared after this
    /// point, so it is reported once, as where the check stopped, and the
    /// caller turns it into `TapectlError::Interrupted`: a stop, not a
    /// failure of the slices.
    #[error("the full hash of the staged slices stopped at file {position} of {files}")]
    Interrupted { position: i32, files: usize },
}

impl LayoutError {
    /// The [`Self::Interrupted`] among `errs`, if a signal stopped the
    /// validation — the one error that is not a finding about the Layout.
    pub fn interruption(errs: &[LayoutError]) -> Option<&LayoutError> {
        errs.iter()
            .find(|e| matches!(e, LayoutError::Interrupted { .. }))
    }
}

/// The complete file plan for one volume.
///
/// `Serialize`/`Deserialize` exist for exactly one purpose (issue #25):
/// `build()` freezes the Layout to `session_dir/layout.json` so a restarted
/// process can REHYDRATE an interrupted session instead of regenerating it
/// (see [`ContentSource`] above for why regeneration is unsafe). That sidecar
/// is a staging-side artifact only — it is never a [`LayoutEntry`], never
/// occupies a tape position, and never reaches the medium. Nothing in
/// `volume-format-v2.md` describes it, and nothing should: the on-tape
/// self-describing record is the front index and the seal marker.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

    /// The full pre-write predicate (ADR-0002 / `layout-session.md`
    /// validation points 1-3+5): every entry sized; on-tape total + reserve
    /// fits the budget; every staged slice has a recorded sha256 (L2
    /// compares against it) and exists on disk at its recorded size — and,
    /// under [`SliceCheck::FullHash`] only, with a matching sha256 (tri-layer
    /// L1; size-only by default since ADR-0012's 2026-09-30 later amendment,
    /// see [`SliceCheck`]); every *materialized* zone exists on disk and matches its
    /// recorded size (and hash, where one was recorded — the placeholder
    /// seal marker deliberately carries none, see `build::build`); keys
    /// resolvable. Point 4 (generated-zone TOML/consistency/`bash -n`
    /// parsing, which needs `format::` and a subprocess) is composed on top
    /// by `build::BuiltLayout::validate` (plan T5b) rather than living here,
    /// so this module stays free of a `format`/process dependency. Returns
    /// every failure found, never stops at the first (this is a pre-flight
    /// report).
    pub fn validate(
        &self,
        keys: &KeyAvailability,
        slice_check: SliceCheck,
    ) -> Result<(), Vec<LayoutError>> {
        let mut errs = Vec::new();
        self.check_capacity(&mut errs);
        self.check_staged_slices(slice_check, &mut errs);
        self.check_materialized_zones(&mut errs);
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

    fn check_staged_slices(&self, slice_check: SliceCheck, errs: &mut Vec<LayoutError>) {
        // Issue #386: a full hash reads every staged byte — the caller's
        // progress phase learns the total here and counts in `hash_file`.
        if slice_check == SliceCheck::FullHash {
            crate::progress::set_total(
                self.entries
                    .iter()
                    .filter(|e| matches!(e.source, ContentSource::Staged(_)))
                    .filter_map(|e| e.size_bytes)
                    .sum(),
            );
        }
        let files = self.entries.len();
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
            // Existence and size first, in both modes — metadata only.
            match std::fs::metadata(path) {
                Ok(meta) => {
                    if let Some(expected_size) = e.size_bytes {
                        if meta.len() != expected_size {
                            errs.push(LayoutError::SliceSizeMismatch {
                                path: path.clone(),
                                expected: expected_size,
                                actual: meta.len(),
                            });
                            continue;
                        }
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    errs.push(LayoutError::SliceFileMissing(path.clone()));
                    continue;
                }
                Err(err) => {
                    errs.push(LayoutError::Io {
                        path: path.clone(),
                        message: err.to_string(),
                    });
                    continue;
                }
            }
            if slice_check != SliceCheck::FullHash {
                continue;
            }
            // #404's follow-up: the full hash reads every staged byte —
            // hours on a full cartridge, before the drive is even opened —
            // so a signal stops it between slices and inside one
            // (`hash_file`), and nothing after that point is checked. Not
            // cleared here: the caller reports the stop and the process ends.
            let interrupted = LayoutError::Interrupted {
                position: e.position,
                files,
            };
            if crate::signal::is_interrupted() {
                errs.push(interrupted);
                return;
            }
            crate::progress::item(format!("hashing file {} of {files}", e.position));
            match hash_file(path) {
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {
                    errs.push(interrupted);
                    return;
                }
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

    /// Every `ContentSource::Materialized` entry must exist on disk with the
    /// exact recorded size; if a hash was also recorded, it must match too.
    /// The seal marker's placeholder bytes deliberately record `sha256:
    /// None` (its real on-tape bytes will differ once `seal()` substitutes
    /// the true `sealed_at`, so hashing the placeholder would record a claim
    /// the eventual tape bytes can never satisfy) — `None` skips the hash
    /// half of this check for that entry without a `ZoneKind` special case.
    fn check_materialized_zones(&self, errs: &mut Vec<LayoutError>) {
        for e in &self.entries {
            let ContentSource::Materialized(path) = &e.source else {
                continue;
            };
            let meta = match std::fs::metadata(path) {
                Ok(m) => m,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    errs.push(LayoutError::MaterializedZoneMissing {
                        position: e.position,
                        path: path.clone(),
                    });
                    continue;
                }
                Err(err) => {
                    errs.push(LayoutError::Io {
                        path: path.clone(),
                        message: err.to_string(),
                    });
                    continue;
                }
            };
            if let Some(expected_size) = e.size_bytes {
                let actual_size = meta.len();
                if actual_size != expected_size {
                    errs.push(LayoutError::MaterializedZoneSizeMismatch {
                        position: e.position,
                        expected: expected_size,
                        actual: actual_size,
                    });
                }
            }
            if let Some(expected_hash) = &e.sha256 {
                match hash_file(path) {
                    Ok(actual) if &actual == expected_hash => {}
                    Ok(actual) => errs.push(LayoutError::MaterializedZoneHashMismatch {
                        position: e.position,
                        expected: expected_hash.clone(),
                        actual,
                    }),
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {
                        errs.push(LayoutError::Interrupted {
                            position: e.position,
                            files: self.entries.len(),
                        });
                        return;
                    }
                    Err(err) => errs.push(LayoutError::Io {
                        path: path.clone(),
                        message: err.to_string(),
                    }),
                }
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
        // `None` means the caller opted out of the check entirely (see
        // `KeyAvailability.escrow_recipient_present`'s doc comment); only an
        // explicit `Some(false)` — a registered-escrow check that came back
        // negative — fails validation.
        if keys.escrow_recipient_present == Some(false) {
            errs.push(LayoutError::EscrowRecipientMissing);
        }
        // Issue #115. Same `None` = opted-out semantics as above; one error
        // per offending stage set, because the remedy (re-stage) is
        // per-stage-set and the operator needs to know which ones. The
        // caller guarantees this list is empty when no escrow is registered
        // at all, so this never doubles up with `EscrowRecipientMissing`.
        if let Some(lacking) = &keys.stage_sets_lacking_escrow {
            for (stage_set_id, unit, reason) in lacking {
                errs.push(LayoutError::StageSetLacksEscrow {
                    stage_set_id: *stage_set_id,
                    unit: unit.clone(),
                    reason: reason.clone(),
                });
            }
        }
    }
}

/// Streamed sha256 of a file (never buffers the whole file — respects the H9
/// streaming direction). `pub(crate)` so `build.rs` can reuse it rather than
/// duplicating a streaming-hash loop (plan T5b).
/// The sha256 of `path`, read in 128 KiB chunks. A signal (#404) stops it
/// between chunks with an error of kind [`std::io::ErrorKind::Interrupted`],
/// so a multi-GiB slice does not hold a Ctrl-C off until it is done.
pub(crate) fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 128 * 1024];
    loop {
        if crate::signal::is_interrupted() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "stopped by a signal",
            ));
        }
        // EINTR from a signal that did not set the flag is retried, as
        // `read_to_end` would; one that did is answered at the top.
        let n = match f.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        crate::progress::add_bytes(n as u64);
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
            stage_sets_lacking_escrow: None,
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
            gen_entry(1, ZoneKind::SystemGuide, 1000),
        ];
        let l = layout_with(entries, 10 * BS, BS);
        assert!(l.validate(&keys_ok(&[]), SliceCheck::Size).is_ok());
    }

    #[test]
    fn capacity_uses_block_padded_sizes() {
        // Three 1-byte files each pad to one block; 3*BS + reserve BS = 4*BS.
        let entries = vec![
            gen_entry(0, ZoneKind::IdThunk, 1),
            gen_entry(1, ZoneKind::SystemGuide, 1),
            gen_entry(2, ZoneKind::RestoreSh, 1),
        ];
        assert_eq!(
            layout_with(entries.clone(), 100 * BS, BS)
                .on_tape_bytes()
                .unwrap(),
            3 * BS
        );
        // Fits exactly at available = 4*BS, reserve = BS.
        assert!(layout_with(entries.clone(), 4 * BS, BS)
            .validate(&keys_ok(&[]), SliceCheck::Size)
            .is_ok());
        // One block short → CapacityExceeded.
        let errs = layout_with(entries, 4 * BS - 1, BS)
            .validate(&keys_ok(&[]), SliceCheck::Size)
            .unwrap_err();
        assert!(matches!(errs[0], LayoutError::CapacityExceeded { .. }));
    }

    #[test]
    fn unsized_entry_is_rejected() {
        let mut e = gen_entry(0, ZoneKind::IdThunk, 0);
        e.size_bytes = None;
        let errs = layout_with(vec![e], 10 * BS, 0)
            .validate(&keys_ok(&[]), SliceCheck::Size)
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
            .validate(&keys_ok(&[]), SliceCheck::Size)
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
            .validate(&keys_ok(&[]), SliceCheck::Size)
            .is_ok());

        let bad = LayoutEntry {
            position: 4,
            kind: ZoneKind::Slice { stage_slice_id: 1 },
            size_bytes: Some(bytes.len() as u64),
            sha256: Some(sha_hex(b"different")),
            source: ContentSource::Staged(good),
        };
        // Default (size-only, ADR-0012 2026-09-30 later): the same-size
        // corruption is NOT caught here — L2 catches it while streaming.
        assert!(layout_with(vec![bad.clone()], 10 * BS, 0)
            .validate(&keys_ok(&[]), SliceCheck::Size)
            .is_ok());
        // `--prewrite-hash`: the full hash refuses it before the tape moves.
        let errs = layout_with(vec![bad], 10 * BS, 0)
            .validate(&keys_ok(&[]), SliceCheck::FullHash)
            .unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, LayoutError::SliceChecksumMismatch { .. })));
    }

    /// #404's follow-up: `--prewrite-hash` reads every staged byte before the
    /// drive opens — hours — and a signal used to wait for all of it. Now it
    /// stops, and says it stopped rather than passing or failing the slices.
    /// Before the change this validate returned `Ok` (the slice is good).
    #[test]
    fn a_signal_stops_the_full_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s1.age");
        let bytes = b"encrypted slice bytes";
        std::fs::write(&path, bytes).unwrap();
        let entry = LayoutEntry {
            position: 4,
            kind: ZoneKind::Slice { stage_slice_id: 1 },
            size_bytes: Some(bytes.len() as u64),
            sha256: Some(sha_hex(bytes)),
            source: ContentSource::Staged(path.clone()),
        };
        // Positive control: unsignalled, the good slice passes the full hash.
        assert!(layout_with(vec![entry.clone()], 10 * BS, 0)
            .validate(&keys_ok(&[]), SliceCheck::FullHash)
            .is_ok());

        crate::signal::interrupt_this_thread(true);
        let errs = layout_with(vec![entry.clone()], 10 * BS, 0)
            .validate(&keys_ok(&[]), SliceCheck::FullHash)
            .unwrap_err();
        // Inside one slice too: `hash_file` itself stops between chunks.
        let mid = hash_file(&path).unwrap_err();
        // The size-only default reads no bytes and has nothing to stop.
        let size_only =
            layout_with(vec![entry], 10 * BS, 0).validate(&keys_ok(&[]), SliceCheck::Size);
        crate::signal::interrupt_this_thread(false);

        assert!(
            matches!(
                LayoutError::interruption(&errs),
                Some(LayoutError::Interrupted { position: 4, .. })
            ),
            "{errs:?}"
        );
        assert!(
            !errs
                .iter()
                .any(|e| matches!(e, LayoutError::SliceChecksumMismatch { .. })),
            "a stop is not a finding about the slice: {errs:?}"
        );
        assert_eq!(mid.kind(), std::io::ErrorKind::Interrupted);
        assert!(size_only.is_ok());
    }

    #[test]
    fn staged_slice_size_is_checked_in_both_modes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s1.age");
        let bytes = b"encrypted slice bytes";
        std::fs::File::create(&path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
        let wrong_size = LayoutEntry {
            position: 4,
            kind: ZoneKind::Slice { stage_slice_id: 1 },
            size_bytes: Some(bytes.len() as u64 + 1),
            sha256: Some(sha_hex(bytes)),
            source: ContentSource::Staged(path),
        };
        for mode in [SliceCheck::Size, SliceCheck::FullHash] {
            let errs = layout_with(vec![wrong_size.clone()], 10 * BS, 0)
                .validate(&keys_ok(&[]), mode)
                .unwrap_err();
            assert!(
                errs.iter().any(|e| matches!(
                    e,
                    LayoutError::SliceSizeMismatch { expected, actual, .. }
                        if *expected == bytes.len() as u64 + 1 && *actual == bytes.len() as u64
                )),
                "{mode:?}: {errs:?}"
            );
        }
    }

    #[test]
    fn size_only_check_still_requires_a_recorded_slice_hash() {
        // L2 compares the streamed hash against the recorded one, so a
        // slice with none must still be refused even when L1 does not hash.
        let entry = LayoutEntry {
            position: 4,
            kind: ZoneKind::Slice { stage_slice_id: 1 },
            size_bytes: Some(10),
            sha256: None,
            source: ContentSource::Staged(PathBuf::from("/nonexistent/tapectl/slice.age")),
        };
        let errs = layout_with(vec![entry], 10 * BS, 0)
            .validate(&keys_ok(&[]), SliceCheck::Size)
            .unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, LayoutError::SliceMissingChecksum { position: 4 })));
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
            .validate(&keys_ok(&[]), SliceCheck::Size)
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
            stage_sets_lacking_escrow: None,
        };
        let errs = l.validate(&keys, SliceCheck::Size).unwrap_err();
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
        assert!(l.validate(&k, SliceCheck::Size).is_ok());
        // Some(false) = concept exists but recipient missing: fails.
        k.escrow_recipient_present = Some(false);
        assert!(l
            .validate(&k, SliceCheck::Size)
            .unwrap_err()
            .contains(&LayoutError::EscrowRecipientMissing));
        // Some(true): passes.
        k.escrow_recipient_present = Some(true);
        assert!(l.validate(&k, SliceCheck::Size).is_ok());
    }

    /// Issue #115: an escrow can be registered (so `escrow_recipient_present`
    /// is `Some(true)` and `EscrowRecipientMissing` never fires) while a
    /// stage set about to be written was encrypted BEFORE that registration
    /// — the envelopes would carry the escrow recipient and the slices would
    /// not. `validate` must refuse, naming the offending stage set, its
    /// unit, and why.
    #[test]
    fn stage_set_lacking_escrow_fails_validation_per_stage_set() {
        let l = layout_with(vec![gen_entry(0, ZoneKind::IdThunk, 10)], 10 * BS, 0);
        let mut k = keys_ok(&[]);
        k.escrow_recipient_present = Some(true);

        // The pass verdict: an escrow exists and every stage set records it.
        k.stage_sets_lacking_escrow = Some(vec![]);
        assert!(
            l.validate(&k, SliceCheck::Size).is_ok(),
            "an empty verdict list is the PASS case and must not error"
        );

        // One lacking stage set → exactly one StageSetLacksEscrow for it.
        k.stage_sets_lacking_escrow = Some(vec![(
            42,
            "photos".to_string(),
            "escrow recipient absent from recorded list".to_string(),
        )]);
        let errs = l.validate(&k, SliceCheck::Size).unwrap_err();
        let lacking: Vec<&LayoutError> = errs
            .iter()
            .filter(|e| matches!(e, LayoutError::StageSetLacksEscrow { .. }))
            .collect();
        assert_eq!(
            lacking.len(),
            1,
            "expected exactly one StageSetLacksEscrow, got: {errs:?}"
        );
        assert_eq!(
            *lacking[0],
            LayoutError::StageSetLacksEscrow {
                stage_set_id: 42,
                unit: "photos".to_string(),
                reason: "escrow recipient absent from recorded list".to_string(),
            }
        );
        // It is NOT the "no escrow registered at all" error — the two have
        // different remedies (register one vs. re-stage) and must stay apart.
        assert!(
            !errs.contains(&LayoutError::EscrowRecipientMissing),
            "must not double-report as EscrowRecipientMissing: {errs:?}"
        );
        // The rendered message must name the remedy the operator has to run.
        let msg = lacking[0].to_string();
        assert!(
            msg.contains("staging clean --force")
                && msg.contains("tapectl stage create photos")
                && msg.contains("ADR-0005"),
            "message must name the re-stage remedy and the ADR: {msg}"
        );

        // `None` = the caller did not compute the field: check skipped.
        k.stage_sets_lacking_escrow = None;
        assert!(l.validate(&k, SliceCheck::Size).is_ok());
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
            stage_sets_lacking_escrow: None,
        };
        let errs = l.validate(&keys, SliceCheck::Size).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, LayoutError::CapacityExceeded { .. })));
        assert!(errs
            .iter()
            .any(|e| matches!(e, LayoutError::SliceFileMissing(_))));
        assert!(errs.contains(&LayoutError::TenantHasNoActiveKey(9)));
    }

    fn materialized_entry(
        position: i32,
        path: PathBuf,
        size: u64,
        sha256: Option<String>,
    ) -> LayoutEntry {
        LayoutEntry {
            position,
            kind: ZoneKind::FrontIndex,
            size_bytes: Some(size),
            sha256,
            source: ContentSource::Materialized(path),
        }
    }

    #[test]
    fn materialized_zone_missing_file_reported() {
        let entry = materialized_entry(
            3,
            PathBuf::from("/nonexistent/tapectl/front_index.toml"),
            10,
            Some("deadbeef".into()),
        );
        let errs = layout_with(vec![entry], 10 * BS, 0)
            .validate(&keys_ok(&[]), SliceCheck::Size)
            .unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, LayoutError::MaterializedZoneMissing { position: 3, .. })));
    }

    #[test]
    fn materialized_zone_size_mismatch_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("front_index.toml");
        let bytes = b"front index bytes";
        std::fs::File::create(&path)
            .unwrap()
            .write_all(bytes)
            .unwrap();

        // Recorded size is wrong (one byte too many) — the hash is left
        // correct so this isolates the size-mismatch branch.
        let entry = materialized_entry(3, path, bytes.len() as u64 + 1, Some(sha_hex(bytes)));
        let errs = layout_with(vec![entry], 10 * BS, 0)
            .validate(&keys_ok(&[]), SliceCheck::Size)
            .unwrap_err();
        assert!(errs.iter().any(|e| matches!(
            e,
            LayoutError::MaterializedZoneSizeMismatch { position: 3, .. }
        )));
    }

    #[test]
    fn materialized_zone_hash_mismatch_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("front_index.toml");
        let bytes = b"front index bytes";
        std::fs::File::create(&path)
            .unwrap()
            .write_all(bytes)
            .unwrap();

        let entry = materialized_entry(3, path, bytes.len() as u64, Some(sha_hex(b"corrupted")));
        let errs = layout_with(vec![entry], 10 * BS, 0)
            .validate(&keys_ok(&[]), SliceCheck::Size)
            .unwrap_err();
        assert!(errs.iter().any(|e| matches!(
            e,
            LayoutError::MaterializedZoneHashMismatch { position: 3, .. }
        )));
    }

    #[test]
    fn materialized_zone_without_recorded_hash_skips_hash_check() {
        // The placeholder seal marker deliberately records sha256: None
        // (build.rs) — validate must still check its size, but not fail it
        // over a hash it was never given, no matter what the file contains.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seal_marker.toml");
        let bytes = b"placeholder seal bytes, timestamp will differ later";
        std::fs::File::create(&path)
            .unwrap()
            .write_all(bytes)
            .unwrap();

        let entry = materialized_entry(9, path, bytes.len() as u64, None);
        assert!(layout_with(vec![entry], 10 * BS, 0)
            .validate(&keys_ok(&[]), SliceCheck::Size)
            .is_ok());
    }

    #[test]
    fn operator_envelope_backup_has_a_distinct_type_label() {
        // volume-format-v2.md §3's type enum lists operator_envelope and
        // operator_envelope_backup as distinct values, and RESTORE.sh's
        // --find-envelope awk pattern (layout.rs) matches the literal
        // "operator_envelope_backup" string separately from
        // "operator_envelope" — the two must not collapse to the same label.
        assert_eq!(ZoneKind::OperatorEnvelope.type_label(), "operator_envelope");
        assert_eq!(
            ZoneKind::OperatorEnvelopeBackup.type_label(),
            "operator_envelope_backup"
        );
        assert_ne!(
            ZoneKind::OperatorEnvelope.type_label(),
            ZoneKind::OperatorEnvelopeBackup.type_label()
        );
    }

    #[test]
    fn from_type_label_round_trips_every_kind_type_label_produces() {
        // Every string type_label() can produce must round-trip through
        // from_type_label() back to a kind with the SAME type_label() (the
        // id/tenant_id payload is necessarily lost for Slice/TenantEnvelope,
        // since a front index's plaintext `type` field never carries one —
        // volume_verify's post-hoc reconstruction depends on exactly this).
        let samples = [
            ZoneKind::IdThunk,
            ZoneKind::SystemGuide,
            ZoneKind::RestoreSh,
            ZoneKind::FrontIndex,
            ZoneKind::Slice { stage_slice_id: 42 },
            ZoneKind::TenantEnvelope { tenant_id: 7 },
            ZoneKind::OperatorEnvelope,
            ZoneKind::OperatorEnvelopeBackup,
            ZoneKind::SealMarker,
        ];
        for kind in samples {
            let label = kind.type_label();
            let round_tripped = ZoneKind::from_type_label(label)
                .unwrap_or_else(|| panic!("from_type_label(\"{label}\") returned None"));
            assert_eq!(
                round_tripped.type_label(),
                label,
                "round-trip through from_type_label changed the type_label for {kind:?}"
            );
        }
    }

    #[test]
    fn from_type_label_rejects_unrecognized_strings() {
        assert_eq!(ZoneKind::from_type_label("not_a_real_type"), None);
        assert_eq!(ZoneKind::from_type_label(""), None);
        // Case-sensitive: the format is a fixed lower-case vocabulary
        // (`volume-format-v2.md` §3), not case-insensitive matching.
        assert_eq!(ZoneKind::from_type_label("Seal_Marker"), None);
    }
}
