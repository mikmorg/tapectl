//! The write session: the §9 typestate machine, exactly
//! (`docs/design/v2-open-questions.md` §9, quoted below; the state
//! table/transition rules it implements live in
//! `docs/design/layout-session.md`). `write.rs` shrinks to CLI orchestration
//! around this (T8); this module owns the state machine itself.
//!
//! ```text
//! Layout::build(conn, cfg, label, batch)  -> BuiltLayout
//!     generators run ONCE; every generated zone materialized to the session
//!     staging dir (frozen bytes, §2.2); envelope permutation applied (§2.1);
//!     front index emitted with all hashes; entry order = format order.
//! BuiltLayout::validate(keys, oracle)     -> ValidatedLayout | Vec<LayoutError>
//!     tri-layer L1: full-hash staged slices; size/hash-check frozen zones;
//!     capacity = Σ block-padded + enospc_buffer vs oracle; keys + escrow.
//! ValidatedLayout::plan(conn)             -> PlannedSession
//!     writes rows 'planned' + write_positions 'pending' (slices only — schema).
//! PlannedSession::execute(store)          -> Executing… -> ReadyToSeal
//!     rewind; per entry: SIGINT check (between entries only; mid-file kill =
//!     crash = startup sweep); stream from disk via a hashing tee reader;
//!     store.execute(src, len, sync); slice entries update their cursor row
//!     ('written' + sha256_on_volume). Inline-hash mismatch (tri-layer L2) or
//!     ENOSPC  =>  Abort: tape stays UNSEALED, writes 'aborted', staging kept.
//! ReadyToSeal::seal(store)                -> SealedPending
//!     regenerate the seal marker with the real sealed_at; write it (sync mark).
//! SealedPending::confirm(store, tier)     -> SessionEnd
//!     store.confirm (chain walk, §10); verification_sessions row (verify_type =
//!     full|quick); pass => ONE transaction: writes 'completed', snapshots
//!     'current', volumes 'sealed'. Three outcomes, not two, on a mismatch
//!     (ADR-0012's 2026-09-18 amendment, issues #260/#267):
//!     `Evidence::proves_medium_bad` true => volumes.observed_condition
//!     'quarantined' (status untouched, issue #242), writes 'aborted', staging
//!     kept; false => Inconclusive — nothing touched (no seal, no
//!     observed_condition write), writes 'interrupted' so `volume resume` can
//!     re-enter confirm (idempotent).
//! ```
//!
//! Each phase's operations exist only on its type, so an invalid order is
//! unrepresentable: e.g. there is no `BuiltLayout::seal`, no
//! `PlannedSession::confirm` — you can only call the next step for the type
//! you actually hold. In particular, no code path other than
//! [`ReadyToSeal::seal`] can ever produce a `SealMarker` write (sacred
//! invariant 1, `v2-implementation-plan.md`): sealing is unreachable unless
//! `execute` ran every non-seal entry to completion. `SealedPending` now has
//! a SECOND construction site (`InterruptedSession::resume_checking`'s
//! `AlreadySealed` arm, ADR-0012's 2026-09-21 amendment) besides
//! `ReadyToSeal::seal`'s return — this is still safe, because `SealedPending`
//! asserts only "a seal marker exists at the Layout's seal position" and its
//! one operation (`confirm`) only ever *reads* the store; the invariant this
//! module protects is that nothing but `ReadyToSeal::seal` ever *writes* one.
//!
//! `ExecuteOutcome`/`ResumeOutcome` stand in for the flow block's single
//! "SessionEnd" box: Rust has no single type for "one of several typed
//! successor states," so each fan-out point (execute can end Ready,
//! Interrupted, or Aborted; confirm can end Sealed, Inconclusive or
//! Quarantined; resume adds Quarantined and Confirming to execute's three) is
//! its own small enum. The state *names* match `layout-session.md`'s table
//! exactly; only the Rust-level packaging is invented here.
//!
//! Status: all four T6-required behaviors landed test-first (happy path;
//! hash-mismatch clean abort; ENOSPC clean abort; SIGINT interrupt +
//! resume), plus two bonus resume-path tests (restart-from-BOT; the File-0
//! identity check's quarantine branch) — see the module's test section.

use std::collections::HashMap;
use std::fs::File;
use std::io::Cursor;
use std::path::Path;

use rusqlite::{params, Connection};

use crate::error::{Result, TapectlError};
use crate::store::{Evidence, Store, Tier};
use crate::util::HashingReader;

use super::build::{BuildUnit, BuiltLayout};
use super::format;
use super::layout;
use super::layout_model::{ContentSource, KeyAvailability, LayoutEntry, LayoutError, ZoneKind};

// ── ValidatedLayout ──

/// A [`BuiltLayout`] that has passed `docs/design/layout-session.md`'s
/// validation predicate. Produced only by [`BuiltLayout::into_validated`];
/// its only operation is [`Self::plan`].
pub struct ValidatedLayout {
    built: BuiltLayout,
}

impl BuiltLayout {
    /// `BuiltLayout -> ValidatedLayout` (§9's `validate(keys, oracle)`).
    /// Runs the existing, already-tested `BuiltLayout::validate(keys)` (tri-layer
    /// L1 full-hash of staged slices, materialized-zone size/hash checks, key
    /// resolvability — `layout-session.md` validation points 2–3+5), then
    /// additionally cross-checks the Layout's on-tape total against a LIVE
    /// `store.capacity()` read.
    ///
    /// This live check is deliberately a NEW method rather than a change to
    /// `BuiltLayout::validate`'s signature: that method (T5b, already landed
    /// with ~10 tests) bakes capacity into the Layout's budget at `build()`
    /// time from `BuildInputs.usable_bytes` — the same config-derived number
    /// `store.capacity()` reports in normal use — so the live re-check is
    /// belt-and-suspenders, not a second independent source of truth. A
    /// store-capacity read failure does not itself fail validation (the
    /// baked-in-budget check above is the primary defense); only an actual
    /// shortfall does.
    pub fn into_validated(
        self,
        keys: &KeyAvailability,
        store: &mut dyn Store,
    ) -> std::result::Result<ValidatedLayout, Vec<LayoutError>> {
        let mut errs = Vec::new();
        if let Err(mut e) = self.validate(keys) {
            errs.append(&mut e);
        }
        if let Ok(needed) = self.layout.on_tape_bytes() {
            if let Ok(report) = store.capacity() {
                if needed + self.layout.budget.reserve_bytes > report.usable_bytes {
                    errs.push(LayoutError::CapacityExceeded {
                        needed,
                        reserve: self.layout.budget.reserve_bytes,
                        available: report.usable_bytes,
                    });
                }
            }
        }
        if errs.is_empty() {
            Ok(ValidatedLayout { built: self })
        } else {
            Err(errs)
        }
    }
}

// ── PlannedSession ──

/// `writes` rows 'planned' + `write_positions` 'pending' rows exist; nothing
/// has touched the store yet. Its only operation is [`Self::execute`].
pub struct PlannedSession {
    built: BuiltLayout,
    volume_id: i64,
    /// One (write_id, snapshot_id) pair per `BuildUnit` passed to `plan` —
    /// mirrors `write.rs`'s v1 `write_ids: Vec<(i64, i64)>` shape.
    write_ids: Vec<(i64, i64)>,
    /// `stage_slice_id -> write_id`, so `execute` knows which `writes` row's
    /// `write_positions` cursor row to update for each slice entry.
    slice_write_id: HashMap<i64, i64>,
}

impl ValidatedLayout {
    /// `ValidatedLayout -> PlannedSession`. Inserts one `writes` row per unit
    /// in `units` (status 'planned'), all sharing `volume_id`, and one
    /// `write_positions` row per slice entry (status 'pending' — metadata
    /// files never get a cursor row, `write_positions.stage_slice_id` is
    /// NOT NULL by schema). Never called on resume: resume reuses the
    /// existing rows (`writes` has `UNIQUE(stage_set_id, volume_id)`).
    pub fn plan(
        self,
        conn: &Connection,
        volume_id: i64,
        units: &[BuildUnit],
    ) -> Result<PlannedSession> {
        let mut write_ids = Vec::with_capacity(units.len());
        let mut slice_write_id = HashMap::new();
        // The frozen staging directory is recorded here, and only here
        // (migration 006, issue #25): plan is the sole `writes`-row writer,
        // and after a process restart this path is the ONLY way back to the
        // materialized zones a resume must re-hash rather than regenerate.
        let session_dir = self.built.session_dir.to_string_lossy().to_string();
        for u in units {
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, session_dir)
                 VALUES (?1, ?2, ?3, 'planned', ?4)",
                params![u.stage_set_id, u.snapshot_id, volume_id, session_dir],
            )?;
            let write_id = conn.last_insert_rowid();
            write_ids.push((write_id, u.snapshot_id));
            for slice in &u.slices {
                slice_write_id.insert(slice.slice_id, write_id);
            }
        }

        for entry in &self.built.layout.entries {
            if let ZoneKind::Slice { stage_slice_id } = entry.kind {
                let write_id = *slice_write_id.get(&stage_slice_id).ok_or_else(|| {
                    TapectlError::Other(format!(
                        "plan: no unit in `units` owns staged slice {stage_slice_id} \
                         (Layout position {})",
                        entry.position
                    ))
                })?;
                conn.execute(
                    "INSERT INTO write_positions (write_id, stage_slice_id, position, status)
                     VALUES (?1, ?2, ?3, 'pending')",
                    params![write_id, stage_slice_id, entry.position.to_string()],
                )?;
            }
        }

        Ok(PlannedSession {
            built: self.built,
            volume_id,
            write_ids,
            slice_write_id,
        })
    }
}

// ── Executing / ReadyToSeal / Interrupted / Aborted ──

/// What `execute` (fresh) ended with.
pub enum ExecuteOutcome {
    Ready(ReadyToSeal),
    Interrupted(InterruptedSession),
    Aborted(AbortedSession),
}

/// What `resume` ended with — everything `ExecuteOutcome` can, plus
/// `Quarantined` (the File-0 identity check found a divergent tape) and
/// `Confirming` (ADR-0012's 2026-09-21 amendment, issues #260/#267):
/// `check_tape_contact` found the tape already sealed exactly where THIS
/// session left it — File 0's identity matches, the recorded seal position
/// agrees with this session's own Layout, and that position is the tape's
/// own File-0 pointer, none of the three inferred (see
/// `resume_reconfirm_eligible`). `seal()` must NEVER run on this path
/// (sacred invariant 1) — the tape is already sealed; resume re-enters
/// `confirm` directly on the `SealedPending` it already is.
pub enum ResumeOutcome {
    Ready(ReadyToSeal),
    Interrupted(InterruptedSession),
    Aborted(AbortedSession),
    Quarantined(QuarantinedSession),
    Confirming(SealedPending),
}

impl From<ExecuteOutcome> for ResumeOutcome {
    fn from(o: ExecuteOutcome) -> Self {
        match o {
            ExecuteOutcome::Ready(r) => ResumeOutcome::Ready(r),
            ExecuteOutcome::Interrupted(i) => ResumeOutcome::Interrupted(i),
            ExecuteOutcome::Aborted(a) => ResumeOutcome::Aborted(a),
        }
    }
}

/// Every non-seal entry executed successfully. Its only operation is
/// [`Self::seal`] — no code path outside `seal` can ever write a
/// `SealMarker` entry (sacred invariant 1).
pub struct ReadyToSeal {
    built: BuiltLayout,
    volume_id: i64,
    write_ids: Vec<(i64, i64)>,
}

/// SIGINT (or a startup-sweep-detected crash) stopped execution between
/// entries. Resumable: its only operation is [`Self::resume`].
pub struct InterruptedSession {
    built: BuiltLayout,
    volume_id: i64,
    write_ids: Vec<(i64, i64)>,
    slice_write_id: HashMap<i64, i64>,
}

/// Terminal, not resumable: the tape is not a copy
/// (`docs/design/layout-session.md`'s Aborted row). No further operations —
/// the operator reloads a fresh cartridge and re-plans from scratch.
pub struct AbortedSession {
    pub volume_id: i64,
    pub reason: String,
}

/// Why a session ended quarantined: either the sealed tape failed the
/// confirm chain walk (structured [`Evidence`]), or resume's File-0 identity
/// check found this isn't the same tape
/// (`docs/design/layout-session.md`: "mismatch = divergence = quarantine,
/// not overwrite"). These are different kinds of evidence — a chain walk
/// report vs. a label/uuid disagreement vs. an already-sealed tape — so they
/// are not forced into the same shape.
#[derive(Debug)]
pub enum QuarantineReason {
    ConfirmFailed(Evidence),
    IdentityMismatch {
        expected_label: String,
        expected_uuid: String,
        /// `None` if File 0 was present but did not even parse as a valid
        /// ID thunk (still a mismatch — never overwrite on ambiguity).
        found: Option<format::IdThunkIdentity>,
    },
    /// Resume found a parseable seal marker at the tape's last position, and
    /// [`resume_reconfirm_eligible`] found at least one of its three
    /// conditions did not hold (ADR-0012's 2026-09-21 amendment, issues
    /// #260/#267): either this isn't genuinely the same tape/position this
    /// session left behind, or that could not be verified against the
    /// tape's own File-0 pointer. Sealed volumes are immutable (ADR-0003 —
    /// there is no append), so resuming would risk rewriting a finished
    /// tape, and the session refuses. The catalog and the tape disagree
    /// about this volume's state, which is a divergence (ADR-0001) — hence
    /// quarantine rather than a plain abort. (When all three conditions DO
    /// hold, resume does not reach this variant at all — it re-enters
    /// `confirm` instead via `ResumeOutcome::Confirming`.)
    AlreadySealed {
        seal_position: u32,
    },
}

/// Terminal: the catalog quarantines the volume, but the tape itself is
/// physically immutable — the evidence of quarantine is recomputable by
/// anyone from the tape (`v2-open-questions.md` §2.6). No further
/// operations.
pub struct QuarantinedSession {
    pub volume_id: i64,
    pub label: String,
    pub reason: QuarantineReason,
}

/// Terminal success: confirm passed, `writes`/`snapshots`/`volumes` flipped
/// in one transaction. No further operations.
pub struct SealedSession {
    pub volume_id: i64,
    pub label: String,
}

/// Confirm's readback did not succeed, but nothing it saw proves the medium
/// itself is bad (`Evidence::proves_medium_bad` is false — drive/transport
/// evidence only, e.g. `MismatchKind::ContentUnreadable`). ADR-0012's
/// 2026-09-18 amendment (issues #260/#267): sealing here would assert a
/// durability claim confirm never verified (ADR-0001), and quarantining
/// would condemn a tape that may well be sound. Neither `volumes.status` nor
/// `observed_condition` is touched — nothing was learned about the medium —
/// and `writes` rows are moved to `interrupted` (not `aborted`) so
/// `tapectl volume resume` can pick the session back up and re-enter
/// `confirm` (confirm is idempotent and the tape is physically unchanged by
/// a failed read). Terminal for THIS confirm attempt only, not for the
/// session.
pub struct InconclusiveSession {
    pub volume_id: i64,
    pub label: String,
    pub evidence: Evidence,
}

/// What `confirm` ended with. Three outcomes, not two (ADR-0012's 2026-09-18
/// amendment, issues #260/#267) — `Evidence::proves_medium_bad` decides
/// which of the last two applies: true => `Quarantined`, false =>
/// `Inconclusive`. Matched exhaustively with no wildcard arm on purpose,
/// same discipline as `MismatchKind::proves_medium_bad`'s own match: an
/// eighth outcome is a decision this ADR makes, never a default.
pub enum ConfirmOutcome {
    Sealed(SealedSession),
    Inconclusive(InconclusiveSession),
    Quarantined(QuarantinedSession),
}

// ── shared contact check (File 0 + seal marker) — issue #27 ──

/// What [`check_tape_contact`] found. Carries no side effects (no
/// `Connection`, no DB writes) — every caller decides for itself what an
/// outcome means: `InterruptedSession::resume_checking` maps it onto its
/// existing quarantine bookkeeping (below); the fresh-write path
/// (`write::check_fresh_write_contact`) maps it onto a plain refusal, since
/// a wrong cartridge loaded for a fresh write means the operator grabbed the
/// wrong tape — it says nothing about the not-yet-written logical volume
/// itself having diverged from anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContactOutcome {
    /// File 0 unreadable: nothing recorded there. For resume this is a
    /// session crashed before File 0 ever landed; for a fresh write it is
    /// the ordinary shape of a blank cartridge. Either way: safe to
    /// (re)write from BOT.
    Blank,
    /// File 0 parsed and its identity matches `expected_label`/`expected_uuid`,
    /// and (when a seal position was given) nothing parseable was found
    /// there either — safe to continue.
    Matches,
    /// File 0 parsed but disagrees with the expected identity, or was
    /// readable yet failed to parse at all (ambiguous is treated as a
    /// mismatch — never overwrite on ambiguity).
    IdentityMismatch {
        found: Option<format::IdThunkIdentity>,
    },
    /// The seal-marker position parsed as a seal marker: this tape already
    /// holds a SEALED volume (ADR-0003 — sealed volumes are immutable,
    /// there is no append).
    AlreadySealed { seal_position: u32 },
}

/// The one check both [`InterruptedSession::resume_checking`] and the
/// fresh-write path (`write::volume_init`/`write::volume_write`, issue #27)
/// run before ever executing an entry — `docs/design/layout-session.md`'s
/// Resume rule: "rewind, read file 0, require ID-thunk identity match (label
/// and uuid) — mismatch = divergence = quarantine, not overwrite," plus the
/// seal-marker absence check that follows it ("The absent seal marker
/// confirms the tape is legitimately unsealed"). Factored out so the two
/// call sites cannot drift apart — this is the algorithm, not a description
/// of it.
///
/// `seal_position` is the tape position to probe for a seal marker, if the
/// caller has one to check: `resume_checking` and `volume_write` always
/// pass `Some` (they hold a real [`BuiltLayout`] with a known seal-marker
/// entry); `volume_init` has no Layout yet (no staged units — only a label
/// and a freshly generated candidate uuid) and passes `None`. This loses
/// nothing in practice: the only way this function would ever reach a
/// *matching*-identity seal check is if the tape's File 0 already carried
/// the caller's own `expected_uuid` — impossible for `volume_init`, whose
/// uuid is always freshly generated. Any tape `volume_init` is about to
/// stamp that already has a parseable File 0 therefore already refuses via
/// `IdentityMismatch`, sealed or not.
///
/// The two probes are independent: the seal-marker check runs whether or
/// not File 0 was readable (a File-0-unreadable-but-sealed-tail tape is
/// exactly the front/tail damage asymmetry `volume-format-v2.md` §4 designs
/// for), but not if File 0 was readable and already mismatched (no need —
/// that already refuses).
pub fn check_tape_contact(
    store: &mut dyn Store,
    expected_label: &str,
    expected_uuid: &str,
    seal_position: Option<u32>,
) -> ContactOutcome {
    let mut id_thunk_bytes = Vec::new();
    let file_zero_present = store.read_file(0, &mut id_thunk_bytes).is_ok();
    if file_zero_present {
        let text = String::from_utf8_lossy(&id_thunk_bytes);
        let identity = format::parse_id_thunk_identity(&text);
        let matches = matches!(
            &identity,
            Ok(id) if id.label == expected_label && id.uuid == expected_uuid
        );

        // THE TAPE'S OWN seal pointer, consulted whether or not the identity
        // matched (issue #208, 2026-09-17 pre-production review).
        //
        // This used to sit inside the `!matches` arm below, which left the
        // matching-identity case relying entirely on the CALLER's
        // `seal_position` argument. For `resume_checking` that is sound --
        // its layout is rehydrated from the very session that wrote this
        // tape, so its seal entry is where the seal really is. For
        // `volume_write` it is not: its layout is freshly built from
        // whatever is staged NOW, so its seal position matches the tape's
        // only when the new content happens to lay out identically. Write
        // different content to a tape the catalog still believes is
        // `initialized` -- a DB restored from a backup predating the seal,
        // or a row a rebuild left alone -- and the probe reads a position
        // with no marker, returns `Matches`, and a SEALED tape is
        // overwritten. ADR-0003 forbids that outright and `--force` cannot
        // reach it, so the guard must not depend on the caller guessing the
        // right position.
        //
        // The tape's self-reported `[layout].seal_marker` has no such
        // problem: it is where THIS tape says its own seal is. Probing it
        // first is strictly more conservative -- it can only add refusals,
        // and only for tapes that genuinely carry a parsing seal marker.
        // Re-initialising a cartridge is unaffected: that path erases the
        // medium first, so File 0 is gone or unparseable long before here.
        if let Ok(pointers) = format::parse_id_thunk_layout_pointers(&text) {
            if pointers.seal_marker >= 0
                && seal_marker_parses_at(store, pointers.seal_marker as u32)
            {
                return ContactOutcome::AlreadySealed {
                    seal_position: pointers.seal_marker as u32,
                };
            }
        }

        if !matches {
            // The sealed case already returned above (issue #208), so a
            // mismatch reaching here is a genuinely unsealed foreign or
            // stale tape. That is what `--force` is allowed to overwrite;
            // issue #27's headline scenario -- a foreign-but-SEALED
            // cartridge presenting as a plain mismatch the flag could
            // defeat -- is closed by the hoisted probe, not here.
            return ContactOutcome::IdentityMismatch {
                found: identity.ok(),
            };
        }
    }

    if let Some(seal_pos) = seal_position {
        if seal_marker_parses_at(store, seal_pos) {
            return ContactOutcome::AlreadySealed {
                seal_position: seal_pos,
            };
        }
    }

    if file_zero_present {
        ContactOutcome::Matches
    } else {
        ContactOutcome::Blank
    }
}

/// Read `position` and report whether it parses as a seal marker — a read
/// failure (nothing recorded there) is the expected, safe "not sealed"
/// case, never an error. Shared by [`check_tape_contact`]'s two seal probes
/// (the caller-supplied position, and a foreign tape's own self-reported
/// one) so there is exactly one "does this position hold a seal marker"
/// check, not two copies that could drift.
///
/// That conflation is safe ONLY for a fresh write to a blank tape — it is
/// exactly what made an unreadable-but-genuinely-sealed position on resume
/// indistinguishable from "never sealed" (ADR-0012's 2026-09-21 correction
/// "the seal is RECORDED, not inferred", issue #277). `resume_checking`
/// no longer relies on this function's answer alone to decide whether IT
/// owes a seal; it consults `volumes.sealed_at` (migration 018) first. This
/// function itself is unchanged — the conflation remains correct for the
/// fresh-write path (`write::check_fresh_write_contact`) and for
/// [`resume_reconfirm_eligible`]'s defence-in-depth conditions.
fn seal_marker_parses_at(store: &mut dyn Store, position: u32) -> bool {
    let mut bytes = Vec::new();
    if store.read_file(position, &mut bytes).is_ok() {
        let text = String::from_utf8_lossy(&bytes);
        format::parse_seal_marker(&text).is_ok()
    } else {
        false
    }
}

/// Whether a resume that met [`ContactOutcome::AlreadySealed`] may skip the
/// write phase and re-enter `confirm` directly, instead of quarantining —
/// ADR-0012's 2026-09-21 amendment, "`volume resume` re-confirms a tape that
/// is already sealed" (issues #260/#267).
///
/// All three of the ruling's conditions are re-derived HERE, from scratch,
/// independently of whichever internal branch of [`check_tape_contact`]
/// produced the `AlreadySealed` outcome — that function reports the exact
/// same shape for a genuinely FOREIGN sealed tape (its seal probe runs
/// "whether or not the identity matched", issue #208) and, even when the
/// identity DOES match, can report a position it verified only via the
/// CALLER's own guess rather than the tape's self-reported pointer (sound
/// for THAT function's own contract — `resume_checking`'s layout is
/// rehydrated from the very session that wrote this tape — but not a fact
/// this decision may assume without checking independently):
///
/// 1. File 0's identity (label + uuid) matches `expected_label`/
///    `expected_uuid` — checked here explicitly, never inferred from having
///    reached this arm rather than `IdentityMismatch`.
/// 2. File 0's OWN recorded `[layout] seal_marker` pointer equals
///    `expected_seal_position` (this session's own Layout).
/// 3. That exact position parses as a real seal marker.
///
/// Condition 3 is meaningless without condition 2 reading the pointer from
/// File 0 itself rather than trusting a value `check_tape_contact` already
/// decided — that trust is exactly what would let the caller-guess fallback
/// (safe only inside `check_tape_contact`'s own broader contract) leak into
/// a decision that must never rest on a guess. This is why this function
/// never calls `check_tape_contact` and never accepts its returned
/// `seal_position` as an argument: it re-reads File 0 and re-parses its
/// `[layout]` table itself.
///
/// Any failure — File 0 unreadable, unparseable, a non-matching identity, no
/// recorded pointer, a pointer that disagrees with this session's Layout, or
/// a position that does not actually parse as a seal marker — returns
/// `false`, and the caller keeps today's behaviour: quarantine, never
/// proceed.
fn resume_reconfirm_eligible(
    store: &mut dyn Store,
    expected_label: &str,
    expected_uuid: &str,
    expected_seal_position: Option<u32>,
) -> bool {
    let Some(expected_seal_position) = expected_seal_position else {
        return false;
    };
    let mut id_thunk_bytes = Vec::new();
    if store.read_file(0, &mut id_thunk_bytes).is_err() {
        return false;
    }
    let text = String::from_utf8_lossy(&id_thunk_bytes);
    let identity = match format::parse_id_thunk_identity(&text) {
        Ok(id) => id,
        Err(_) => return false,
    };
    if identity.label != expected_label || identity.uuid != expected_uuid {
        return false;
    }
    let pointers = match format::parse_id_thunk_layout_pointers(&text) {
        Ok(p) => p,
        Err(_) => return false,
    };
    if pointers.seal_marker < 0 || pointers.seal_marker as u32 != expected_seal_position {
        return false;
    }
    seal_marker_parses_at(store, expected_seal_position)
}

/// Whether THIS volume's own `seal()` already ran — `volumes.sealed_at`
/// (migration 018), ADR-0012's 2026-09-21 correction "the seal is RECORDED,
/// not inferred" (issue #277).
///
/// `sealed_at` is set exactly once, at the single production `seal()` call
/// site (`write::finish_session`, immediately after `ready.seal(store)`
/// returns `Ok`), and is never cleared afterward by any confirm outcome —
/// not even an `Inconclusive` confirm's `mark_writes(..., "interrupted")`
/// (`SealedPending::confirm`). That is what makes its mere presence settle
/// what `seal_marker_parses_at` cannot: whether an unreadable seal position
/// means "never sealed" (this volume's `sealed_at` is still NULL — the seal
/// is genuinely still owed) or "sealed, but this read attempt failed" (this
/// volume's `sealed_at` is set — the seal must never be attempted again).
/// [`InterruptedSession::resume_checking`] consults this before it will
/// ever reposition the store or call `seal()` again.
fn seal_recorded(conn: &Connection, volume_id: i64) -> Result<bool> {
    let sealed_at: Option<String> = conn.query_row(
        "SELECT sealed_at FROM volumes WHERE id = ?1",
        params![volume_id],
        |r| r.get(0),
    )?;
    Ok(sealed_at.is_some())
}

impl PlannedSession {
    /// `PlannedSession -> ExecuteOutcome`, checking for interruption via the
    /// real process-global signal flag. See [`Self::execute_checking`] for
    /// the injectable form tests use.
    pub fn execute(self, conn: &Connection, store: &mut dyn Store) -> Result<ExecuteOutcome> {
        self.execute_checking(conn, store, crate::signal::is_interrupted)
    }

    /// `PlannedSession -> ExecuteOutcome` with an injectable interruption
    /// predicate. `is_interrupted` is checked BETWEEN entries only — a
    /// mid-file kill is a crash, handled by the startup sweep
    /// (`crate::db::recover_orphaned_sessions`), not by this loop. Split out
    /// from [`Self::execute`] so tests can control exactly when "SIGINT
    /// fires" without touching the real process-global flag
    /// (`crate::signal::is_interrupted`), which is process-wide state that
    /// Rust's multithreaded test runner would otherwise leak across tests.
    pub fn execute_checking(
        self,
        conn: &Connection,
        store: &mut dyn Store,
        mut is_interrupted: impl FnMut() -> bool,
    ) -> Result<ExecuteOutcome> {
        for (write_id, _) in &self.write_ids {
            conn.execute(
                "UPDATE writes SET status = 'in_progress',
                 started_at = COALESCE(started_at, datetime('now'))
                 WHERE id = ?1",
                params![write_id],
            )?;
        }
        run_entries(
            conn,
            store,
            self.built,
            self.volume_id,
            self.write_ids,
            self.slice_write_id,
            0,
            &mut is_interrupted,
            park_marker_from_env(),
        )
    }
}

impl InterruptedSession {
    /// The Layout this session will resume against. Exposed because the CLI
    /// orchestrator (`write::volume_resume`) needs it BEFORE `resume`
    /// consumes `self`: the tenant-envelope entries are where a restarted
    /// process learns which tenants' keys to require (there is no staged
    /// batch to derive them from), and the post-confirm bookkeeping needs the
    /// slice entries.
    pub fn layout(&self) -> &super::layout_model::Layout {
        &self.built.layout
    }

    /// Reconstruct an interrupted session for `volume_id` from durable state
    /// alone — the `writes`/`write_positions` rows and the frozen session
    /// staging directory they point at — so `tapectl volume resume` can pick
    /// up a session whose process is gone (issue #25, playbook T8 remainder).
    /// Returns `Ok(None)` when there is nothing to resume.
    ///
    /// **The Layout is REHYDRATED, never regenerated.** `build()` is not
    /// reproducible across a process restart, for two independent reasons:
    /// `BuildInputs::created_at` is `chrono::Utc::now()` at `volume_write`
    /// call time and is persisted nowhere, and `BuildInputs::mam_loads` is
    /// `tape::mam::read_mam`'s `load_count`, which increments on every
    /// cartridge load. Either one drifting changes the ID-thunk bytes, hence
    /// File 0's sha256, hence the front index that records it. Since
    /// [`SealedPending::confirm`] diffs the front index read back off the
    /// medium against the Layout it holds, a regenerated Layout would
    /// quarantine a perfectly good tape — silent process corruption, not a
    /// loud failure. `docs/design/layout-session.md`'s Resume rule says the
    /// frozen generated zones "re-hash byte-identical" (re-hash the frozen
    /// files, not re-generate them), and `ContentSource`'s doc comment
    /// records the same conclusion. So this reads `layout.json`
    /// (`build::LAYOUT_SIDECAR`) and points at the ORIGINAL `session_dir`.
    ///
    /// **Only `interrupted` rows are accepted, never `in_progress`.**
    /// `db::open` calls `recover_orphaned_sessions` unconditionally, which
    /// sweeps every `in_progress` row to `interrupted` before any command
    /// holds a `Connection`. A row still `in_progress` by the time this runs
    /// therefore means a LIVE writer in another process — resuming it would
    /// put two processes on one tape. It is skipped rather than adopted.
    ///
    /// This method only READS. `ValidatedLayout::plan` remains the sole
    /// writer of `writes` rows (`UNIQUE(stage_set_id, volume_id)` would
    /// reject an insert here anyway, and `layout-session.md` is explicit that
    /// resume reuses the existing rows).
    pub fn rehydrate(conn: &Connection, volume_id: i64) -> Result<Option<InterruptedSession>> {
        // `plan` inserts one row per unit in order, so ordering by id
        // reproduces its `write_ids` sequence exactly.
        let rows: Vec<(i64, i64, Option<String>)> = conn
            .prepare(
                "SELECT id, snapshot_id, session_dir FROM writes
                 WHERE volume_id = ?1 AND status = 'interrupted'
                 ORDER BY id",
            )?
            .query_map(params![volume_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<_>>()?;

        if rows.is_empty() {
            return Ok(None);
        }

        let write_ids: Vec<(i64, i64)> = rows.iter().map(|(id, snap, _)| (*id, *snap)).collect();

        // Every row of one session shares one session directory (plan writes
        // the same value to all of them). Disagreement means these rows are
        // not one session, which is not something to guess through.
        let mut dirs: Vec<&str> = rows
            .iter()
            .map(|(id, _, dir)| {
                dir.as_deref().ok_or_else(|| {
                    TapectlError::Other(format!(
                        "volume {volume_id}: interrupted write {id} has no recorded session \
                         directory (it predates migration 006). The frozen staging files are \
                         the rewrite source; without them this session cannot be resumed, only \
                         aborted — mark its `writes` rows aborted and start a fresh write to a \
                         blank cartridge."
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        dirs.sort_unstable();
        dirs.dedup();
        if dirs.len() > 1 {
            return Err(TapectlError::Other(format!(
                "volume {volume_id}: its interrupted `writes` rows name {} different session \
                 directories ({dirs:?}) — these are not one write session, and resuming would \
                 mix frozen files from different builds. Resolve by hand before retrying.",
                dirs.len()
            )));
        }
        let session_dir = Path::new(dirs[0]).to_path_buf();

        let sidecar = session_dir.join(super::build::LAYOUT_SIDECAR);
        let json = std::fs::read(&sidecar).map_err(|e| {
            TapectlError::Other(format!(
                "volume {volume_id}: cannot read the frozen layout at {}: {e}. The frozen \
                 staging files are the rewrite source; without them this session cannot be \
                 resumed, only aborted (the Layout cannot be rebuilt — its ID thunk embeds a \
                 `created_at` and a MAM load count that no longer exist).",
                sidecar.display()
            ))
        })?;
        let layout: super::layout_model::Layout = serde_json::from_slice(&json).map_err(|e| {
            TapectlError::Other(format!(
                "volume {volume_id}: the frozen layout at {} is unparseable: {e}",
                sidecar.display()
            ))
        })?;

        // `slice_write_id`, exactly as `plan` built it: it inserted one
        // `write_positions` row per slice entry, keyed by the owning unit's
        // write_id, so reading those rows back reproduces the map without
        // needing the `BuildUnit`s (which no longer exist).
        let mut slice_write_id = HashMap::new();
        for (write_id, _) in &write_ids {
            let mut stmt =
                conn.prepare("SELECT stage_slice_id FROM write_positions WHERE write_id = ?1")?;
            let ids: Vec<i64> = stmt
                .query_map(params![write_id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for slice_id in ids {
                slice_write_id.insert(slice_id, *write_id);
            }
        }

        Ok(Some(InterruptedSession {
            built: BuiltLayout {
                layout,
                session_dir,
            },
            volume_id,
            write_ids,
            slice_write_id,
        }))
    }

    /// `InterruptedSession -> ResumeOutcome`, checking for interruption via
    /// the real process-global signal flag. See [`Self::resume_checking`]
    /// for the injectable form tests use.
    pub fn resume(
        self,
        conn: &Connection,
        keys: &KeyAvailability,
        store: &mut dyn Store,
    ) -> Result<ResumeOutcome> {
        self.resume_checking(conn, keys, store, crate::signal::is_interrupted)
    }

    /// Resume the same session against the same tape —
    /// `docs/design/layout-session.md`'s Resume rule, verbatim: revalidate
    /// the Layout (staged slices unchanged; frozen generated zones re-hash
    /// byte-identical), rewind, read file 0, require ID-thunk identity match
    /// (label + uuid) — mismatch = divergence = quarantine, not overwrite —
    /// then the two-case cursor rule
    /// (`write_positions.stage_slice_id` is NOT NULL, so only slices have
    /// cursor rows): if zero slices are recorded `written`, restart from
    /// BOT (the front zone regenerates byte-identical from the frozen
    /// staging files); if ≥1 slice is written, reposition to
    /// `front_zone_len + written_slices` (both terms exact) and continue.
    /// The absent seal marker confirms the tape is legitimately unsealed.
    /// A PRESENT seal marker does not automatically mean divergence any
    /// more (ADR-0012's 2026-09-21 amendment, issues #260/#267): if
    /// [`resume_reconfirm_eligible`]'s three conditions all hold — File 0's
    /// identity matches, its own recorded seal pointer agrees with this
    /// session's Layout, and that pointer genuinely parses as a seal marker
    /// — this is exactly what THIS session's own `seal()` left behind, and
    /// resume skips straight to re-entering `confirm` instead of
    /// quarantining. Any of the three failing keeps the original rule:
    /// mismatch = divergence = quarantine, not overwrite.
    ///
    /// Caller note: this requires only that `self` carry a valid
    /// `BuiltLayout`, however it was sourced — either the same in-memory
    /// session value `execute` returned (a caught SIGINT within one process,
    /// or a test), or one rebuilt from durable state by
    /// [`Self::rehydrate`] after an actual process restart. Rehydration
    /// re-reads the ORIGINAL `session_dir`'s frozen files and never calls
    /// `build()` again, which is not reproducible
    /// (`ContentSource::Materialized`'s doc comment); `write::volume_resume`
    /// is the CLI orchestrator around it.
    pub fn resume_checking(
        self,
        conn: &Connection,
        keys: &KeyAvailability,
        store: &mut dyn Store,
        mut is_interrupted: impl FnMut() -> bool,
    ) -> Result<ResumeOutcome> {
        // 1. Revalidate: staged slices unchanged, frozen zones re-hash
        // byte-identical (re-runs the same tri-layer-L1 + materialized-zone
        // checks `into_validated` ran originally). Failure here NEVER
        // auto-aborts (issue #94): it reports and leaves the session
        // `interrupted`, so a later `resume` can still adopt it.
        //
        // `docs/design/layout-session.md`'s Aborted row says a resume
        // revalidation failure aborts only when it fails *unrecoverably* —
        // and nothing here can judge that. `LayoutError::SliceFileMissing`
        // and `MaterializedZoneMissing` cannot distinguish "staging
        // filesystem is temporarily unmounted" (transient) from "the file was
        // deleted" (terminal); the fact that decides it is not carried by the
        // variant, so no classification table can be written. The asymmetry
        // settles the default: treating a terminal failure as retryable costs
        // the operator one extra command, while treating a transient failure
        // as terminal permanently abandons a good tape and forces a full
        // re-stage and re-write. So the operator — who knows whether the disk
        // is unmounted or wiped — judges "unrecoverably", via
        // `tapectl volume abort`.
        if let Err(errs) = self.built.validate(keys) {
            let detail = errs
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(TapectlError::Other(format!(
                "volume resume: revalidation failed, so nothing was written and the session was \
                 left untouched in the `interrupted` state — the cartridge is unharmed. Causes: \
                 {detail}. Fix the cause (a staging filesystem that is not mounted, or a key \
                 that is temporarily absent, are both transient and are indistinguishable here \
                 from permanent loss) and run `tapectl volume resume` again; or, if the inputs \
                 are genuinely gone, abandon the session deliberately with \
                 `tapectl volume abort`."
            )));
        }

        // 2+2b. File-0 identity, then (independently) the seal-marker
        // absence check — `docs/design/layout-session.md`'s Resume rule: "The
        // absent seal marker confirms the tape is legitimately unsealed
        // (safe to resume, not an append to a sealed volume)" — so a seal
        // marker that IS present means this tape is finished and resuming
        // would append to a sealed volume, which ADR-0003 forbids outright.
        // This is not hypothetical: `recover_orphaned_sessions` sweeps a
        // crashed `in_progress` session to `interrupted` (resumable), and a
        // crash during confirm — i.e. AFTER seal() wrote the marker — lands
        // exactly here. The File-0 identity check alone cannot catch it: the
        // identity genuinely matches, because it genuinely is the same tape.
        // Both checks are [`check_tape_contact`] (#27) — shared verbatim with
        // the fresh-write path so the two cannot drift apart.
        let seal_position = self
            .built
            .layout
            .entries
            .iter()
            .find(|e| matches!(e.kind, ZoneKind::SealMarker))
            .map(|e| e.position as u32);
        match check_tape_contact(
            store,
            &self.built.layout.label,
            &self.built.layout.volume_uuid,
            seal_position,
        ) {
            ContactOutcome::Blank | ContactOutcome::Matches => {
                // ADR-0012's 2026-09-21 correction "the seal is RECORDED,
                // not inferred" (issue #277). Both `Blank` and `Matches`
                // are reached whenever the seal position at the end of
                // this session's own Layout does not read as a parseable
                // seal marker — and that happens both when it genuinely
                // never got one (a real resume of a still-unsealed tape)
                // AND when THIS session's own `seal()` already wrote one
                // but this read attempt failed (`MismatchKind::SealUnreadable`
                // is exactly what produces an `Inconclusive` confirm, so a
                // resume after that always meets this shape). Those two
                // cases must never be told apart by guessing from the tape:
                // `volumes.sealed_at` (migration 018) is the recorded fact.
                // Sealed already → re-enter confirm; never reposition or
                // seal again — the same terminal shape as the
                // `AlreadySealed` arm's own `Confirming` outcome below, just
                // reached without a readable seal marker to probe.
                if seal_recorded(conn, self.volume_id)? {
                    return Ok(ResumeOutcome::Confirming(SealedPending {
                        built: self.built,
                        volume_id: self.volume_id,
                        write_ids: self.write_ids,
                    }));
                }
                // sealed_at is NULL: the seal is still genuinely owed.
                // Fall through to the two-case cursor rule exactly as
                // before.
            }
            ContactOutcome::IdentityMismatch { found } => {
                // ADR-0012's 2026-09-17 amendment (issue #242): this is a
                // catalog fact tapectl OBSERVED, never one the operator
                // chose -- so it moves `observed_condition`, not `status`.
                // The operator's own status (e.g. a terminal `retired`) is
                // never overwritten by this write.
                conn.execute(
                    "UPDATE volumes SET observed_condition = 'quarantined' WHERE id = ?1",
                    params![self.volume_id],
                )?;
                mark_writes(conn, &self.write_ids, "aborted")?;
                return Ok(ResumeOutcome::Quarantined(QuarantinedSession {
                    volume_id: self.volume_id,
                    label: self.built.layout.label.clone(),
                    reason: QuarantineReason::IdentityMismatch {
                        expected_label: self.built.layout.label.clone(),
                        expected_uuid: self.built.layout.volume_uuid.clone(),
                        found,
                    },
                }));
            }
            ContactOutcome::AlreadySealed {
                seal_position: found_seal_position,
            } => {
                // ADR-0012's 2026-09-21 amendment (issues #260/#267): an
                // already-sealed tape at resume is not automatically
                // divergence — it is exactly what THIS session's own
                // `seal()` call left behind if confirm was interrupted or
                // ended `Inconclusive`. Re-enter confirm instead of
                // quarantining, but ONLY if all three of
                // `resume_reconfirm_eligible`'s conditions hold, none
                // inferred from merely reaching this arm:
                // `check_tape_contact` reports this same shape for a
                // genuinely FOREIGN sealed tape too (its own seal probe runs
                // "whether or not the identity matched", issue #208).
                if resume_reconfirm_eligible(
                    store,
                    &self.built.layout.label,
                    &self.built.layout.volume_uuid,
                    seal_position,
                ) {
                    return Ok(ResumeOutcome::Confirming(SealedPending {
                        built: self.built,
                        volume_id: self.volume_id,
                        write_ids: self.write_ids,
                    }));
                }

                // Any of the three conditions failing keeps today's
                // behaviour exactly. Same reasoning as the
                // `IdentityMismatch` arm just above (issue #242): an
                // observed fact, not an operator choice.
                conn.execute(
                    "UPDATE volumes SET observed_condition = 'quarantined' WHERE id = ?1",
                    params![self.volume_id],
                )?;
                mark_writes(conn, &self.write_ids, "aborted")?;
                return Ok(ResumeOutcome::Quarantined(QuarantinedSession {
                    volume_id: self.volume_id,
                    label: self.built.layout.label.clone(),
                    reason: QuarantineReason::AlreadySealed {
                        seal_position: found_seal_position,
                    },
                }));
            }
        }

        // 3. Two-case cursor rule (`write_positions.stage_slice_id` is NOT
        // NULL, so only slice entries ever have a cursor row): zero slices
        // written => restart from BOT (index 0); >=1 written => reposition
        // to front_zone_len + written_slices (both terms exact — the first
        // slice's Layout position, and the DB's own count of 'written' rows).
        let written_slices = count_written_slices(conn, &self.write_ids)?;
        let content_entries: Vec<&LayoutEntry> = self
            .built
            .layout
            .entries
            .iter()
            .filter(|e| !matches!(e.kind, ZoneKind::SealMarker))
            .collect();
        let first_slice_index = content_entries
            .iter()
            .position(|e| matches!(e.kind, ZoneKind::Slice { .. }))
            .ok_or_else(|| {
                TapectlError::Other("resume: layout has no slice entries".to_string())
            })?;
        let start_index = if written_slices == 0 {
            0
        } else {
            first_slice_index + written_slices
        };

        store.reposition_for_resume(start_index as u32)?;

        for (write_id, _) in &self.write_ids {
            conn.execute(
                "UPDATE writes SET status = 'in_progress' WHERE id = ?1",
                params![write_id],
            )?;
        }

        let outcome = run_entries(
            conn,
            store,
            self.built,
            self.volume_id,
            self.write_ids,
            self.slice_write_id,
            start_index,
            // A resume never parks: `start_index` is non-zero on any real
            // resume, and the hook is scoped to the fresh beginning-of-tape
            // execute it exists to make reachable.
            &mut is_interrupted,
            park_marker_from_env(),
        )?;
        Ok(outcome.into())
    }
}

impl ReadyToSeal {
    /// `ReadyToSeal -> SealedPending`. Regenerates the seal marker with the
    /// REAL `sealed_at` (the frozen placeholder from `build()` was sized
    /// only), asserts its byte length equals the frozen placeholder's exactly
    /// (if this ever fires, the placeholder-sizing trick broke — see
    /// `layout::generate_seal_marker`'s doc comment), and writes it with
    /// `sync=true` (the only entry in the whole session that uses a
    /// synchronous filemark).
    pub fn seal(self, store: &mut dyn Store) -> Result<SealedPending> {
        let seal_entry = self
            .built
            .layout
            .entries
            .iter()
            .find(|e| matches!(e.kind, ZoneKind::SealMarker))
            .ok_or_else(|| TapectlError::Other("seal: layout has no seal_marker entry".into()))?;
        let fi_entry = self
            .built
            .layout
            .entries
            .iter()
            .find(|e| matches!(e.kind, ZoneKind::FrontIndex))
            .ok_or_else(|| TapectlError::Other("seal: layout has no front_index entry".into()))?;
        let fi_hash = fi_entry.sha256.clone().ok_or_else(|| {
            TapectlError::Other("seal: front_index entry has no recorded hash".into())
        })?;
        let placeholder_len = seal_entry.size_bytes.ok_or_else(|| {
            TapectlError::Other("seal: seal_marker entry has no recorded size".into())
        })?;

        // The embedded copy: File 3's own entry gets its real size+hash
        // (known now); the seal marker's own entry stays bare (self-reference)
        // — same construction `build()` used for the placeholder
        // (`volume-format-v2.md` §4), reconstructed here from the Layout's
        // own entries rather than carried as extra state, since
        // `layout.entries` already has everything (verified: `build.rs`'s
        // `front_index_layout_entry_carries_its_true_size_and_hash` test
        // pins that the FrontIndex LayoutEntry carries its real size+hash).
        let seal_files: Vec<layout::FrontIndexFile> = self
            .built
            .layout
            .entries
            .iter()
            .map(|e| {
                let is_seal = matches!(e.kind, ZoneKind::SealMarker);
                layout::FrontIndexFile {
                    position: e.position,
                    type_label: e.kind.type_label(),
                    size_bytes: if is_seal { None } else { e.size_bytes },
                    sha256_encrypted: if is_seal { None } else { e.sha256.clone() },
                }
            })
            .collect();

        let real_seal_bytes = layout::generate_seal_marker(
            &self.built.layout.label,
            self.built.layout.entries.len() as i32,
            &fi_hash,
            &seal_files,
        );

        if real_seal_bytes.len() as u64 != placeholder_len {
            return Err(TapectlError::Other(format!(
                "seal: real seal marker ({} bytes) != frozen placeholder ({placeholder_len} \
                 bytes) — the placeholder-sizing trick broke; generate_seal_marker's timestamp \
                 must render at fixed width",
                real_seal_bytes.len()
            )));
        }

        store.execute(
            &mut Cursor::new(real_seal_bytes.as_bytes()),
            real_seal_bytes.len() as u64,
            true,
        )?;

        Ok(SealedPending {
            built: self.built,
            volume_id: self.volume_id,
            write_ids: self.write_ids,
        })
    }
}

/// The seal marker is on tape; confirm has not run yet. Its only operation
/// is [`Self::confirm`].
pub struct SealedPending {
    built: BuiltLayout,
    volume_id: i64,
    write_ids: Vec<(i64, i64)>,
}

impl SealedPending {
    /// `SealedPending -> ConfirmOutcome`. Runs `store.confirm` (the §5 chain
    /// walk), records a `verification_sessions` row (`verify_type`:
    /// Integrity -> 'full', Navigable -> 'quick', ADR-0001), then three
    /// outcomes, not two (ADR-0012's 2026-09-18 amendment, issues
    /// #260/#267): pass => ONE transaction flipping `writes` 'completed',
    /// `snapshots` 'current', `volumes` 'sealed'; a mismatch that
    /// `Evidence::proves_medium_bad` => `volumes.observed_condition`
    /// 'quarantined', `writes` 'aborted'; a mismatch that does NOT => nothing
    /// touched on `volumes`, `writes` 'interrupted' so `volume resume` can
    /// re-enter this same method (staging kept in every case — this method
    /// never touches staging).
    pub fn confirm(
        self,
        conn: &Connection,
        store: &mut dyn Store,
        tier: Tier,
    ) -> Result<ConfirmOutcome> {
        let verify_type = match tier {
            Tier::Integrity => "full",
            Tier::Navigable => "quick",
        };
        conn.execute(
            "INSERT INTO verification_sessions (volume_id, verify_type, outcome)
             VALUES (?1, ?2, 'in_progress')",
            params![self.volume_id, verify_type],
        )?;
        let vs_id = conn.last_insert_rowid();

        let evidence = store.confirm(&self.built.layout, tier)?;
        let passed = evidence.mismatches.is_empty();
        // ADR-0012's 2026-09-18 amendment: a mismatch alone is not a
        // quarantine verdict. `Tier::default()` is `Tier::Integrity`, so a
        // routine confirm reads back the WHOLE cartridge — hours on a full
        // LTO-6 — and one transient SCSI error in that window must not
        // condemn a physically sound tape.
        let proves_medium_bad = evidence.proves_medium_bad();

        conn.execute(
            "UPDATE verification_sessions
             SET completed_at = datetime('now'), outcome = ?1,
                 slices_checked = ?2, slices_passed = ?3, slices_failed = ?4
             WHERE id = ?5",
            params![
                if passed { "passed" } else { "failed" },
                evidence.files_checked as i64,
                if passed {
                    evidence.files_checked as i64
                } else {
                    0
                },
                if passed {
                    0
                } else {
                    evidence.mismatches.len() as i64
                },
                vs_id,
            ],
        )?;
        // Issue #142: the same per-mismatch detail `volume verify` records,
        // through the same writer — write-time confirm is the OTHER producer
        // of an `Evidence`, and a quarantine that cannot say which position
        // failed is as unhelpful here as it was there.
        super::write::record_verification_results(conn, vs_id, self.volume_id, &evidence)?;

        if passed {
            let tx = conn.unchecked_transaction()?;
            for (write_id, _) in &self.write_ids {
                tx.execute(
                    "UPDATE writes SET status = 'completed', completed_at = datetime('now')
                     WHERE id = ?1",
                    params![write_id],
                )?;
            }
            // Snapshot promotions, plus what each one needs for its audit row
            // (issue #58). The pre-flip status is read BEFORE the update so the
            // event records a real old->new transition rather than guessing,
            // and so nothing is logged when the guard matches no row.
            let mut promoted: Vec<(i64, String, String, i64)> = Vec::new();
            for (_, snapshot_id) in &self.write_ids {
                let before: Option<(String, String, i64)> = tx
                    .query_row(
                        "SELECT s.status, u.name, u.tenant_id
                         FROM snapshots s JOIN units u ON u.id = s.unit_id
                         WHERE s.id = ?1",
                        params![snapshot_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .ok();

                let changed = tx.execute(
                    "UPDATE snapshots SET status = 'current'
                     WHERE id = ?1 AND status IN ('created', 'staged')",
                    params![snapshot_id],
                )?;

                if changed > 0 {
                    if let Some((old_status, unit_name, tenant_id)) = before {
                        promoted.push((*snapshot_id, old_status, unit_name, tenant_id));
                    }
                }
            }
            tx.execute(
                "UPDATE volumes SET status = 'sealed' WHERE id = ?1",
                params![self.volume_id],
            )?;
            tx.commit()?;

            // Audit rows are emitted AFTER the commit, deliberately, and a
            // failure here is warned about rather than propagated (issue #58).
            //
            // Inside the transaction, a failing audit insert would roll back
            // the seal — leaving a tape that is physically sealed (the seal
            // marker is already on it; `seal` ran before `confirm`) while the
            // catalog still calls the volume unsealed. That is a divergence
            // ADR-0001 would quarantine on at next contact, traded for an
            // advisory row. The audit trail is advisory; the seal record is
            // not. Losing an event is strictly the cheaper failure, so the
            // ordering is chosen rather than incidental.
            for (snapshot_id, old_status, unit_name, tenant_id) in promoted {
                if let Err(e) = crate::db::events::log_field_change(
                    conn,
                    "snapshot",
                    snapshot_id,
                    &unit_name,
                    "sealed_current",
                    "status",
                    Some(&old_status),
                    "current",
                    Some(tenant_id),
                ) {
                    tracing::warn!(
                        snapshot_id,
                        unit = %unit_name,
                        error = %e,
                        "sealed successfully, but the snapshot-promotion audit row could not be written"
                    );
                }
            }
            Ok(ConfirmOutcome::Sealed(SealedSession {
                volume_id: self.volume_id,
                label: self.built.layout.label.clone(),
            }))
        } else if proves_medium_bad {
            // Issue #242: the chain-walk's own quarantine finding is an
            // observed fact too -- `observed_condition`, not `status`. This
            // volume never reached the transaction above, so `status` is
            // left exactly where it was (never 'sealed').
            conn.execute(
                "UPDATE volumes SET observed_condition = 'quarantined' WHERE id = ?1",
                params![self.volume_id],
            )?;
            mark_writes(conn, &self.write_ids, "aborted")?;
            Ok(ConfirmOutcome::Quarantined(QuarantinedSession {
                volume_id: self.volume_id,
                label: self.built.layout.label.clone(),
                reason: QuarantineReason::ConfirmFailed(evidence),
            }))
        } else {
            // ADR-0012's 2026-09-18 amendment (issues #260/#267): nothing
            // here proves the medium bad, so nothing is asserted about it
            // either way -- `observed_condition` is left exactly as it was
            // (and `status` was never touched by this branch to begin
            // with). `writes` rows move to `interrupted`, NOT `aborted`:
            // confirm is idempotent and the tape is physically unchanged by
            // a failed read, so `tapectl volume resume` must be able to
            // pick this session back up (`rehydrate` selects only
            // `interrupted` rows) and re-enter confirm.
            mark_writes(conn, &self.write_ids, "interrupted")?;
            Ok(ConfirmOutcome::Inconclusive(InconclusiveSession {
                volume_id: self.volume_id,
                label: self.built.layout.label.clone(),
                evidence,
            }))
        }
    }
}

// ── shared execute loop ──

/// The shared per-entry execute loop (§9's `execute`): both a fresh
/// `PlannedSession::execute` (`start_index = 0`) and a resumed
/// `InterruptedSession::resume` (`start_index` from the two-case cursor
/// rule) funnel through this, so there is exactly one implementation of
/// "stream an entry, update its cursor row." Never includes the seal marker
/// entry — that is `ReadyToSeal::seal`'s job alone (sacred invariant 1).
///
/// Status: cycles 1-3 landed (happy path; tri-layer L2 hash verification
/// with a clean abort on mismatch; a `store.execute` error — ENOSPC being
/// the expected one, but any of them — is caught and produces the same
/// clean abort, never a hard `Err` out of the whole session). Still
/// pending: cycle 4's `is_interrupted` check (currently unused — accepted
/// but not called, since the public `execute_checking`/`resume_checking`
/// signatures are already the final ones the four behaviors need).
#[allow(clippy::too_many_arguments)]
/// The ONE place the park hook's environment variable is read (issue #113).
///
/// Deliberately a boundary function rather than a lookup inside `run_entries`:
/// environment variables are process-global, so an in-process test that set
/// one would leak into every other test running in parallel in the same
/// binary — reintroducing exactly the nondeterminism #113 exists to remove.
/// With the marker passed in as a parameter, the hook is testable by passing
/// `Some(path)` and no test ever touches the environment.
fn park_marker_from_env() -> Option<String> {
    std::env::var("TAPECTL_TEST_PAUSE_AFTER_PLAN").ok()
}

// Nine parameters, one over clippy's threshold. Grouping them into a struct
// would be churn for its own sake: this is a private function with exactly
// two call sites, and every argument is already a distinct piece of session
// state that the typestate deliberately keeps separate. Same allow the audit
// trail's `log_event` carries for the same reason.
#[allow(clippy::too_many_arguments)]
fn run_entries(
    conn: &Connection,
    store: &mut dyn Store,
    built: BuiltLayout,
    volume_id: i64,
    write_ids: Vec<(i64, i64)>,
    slice_write_id: HashMap<i64, i64>,
    start_index: usize,
    is_interrupted: &mut dyn FnMut() -> bool,
    park_marker: Option<String>,
) -> Result<ExecuteOutcome> {
    let content_entries: Vec<&LayoutEntry> = built
        .layout
        .entries
        .iter()
        .filter(|e| !matches!(e.kind, ZoneKind::SealMarker))
        .collect();

    // Issue #113: the beginning-of-tape resume arm of the mhvtl gate used to
    // race. It waited on `writes.status='in_progress'` — true from `plan()`,
    // BEFORE any entry is confirmed — and then signalled, so the writer could
    // confirm entry 0 inside the signal-delivery window and break the arm's
    // "zero confirmed" assertion. It reds roughly 1 run in 3.
    //
    // That is structural, not tuning: "in_progress with zero confirmed" is a
    // MOMENT, not a state a poller can latch onto. Widening the bound to 0..1
    // was rejected — it turns the BOT arm into a duplicate of the midwrite arm
    // and deletes the beginning-of-tape case entirely. So the race is removed
    // at its source: when this variable names a path, execute parks HERE, at
    // exactly the BOT state, and announces it by creating that file. The gate
    // waits for the file (a fact, not a timing guess), then signals.
    //
    // The park loop reuses `is_interrupted` rather than inventing a second
    // mechanism, so the wake-up path is the very code the arm exists to test.
    // Three safety properties, all deliberate:
    //   - unset variable => not one branch taken, zero production behavior
    //     change (this is a runtime check because `#[cfg(test)]` does not
    //     reach integration binaries — the #87 finding);
    //   - it warns LOUDLY, so it can never be doing this silently on a real
    //     tape;
    //   - it TIMES OUT back into the normal write rather than parking
    //     forever, so a gate that never signals fails on its assertion with a
    //     sealed volume instead of hanging the suite.
    if start_index == 0 {
        if let Some(marker) = park_marker {
            tracing::warn!(
                marker = %marker,
                "TAPECTL_TEST_PAUSE_AFTER_PLAN is set — parking after plan() with no \
                 entry written, waiting to be interrupted. This is a TEST hook; it must \
                 never be set for a real write."
            );
            std::fs::write(&marker, "parked\n").map_err(|e| {
                TapectlError::Other(format!(
                    "TAPECTL_TEST_PAUSE_AFTER_PLAN: cannot create readiness marker {marker}: {e}"
                ))
            })?;

            let parked_at = std::time::Instant::now();
            let limit = std::time::Duration::from_secs(120);
            while !is_interrupted() {
                if parked_at.elapsed() >= limit {
                    tracing::warn!(
                        "TAPECTL_TEST_PAUSE_AFTER_PLAN: no interrupt within 120s — \
                         proceeding with a normal write so the caller fails on its own \
                         assertion rather than hanging"
                    );
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            if is_interrupted() {
                mark_writes(conn, &write_ids, "interrupted")?;
                return Ok(ExecuteOutcome::Interrupted(InterruptedSession {
                    built,
                    volume_id,
                    write_ids,
                    slice_write_id,
                }));
            }
        }
    }

    for entry in &content_entries[start_index..] {
        // Checked BETWEEN entries only — a mid-file kill is a crash, handled
        // by the startup sweep (`crate::db::recover_orphaned_sessions`), not
        // here. Checking before every entry (including the very first of
        // this call) is still "between entries": on a fresh execute there is
        // nothing before entry 0 to interrupt; on a resumed call it is
        // between the previous call's last entry and this one's first.
        if is_interrupted() {
            mark_writes(conn, &write_ids, "interrupted")?;
            return Ok(ExecuteOutcome::Interrupted(InterruptedSession {
                built,
                volume_id,
                write_ids,
                slice_write_id,
            }));
        }

        let path = entry_path(entry)?;
        let size = entry.size_bytes.ok_or_else(|| {
            TapectlError::Other(format!(
                "execute: entry at position {} has no recorded size \
                 (validate should have caught this)",
                entry.position
            ))
        })?;

        // Stream the entry through the hashing tee reader into the store.
        // Any store-level failure here — ENOSPC being the expected one,
        // but this treats any of them alike (device gone, I/O error, ...) —
        // is caught rather than propagated: a full medium has no salvage
        // path (ADR-0007), so it becomes the same clean abort as a hash
        // mismatch, not a hard `Err` out of the whole session.
        let stream_result: Result<String> = (|| {
            let file = File::open(path).map_err(|e| {
                TapectlError::Other(format!(
                    "execute: open entry at position {}: {e}",
                    entry.position
                ))
            })?;
            let mut reader = HashingReader::new(file);
            store.execute(&mut reader, size, false)?;
            Ok(reader.finalize_hex())
        })();

        // Tri-layer L2 (`v2-open-questions.md` §2.4): re-hash inline on the
        // same streaming read that fed the store, and clean-abort on
        // mismatch. This is what closes the validate->execute TOCTOU window
        // — `validate` already full-hashed this same file from disk, but a
        // rot between then and now would otherwise land on tape unnoticed.
        let expected_hash = entry.sha256.as_deref();
        let abort_reason = match &stream_result {
            Err(e) => Some(format!(
                "execute failed at position {}: {e}",
                entry.position
            )),
            Ok(actual_hash) if expected_hash != Some(actual_hash.as_str()) => Some(format!(
                "hash mismatch at position {}: expected {expected_hash:?}, got {actual_hash}",
                entry.position
            )),
            Ok(_) => None,
        };

        if let ZoneKind::Slice { stage_slice_id } = entry.kind {
            let write_id = *slice_write_id
                .get(&stage_slice_id)
                .expect("plan() populated slice_write_id for every slice entry");
            match (&stream_result, &abort_reason) {
                (Ok(actual_hash), None) => {
                    conn.execute(
                        "UPDATE write_positions
                         SET status = 'written', written_at = datetime('now'),
                             sha256_on_volume = ?1
                         WHERE write_id = ?2 AND stage_slice_id = ?3",
                        params![actual_hash, write_id, stage_slice_id],
                    )?;
                }
                (Ok(actual_hash), Some(_)) => {
                    // Streamed, but the hash didn't match.
                    conn.execute(
                        "UPDATE write_positions SET status = 'failed', sha256_on_volume = ?1
                         WHERE write_id = ?2 AND stage_slice_id = ?3",
                        params![actual_hash, write_id, stage_slice_id],
                    )?;
                }
                (Err(_), _) => {
                    // Never streamed at all (open failed or store.execute
                    // errored) — no sha256_on_volume to record.
                    conn.execute(
                        "UPDATE write_positions SET status = 'failed'
                         WHERE write_id = ?1 AND stage_slice_id = ?2",
                        params![write_id, stage_slice_id],
                    )?;
                }
            }
        }

        if let Some(reason) = abort_reason {
            mark_writes(conn, &write_ids, "aborted")?;
            return Ok(ExecuteOutcome::Aborted(AbortedSession {
                volume_id,
                reason,
            }));
        }
    }

    Ok(ExecuteOutcome::Ready(ReadyToSeal {
        built,
        volume_id,
        write_ids,
    }))
}

fn entry_path(entry: &LayoutEntry) -> Result<&Path> {
    match &entry.source {
        ContentSource::Staged(p) | ContentSource::Materialized(p) => Ok(p.as_path()),
        ContentSource::Generated => Err(TapectlError::Other(format!(
            "execute: entry at position {} has ContentSource::Generated (v1-only; \
             build::build never produces this)",
            entry.position
        ))),
    }
}

fn mark_writes(conn: &Connection, write_ids: &[(i64, i64)], status: &str) -> Result<()> {
    for (write_id, _) in write_ids {
        conn.execute(
            "UPDATE writes SET status = ?1 WHERE id = ?2",
            params![status, write_id],
        )?;
    }
    Ok(())
}

/// The two-case cursor rule's slice count: how many `write_positions` rows
/// across this session's `writes` rows are already `'written'`. Zero means
/// restart from BOT; any other value feeds `front_zone_len + written_slices`
/// (`docs/design/layout-session.md`'s Resume rule).
fn count_written_slices(conn: &Connection, write_ids: &[(i64, i64)]) -> Result<usize> {
    let mut total: i64 = 0;
    for (write_id, _) in write_ids {
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM write_positions WHERE write_id = ?1 AND status = 'written'",
            params![write_id],
            |r| r.get(0),
        )?;
        total += n;
    }
    Ok(total as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::store::MemStore;
    use crate::volume::build::{self, BuildInputs, BuildSlice, TenantInfo};
    use rusqlite::params;
    use sha2::{Digest, Sha256};
    use std::io::Write;
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tempfile::TempDir;

    const BS: u64 = 512 * 1024;

    fn sha_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }

    /// A fully seeded fixture: an in-memory DB with one operator tenant, one
    /// content tenant ("alpha") with one unit/snapshot/stage_set/2 staged
    /// slices (real bytes on disk, matching the recorded hashes so
    /// `validate`'s tri-layer L1 passes), one volume row, and the
    /// `BuiltLayout` + `KeyAvailability` + `BuildUnit` list a session needs.
    /// The two `TempDir` guards (slice source files, session materialize
    /// dir) must outlive anything built from the returned `BuiltLayout`.
    struct Fixture {
        conn: Connection,
        built: BuiltLayout,
        keys: KeyAvailability,
        units: Vec<BuildUnit>,
        volume_id: i64,
        _slices_dir: tempfile::TempDir,
        _session_dir: tempfile::TempDir,
    }

    fn make_fixture() -> Fixture {
        let conn = db::open_memory().unwrap();

        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('operator', 1, 'active')",
            [],
        )
        .unwrap();
        let operator_id = conn.last_insert_rowid();
        let op_key = crate::crypto::keys::generate_keypair();
        conn.execute(
            "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key, key_type, is_active)
             VALUES (?1, 'operator-key', ?2, ?3, 'primary', 1)",
            params![operator_id, op_key.fingerprint, op_key.public_key],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('alpha', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        let tenant_key = crate::crypto::keys::generate_keypair();
        conn.execute(
            "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key, key_type, is_active)
             VALUES (?1, 'alpha-key', ?2, ?3, 'primary', 1)",
            params![tenant_id, tenant_key.fingerprint, tenant_key.public_key],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES ('unit-uuid-1', 'unit-alpha', ?1, '/tmp/unit-alpha', 'active')",
            params![tenant_id],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'staged', '/tmp/unit-alpha', 1, 32)",
            params![unit_id],
        )
        .unwrap();
        let snapshot_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
            params![snapshot_id],
        )
        .unwrap();
        let stage_set_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('SESSTEST', 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
            [],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();

        let slices_dir = tempfile::tempdir().unwrap();
        let slice_1 = fake_slice(
            &conn,
            slices_dir.path(),
            stage_set_id,
            1,
            b"first staged slice bytes",
        );
        let slice_2 = fake_slice(
            &conn,
            slices_dir.path(),
            stage_set_id,
            2,
            b"second staged slice bytes, a bit longer",
        );

        let build_unit = BuildUnit {
            stage_set_id,
            snapshot_id,
            unit_name: "unit-alpha".to_string(),
            unit_uuid: "unit-uuid-1".to_string(),
            tenant_id,
            dar_version: Some("2.7.20".to_string()),
            dar_command: Some("dar -c base -R /src".to_string()),
            catalog_path: None,
            snapshot_version: 1,
            slices: vec![slice_1, slice_2],
        };

        let inputs = BuildInputs {
            label: "SESSTEST".to_string(),
            volume_uuid: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            media_type: "LTO-6".to_string(),
            tapectl_version: "0.1.0-test".to_string(),
            created_at: "2026-07-22T20:09:00Z".to_string(),
            block_size: BS,
            usable_bytes: 1000 * BS,
            enospc_buffer: BS,
            nominal_capacity: 2_500_000_000_000,
            mam_capacity: 0,
            mam_manufacturer: String::new(),
            mam_serial: String::new(),
            cartridge_identity_source: None,
            mam_length: 0,
            mam_loads: 0,
            units: vec![build_unit.clone()],
            tenants: vec![TenantInfo {
                tenant_id,
                tenant_name: "alpha".to_string(),
                public_keys: vec![tenant_key.public_key],
            }],
            operator_public_keys: vec![op_key.public_key],
            escrow_public_key: None,
            catalog_db_path: None,
        };

        let session_dir = tempfile::tempdir().unwrap();
        let built = build::build(&inputs, session_dir.path()).unwrap();

        let keys = KeyAvailability {
            tenant_ids: vec![tenant_id],
            tenants_with_active_key: [tenant_id].into_iter().collect(),
            operator_key_present: true,
            escrow_recipient_present: None,
            stage_sets_lacking_escrow: None,
        };

        Fixture {
            conn,
            built,
            keys,
            units: vec![build_unit],
            volume_id,
            _slices_dir: slices_dir,
            _session_dir: session_dir,
        }
    }

    /// Inserts a real `stage_slices` row (so `write_positions`'s FK on
    /// `stage_slice_id` is satisfiable) and writes the matching bytes to
    /// disk, returning a `BuildSlice` with the DB-assigned `slice_id`.
    fn fake_slice(
        conn: &Connection,
        dir: &Path,
        stage_set_id: i64,
        slice_number: i64,
        content: &[u8],
    ) -> BuildSlice {
        let sha_plain = sha_hex(b"plaintext hash is not exercised by this fixture");
        let sha_enc = sha_hex(content);
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                        sha256_plain, sha256_encrypted)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                stage_set_id,
                slice_number,
                content.len() as i64,
                content.len() as i64,
                sha_plain,
                sha_enc,
            ],
        )
        .unwrap();
        let slice_id = conn.last_insert_rowid();

        let path = dir.join(format!("slice_{slice_id}.age"));
        std::fs::File::create(&path)
            .unwrap()
            .write_all(content)
            .unwrap();
        conn.execute(
            "UPDATE stage_slices SET staging_path = ?1 WHERE id = ?2",
            params![path.to_string_lossy(), slice_id],
        )
        .unwrap();

        BuildSlice {
            slice_id,
            slice_number,
            size_bytes: content.len() as i64,
            encrypted_bytes: content.len() as i64,
            sha256_plain: sha_plain,
            sha256_encrypted: sha_enc,
            staging_path: path,
        }
    }

    // --- behavior 1: happy path over MemStore ends Sealed ----------------

    #[test]
    fn happy_path_over_memstore_ends_sealed_with_correct_db_rows() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);
        let expected_file_count = f.built.layout.entries.len();

        let validated = f
            .built
            .into_validated(&f.keys, &mut store)
            .expect("validate should pass for a well-formed fixture");
        let planned = validated
            .plan(&f.conn, f.volume_id, &f.units)
            .expect("plan should insert writes/write_positions rows");
        let ready = match planned
            .execute(&f.conn, &mut store)
            .expect("execute should not error")
        {
            ExecuteOutcome::Ready(r) => r,
            _ => panic!("expected Ready on a happy-path MemStore run"),
        };
        let sealed_pending = ready.seal(&mut store).expect("seal should succeed");
        let outcome = sealed_pending
            .confirm(&f.conn, &mut store, Tier::Integrity)
            .expect("confirm should not error");

        let sealed = match outcome {
            ConfirmOutcome::Sealed(s) => s,
            ConfirmOutcome::Quarantined(q) => panic!(
                "expected Sealed on a happy-path MemStore run, got Quarantined: {:?}",
                match q.reason {
                    QuarantineReason::ConfirmFailed(e) => format!("{:?}", e.mismatches),
                    other => format!("{other:?}"),
                }
            ),
            ConfirmOutcome::Inconclusive(inc) => panic!(
                "expected Sealed on a happy-path MemStore run, got Inconclusive: {:?}",
                inc.evidence.mismatches
            ),
        };
        assert_eq!(sealed.volume_id, f.volume_id);
        assert_eq!(sealed.label, "SESSTEST");

        // --- DB row assertions ---
        let volume_status: String = f
            .conn
            .query_row(
                "SELECT status FROM volumes WHERE id = ?1",
                params![f.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(volume_status, "sealed");

        let write_statuses: Vec<String> = f
            .conn
            .prepare("SELECT status FROM writes WHERE volume_id = ?1")
            .unwrap()
            .query_map(params![f.volume_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(write_statuses.len(), 1, "one unit => one writes row");
        assert_eq!(write_statuses[0], "completed");

        let snapshot_status: String = f
            .conn
            .query_row("SELECT status FROM snapshots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(snapshot_status, "current");

        let written_positions: i64 = f
            .conn
            .query_row(
                "SELECT COUNT(*) FROM write_positions WHERE status = 'written'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(written_positions, 2, "both staged slices recorded written");

        let vs_count: i64 = f
            .conn
            .query_row(
                "SELECT COUNT(*) FROM verification_sessions WHERE volume_id = ?1 AND outcome = 'passed' AND verify_type = 'full'",
                params![f.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(vs_count, 1);

        // The store actually holds every entry, seal marker included.
        assert_eq!(store.files.len(), expected_file_count);

        // Issue #58: the created/staged -> current promotion is auditable.
        // Before this, confirm logged only a volume-level `write_completed`,
        // so `events` could not answer when or why a snapshot became current
        // — the transition that makes it count as coverage.
        let (action, field, old_value, new_value): (
            String,
            String,
            Option<String>,
            Option<String>,
        ) = f
            .conn
            .query_row(
                "SELECT action, field, old_value, new_value FROM events
                     WHERE entity_type = 'snapshot' AND action = 'sealed_current'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .expect("confirm must log the snapshot promotion");
        assert_eq!(action, "sealed_current");
        assert_eq!(field, "status");
        assert_eq!(
            new_value.as_deref(),
            Some("current"),
            "the event must record the new status"
        );
        assert!(
            matches!(old_value.as_deref(), Some("created") | Some("staged")),
            "the event must record the REAL pre-flip status, not a guess — got {old_value:?}"
        );
    }

    /// Migration 006 / issue #25: `plan` is the only writer of
    /// `writes.session_dir`, and every row it inserts must carry the frozen
    /// staging directory — without it a restarted process has no way back to
    /// the materialized zones, and the session is abortable but not
    /// resumable.
    #[test]
    fn plan_records_the_session_dir_on_every_writes_row() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);
        let expected = f.built.session_dir.to_string_lossy().to_string();

        let validated = f.built.into_validated(&f.keys, &mut store).unwrap();
        validated.plan(&f.conn, f.volume_id, &f.units).unwrap();

        let recorded: Vec<Option<String>> = f
            .conn
            .prepare("SELECT session_dir FROM writes WHERE volume_id = ?1")
            .unwrap()
            .query_map(params![f.volume_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].as_deref(), Some(expected.as_str()));
    }

    // --- behavior 2: injected hash mismatch mid-execute -------------------

    #[test]
    fn hash_mismatch_mid_execute_aborts_unsealed_with_no_seal_marker_written() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);
        let seal_position = f
            .built
            .layout
            .entries
            .iter()
            .position(|e| matches!(e.kind, ZoneKind::SealMarker))
            .expect("fixture layout always has a seal marker");

        // validate + plan while the staged slice is still good (this is the
        // TOCTOU window tri-layer L2 exists to close: the file rots AFTER
        // validate/plan, not before — corrupting it before validate would
        // just make validate itself reject it, never reaching execute).
        let validated = f.built.into_validated(&f.keys, &mut store).unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();

        // Corrupt one staged slice's on-disk bytes now, after plan.
        let slice_path = f.units[0].slices[0].staging_path.clone();
        let mut bytes = std::fs::read(&slice_path).unwrap();
        bytes[0] ^= 0xFF;
        std::fs::write(&slice_path, bytes).unwrap();
        let corrupted_slice_id = f.units[0].slices[0].slice_id;

        let outcome = planned
            .execute(&f.conn, &mut store)
            .expect("execute should not hard-error on a hash mismatch — it's a clean abort");

        let aborted = match outcome {
            ExecuteOutcome::Aborted(a) => a,
            ExecuteOutcome::Ready(_) => panic!("expected Aborted on a hash mismatch, got Ready"),
            ExecuteOutcome::Interrupted(_) => {
                panic!("expected Aborted on a hash mismatch, got Interrupted")
            }
        };
        assert_eq!(aborted.volume_id, f.volume_id);

        // The tape stays UNSEALED: no seal marker was ever written, because
        // seal() was never called (sacred invariant 1 — only seal() can
        // produce that entry, and this abort happens well before it).
        assert!(
            store.files.len() <= seal_position,
            "MemStore must not contain a seal marker entry after an aborted execute; \
             files.len()={}, seal position={seal_position}",
            store.files.len(),
        );

        // writes rows: 'aborted', never 'completed'.
        let write_statuses: Vec<String> = f
            .conn
            .prepare("SELECT status FROM writes WHERE volume_id = ?1")
            .unwrap()
            .query_map(params![f.volume_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(write_statuses, vec!["aborted".to_string()]);

        // volumes.status must NOT be 'sealed' — the tape stays unsealed.
        let volume_status: String = f
            .conn
            .query_row(
                "SELECT status FROM volumes WHERE id = ?1",
                params![f.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(volume_status, "sealed");

        // The specific corrupted slice's cursor row reflects the failure,
        // not a false 'written'.
        let wp_status: String = f
            .conn
            .query_row(
                "SELECT status FROM write_positions WHERE stage_slice_id = ?1",
                params![corrupted_slice_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(wp_status, "failed");
    }

    // --- behavior 3: ENOSPC mid-execute cleanly aborts (same as mismatch) -

    #[test]
    fn enospc_mid_execute_aborts_unsealed_same_as_hash_mismatch() {
        let f = make_fixture();
        // A budget that lets a few entries land, then fails — MemStore has
        // no other way to simulate a full medium
        // (`MemStore::with_enospc_after`'s own doc comment).
        let mut store = MemStore::new(BS as usize).with_enospc_after(2 * BS);
        let seal_position = f
            .built
            .layout
            .entries
            .iter()
            .position(|e| matches!(e.kind, ZoneKind::SealMarker))
            .expect("fixture layout always has a seal marker");

        let validated = f.built.into_validated(&f.keys, &mut store).unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();

        let outcome = planned.execute(&f.conn, &mut store).expect(
            "execute should not hard-error on ENOSPC — it's a clean abort, same as a hash mismatch",
        );

        let aborted = match outcome {
            ExecuteOutcome::Aborted(a) => a,
            ExecuteOutcome::Ready(_) => panic!("expected Aborted on ENOSPC, got Ready"),
            ExecuteOutcome::Interrupted(_) => {
                panic!("expected Aborted on ENOSPC, got Interrupted")
            }
        };
        assert_eq!(aborted.volume_id, f.volume_id);

        // Same clean-abort shape as the hash-mismatch behavior: unsealed
        // (no seal marker reached), writes 'aborted', volume not 'sealed'.
        assert!(
            store.files.len() <= seal_position,
            "MemStore must not contain a seal marker entry after an ENOSPC abort; \
             files.len()={}, seal position={seal_position}",
            store.files.len(),
        );

        let write_statuses: Vec<String> = f
            .conn
            .prepare("SELECT status FROM writes WHERE volume_id = ?1")
            .unwrap()
            .query_map(params![f.volume_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(write_statuses, vec!["aborted".to_string()]);

        let volume_status: String = f
            .conn
            .query_row(
                "SELECT status FROM volumes WHERE id = ?1",
                params![f.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(volume_status, "sealed");
    }

    // --- behavior 4: SIGINT between entries -> Interrupted + resumable ---

    #[test]
    fn sigint_between_entries_interrupts_and_resume_completes_the_session() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);

        let validated = f.built.into_validated(&f.keys, &mut store).unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();

        // The fixture's content entries (seal excluded) are, in order:
        // id_thunk, guide, restore_sh, front_index, tenant_envelope,
        // operator_envelope, operator_envelope_backup, slice_1, slice_2 —
        // 9 entries, slice_1 at index 7. Fire "interrupted" starting on the
        // 9th check (0-indexed call count >= 8), i.e. AFTER slice_1 (index 7)
        // has been fully processed but BEFORE slice_2 (index 8) is even
        // opened — exercising the two-case cursor rule's more interesting
        // branch (>=1 slice written) rather than the zero-slices/BOT case.
        let calls = AtomicU32::new(0);
        let is_interrupted = move || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            n >= 8
        };

        let interrupted = match planned
            .execute_checking(&f.conn, &mut store, is_interrupted)
            .expect("execute_checking should not error on a clean interruption")
        {
            ExecuteOutcome::Interrupted(i) => i,
            ExecuteOutcome::Ready(_) => panic!("expected Interrupted, got Ready"),
            ExecuteOutcome::Aborted(a) => panic!("expected Interrupted, got Aborted: {}", a.reason),
        };

        // DB state right after interruption: writes 'interrupted', slice_1
        // recorded 'written', slice_2 still 'pending' (never touched).
        let write_status: String = f
            .conn
            .query_row(
                "SELECT status FROM writes WHERE volume_id = ?1",
                params![f.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(write_status, "interrupted");

        let slice_1_id = f.units[0].slices[0].slice_id;
        let slice_2_id = f.units[0].slices[1].slice_id;
        let slice_1_status: String = f
            .conn
            .query_row(
                "SELECT status FROM write_positions WHERE stage_slice_id = ?1",
                params![slice_1_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(slice_1_status, "written");
        let slice_2_status: String = f
            .conn
            .query_row(
                "SELECT status FROM write_positions WHERE stage_slice_id = ?1",
                params![slice_2_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(slice_2_status, "pending");

        // The store itself only has the 8 entries written before the
        // interruption fired (id_thunk..slice_1), definitely no seal marker.
        assert_eq!(store.files.len(), 8);

        // --- Resume, with a predicate that never interrupts ---
        let ready = match interrupted
            .resume_checking(&f.conn, &f.keys, &mut store, || false)
            .expect("resume_checking should not error")
        {
            ResumeOutcome::Ready(r) => r,
            _ => {
                panic!("expected Ready after a clean resume to completion, got a different outcome")
            }
        };
        let sealed_pending = ready.seal(&mut store).expect("seal should succeed");
        let outcome = sealed_pending
            .confirm(&f.conn, &mut store, Tier::Integrity)
            .expect("confirm should not error");
        match outcome {
            ConfirmOutcome::Sealed(_) => {}
            ConfirmOutcome::Quarantined(_) => panic!("expected Sealed after a completed resume"),
            ConfirmOutcome::Inconclusive(inc) => panic!(
                "expected Sealed after a completed resume, got Inconclusive: {:?}",
                inc.evidence.mismatches
            ),
        }

        // Final DB state matches the ordinary happy path exactly.
        let volume_status: String = f
            .conn
            .query_row(
                "SELECT status FROM volumes WHERE id = ?1",
                params![f.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(volume_status, "sealed");
        let write_status: String = f
            .conn
            .query_row(
                "SELECT status FROM writes WHERE volume_id = ?1",
                params![f.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(write_status, "completed");
        let written_positions: i64 = f
            .conn
            .query_row(
                "SELECT COUNT(*) FROM write_positions WHERE status = 'written'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(written_positions, 2, "both slices written after resume");
    }

    /// Bonus coverage beyond the four required TDD behaviors: the two-case
    /// cursor rule's OTHER branch. The SIGINT test above exercises ">=1
    /// slice written -> reposition"; this exercises "zero slices written ->
    /// restart from BOT" by interrupting before even the first entry lands.
    /// Not written test-first (the same `resume_checking` implementation
    /// cycle 4 built already covers this branch), but still a real
    /// assertion of `docs/design/layout-session.md`'s cursor rule, not a
    /// change to it.
    #[test]
    fn resume_after_interrupt_before_any_entry_restarts_from_bot() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);

        let validated = f.built.into_validated(&f.keys, &mut store).unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();

        // Interrupt on the very first check, before entry 0 (id_thunk) is
        // even opened.
        let interrupted = match planned
            .execute_checking(&f.conn, &mut store, || true)
            .unwrap()
        {
            ExecuteOutcome::Interrupted(i) => i,
            _ => panic!("expected Interrupted"),
        };
        assert_eq!(store.files.len(), 0, "nothing written before interruption");

        let written_slices = count_written_slices(&f.conn, &interrupted.write_ids).unwrap();
        assert_eq!(written_slices, 0);

        let ready = match interrupted
            .resume_checking(&f.conn, &f.keys, &mut store, || false)
            .unwrap()
        {
            ResumeOutcome::Ready(r) => r,
            _ => panic!("expected Ready after resuming from BOT"),
        };
        let sealed_pending = ready.seal(&mut store).unwrap();
        match sealed_pending
            .confirm(&f.conn, &mut store, Tier::Integrity)
            .unwrap()
        {
            ConfirmOutcome::Sealed(_) => {}
            ConfirmOutcome::Quarantined(_) => panic!("expected Sealed"),
            ConfirmOutcome::Inconclusive(inc) => {
                panic!(
                    "expected Sealed, got Inconclusive: {:?}",
                    inc.evidence.mismatches
                )
            }
        }

        let volume_status: String = f
            .conn
            .query_row(
                "SELECT status FROM volumes WHERE id = ?1",
                params![f.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(volume_status, "sealed");
    }

    /// ADR-0012's 2026-09-21 amendment ("`volume resume` re-confirms a tape
    /// that is already sealed", issues #260/#267) — this test PINS THE
    /// OPPOSITE of what it used to. Before that amendment,
    /// `resume_checking` quarantined ANY already-sealed tape unconditionally
    /// (see the superseded doc below, kept for context). Now: File 0's
    /// identity matches AND the tape's own recorded seal pointer agrees with
    /// this session's own Layout — the exact state `seal()` legitimately
    /// leaves behind when confirm crashes or returns `Inconclusive` — so
    /// resume must re-enter confirm (`ResumeOutcome::Confirming`), not
    /// quarantine. `resume_quarantines_a_foreign_tape_that_happens_to_be_sealed`
    /// below is this test's necessary twin: it proves a tape that merely
    /// LOOKS sealed but fails the identity condition still quarantines
    /// exactly as before — the guarantee narrowed from "any sealed tape" to
    /// "any sealed tape that fails one of the three conditions", it was not
    /// dropped.
    ///
    /// Original (now-superseded) rationale, kept because the physical
    /// scenario it describes is still exactly what this test sets up:
    /// layout-session.md's Resume rule: "The absent seal marker confirms the
    /// tape is legitimately unsealed (safe to resume, not an append to a
    /// sealed volume)." The live path: seal() writes the marker, confirm
    /// crashes (or ends `Inconclusive`), then `recover_orphaned_sessions`
    /// sweeps the `in_progress` row to `interrupted` (resumable) and resume
    /// is handed an already-sealed tape. The File-0 identity check ALONE
    /// cannot tell this apart from a genuinely divergent tape — the identity
    /// matches either way — which is exactly why `resume_reconfirm_eligible`
    /// checks the seal POSITION too, against the tape's own File-0 pointer,
    /// never a guess.
    #[test]
    fn resume_reenters_confirm_when_already_sealed_tape_matches_this_sessions_layout() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);

        let label = f.built.layout.label.clone();
        let uuid = f.built.layout.volume_uuid.clone();
        let seal_pos = f
            .built
            .layout
            .entries
            .iter()
            .find(|e| matches!(e.kind, ZoneKind::SealMarker))
            .expect("layout has a seal marker")
            .position as u32;

        let validated = f.built.into_validated(&f.keys, &mut store).unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let interrupted = match planned
            .execute_checking(&f.conn, &mut store, || true)
            .unwrap()
        {
            ExecuteOutcome::Interrupted(i) => i,
            _ => panic!("expected Interrupted"),
        };

        // Put a real, parseable seal marker at the tape's seal position — the
        // state a crash-after-seal leaves behind. Everything up to it is
        // whatever the interrupted execute wrote (or nothing), which is
        // exactly what the sweep would hand back.
        let seal_bytes = layout::generate_seal_marker("SESSTEST", 1, "deadbeef", &[]).into_bytes();
        let mut padded = seal_bytes;
        padded.resize(BS as usize, 0);
        if store.files.len() <= seal_pos as usize {
            store.files.resize(seal_pos as usize + 1, Vec::new());
            store.syncs.resize(seal_pos as usize + 1, false);
        }
        store.files[seal_pos as usize] = padded;

        // File 0 must carry the CORRECT identity: this is the same tape, so
        // the identity check legitimately passes. That is the whole point —
        // only the seal marker can distinguish "crashed mid-write" from
        // "already finished", and this test would pass vacuously if the
        // identity check fired instead.
        let thunk = layout::generate_id_thunk_v2(&layout::IdThunkV2Params {
            label: &label,
            uuid: &uuid,
            media_type: "LTO-6",
            tapectl_version: "0.1.0-test",
            nominal_capacity: 1,
            mam_capacity: 1,
            total_files: seal_pos as i32 + 1,
            mam_manufacturer: "",
            mam_serial: "",
            mam_length: 0,
            mam_loads: 0,
            created_at: "2026-07-22T20:09:00Z",
            cartridge_identity_source: None,
        });
        let mut thunk_padded = thunk.into_bytes();
        thunk_padded.resize(BS as usize, 0);
        store.files[0] = thunk_padded;

        match interrupted
            .resume_checking(&f.conn, &f.keys, &mut store, || false)
            .expect("resume must not hard-error on a matching already-sealed tape")
        {
            ResumeOutcome::Confirming(_) => {}
            ResumeOutcome::Quarantined(q) => panic!(
                "ADR-0012's 2026-09-21 amendment: a sealed tape whose identity AND seal \
                 position match this session's own Layout must re-enter confirm, not \
                 quarantine. Got Quarantined: {:?}",
                q.reason
            ),
            ResumeOutcome::Ready(_) => panic!("expected Confirming, got Ready"),
            ResumeOutcome::Interrupted(_) => panic!("expected Confirming, got Interrupted"),
            ResumeOutcome::Aborted(a) => {
                panic!("expected Confirming, got Aborted: {}", a.reason)
            }
        };

        // Neither `status` nor `observed_condition` is touched by taking the
        // `Confirming` branch — nothing was learned yet (confirm has not run
        // again), so nothing is asserted about the medium.
        let (status, condition): (String, String) = f
            .conn
            .query_row(
                "SELECT status, observed_condition FROM volumes WHERE id = ?1",
                params![f.volume_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            status, "active",
            "resume's Confirming arm must never touch status"
        );
        assert_eq!(
            condition, "ok",
            "resume's Confirming arm must not write observed_condition — nothing was \
             re-verified yet"
        );

        // `writes` rows are untouched by this arm too (still whatever the
        // earlier interrupted execute left them at) — only a completed
        // `confirm()` call moves them again.
        let write_status: String = f
            .conn
            .query_row(
                "SELECT status FROM writes WHERE volume_id = ?1",
                params![f.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(write_status, "interrupted");
    }

    /// The necessary twin of the test above: a tape that merely LOOKS
    /// sealed — `check_tape_contact` reports the exact same
    /// `ContactOutcome::AlreadySealed` shape (issue #208: its seal probe
    /// runs "whether or not the identity matched") — but is a genuinely
    /// FOREIGN, unrelated tape must still quarantine. Proves condition 1
    /// (identity match) is load-bearing on the `resume_checking` path, not
    /// just on `check_tape_contact`'s own unit tests.
    #[test]
    fn resume_quarantines_a_foreign_tape_that_happens_to_be_sealed() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);

        let seal_pos = f
            .built
            .layout
            .entries
            .iter()
            .find(|e| matches!(e.kind, ZoneKind::SealMarker))
            .expect("layout has a seal marker")
            .position as u32;

        let validated = f.built.into_validated(&f.keys, &mut store).unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let interrupted = match planned
            .execute_checking(&f.conn, &mut store, || true)
            .unwrap()
        {
            ExecuteOutcome::Interrupted(i) => i,
            _ => panic!("expected Interrupted"),
        };

        // A DIFFERENT volume's id thunk and a real seal marker at ITS OWN
        // self-reported position — which happens to equal this session's
        // seal position too, so a position-only check would wrongly let
        // this through. Only the identity check (condition 1) catches it.
        let thunk = layout::generate_id_thunk_v2(&layout::IdThunkV2Params {
            label: "WRONGVOL",
            uuid: "00000000-0000-0000-0000-000000000000",
            media_type: "LTO-6",
            tapectl_version: "0.1.0-test",
            nominal_capacity: 1,
            mam_capacity: 1,
            total_files: seal_pos as i32 + 1,
            mam_manufacturer: "",
            mam_serial: "",
            mam_length: 0,
            mam_loads: 0,
            created_at: "2026-07-22T20:09:00Z",
            cartridge_identity_source: None,
        });
        let mut thunk_padded = thunk.into_bytes();
        thunk_padded.resize(BS as usize, 0);
        if store.files.is_empty() {
            store.files.push(thunk_padded);
            store.syncs.push(false);
        } else {
            store.files[0] = thunk_padded;
        }

        let seal_bytes = layout::generate_seal_marker("WRONGVOL", 1, "deadbeef", &[]).into_bytes();
        let mut seal_padded = seal_bytes;
        seal_padded.resize(BS as usize, 0);
        if store.files.len() <= seal_pos as usize {
            store.files.resize(seal_pos as usize + 1, Vec::new());
            store.syncs.resize(seal_pos as usize + 1, false);
        }
        store.files[seal_pos as usize] = seal_padded;

        let quarantined = match interrupted
            .resume_checking(&f.conn, &f.keys, &mut store, || false)
            .expect("resume must not hard-error on a foreign sealed tape")
        {
            ResumeOutcome::Quarantined(q) => q,
            other => {
                let kind = match other {
                    ResumeOutcome::Ready(_) => "Ready",
                    ResumeOutcome::Interrupted(_) => "Interrupted",
                    ResumeOutcome::Aborted(_) => "Aborted",
                    ResumeOutcome::Confirming(_) => "Confirming",
                    ResumeOutcome::Quarantined(_) => unreachable!(),
                };
                panic!(
                    "expected Quarantined — a foreign sealed tape must never re-enter confirm, \
                     got {kind}"
                )
            }
        };
        match quarantined.reason {
            QuarantineReason::AlreadySealed { seal_position } => {
                assert_eq!(seal_position, seal_pos);
            }
            other => panic!("expected AlreadySealed, got {other:?}"),
        }

        let (status, condition): (String, String) = f
            .conn
            .query_row(
                "SELECT status, observed_condition FROM volumes WHERE id = ?1",
                params![f.volume_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            status, "active",
            "the resume writer must never touch status"
        );
        assert_eq!(condition, "quarantined");
    }

    /// Condition 2 alone failing: File 0's identity matches (condition 1
    /// holds) and its OWN self-reported seal pointer parses as a real seal
    /// marker (condition 3 holds in isolation), but that position DISAGREES
    /// with this session's own Layout. `resume_reconfirm_eligible` must
    /// refuse — a tape whose own map disagrees with what we planned is not
    /// "exactly where we left it".
    #[test]
    fn resume_quarantines_when_the_tapes_own_seal_pointer_disagrees_with_our_layout() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);

        let label = f.built.layout.label.clone();
        let uuid = f.built.layout.volume_uuid.clone();
        let our_seal_pos = f
            .built
            .layout
            .entries
            .iter()
            .find(|e| matches!(e.kind, ZoneKind::SealMarker))
            .expect("layout has a seal marker")
            .position as u32;
        // A position that genuinely disagrees with our own Layout.
        let foreign_seal_pos = our_seal_pos + 1;

        let validated = f.built.into_validated(&f.keys, &mut store).unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let interrupted = match planned
            .execute_checking(&f.conn, &mut store, || true)
            .unwrap()
        {
            ExecuteOutcome::Interrupted(i) => i,
            _ => panic!("expected Interrupted"),
        };

        // File 0 claims (correctly, for ITSELF) that its own seal marker is
        // at `foreign_seal_pos`, one past where OUR Layout puts it.
        let thunk = layout::generate_id_thunk_v2(&layout::IdThunkV2Params {
            label: &label,
            uuid: &uuid,
            media_type: "LTO-6",
            tapectl_version: "0.1.0-test",
            nominal_capacity: 1,
            mam_capacity: 1,
            total_files: foreign_seal_pos as i32 + 1,
            mam_manufacturer: "",
            mam_serial: "",
            mam_length: 0,
            mam_loads: 0,
            created_at: "2026-07-22T20:09:00Z",
            cartridge_identity_source: None,
        });
        let mut thunk_padded = thunk.into_bytes();
        thunk_padded.resize(BS as usize, 0);
        if store.files.is_empty() {
            store.files.push(thunk_padded);
            store.syncs.push(false);
        } else {
            store.files[0] = thunk_padded;
        }

        // A real, parseable seal marker genuinely sits at `foreign_seal_pos`
        // — File 0 is telling the truth about ITSELF, just not about our plan.
        let seal_bytes = layout::generate_seal_marker(&label, 1, "deadbeef", &[]).into_bytes();
        let mut seal_padded = seal_bytes;
        seal_padded.resize(BS as usize, 0);
        if store.files.len() <= foreign_seal_pos as usize {
            store
                .files
                .resize(foreign_seal_pos as usize + 1, Vec::new());
            store.syncs.resize(foreign_seal_pos as usize + 1, false);
        }
        store.files[foreign_seal_pos as usize] = seal_padded;

        let quarantined = match interrupted
            .resume_checking(&f.conn, &f.keys, &mut store, || false)
            .expect("resume must not hard-error")
        {
            ResumeOutcome::Quarantined(q) => q,
            ResumeOutcome::Confirming(_) => panic!(
                "expected Quarantined — the tape's own seal pointer disagrees with our \
                 Layout, so this must NOT be treated as exactly where we left off"
            ),
            ResumeOutcome::Ready(_) => panic!("expected Quarantined, got Ready"),
            ResumeOutcome::Interrupted(_) => panic!("expected Quarantined, got Interrupted"),
            ResumeOutcome::Aborted(a) => panic!("expected Quarantined, got Aborted: {}", a.reason),
        };
        match quarantined.reason {
            QuarantineReason::AlreadySealed { seal_position } => {
                assert_eq!(seal_position, foreign_seal_pos);
            }
            other => panic!("expected AlreadySealed, got {other:?}"),
        }
    }

    /// Condition 3 in isolation: this is the test that proves
    /// `resume_reconfirm_eligible` may never trust the position
    /// [`check_tape_contact`] already decided — it must re-derive it from
    /// File 0 itself. File 0's `[volume]` identity matches (condition 1
    /// holds) and its `[layout]` section is UNPARSEABLE (so the tape's own
    /// pointer cannot be read at all — condition 3 fails by construction),
    /// yet a real seal marker genuinely sits at exactly the position OUR
    /// Layout expects (condition 2 would look satisfied to anyone trusting
    /// `check_tape_contact`'s return value, since its caller-guess fallback
    /// finds it too — `check_tape_contact`'s own comment explains that
    /// fallback is safe for ITS contract, not for this one). A
    /// `resume_reconfirm_eligible` that trusted that returned position
    /// instead of re-reading File 0 would wrongly say eligible here.
    #[test]
    fn resume_quarantines_when_the_tapes_own_layout_section_is_unparseable() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);

        let label = f.built.layout.label.clone();
        let uuid = f.built.layout.volume_uuid.clone();
        let seal_pos = f
            .built
            .layout
            .entries
            .iter()
            .find(|e| matches!(e.kind, ZoneKind::SealMarker))
            .expect("layout has a seal marker")
            .position as u32;

        let validated = f.built.into_validated(&f.keys, &mut store).unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let interrupted = match planned
            .execute_checking(&f.conn, &mut store, || true)
            .unwrap()
        {
            ExecuteOutcome::Interrupted(i) => i,
            _ => panic!("expected Interrupted"),
        };

        // A genuine, correctly-shaped thunk (identity matches, and its
        // `[layout] seal_marker` value legitimately equals `seal_pos`) —
        // then mangle ONLY that one value into something that fails to
        // parse as the required integer, breaking `[layout]` alone. `[volume]`
        // is untouched, so the identity check still legitimately passes.
        let thunk = layout::generate_id_thunk_v2(&layout::IdThunkV2Params {
            label: &label,
            uuid: &uuid,
            media_type: "LTO-6",
            tapectl_version: "0.1.0-test",
            nominal_capacity: 1,
            mam_capacity: 1,
            total_files: seal_pos as i32 + 1,
            mam_manufacturer: "",
            mam_serial: "",
            mam_length: 0,
            mam_loads: 0,
            created_at: "2026-07-22T20:09:00Z",
            cartridge_identity_source: None,
        });
        let needle = format!("seal_marker = {seal_pos}\n");
        assert!(
            thunk.contains(&needle),
            "fixture assumption: the generator must emit `{needle:?}` verbatim"
        );
        let mangled = thunk.replacen(&needle, "seal_marker = \"oops\"\n", 1);
        let mut thunk_padded = mangled.into_bytes();
        thunk_padded.resize(BS as usize, 0);
        if store.files.is_empty() {
            store.files.push(thunk_padded);
            store.syncs.push(false);
        } else {
            store.files[0] = thunk_padded;
        }

        // Sanity: `[layout]` really is unparseable now, and `[volume]`
        // really does still parse — otherwise this test would not be
        // exercising what it claims to.
        let text_check = String::from_utf8_lossy(&store.files[0]);
        assert!(format::parse_id_thunk_layout_pointers(&text_check).is_err());
        assert!(format::parse_id_thunk_identity(&text_check).is_ok());

        // A real seal marker genuinely sits at OUR Layout's own seal
        // position — the caller-guess fallback inside `check_tape_contact`
        // will find it, since File 0's own pointer could not be read.
        let seal_bytes = layout::generate_seal_marker(&label, 1, "deadbeef", &[]).into_bytes();
        let mut seal_padded = seal_bytes;
        seal_padded.resize(BS as usize, 0);
        if store.files.len() <= seal_pos as usize {
            store.files.resize(seal_pos as usize + 1, Vec::new());
            store.syncs.resize(seal_pos as usize + 1, false);
        }
        store.files[seal_pos as usize] = seal_padded;

        // Confirm the premise: `check_tape_contact` itself, called exactly
        // as `resume_checking` calls it, reports `AlreadySealed` at OUR
        // position via its caller-guess fallback — proving this scenario
        // really does reach the trap `resume_reconfirm_eligible` must not
        // fall into.
        match check_tape_contact(&mut store, &label, &uuid, Some(seal_pos)) {
            ContactOutcome::AlreadySealed { seal_position } => {
                assert_eq!(seal_position, seal_pos)
            }
            other => panic!(
                "test premise broken: expected check_tape_contact to report AlreadySealed \
                 via its caller-guess fallback, got {other:?}"
            ),
        }

        let quarantined = match interrupted
            .resume_checking(&f.conn, &f.keys, &mut store, || false)
            .expect("resume must not hard-error")
        {
            ResumeOutcome::Quarantined(q) => q,
            ResumeOutcome::Confirming(_) => panic!(
                "expected Quarantined — the tape's own [layout] pointer could not be read, \
                 so eligibility must never be inferred from check_tape_contact's caller-guess \
                 fallback"
            ),
            ResumeOutcome::Ready(_) => panic!("expected Quarantined, got Ready"),
            ResumeOutcome::Interrupted(_) => panic!("expected Quarantined, got Interrupted"),
            ResumeOutcome::Aborted(a) => panic!("expected Quarantined, got Aborted: {}", a.reason),
        };
        match quarantined.reason {
            QuarantineReason::AlreadySealed { seal_position } => {
                assert_eq!(seal_position, seal_pos);
            }
            other => panic!("expected AlreadySealed, got {other:?}"),
        }
    }

    #[test]
    fn resume_quarantines_on_id_thunk_identity_mismatch() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);

        let validated = f.built.into_validated(&f.keys, &mut store).unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let interrupted = match planned
            .execute_checking(&f.conn, &mut store, || true)
            .unwrap()
        {
            ExecuteOutcome::Interrupted(i) => i,
            _ => panic!("expected Interrupted"),
        };

        // Overwrite whatever landed at position 0 with a DIFFERENT volume's
        // id thunk — as if this tape actually belongs to a different,
        // unrelated session (e.g. the wrong cartridge got loaded).
        let wrong_params = layout::IdThunkV2Params {
            label: "WRONGVOL",
            uuid: "00000000-0000-0000-0000-000000000000",
            media_type: "LTO-6",
            tapectl_version: "0.1.0-test",
            nominal_capacity: 1,
            mam_capacity: 1,
            total_files: 1,
            mam_manufacturer: "",
            mam_serial: "",
            mam_length: 0,
            mam_loads: 0,
            created_at: "2026-07-22T20:09:00Z",
            cartridge_identity_source: None,
        };
        let wrong_bytes = layout::generate_id_thunk_v2(&wrong_params).into_bytes();
        let mut padded = wrong_bytes;
        padded.resize(BS as usize, 0);
        if store.files.is_empty() {
            store.files.push(padded);
            store.syncs.push(false);
        } else {
            store.files[0] = padded;
        }

        let quarantined = match interrupted
            .resume_checking(&f.conn, &f.keys, &mut store, || false)
            .expect("resume_checking should not hard-error on a divergent tape")
        {
            ResumeOutcome::Quarantined(q) => q,
            other => panic!(
                "expected Quarantined on an id-thunk identity mismatch, got a different outcome \
                 ({} entries recorded)",
                match other {
                    ResumeOutcome::Ready(_) => "Ready",
                    ResumeOutcome::Interrupted(_) => "Interrupted",
                    ResumeOutcome::Aborted(_) => "Aborted",
                    ResumeOutcome::Confirming(_) => "Confirming",
                    ResumeOutcome::Quarantined(_) => unreachable!(),
                }
            ),
        };
        assert_eq!(quarantined.volume_id, f.volume_id);
        match quarantined.reason {
            QuarantineReason::IdentityMismatch {
                expected_label,
                found,
                ..
            } => {
                assert_eq!(expected_label, "SESSTEST");
                assert_eq!(found.unwrap().label, "WRONGVOL");
            }
            other => panic!("expected IdentityMismatch, got {other:?}"),
        }

        let (volume_status, volume_condition): (String, String) = f
            .conn
            .query_row(
                "SELECT status, observed_condition FROM volumes WHERE id = ?1",
                params![f.volume_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        // ADR-0012's 2026-09-17 amendment (issue #242): the identity check's
        // finding is observed, not chosen -- it moves `observed_condition`
        // only, leaving `status` exactly as the fixture set it ('active').
        assert_eq!(
            volume_status, "active",
            "the resume writer must never touch status"
        );
        assert_eq!(volume_condition, "quarantined");

        let write_status: String = f
            .conn
            .query_row(
                "SELECT status FROM writes WHERE volume_id = ?1",
                params![f.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(write_status, "aborted");
    }

    // --- bonus: confirm's failure branch (never hit by behaviors 1-4) -----

    /// The four TDD-mandated behaviors never reach `confirm()`'s own fail
    /// branch: the happy-path test reaches `confirm()` only to pass, and the
    /// mismatch/ENOSPC/interrupt tests (plus
    /// `resume_quarantines_on_id_thunk_identity_mismatch`) all abort or
    /// quarantine before `confirm()` is ever called. None of them exercise
    /// what happens when a clean `execute`+`seal` is followed by tape bytes
    /// rotting (or a readback error) strictly between seal and confirm —
    /// caught only by the §5 chain walk itself, since that corruption
    /// happens after both L1 (validate) and L2 (execute's inline re-hash)
    /// have already passed. Added after the fact (like the two resume bonus
    /// tests above) because it covers already-implemented `confirm()` logic
    /// rather than driving new logic into existence — not a TDD red/green
    /// cycle.
    #[test]
    fn confirm_failure_quarantines_volume_and_aborts_writes_without_touching_staging() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);

        let validated = f.built.into_validated(&f.keys, &mut store).unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let ready = match planned.execute(&f.conn, &mut store).unwrap() {
            ExecuteOutcome::Ready(r) => r,
            _ => panic!("expected Ready on a happy-path MemStore run"),
        };
        let sealed_pending = ready.seal(&mut store).expect("seal should succeed");

        // Tape rots strictly AFTER seal(), so L1 (validate) and L2 (execute's
        // inline re-hash) never see it — only the §5 chain walk inside
        // confirm() can catch this. Flip a byte at offset 0 of a slice entry;
        // both fixture slices are >= 24 bytes, so offset 0 is always inside
        // the true (unpadded) region MemStore hashes.
        let slice_position = sealed_pending
            .built
            .layout
            .entries
            .iter()
            .position(|e| matches!(e.kind, ZoneKind::Slice { .. }))
            .expect("fixture layout always has at least one slice entry");
        store.files[slice_position][0] ^= 0xFF;

        let outcome = sealed_pending
            .confirm(&f.conn, &mut store, Tier::Integrity)
            .expect("confirm should not hard-error on a chain-walk mismatch");

        let quarantined = match outcome {
            ConfirmOutcome::Quarantined(q) => q,
            ConfirmOutcome::Sealed(_) => {
                panic!("expected Quarantined on a post-seal content mismatch, got Sealed")
            }
            // Negative control (ADR-0012's 2026-09-18 amendment, issues
            // #260/#267): a flipped content byte produces
            // `MismatchKind::ContentHashMismatch`, which
            // `proves_medium_bad()` rules TRUE — this path must stay
            // `Quarantined`, unchanged by the `Inconclusive` amendment. If
            // this ever fires, the amendment has been widened too far.
            ConfirmOutcome::Inconclusive(inc) => panic!(
                "a genuine content-hash mismatch must still quarantine (medium-proving), not \
                 go Inconclusive: {:?}",
                inc.evidence.mismatches
            ),
        };
        assert_eq!(quarantined.volume_id, f.volume_id);
        match quarantined.reason {
            QuarantineReason::ConfirmFailed(evidence) => {
                assert!(
                    !evidence.mismatches.is_empty(),
                    "expected at least one chain-walk mismatch"
                );
                assert_eq!(evidence.mismatches[0].position, slice_position as u32);
            }
            other => panic!("expected ConfirmFailed, got {other:?}"),
        }

        let (volume_status, volume_condition): (String, String) = f
            .conn
            .query_row(
                "SELECT status, observed_condition FROM volumes WHERE id = ?1",
                params![f.volume_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        // ADR-0012's 2026-09-17 amendment (issue #242): the chain walk's
        // finding is observed, not chosen -- `status` is never touched by a
        // failed confirm (only the passing branch's transaction flips it to
        // 'sealed'), so it stays exactly what the fixture set ('active').
        assert_eq!(
            volume_status, "active",
            "the confirm writer must never touch status"
        );
        assert_eq!(volume_condition, "quarantined");

        // ALL writes rows abort, not just the one touching the corrupted
        // slice — confirm operates per-volume, not per-write.
        let write_statuses: Vec<String> = f
            .conn
            .prepare("SELECT status FROM writes WHERE volume_id = ?1")
            .unwrap()
            .query_map(params![f.volume_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(!write_statuses.is_empty());
        assert!(write_statuses.iter().all(|s| s == "aborted"));

        // Confirm never touches staging either way (it only ever reads the
        // store, never the staging directory) — the corrupted slice's
        // ORIGINAL staged input file is untouched and still on disk.
        let staged_path = &f.units[0].slices[0].staging_path;
        assert!(
            staged_path.exists(),
            "confirm() must never delete/modify staging inputs"
        );

        // The audit feedback loop closes even on failure: a 'failed'
        // verification_sessions row is recorded, not just silently dropped.
        let vs_outcome: String = f
            .conn
            .query_row(
                "SELECT outcome FROM verification_sessions WHERE volume_id = ?1",
                params![f.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(vs_outcome, "failed");
    }

    /// Negative control 1 (ADR-0012's 2026-09-18 amendment, issues
    /// #260/#267): a confirm whose ONLY mismatch is a short read
    /// (`MismatchKind::ContentUnreadable` — drive/transport evidence, not a
    /// medium-proving one) must go `Inconclusive`, not `Quarantined`: no
    /// seal, no `observed_condition` write, and `writes` left `interrupted`
    /// (never `aborted`) so `tapectl volume resume` can re-enter confirm.
    ///
    /// Contrast with `confirm_failure_quarantines_volume_and_aborts_writes_without_touching_staging`
    /// just above (negative control 2): THAT test flips a byte in place (same
    /// length), producing `ContentHashMismatch` (medium-proving) — unchanged,
    /// still `Quarantined`. THIS test truncates the on-tape bytes to fewer
    /// than the front index's declared size, producing a genuine short read.
    #[test]
    fn confirm_with_only_a_short_read_goes_inconclusive_not_quarantined() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);

        let validated = f.built.into_validated(&f.keys, &mut store).unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let ready = match planned.execute(&f.conn, &mut store).unwrap() {
            ExecuteOutcome::Ready(r) => r,
            _ => panic!("expected Ready on a happy-path MemStore run"),
        };
        let sealed_pending = ready.seal(&mut store).expect("seal should succeed");

        // Truncate one slice's ON-TAPE bytes to fewer than its declared
        // (true) size — a short read, never a hash disagreement (no hash is
        // ever computed: `chain_walk` checks `want_size > n_read` BEFORE
        // hashing). "first staged slice bytes" is 25 bytes; 5 is short.
        let slice_position = sealed_pending
            .built
            .layout
            .entries
            .iter()
            .position(|e| matches!(e.kind, ZoneKind::Slice { .. }))
            .expect("fixture layout always has at least one slice entry");
        store.files[slice_position].truncate(5);

        let outcome = sealed_pending
            .confirm(&f.conn, &mut store, Tier::Integrity)
            .expect("confirm should not hard-error on a short read");

        let inconclusive = match outcome {
            ConfirmOutcome::Inconclusive(inc) => inc,
            ConfirmOutcome::Sealed(_) => {
                panic!("expected Inconclusive on a short read, got Sealed")
            }
            ConfirmOutcome::Quarantined(q) => panic!(
                "a short read (drive/transport evidence only) must NOT quarantine — that is \
                 the exact false-quarantine this amendment exists to prevent. Got \
                 Quarantined: {:?}",
                match q.reason {
                    QuarantineReason::ConfirmFailed(e) => format!("{:?}", e.mismatches),
                    other => format!("{other:?}"),
                }
            ),
        };
        assert_eq!(inconclusive.volume_id, f.volume_id);
        assert_eq!(
            inconclusive.evidence.mismatches.len(),
            1,
            "exactly the one injected short read: {:?}",
            inconclusive.evidence.mismatches
        );
        assert_eq!(
            inconclusive.evidence.mismatches[0].kind,
            crate::store::MismatchKind::ContentUnreadable
        );
        assert!(!inconclusive.evidence.proves_medium_bad());

        // Does NOT seal: `volumes.status` is untouched (never 'sealed'), and
        // `observed_condition` is untouched too (never 'quarantined') —
        // nothing was learned about the medium.
        let (volume_status, volume_condition): (String, String) = f
            .conn
            .query_row(
                "SELECT status, observed_condition FROM volumes WHERE id = ?1",
                params![f.volume_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            volume_status, "active",
            "Inconclusive must never touch status"
        );
        assert_eq!(
            volume_condition, "ok",
            "Inconclusive must never write observed_condition — nothing was learned"
        );

        // `writes` rows move to 'interrupted', NOT 'aborted' — this is what
        // makes the session reachable again by `InterruptedSession::rehydrate`
        // (`WHERE status = 'interrupted'`), i.e. by `tapectl volume resume`.
        let write_statuses: Vec<String> = f
            .conn
            .prepare("SELECT status FROM writes WHERE volume_id = ?1")
            .unwrap()
            .query_map(params![f.volume_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(!write_statuses.is_empty());
        assert!(
            write_statuses.iter().all(|s| s == "interrupted"),
            "expected every writes row 'interrupted', got {write_statuses:?}"
        );

        // The session is genuinely reachable by resume: rehydrate must find
        // it (this is the empirical proof, not just a status string).
        let rehydrated = InterruptedSession::rehydrate(&f.conn, f.volume_id)
            .expect("rehydrate should not error")
            .expect("an Inconclusive confirm must leave the session resumable");
        // Rehydration reads the frozen Layout back from `session_dir`, so
        // this alone proves resume has something real to work with.
        assert_eq!(rehydrated.layout().label, "SESSTEST");

        // The audit feedback loop closes even here: a 'failed'
        // verification_sessions row is recorded (same convention `volume
        // verify` uses for a non-medium-proving mismatch).
        let vs_outcome: String = f
            .conn
            .query_row(
                "SELECT outcome FROM verification_sessions WHERE volume_id = ?1",
                params![f.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(vs_outcome, "failed");
    }

    /// The full lifecycle ADR-0012's 2026-09-21 amendment describes,
    /// end-to-end with no status-string shortcuts: execute -> seal ->
    /// confirm (Inconclusive, via a transient short read) -> the transient
    /// cause resolves -> rehydrate -> resume_checking (re-enters confirm
    /// instead of quarantining) -> confirm (Sealed). This is the empirical
    /// proof that "resume re-confirms" actually delivers a sealed copy, not
    /// just an outcome variant.
    #[test]
    fn inconclusive_confirm_is_resumed_to_a_genuine_seal() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);

        let validated = f.built.into_validated(&f.keys, &mut store).unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let ready = match planned.execute(&f.conn, &mut store).unwrap() {
            ExecuteOutcome::Ready(r) => r,
            _ => panic!("expected Ready on a happy-path MemStore run"),
        };
        let sealed_pending = ready.seal(&mut store).expect("seal should succeed");

        let slice_position = sealed_pending
            .built
            .layout
            .entries
            .iter()
            .position(|e| matches!(e.kind, ZoneKind::Slice { .. }))
            .expect("fixture layout always has at least one slice entry");
        // The transient fault: truncate the on-tape bytes below the front
        // index's declared size. Saved so it can be "healed" below —
        // standing in for a drive that reads fine on the next attempt.
        let original_bytes = store.files[slice_position].clone();
        store.files[slice_position].truncate(5);

        match sealed_pending
            .confirm(&f.conn, &mut store, Tier::Integrity)
            .expect("confirm should not hard-error on a short read")
        {
            ConfirmOutcome::Inconclusive(_) => {}
            ConfirmOutcome::Sealed(_) => panic!("expected Inconclusive first, got Sealed"),
            ConfirmOutcome::Quarantined(q) => {
                panic!(
                    "expected Inconclusive first, got Quarantined: {:?}",
                    q.reason
                )
            }
        }

        // The transient fault resolves — the tape reads fine now (a real
        // drive that had one bad pass, cleaned and retried).
        store.files[slice_position] = original_bytes;

        // Cross the same seam a real `tapectl volume resume` crosses:
        // rehydrate from durable state alone, never the in-memory session.
        let rehydrated = InterruptedSession::rehydrate(&f.conn, f.volume_id)
            .unwrap()
            .expect("an Inconclusive confirm must leave the session resumable");

        let sealed_pending_again = match rehydrated
            .resume_checking(&f.conn, &f.keys, &mut store, || false)
            .expect("resume must not hard-error")
        {
            ResumeOutcome::Confirming(pending) => pending,
            ResumeOutcome::Quarantined(q) => panic!(
                "expected Confirming — this is exactly the tape this session sealed, got \
                 Quarantined: {:?}",
                q.reason
            ),
            ResumeOutcome::Ready(_) => panic!("expected Confirming, got Ready"),
            ResumeOutcome::Interrupted(_) => panic!("expected Confirming, got Interrupted"),
            ResumeOutcome::Aborted(a) => panic!("expected Confirming, got Aborted: {}", a.reason),
        };

        match sealed_pending_again
            .confirm(&f.conn, &mut store, Tier::Integrity)
            .expect("confirm should not error on the healed tape")
        {
            ConfirmOutcome::Sealed(s) => assert_eq!(s.label, "SESSTEST"),
            ConfirmOutcome::Quarantined(q) => {
                panic!(
                    "expected Sealed on the healed tape, got Quarantined: {:?}",
                    q.reason
                )
            }
            ConfirmOutcome::Inconclusive(inc) => panic!(
                "expected Sealed on the healed tape, got Inconclusive again: {:?}",
                inc.evidence.mismatches
            ),
        }

        let (volume_status, write_status): (String, String) = f
            .conn
            .query_row(
                "SELECT v.status, w.status FROM volumes v JOIN writes w ON w.volume_id = v.id \
                 WHERE v.id = ?1",
                params![f.volume_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(volume_status, "sealed");
        assert_eq!(write_status, "completed");
    }

    // --- issue #277: the seal is RECORDED, not inferred -------------------
    //
    // ADR-0012's 2026-09-21 correction to its own preceding amendment. The
    // preceding amendment's `resume_reconfirm_eligible` machinery (tested
    // above) routes every one of its three conditions through
    // `seal_marker_parses_at`, which cannot distinguish "no marker here"
    // from "a read error at this position" — deliberately, for the
    // fresh-write path. On resume that conflation is fatal: an `Inconclusive`
    // confirm's own `MismatchKind::SealUnreadable` is exactly a read error at
    // the seal position, so the very seal a session wrote is the one seal
    // resume cannot see. These tests exercise the fixture detail none of
    // commit 8b061b7's five resume tests cover: a seal position that
    // outright ERRORS on read, not one that parses to something else or
    // merely disagrees.

    /// A `Store` wrapping a `MemStore` that panics if `execute` or
    /// `reposition_for_resume` is called. Once `volumes.sealed_at` is
    /// recorded, `resume_checking` must never attempt either — doing so
    /// would be a write-path operation (a second seal marker, or a
    /// reposition ahead of one) against a cartridge ADR-0003 says is already
    /// immutable. `capacity` and `read_file` delegate normally: revalidation
    /// and the contact/seal probes both need to read.
    struct NoWriteStore(MemStore);

    impl Store for NoWriteStore {
        fn capacity(&mut self) -> Result<crate::store::CapacityReport> {
            self.0.capacity()
        }
        fn execute(&mut self, _src: &mut dyn std::io::Read, _len: u64, _sync: bool) -> Result<u64> {
            panic!(
                "issue #277: resume must never write once `volumes.sealed_at` is recorded — \
                 this would write a second seal marker to a physically sealed cartridge"
            );
        }
        fn read_file(&mut self, position: u32, sink: &mut dyn Write) -> Result<u64> {
            self.0.read_file(position, sink)
        }
        fn reposition_for_resume(&mut self, _file_index: u32) -> Result<()> {
            panic!(
                "issue #277: resume must never reposition once `volumes.sealed_at` is \
                 recorded — that is a write-path operation against an already-sealed tape"
            );
        }
    }

    /// THE headline regression for issue #277. Builds exactly the scenario
    /// ADR-0012's 2026-09-21 correction names: execute finishes, `seal()`
    /// succeeds (a real seal marker goes on tape, and — simulating what
    /// `write::finish_session` does at its one call site — `sealed_at` is
    /// recorded), then the seal position becomes UNREADABLE (an outright
    /// read error, not a parseable-but-different marker and not a short
    /// read — both of those are already covered elsewhere). Before Change 3,
    /// `check_tape_contact` cannot tell "sealed but this read failed" apart
    /// from "never sealed" (`seal_marker_parses_at` returns `false` for a
    /// read error exactly as it does for "no marker here"), reports
    /// `Matches`, and resume falls through the (until now empty)
    /// `Blank | Matches` arm straight toward `reposition_for_resume` and a
    /// second `seal()` — a write against a physically sealed cartridge,
    /// ADR-0003 bypassed. `NoWriteStore` turns that fallthrough into an
    /// immediate panic instead of a silent pass, so red is unambiguous.
    #[test]
    fn resume_never_rewrites_a_sealed_tape_when_the_seal_position_is_unreadable() {
        let f = make_fixture();
        let mut inner = MemStore::new(BS as usize);

        let validated = f.built.into_validated(&f.keys, &mut inner).unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let ready = match planned
            .execute_checking(&f.conn, &mut inner, || false)
            .unwrap()
        {
            ExecuteOutcome::Ready(r) => r,
            _ => panic!("expected Ready on a happy-path MemStore run"),
        };

        let seal_pos = ready
            .built
            .layout
            .entries
            .iter()
            .find(|e| matches!(e.kind, ZoneKind::SealMarker))
            .expect("layout has a seal marker")
            .position as u32;

        let sealed_pending = ready.seal(&mut inner).expect("seal should succeed");

        // Change 2's effect, simulated exactly where it happens in
        // production (`write::finish_session`, immediately after
        // `ready.seal` returns `Ok` — `ReadyToSeal::seal` itself takes no
        // `Connection`).
        f.conn
            .execute(
                "UPDATE volumes SET sealed_at = datetime('now') WHERE id = ?1",
                params![f.volume_id],
            )
            .unwrap();

        // Simulate the crash: a real `Inconclusive` confirm (or a crash
        // mid-confirm swept by `recover_orphaned_sessions`) leaves `writes`
        // 'interrupted' — the only status `InterruptedSession::rehydrate`
        // will adopt.
        mark_writes(&f.conn, &sealed_pending.write_ids, "interrupted").unwrap();

        // The fixture's load-bearing premise, checked rather than assumed:
        // the seal marker really is the last file on the (simulated) tape,
        // so popping it below removes exactly the seal and nothing else.
        assert_eq!(
            inner.files.len() as u32 - 1,
            seal_pos,
            "fixture premise: the seal marker must be the last entry"
        );
        // The defect's trigger: the seal position becomes unreadable.
        inner.files.pop();

        let interrupted = InterruptedSession::rehydrate(&f.conn, f.volume_id)
            .unwrap()
            .expect("an Inconclusive-confirm-shaped interruption must be resumable");

        let mut store = NoWriteStore(inner);
        let outcome = interrupted
            .resume_checking(&f.conn, &f.keys, &mut store, || false)
            .expect("resume must not hard-error, and must not panic via NoWriteStore either");

        match outcome {
            ResumeOutcome::Confirming(_) => {}
            ResumeOutcome::Ready(_) => panic!(
                "issue #277: an unreadable seal position must not be inferred as \"never \
                 sealed\" when `sealed_at` says otherwise — expected Confirming, got Ready \
                 (the shape that leads straight to a second seal() call in finish_session)"
            ),
            ResumeOutcome::Quarantined(q) => panic!(
                "expected Confirming (sealed_at recorded), got Quarantined: {:?}",
                q.reason
            ),
            ResumeOutcome::Interrupted(_) => panic!("expected Confirming, got Interrupted"),
            ResumeOutcome::Aborted(a) => panic!("expected Confirming, got Aborted: {}", a.reason),
        }

        // No side effect at all: this arm returns before touching `writes`
        // again — only a completed confirm() moves it from here.
        let write_status: String = f
            .conn
            .query_row(
                "SELECT status FROM writes WHERE volume_id = ?1",
                params![f.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            write_status, "interrupted",
            "the Confirming arm must not touch `writes` — only a completed confirm() does"
        );
    }

    /// The `Blank`-half twin of the test above (issue #277): File 0 is ALSO
    /// unreadable, so `check_tape_contact` reports `Blank` instead of
    /// `Matches` — the ADR names this shape explicitly ("`Blank` reaches the
    /// same line when File 0 is transiently unreadable"). A recorded
    /// `sealed_at` must win here exactly as it does for `Matches`: resume
    /// must re-enter confirm, never reposition or seal.
    #[test]
    fn resume_never_rewrites_a_sealed_tape_when_file_zero_is_also_unreadable() {
        let f = make_fixture();
        let mut inner = MemStore::new(BS as usize);

        let validated = f.built.into_validated(&f.keys, &mut inner).unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let ready = match planned
            .execute_checking(&f.conn, &mut inner, || false)
            .unwrap()
        {
            ExecuteOutcome::Ready(r) => r,
            _ => panic!("expected Ready on a happy-path MemStore run"),
        };
        let sealed_pending = ready.seal(&mut inner).expect("seal should succeed");

        f.conn
            .execute(
                "UPDATE volumes SET sealed_at = datetime('now') WHERE id = ?1",
                params![f.volume_id],
            )
            .unwrap();
        mark_writes(&f.conn, &sealed_pending.write_ids, "interrupted").unwrap();

        // Both File 0 (position 0) and the seal marker (the last file) are
        // now unreadable — `check_tape_contact` falls through its
        // `file_zero_present` branch entirely and reports `Blank`.
        inner.files.clear();

        let interrupted = InterruptedSession::rehydrate(&f.conn, f.volume_id)
            .unwrap()
            .expect("an Inconclusive-confirm-shaped interruption must be resumable");

        let mut store = NoWriteStore(inner);
        match interrupted
            .resume_checking(&f.conn, &f.keys, &mut store, || false)
            .expect("resume must not hard-error, and must not panic via NoWriteStore either")
        {
            ResumeOutcome::Confirming(_) => {}
            ResumeOutcome::Ready(_) => panic!(
                "issue #277: a Blank contact reading must not override a recorded seal — \
                 expected Confirming, got Ready"
            ),
            ResumeOutcome::Quarantined(q) => panic!(
                "expected Confirming (sealed_at recorded), got Quarantined: {:?}",
                q.reason
            ),
            ResumeOutcome::Interrupted(_) => panic!("expected Confirming, got Interrupted"),
            ResumeOutcome::Aborted(a) => panic!("expected Confirming, got Aborted: {}", a.reason),
        }
    }

    /// The necessary contrast to the two tests above (issue #277): when
    /// `sealed_at` is genuinely NULL — `seal()` never ran, only `execute`
    /// finished before the crash — resume must still reach `Ready`, so the
    /// caller (`write::finish_session`) goes on to call `seal()` for the
    /// first time. A fix that made resume unconditionally re-confirm instead
    /// of sealing would silently strand every session interrupted between
    /// execute finishing and seal ever running — this is the case a naive
    /// fix breaks.
    #[test]
    fn resume_still_seals_when_the_seal_was_never_recorded() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);

        let validated = f.built.into_validated(&f.keys, &mut store).unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let ready = match planned
            .execute_checking(&f.conn, &mut store, || false)
            .unwrap()
        {
            ExecuteOutcome::Ready(r) => r,
            _ => panic!("expected Ready on a happy-path MemStore run"),
        };

        // The checked premise: execute really did finish (every slice is
        // recorded 'written'), so this is genuinely case (a) — "execute
        // finished, seal() never ran" — not an early interruption.
        let total_slices = ready
            .built
            .layout
            .entries
            .iter()
            .filter(|e| matches!(e.kind, ZoneKind::Slice { .. }))
            .count();
        assert_eq!(
            count_written_slices(&f.conn, &ready.write_ids).unwrap(),
            total_slices,
            "fixture premise: execute must have written every slice"
        );

        // `seal()` is deliberately never called. Simulate the crash: a real
        // crash here leaves `writes` 'in_progress', and
        // `recover_orphaned_sessions` sweeps it to 'interrupted' before any
        // command holds a `Connection`.
        mark_writes(&f.conn, &ready.write_ids, "interrupted").unwrap();

        let sealed_at: Option<String> = f
            .conn
            .query_row(
                "SELECT sealed_at FROM volumes WHERE id = ?1",
                params![f.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            sealed_at.is_none(),
            "seal() never ran in this test; sealed_at must be NULL"
        );

        let interrupted = InterruptedSession::rehydrate(&f.conn, f.volume_id)
            .unwrap()
            .expect("resumable");

        let ready_again = match interrupted
            .resume_checking(&f.conn, &f.keys, &mut store, || false)
            .expect("resume must not hard-error")
        {
            ResumeOutcome::Ready(r) => r,
            ResumeOutcome::Confirming(_) => panic!(
                "issue #277: sealed_at is NULL — the seal is still owed. Expected Ready, got \
                 Confirming"
            ),
            ResumeOutcome::Quarantined(q) => {
                panic!("expected Ready, got Quarantined: {:?}", q.reason)
            }
            ResumeOutcome::Interrupted(_) => panic!("expected Ready, got Interrupted"),
            ResumeOutcome::Aborted(a) => panic!("expected Ready, got Aborted: {}", a.reason),
        };

        let sealed_pending = ready_again.seal(&mut store).expect("seal should succeed");
        match sealed_pending
            .confirm(&f.conn, &mut store, Tier::Integrity)
            .expect("confirm should not error")
        {
            ConfirmOutcome::Sealed(s) => assert_eq!(s.label, "SESSTEST"),
            ConfirmOutcome::Quarantined(q) => {
                panic!("expected Sealed, got Quarantined: {:?}", q.reason)
            }
            ConfirmOutcome::Inconclusive(inc) => panic!(
                "expected Sealed, got Inconclusive: {:?}",
                inc.evidence.mismatches
            ),
        }

        let volume_status: String = f
            .conn
            .query_row(
                "SELECT status FROM volumes WHERE id = ?1",
                params![f.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(volume_status, "sealed");
    }

    /// `sealed_at` is write-once and must NEVER be cleared by any confirm
    /// outcome (issue #277) — in particular not by an `Inconclusive`
    /// confirm's own `mark_writes(..., "interrupted")`, which is precisely
    /// the transition that, without this column, erases the distinction
    /// between "never sealed" and "sealed, but this readback failed".
    #[test]
    fn sealed_at_survives_an_inconclusive_confirms_mark_writes() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);

        let validated = f.built.into_validated(&f.keys, &mut store).unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let ready = match planned.execute(&f.conn, &mut store).unwrap() {
            ExecuteOutcome::Ready(r) => r,
            _ => panic!("expected Ready on a happy-path MemStore run"),
        };
        let sealed_pending = ready.seal(&mut store).expect("seal should succeed");

        // Change 2's effect, simulated at the point `write::finish_session`
        // records it.
        f.conn
            .execute(
                "UPDATE volumes SET sealed_at = datetime('now') WHERE id = ?1",
                params![f.volume_id],
            )
            .unwrap();
        let sealed_at_before: String = f
            .conn
            .query_row(
                "SELECT sealed_at FROM volumes WHERE id = ?1",
                params![f.volume_id],
                |r| r.get(0),
            )
            .unwrap();

        // A short read -> Inconclusive, same fixture as
        // `confirm_with_only_a_short_read_goes_inconclusive_not_quarantined`.
        let slice_position = sealed_pending
            .built
            .layout
            .entries
            .iter()
            .position(|e| matches!(e.kind, ZoneKind::Slice { .. }))
            .expect("fixture layout always has at least one slice entry");
        store.files[slice_position].truncate(5);

        match sealed_pending
            .confirm(&f.conn, &mut store, Tier::Integrity)
            .expect("confirm should not hard-error on a short read")
        {
            ConfirmOutcome::Inconclusive(_) => {}
            ConfirmOutcome::Sealed(_) => panic!("expected Inconclusive, got Sealed"),
            ConfirmOutcome::Quarantined(q) => {
                panic!("expected Inconclusive, got Quarantined: {:?}", q.reason)
            }
        }

        let write_status: String = f
            .conn
            .query_row(
                "SELECT status FROM writes WHERE volume_id = ?1",
                params![f.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            write_status, "interrupted",
            "confirm's Inconclusive branch moves writes to 'interrupted' via mark_writes"
        );

        let sealed_at_after: String = f
            .conn
            .query_row(
                "SELECT sealed_at FROM volumes WHERE id = ?1",
                params![f.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            sealed_at_before, sealed_at_after,
            "mark_writes(..., 'interrupted') must never clear sealed_at — that survival is \
             the entire point of recording it"
        );
    }

    // --- check_tape_contact: the shared File-0 + seal-marker check (#27) ---
    //
    // Pure: no `Connection`, no DB access — MemStore only. These pin the
    // exact algorithm `resume_checking` (above) and the fresh-write path
    // (`write::check_fresh_write_contact`) both run, so the two callers
    // cannot silently diverge. `resume_quarantines_on_id_thunk_identity_mismatch`
    // and `resume_refuses_a_tape_that_already_carries_a_seal_marker` above
    // already exercise this same code through `resume_checking`; these tests
    // exercise it directly, including shapes (`seal_position = None`) that
    // only the fresh-write caller ever produces.

    const CONTACT_LABEL: &str = "SESSTEST";
    const CONTACT_UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn contact_id_thunk_bytes(label: &str, uuid: &str, total_files: i32) -> Vec<u8> {
        let thunk = layout::generate_id_thunk_v2(&layout::IdThunkV2Params {
            label,
            uuid,
            media_type: "LTO-6",
            tapectl_version: "0.1.0-test",
            nominal_capacity: 1,
            mam_capacity: 1,
            total_files,
            mam_manufacturer: "",
            mam_serial: "",
            mam_length: 0,
            mam_loads: 0,
            created_at: "2026-07-28T00:00:00Z",
            cartridge_identity_source: None,
        });
        let mut padded = thunk.into_bytes();
        padded.resize(BS as usize, 0);
        padded
    }

    /// Writes `bytes` into `store.files[position]`, growing the (contiguous)
    /// vec with empty placeholder files if needed — mirrors how the session
    /// bonus tests above stage a MemStore by hand.
    fn put_file(store: &mut MemStore, position: usize, bytes: Vec<u8>) {
        if store.files.len() <= position {
            store.files.resize(position + 1, Vec::new());
            store.syncs.resize(position + 1, false);
        }
        store.files[position] = bytes;
    }

    #[test]
    fn check_tape_contact_blank_tape_with_a_known_seal_position_proceeds() {
        // volume_write's shape on a genuinely blank cartridge: a real Layout
        // (so `seal_position` is `Some`), but nothing at all on tape yet.
        let mut store = MemStore::new(BS as usize);
        let outcome = check_tape_contact(&mut store, CONTACT_LABEL, CONTACT_UUID, Some(5));
        assert_eq!(outcome, ContactOutcome::Blank);
    }

    #[test]
    fn check_tape_contact_blank_tape_with_no_seal_position_proceeds() {
        // volume_init's shape: no Layout yet at all, so nothing to check
        // beyond File 0 (see `check_tape_contact`'s doc comment on why this
        // costs nothing in practice).
        let mut store = MemStore::new(BS as usize);
        let outcome = check_tape_contact(&mut store, CONTACT_LABEL, CONTACT_UUID, None);
        assert_eq!(outcome, ContactOutcome::Blank);
    }

    #[test]
    fn check_tape_contact_matches_when_identity_agrees_and_seal_position_is_unwritten() {
        // THE critical fresh-write happy path (not just "blank tape"):
        // `volume_init` already stamped this exact tape's File 0 with this
        // exact label+uuid; `volume_write` runs next, on the SAME physical
        // tape, with a real Layout (`seal_position = Some`) but nothing
        // written at that position yet. This must proceed — refusing here
        // would break ordinary, correct usage, not just the wrong-tape case.
        let mut store = MemStore::new(BS as usize);
        put_file(
            &mut store,
            0,
            contact_id_thunk_bytes(CONTACT_LABEL, CONTACT_UUID, 8),
        );
        let outcome = check_tape_contact(&mut store, CONTACT_LABEL, CONTACT_UUID, Some(7));
        assert_eq!(outcome, ContactOutcome::Matches);
    }

    #[test]
    fn check_tape_contact_matches_when_identity_agrees_and_no_seal_position_given() {
        let mut store = MemStore::new(BS as usize);
        put_file(
            &mut store,
            0,
            contact_id_thunk_bytes(CONTACT_LABEL, CONTACT_UUID, 8),
        );
        let outcome = check_tape_contact(&mut store, CONTACT_LABEL, CONTACT_UUID, None);
        assert_eq!(outcome, ContactOutcome::Matches);
    }

    #[test]
    fn check_tape_contact_identity_mismatch_reports_found_identity() {
        let mut store = MemStore::new(BS as usize);
        put_file(
            &mut store,
            0,
            contact_id_thunk_bytes("WRONGVOL", "00000000-0000-0000-0000-000000000000", 8),
        );
        let outcome = check_tape_contact(&mut store, CONTACT_LABEL, CONTACT_UUID, None);
        match outcome {
            ContactOutcome::IdentityMismatch { found } => {
                let found = found.expect("File 0 parsed, just disagreed");
                assert_eq!(found.label, "WRONGVOL");
                assert_eq!(found.uuid, "00000000-0000-0000-0000-000000000000");
            }
            other => panic!("expected IdentityMismatch, got {other:?}"),
        }
    }

    #[test]
    fn check_tape_contact_identity_mismatch_when_file_zero_is_unparseable_garbage() {
        // Readable but not valid TOML at all: ambiguous, so treated as a
        // mismatch (never overwrite on ambiguity) — `found` is `None`, not a
        // crash.
        let mut store = MemStore::new(BS as usize);
        put_file(&mut store, 0, b"not a tapectl id thunk at all".to_vec());
        let outcome = check_tape_contact(&mut store, CONTACT_LABEL, CONTACT_UUID, None);
        match outcome {
            ContactOutcome::IdentityMismatch { found } => assert!(found.is_none()),
            other => panic!("expected IdentityMismatch{{found: None}}, got {other:?}"),
        }
    }

    #[test]
    fn check_tape_contact_already_sealed_when_seal_position_parses() {
        // Identity matches (it genuinely is "this" volume) but the tape is
        // already sealed — mirrors `resume_refuses_a_tape_that_already_carries_a_seal_marker`,
        // proving the same outcome is reachable directly, not only through
        // `resume_checking`.
        let mut store = MemStore::new(BS as usize);
        put_file(
            &mut store,
            0,
            contact_id_thunk_bytes(CONTACT_LABEL, CONTACT_UUID, 6),
        );
        let seal_bytes =
            layout::generate_seal_marker(CONTACT_LABEL, 6, "deadbeef", &[]).into_bytes();
        let mut seal_padded = seal_bytes;
        seal_padded.resize(BS as usize, 0);
        put_file(&mut store, 5, seal_padded);

        let outcome = check_tape_contact(&mut store, CONTACT_LABEL, CONTACT_UUID, Some(5));
        match outcome {
            ContactOutcome::AlreadySealed { seal_position } => assert_eq!(seal_position, 5),
            other => panic!("expected AlreadySealed, got {other:?}"),
        }
    }

    #[test]
    /// Issue #208, the pre-production review's highest-severity finding.
    ///
    /// The sibling test below covers a foreign SEALED tape. This one is the
    /// case that was NOT covered and was the actual hole: identity MATCHES
    /// -- the tape really does hold this volume -- and the tape is sealed,
    /// but the caller's `seal_position` points somewhere else.
    ///
    /// That is `volume_write`'s ordinary shape. Its layout is built from
    /// whatever is staged now, so its seal-marker entry lands where the NEW
    /// content would end, not where the existing seal sits. Reachable
    /// whenever the catalog still believes the volume is writable while the
    /// tape is already sealed -- a DB restored from a backup predating the
    /// write, or a row `catalog rebuild` deliberately left alone (#158).
    /// Before the fix this returned `Matches` and a sealed tape was
    /// overwritten, with no `--force` required and nothing to refuse it:
    /// ADR-0003 says sealed volumes are immutable and no flag reaches that.
    ///
    /// Deliberately passes caller `seal_position` 7 against a real seal at
    /// 5, so the test fails if the fix ever regresses to trusting the
    /// caller's guess.
    fn check_tape_contact_already_sealed_when_identity_matches_and_the_caller_probes_elsewhere() {
        let mut store = MemStore::new(BS as usize);
        put_file(
            &mut store,
            0,
            contact_id_thunk_bytes(CONTACT_LABEL, CONTACT_UUID, 6),
        );
        let seal_bytes =
            layout::generate_seal_marker(CONTACT_LABEL, 6, "deadbeef", &[]).into_bytes();
        let mut seal_padded = seal_bytes;
        seal_padded.resize(BS as usize, 0);
        put_file(&mut store, 5, seal_padded);

        let outcome = check_tape_contact(&mut store, CONTACT_LABEL, CONTACT_UUID, Some(7));
        match outcome {
            ContactOutcome::AlreadySealed { seal_position } => assert_eq!(seal_position, 5),
            other => panic!(
                "a sealed tape whose identity MATCHES must refuse (ADR-0003); \
                 the caller's own seal_position must not be the only probe. got {other:?}"
            ),
        }
    }

    #[test]
    /// The same hole with `seal_position: None` -- no caller probe at all.
    fn check_tape_contact_already_sealed_when_identity_matches_and_no_caller_probe() {
        let mut store = MemStore::new(BS as usize);
        put_file(
            &mut store,
            0,
            contact_id_thunk_bytes(CONTACT_LABEL, CONTACT_UUID, 6),
        );
        let seal_bytes =
            layout::generate_seal_marker(CONTACT_LABEL, 6, "deadbeef", &[]).into_bytes();
        let mut seal_padded = seal_bytes;
        seal_padded.resize(BS as usize, 0);
        put_file(&mut store, 5, seal_padded);

        match check_tape_contact(&mut store, CONTACT_LABEL, CONTACT_UUID, None) {
            ContactOutcome::AlreadySealed { seal_position } => assert_eq!(seal_position, 5),
            other => panic!("expected AlreadySealed with no caller probe, got {other:?}"),
        }
    }

    #[test]
    fn check_tape_contact_already_sealed_wins_over_identity_mismatch_on_a_foreign_tape() {
        // THE dangerous scenario the issue exists for: a wrong cartridge
        // loaded, holding a DIFFERENT, already-sealed volume. The caller's
        // `seal_position` argument is a position in the CALLER's own
        // not-yet-written layout (or, for `volume_init`, `None` — no layout
        // at all yet) — it has no relationship to where a FOREIGN tape's
        // real seal marker actually sits. If `check_tape_contact` only ever
        // consulted the caller's own `seal_position`, a foreign sealed tape
        // would present as a plain `IdentityMismatch` — which `--force`
        // (write.rs's `decide_fresh_write_contact`) is allowed to override —
        // silently permitting exactly the destructive overwrite ADR-0003
        // forbids. The fix: when identity mismatches, ALSO probe the
        // foreign tape's OWN self-reported `[layout].seal_marker` position
        // (`format::parse_id_thunk_layout_pointers`) before concluding
        // IdentityMismatch. Deliberately passes a caller `seal_position`
        // (7) that does NOT match the foreign tape's real seal position (5)
        // — proving the self-report, not the caller's guess, is what finds
        // it.
        let mut store = MemStore::new(BS as usize);
        put_file(
            &mut store,
            0,
            contact_id_thunk_bytes("WRONGVOL", "00000000-0000-0000-0000-000000000000", 6),
        );
        let seal_bytes = layout::generate_seal_marker("WRONGVOL", 6, "deadbeef", &[]).into_bytes();
        let mut seal_padded = seal_bytes;
        seal_padded.resize(BS as usize, 0);
        put_file(&mut store, 5, seal_padded);

        let outcome = check_tape_contact(&mut store, CONTACT_LABEL, CONTACT_UUID, Some(7));
        match outcome {
            ContactOutcome::AlreadySealed { seal_position } => assert_eq!(seal_position, 5),
            other => panic!(
                "expected AlreadySealed (foreign tape's own self-reported position), got {other:?}"
            ),
        }
    }

    #[test]
    fn check_tape_contact_blank_when_seal_position_given_but_nothing_recorded_there() {
        // An unsealed tape has nothing readable at the (hypothetical) seal
        // position — a read failure there is the expected, safe case, not
        // an error (mirrors `resume_checking`'s own comment on this exact
        // point).
        let mut store = MemStore::new(BS as usize);
        let outcome = check_tape_contact(&mut store, CONTACT_LABEL, CONTACT_UUID, Some(5));
        assert_eq!(outcome, ContactOutcome::Blank);
    }

    // --- issue #113: the beginning-of-tape park hook ---------------------

    /// The property the mhvtl gate's BOT arm depends on: with a park marker
    /// supplied, `execute` stops at exactly the beginning-of-tape state —
    /// session planned, **nothing written** — announces itself by creating
    /// the marker, and comes back `Interrupted`.
    ///
    /// Deterministic with no sleeps and no threads: `is_interrupted` is
    /// `|| marker.exists()`, and the hook itself creates that file, so the
    /// park loop observes the interrupt on its very first poll. That is the
    /// same ordering the gate produces with a real signal, minus the race.
    #[test]
    fn park_hook_stops_at_bot_with_nothing_written_and_reports_interrupted() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);
        let tmp = TempDir::new().unwrap();
        let marker = tmp.path().join("parked");

        let validated = f.built.into_validated(&f.keys, &mut store).unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();

        let marker_for_check = marker.clone();
        let outcome = run_entries(
            &f.conn,
            &mut store,
            planned.built,
            planned.volume_id,
            planned.write_ids,
            planned.slice_write_id,
            0,
            &mut || marker_for_check.exists(),
            Some(marker.to_string_lossy().into_owned()),
        )
        .expect("a parked execute must not error");

        assert!(
            matches!(outcome, ExecuteOutcome::Interrupted(_)),
            "the park hook must yield Interrupted, not Ready"
        );
        assert!(
            marker.exists(),
            "the hook must announce readiness by creating the marker — the gate \
             waits on this file instead of guessing at timing"
        );

        // The whole point of the BOT arm: zero entries confirmed. If the hook
        // let even one through, the arm becomes a duplicate of resume_midwrite.
        let written: i64 = f
            .conn
            .query_row(
                "SELECT COUNT(*) FROM write_positions WHERE status = 'written'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            written, 0,
            "parking after plan() must confirm NOTHING; got {written} written positions"
        );
    }

    /// The production-safety guarantee: with no marker supplied — which is
    /// what `park_marker_from_env()` returns whenever the variable is unset —
    /// not one branch of the hook is taken and the write proceeds normally.
    /// This is the assertion that keeps a test hook on the production write
    /// path defensible.
    #[test]
    fn without_a_park_marker_the_hook_is_entirely_inert() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);

        let validated = f.built.into_validated(&f.keys, &mut store).unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();

        let outcome = run_entries(
            &f.conn,
            &mut store,
            planned.built,
            planned.volume_id,
            planned.write_ids,
            planned.slice_write_id,
            0,
            &mut || false,
            None,
        )
        .expect("execute should not error");

        assert!(
            matches!(outcome, ExecuteOutcome::Ready(_)),
            "with no park marker the session must run to Ready exactly as before"
        );
    }

    /// `park_marker_from_env` is the single boundary where the variable is
    /// read (never inside `run_entries`), so that no test has to mutate
    /// process-global environment state to exercise the hook — doing so
    /// would leak into every other test running in parallel in this binary,
    /// which is the very nondeterminism #113 removes.
    #[test]
    fn park_marker_is_absent_by_default() {
        assert!(
            park_marker_from_env().is_none(),
            "TAPECTL_TEST_PAUSE_AFTER_PLAN must not be set in the test environment; \
             if this fails, something is exporting it and every write is parking"
        );
    }
}
