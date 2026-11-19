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

use crate::pass_count::Sha256;
use sha2::Digest;
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
    /// A tenant envelope's backup copy (`volume-format-v2.md` §1.3, #418
    /// item 2): the primary's exact bytes, after the operator pair, in the
    /// primaries' order. Keyed by `tenants.id` like its primary.
    TenantEnvelopeBackup {
        tenant_id: i64,
    },
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
            ZoneKind::TenantEnvelopeBackup { .. } => "tenant_envelope_backup",
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
            "tenant_envelope_backup" => ZoneKind::TenantEnvelopeBackup { tenant_id: 0 },
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

/// The capacity a Layout must fit inside. `available_bytes` is the fill
/// budget (the cartridge's capacity times the drive's fill ceiling);
/// `reserve_bytes` is the end-of-tape reserve (`enospc_buffer`), counted
/// against it.
///
/// **The plan bound** (ADR-0012 item 113; `volume-format-v2.md` §4.3, its
/// closing paragraph). `plan_bound_bytes` is `Some(capacity)` — 100% of the
/// cartridge's nominal capacity, the figure `volume init` decided (ADR-0010)
/// — only when the writing drive declares `post_warning_bytes`
/// ([`crate::config::LtoBackendConfig::plan_bound_bytes`]). Then the units
/// are split in the caller's order ([`CapacityBudget::split`]): the prefix
/// of whole units that fits the fill budget, with the reserve, is the
/// committed plan and today's gate applies to it; whole units after it are
/// planned past the fill ceiling while the plan's end stays within the
/// bound. They are ordinary entries: the front index lists them, and a short
/// seal may cut them. `None` (no `post_warning_bytes`) stops every plan at
/// the fill ceiling exactly as before 1.6.0.
///
/// The reserve is the fill ceiling's: the bound is the plan's end itself,
/// with no reserve under it. Past the ceiling the defence is the early
/// warning and the short seal (§4.3 W4), not a margin.
///
/// Frozen into the session's `layout.json` with the rest of the Layout, so a
/// resume revalidates against the bound the plan was made with. Absent from
/// that file when `None`, which is how it reads back when absent: the
/// writer omits the key, so its absence is the current shape, not an older
/// one.
///
/// **The span's tail** (ADR-0012 item 123; `layout-session.md` "Spanning").
/// `span_tail_from` is the position of the first slice of the plan's last
/// unit when that unit may span: the one unit the gate lets pass the plan
/// bound ([`Layout::capacity_errors_with_tail`]). The plan order puts the
/// tail last ([`CapacityBudget::place`]), so the last unit of a span tape
/// is the only one that can be it. Frozen with the bound, so a resume
/// revalidates the tail the plan was made with. Absent from `layout.json`
/// when `None`, as the bound is: a plan with no unit that may span writes
/// no key, and reads back as no tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CapacityBudget {
    pub available_bytes: u64,
    pub reserve_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_bound_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_tail_from: Option<i32>,
}

impl CapacityBudget {
    /// Where a set whose slices end the plan at `end` sits: under the fill
    /// ceiling, past it within the plan bound, or across the bound or the
    /// ceiling, which makes it the plan's [`Placed::Tail`] (items 123, 288
    /// and 289).
    fn placed_at(&self, end: u64) -> Placed {
        if end.saturating_add(self.reserve_bytes) <= self.available_bytes {
            return Placed::Committed;
        }
        match self.plan_bound_bytes {
            Some(bound) if end <= bound => Placed::PastCeiling,
            _ => Placed::Tail,
        }
    }

    /// The plan order of one tape (`layout-session.md` "The plan order";
    /// ADR-0012 items 122, 123, 288 and 289, D20), on block-padded sizes
    /// alone. `fixed_bytes` is every file of the plan that is not a data
    /// slice, block-padded (Files 0-3, the envelopes and their backups, the
    /// seal); `items` are the stage sets in the caller's order, each with
    /// its [`PlanClass`]. The plan is, in order:
    ///
    /// 1. **Owed runs**, in the caller's order (item 122): a set in
    ///    progress lays its owed slices first, as one unbroken run in slice
    ///    order.
    /// 2. **The queue**, in the caller's order. There is no second class and
    ///    nothing is deferred behind another set.
    ///
    /// **The bound** (D20, item 289). On a drive that declares
    /// `post_warning_bytes` ([`Self::plan_bound_bytes`]) sets are added
    /// while the plan's end is at or below the cartridge's nominal capacity;
    /// the one that carries the plan past it is the [`Placed::Tail`],
    /// planned to its end, and no set is planned after it (the early warning
    /// ends it on the tape). An owed run is held to the same rule. On a
    /// drive without the key, sets are added while the plan's end is at or
    /// below the fill ceiling (with the reserve); the set that carries it
    /// past is the [`Placed::Tail`] again, and the plan ends at the last
    /// slice of it that fits, a prefix of its slices that the caller finds
    /// from [`Placement::tail_room`]. Nothing is refused for the missing
    /// key.
    pub fn place(&self, fixed_bytes: u64, items: &[PlanItem]) -> Placement {
        let owed = (0..items.len()).filter(|&i| items[i].class == PlanClass::Owed);
        let queued = (0..items.len()).filter(|&i| items[i].class == PlanClass::Queued);
        let mut placed = vec![Placed::Deferred; items.len()];
        let mut order = Vec::new();
        let mut tail_room = None;
        let mut end = fixed_bytes;
        for i in owed.chain(queued) {
            let before = end;
            end = end.saturating_add(items[i].bytes);
            let at = self.placed_at(end);
            placed[i] = at;
            order.push(i);
            if at == Placed::Tail {
                if self.plan_bound_bytes.is_none() {
                    tail_room = Some(
                        self.available_bytes
                            .saturating_sub(self.reserve_bytes)
                            .saturating_sub(before),
                    );
                }
                break;
            }
        }
        Placement {
            order,
            placed,
            tail_room,
        }
    }
}

/// What a stage set is to the plan order (`layout-session.md` "The plan
/// order"; ADR-0012 items 122 and 288). The caller knows which a set is: the
/// catalog's rows say whether it has a range on a sealed volume and slices
/// still owed, and a Layout knows neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanClass {
    /// A span in progress: a set with a `range` row on a sealed volume and
    /// slices still owed. Its owed run, from its lowest owed slice to its
    /// highest, goes first (item 122).
    Owed,
    /// Every other set the write takes, with its whole slice list, in the
    /// order the write takes them (item 288: no set is whole or cut).
    Queued,
}

/// One set of a plan for [`CapacityBudget::place`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanItem {
    pub class: PlanClass,
    /// The slices this tape plans of it, block-padded.
    pub bytes: u64,
}

/// Where [`CapacityBudget::place`] put one set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placed {
    /// Within the fill ceiling, with the reserve: the committed plan.
    Committed,
    /// Past the fill ceiling and within the plan bound (item 113): the
    /// early warning, not the plan, may end the tape inside it.
    PastCeiling,
    /// The set that carries the plan past its bound (items 123 and 289):
    /// with the key, planned to its end, and the early warning ends its
    /// range; without it, planned up to the fill ceiling
    /// ([`Placement::tail_room`]). At most one, and the last of the plan.
    Tail,
    /// Not on this tape: queued after the tail. It stays staged and waits
    /// for a later tape.
    Deferred,
}

/// The plan order of one tape ([`CapacityBudget::place`]).
#[derive(Debug, PartialEq, Eq)]
pub struct Placement {
    /// The sets this tape plans, as indices into the caller's items, in
    /// tape order: owed runs, then the queue. No deferred set is in it.
    pub order: Vec<usize>,
    /// Where each set went, by the caller's index.
    pub placed: Vec<Placed>,
    /// On a drive with no plan bound: the bytes, block-padded, that the
    /// tail's slices may take before the fill ceiling (less the reserve). The
    /// tail is planned as the longest prefix of its slices that fits in it,
    /// and is deferred when not even the first does. `None` with a bound, and
    /// when there is no tail.
    pub tail_room: Option<u64>,
}

impl Placement {
    /// The set planned past the bound to its end, if any (item 123).
    pub fn tail(&self) -> Option<usize> {
        self.placed.iter().position(|&p| p == Placed::Tail)
    }

    /// The sets left for a later tape, in the caller's order.
    pub fn deferred(&self) -> Vec<usize> {
        self.with(Placed::Deferred)
    }

    /// The sets placed `p`, in the caller's order.
    pub fn with(&self, p: Placed) -> Vec<usize> {
        (0..self.placed.len())
            .filter(|&i| self.placed[i] == p)
            .collect()
    }
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

/// The sentence a capacity refusal ends with (issue #391): the fill ceiling
/// and the capacity it is a fraction of, and the two ways forward. `None`
/// unless `errs` holds a [`LayoutError::CapacityExceeded`], so a caller can
/// append it to any refusal unconditionally.
///
/// It names `--fill-ceiling`, so it belongs only to a write planned afresh:
/// `volume write` and the commands that end in one (`volume compact-write`,
/// `volume compact`, `quick-archive`, `collection run`), which all take the
/// flag. `volume resume` never reaches it — it revalidates the frozen
/// layout against the budget that layout was planned with, which cannot
/// newly exceed it.
pub fn capacity_refusal_note(
    errs: &[LayoutError],
    fill_ceiling: f64,
    capacity_bytes: u64,
) -> Option<String> {
    errs.iter()
        .any(|e| matches!(e, LayoutError::CapacityExceeded { .. }))
        .then(|| {
            format!(
                "the fill ceiling is {}% of this volume's {} capacity (ADR-0012). Write less \
                 to this volume, or, if you know this cartridge can take it, raise the ceiling \
                 for this write with --fill-ceiling",
                fill_ceiling_percent(fill_ceiling),
                crate::util::format_bytes_decimal(
                    i64::try_from(capacity_bytes).unwrap_or(i64::MAX)
                ),
            )
        })
}

/// A fill ceiling as a percentage for a message: `0.97` -> `"97"`,
/// `0.975` -> `"97.5"`.
pub fn fill_ceiling_percent(fill_ceiling: f64) -> String {
    let pct = (fill_ceiling * 1000.0).round() / 10.0;
    if pct.fract() == 0.0 {
        format!("{}", pct as i64)
    } else {
        format!("{pct}")
    }
}

/// A validation failure. `validate` collects all failures rather than
/// stopping at the first, because this is a pre-flight report.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LayoutError {
    #[error("entry at position {position} ({label}) has no size")]
    Unsized { position: i32, label: &'static str },
    #[error("slice at position {position} has no recorded sha256")]
    SliceMissingChecksum { position: i32 },
    /// Issue #391: names the overage, not only the two sides of the sum.
    /// `available` is the volume's capacity times its fill ceiling;
    /// [`capacity_refusal_note`] adds the ceiling and the capacity it came
    /// from where the caller knows them.
    #[error(
        "capacity exceeded: the layout needs {needed} bytes on tape plus a {reserve}-byte \
         end-of-tape reserve, {over} bytes ({over_h}) over the {available} bytes this volume \
         may be filled to",
        over = (needed + reserve).saturating_sub(*available),
        over_h = crate::util::format_bytes_binary(
            i64::try_from((needed + reserve).saturating_sub(*available)).unwrap_or(i64::MAX)
        ),
    )]
    CapacityExceeded {
        needed: u64,
        reserve: u64,
        available: u64,
    },
    /// ADR-0012 item 113: a plan that reaches past the fill ceiling (the
    /// drive declares `post_warning_bytes`) and past the cartridge's nominal
    /// capacity too. Not [`Self::CapacityExceeded`]: raising the fill
    /// ceiling ([`capacity_refusal_note`]) cannot help a plan the whole
    /// cartridge cannot hold.
    #[error(
        "capacity exceeded: the layout needs {needed} bytes on tape, {over} bytes ({over_h}) \
         over the {bound} bytes of this volume's nominal capacity, the most a plan may reach \
         past the fill ceiling (ADR-0012 item 113)",
        over = needed.saturating_sub(*bound),
        over_h = crate::util::format_bytes_binary(
            i64::try_from(needed.saturating_sub(*bound)).unwrap_or(i64::MAX)
        ),
    )]
    PlanBoundExceeded { needed: u64, bound: u64 },
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
         it: tapectl stage clean --force (releases the old set), then \
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

    /// The capacity gate. Without a plan bound, today's: every file plus the
    /// reserve within the fill budget. With one (ADR-0012 item 113), a plan
    /// over the fill budget may stand: its committed prefix of whole units
    /// fits the fill budget by construction ([`CapacityBudget::split`]), so
    /// today's gate is left to apply to what every committed plan holds —
    /// the files that are not data slices, plus the reserve — and the whole
    /// plan must end within the bound. The gate needs no unit boundaries:
    /// the Layout knows none, and these two conditions are what the split's
    /// definition leaves to check.
    fn check_capacity(&self, errs: &mut Vec<LayoutError>) {
        if let Err(mut unsized_errs) = self.on_tape_bytes() {
            errs.append(&mut unsized_errs);
            return;
        }
        errs.extend(self.capacity_errors_against(self.budget.available_bytes));
    }

    /// The capacity gate of [`Self::validate`] with another fill budget in
    /// place of the budget's own `available_bytes` — the live
    /// `Store::capacity()` read at contact (`BuiltLayout::into_validated`) —
    /// and the budget's own reserve, plan bound and span tail
    /// ([`CapacityBudget::span_tail_from`], [`Self::capacity_errors_with_tail`]).
    /// Empty when the Layout fits, and when an entry is unsized
    /// ([`Self::validate`] reports that). One gate, so the contact check
    /// cannot refuse a plan past the fill ceiling, or a span's tail past the
    /// bound, that the pre-flight accepted.
    pub fn capacity_errors_against(&self, available_bytes: u64) -> Vec<LayoutError> {
        self.capacity_errors_with_tail(available_bytes, self.budget.span_tail_from)
    }

    /// [`Self::capacity_errors_against`] for a plan whose last unit is the
    /// plan's tail ([`Placed::Tail`], ADR-0012 items 123 and 289): `tail_from` is the
    /// position of the tail's first slice, and every slice from there to the
    /// seal is the tail. Only the tail may pass the plan bound: the rest of
    /// the plan, the seal included, must end within it, which puts the
    /// tail's start before the bound (it crosses the bound, it does not lie
    /// beyond it). Today's committed gate still applies (the files that are
    /// not slices, plus the reserve, within the fill budget). Without a plan
    /// bound there is no tail: the plan fits the fill budget or is refused.
    /// `None`, or a position after every slice, is 1.6.0's gate exactly.
    /// [`Self::capacity_errors_against`] passes the budget's own
    /// [`CapacityBudget::span_tail_from`].
    pub fn capacity_errors_with_tail(
        &self,
        available_bytes: u64,
        tail_from: Option<i32>,
    ) -> Vec<LayoutError> {
        let Ok(needed) = self.on_tape_bytes() else {
            return Vec::new();
        };
        let reserve = self.budget.reserve_bytes;
        if needed + reserve <= available_bytes {
            return Vec::new();
        }
        // Without a plan bound the plan stops at the fill ceiling, a tail
        // trimmed to the slices that fit it (item 289, D20): nothing passes.
        let Some(bound) = self.budget.plan_bound_bytes else {
            return vec![LayoutError::CapacityExceeded {
                needed,
                reserve,
                available: available_bytes,
            }];
        };
        let tail = tail_from.map_or(0, |from| self.slice_bytes_from(from));
        let before_tail = needed - tail;
        let mut errs = Vec::new();
        let fixed = self.fixed_on_tape_bytes();
        if fixed + reserve > available_bytes {
            errs.push(LayoutError::CapacityExceeded {
                needed: fixed,
                reserve,
                available: available_bytes,
            });
        }
        if before_tail > bound {
            errs.push(LayoutError::PlanBoundExceeded {
                needed: before_tail,
                bound,
            });
        }
        errs
    }

    /// The block-padded bytes of the data slices at `from` and after.
    fn slice_bytes_from(&self, from: i32) -> u64 {
        self.entries
            .iter()
            .filter(|e| e.kind.is_slice() && e.position >= from)
            .filter_map(|e| e.on_tape_bytes(self.block_size))
            .sum()
    }

    /// Every entry that is not a data slice, block-padded: the part of the
    /// plan every unit split leaves in place (Files 0-3, the envelopes and
    /// their backups, the seal). An unsized entry counts nothing here;
    /// [`Self::validate`] reports it.
    pub fn fixed_on_tape_bytes(&self) -> u64 {
        self.entries
            .iter()
            .filter(|e| !e.kind.is_slice())
            .filter_map(|e| e.on_tape_bytes(self.block_size))
            .sum()
    }

    /// [`CapacityBudget::place`] of units given as each one's slices' `stage_slice_id`s in the caller's order,
    /// each with its [`PlanClass`] (`classes[i]` is `unit_slice_ids[i]`'s),
    /// against this Layout's budget and its files that are not slices. The
    /// placement is of these sizes in the plan order, whatever order the
    /// Layout holds them in: it describes this Layout only when the Layout
    /// was built in [`Placement::order`] with no deferred unit.
    pub fn place_units(&self, unit_slice_ids: &[Vec<i64>], classes: &[PlanClass]) -> Placement {
        let items: Vec<PlanItem> = self
            .unit_bytes(unit_slice_ids)
            .into_iter()
            .zip(classes)
            .map(|(bytes, &class)| PlanItem { class, bytes })
            .collect();
        self.budget.place(self.fixed_on_tape_bytes(), &items)
    }

    /// Each unit's slices, block-padded. A slice id the Layout does not
    /// hold, or an unsized slice, counts nothing.
    fn unit_bytes(&self, unit_slice_ids: &[Vec<i64>]) -> Vec<u64> {
        let padded: std::collections::HashMap<i64, u64> = self
            .entries
            .iter()
            .filter_map(|e| match e.kind {
                ZoneKind::Slice { stage_slice_id } => e
                    .on_tape_bytes(self.block_size)
                    .map(|bytes| (stage_slice_id, bytes)),
                _ => None,
            })
            .collect();
        unit_slice_ids
            .iter()
            .map(|ids| ids.iter().filter_map(|id| padded.get(id)).sum())
            .collect()
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
    let mut f = crate::pass_count::StagedRead::open(path)?;
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
                plan_bound_bytes: None,
                span_tail_from: None,
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

    // ── the plan bound (ADR-0012 item 113) ──

    fn slice_entry(position: i32, stage_slice_id: i64, size: u64) -> LayoutEntry {
        LayoutEntry {
            position,
            kind: ZoneKind::Slice { stage_slice_id },
            size_bytes: Some(size),
            sha256: Some("ab".into()),
            source: ContentSource::Generated,
        }
    }

    /// Two fixed blocks (File 0 at 0, the seal at 5) and three units in the
    /// caller's order: unit A = slices 10, 11 (2 blocks each, positions 1-2),
    /// unit B = slice 20 (3 blocks, position 3), unit C = slice 30 (1 block,
    /// position 4). Ten blocks in all.
    fn three_unit_layout(budget: CapacityBudget) -> (Layout, Vec<Vec<i64>>) {
        let entries = vec![
            gen_entry(0, ZoneKind::IdThunk, BS),
            slice_entry(1, 10, 2 * BS),
            slice_entry(2, 11, 2 * BS - 7),
            slice_entry(3, 20, 3 * BS),
            slice_entry(4, 30, 1),
            gen_entry(5, ZoneKind::SealMarker, BS),
        ];
        let mut layout = layout_with(entries, 0, 0);
        layout.budget = budget;
        (layout, vec![vec![10, 11], vec![20], vec![30]])
    }

    fn budget(available: u64, reserve: u64, bound: Option<u64>) -> CapacityBudget {
        CapacityBudget {
            available_bytes: available,
            reserve_bytes: reserve,
            plan_bound_bytes: bound,
            span_tail_from: None,
        }
    }

    /// The plan of units given as block counts, all queued.
    fn queued(blocks: &[u64]) -> Vec<PlanItem> {
        blocks.iter().map(|&b| item(PlanClass::Queued, b)).collect()
    }

    #[test]
    fn the_plan_appends_sets_past_the_ceiling_up_to_the_bound_in_order() {
        // Fixed 2 + A 4 = 6 blocks: A is committed under a 7-block fill
        // budget. B ends at 9 and C at 10, both past the ceiling. The order
        // is the caller's and is kept.
        let items = queued(&[4, 3, 1]);
        let p = budget(7 * BS, 0, Some(10 * BS)).place(2 * BS, &items);
        assert_eq!(
            p.placed,
            vec![Placed::Committed, Placed::PastCeiling, Placed::PastCeiling]
        );
        assert_eq!(p.order, vec![0, 1, 2]);
        assert_eq!((p.tail(), p.deferred(), p.tail_room), (None, vec![], None));

        // The bound is inclusive: B ends at exactly 9 blocks and is planned
        // past the ceiling; C would end at 10, which crosses it, so C is the
        // tail, planned to its end, and nothing follows it.
        let p = budget(7 * BS, 0, Some(9 * BS)).place(2 * BS, &items);
        assert_eq!(
            p.placed,
            vec![Placed::Committed, Placed::PastCeiling, Placed::Tail]
        );
        assert_eq!(p.tail(), Some(2));

        // The set that crosses the bound is the last planned, and a set after
        // it, even one that would fit, waits.
        let items = queued(&[4, 3, 0]);
        let p = budget(7 * BS, 0, Some(9 * BS - 1)).place(2 * BS, &items);
        assert_eq!(
            p.placed,
            vec![Placed::Committed, Placed::Tail, Placed::Deferred]
        );
        assert_eq!(p.order, vec![0, 1]);
        assert_eq!(p.deferred(), vec![2]);
    }

    #[test]
    fn the_reserve_counts_against_the_fill_budget_and_not_against_the_bound() {
        let items = queued(&[4, 3, 1]);
        // With a 1-block reserve, A (end 6) still commits at 7; the bound
        // (10 blocks, the plan's end itself) still takes C.
        let p = budget(7 * BS, BS, Some(10 * BS)).place(2 * BS, &items);
        assert_eq!(p.with(Placed::Committed), vec![0]);
        assert_eq!(p.with(Placed::PastCeiling), vec![1, 2]);
        // A 2-block reserve pushes A past the ceiling too.
        let p = budget(7 * BS, 2 * BS, Some(10 * BS)).place(2 * BS, &items);
        assert_eq!(p.with(Placed::Committed), Vec::<usize>::new());
        assert_eq!(p.with(Placed::PastCeiling), vec![0, 1, 2]);
    }

    /// Item 289, D20: without `post_warning_bytes` the plan stops at the fill
    /// ceiling. The set that carries it past is the tail, and its room is
    /// what the ceiling leaves, less the reserve; the caller plans the
    /// longest prefix of its slices that fits. Nothing is refused.
    #[test]
    fn without_a_plan_bound_the_tail_is_planned_up_to_the_fill_ceiling() {
        let items = queued(&[4, 3, 1]);
        let p = budget(7 * BS, 0, None).place(2 * BS, &items);
        assert_eq!(
            p.placed,
            vec![Placed::Committed, Placed::Tail, Placed::Deferred]
        );
        // 7 blocks of fill, 2 fixed, 4 taken by A: one block is left for B.
        assert_eq!(p.tail_room, Some(BS));
        let p = budget(7 * BS, BS, None).place(2 * BS, &items);
        assert_eq!(p.tail_room, Some(0), "the reserve comes off the room");
        // Everything under the ceiling: all committed, with or without a
        // bound, and no tail room.
        for bound in [None, Some(100 * BS)] {
            let p = budget(100 * BS, BS, bound).place(2 * BS, &items);
            assert_eq!(p.with(Placed::Committed), vec![0, 1, 2], "{bound:?}");
            assert_eq!((p.tail(), p.tail_room), (None, None), "{bound:?}");
        }
    }

    #[test]
    fn a_layout_places_its_units_by_their_padded_slices() {
        let (layout, units) = three_unit_layout(budget(7 * BS, 0, Some(10 * BS)));
        // Slice 11 is 7 bytes short of 2 blocks and pads to 2; slice 30 (1
        // byte) pads to 1 block.
        assert_eq!(layout.fixed_on_tape_bytes(), 2 * BS);
        assert_eq!(layout.on_tape_bytes().unwrap(), 10 * BS);
        let p = layout.place_units(&units, &[PlanClass::Queued; 3]);
        assert_eq!(
            p.placed,
            vec![Placed::Committed, Placed::PastCeiling, Placed::PastCeiling]
        );
    }

    #[test]
    fn with_a_plan_bound_a_plan_past_the_ceiling_validates_up_to_nominal() {
        // Ten blocks against a 7-block fill budget: refused without the
        // bound (today's gate, unchanged) ...
        let (layout, _) = three_unit_layout(budget(7 * BS, 0, None));
        let errs = layout
            .validate(&keys_ok(&[]), SliceCheck::Size)
            .unwrap_err();
        assert_eq!(
            errs,
            vec![LayoutError::CapacityExceeded {
                needed: 10 * BS,
                reserve: 0,
                available: 7 * BS
            }]
        );
        // ... accepted with a bound of exactly the plan's end ...
        let (layout, _) = three_unit_layout(budget(7 * BS, BS, Some(10 * BS)));
        assert!(layout.validate(&keys_ok(&[]), SliceCheck::Size).is_ok());
        // ... and refused one byte under it, by the bound's own error, which
        // gets no --fill-ceiling note.
        let (layout, _) = three_unit_layout(budget(7 * BS, BS, Some(10 * BS - 1)));
        let errs = layout
            .validate(&keys_ok(&[]), SliceCheck::Size)
            .unwrap_err();
        assert_eq!(
            errs,
            vec![LayoutError::PlanBoundExceeded {
                needed: 10 * BS,
                bound: 10 * BS - 1
            }]
        );
        assert_eq!(capacity_refusal_note(&errs, 0.97, 10 * BS), None);
        let msg = errs[0].to_string();
        assert!(
            msg.contains("1 bytes") && msg.contains("nominal capacity") && msg.contains("113"),
            "{msg}"
        );
    }

    #[test]
    fn with_a_plan_bound_the_committed_plan_still_meets_todays_gate() {
        // The files that are not slices, plus the reserve, are in every
        // committed plan: over the fill budget they are refused as today,
        // whatever the bound.
        let (layout, _) = three_unit_layout(budget(2 * BS, BS, Some(100 * BS)));
        let errs = layout
            .validate(&keys_ok(&[]), SliceCheck::Size)
            .unwrap_err();
        assert_eq!(
            errs,
            vec![LayoutError::CapacityExceeded {
                needed: 2 * BS,
                reserve: BS,
                available: 2 * BS
            }]
        );
        // At exactly the fill budget they pass.
        let (layout, _) = three_unit_layout(budget(3 * BS, BS, Some(100 * BS)));
        assert!(layout.validate(&keys_ok(&[]), SliceCheck::Size).is_ok());
    }

    /// The contact check's gate (a live fill budget in place of the Layout's)
    /// is the same gate: a plan past the ceiling that the pre-flight accepts,
    /// the live figure accepts too, and without a bound it refuses as before.
    #[test]
    fn the_gate_against_a_live_fill_budget_keeps_the_plan_bound() {
        let (bounded, _) = three_unit_layout(budget(7 * BS, BS, Some(10 * BS)));
        assert!(bounded.capacity_errors_against(7 * BS).is_empty());
        assert_eq!(
            bounded.capacity_errors_against(2 * BS),
            vec![LayoutError::CapacityExceeded {
                needed: 2 * BS,
                reserve: BS,
                available: 2 * BS
            }]
        );
        let (unbounded, _) = three_unit_layout(budget(7 * BS, BS, None));
        assert_eq!(
            unbounded.capacity_errors_against(7 * BS),
            vec![LayoutError::CapacityExceeded {
                needed: 10 * BS,
                reserve: BS,
                available: 7 * BS
            }]
        );
        assert!(unbounded.capacity_errors_against(11 * BS).is_empty());
        // An unsized entry is validate's to report, not this gate's.
        let mut unsized_layout = unbounded.clone();
        unsized_layout.entries[1].size_bytes = None;
        assert!(unsized_layout.capacity_errors_against(0).is_empty());
    }

    #[test]
    fn a_plan_bound_never_refuses_what_the_fill_ceiling_accepts() {
        // `capacity_uses_block_padded_sizes`' passing layouts still pass with
        // a bound, even one below the fill budget: the gate returns before
        // the bound is read when the plan fits under the ceiling.
        let entries = vec![
            gen_entry(0, ZoneKind::IdThunk, 1),
            gen_entry(1, ZoneKind::SystemGuide, 1),
            gen_entry(2, ZoneKind::RestoreSh, 1),
        ];
        for (available, reserve) in [(4 * BS, BS), (100 * BS, BS), (3 * BS, 0)] {
            let mut l = layout_with(entries.clone(), available, reserve);
            assert!(l.validate(&keys_ok(&[]), SliceCheck::Size).is_ok());
            l.budget.plan_bound_bytes = Some(3 * BS);
            assert!(l.validate(&keys_ok(&[]), SliceCheck::Size).is_ok());
        }
    }

    #[test]
    fn the_plan_bound_is_absent_from_a_frozen_layout_without_one() {
        // A session's `layout.json` without the bound omits the key, and
        // reads back as no bound.
        let none = budget(7, 1, None);
        let json = serde_json::to_string(&none).unwrap();
        assert_eq!(json, r#"{"available_bytes":7,"reserve_bytes":1}"#);
        let old: CapacityBudget = serde_json::from_str(&json).unwrap();
        assert_eq!(old, none);
        let some = budget(7, 1, Some(9));
        let json = serde_json::to_string(&some).unwrap();
        assert!(json.contains(r#""plan_bound_bytes":9"#), "{json}");
        assert!(!json.contains("span_tail_from"), "{json}");
        assert_eq!(serde_json::from_str::<CapacityBudget>(&json).unwrap(), some);
        // A span tape's tail (ADR-0012 item 123) round-trips with the bound,
        // and a plan with a bound and no tail (the writer omits the key)
        // reads as no tail.
        let span = CapacityBudget {
            span_tail_from: Some(12),
            ..some
        };
        let json = serde_json::to_string(&span).unwrap();
        assert_eq!(
            json,
            r#"{"available_bytes":7,"reserve_bytes":1,"plan_bound_bytes":9,"span_tail_from":12}"#
        );
        assert_eq!(serde_json::from_str::<CapacityBudget>(&json).unwrap(), span);
        let no_tail: CapacityBudget =
            serde_json::from_str(r#"{"available_bytes":7,"reserve_bytes":1,"plan_bound_bytes":9}"#)
                .unwrap();
        assert_eq!(no_tail.span_tail_from, None);
    }

    /// The gate is wired to the frozen tail: a span Layout whose tail crosses
    /// the bound passes `capacity_errors_against` and `validate`, and the
    /// same Layout without `span_tail_from` is refused by the bound, as a
    /// whole-unit plan is. The frozen Layout's JSON round trip keeps it.
    #[test]
    fn the_wired_gate_lets_a_frozen_span_layouts_tail_pass_the_bound() {
        // three_unit_layout: File 0, A (positions 1-2, 4 blocks), B (3, 3
        // blocks), C (4, 1 block), the seal; ten blocks. B and C are the tail
        // from position 3; everything else takes 6 blocks, the bound.
        let mut b = budget(5 * BS, 0, Some(6 * BS));
        b.span_tail_from = Some(3);
        let (span, _) = three_unit_layout(b);
        let frozen: Layout = serde_json::from_str(&serde_json::to_string(&span).unwrap()).unwrap();
        assert_eq!(frozen, span);
        assert!(frozen.capacity_errors_against(5 * BS).is_empty());
        assert!(frozen.validate(&keys_ok(&[]), SliceCheck::Size).is_ok());

        let mut whole = frozen.clone();
        whole.budget.span_tail_from = None;
        let refused = vec![LayoutError::PlanBoundExceeded {
            needed: 10 * BS,
            bound: 6 * BS,
        }];
        assert_eq!(whole.capacity_errors_against(5 * BS), refused);
        assert_eq!(
            whole.validate(&keys_ok(&[]), SliceCheck::Size).unwrap_err(),
            refused
        );
    }

    // ── the plan order (ADR-0012 items 113, 122, 123, 288 and 289) ──

    fn item(class: PlanClass, blocks: u64) -> PlanItem {
        PlanItem {
            class,
            bytes: blocks * BS,
        }
    }

    /// `layout-session.md` "The plan order": owed runs, then the queue, each
    /// in the caller's order. Item 93's second tape is the first case: the
    /// owed run is the whole set, and it leads.
    #[test]
    fn the_plan_order_is_owed_runs_then_the_queue() {
        use PlanClass::{Owed, Queued};
        let items = [
            item(Queued, 1),
            item(Queued, 2),
            item(Owed, 3),
            item(Queued, 1),
            item(Queued, 2),
            item(Owed, 1),
        ];
        for bound in [Some(200 * BS), None] {
            let p = budget(100 * BS, BS, bound).place(2 * BS, &items);
            assert_eq!(p.order, vec![2, 5, 0, 1, 3, 4], "{bound:?}");
            assert!(p.placed.iter().all(|&x| x == Placed::Committed), "{p:?}");
            assert_eq!((p.tail(), p.deferred()), (None, vec![]));
        }
    }

    /// Item 289, D20: the plan adds sets while its end is at or below the
    /// bound; the set that carries it past is the tail, planned to its end,
    /// and no set is planned after it (items 123 and 288: nothing is
    /// refused, whatever a set's size).
    #[test]
    fn the_set_that_crosses_the_bound_is_the_last_planned_whatever_its_size() {
        use PlanClass::Queued;
        // Fixed 1 block, fill 8, bound 10. A 5 (end 6), B 100 (end 106: the
        // tail, larger than the whole tape), C waits.
        let items = [item(Queued, 5), item(Queued, 100), item(Queued, 1)];
        let p = budget(8 * BS, 0, Some(10 * BS)).place(BS, &items);
        assert_eq!(
            p.placed,
            vec![Placed::Committed, Placed::Tail, Placed::Deferred]
        );
        assert_eq!(p.order, vec![0, 1]);
        assert_eq!(
            p.tail_room, None,
            "with a bound the tail is planned to its end"
        );
        // Alone, a set larger than a tape is the tail, never refused.
        let p = budget(8 * BS, 0, Some(10 * BS)).place(BS, &items[1..2]);
        assert_eq!(p.placed, vec![Placed::Tail]);
        let p = budget(8 * BS, 0, None).place(BS, &items[1..2]);
        assert_eq!((p.placed, p.tail_room), (vec![Placed::Tail], Some(7 * BS)));
    }

    /// An owed run is held to the same rule: one that crosses the bound is
    /// the tail, and the queue after it waits.
    #[test]
    fn an_owed_run_that_crosses_the_bound_is_the_tail_and_the_queue_waits() {
        use PlanClass::{Owed, Queued};
        let items = [item(Queued, 1), item(Owed, 20), item(Queued, 1)];
        let p = budget(8 * BS, 0, Some(10 * BS)).place(BS, &items);
        assert_eq!(p.order, vec![1]);
        assert_eq!(
            p.placed,
            vec![Placed::Deferred, Placed::Tail, Placed::Deferred]
        );
    }

    /// A Layout's units are placed by their padded slices, in the plan order
    /// whatever order the Layout holds them in.
    #[test]
    fn a_layouts_units_are_placed_in_the_plan_order() {
        use PlanClass::{Owed, Queued};
        // three_unit_layout: fixed 2, A 4 blocks, B 3, C 1. C is an owed run.
        let (layout, units) = three_unit_layout(budget(7 * BS, 0, Some(8 * BS)));
        let p = layout.place_units(&units, &[Queued, Queued, Owed]);
        assert_eq!(p.order, vec![2, 0, 1]);
        assert_eq!(
            p.placed,
            vec![Placed::Committed, Placed::Tail, Placed::Committed]
        );
    }

    /// Item 123 at the gate: only a span's tail may pass the plan bound.
    /// Everything before the tail, the seal included, ends within it.
    #[test]
    fn the_gate_lets_only_a_spans_tail_pass_the_bound() {
        // Ten blocks: File 0, A (positions 1-2, 4 blocks), B (3, 3 blocks),
        // C (4, 1 block), the seal. A bound of 6 blocks.
        let (layout, _) = three_unit_layout(budget(5 * BS, 0, Some(6 * BS)));
        let as_whole = vec![LayoutError::PlanBoundExceeded {
            needed: 10 * BS,
            bound: 6 * BS,
        }];
        assert_eq!(layout.capacity_errors_against(5 * BS), as_whole);
        // B and C the tail: File 0, A and the seal take the bound's 6 blocks.
        assert!(layout.capacity_errors_with_tail(5 * BS, Some(3)).is_empty());
        // C alone the tail: B is a whole unit beyond the bound, refused.
        assert_eq!(
            layout.capacity_errors_with_tail(5 * BS, Some(4)),
            vec![LayoutError::PlanBoundExceeded {
                needed: 9 * BS,
                bound: 6 * BS
            }]
        );
        // A position after every slice is no tail.
        assert_eq!(layout.capacity_errors_with_tail(5 * BS, Some(5)), as_whole);
        // Today's committed gate still holds under a tail.
        let (tight, _) = three_unit_layout(budget(2 * BS, BS, Some(6 * BS)));
        assert_eq!(
            tight.capacity_errors_with_tail(2 * BS, Some(3)),
            vec![LayoutError::CapacityExceeded {
                needed: 2 * BS,
                reserve: BS,
                available: 2 * BS
            }]
        );
        // Without a bound there is no tail (item 289): the plan stops at the
        // fill ceiling, and a layout past it is refused as one.
        let (unbounded, _) = three_unit_layout(budget(7 * BS, 0, None));
        assert_eq!(
            unbounded.capacity_errors_with_tail(7 * BS, Some(3)),
            vec![LayoutError::CapacityExceeded {
                needed: 10 * BS,
                reserve: 0,
                available: 7 * BS
            }]
        );
        // Under the fill budget a tail changes nothing.
        assert!(unbounded
            .capacity_errors_with_tail(10 * BS, Some(3))
            .is_empty());
    }

    /// The capacity refusals no longer say a unit is never split (item 288):
    /// every unit continues on the next cartridge.
    #[test]
    fn the_capacity_refusals_name_no_opt_in_to_spanning() {
        let cap = [LayoutError::CapacityExceeded {
            needed: 10 * BS,
            reserve: 0,
            available: 7 * BS,
        }];
        let note = capacity_refusal_note(&cap, 0.97, 10 * BS).unwrap();
        let bound = LayoutError::PlanBoundExceeded {
            needed: 10 * BS,
            bound: 9 * BS,
        }
        .to_string();
        for text in [&note, &bound] {
            assert!(!text.contains("spanning"), "{text}");
            assert!(!text.contains("never split"), "{text}");
        }
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
            msg.contains("stage clean --force")
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
    fn tenant_envelope_backup_has_a_distinct_type_label() {
        // volume-format-v2.md §1.3: a reader pairs a backup with its primary
        // by hash and must be able to tell the two apart by kind, so
        // RESTORE.sh opens the primary first and the backup only on failure.
        let kind = ZoneKind::TenantEnvelopeBackup { tenant_id: 3 };
        assert_eq!(kind.type_label(), "tenant_envelope_backup");
        assert_ne!(
            kind.type_label(),
            ZoneKind::TenantEnvelope { tenant_id: 3 }.type_label()
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
            ZoneKind::TenantEnvelopeBackup { tenant_id: 7 },
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
