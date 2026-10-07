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
//!     tri-layer L1: staged slices exist at their recorded size (full hash
//!     only under `--prewrite-hash`, ADR-0012 2026-09-30 later amendment);
//!     size/hash-check frozen zones;
//!     capacity = Σ block-padded + enospc_buffer vs oracle; keys + escrow.
//! ValidatedLayout::plan(conn)             -> PlannedSession
//!     writes rows 'planned' + write_positions 'pending' (slices only — schema).
//! PlannedSession::execute(store)          -> Executing… -> ReadyToSeal
//!     rewind; per entry: SIGINT check (between entries only; mid-file kill =
//!     crash = startup sweep); stream from disk through the write pipeline
//!     (reader thread -> hasher thread -> store.execute(src, len, sync) on
//!     this thread, #390); slice entries update their cursor row
//!     ('written' + sha256_on_volume). Inline-hash mismatch (tri-layer L2,
//!     judged before the file's last block reaches the store) or
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
use std::io::Cursor;
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use crate::db::busy::{self, BusyPolicy};
use crate::error::{Result, TapectlError};
use crate::pipeline::{self, BufferPool, Verdict};
use crate::store::{Checkpoint, Checkpoints, ConfirmPlan, Evidence, ReadOrder, Store, Tier};

use super::build::{BuildUnit, BuiltLayout};
use super::format;
use super::layout;
use super::layout_model::{
    ContentSource, KeyAvailability, LayoutEntry, LayoutError, SliceCheck, ZoneKind,
};

// ── ValidatedLayout ──

/// A [`BuiltLayout`] that has passed `docs/design/layout-session.md`'s
/// validation predicate. Produced only by [`BuiltLayout::into_validated`];
/// its only operation is [`Self::plan`].
pub struct ValidatedLayout {
    built: BuiltLayout,
}

impl BuiltLayout {
    /// `BuiltLayout -> ValidatedLayout` (§9's `validate(keys, oracle)`).
    /// Runs the existing, already-tested `BuiltLayout::validate(keys, slice_check)`
    /// (tri-layer L1 — staged slices size-checked, full-hashed only under
    /// [`SliceCheck::FullHash`] — materialized-zone size/hash checks, key
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
        slice_check: SliceCheck,
        store: &mut dyn Store,
    ) -> std::result::Result<ValidatedLayout, Vec<LayoutError>> {
        let mut errs = Vec::new();
        if let Err(mut e) = self.validate(keys, slice_check) {
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
    ///
    /// **One transaction (issue #401).** Every row lands, or none does: a
    /// failure partway used to leave a `planned` row behind, which then
    /// blocked the next `volume write` as an "unresolved write session".
    ///
    /// **A new attempt replaces an abandoned one (issue #401).** A volume
    /// whose earlier session ended `aborted` or `failed` before its seal was
    /// recorded (an end of tape, a tri-layer L2 mismatch, a plan cleared by
    /// `volume abort`) is written again from the beginning — that is what
    /// the operator documentation has always told the operator to do. The
    /// old attempt's rows used to make that impossible: `writes` is
    /// `UNIQUE(stage_set_id, volume_id)`, so the retry's own INSERT failed
    /// on them. They describe a recording the new session is about to
    /// overwrite from BOT and that no command can ever resume (an unsealed
    /// aborted session is never adopted, ADR-0012 2026-09-23), so they are
    /// deleted here, in this same transaction, with a `write_session_superseded`
    /// event saying which rows went; the abort itself stays recorded in the
    /// `write_aborted` event it already has. A volume's `writes` rows thus
    /// keep describing ONE session, which `rehydrate`, `adopt_aborted`,
    /// `volume abort` and `staging clean`'s guard all assume. Their frozen
    /// session directories are removed after the commit
    /// ([`supersede_abandoned_attempts`]).
    pub fn plan(
        self,
        conn: &Connection,
        volume_id: i64,
        units: &[BuildUnit],
    ) -> Result<PlannedSession> {
        // The frozen staging directory is recorded here, and only here
        // (migration 006, issue #25): plan is the sole `writes`-row writer,
        // and after a process restart this path is the ONLY way back to the
        // materialized zones a resume must re-hash rather than regenerate.
        let session_dir = self.built.session_dir.to_string_lossy().to_string();
        let entries = &self.built.layout.entries;
        let (write_ids, slice_write_id, superseded_dirs) =
            busy::retry(BusyPolicy::DEFAULT, "a write session's plan", || {
                let tx = busy::immediate_tx(conn)?;
                let superseded_dirs = supersede_abandoned_attempts(&tx, volume_id)?;
                let (write_ids, slice_write_id) =
                    insert_plan_rows(&tx, volume_id, units, entries, &session_dir)?;
                tx.commit()?;
                Ok((write_ids, slice_write_id, superseded_dirs))
            })?;

        // After the commit, never before: a directory removed for rows a
        // rollback then kept would strand them (the DB-then-filesystem order
        // `staging::clean` documents). Best-effort — a directory that cannot
        // be removed is an orphan `staging clean --force` reclaims, never a
        // reason to fail a write whose rows are already committed.
        for dir in superseded_dirs {
            if dir == session_dir {
                continue;
            }
            let path = Path::new(&dir);
            if path.is_dir() {
                if let Err(e) = std::fs::remove_dir_all(path) {
                    tracing::warn!(
                        dir = %path.display(),
                        error = %e,
                        "could not remove a superseded write session's staging directory"
                    );
                }
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

/// `plan`'s rows: one `writes` row per unit and one `write_positions` row
/// per slice entry, on `tx`. Returns the `(write_id, snapshot_id)` pairs and
/// the `stage_slice_id -> write_id` map the session carries.
#[allow(clippy::type_complexity)]
fn insert_plan_rows(
    tx: &Connection,
    volume_id: i64,
    units: &[BuildUnit],
    entries: &[LayoutEntry],
    session_dir: &str,
) -> Result<(Vec<(i64, i64)>, HashMap<i64, i64>)> {
    let mut write_ids = Vec::with_capacity(units.len());
    let mut slice_write_id = HashMap::new();
    for u in units {
        tx.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, session_dir)
             VALUES (?1, ?2, ?3, 'planned', ?4)",
            params![u.stage_set_id, u.snapshot_id, volume_id, session_dir],
        )?;
        let write_id = tx.last_insert_rowid();
        write_ids.push((write_id, u.snapshot_id));
        for slice in &u.slices {
            slice_write_id.insert(slice.slice_id, write_id);
        }
    }

    for entry in entries {
        if let ZoneKind::Slice { stage_slice_id } = entry.kind {
            let write_id = *slice_write_id.get(&stage_slice_id).ok_or_else(|| {
                TapectlError::Other(format!(
                    "plan: no unit in `units` owns staged slice {stage_slice_id} \
                     (Layout position {})",
                    entry.position
                ))
            })?;
            tx.execute(
                "INSERT INTO write_positions (write_id, stage_slice_id, position, status)
                 VALUES (?1, ?2, ?3, 'pending')",
                params![write_id, stage_slice_id, entry.position.to_string()],
            )?;
        }
    }
    Ok((write_ids, slice_write_id))
}

/// Delete the `writes` rows (and their `write_positions`) of every earlier
/// session on `volume_id` that ended `aborted` or `failed` before its seal
/// was recorded, inside `plan`'s transaction (issue #401). Returns the
/// session directories those rows named that no remaining row names, for
/// `plan` to remove once the transaction commits.
///
/// Does nothing when the volume's seal is recorded: such an `aborted`
/// session is the one `volume resume` may adopt for re-confirmation
/// (ADR-0012's 2026-09-23 amendment, #280), and a sealed volume is never
/// planned again anyway — `volume write` refuses it first.
///
/// Refuses, changing nothing, if a `verification_results` row points at
/// one of those positions: the rows are then evidence about the medium, not
/// leftovers of an abandoned attempt, and only a person should decide what
/// becomes of them.
fn supersede_abandoned_attempts(tx: &Connection, volume_id: i64) -> Result<Vec<String>> {
    let (label, sealed): (String, bool) = tx.query_row(
        "SELECT label, sealed_at IS NOT NULL FROM volumes WHERE id = ?1",
        params![volume_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if sealed {
        return Ok(Vec::new());
    }
    let rows: Vec<(i64, String, Option<String>)> = tx
        .prepare(
            "SELECT id, status, session_dir FROM writes
             WHERE volume_id = ?1 AND status IN ('aborted', 'failed')
             ORDER BY id",
        )?
        .query_map(params![volume_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let ids = rows
        .iter()
        .map(|(id, _, _)| id.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    // `ids` is integers this function just read, never input.
    let evidence: i64 = tx.query_row(
        &format!(
            "SELECT COUNT(*) FROM verification_results vr
             JOIN write_positions wp ON wp.id = vr.write_position_id
             WHERE wp.write_id IN ({ids})"
        ),
        [],
        |r| r.get(0),
    )?;
    if evidence > 0 {
        return Err(TapectlError::Other(format!(
            "volume \"{label}\": cannot start a new write session — its earlier, abandoned \
             session (writes row(s) {ids}) has {evidence} verification result(s) recorded \
             against its positions, so those rows are evidence about this cartridge and \
             tapectl will not delete them to make room. Nothing was changed. Inspect them \
             (`verification_results`, `write_positions` with write_id in {ids}) before \
             writing this volume again."
        )));
    }

    tx.execute(
        &format!("DELETE FROM write_positions WHERE write_id IN ({ids})"),
        [],
    )?;
    tx.execute(&format!("DELETE FROM writes WHERE id IN ({ids})"), [])?;

    let mut dirs: Vec<String> = rows.iter().filter_map(|(_, _, d)| d.clone()).collect();
    dirs.sort();
    dirs.dedup();
    let statuses = rows
        .iter()
        .map(|(id, status, _)| format!("{id} {status}"))
        .collect::<Vec<_>>()
        .join(", ");
    crate::db::events::log_event(
        tx,
        "volume",
        volume_id,
        Some(&label),
        "write_session_superseded",
        None,
        None,
        None,
        Some(&format!(
            "a new write session replaces an earlier unsealed attempt that ended aborted or \
             failed; its writes rows ({statuses}) and their write_positions were removed; \
             session dir(s): {}",
            if dirs.is_empty() {
                "none recorded".to_string()
            } else {
                dirs.join(", ")
            }
        )),
        None,
    )?;

    let mut orphaned = Vec::new();
    for dir in dirs {
        let still_named: i64 = tx.query_row(
            "SELECT COUNT(*) FROM writes WHERE session_dir = ?1",
            params![dir],
            |r| r.get(0),
        )?;
        if still_named == 0 {
            orphaned.push(dir);
        }
    }
    Ok(orphaned)
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
/// entries — or, since ADR-0012's 2026-09-23 amendment (#280), an `aborted`
/// session with a recorded seal that [`Self::adopt_aborted`] picked up for
/// re-confirmation. Resumable: its only operation is [`Self::resume`].
pub struct InterruptedSession {
    built: BuiltLayout,
    volume_id: i64,
    write_ids: Vec<(i64, i64)>,
    slice_write_id: HashMap<i64, i64>,
    /// True only for a session [`Self::adopt_aborted`] picked up (ADR-0012's
    /// 2026-09-23 amendment, #280). Such a session's seal is recorded, so
    /// `resume_checking` can only re-enter `confirm` or quarantine; this flag
    /// turns any path that would reach the write phase into a hard refusal
    /// rather than trusting that it is unreachable (ADR-0003).
    adopted_from_aborted: bool,
    /// Why execution stopped, when it was not SIGINT: a staged file or the
    /// drive failed mid-write (issue #408). `None` for an interrupt and for
    /// a session rehydrated from the catalog.
    reason: Option<String>,
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
    /// File 0 unreadable AND the medium proved blank: end of data at the
    /// beginning of the tape ([`Store::blank_at_bot`], issue #400). For
    /// resume this is a session crashed before File 0 ever landed; for a
    /// fresh write it is the ordinary shape of a blank cartridge. Either
    /// way: safe to (re)write from BOT.
    Blank,
    /// File 0 could not be read, and nothing proved the medium blank (issue
    /// #400). st fails a read at BOT with the same EIO for a blank tape, a
    /// medium error on a recorded tape — a live sealed volume on a drive
    /// that needs cleaning — and a tape written in another block size, so
    /// the error alone is no consent to write (ADR-0003 fails closed): the
    /// fresh-write path refuses unless `--force`, resume refuses and leaves
    /// the session to be resumed again.
    FileZeroUnreadable { error: String },
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
    /// File 0 was readable but EMPTY: zero bytes before its filemark — a
    /// filemark at BOT (issue #327; `mt rewind; mt weof 1` produces it). It
    /// identifies no volume and is not corrupt, so it must not be reported
    /// as either; but it is not provably blank either (a blank tape's File
    /// 0 read fails, [`Self::Blank`]), so every caller treats it exactly as
    /// it treats [`Self::IdentityMismatch`]: the fresh-write path refuses
    /// without `--force` (ADR-0003 fails closed), resume quarantines.
    EmptyFileZero,
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
    contact_report(store, expected_label, expected_uuid, seal_position).0
}

/// What one contact read off the tape, beside [`check_tape_contact`]'s
/// outcome (issue #403) — so a caller that must re-derive a decision from
/// the tape's own facts ([`resume_reconfirm_eligible`]) does it from these
/// reads instead of rewinding and reading File 0 and the seal again, and
/// `confirm` can start from the seal bytes already in hand.
#[derive(Debug, Default)]
pub(crate) struct ContactFacts {
    /// File 0's identity, when File 0 was read and parsed.
    identity: Option<format::IdThunkIdentity>,
    /// File 0's own `[layout] seal_marker` pointer, when it parsed.
    seal_pointer: Option<i64>,
    /// Every seal position probed: the bytes read there, or `None` if the
    /// read failed. Each position is read at most once per contact.
    seal_reads: HashMap<u32, Option<Vec<u8>>>,
}

impl ContactFacts {
    /// Whether `position` holds a parsing seal marker — reading it only the
    /// first time it is asked about (issue #403: resume used to read the
    /// same seal position up to three times, each a long locate).
    fn probe_seal(&mut self, store: &mut dyn Store, position: u32) -> bool {
        let read = self.seal_reads.entry(position).or_insert_with(|| {
            // Bounded (issue #400): an oversized file is no seal marker.
            match crate::store::read_small(store, position) {
                Ok(crate::store::SmallRead::Bytes(bytes)) => Some(bytes),
                Ok(crate::store::SmallRead::Oversized) | Err(_) => None,
            }
        });
        read.as_deref()
            .is_some_and(|bytes| format::parse_seal_marker(&String::from_utf8_lossy(bytes)).is_ok())
    }

    /// The bytes this contact read at `position`, if it read them.
    fn seal_bytes(&self, position: u32) -> Option<Vec<u8>> {
        self.seal_reads.get(&position).cloned().flatten()
    }
}

/// [`check_tape_contact`], with the facts it read.
pub(crate) fn contact_report(
    store: &mut dyn Store,
    expected_label: &str,
    expected_uuid: &str,
    seal_position: Option<u32>,
) -> (ContactOutcome, ContactFacts) {
    let mut facts = ContactFacts::default();
    // Bounded (issue #400): File 0 is one block by construction. A file
    // there larger than `SMALL_FILE_CAP` is not an ID thunk — and reading it
    // whole could exhaust the host's memory — so it is read no further and
    // treated as the unparseable File 0 it is.
    let file_zero_error = match crate::store::read_small(store, 0) {
        Ok(crate::store::SmallRead::Oversized) => {
            return (ContactOutcome::IdentityMismatch { found: None }, facts)
        }
        Ok(crate::store::SmallRead::Bytes(id_thunk_bytes)) => {
            let text = String::from_utf8_lossy(&id_thunk_bytes);
            let identity = format::parse_id_thunk_identity(&text);
            facts.identity = identity.as_ref().ok().cloned();
            let matches = matches!(
                &identity,
                Ok(id) if id.label == expected_label && id.uuid == expected_uuid
            );

            // THE TAPE'S OWN seal pointer, consulted whether or not the
            // identity matched (issue #208, 2026-09-17 pre-production review).
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
                facts.seal_pointer = Some(i64::from(pointers.seal_marker));
                if pointers.seal_marker >= 0 && facts.probe_seal(store, pointers.seal_marker as u32)
                {
                    let seal_position = pointers.seal_marker as u32;
                    return (ContactOutcome::AlreadySealed { seal_position }, facts);
                }
            }

            if !matches {
                // The sealed case already returned above (issue #208), so a
                // mismatch reaching here is a genuinely unsealed foreign or
                // stale tape. That is what `--force` is allowed to overwrite;
                // issue #27's headline scenario -- a foreign-but-SEALED
                // cartridge presenting as a plain mismatch the flag could
                // defeat -- is closed by the hoisted probe, not here.
                //
                // Issue #327: `Store::read_file` succeeds with zero bytes when
                // File 0 is only a filemark (`TapeStore`: the first read
                // returns 0), and "" fails to parse just as garbage does. The
                // two are different facts and are reported as such; both
                // still refuse.
                if id_thunk_bytes.is_empty() {
                    return (ContactOutcome::EmptyFileZero, facts);
                }
                return (
                    ContactOutcome::IdentityMismatch {
                        found: identity.ok(),
                    },
                    facts,
                );
            }
            None
        }
        Err(e) => Some(e.to_string()),
    };

    // Issue #400: an unreadable File 0 is blank only on positive evidence,
    // asked of the medium itself. Asked before the seal probe below, which
    // stays independent of it: a File-0-unreadable-but-sealed-tail tape is
    // exactly the front/tail damage asymmetry `volume-format-v2.md` §4
    // designs for.
    let blank = file_zero_error.is_some() && store.blank_at_bot().unwrap_or(false);

    // The caller's position — not read again when it is the tape's own
    // pointer, probed just above (issue #403).
    if let Some(seal_pos) = seal_position {
        if facts.probe_seal(store, seal_pos) {
            return (
                ContactOutcome::AlreadySealed {
                    seal_position: seal_pos,
                },
                facts,
            );
        }
    }

    let outcome = match file_zero_error {
        None => ContactOutcome::Matches,
        Some(_) if blank => ContactOutcome::Blank,
        Some(error) => ContactOutcome::FileZeroUnreadable { error },
    };
    (outcome, facts)
}

/// Whether a resume that met [`ContactOutcome::AlreadySealed`] may skip the
/// write phase and re-enter `confirm` directly, instead of quarantining —
/// ADR-0012's 2026-09-21 amendment, "`volume resume` re-confirms a tape that
/// is already sealed" (issues #260/#267).
///
/// All three of the ruling's conditions are re-derived HERE from what the
/// contact READ ([`ContactFacts`]), never from which internal branch of
/// [`contact_report`] produced the `AlreadySealed` outcome — that function
/// reports the exact same shape for a genuinely FOREIGN sealed tape (its seal
/// probe runs "whether or not the identity matched", issue #208) and, even
/// when the identity DOES match, can report a position it verified only via
/// the CALLER's own guess rather than the tape's self-reported pointer:
///
/// 1. File 0's identity (label + uuid) matches `expected_label`/
///    `expected_uuid`.
/// 2. File 0's OWN recorded `[layout] seal_marker` pointer equals
///    `expected_seal_position` (this session's own Layout).
/// 3. The bytes read at that exact position parse as a seal marker.
///
/// Until issue #403 this re-read File 0 and the seal itself — a rewind and a
/// second long locate to the end of the tape, on top of the contact's own —
/// to keep the decision independent of the contact's verdict. The facts
/// keep it just as independent: they are the raw reads, File 0's own
/// pointer is condition 2's only source, and the seal bytes are the ones
/// read at that position in this same contact.
///
/// Any failure — File 0 unreadable, unparseable, a non-matching identity, no
/// recorded pointer, a pointer that disagrees with this session's Layout, or
/// a position that does not parse as a seal marker — returns `false`, and
/// the caller keeps today's behaviour: quarantine, never proceed.
fn resume_reconfirm_eligible(
    facts: &ContactFacts,
    expected_label: &str,
    expected_uuid: &str,
    expected_seal_position: Option<u32>,
) -> bool {
    let Some(expected_seal_position) = expected_seal_position else {
        return false;
    };
    let Some(identity) = &facts.identity else {
        return false;
    };
    if identity.label != expected_label || identity.uuid != expected_uuid {
        return false;
    }
    if facts.seal_pointer != Some(i64::from(expected_seal_position)) {
        return false;
    }
    facts
        .seal_bytes(expected_seal_position)
        .is_some_and(|bytes| format::parse_seal_marker(&String::from_utf8_lossy(&bytes)).is_ok())
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
/// what `ContactFacts::probe_seal` cannot: whether an unreadable seal position
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

// ── a full readback continues an interrupted one (#410) ──

/// An interrupted full readback a new one continues (issue #410): which
/// readback session it was, and what it had read back clean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Continuation {
    /// The `verification_sessions` row being continued.
    pub session_id: i64,
    pub checkpoints: Checkpoints,
}

impl Continuation {
    /// The checkpoints, for [`ConfirmPlan::resuming`].
    pub fn checkpoints(this: Option<&Self>) -> Option<&Checkpoints> {
        this.map(|c| &c.checkpoints)
    }
}

/// What the previous readback of this volume read back clean, when it is
/// one a full readback may continue (issue #410): the volume's latest
/// `verification_sessions` row is a FULL one that never finished —
/// `in_progress` (its process died) or `aborted` (a stopped verify, or the
/// startup sweep's word for a dead one) — with checkpoints all taken
/// against one front index. A readback that finished, passed or failed, is
/// never continued: a failed one is re-read whole, so a drive that was
/// cleaned in between gets to read everything again.
///
/// Nor is one any of whose files was read back at or before the volume's
/// recorded write abort: ADR-0012's 2026-09-23 adoption rule wants a full
/// readback wholly after the abort ([`aborted_adoption`], by `started_at`),
/// and a continuation would otherwise let files read before it stand in a
/// verify that started after it. A skipped file keeps the time it was
/// actually read ([`record_checkpoint`]), so this holds down a chain of
/// interruptions too.
///
/// Shared by the write's confirm and `volume verify` (both record their
/// readbacks in `verification_sessions`), so either continues the other's
/// interrupted readback: the checkpoints are anchored to File 3's bytes,
/// not to the command that took them.
pub(crate) fn interrupted_readback(
    conn: &Connection,
    volume_id: i64,
) -> Result<Option<Continuation>> {
    let latest: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT id, verify_type, outcome FROM verification_sessions
             WHERE volume_id = ?1 ORDER BY id DESC LIMIT 1",
            params![volume_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((session_id, verify_type, outcome)) = latest else {
        return Ok(None);
    };
    if verify_type != Tier::Integrity.verify_type()
        || !matches!(outcome.as_str(), "in_progress" | "aborted")
    {
        return Ok(None);
    }
    let rows: Vec<(u32, String, String, String)> = conn
        .prepare(
            "SELECT position, sha256, front_index_sha256, checked_at FROM readback_checkpoints
             WHERE session_id = ?1",
        )?
        .query_map(params![session_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let Some(front_index_sha256) = rows.first().map(|(_, _, fi, _)| fi.clone()) else {
        return Ok(None);
    };
    if rows.iter().any(|(_, _, fi, _)| *fi != front_index_sha256) {
        return Ok(None);
    }
    if let Some(aborted_at) = recorded_abort_time(conn, volume_id)? {
        if rows.iter().any(|(_, _, _, at)| *at <= aborted_at) {
            return Ok(None);
        }
    }
    Ok(Some(Continuation {
        session_id,
        checkpoints: Checkpoints {
            front_index_sha256,
            passed: rows.into_iter().map(|(p, sha, _, _)| (p, sha)).collect(),
        },
    }))
}

/// Record one file a full readback read back clean, under readback session
/// `session_id` (issue #410), as the walk goes. A file the readback skipped
/// on `continuing`'s word keeps the time it was actually read there, not
/// now: what a checkpoint dates is the read. Best-effort: a row that cannot
/// be written costs one re-read on a later continuation, never the readback
/// itself, so a failure is warned about and swallowed.
pub(crate) fn record_checkpoint(
    conn: &Connection,
    session_id: i64,
    continuing: Option<&Continuation>,
    c: Checkpoint<'_>,
) {
    if let Err(e) = conn.execute(
        "INSERT OR IGNORE INTO readback_checkpoints
             (session_id, position, sha256, front_index_sha256, checked_at)
         VALUES (?1, ?2, ?3, ?4, COALESCE(
             (SELECT checked_at FROM readback_checkpoints
              WHERE session_id = ?5 AND position = ?2 AND sha256 = ?3
                AND front_index_sha256 = ?4),
             datetime('now')))",
        params![
            session_id,
            c.position,
            c.sha256,
            c.front_index_sha256,
            continuing.map(|k| k.session_id),
        ],
    ) {
        tracing::warn!(
            position = c.position,
            error = %e,
            "could not record a readback checkpoint (an interruption would re-read this file)"
        );
    }
}

// ── `volume resume` adopts an aborted, sealed, cleared session (#280) ──

/// Whether `volume resume` may adopt a volume's `aborted` write session —
/// ADR-0012's 2026-09-23 amendment (issue #280, Option 2). The ONE
/// derivation of that answer: [`InterruptedSession::adopt_aborted`] gates on
/// it, `write::volume_resume` names its refusals from it, and `volume
/// verify`'s clean-clear message decides whether to name `volume resume`
/// from it (via [`resume_would_reconfirm`]) — never a second copy (#96).
///
/// The variants other than `Adoptable` are the amendment's conditions, in
/// its order, and the first unmet one is the one reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbortedAdoption {
    /// Every condition holds: resume re-enters `confirm` on this session.
    Adoptable,
    /// `volumes.status` is not `initialized` — not a write target at all,
    /// so resume refuses on status long before this question (ADR-0012).
    NotInitialized { status: String },
    /// Condition 1: `volumes.sealed_at` is NULL. The session was aborted
    /// before its own `seal()` returned, so there is no sealed tape to
    /// re-confirm — and the seal is RECORDED, never inferred (the
    /// 2026-09-21 amendment). Not resumable, ever.
    NotSealed,
    /// Condition 2, first half: `observed_condition` is not `'ok'`.
    /// Resolved by a clean full verify (`tapectl volume verify <label>`).
    ConditionNotOk { condition: String },
    /// Condition 2's timestamp has nothing to be later than: no `events`
    /// row records this volume's session being aborted
    /// (`write_aborted`/`write_quarantined`). Refused rather than inferred.
    NoRecordedAbort,
    /// Condition 2, second half: no `verification_sessions` row with
    /// `verify_type = 'full'` and `outcome = 'passed'` STARTED strictly after
    /// the recorded abort. `observed_condition = 'ok'` alone is not evidence
    /// — a `volume abort`ed session's condition may never have been
    /// quarantined at all.
    NoCleanFullVerifyAfterAbort { aborted_at: String },
}

/// What `volume resume` finds in a volume's `writes` rows, before it
/// touches the tape. The row-level half of resume's admission, shared by
/// `write::volume_resume` and [`resume_would_reconfirm`] so the two cannot
/// disagree about which session resume would pick up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeAdmission {
    /// At least one `interrupted` row: [`InterruptedSession::rehydrate`]
    /// adopts it, exactly as before #280.
    Interrupted,
    /// No `interrupted`, `planned` or `in_progress` row, at least one
    /// `aborted` row, and [`AbortedAdoption`]'s verdict on it.
    Aborted(AbortedAdoption),
    /// Anything else (`planned`/`in_progress` rows, only `completed`/`failed`
    /// rows, or none): nothing resume adopts; `write::nothing_to_resume`
    /// names why.
    Nothing,
}

/// The recorded time this volume's write session was aborted: the latest
/// `events` row for the volume whose action is one of the two every
/// abort-to-`aborted` path logs — `write_aborted` (`write::volume_abort`, in
/// the same transaction as the status flip; `write::finish_session`'s
/// execute-abort arm) or `write_quarantined` ([`record_quarantine`], in the
/// same transaction as the status flip, for confirm's Quarantined arm and
/// resume's own divergence arms — issue #324). `None` when
/// no such row exists; the caller refuses rather than guessing.
fn recorded_abort_time(conn: &Connection, volume_id: i64) -> Result<Option<String>> {
    Ok(conn.query_row(
        "SELECT MAX(timestamp) FROM events
         WHERE entity_type = 'volume' AND entity_id = ?1
           AND action IN ('write_aborted', 'write_quarantined')",
        params![volume_id],
        |r| r.get(0),
    )?)
}

/// ADR-0012's 2026-09-23 amendment, conditions 1 and 2, evaluated from
/// recorded rows only — see [`AbortedAdoption`]. Condition 3 (File 0
/// identity and the seal position agreeing with this session's layout) is
/// the tape's to answer, and `InterruptedSession::resume_checking` keeps
/// answering it exactly as it does for an `interrupted` session.
///
/// This does not look at `writes` rows; [`resume_admission`] decides
/// whether there is an aborted session to ask about at all.
pub fn aborted_adoption(conn: &Connection, volume_id: i64) -> Result<AbortedAdoption> {
    let (status, sealed_at, condition): (String, Option<String>, String) = conn.query_row(
        "SELECT status, sealed_at, observed_condition FROM volumes WHERE id = ?1",
        params![volume_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if status != "initialized" {
        return Ok(AbortedAdoption::NotInitialized { status });
    }
    if sealed_at.is_none() {
        return Ok(AbortedAdoption::NotSealed);
    }
    if condition != "ok" {
        return Ok(AbortedAdoption::ConditionNotOk { condition });
    }
    let Some(aborted_at) = recorded_abort_time(conn, volume_id)? else {
        return Ok(AbortedAdoption::NoRecordedAbort);
    };
    // Strictly later, and by `started_at`: the whole readback must postdate
    // the abort. `datetime('now')` has one-second resolution, so a verify
    // recorded in the same second as the abort is refused — the safe side.
    let cleared_after: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM verification_sessions
                       WHERE volume_id = ?1 AND verify_type = 'full'
                         AND outcome = 'passed' AND started_at > ?2)",
        params![volume_id, aborted_at],
        |r| r.get(0),
    )?;
    if !cleared_after {
        return Ok(AbortedAdoption::NoCleanFullVerifyAfterAbort { aborted_at });
    }
    Ok(AbortedAdoption::Adoptable)
}

/// The row-level admission — see [`ResumeAdmission`].
pub fn resume_admission(conn: &Connection, volume_id: i64) -> Result<ResumeAdmission> {
    let statuses: Vec<String> = conn
        .prepare("SELECT DISTINCT status FROM writes WHERE volume_id = ?1")?
        .query_map(params![volume_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let has = |s: &str| statuses.iter().any(|x| x == s);
    if has("interrupted") {
        return Ok(ResumeAdmission::Interrupted);
    }
    if has("planned") || has("in_progress") || !has("aborted") {
        return Ok(ResumeAdmission::Nothing);
    }
    Ok(ResumeAdmission::Aborted(aborted_adoption(conn, volume_id)?))
}

/// Whether `tapectl volume resume <label>` would, right now, pick this
/// volume's session up and re-enter `confirm` on its recorded seal — the
/// question `volume verify`'s clean-clear message asks before it names
/// `volume resume` (issue #280). Built only from the gates resume itself
/// applies before touching the tape, through the same functions:
/// `coverage::is_write_target` and `coverage::has_completed_write`, then
/// [`resume_admission`] — `Aborted(Adoptable)`, or `Interrupted` with the
/// seal recorded (the 2026-09-21 amendment's re-confirm).
pub fn resume_would_reconfirm(conn: &Connection, volume_id: i64) -> Result<bool> {
    let (status, condition): (String, String) = conn.query_row(
        "SELECT status, observed_condition FROM volumes WHERE id = ?1",
        params![volume_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if !crate::policy::coverage::is_write_target(&status, &condition)
        || crate::policy::coverage::has_completed_write(conn, volume_id)?
    {
        return Ok(false);
    }
    Ok(match resume_admission(conn, volume_id)? {
        ResumeAdmission::Aborted(AbortedAdoption::Adoptable) => true,
        ResumeAdmission::Interrupted => seal_recorded(conn, volume_id)?,
        ResumeAdmission::Aborted(_) | ResumeAdmission::Nothing => false,
    })
}

/// Whether this volume's `aborted` write session is one `tapectl volume
/// resume <label>` would re-confirm once a clean full verify of the volume
/// is recorded — the question `stage create`'s refusal for an abandoned
/// stage set asks before it tells the operator that releasing the staged
/// files forfeits a re-confirmation (issue #325). Unlike
/// [`resume_would_reconfirm`] it does not require that verify to have
/// happened yet: the operator is told to run it. Built from the same gates
/// `write::volume_resume` applies, through the same functions — the volume
/// is `initialized` with no completed write, and [`resume_admission`] finds
/// an aborted session whose only unmet conditions (if any) are the ones a
/// clean full verify resolves (`ConditionNotOk`,
/// `NoCleanFullVerifyAfterAbort`). `NotSealed`, `NotInitialized` and
/// `NoRecordedAbort` never become adoptable, so they answer `false`.
pub fn aborted_session_reconfirmable_after_verify(
    conn: &Connection,
    volume_id: i64,
) -> Result<bool> {
    let status: String = conn.query_row(
        "SELECT status FROM volumes WHERE id = ?1",
        params![volume_id],
        |r| r.get(0),
    )?;
    if status != "initialized" || crate::policy::coverage::has_completed_write(conn, volume_id)? {
        return Ok(false);
    }
    Ok(matches!(
        resume_admission(conn, volume_id)?,
        ResumeAdmission::Aborted(
            AbortedAdoption::Adoptable
                | AbortedAdoption::ConditionNotOk { .. }
                | AbortedAdoption::NoCleanFullVerifyAfterAbort { .. }
        )
    ))
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
    /// Why execution stopped, when it was not an interrupt (issue #408): a
    /// staged file that could not be read, or a drive error other than a
    /// full medium. `None` for SIGINT.
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    pub fn layout(&self) -> &super::layout_model::Layout {
        &self.built.layout
    }

    /// Whether resuming this session can only READ the tape: its volume's
    /// seal is recorded (`volumes.sealed_at`, [`seal_recorded`]), so
    /// [`Self::resume_checking`] ends in `Confirming` or `Quarantined` on
    /// every arm and never reaches the write phase — the `Blank | Matches`
    /// arm returns `Confirming` on a recorded seal, the other two arms
    /// return before writing, and an adopted `aborted` session is refused
    /// outright past them. `volume resume` opens the drive read-only on this
    /// answer (issue #407), so a sealed tape shelved write-protected can be
    /// re-confirmed without sliding the tab. A write the answer missed would
    /// fail on the read-only descriptor rather than reach the tape.
    pub fn confirm_only(&self, conn: &Connection) -> Result<bool> {
        seal_recorded(conn, self.volume_id)
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
        Self::load(conn, volume_id, "interrupted")
    }

    /// Adopt a volume's `aborted` write session for `volume resume` —
    /// ADR-0012's 2026-09-23 amendment (issue #280, Option 2). Returns
    /// `Ok(None)` unless [`resume_admission`] says `Aborted(Adoptable)`: no
    /// `interrupted`/`planned`/`in_progress` row, the seal recorded
    /// (`volumes.sealed_at`), `observed_condition = 'ok'`, and a passing
    /// FULL verify recorded after the recorded abort ([`aborted_adoption`]).
    ///
    /// The session it returns is rehydrated exactly as [`Self::rehydrate`]
    /// rehydrates an interrupted one (the frozen Layout, never a rebuilt
    /// one), and it is marked so that `resume_checking` can only re-enter
    /// `confirm` or quarantine: never `reposition_for_resume`, never a data
    /// write, never `seal()` (ADR-0003). Only a passing `confirm` then moves
    /// its rows to `completed` and the volume to `sealed`; this method writes
    /// nothing.
    pub fn adopt_aborted(conn: &Connection, volume_id: i64) -> Result<Option<InterruptedSession>> {
        if resume_admission(conn, volume_id)?
            != ResumeAdmission::Aborted(AbortedAdoption::Adoptable)
        {
            return Ok(None);
        }
        let session = Self::load(conn, volume_id, "aborted")?;
        // Defence in depth: `load` must hand back a session flagged as
        // adopted, or `resume_checking`'s write-phase refusal cannot fire.
        debug_assert!(session.as_ref().is_none_or(|s| s.adopted_from_aborted));
        Ok(session)
    }

    /// [`Self::load`]'s refusal when one volume's `writes` rows in `status`
    /// name more than one session directory. The directories are listed
    /// plainly, comma-separated — Rust's `{:?}` of the Vec until issue #357.
    fn session_dirs_disagree(volume_id: i64, status: &str, dirs: &[&str]) -> TapectlError {
        TapectlError::Other(format!(
            "volume {volume_id}: its {status} `writes` rows name {} different session \
             directories ({}) — these are not one write session, and resuming would \
             mix frozen files from different builds. Resolve by hand before retrying.",
            dirs.len(),
            dirs.join(", ")
        ))
    }

    /// The shared loader behind [`Self::rehydrate`] and
    /// [`Self::adopt_aborted`]: every `writes` row of `volume_id` in
    /// `status`, their one shared session directory, and the frozen Layout
    /// in it.
    fn load(conn: &Connection, volume_id: i64, status: &str) -> Result<Option<InterruptedSession>> {
        // `plan` inserts one row per unit in order, so ordering by id
        // reproduces its `write_ids` sequence exactly.
        let rows: Vec<(i64, i64, Option<String>)> = conn
            .prepare(
                "SELECT id, snapshot_id, session_dir FROM writes
                 WHERE volume_id = ?1 AND status = ?2
                 ORDER BY id",
            )?
            .query_map(params![volume_id, status], |r| {
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
                        "volume {volume_id}: {status} write {id} has no recorded session \
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
            return Err(Self::session_dirs_disagree(volume_id, status, &dirs));
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
            adopted_from_aborted: status == "aborted",
            reason: None,
        }))
    }

    /// `InterruptedSession -> ResumeOutcome`, checking for interruption via
    /// the real process-global signal flag. See [`Self::resume_checking`]
    /// for the injectable form tests use.
    pub fn resume(
        self,
        conn: &Connection,
        keys: &KeyAvailability,
        slice_check: SliceCheck,
        store: &mut dyn Store,
    ) -> Result<ResumeOutcome> {
        self.resume_checking(
            conn,
            keys,
            slice_check,
            store,
            crate::signal::is_interrupted,
        )
    }

    /// Resume the same session against the same tape —
    /// `docs/design/layout-session.md`'s Resume rule, verbatim: revalidate
    /// the Layout (staged slices present at their recorded size — full-hashed
    /// only under `slice_check` = [`SliceCheck::FullHash`]; frozen generated
    /// zones re-hash byte-identical, always), rewind, read file 0, require ID-thunk identity match
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
        slice_check: SliceCheck,
        store: &mut dyn Store,
        mut is_interrupted: impl FnMut() -> bool,
    ) -> Result<ResumeOutcome> {
        // 1. Revalidate: staged slices present at their recorded size (and
        // full-hashed under `--prewrite-hash`), frozen zones re-hash
        // byte-identical (re-runs the same tri-layer-L1 + materialized-zone
        // checks `into_validated` ran originally). Without the full hash a
        // slice that rotted in staging is caught by L2 during the resumed
        // execute, exactly as on a fresh write. Failure here NEVER
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
        let phase = crate::progress::phase("revalidate", None);
        let revalidated = self.built.validate(keys, slice_check);
        if revalidated.is_ok() {
            phase.done();
        }
        if let Err(errs) = revalidated {
            // #404's follow-up: a signal stopped `--prewrite-hash`'s full
            // read. That is a stop, not a finding about the slices: nothing
            // was compared past that point, nothing was written, and the
            // session keeps the state it had, so the answer is to run the
            // resume again — not to "fix the cause" or abort.
            if let Some(stop) = LayoutError::interruption(&errs) {
                return Err(TapectlError::Interrupted(format!(
                    "volume resume: {stop}, before anything was written — the session keeps \
                     its state and the cartridge is untouched. Run `tapectl volume resume {}` \
                     again",
                    self.built.layout.label
                )));
            }
            let detail = errs
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            // An adopted `aborted` session (#280) is not in `interrupted`
            // and `volume abort` refuses it (it selects only unfinished
            // rows), so neither may be claimed or named for it.
            if self.adopted_from_aborted {
                return Err(TapectlError::Other(format!(
                    "volume resume: revalidation failed, so nothing was written — the session's \
                     `writes` rows stay `aborted` and the sealed cartridge is untouched. Causes: \
                     {detail}. Re-entering confirm on the recorded seal needs this session's \
                     frozen staging files and keys present; fix the cause (an unmounted staging \
                     filesystem or a temporarily absent key are indistinguishable here from \
                     permanent loss) and run `tapectl volume resume` again. If the inputs are \
                     genuinely gone, this volume cannot be re-confirmed; copy its data to \
                     another volume if it must count as a copy."
                )));
            }
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
        let phase = crate::progress::phase("identify", None);
        let (contact, facts) = contact_report(
            store,
            &self.built.layout.label,
            &self.built.layout.volume_uuid,
            seal_position,
        );
        phase.done();
        // Issue #403: whatever this contact already read at the seal
        // position goes to confirm, so a re-confirming resume locates the
        // seal once, not again for the chain walk.
        let seal_bytes = seal_position.and_then(|p| facts.seal_bytes(p));
        match contact {
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
                        // The seal is recorded but did not read just now:
                        // the gate goes first, so an unreadable seal is
                        // found at one read, not after the whole pass.
                        seal_order: ReadOrder::SealFirst,
                        seal_bytes,
                    }));
                }
                // sealed_at is NULL: the seal is still genuinely owed.
                // Fall through to the two-case cursor rule exactly as
                // before.
            }
            ContactOutcome::FileZeroUnreadable { error } => {
                // Issue #400. A sealed session only re-enters confirm, which
                // reads and never writes — the same answer `Blank` gets
                // above, and the one a resume of a sealed tape with a bad
                // File 0 always had.
                if seal_recorded(conn, self.volume_id)? {
                    return Ok(ResumeOutcome::Confirming(SealedPending {
                        built: self.built,
                        volume_id: self.volume_id,
                        write_ids: self.write_ids,
                        // Nothing on this tape read just now: the gate goes
                        // first, as for a recorded seal that did not read.
                        seal_order: ReadOrder::SealFirst,
                        seal_bytes,
                    }));
                }
                // An unsealed one would write. A read error is not proof
                // the tape is blank — it may be this session's own File 0
                // on a drive that needs cleaning, or another cartridge — and
                // not proof of divergence either, so neither the rewrite
                // nor a quarantine: refuse, change nothing, and let the
                // operator resume again once the drive reads.
                return Err(TapectlError::Other(format!(
                    "volume resume: File 0 of the loaded cartridge could not be read ({error}), \
                     and the tape is not provably blank, so tapectl cannot confirm it is \
                     volume \"{}\" and will not write to it (ADR-0003). Nothing was written; \
                     the session stays `interrupted`. Check the drive (clean it, reseat the \
                     cartridge) and that the right cartridge is loaded, then run `tapectl \
                     volume resume {}` again.",
                    self.built.layout.label, self.built.layout.label
                )));
            }
            contact @ (ContactOutcome::IdentityMismatch { .. } | ContactOutcome::EmptyFileZero) => {
                // Issue #327: an empty File 0 at resume is divergence too —
                // this session wrote a real ID thunk there. It quarantines
                // exactly as an unparseable File 0 does (`found: None`).
                let found = match contact {
                    ContactOutcome::IdentityMismatch { found } => found,
                    _ => None,
                };
                // ADR-0012's 2026-09-17 amendment (issue #242): this is a
                // catalog fact tapectl OBSERVED, never one the operator
                // chose -- so it moves `observed_condition`, not `status`.
                // The operator's own status (e.g. a terminal `retired`) is
                // never overwritten by this write.
                return Ok(ResumeOutcome::Quarantined(record_quarantine(
                    conn,
                    self.volume_id,
                    &self.built.layout.label,
                    &self.write_ids,
                    QuarantineReason::IdentityMismatch {
                        expected_label: self.built.layout.label.clone(),
                        expected_uuid: self.built.layout.volume_uuid.clone(),
                        found,
                    },
                )?));
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
                    &facts,
                    &self.built.layout.label,
                    &self.built.layout.volume_uuid,
                    seal_position,
                ) {
                    return Ok(ResumeOutcome::Confirming(SealedPending {
                        built: self.built,
                        volume_id: self.volume_id,
                        write_ids: self.write_ids,
                        // `resume_reconfirm_eligible` has just parsed a seal
                        // marker at this session's own seal position, so it
                        // is read last, in the forward pass (issue #397).
                        seal_order: ReadOrder::SealLast,
                        seal_bytes,
                    }));
                }

                // Any of the three conditions failing keeps today's
                // behaviour exactly. Same reasoning as the
                // `IdentityMismatch` arm just above (issue #242): an
                // observed fact, not an operator choice.
                return Ok(ResumeOutcome::Quarantined(record_quarantine(
                    conn,
                    self.volume_id,
                    &self.built.layout.label,
                    &self.write_ids,
                    QuarantineReason::AlreadySealed {
                        seal_position: found_seal_position,
                    },
                )?));
            }
        }

        // ADR-0012's 2026-09-23 amendment (#280): an adopted `aborted`
        // session was admitted only because its seal is recorded, so the
        // `Blank | Matches` arm above has already returned `Confirming` and
        // the other two arms returned `Quarantined`. Reaching the write
        // phase here would mean `sealed_at` vanished between admission and
        // now; refuse rather than write to a cartridge ADR-0003 says is
        // immutable.
        if self.adopted_from_aborted {
            return Err(TapectlError::Other(format!(
                "volume resume: volume {} was adopted from an aborted session because its seal \
                 is recorded, but the catalog no longer records that seal. Refusing to write: \
                 a sealed tape is never rewritten (ADR-0003). Nothing was written.",
                self.volume_id
            )));
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
        let cursor = if written_slices == 0 {
            0
        } else {
            first_slice_index + written_slices
        };

        // Issue #403: the catalog's cursor is not the last word. Every file
        // but the seal ends with an IMMEDIATE filemark, so a power loss or a
        // bus reset can leave the tape short of what was recorded `written`.
        // Resume continues from what the medium really holds when that is
        // less — never forward of the catalog — and the positions past it
        // go back to `pending`, to be written again.
        let phase = crate::progress::phase("positioning", None);
        let start_index = store.reposition_at_most(cursor as u32)? as usize;
        phase.done();
        if start_index < cursor {
            tracing::warn!(
                catalog_cursor = cursor,
                medium_files = start_index,
                "the tape holds fewer files than the catalog recorded written; resuming from \
                 the medium's count"
            );
            crate::progress::log(&format!(
                "the tape holds {start_index} file(s) where the catalog recorded {cursor} \
                 written; resuming from file {start_index}"
            ));
            demote_positions_from(conn, &self.write_ids, start_index)?;
        }

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
            // Just written, with a synchronous filemark: the head is at end
            // of data, and the confirm reads the seal last, at the end of
            // its one forward pass (issue #397).
            seal_order: ReadOrder::SealLast,
            seal_bytes: None,
        })
    }
}

/// The seal marker is on tape; confirm has not run yet. Its only operation
/// is [`Self::confirm`].
pub struct SealedPending {
    built: BuiltLayout,
    volume_id: i64,
    write_ids: Vec<(i64, i64)>,
    /// Where confirm reads the seal marker (issue #397): last when this
    /// session has just written or just read it, first otherwise.
    seal_order: ReadOrder,
    /// The seal marker's on-tape bytes, when this contact already read them
    /// (a resume's contact check, issue #403) — `confirm` starts from them
    /// instead of locating the seal a second time. `None` after `seal()`.
    seal_bytes: Option<Vec<u8>>,
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
        let verify_type = tier.verify_type();
        // Issue #410: read before this confirm's own row exists, which
        // would otherwise be "the latest" itself.
        let resume = match tier {
            Tier::Integrity => interrupted_readback(conn, self.volume_id)?,
            Tier::Navigable => None,
        };
        if let Some(r) = &resume {
            tracing::info!(
                label = %self.built.layout.label,
                files = r.checkpoints.passed.len(),
                "continuing an interrupted full readback: files it read back clean are not \
                 read again if the front index is unchanged"
            );
        }
        conn.execute(
            "INSERT INTO verification_sessions (volume_id, verify_type, outcome)
             VALUES (?1, ?2, 'in_progress')",
            params![self.volume_id, verify_type],
        )?;
        let vs_id = conn.last_insert_rowid();

        // Issue #376 (c): the status each `writes` row entered confirm in —
        // `in_progress` after a fresh seal, `interrupted` on a re-confirm,
        // `aborted` for a session `adopt_aborted` picked up (#280). The seal
        // transaction below completes a row only if it still reads the same,
        // so an abort that landed during the readback is reported, never
        // overwritten.
        let entry_statuses: Vec<String> = self
            .write_ids
            .iter()
            .map(|(write_id, _)| {
                conn.query_row(
                    "SELECT status FROM writes WHERE id = ?1",
                    params![write_id],
                    |r| r.get(0),
                )
            })
            .collect::<rusqlite::Result<_>>()?;

        // Issue #386: the readback — hours on a full cartridge — is its own
        // phase, counted byte by byte through `Store::confirm`. A quick
        // confirm reads no content file, so only a full one has a byte
        // total (as `volume verify` already does).
        let total = match tier {
            Tier::Integrity => self.built.layout.on_tape_bytes().ok(),
            Tier::Navigable => None,
        };
        let phase = crate::progress::phase("confirm", total);
        // Issue #410: every file read back clean is recorded as the walk
        // goes, so an interruption (#404) leaves this readback continuable.
        // Best-effort: a row that cannot be written costs one re-read on a
        // later resume, never this readback.
        let record = |c: Checkpoint<'_>| record_checkpoint(conn, vs_id, resume.as_ref(), c);
        let plan = ConfirmPlan::new(tier)
            .with_order(self.seal_order)
            .resuming(Continuation::checkpoints(resume.as_ref()));
        let plan = match tier {
            Tier::Integrity => plan.checkpointing(&record),
            Tier::Navigable => plan,
        };
        let plan = plan.with_seal(self.seal_bytes.as_deref());
        let evidence = store.confirm_with(&self.built.layout, plan)?;
        phase.done();
        let passed = evidence.mismatches.is_empty();
        // ADR-0012's 2026-09-18 amendment: a mismatch alone is not a
        // quarantine verdict. A full confirm (`--full-confirm`) reads back
        // the WHOLE cartridge — hours on a full LTO-6 — and one transient
        // SCSI error in that window must not condemn a physically sound
        // tape.
        let proves_medium_bad = evidence.proves_medium_bad();

        // Issue #377: recorded after an hours-long readback, so a busy
        // catalog is waited out rather than failing the confirm.
        busy::retry(
            BusyPolicy::DEFAULT,
            "the confirm's verification session",
            || {
                Ok(conn.execute(
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
                )?)
            },
        )?;
        // Issue #142: the same per-mismatch detail `volume verify` records,
        // through the same writer — write-time confirm is the OTHER producer
        // of an `Evidence`, and a quarantine that cannot say which position
        // failed is as unhelpful here as it was there.
        super::write::record_verification_results(conn, vs_id, self.volume_id, &evidence)?;

        if passed {
            // IMMEDIATE (issue #377: it reads snapshot statuses, then
            // writes) and retried as a whole on a busy catalog — a dropped
            // transaction rolls back, so each attempt starts clean.
            let promoted = busy::retry(BusyPolicy::DEFAULT, "the seal", || {
                let tx = busy::immediate_tx(conn)?;
                for ((write_id, _), entered) in self.write_ids.iter().zip(&entry_statuses) {
                    // ADR-0012 2026-10-06 item 24: `write_verified` means
                    // fully read back — an Integrity confirm, never a quick one.
                    let completed = tx.execute(
                        "UPDATE writes SET status = 'completed', completed_at = datetime('now'),
                                write_verified = ?3
                         WHERE id = ?1 AND status = ?2",
                        params![write_id, entered, tier == Tier::Integrity],
                    )?;
                    if completed == 0 {
                        // Issue #376 (c): the row moved while confirm read the
                        // tape — an operator's abort got there first. Dropping
                        // `tx` rolls back; nothing is sealed over it.
                        let now: Option<String> = tx
                            .query_row(
                                "SELECT status FROM writes WHERE id = ?1",
                                params![write_id],
                                |r| r.get(0),
                            )
                            .ok();
                        return Err(TapectlError::Other(format!(
                            "volume \"{}\": the confirm readback passed, but write session \
                             {write_id} changed while it ran (entered confirm `{entered}`, now \
                             `{}`) — most likely `tapectl volume abort`. The catalog was NOT \
                             updated to sealed over it: the volume stays `initialized` and \
                             does not count as a copy. The tape itself is sealed; see `tapectl \
                             volume resume` for re-confirming an aborted session.",
                            self.built.layout.label,
                            now.as_deref().unwrap_or("gone"),
                        )));
                    }
                }
                // Snapshot promotions, plus what each one needs for its audit
                // row (issue #58). The pre-flip status is read BEFORE the
                // update so the event records a real old->new transition
                // rather than guessing, and so nothing is logged when the
                // guard matches no row.
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
                Ok(promoted)
            })?;

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
            // left exactly where it was (never 'sealed'). `aborted` here is
            // not final: the seal is recorded, so once a clean full verify
            // clears the condition `volume resume` adopts this session
            // (`InterruptedSession::adopt_aborted`, ADR-0012 2026-09-23).
            Ok(ConfirmOutcome::Quarantined(record_quarantine(
                conn,
                self.volume_id,
                &self.built.layout.label,
                &self.write_ids,
                QuarantineReason::ConfirmFailed(evidence),
            )?))
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

    /// Move this session's `writes` rows to `'interrupted'` without ever
    /// calling `confirm` (issue #276's `TAPECTL_TEST_PAUSE_AFTER_SEAL` hook,
    /// `volume::write::finish_session`). Exposed `pub(crate)` rather than
    /// inlined at that call site because `write_ids` and [`mark_writes`] are
    /// both private to this module; this is a thin, read-only-in-intent
    /// door onto the same helper `confirm`'s own `Inconclusive`/`Quarantined`
    /// arms use, so an interruption caught here leaves EXACTLY the same
    /// `writes.status = 'interrupted'` / `volumes.sealed_at` set /
    /// `volumes.status = 'initialized'` shape migration 018's case (b)
    /// describes — `tapectl volume resume` (`rehydrate` selects
    /// `interrupted` rows, `seal_recorded` reads `sealed_at`) picks it back
    /// up and re-enters confirm directly, exactly as it does for a real
    /// `Inconclusive` outcome.
    pub(crate) fn mark_interrupted(&self, conn: &Connection) -> Result<()> {
        mark_writes(conn, &self.write_ids, "interrupted")
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
/// with a clean abort on mismatch; a `store.execute` error is caught, never
/// a hard `Err` out of the whole session — a full medium is the same clean
/// abort, any other failure an `interrupted` session, issue #408). Still
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
                    adopted_from_aborted: false,
                    reason: None,
                }));
            }
        }
    }

    // Issue #386: the write phase — every byte streamed to the store is
    // counted, and the file being written is the current item. A resume
    // counts only what is left to write.
    let to_write: u64 = content_entries[start_index..]
        .iter()
        .filter_map(|e| e.size_bytes)
        .sum();
    let file_count = built.layout.entries.len();
    let phase = crate::progress::phase("write", Some(to_write));
    // Issue #390: the write pipeline's buffers, one tape block each, reused
    // by every entry of this call; their total is the pipeline's memory
    // bound (`pipeline::QUEUE_BYTES`).
    let mut pool = BufferPool::with_queue_bytes(
        built.layout.block_size.max(1) as usize,
        pipeline::QUEUE_BYTES,
    );

    for entry in &content_entries[start_index..] {
        phase.item(format!(
            "file {} of {file_count} ({})",
            entry.position,
            entry.kind.type_label()
        ));
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
                adopted_from_aborted: false,
                reason: None,
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

        // Stream the entry into the store through the write pipeline
        // (issue #390): a reader thread reads the staged file, a hasher
        // thread hashes it, and the store's `execute` writes it here, on this
        // thread — three stages overlapped, where they used to take turns.
        // Any store-level failure is caught rather than propagated: a full
        // medium has no salvage path (ADR-0007), so it becomes the same clean
        // abort as a hash mismatch; any other failure — a staged file the
        // disk cannot read, a drive error that is not ENOSPC — becomes an
        // `interrupted` session (issue #408, `Stop` below). Neither is a hard
        // `Err` out of the whole session.
        //
        // Tri-layer L2 (`v2-open-questions.md` §2.4): the hash is of the very
        // bytes the store takes, and a mismatch is a clean abort. This is what
        // closes the validate->execute TOCTOU window, and since ADR-0012's
        // 2026-09-30 (later) amendment it is also the DEFAULT rot check for
        // staged slices: `validate` only size-checks them unless
        // `--prewrite-hash` asked it to full-hash from disk. Since #390 the
        // verdict comes BEFORE the file's last block is handed to the store:
        // a mismatched file never gets its last block or its filemark, let
        // alone a seal (an empty file, with no block to hold back, is judged
        // after, as every file used to be).
        let entry_started = std::time::Instant::now();
        let expected_hash = entry.sha256.as_deref();
        // Issue #417: read once and not again soon (the next read of a
        // staged slice is the next copy's write), so behind the cursor the
        // pages are dropped instead of filling the host's page cache.
        let (verdict, waits) = match crate::util::DropBehind::open(path) {
            Err(e) => (
                Verdict::Failed(TapectlError::SourceIo(format!(
                    "open entry at position {}: {e}",
                    entry.position
                ))),
                None,
            ),
            Ok(file) => {
                let streamed =
                    pipeline::write_verified(&mut pool, file, size, expected_hash, |src| {
                        // Counted here, as the store takes them — not where
                        // the reader thread, up to a queue ahead, reads them.
                        store.execute(&mut crate::progress::CountingReader(src), size, false)
                    });
                let waits = streamed.stats;
                (streamed.verdict(expected_hash), Some(waits))
            }
        };

        // Issue #408: only a full medium and a tri-layer L2 mismatch end
        // the session `aborted` — the two ADR-0007 and §2.4 name. Any other
        // failure (a staged file the disk could not read, a drive error that
        // is not ENOSPC) leaves the tape exactly where a crash leaves it: the
        // files before this one whole, this one partial. That is the
        // `interrupted` state resume already repositions from, so the
        // session stays resumable once the cause is fixed.
        let stop = match &verdict {
            Verdict::Written(_) => None,
            Verdict::Mismatch(actual_hash) => Some(Stop::Abort(format!(
                "hash mismatch at position {}: expected {}, got {actual_hash}",
                entry.position,
                expected_hash.unwrap_or("no recorded hash")
            ))),
            Verdict::Failed(e @ TapectlError::MediumFull(_)) => Some(Stop::Abort(format!(
                "execute failed at position {}: {e}",
                entry.position
            ))),
            Verdict::Failed(e) => Some(Stop::Interrupt(format!(
                "execute stopped at position {}: {e}",
                entry.position
            ))),
        };

        if let ZoneKind::Slice { stage_slice_id } = entry.kind {
            let write_id = *slice_write_id
                .get(&stage_slice_id)
                .expect("plan() populated slice_write_id for every slice entry");
            // Issue #377: the cursor row for a slice already on tape. A busy
            // catalog is waited out (minutes), never allowed to stop the
            // drive mid-tape; each UPDATE is idempotent, so a retry is safe.
            busy::retry(BusyPolicy::DEFAULT, "a slice's write position", || {
                match &verdict {
                    Verdict::Written(actual_hash) => {
                        conn.execute(
                            "UPDATE write_positions
                             SET status = 'written', written_at = datetime('now'),
                                 sha256_on_volume = ?1
                             WHERE write_id = ?2 AND stage_slice_id = ?3",
                            params![actual_hash, write_id, stage_slice_id],
                        )?;
                    }
                    Verdict::Mismatch(actual_hash) => {
                        // Streamed, but the hash didn't match.
                        conn.execute(
                            "UPDATE write_positions SET status = 'failed', sha256_on_volume = ?1
                             WHERE write_id = ?2 AND stage_slice_id = ?3",
                            params![actual_hash, write_id, stage_slice_id],
                        )?;
                    }
                    Verdict::Failed(_) => {
                        // Never streamed in full (open failed or store.execute
                        // errored) — no sha256_on_volume to record. `failed`
                        // when the session aborts; `pending` when it stays
                        // resumable, since resume writes it again (#408).
                        let status = match &stop {
                            Some(Stop::Interrupt(_)) => "pending",
                            _ => "failed",
                        };
                        conn.execute(
                            "UPDATE write_positions SET status = ?1
                             WHERE write_id = ?2 AND stage_slice_id = ?3",
                            params![status, write_id, stage_slice_id],
                        )?;
                    }
                }
                Ok(())
            })?;
        }

        // Issue #386: one session-log line per file written, so a log
        // always says which file a long write was on, and how fast each went.
        // Issue #390 adds where the time went: the tape writer waiting for
        // data (the disk read or the hash behind it), and the reader waiting
        // for queue space (the hash or the tape behind it). Together they
        // name the slowest stage: tape waited ~0 = the drive; both high =
        // the hash; only tape waited high = the staged-file read.
        let took = entry_started.elapsed();
        crate::progress::log(&format!(
            "wrote file {} ({}): {} in {}{}{}",
            entry.position,
            entry.kind.type_label(),
            crate::progress::format_bytes(size),
            crate::progress::format_duration(took),
            match waits {
                Some(w) => format!(
                    "; tape waited {} for data, queue full {}",
                    crate::progress::format_duration(w.consumer_waited),
                    crate::progress::format_duration(w.producer_waited)
                ),
                None => String::new(),
            },
            match &stop {
                Some(Stop::Abort(_)) => " — ABORTED",
                Some(Stop::Interrupt(_)) => " — INTERRUPTED",
                None => "",
            }
        ));

        match stop {
            None => {}
            Some(Stop::Abort(reason)) => {
                mark_writes(conn, &write_ids, "aborted")?;
                return Ok(ExecuteOutcome::Aborted(AbortedSession {
                    volume_id,
                    reason,
                }));
            }
            Some(Stop::Interrupt(reason)) => {
                mark_writes(conn, &write_ids, "interrupted")?;
                return Ok(ExecuteOutcome::Interrupted(InterruptedSession {
                    built,
                    volume_id,
                    write_ids,
                    slice_write_id,
                    adopted_from_aborted: false,
                    reason: Some(reason),
                }));
            }
        }
    }

    phase.done();
    Ok(ExecuteOutcome::Ready(ReadyToSeal {
        built,
        volume_id,
        write_ids,
    }))
}

/// How one entry's write ended the session, when it did (issue #408).
enum Stop {
    /// A full medium or an L2 mismatch: `aborted`, never resumed.
    Abort(String),
    /// Any other failure: `interrupted`, resumable once the cause is fixed.
    Interrupt(String),
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

/// Record a write-path quarantine as ONE act (issue #324): the observed
/// condition (ADR-0012's 2026-09-17 amendment, issue #242 — `status` is
/// never touched), every `writes` row of the session `aborted`, and the
/// `write_quarantined` event whose timestamp is the recorded abort
/// [`InterruptedSession::adopt_aborted`] must find later (ADR-0012,
/// 2026-09-23). All three commit together or not at all: a crash can no
/// longer leave `aborted` rows with no recorded abort, which `volume resume`
/// would refuse forever (`AbortedAdoption::NoRecordedAbort`). The event's
/// text has one writer, `write::log_quarantine`, called here on the
/// transaction.
fn record_quarantine(
    conn: &Connection,
    volume_id: i64,
    label: &str,
    write_ids: &[(i64, i64)],
    reason: QuarantineReason,
) -> Result<QuarantinedSession> {
    // IMMEDIATE, so no statement inside can meet a busy catalog, and the
    // whole act retried as one on a busy BEGIN (issue #377).
    busy::retry(BusyPolicy::DEFAULT, "a quarantine", || {
        let tx = busy::immediate_tx(conn)?;
        tx.execute(
            "UPDATE volumes SET observed_condition = 'quarantined' WHERE id = ?1",
            params![volume_id],
        )?;
        mark_writes(&tx, write_ids, "aborted")?;
        super::write::log_quarantine(&tx, volume_id, label, &reason)?;
        tx.commit()?;
        Ok(())
    })?;
    Ok(QuarantinedSession {
        volume_id,
        label: label.to_string(),
        reason,
    })
}

/// Move a session's `writes` rows to `status`. Retried on a busy catalog
/// (issue #377): this records where a tape session stopped, and losing it to
/// a 5-second lock wait would leave the rows claiming a state the tape is
/// not in. Idempotent, so safe to retry.
fn mark_writes(conn: &Connection, write_ids: &[(i64, i64)], status: &str) -> Result<()> {
    for (write_id, _) in write_ids {
        busy::retry(BusyPolicy::DEFAULT, "a write session's status", || {
            Ok(conn.execute(
                "UPDATE writes SET status = ?1 WHERE id = ?2",
                params![status, write_id],
            )?)
        })?;
    }
    Ok(())
}

/// Move this session's `write_positions` at Layout position `from` or later
/// back to `pending` (issue #403): the medium does not hold them, whatever
/// the catalog recorded.
fn demote_positions_from(conn: &Connection, write_ids: &[(i64, i64)], from: usize) -> Result<()> {
    for (write_id, _) in write_ids {
        busy::retry(BusyPolicy::DEFAULT, "a resume's write positions", || {
            Ok(conn.execute(
                "UPDATE write_positions
                 SET status = 'pending', written_at = NULL, sha256_on_volume = NULL
                 WHERE write_id = ?1 AND CAST(position AS INTEGER) >= ?2",
                params![write_id, from as i64],
            )?)
        })?;
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
        /// What `built` was built from, so a test can build the same
        /// volume's session again — a second attempt (issue #401).
        inputs: BuildInputs,
        _slices_dir: tempfile::TempDir,
        _session_dir: tempfile::TempDir,
    }

    impl Fixture {
        /// The same volume's Layout built again into a fresh session
        /// directory — what a second `volume write` of the label builds.
        fn rebuild(&self) -> (BuiltLayout, tempfile::TempDir) {
            let dir = tempfile::tempdir().unwrap();
            let built = build::build(&self.inputs, dir.path()).unwrap();
            (built, dir)
        }
    }

    fn make_fixture() -> Fixture {
        make_fixture_on(db::open_memory().unwrap())
    }

    /// [`make_fixture`] on a given connection — a file-backed `db::open`
    /// when a test needs a second connection to contend with (issue #377).
    fn make_fixture_on(conn: Connection) -> Fixture {
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
            inputs,
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

    /// A `MemStore` whose readback lets an operator's `volume abort` land in
    /// the middle of confirm — the race issue #376 (c) names.
    struct AbortsDuringConfirm<'c> {
        inner: MemStore,
        conn: &'c Connection,
        volume_id: i64,
    }

    impl Store for AbortsDuringConfirm<'_> {
        fn capacity(&mut self) -> Result<crate::store::CapacityReport> {
            self.inner.capacity()
        }
        fn execute(&mut self, src: &mut dyn std::io::Read, len: u64, sync: bool) -> Result<u64> {
            self.inner.execute(src, len, sync)
        }
        fn read_file(&mut self, position: u32, sink: &mut dyn Write) -> Result<u64> {
            self.inner.read_file(position, sink)
        }
        fn reposition_for_resume(&mut self, file_index: u32) -> Result<()> {
            self.inner.reposition_for_resume(file_index)
        }
        fn confirm_with(
            &mut self,
            layout: &crate::volume::layout_model::Layout,
            plan: ConfirmPlan,
        ) -> Result<Evidence> {
            self.conn
                .execute(
                    "UPDATE writes SET status = 'aborted' WHERE volume_id = ?1",
                    params![self.volume_id],
                )
                .unwrap();
            self.inner.confirm_with(layout, plan)
        }
    }

    /// Issue #376 (c): confirm's completion no longer overwrites an abort
    /// that landed during the readback. The readback passes, but the seal
    /// transaction finds the rows moved, rolls back and says so: `writes`
    /// stay `aborted`, the volume is not `sealed`, no snapshot is promoted.
    #[test]
    fn confirm_does_not_complete_a_session_aborted_during_its_readback() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);
        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let ready = match planned.execute(&f.conn, &mut store).unwrap() {
            ExecuteOutcome::Ready(r) => r,
            _ => panic!("expected Ready on a happy-path MemStore run"),
        };
        let sealed_pending = ready.seal(&mut store).unwrap();
        let mut racing = AbortsDuringConfirm {
            inner: store,
            conn: &f.conn,
            volume_id: f.volume_id,
        };
        let err = match sealed_pending.confirm(&f.conn, &mut racing, Tier::Integrity) {
            Err(e) => e,
            Ok(_) => panic!("confirm must not complete a session aborted under it"),
        };
        assert!(
            err.to_string().contains("changed while it ran"),
            "must name the abort that got there first: {err}"
        );
        let statuses: Vec<String> = f
            .conn
            .prepare("SELECT status FROM writes WHERE volume_id = ?1")
            .unwrap()
            .query_map(params![f.volume_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(statuses.iter().all(|s| s == "aborted"), "{statuses:?}");
        let volume_status: String = f
            .conn
            .query_row(
                "SELECT status FROM volumes WHERE id = ?1",
                params![f.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(volume_status, "sealed", "nothing sealed over the abort");
    }

    /// A `MemStore` that, on the first entry it is asked to write, has
    /// another connection take the catalog's write lock and hold it for
    /// `hold` — a second tapectl process's long finalization landing
    /// mid-tape (issue #377 item 2).
    struct CatalogLockedMidWrite {
        inner: MemStore,
        db_path: std::path::PathBuf,
        hold: std::time::Duration,
        holder: Option<std::thread::JoinHandle<()>>,
    }

    impl Store for CatalogLockedMidWrite {
        fn capacity(&mut self) -> Result<crate::store::CapacityReport> {
            self.inner.capacity()
        }
        fn execute(&mut self, src: &mut dyn std::io::Read, len: u64, sync: bool) -> Result<u64> {
            if self.holder.is_none() {
                let (ready_tx, ready_rx) = std::sync::mpsc::channel();
                let (path, hold) = (self.db_path.clone(), self.hold);
                self.holder = Some(std::thread::spawn(move || {
                    let c = Connection::open(&path).unwrap();
                    c.execute_batch("BEGIN IMMEDIATE").unwrap();
                    ready_tx.send(()).unwrap();
                    std::thread::sleep(hold);
                    c.execute_batch("COMMIT").unwrap();
                }));
                ready_rx.recv().unwrap();
            }
            self.inner.execute(src, len, sync)
        }
        fn read_file(&mut self, position: u32, sink: &mut dyn Write) -> Result<u64> {
            self.inner.read_file(position, sink)
        }
        fn reposition_for_resume(&mut self, file_index: u32) -> Result<()> {
            self.inner.reposition_for_resume(file_index)
        }
    }

    /// Issue #377 item 2: another connection holds the write lock for ten
    /// times this connection's busy_timeout while the tape is being
    /// written. Before, the first slice's `write_positions` UPDATE failed
    /// and stopped the drive; now it waits the lock out and the session
    /// reaches Ready with every slice recorded `written`.
    #[test]
    fn a_catalog_lock_mid_write_is_waited_out_not_fatal() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("tapectl.db");
        let f = make_fixture_on(db::open(&db_path).unwrap());
        f.conn.pragma_update(None, "busy_timeout", 50).unwrap();
        let mut store = CatalogLockedMidWrite {
            inner: MemStore::new(BS as usize),
            db_path,
            hold: std::time::Duration::from_millis(500),
            holder: None,
        };
        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let outcome = planned
            .execute(&f.conn, &mut store)
            .expect("a busy catalog mid-write must be waited out");
        assert!(matches!(outcome, ExecuteOutcome::Ready(_)));
        store.holder.take().unwrap().join().unwrap();
        let unwritten: i64 = f
            .conn
            .query_row(
                "SELECT COUNT(*) FROM write_positions WHERE status != 'written'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unwritten, 0, "every slice's cursor row must be recorded");
    }

    /// Issue #417: execute reads every staged file through `DropBehind`,
    /// so a cartridge's worth of slices does not fill the host's page cache
    /// on the way to the tape. Before, a plain `File::open`.
    #[test]
    fn execute_reads_the_staged_files_through_drop_behind() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);
        let slices: Vec<std::path::PathBuf> = f
            .units
            .iter()
            .flat_map(|u| u.slices.iter().map(|s| s.staging_path.clone()))
            .collect();
        assert!(
            !slices.is_empty(),
            "positive control: the fixture stages slices"
        );
        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        crate::util::page_cache_log::take();
        match planned.execute(&f.conn, &mut store).unwrap() {
            ExecuteOutcome::Ready(_) => {}
            _ => panic!("expected Ready"),
        }
        let read = crate::util::page_cache_log::take();
        for slice in &slices {
            assert!(
                read.contains(slice),
                "{} was not read through DropBehind: {read:?}",
                slice.display()
            );
        }
    }

    /// Issue #394 (`docs/design/threat-model.md` §2, ADR-0012 2026-10-06
    /// item 14): every file of a volume ends with an immediate filemark and
    /// the seal marker — the last — with a synchronous one, which flushes
    /// the drive's buffer to the medium before the catalog records the
    /// seal. The power baseline rests on this, so it is pinned.
    #[test]
    fn only_the_seal_marker_is_written_with_a_synchronous_filemark() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);
        let seal_position = f
            .built
            .layout
            .entries
            .iter()
            .position(|e| matches!(e.kind, ZoneKind::SealMarker))
            .unwrap();
        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let ready = match planned.execute(&f.conn, &mut store).unwrap() {
            ExecuteOutcome::Ready(r) => r,
            _ => panic!("expected Ready"),
        };
        assert!(
            store.syncs.iter().all(|s| !s),
            "every file before the seal ends with an immediate filemark: {:?}",
            store.syncs
        );
        ready.seal(&mut store).unwrap();
        let synced: Vec<usize> = store
            .syncs
            .iter()
            .enumerate()
            .filter(|(_, s)| **s)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            synced,
            [seal_position],
            "only the seal marker's filemark is synchronous"
        );
        assert_eq!(
            store.syncs.len(),
            seal_position + 1,
            "the seal is the last file"
        );
    }

    // --- behavior 1: happy path over MemStore ends Sealed ----------------

    #[test]
    fn happy_path_over_memstore_ends_sealed_with_correct_db_rows() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);
        let expected_file_count = f.built.layout.entries.len();

        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
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

        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
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
        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
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
        // Issue #357: the abort reason reaches the operator in words — the
        // recorded hash itself, not Rust's `{:?}` of an Option
        // (`expected Some("ab12…")`).
        assert!(
            aborted.reason.starts_with("hash mismatch at position ")
                && aborted.reason.contains(", got "),
            "{}",
            aborted.reason
        );
        assert!(
            !aborted.reason.contains("Some(") && !aborted.reason.contains('"'),
            "no Debug rendering of the recorded hash: {}",
            aborted.reason
        );

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

    /// Flip the first byte of a file in place — same size, different bytes.
    fn rot_in_place(path: &Path) {
        let mut bytes = std::fs::read(path).unwrap();
        bytes[0] ^= 0xFF;
        std::fs::write(path, bytes).unwrap();
    }

    // --- ADR-0012 2026-09-30 (later): the pre-write full hash is off by default

    /// The default L1 reads no slice bytes: a staged slice that rotted
    /// BEFORE validate (same size) passes it, and L2 — the inline re-hash of
    /// the bytes streamed to the store — is what catches it: a clean abort
    /// with the hash-mismatch reason, and no seal marker.
    #[test]
    fn default_validate_passes_same_size_rot_and_l2_aborts_it_unsealed() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);
        let seal_position = f
            .built
            .layout
            .entries
            .iter()
            .position(|e| matches!(e.kind, ZoneKind::SealMarker))
            .unwrap();
        rot_in_place(&f.units[0].slices[0].staging_path);

        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .expect("size-only L1 must not read the slice, so same-size rot passes it");
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let aborted = match planned.execute(&f.conn, &mut store).unwrap() {
            ExecuteOutcome::Aborted(a) => a,
            ExecuteOutcome::Ready(_) => panic!("L2 must abort on the rotted slice, got Ready"),
            ExecuteOutcome::Interrupted(_) => panic!("expected Aborted, got Interrupted"),
        };
        assert!(
            aborted.reason.starts_with("hash mismatch at position "),
            "{}",
            aborted.reason
        );
        assert!(
            store.files.len() <= seal_position,
            "no seal marker after an L2 abort: files.len()={}, seal position={seal_position}",
            store.files.len()
        );
        let statuses: Vec<String> = f
            .conn
            .prepare("SELECT status FROM writes WHERE volume_id = ?1")
            .unwrap()
            .query_map(params![f.volume_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(statuses, vec!["aborted".to_string()]);
    }

    /// `--prewrite-hash`: the same rot is refused by validate itself, before
    /// the store sees a single write.
    #[test]
    fn prewrite_hash_refuses_same_size_rot_before_any_tape_io() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);
        rot_in_place(&f.units[0].slices[0].staging_path);

        let errs = match f
            .built
            .into_validated(&f.keys, SliceCheck::FullHash, &mut store)
        {
            Err(errs) => errs,
            Ok(_) => panic!("the full pre-write hash must refuse a rotted slice"),
        };
        assert!(
            errs.iter()
                .any(|e| matches!(e, LayoutError::SliceChecksumMismatch { .. })),
            "{errs:?}"
        );
        assert!(store.files.is_empty(), "nothing written to the store");
    }

    /// Resume without `--prewrite-hash` still re-hashes the frozen generated
    /// zones byte-identical (only the staged-slice full hash is optional):
    /// a tampered materialized zone is refused before anything is written.
    #[test]
    fn resume_without_prewrite_hash_still_refuses_a_tampered_frozen_zone() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);
        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let interrupted = match planned
            .execute_checking(&f.conn, &mut store, || true)
            .unwrap()
        {
            ExecuteOutcome::Interrupted(i) => i,
            _ => panic!("expected Interrupted"),
        };
        let frozen = interrupted
            .built
            .layout
            .entries
            .iter()
            .find_map(|e| match (&e.kind, &e.source) {
                (ZoneKind::SystemGuide, ContentSource::Materialized(p)) => Some(p.clone()),
                _ => None,
            })
            .expect("fixture materializes the system guide");
        rot_in_place(&frozen);

        let msg = match interrupted.resume_checking(
            &f.conn,
            &f.keys,
            SliceCheck::Size,
            &mut store,
            || false,
        ) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a tampered frozen zone must fail resume's revalidation"),
        };
        assert!(msg.contains("revalidation failed"), "{msg}");
        assert!(msg.contains("hash mismatch"), "{msg}");
        assert!(
            store.files.is_empty(),
            "nothing written: {}",
            store.files.len()
        );
    }

    /// Resume without `--prewrite-hash`: a slice that rotted in staging while
    /// the session was interrupted passes revalidation and L2 aborts the
    /// resumed execute cleanly — the same net as a fresh write.
    #[test]
    fn resume_without_prewrite_hash_lets_l2_abort_a_slice_that_rotted_meanwhile() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);
        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let interrupted = match planned
            .execute_checking(&f.conn, &mut store, || true)
            .unwrap()
        {
            ExecuteOutcome::Interrupted(i) => i,
            _ => panic!("expected Interrupted"),
        };
        rot_in_place(&f.units[0].slices[0].staging_path);

        match interrupted
            .resume_checking(&f.conn, &f.keys, SliceCheck::Size, &mut store, || false)
            .unwrap()
        {
            ResumeOutcome::Aborted(a) => assert!(
                a.reason.starts_with("hash mismatch at position "),
                "{}",
                a.reason
            ),
            _ => panic!("expected the resumed execute to abort on L2"),
        }
    }

    /// Resume with `--prewrite-hash`: the same rot is refused at
    /// revalidation, before any write.
    #[test]
    fn resume_with_prewrite_hash_refuses_a_slice_that_rotted_meanwhile() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);
        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let interrupted = match planned
            .execute_checking(&f.conn, &mut store, || true)
            .unwrap()
        {
            ExecuteOutcome::Interrupted(i) => i,
            _ => panic!("expected Interrupted"),
        };
        rot_in_place(&f.units[0].slices[0].staging_path);

        let msg = match interrupted.resume_checking(
            &f.conn,
            &f.keys,
            SliceCheck::FullHash,
            &mut store,
            || false,
        ) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("the full pre-write hash must refuse the rotted slice"),
        };
        assert!(msg.contains("revalidation failed"), "{msg}");
        assert!(msg.contains("checksum mismatch"), "{msg}");
        assert!(
            store.files.is_empty(),
            "nothing written: {}",
            store.files.len()
        );
    }

    /// #404's follow-up: a signal during resume's `--prewrite-hash` stops
    /// the revalidation as a stop (`TapectlError::Interrupted`, "run the
    /// resume again"), not as a revalidation failure telling the operator
    /// to fix a cause or abort. Before, it was `Other("revalidation
    /// failed … Causes: …")`.
    #[test]
    fn a_signal_during_resume_revalidation_is_a_stop_not_a_failure() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);
        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let interrupted = match planned
            .execute_checking(&f.conn, &mut store, || true)
            .unwrap()
        {
            ExecuteOutcome::Interrupted(i) => i,
            _ => panic!("expected Interrupted"),
        };
        let written = store.files.len();

        crate::signal::interrupt_this_thread(true);
        let r =
            interrupted
                .resume_checking(&f.conn, &f.keys, SliceCheck::FullHash, &mut store, || false);
        crate::signal::interrupt_this_thread(false);
        match r {
            Err(TapectlError::Interrupted(at)) => {
                assert!(
                    at.contains("full hash of the staged slices stopped"),
                    "{at}"
                );
                assert!(at.contains("volume resume SESSTEST"), "{at}");
            }
            Err(other) => panic!("a stop, not a failure: {other}"),
            Ok(_) => panic!("the signal must stop the revalidation"),
        }
        assert_eq!(store.files.len(), written, "nothing more written");
        let states: Vec<String> = f
            .conn
            .prepare("SELECT status FROM writes WHERE volume_id = ?1")
            .unwrap()
            .query_map(params![f.volume_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            !states.is_empty() && states.iter().all(|s| s == "interrupted"),
            "the session keeps its state: {states:?}"
        );
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

        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
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

    // --- issue #401: an abandoned attempt never blocks the next one ---

    fn statuses_on(conn: &Connection, volume_id: i64) -> Vec<String> {
        conn.prepare("SELECT status FROM writes WHERE volume_id = ?1 ORDER BY id")
            .unwrap()
            .query_map(params![volume_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// What a second attempt needs from a [`Fixture`] whose `built` the
    /// first attempt consumed.
    #[derive(Clone, Copy)]
    struct Retry<'a> {
        conn: &'a Connection,
        keys: &'a KeyAvailability,
        units: &'a [BuildUnit],
        volume_id: i64,
    }

    /// Field by field, so it works on a fixture whose `built` is gone.
    macro_rules! retry_of {
        ($f:expr) => {
            Retry {
                conn: &$f.conn,
                keys: &$f.keys,
                units: &$f.units,
                volume_id: $f.volume_id,
            }
        };
    }

    /// Run a fresh session for `built` to its end over `store` and say how
    /// it ended — Sealed must be what a retry reaches.
    fn run_to_sealed(f: Retry<'_>, built: BuiltLayout, store: &mut MemStore) -> SealedSession {
        let planned = built
            .into_validated(f.keys, SliceCheck::Size, store)
            .unwrap_or_else(|e| panic!("validate: {e:?}"))
            .plan(f.conn, f.volume_id, f.units)
            .expect("issue #401: a new attempt plans on the same volume");
        let ready = match planned.execute(f.conn, store).unwrap() {
            ExecuteOutcome::Ready(r) => r,
            _ => panic!("the retry's execute did not reach Ready"),
        };
        match ready
            .seal(store)
            .unwrap()
            .confirm(f.conn, store, Tier::Integrity)
            .unwrap()
        {
            ConfirmOutcome::Sealed(s) => s,
            _ => panic!("the retry did not seal"),
        }
    }

    /// Issue #401: after an END OF TAPE abort, the same volume is written
    /// again from the beginning and seals. The old attempt's rows used to
    /// make that second `plan` fail on `writes`' `UNIQUE(stage_set_id,
    /// volume_id)`, wedging the label. The superseded rows are gone, the
    /// event says which went, and the old attempt's session directory is
    /// removed with them.
    #[test]
    fn an_enospc_abort_is_written_again_on_the_same_volume_and_seals() {
        let f = make_fixture();
        let first_dir = f.built.session_dir.clone();
        let (second, _second_dir) = f.rebuild();
        let mut short = MemStore::new(BS as usize).with_enospc_after(2 * BS);
        let planned = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut short)
            .unwrap()
            .plan(&f.conn, f.volume_id, &f.units)
            .unwrap();
        assert!(matches!(
            planned.execute(&f.conn, &mut short).unwrap(),
            ExecuteOutcome::Aborted(_)
        ));
        assert_eq!(statuses_on(&f.conn, f.volume_id), vec!["aborted"]);
        assert!(
            first_dir.is_dir(),
            "positive control: the first attempt's dir exists"
        );

        // The same cartridge reloaded: what the first attempt left on it,
        // without the simulated end of tape.
        let mut store = MemStore::new(BS as usize);
        store.files = short.files.clone();
        store.syncs = short.syncs.clone();
        store.reposition_for_resume(0).unwrap();
        run_to_sealed(retry_of!(f), second, &mut store);

        assert_eq!(
            statuses_on(&f.conn, f.volume_id),
            vec!["completed"],
            "the aborted attempt's row was superseded, not left beside the new one"
        );
        let orphan_positions: i64 = f
            .conn
            .query_row(
                "SELECT COUNT(*) FROM write_positions
                 WHERE write_id NOT IN (SELECT id FROM writes)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphan_positions, 0);
        let detail: String = f
            .conn
            .query_row(
                "SELECT details FROM events WHERE action = 'write_session_superseded'
                 AND entity_id = ?1",
                params![f.volume_id],
                |r| r.get(0),
            )
            .expect("the supersession is recorded");
        assert!(detail.contains("aborted"), "{detail}");
        assert!(
            !first_dir.exists(),
            "the superseded attempt's session dir is removed: {}",
            first_dir.display()
        );
    }

    /// Issue #401: after a tri-layer L2 abort (a staged slice rotted after
    /// plan), the slice is put right and the same volume is written again
    /// from the beginning — the troubleshooting guide's recipe.
    #[test]
    fn an_l2_abort_is_written_again_on_the_same_volume_once_the_slice_is_good() {
        let f = make_fixture();
        let (second, _second_dir) = f.rebuild();
        let mut store = MemStore::new(BS as usize);
        let planned = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap()
            .plan(&f.conn, f.volume_id, &f.units)
            .unwrap();
        let slice_path = f.units[0].slices[0].staging_path.clone();
        let good = std::fs::read(&slice_path).unwrap();
        rot_in_place(&slice_path);
        assert!(matches!(
            planned.execute(&f.conn, &mut store).unwrap(),
            ExecuteOutcome::Aborted(_)
        ));
        std::fs::write(&slice_path, &good).unwrap();

        store.reposition_for_resume(0).unwrap();
        run_to_sealed(retry_of!(f), second, &mut store);
        assert_eq!(statuses_on(&f.conn, f.volume_id), vec!["completed"]);
    }

    /// Issue #401: a session killed between plan and execute leaves
    /// `planned` rows; `volume abort` turns them `aborted`, and the next
    /// attempt plans and seals — what `volume resume`'s refusal tells the
    /// operator to do.
    #[test]
    fn a_plan_cleared_by_volume_abort_does_not_block_the_next_attempt() {
        let f = make_fixture();
        let (second, _second_dir) = f.rebuild();
        let mut store = MemStore::new(BS as usize);
        let _planned = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap()
            .plan(&f.conn, f.volume_id, &f.units)
            .unwrap();
        // The kill, then `volume abort`'s own UPDATE.
        f.conn
            .execute(
                "UPDATE writes SET status = 'aborted' WHERE volume_id = ?1",
                params![f.volume_id],
            )
            .unwrap();

        run_to_sealed(retry_of!(f), second, &mut store);
        assert_eq!(statuses_on(&f.conn, f.volume_id), vec!["completed"]);
    }

    /// Issue #401: `plan` is one transaction. A failure after the first
    /// `writes` row used to leave it `planned`, and that stray row then
    /// refused every later `volume write` as an unresolved session.
    #[test]
    fn a_plan_that_fails_partway_leaves_no_rows() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);
        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
        // A unit that owns none of the Layout's slices: the first INSERT
        // lands, then the position loop fails.
        let mut unit = f.units[0].clone();
        unit.slices.clear();
        let err = match validated.plan(&f.conn, f.volume_id, &[unit]) {
            Ok(_) => panic!("plan must fail when no unit owns a slice"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("owns staged slice"), "{err}");
        let rows: i64 = f
            .conn
            .query_row("SELECT COUNT(*) FROM writes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "a failed plan leaves no `writes` row behind");
    }

    /// Issue #401's limit: a volume whose seal is recorded keeps its
    /// `aborted` rows — they are the session `volume resume` may adopt for
    /// re-confirmation (ADR-0012 2026-09-23) — so `plan` touches nothing and
    /// still fails on the constraint, as `volume write` refuses such a
    /// volume long before it would get here.
    #[test]
    fn a_volume_whose_seal_is_recorded_keeps_its_aborted_rows() {
        let f = make_fixture();
        let (second, _second_dir) = f.rebuild();
        let mut store = MemStore::new(BS as usize).with_enospc_after(2 * BS);
        let planned = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap()
            .plan(&f.conn, f.volume_id, &f.units)
            .unwrap();
        let _ = planned.execute(&f.conn, &mut store).unwrap();
        f.conn
            .execute(
                "UPDATE volumes SET sealed_at = datetime('now') WHERE id = ?1",
                params![f.volume_id],
            )
            .unwrap();
        let mut fresh = MemStore::new(BS as usize);
        let result = second
            .into_validated(&f.keys, SliceCheck::Size, &mut fresh)
            .unwrap()
            .plan(&f.conn, f.volume_id, &f.units);
        assert!(result.is_err(), "a sealed volume is never planned over");
        assert_eq!(statuses_on(&f.conn, f.volume_id), vec!["aborted"]);
    }

    // --- behavior 4: SIGINT between entries -> Interrupted + resumable ---

    #[test]
    fn sigint_between_entries_interrupts_and_resume_completes_the_session() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);

        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
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
            .resume_checking(&f.conn, &f.keys, SliceCheck::Size, &mut store, || false)
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

    /// A fresh session of a fixture's `built`, interrupted between entries
    /// after slice_1 — the SIGINT test's shape: 8 files recorded (id_thunk
    /// .. slice_1), slice_1 `written`, slice_2 `pending`, rows
    /// `interrupted`. For the resume tests below.
    fn interrupt_after_first_slice(
        built: BuiltLayout,
        conn: &Connection,
        keys: &KeyAvailability,
        units: &[BuildUnit],
        volume_id: i64,
    ) -> (InterruptedSession, MemStore) {
        let mut store = MemStore::new(BS as usize);
        let planned = built
            .into_validated(keys, SliceCheck::Size, &mut store)
            .unwrap()
            .plan(conn, volume_id, units)
            .unwrap();
        let calls = AtomicU32::new(0);
        let is_interrupted = move || calls.fetch_add(1, Ordering::SeqCst) >= 8;
        match planned
            .execute_checking(conn, &mut store, is_interrupted)
            .unwrap()
        {
            ExecuteOutcome::Interrupted(i) => {
                assert_eq!(store.files.len(), 8);
                (i, store)
            }
            _ => panic!("expected Interrupted"),
        }
    }

    /// Issue #400: resume over a tape whose File 0 read fails and which is
    /// NOT provably blank — here the session's own tape, eight files
    /// recorded, File 0 unreadable as a drive needing cleaning makes it.
    /// The read error used to count as a blank tape (`ContactOutcome::Blank`),
    /// which is consent to write: resume repositioned and wrote on. Now it
    /// refuses, writes nothing, and leaves the session `interrupted`.
    #[test]
    fn resume_refuses_an_unreadable_file_zero_on_a_tape_not_provably_blank() {
        use crate::tape::fake::{FakeTape, Op};
        let f = make_fixture();
        let (interrupted, mem) =
            interrupt_after_first_slice(f.built, &f.conn, &f.keys, &f.units, f.volume_id);
        let fake = FakeTape::with_files(mem.files.clone(), BS as usize);
        fake.state().unreadable.push(0);
        let mut store = crate::store::TapeStore::from_ops(fake.boxed(), u64::MAX).unwrap();

        let err = match interrupted.resume_checking(
            &f.conn,
            &f.keys,
            SliceCheck::Size,
            &mut store,
            || false,
        ) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("resume must refuse an unreadable File 0 on a recorded tape"),
        };
        assert!(err.contains("could not be read"), "{err}");
        assert!(
            !fake.ops().iter().any(|op| matches!(op, Op::Write(_))),
            "nothing written: {:?}",
            fake.ops()
        );
        assert_eq!(
            fake.state().files.len(),
            8,
            "the tape is as the session left it"
        );
        assert_eq!(statuses_on(&f.conn, f.volume_id), vec!["interrupted"]);
    }

    /// Issue #403: the catalog recorded slice_1 `written` (8 files), but
    /// the tape holds 7 — the immediate filemark after slice_1 was still in
    /// the drive's buffer when the power went. Resume used to rewind and
    /// space 8 filemarks blind, fail at end of data, and leave abort and a
    /// full rewrite as the only exit. Now it resumes from the medium's 7,
    /// sets slice_1 back to `pending`, writes it again, and seals.
    #[test]
    fn resume_continues_from_the_medium_when_the_tape_is_behind_the_catalog() {
        use crate::tape::fake::FakeTape;
        let f = make_fixture();
        let (interrupted, mem) =
            interrupt_after_first_slice(f.built, &f.conn, &f.keys, &f.units, f.volume_id);
        let mut files = mem.files.clone();
        files.pop(); // slice_1 never reached the tape
        let fake = FakeTape::with_files(files, BS as usize);
        let mut store = crate::store::TapeStore::from_ops(fake.boxed(), u64::MAX).unwrap();

        let ready = match interrupted
            .resume_checking(&f.conn, &f.keys, SliceCheck::Size, &mut store, || false)
            .expect("resume continues from what the tape holds")
        {
            ResumeOutcome::Ready(r) => r,
            _ => panic!("expected Ready"),
        };
        match ready
            .seal(&mut store)
            .unwrap()
            .confirm(&f.conn, &mut store, Tier::Integrity)
            .unwrap()
        {
            ConfirmOutcome::Sealed(_) => {}
            ConfirmOutcome::Inconclusive(i) => panic!("inconclusive: {:?}", i.evidence.mismatches),
            ConfirmOutcome::Quarantined(q) => panic!("quarantined: {:?}", q.reason),
        }
        assert_eq!(fake.state().files.len(), 10, "9 content files and the seal");
        assert_eq!(statuses_on(&f.conn, f.volume_id), vec!["completed"]);
        let written: i64 = f
            .conn
            .query_row(
                "SELECT COUNT(*) FROM write_positions WHERE status = 'written'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(written, 2);
    }

    /// Issue #403: a resume that re-enters confirm on a tape sealed by this
    /// session locates the seal ONCE. It used to read the seal position at
    /// the tape's own pointer, again at the caller's identical position,
    /// again in `resume_reconfirm_eligible`, and again at the start of the
    /// chain walk — four long locates to the end of the tape.
    #[test]
    fn a_reconfirming_resume_reads_the_seal_position_once() {
        use crate::tape::fake::{FakeTape, Op};
        let f = make_fixture();
        let seal_pos = f
            .built
            .layout
            .entries
            .iter()
            .find(|e| matches!(e.kind, ZoneKind::SealMarker))
            .unwrap()
            .position as u32;
        let mut mem = MemStore::new(BS as usize);
        let planned = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut mem)
            .unwrap()
            .plan(&f.conn, f.volume_id, &f.units)
            .unwrap();
        let ready = match planned.execute(&f.conn, &mut mem).unwrap() {
            ExecuteOutcome::Ready(r) => r,
            _ => panic!("expected Ready"),
        };
        // Sealed, then the process died before confirm.
        let _ = ready.seal(&mut mem).unwrap();
        f.conn
            .execute(
                "UPDATE volumes SET sealed_at = datetime('now') WHERE id = ?1",
                params![f.volume_id],
            )
            .unwrap();
        f.conn
            .execute(
                "UPDATE writes SET status = 'interrupted' WHERE volume_id = ?1",
                params![f.volume_id],
            )
            .unwrap();
        let interrupted = InterruptedSession::rehydrate(&f.conn, f.volume_id)
            .unwrap()
            .expect("the interrupted session rehydrates");

        let fake = FakeTape::with_files(mem.files.clone(), BS as usize);
        let mut store = crate::store::TapeStore::from_ops(fake.boxed(), 0).unwrap();
        fake.clear_ops();
        let pending = match interrupted
            .resume_checking(&f.conn, &f.keys, SliceCheck::Size, &mut store, || false)
            .unwrap()
        {
            ResumeOutcome::Confirming(p) => p,
            _ => panic!("expected Confirming"),
        };
        match pending
            .confirm(&f.conn, &mut store, Tier::Integrity)
            .unwrap()
        {
            ConfirmOutcome::Sealed(_) => {}
            _ => panic!("expected Sealed"),
        }
        let seal_reads = fake
            .ops()
            .iter()
            .filter(|op| matches!(op, Op::Read(p) | Op::ReadHead(p) if *p == seal_pos))
            .count();
        assert_eq!(seal_reads, 1, "ops: {:?}", fake.ops());
    }

    /// Issue #400 at the contact check itself, through the real
    /// `TapeStore`: File 0 unreadable on a recorded tape is
    /// `FileZeroUnreadable` — it used to be `Blank`.
    #[test]
    fn check_tape_contact_an_unreadable_file_zero_on_a_recorded_tape_is_not_blank() {
        use crate::tape::fake::FakeTape;
        let fake = FakeTape::with_files(vec![vec![7u8; BS as usize]; 3], BS as usize);
        fake.state().unreadable.push(0);
        let mut store = crate::store::TapeStore::from_ops(fake.boxed(), 0).unwrap();
        match check_tape_contact(&mut store, "L", "U", None) {
            ContactOutcome::FileZeroUnreadable { error } => {
                assert!(error.contains("Input/output error"), "{error}")
            }
            other => panic!("expected FileZeroUnreadable, got {other:?}"),
        }
    }

    /// The positive control: a blank tape — end of data at BOT, so st fails
    /// the File 0 read with EIO and a forward space with BLANK CHECK at file
    /// 0 — is `Blank`, through the same real `TapeStore` probe.
    #[test]
    fn check_tape_contact_a_blank_tape_is_proved_blank_by_the_medium() {
        use crate::tape::fake::FakeTape;
        let fake = FakeTape::with_files(Vec::new(), BS as usize);
        let mut store = crate::store::TapeStore::from_ops(fake.boxed(), 0).unwrap();
        assert_eq!(
            check_tape_contact(&mut store, "L", "U", Some(9)),
            ContactOutcome::Blank
        );
    }

    /// Issue #400, bounded reads: a File 0 far larger than any ID thunk (the
    /// fill script leaves 2.5 TB of it) is read no further than
    /// `SMALL_FILE_CAP` and refused as not an ID thunk. It used to be read
    /// whole into a `Vec`.
    #[test]
    fn check_tape_contact_reads_an_oversized_file_zero_no_further_than_the_cap() {
        use crate::tape::fake::FakeTape;
        let cap = crate::store::SMALL_FILE_CAP as usize;
        let big = vec![b'x'; cap + 8 * BS as usize];
        let fake = FakeTape::with_files(vec![big], BS as usize);
        let mut store = crate::store::TapeStore::from_ops(fake.boxed(), 0).unwrap();
        assert_eq!(
            check_tape_contact(&mut store, "L", "U", None),
            ContactOutcome::IdentityMismatch { found: None }
        );
        let read = fake.state().bytes_read;
        assert!(
            read <= (cap + BS as usize) as u64,
            "read {read} bytes of a {}-byte File 0",
            cap + 8 * BS as usize
        );
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

        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
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
            .resume_checking(&f.conn, &f.keys, SliceCheck::Size, &mut store, || false)
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

        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
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
            .resume_checking(&f.conn, &f.keys, SliceCheck::Size, &mut store, || false)
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

        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
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
            .resume_checking(&f.conn, &f.keys, SliceCheck::Size, &mut store, || false)
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

        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
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
            .resume_checking(&f.conn, &f.keys, SliceCheck::Size, &mut store, || false)
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

        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
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
            .resume_checking(&f.conn, &f.keys, SliceCheck::Size, &mut store, || false)
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

        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
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
            .resume_checking(&f.conn, &f.keys, SliceCheck::Size, &mut store, || false)
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

        // Issue #324: resume's divergence arms record their abort in the
        // same act as the status change — exactly one `write_quarantined`
        // row, so `adopt_aborted` has a recorded abort to be later than.
        assert_eq!(quarantine_event_count(&f.conn, f.volume_id), 1);
        assert!(recorded_abort_time(&f.conn, f.volume_id).unwrap().is_some());
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

        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
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

        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
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

        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
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
            .resume_checking(&f.conn, &f.keys, SliceCheck::Size, &mut store, || false)
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
    // `ContactFacts::probe_seal`, which cannot distinguish "no marker here"
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
    /// from "never sealed" (`ContactFacts::probe_seal` returns `false` for a
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

        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut inner)
            .unwrap();
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
            .resume_checking(&f.conn, &f.keys, SliceCheck::Size, &mut store, || false)
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

        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut inner)
            .unwrap();
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
            .resume_checking(&f.conn, &f.keys, SliceCheck::Size, &mut store, || false)
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

        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
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
            .resume_checking(&f.conn, &f.keys, SliceCheck::Size, &mut store, || false)
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

    /// The tape a session left, on the in-memory tape that has the st
    /// driver's open semantics, write-protected and opened `mode` — what
    /// `volume resume` gets from `TapeStore::open_as` on a sealed tape
    /// shelved with its tab set (issue #407).
    fn protected_drive(
        files: &[Vec<u8>],
        mode: crate::store::OpenMode,
    ) -> (
        crate::store::TapeStore,
        crate::tape::fake::FakeTape,
        crate::store::injected::InjectedDrive,
    ) {
        let fake = crate::tape::fake::FakeTape::with_files(files.to_vec(), BS as usize);
        fake.write_protect();
        let drive = crate::store::injected::InjectedDrive::install(&fake);
        let store = crate::store::TapeStore::open_as(
            "/nonexistent/tapectl-resume-nst",
            BS as usize,
            mode,
            0,
        )
        .expect("a read-only open of a protected tape succeeds");
        (store, fake, drive)
    }

    /// Issue #407: a session whose seal is RECORDED is `confirm_only`, and
    /// `volume resume` opens the drive read-only on that answer. Its resume
    /// really does run on a read-only drive: over a write-protected tape
    /// opened read-only, it re-enters confirm and the confirm passes —
    /// nothing tried to write (the fake refuses a write through a read-only
    /// open with EBADF, as st does).
    #[test]
    fn a_resume_whose_seal_is_recorded_is_confirm_only_and_runs_read_only() {
        use crate::store::OpenMode;

        let f = make_fixture();
        let mut inner = MemStore::new(BS as usize);
        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut inner)
            .unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let ready = match planned
            .execute_checking(&f.conn, &mut inner, || false)
            .unwrap()
        {
            ExecuteOutcome::Ready(r) => r,
            _ => panic!("expected Ready on a happy-path MemStore run"),
        };
        let sealed_pending = ready.seal(&mut inner).expect("seal should succeed");
        // `write::finish_session`'s record of the seal, then the crash.
        f.conn
            .execute(
                "UPDATE volumes SET sealed_at = datetime('now') WHERE id = ?1",
                params![f.volume_id],
            )
            .unwrap();
        mark_writes(&f.conn, &sealed_pending.write_ids, "interrupted").unwrap();

        let interrupted = InterruptedSession::rehydrate(&f.conn, f.volume_id)
            .unwrap()
            .expect("resumable");
        assert!(interrupted.confirm_only(&f.conn).unwrap());

        let (mut store, fake, _drive) = protected_drive(&inner.files, OpenMode::ReadOnly);
        let pending = match interrupted
            .resume_checking(&f.conn, &f.keys, SliceCheck::Size, &mut store, || false)
            .expect("a confirm-only resume runs on a read-only drive")
        {
            ResumeOutcome::Confirming(p) => p,
            ResumeOutcome::Ready(_) => panic!("expected Confirming, got Ready"),
            ResumeOutcome::Quarantined(q) => panic!("expected Confirming: {:?}", q.reason),
            ResumeOutcome::Interrupted(_) => panic!("expected Confirming, got Interrupted"),
            ResumeOutcome::Aborted(a) => panic!("expected Confirming, got Aborted: {}", a.reason),
        };
        match pending
            .confirm(&f.conn, &mut store, Tier::Integrity)
            .expect("confirm should not error")
        {
            ConfirmOutcome::Sealed(s) => assert_eq!(s.label, "SESSTEST"),
            ConfirmOutcome::Quarantined(q) => panic!("expected Sealed: {:?}", q.reason),
            ConfirmOutcome::Inconclusive(inc) => {
                panic!("expected Sealed: {:?}", inc.evidence.mismatches)
            }
        }
        assert_eq!(fake.opens(), vec![OpenMode::ReadOnly]);
    }

    /// The control for the test above: a session that still OWES its seal
    /// is not `confirm_only` — its resume goes on to `seal()`, which writes —
    /// and on a read-only drive that write is refused. So the open mode
    /// `volume resume` picks from `confirm_only` is load-bearing, and the
    /// fake's read-only refusal is real.
    #[test]
    fn a_resume_that_still_owes_its_seal_is_not_confirm_only() {
        use crate::store::OpenMode;

        let f = make_fixture();
        let mut inner = MemStore::new(BS as usize);
        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut inner)
            .unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let ready = match planned
            .execute_checking(&f.conn, &mut inner, || false)
            .unwrap()
        {
            ExecuteOutcome::Ready(r) => r,
            _ => panic!("expected Ready on a happy-path MemStore run"),
        };
        mark_writes(&f.conn, &ready.write_ids, "interrupted").unwrap();

        let interrupted = InterruptedSession::rehydrate(&f.conn, f.volume_id)
            .unwrap()
            .expect("resumable");
        assert!(!interrupted.confirm_only(&f.conn).unwrap());

        let (mut store, _fake, _drive) = protected_drive(&inner.files, OpenMode::ReadOnly);
        let ready_again = match interrupted
            .resume_checking(&f.conn, &f.keys, SliceCheck::Size, &mut store, || false)
            .expect("resume reaches the seal it owes")
        {
            ResumeOutcome::Ready(r) => r,
            _ => panic!("expected Ready: the seal is still owed"),
        };
        let err = ready_again
            .seal(&mut store)
            .err()
            .expect("a seal through a read-only open must fail")
            .to_string();
        assert!(err.contains("Bad file descriptor"), "{err}");
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

        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
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

    /// Issue #327: File 0 present with ZERO bytes — a filemark at BOT.
    /// `TapeStore::read_file` returns `Ok(0)` for it (the first read hits
    /// the filemark), which `MemStore` models as an empty recorded file. It
    /// is its own finding, neither `Blank` (a blank tape's File 0 read
    /// FAILS) nor an unparseable `IdentityMismatch`.
    #[test]
    fn check_tape_contact_empty_file_zero_is_its_own_finding() {
        for seal_position in [None, Some(5)] {
            let mut store = MemStore::new(BS as usize);
            put_file(&mut store, 0, Vec::new());
            let outcome =
                check_tape_contact(&mut store, CONTACT_LABEL, CONTACT_UUID, seal_position);
            assert_eq!(outcome, ContactOutcome::EmptyFileZero, "{seal_position:?}");
        }
    }

    /// Issue #327's positive control: a File 0 of zero-FILLED bytes is not
    /// empty — it has bytes, and they do not parse. It stays the
    /// unparseable `IdentityMismatch`, so the empty check keys on length,
    /// not on content.
    #[test]
    fn check_tape_contact_zero_filled_file_zero_is_unparseable_not_empty() {
        let mut store = MemStore::new(BS as usize);
        put_file(&mut store, 0, vec![0u8; BS as usize]);
        let outcome = check_tape_contact(&mut store, CONTACT_LABEL, CONTACT_UUID, None);
        assert_eq!(outcome, ContactOutcome::IdentityMismatch { found: None });
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

        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
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

        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
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

    // --- issue #280: resume adopts an aborted, sealed, cleared session -----
    //
    // ADR-0012's 2026-09-23 amendment (Option 2). The state: confirm's
    // Quarantined arm left `sealed_at` set, `observed_condition =
    // 'quarantined'`, `status = 'initialized'` and every `writes` row
    // `aborted`; a later clean FULL verify cleared the condition. Resume must
    // adopt it and re-enter confirm, never write or seal, and refuse naming
    // the first unmet condition otherwise.

    /// A fixture that has been through execute + seal, with its rows left by
    /// one of the two sealed-then-aborted paths. `store` is the (healed)
    /// tape as it now reads.
    struct SealedAborted {
        conn: Connection,
        keys: KeyAvailability,
        volume_id: i64,
        store: MemStore,
        _slices_dir: TempDir,
        _session_dir: TempDir,
    }

    const ADOPT_TEST_DEVICE: &str = "/nonexistent/tapectl-pm280-resume-test-nst";

    /// Execute and seal on a MemStore, recording `sealed_at` exactly as
    /// `write::finish_session` does, with the volume `initialized` as a real
    /// write target is. Returns the pending confirm and the slice position.
    fn sealed_pending_on_initialized_volume(f: Fixture) -> (SealedPending, SealedAborted, usize) {
        f.conn
            .execute(
                "UPDATE volumes SET status = 'initialized' WHERE id = ?1",
                params![f.volume_id],
            )
            .unwrap();
        let mut store = MemStore::new(BS as usize);
        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let ready = match planned
            .execute_checking(&f.conn, &mut store, || false)
            .unwrap()
        {
            ExecuteOutcome::Ready(r) => r,
            _ => panic!("expected Ready on a happy-path MemStore run"),
        };
        let sealed_pending = ready.seal(&mut store).expect("seal should succeed");
        f.conn
            .execute(
                "UPDATE volumes SET sealed_at = datetime('now') WHERE id = ?1",
                params![f.volume_id],
            )
            .unwrap();
        let slice_position = sealed_pending
            .built
            .layout
            .entries
            .iter()
            .position(|e| matches!(e.kind, ZoneKind::Slice { .. }))
            .expect("fixture layout always has at least one slice entry");
        (
            sealed_pending,
            SealedAborted {
                conn: f.conn,
                keys: f.keys,
                volume_id: f.volume_id,
                store,
                _slices_dir: f._slices_dir,
                _session_dir: f._session_dir,
            },
            slice_position,
        )
    }

    /// Move every abort-recording event of this volume `ago` into the past
    /// (an SQLite modifier such as `'-1 hour'`), so a verify recorded now is
    /// unambiguously later — `datetime('now')` has one-second resolution.
    fn backdate_abort_events(conn: &Connection, volume_id: i64, ago: &str) {
        let n = conn
            .execute(
                "UPDATE events SET timestamp = datetime('now', ?2)
                 WHERE entity_type = 'volume' AND entity_id = ?1
                   AND action IN ('write_aborted', 'write_quarantined')",
                params![volume_id, ago],
            )
            .unwrap();
        assert!(n > 0, "fixture premise: an abort event was recorded");
    }

    /// State (B) of issue #280: a medium-proving confirm failure
    /// (Quarantined — `writes` aborted, condition quarantined, and the
    /// `write_quarantined` event, all recorded by confirm itself since issue
    /// #324), an hour ago; then the tape reads clean again.
    fn quarantined_by_confirm() -> SealedAborted {
        let (pending, mut sa, slice_position) =
            sealed_pending_on_initialized_volume(make_fixture());
        let good = sa.store.files[slice_position].clone();
        sa.store.files[slice_position][0] ^= 0xFF;
        match pending
            .confirm(&sa.conn, &mut sa.store, Tier::Integrity)
            .unwrap()
        {
            ConfirmOutcome::Quarantined(_) => {}
            _ => panic!("fixture premise: a flipped content byte quarantines"),
        }
        // No explicit event write here (issue #324): confirm recorded it,
        // and `backdate_abort_events`'s `n > 0` is the positive control.
        backdate_abort_events(&sa.conn, sa.volume_id, "-1 hour");
        sa.store.files[slice_position] = good;
        sa
    }

    fn quarantine_event_count(conn: &Connection, volume_id: i64) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM events
             WHERE entity_type = 'volume' AND entity_id = ?1 AND action = 'write_quarantined'",
            params![volume_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// Issue #324: confirm's Quarantined arm records its own abort — the
    /// `write_quarantined` event lands with `writes.status = 'aborted'`,
    /// with no second call from the caller. Before the fix it was written
    /// later, by `write::log_quarantine`, so a crash between the two left
    /// `adopt_aborted` with no recorded abort to be later than, forever.
    #[test]
    fn confirm_quarantine_records_its_abort_event_with_the_status_change() {
        let (pending, mut sa, slice_position) =
            sealed_pending_on_initialized_volume(make_fixture());
        sa.store.files[slice_position][0] ^= 0xFF;
        let q = match pending
            .confirm(&sa.conn, &mut sa.store, Tier::Integrity)
            .unwrap()
        {
            ConfirmOutcome::Quarantined(q) => q,
            _ => panic!("fixture premise: a flipped content byte quarantines"),
        };

        // Exactly the time `adopt_aborted` reads, from nothing but confirm.
        assert!(
            recorded_abort_time(&sa.conn, sa.volume_id)
                .unwrap()
                .is_some(),
            "confirm's Quarantined arm must record the abort itself"
        );
        assert_eq!(quarantine_event_count(&sa.conn, sa.volume_id), 1);
        // The row shape `report events` renders (cli/report.rs pins it):
        // reason in `new_value`, no `field`, no `details`.
        let (field, new_value, details): (Option<String>, Option<String>, Option<String>) = sa
            .conn
            .query_row(
                "SELECT field, new_value, details FROM events
                 WHERE entity_id = ?1 AND action = 'write_quarantined'",
                params![sa.volume_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(field, None);
        assert_eq!(details, None);
        assert_eq!(
            new_value.as_deref(),
            Some(super::super::write::describe_quarantine(&q.reason).as_str())
        );
        // No event without its status, and no status without its event.
        let statuses: Vec<String> = sa
            .conn
            .prepare("SELECT status FROM writes WHERE volume_id = ?1")
            .unwrap()
            .query_map(params![sa.volume_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(!statuses.is_empty() && statuses.iter().all(|s| s == "aborted"));

        // And the report path the caller takes afterwards writes nothing
        // more: one abort, one row.
        let _ = super::super::write::quarantine_error(&q.label, &q.reason);
        assert_eq!(quarantine_event_count(&sa.conn, sa.volume_id), 1);
    }

    /// Issue #324, the atomicity half: if the event insert fails, neither
    /// the status change nor the event lands. The failure is injected with
    /// an SQLite trigger that aborts exactly the `write_quarantined` insert
    /// — no code seam — so a pre-fix confirm (status first, event never
    /// attempted inside it) leaves `aborted` rows and no event: red.
    #[test]
    fn confirm_quarantine_rolls_back_status_and_event_together() {
        let (pending, mut sa, slice_position) =
            sealed_pending_on_initialized_volume(make_fixture());
        let statuses_before: Vec<String> = sa
            .conn
            .prepare("SELECT status FROM writes WHERE volume_id = ?1 ORDER BY id")
            .unwrap()
            .query_map(params![sa.volume_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            !statuses_before.is_empty() && statuses_before.iter().all(|s| s != "aborted"),
            "fixture premise: {statuses_before:?}"
        );
        sa.conn
            .execute_batch(
                "CREATE TEMP TRIGGER inject_quarantine_event_failure
                 BEFORE INSERT ON events WHEN NEW.action = 'write_quarantined'
                 BEGIN SELECT RAISE(ABORT, 'injected: write_quarantined insert failed'); END;",
            )
            .unwrap();
        sa.store.files[slice_position][0] ^= 0xFF;

        let result = pending.confirm(&sa.conn, &mut sa.store, Tier::Integrity);
        assert!(
            result.is_err(),
            "the injected event failure must surface, not be swallowed"
        );

        let statuses_after: Vec<String> = sa
            .conn
            .prepare("SELECT status FROM writes WHERE volume_id = ?1 ORDER BY id")
            .unwrap()
            .query_map(params![sa.volume_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(statuses_after, statuses_before, "status must roll back");
        let condition: String = sa
            .conn
            .query_row(
                "SELECT observed_condition FROM volumes WHERE id = ?1",
                params![sa.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(condition, "ok", "condition must roll back with the status");
        assert_eq!(quarantine_event_count(&sa.conn, sa.volume_id), 0);
    }

    /// The other sealed-then-aborted path: an Inconclusive confirm (rows
    /// `interrupted`, condition never touched), then the operator's own
    /// `volume abort` — an hour ago. The tape reads clean again.
    fn operator_aborted_after_inconclusive() -> SealedAborted {
        let (pending, mut sa, slice_position) =
            sealed_pending_on_initialized_volume(make_fixture());
        let good = sa.store.files[slice_position].clone();
        sa.store.files[slice_position].truncate(5);
        match pending
            .confirm(&sa.conn, &mut sa.store, Tier::Integrity)
            .unwrap()
        {
            ConfirmOutcome::Inconclusive(_) => {}
            _ => panic!("fixture premise: a short read is Inconclusive"),
        }
        super::super::write::volume_abort(&sa.conn, "SESSTEST", true).unwrap();
        backdate_abort_events(&sa.conn, sa.volume_id, "-1 hour");
        sa.store.files[slice_position] = good;
        sa
    }

    /// A clean FULL verify, through the production verify path
    /// (`write::volume_verify_with_store`), recorded now.
    fn run_clean_full_verify(sa: &mut SealedAborted) {
        let config = crate::config::Config::default();
        let report = super::super::write::volume_verify_with_store(
            &sa.conn,
            &mut sa.store,
            "SESSTEST",
            sa.volume_id,
            BS as usize,
            Tier::Integrity,
            crate::tape::contact::ContactSite::new(
                &config,
                crate::tape::contact::Operation::VolumeVerify,
                ADOPT_TEST_DEVICE,
                crate::tape::contact::Medium::NoBackend,
            ),
        )
        .expect("verify should run against the MemStore");
        assert_eq!(
            report.failed, 0,
            "fixture premise: the healed tape verifies clean"
        );
    }

    fn insert_verify(sa: &SealedAborted, verify_type: &str, outcome: &str, started: &str) {
        sa.conn
            .execute(
                "INSERT INTO verification_sessions
                    (volume_id, started_at, completed_at, verify_type, outcome)
                 VALUES (?1, datetime('now', ?2), datetime('now', ?2), ?3, ?4)",
                params![sa.volume_id, started, verify_type, outcome],
            )
            .unwrap();
    }

    fn set_condition_ok(sa: &SealedAborted) {
        sa.conn
            .execute(
                "UPDATE volumes SET observed_condition = 'ok' WHERE id = ?1",
                params![sa.volume_id],
            )
            .unwrap();
    }

    /// THE headline: state (B), cleared by a clean full verify recorded after
    /// the abort, is adopted by resume, re-confirmed WITHOUT a single write
    /// or reposition (`NoWriteStore` panics on either), and a passing confirm
    /// — the only writer of `status = 'sealed'` — makes it count as a copy.
    ///
    /// Paired with the clean-clear message: on this exact fixture state the
    /// message's `resume_would_reconfirm` input is true and it names
    /// `tapectl volume resume SESSTEST`, and the resume it names succeeds.
    #[test]
    fn resume_adopts_an_aborted_sealed_session_after_a_clean_full_verify_and_seals_it() {
        let mut sa = quarantined_by_confirm();
        run_clean_full_verify(&mut sa);

        let condition: String = sa
            .conn
            .query_row(
                "SELECT observed_condition FROM volumes WHERE id = ?1",
                params![sa.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            condition, "ok",
            "the clean full verify cleared the condition"
        );
        assert!(
            !crate::policy::coverage::counts_as_copy(&sa.conn, "SESSTEST").unwrap(),
            "precondition: not a copy until confirm passes"
        );
        assert_eq!(
            resume_admission(&sa.conn, sa.volume_id).unwrap(),
            ResumeAdmission::Aborted(AbortedAdoption::Adoptable)
        );
        assert!(resume_would_reconfirm(&sa.conn, sa.volume_id).unwrap());
        let msg = crate::cli::volume::clean_clear_message(
            "SESSTEST",
            "quarantined",
            "initialized",
            true,
            false,
            resume_would_reconfirm(&sa.conn, sa.volume_id).unwrap(),
        );
        assert!(
            msg.contains("tapectl volume resume SESSTEST"),
            "the clean-clear message must name the resume that works: {msg}"
        );

        // `rehydrate` is unchanged: it still adopts only `interrupted` rows.
        assert!(InterruptedSession::rehydrate(&sa.conn, sa.volume_id)
            .unwrap()
            .is_none());
        let adopted = InterruptedSession::adopt_aborted(&sa.conn, sa.volume_id)
            .unwrap()
            .expect("an aborted, sealed, cleared session must be adoptable");

        let tape_before = sa.store.files.clone();
        let mut store = NoWriteStore(std::mem::replace(&mut sa.store, MemStore::new(BS as usize)));
        let pending = match adopted
            .resume_checking(&sa.conn, &sa.keys, SliceCheck::Size, &mut store, || false)
            .expect("resume must not error, and must not write via NoWriteStore")
        {
            ResumeOutcome::Confirming(p) => p,
            ResumeOutcome::Ready(_) => panic!("an adopted sealed session must never reach Ready"),
            ResumeOutcome::Quarantined(q) => panic!("expected Confirming, got {:?}", q.reason),
            ResumeOutcome::Interrupted(_) => panic!("expected Confirming, got Interrupted"),
            ResumeOutcome::Aborted(a) => panic!("expected Confirming, got Aborted: {}", a.reason),
        };
        match pending
            .confirm(&sa.conn, &mut store, Tier::Integrity)
            .expect("confirm must not error")
        {
            ConfirmOutcome::Sealed(s) => assert_eq!(s.label, "SESSTEST"),
            ConfirmOutcome::Quarantined(q) => panic!("expected Sealed, got {:?}", q.reason),
            ConfirmOutcome::Inconclusive(i) => {
                panic!(
                    "expected Sealed, got Inconclusive: {:?}",
                    i.evidence.mismatches
                )
            }
        }
        assert_eq!(
            store.0.files, tape_before,
            "the tape must be byte-identical: nothing written, nothing re-sealed"
        );

        let status: String = sa
            .conn
            .query_row(
                "SELECT status FROM volumes WHERE id = ?1",
                params![sa.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "sealed");
        let rows: Vec<(String, Option<String>)> = sa
            .conn
            .prepare("SELECT status, completed_at FROM writes WHERE volume_id = ?1")
            .unwrap()
            .query_map(params![sa.volume_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(!rows.is_empty());
        assert!(
            rows.iter().all(|(s, c)| s == "completed" && c.is_some()),
            "a passing confirm leaves the rows exactly as a normal one does: {rows:?}"
        );
        assert!(
            crate::policy::coverage::counts_as_copy(&sa.conn, "SESSTEST").unwrap(),
            "after the passing confirm the volume counts as a copy"
        );
    }

    /// An adopted session whose frozen inputs are missing fails
    /// revalidation without writing, and the message must not claim the
    /// rows are `interrupted` or name `volume abort` (which refuses an
    /// already-aborted session) — the #292 shape.
    #[test]
    fn adopted_session_revalidation_failure_names_no_refusing_command() {
        let mut sa = quarantined_by_confirm();
        run_clean_full_verify(&mut sa);
        let adopted = InterruptedSession::adopt_aborted(&sa.conn, sa.volume_id)
            .unwrap()
            .expect("fixture premise: adoptable");
        // The operator's `staging clean --force`: the frozen slices are gone.
        let slice = adopted
            .built
            .layout
            .entries
            .iter()
            .find(|e| matches!(e.kind, ZoneKind::Slice { .. }))
            .and_then(|e| match &e.source {
                ContentSource::Staged(p) => Some(p.clone()),
                _ => None,
            })
            .expect("fixture layout carries a staged slice");
        std::fs::remove_file(&slice).unwrap();

        let mut store = NoWriteStore(std::mem::replace(&mut sa.store, MemStore::new(BS as usize)));
        let msg =
            match adopted
                .resume_checking(&sa.conn, &sa.keys, SliceCheck::Size, &mut store, || false)
            {
                Err(e) => e.to_string(),
                Ok(_) => panic!("revalidation must fail with a staged slice gone"),
            };
        assert!(msg.contains("revalidation failed"), "{msg}");
        assert!(!msg.contains("volume abort"), "{msg}");
        assert!(!msg.contains("`interrupted` state"), "{msg}");
        assert!(msg.contains("stay `aborted`"), "{msg}");
    }

    /// Condition 1: the seal is RECORDED, never inferred. With `sealed_at`
    /// NULL, everything else in place, there is nothing sealed to re-confirm.
    #[test]
    fn adoption_refused_when_the_seal_is_not_recorded() {
        let mut sa = quarantined_by_confirm();
        run_clean_full_verify(&mut sa);
        sa.conn
            .execute(
                "UPDATE volumes SET sealed_at = NULL WHERE id = ?1",
                params![sa.volume_id],
            )
            .unwrap();
        assert_eq!(
            aborted_adoption(&sa.conn, sa.volume_id).unwrap(),
            AbortedAdoption::NotSealed
        );
        assert!(InterruptedSession::adopt_aborted(&sa.conn, sa.volume_id)
            .unwrap()
            .is_none());
        assert!(!resume_would_reconfirm(&sa.conn, sa.volume_id).unwrap());
    }

    /// Condition 2, first half: still quarantined (no verify has cleared it).
    #[test]
    fn adoption_refused_while_the_condition_is_still_quarantined() {
        let sa = quarantined_by_confirm();
        assert_eq!(
            aborted_adoption(&sa.conn, sa.volume_id).unwrap(),
            AbortedAdoption::ConditionNotOk {
                condition: "quarantined".to_string()
            }
        );
        assert!(InterruptedSession::adopt_aborted(&sa.conn, sa.volume_id)
            .unwrap()
            .is_none());
    }

    /// Condition 2: `observed_condition = 'ok'` alone is not evidence. An
    /// operator's `volume abort` of a sealed Inconclusive session leaves the
    /// condition `ok` without any verify having cleared anything.
    #[test]
    fn adoption_refused_for_an_operator_abort_with_no_later_verify() {
        let sa = operator_aborted_after_inconclusive();
        assert!(matches!(
            aborted_adoption(&sa.conn, sa.volume_id).unwrap(),
            AbortedAdoption::NoCleanFullVerifyAfterAbort { .. }
        ));
        assert!(InterruptedSession::adopt_aborted(&sa.conn, sa.volume_id)
            .unwrap()
            .is_none());
        assert!(!resume_would_reconfirm(&sa.conn, sa.volume_id).unwrap());
    }

    /// A passing full verify recorded BEFORE the abort is not evidence about
    /// the medium after it.
    #[test]
    fn adoption_refused_when_the_passing_verify_predates_the_abort() {
        let sa = operator_aborted_after_inconclusive();
        insert_verify(&sa, "full", "passed", "-2 hours");
        assert!(matches!(
            aborted_adoption(&sa.conn, sa.volume_id).unwrap(),
            AbortedAdoption::NoCleanFullVerifyAfterAbort { .. }
        ));
    }

    /// A quick verify checks the map, not the bytes (the 2026-09-18
    /// amendment), so it never counts.
    #[test]
    fn adoption_refused_when_the_later_verify_is_only_quick() {
        let sa = quarantined_by_confirm();
        set_condition_ok(&sa);
        insert_verify(&sa, "quick", "passed", "+0 seconds");
        assert!(matches!(
            aborted_adoption(&sa.conn, sa.volume_id).unwrap(),
            AbortedAdoption::NoCleanFullVerifyAfterAbort { .. }
        ));
    }

    /// A failed full verify after the abort is not a clean one.
    #[test]
    fn adoption_refused_when_the_later_full_verify_failed() {
        let sa = quarantined_by_confirm();
        set_condition_ok(&sa);
        insert_verify(&sa, "full", "failed", "+0 seconds");
        assert!(matches!(
            aborted_adoption(&sa.conn, sa.volume_id).unwrap(),
            AbortedAdoption::NoCleanFullVerifyAfterAbort { .. }
        ));
    }

    /// With no recorded abort time there is nothing for the verify to be
    /// later than, and the answer is refused rather than inferred.
    #[test]
    fn adoption_refused_when_no_abort_is_recorded() {
        let mut sa = quarantined_by_confirm();
        run_clean_full_verify(&mut sa);
        sa.conn
            .execute(
                "DELETE FROM events WHERE action IN ('write_aborted', 'write_quarantined')",
                [],
            )
            .unwrap();
        assert_eq!(
            aborted_adoption(&sa.conn, sa.volume_id).unwrap(),
            AbortedAdoption::NoRecordedAbort
        );
    }

    /// Positive control for the refusals above: the SAME operator-abort
    /// fixture with a passing full verify strictly after the abort meets
    /// every condition the amendment states, so the refusals above are
    /// refused by the verify's timing/type/outcome and not by the fixture.
    #[test]
    fn operator_abort_then_a_later_clean_full_verify_is_adoptable() {
        let mut sa = operator_aborted_after_inconclusive();
        run_clean_full_verify(&mut sa);
        assert_eq!(
            aborted_adoption(&sa.conn, sa.volume_id).unwrap(),
            AbortedAdoption::Adoptable
        );
        assert!(InterruptedSession::adopt_aborted(&sa.conn, sa.volume_id)
            .unwrap()
            .is_some());
    }

    /// An aborted session that never sealed is still never adopted — the
    /// pre-#280 behaviour for every aborted session, kept for this one.
    #[test]
    fn an_unsealed_aborted_session_is_never_adopted() {
        let f = make_fixture();
        f.conn
            .execute(
                "UPDATE volumes SET status = 'initialized' WHERE id = ?1",
                params![f.volume_id],
            )
            .unwrap();
        let mut store = MemStore::new(BS as usize);
        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let _interrupted = planned
            .execute_checking(&f.conn, &mut store, || true)
            .unwrap();
        crate::volume::write::volume_abort(&f.conn, "SESSTEST", true).unwrap();
        assert_eq!(
            resume_admission(&f.conn, f.volume_id).unwrap(),
            ResumeAdmission::Aborted(AbortedAdoption::NotSealed)
        );
        assert!(InterruptedSession::adopt_aborted(&f.conn, f.volume_id)
            .unwrap()
            .is_none());
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

    /// Issue #357: the conflicting session directories are listed plainly,
    /// not as Rust's `{:?}` of a Vec (`["/a", "/b"]`).
    #[test]
    fn session_dirs_disagree_lists_the_directories_in_words() {
        let msg = InterruptedSession::session_dirs_disagree(
            7,
            "interrupted",
            &["/scratch/session-a", "/scratch/session-b"],
        )
        .to_string();
        assert!(
            msg.contains(
                "name 2 different session directories \
                 (/scratch/session-a, /scratch/session-b)"
            ),
            "{msg}"
        );
        assert!(
            !msg.contains('[') && !msg.contains("\"/scratch"),
            "no Debug rendering of the directory list: {msg}"
        );
    }

    // ── issue #390: the write pipeline under the execute loop ──

    /// Each `execute` call's outcome, in order, over a `MemStore` — and, on
    /// the call `fail_on` names, a store error after taking `take` bytes
    /// (a full medium mid-file).
    struct Recording {
        inner: MemStore,
        calls: Vec<std::result::Result<u64, String>>,
        fail_on: Option<(usize, usize)>,
        /// The error a `fail_on` call returns: a full medium unless set to
        /// a plain drive error (issue #408).
        fail_eio: bool,
    }

    impl Recording {
        fn new() -> Self {
            Self {
                inner: MemStore::new(BS as usize),
                calls: Vec::new(),
                fail_on: None,
                fail_eio: false,
            }
        }
    }

    impl Store for Recording {
        fn capacity(&mut self) -> Result<crate::store::CapacityReport> {
            self.inner.capacity()
        }
        fn execute(&mut self, src: &mut dyn std::io::Read, len: u64, sync: bool) -> Result<u64> {
            let result = match self.fail_on {
                Some((call, take)) if call == self.calls.len() => {
                    let mut some = vec![0u8; take];
                    src.read_exact(&mut some).unwrap();
                    Err(if self.fail_eio {
                        TapectlError::TapeIo("write: Input/output error (os error 5)".into())
                    } else {
                        TapectlError::MediumFull(
                            "write: No space left on device (os error 28)".into(),
                        )
                    })
                }
                _ => self.inner.execute(src, len, sync),
            };
            self.calls
                .push(result.as_ref().map(|n| *n).map_err(|e| e.to_string()));
            result
        }
        fn read_file(&mut self, position: u32, sink: &mut dyn Write) -> Result<u64> {
            self.inner.read_file(position, sink)
        }
        fn reposition_for_resume(&mut self, file_index: u32) -> Result<()> {
            self.inner.reposition_for_resume(file_index)
        }
    }

    fn slice_position(f: &Fixture, slice_id: i64) -> usize {
        f.built
            .layout
            .entries
            .iter()
            .position(|e| matches!(e.kind, ZoneKind::Slice { stage_slice_id } if stage_slice_id == slice_id))
            .unwrap()
    }

    /// Byte-identical output: every entry the pipelined execute hands the
    /// store is its source file's bytes, block-padded — checked against the
    /// files on disk, not against another run of the same code. The write
    /// phase counts exactly the entries' true sizes, on the session's own
    /// thread, and the session log names each file with its waits.
    #[test]
    fn every_entry_reaches_the_store_byte_identical_to_its_source() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);
        let sources: Vec<(usize, Vec<u8>)> = f
            .built
            .layout
            .entries
            .iter()
            .filter(|e| !matches!(e.kind, ZoneKind::SealMarker))
            .map(|e| {
                (
                    e.position as usize,
                    std::fs::read(entry_path(e).unwrap()).unwrap(),
                )
            })
            .collect();
        let true_total: u64 = sources.iter().map(|(_, b)| b.len() as u64).sum();

        let logs = TempDir::new().unwrap();
        let session = crate::progress::start_session(
            Some(logs.path()),
            "volume write",
            crate::progress::Display::Off,
            false,
        );
        let log_path = session.log_path().unwrap().to_path_buf();
        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let ready = match planned.execute(&f.conn, &mut store).unwrap() {
            ExecuteOutcome::Ready(r) => r,
            _ => panic!("expected Ready"),
        };
        let phases = crate::progress::drain();
        drop(session);

        assert_eq!(store.files.len(), sources.len());
        for (position, bytes) in &sources {
            let mut want = bytes.clone();
            want.resize(bytes.len().div_ceil(BS as usize) * BS as usize, 0);
            assert!(
                store.files[*position] == want,
                "file {position} differs from its source"
            );
        }
        let write = phases.iter().find(|p| p.phase == "write").unwrap();
        assert_eq!(write.bytes, Some(true_total), "every true byte, once");
        let log = std::fs::read_to_string(log_path).unwrap();
        assert_eq!(
            log.matches("; tape waited ").count(),
            sources.len(),
            "one line per file names its waits:\n{log}"
        );
        drop(ready);
    }

    /// Tri-layer L2 since #390: the rotted slice never completes at the
    /// store — its `execute` ends in an error, so neither its last block nor
    /// its filemark is written, and MemStore records nothing for it — while
    /// the abort reason, the `writes` rows and the slice's cursor row (with
    /// the hash the bytes actually have) are exactly what they always were.
    #[test]
    fn an_l2_mismatch_never_reaches_the_files_filemark() {
        let f = make_fixture();
        let mut store = Recording::new();
        let slice = &f.units[0].slices[0];
        let position = slice_position(&f, slice.slice_id);
        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        rot_in_place(&slice.staging_path);
        let rotted_hash = format!(
            "{:x}",
            Sha256::digest(std::fs::read(&slice.staging_path).unwrap())
        );

        let aborted = match planned.execute(&f.conn, &mut store).unwrap() {
            ExecuteOutcome::Aborted(a) => a,
            _ => panic!("L2 must abort"),
        };
        assert_eq!(
            aborted.reason,
            format!(
                "hash mismatch at position {position}: expected {}, got {rotted_hash}",
                slice.sha256_encrypted
            )
        );
        assert_eq!(
            store.calls.len(),
            position + 1,
            "nothing after the rotted slice"
        );
        assert!(
            store.calls[..position].iter().all(|c| c.is_ok()),
            "{:?}",
            store.calls
        );
        let err = store.calls[position].as_ref().unwrap_err();
        assert!(err.contains("withheld"), "the store stopped short: {err}");
        assert_eq!(
            store.inner.files.len(),
            position,
            "the rotted slice was never completed on the medium"
        );
        let (wp_status, wp_hash): (String, Option<String>) = f
            .conn
            .query_row(
                "SELECT status, sha256_on_volume FROM write_positions WHERE stage_slice_id = ?1",
                params![slice.slice_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(wp_status, "failed");
        assert_eq!(wp_hash.as_deref(), Some(rotted_hash.as_str()));
        let statuses: Vec<String> = f
            .conn
            .prepare("SELECT status FROM writes WHERE volume_id = ?1")
            .unwrap()
            .query_map(params![f.volume_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(statuses, vec!["aborted".to_string()]);
    }

    /// A store error partway through a file (a full medium) is the same
    /// clean abort with the store's own message, as before #390 — and the
    /// call returns, so the pipeline's threads have been joined.
    #[test]
    fn a_store_error_mid_file_is_the_same_clean_abort() {
        let f = make_fixture();
        let mut store = Recording::new();
        let slice = &f.units[0].slices[0];
        let position = slice_position(&f, slice.slice_id);
        store.fail_on = Some((position, 10));
        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let aborted = match planned.execute(&f.conn, &mut store).unwrap() {
            ExecuteOutcome::Aborted(a) => a,
            _ => panic!("a store error must abort"),
        };
        assert_eq!(
            aborted.reason,
            format!(
                "execute failed at position {position}: {}",
                TapectlError::MediumFull("write: No space left on device (os error 28)".into())
            )
        );
        let (wp_status, wp_hash): (String, Option<String>) = f
            .conn
            .query_row(
                "SELECT status, sha256_on_volume FROM write_positions WHERE stage_slice_id = ?1",
                params![slice.slice_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((wp_status.as_str(), wp_hash), ("failed", None));
        assert_eq!(store.inner.files.len(), position);
    }

    // ── issue #397: the confirm after a write reads the seal without a long
    // locate. Driven over the REAL `TapeStore` on `tape::fake::FakeTape`,
    // which logs every motion, through the session's own seal -> confirm.

    use crate::store::TapeStore;
    use crate::tape::fake::{FakeTape, Op};

    /// Write this fixture's whole session (seal included) onto a fake tape,
    /// then clear the motion log: what is logged after this is the confirm.
    fn sealed_on_a_fake_tape(f: Fixture) -> (Connection, SealedPending, TapeStore, FakeTape, u32) {
        let (conn, pending, store, fake, seal, _dirs) = sealed_on_a_fake_tape_keeping_dirs(f);
        (conn, pending, store, fake, seal)
    }

    /// [`sealed_on_a_fake_tape`], handing back the fixture's staging and
    /// session directories too, for a test that goes on to `volume resume`
    /// (which reads the frozen layout from the session directory).
    #[allow(clippy::type_complexity)]
    fn sealed_on_a_fake_tape_keeping_dirs(
        f: Fixture,
    ) -> (
        Connection,
        SealedPending,
        TapeStore,
        FakeTape,
        u32,
        (tempfile::TempDir, tempfile::TempDir),
    ) {
        let seal = (f.built.layout.entries.len() - 1) as u32;
        let fake = FakeTape::with_files(Vec::new(), BS as usize);
        let mut store = TapeStore::from_ops(fake.boxed(), u64::MAX).unwrap();
        let validated = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap();
        let planned = validated.plan(&f.conn, f.volume_id, &f.units).unwrap();
        let ready = match planned.execute(&f.conn, &mut store).unwrap() {
            ExecuteOutcome::Ready(r) => r,
            _ => panic!("expected Ready on a happy-path fake-tape run"),
        };
        let pending = ready.seal(&mut store).unwrap();
        fake.clear_ops();
        (
            f.conn,
            pending,
            store,
            fake,
            seal,
            (f._slices_dir, f._session_dir),
        )
    }

    /// A full confirm straight after the seal: one rewind and one forward
    /// pass that reads the seal last — no locate out to the seal and back
    /// (before #397: rewind, space to the seal, read it, rewind again).
    #[test]
    fn a_full_confirm_right_after_the_seal_is_one_rewind_and_one_forward_pass() {
        let (conn, pending, mut store, fake, seal) = sealed_on_a_fake_tape(make_fixture());
        let outcome = pending.confirm(&conn, &mut store, Tier::Integrity).unwrap();
        assert!(matches!(outcome, ConfirmOutcome::Sealed(_)));
        let mut expected = vec![Op::Rewind];
        expected.extend((0..=seal).map(Op::Read));
        assert_eq!(fake.ops(), expected);
        assert_eq!(fake.rewinds(), 1);
        assert_eq!(fake.spaces(), 0, "no locate to the seal");
    }

    /// The quick confirm straight after the seal: File 3, then a relative
    /// forward space to the seal — one rewind (before #397: two).
    #[test]
    fn a_quick_confirm_right_after_the_seal_reads_file_3_then_the_seal() {
        let (conn, pending, mut store, fake, seal) = sealed_on_a_fake_tape(make_fixture());
        let outcome = pending.confirm(&conn, &mut store, Tier::Navigable).unwrap();
        assert!(matches!(outcome, ConfirmOutcome::Sealed(_)));
        assert_eq!(
            fake.ops(),
            vec![
                Op::Rewind,
                Op::Space(3),
                Op::Read(3),
                Op::Space(seal - 4),
                Op::Read(seal),
            ]
        );
    }

    // ── issue #410: an interrupted full confirm is continued by `volume
    // resume`, not redone.

    /// A `TapeStore` that has a signal arrive right after it reads `at`.
    struct SignalAfter {
        inner: TapeStore,
        at: u32,
    }

    impl Store for SignalAfter {
        fn capacity(&mut self) -> Result<crate::store::CapacityReport> {
            self.inner.capacity()
        }
        fn execute(&mut self, src: &mut dyn std::io::Read, len: u64, sync: bool) -> Result<u64> {
            self.inner.execute(src, len, sync)
        }
        fn read_file(&mut self, position: u32, sink: &mut dyn Write) -> Result<u64> {
            let r = self.inner.read_file(position, sink);
            if position == self.at {
                crate::signal::interrupt_this_thread(true);
            }
            r
        }
        fn reposition_for_resume(&mut self, file_index: u32) -> Result<()> {
            self.inner.reposition_for_resume(file_index)
        }
    }

    /// The acceptance test, through the catalog: a full confirm stopped by a
    /// signal after file k leaves its checkpoints, and the confirm `volume
    /// resume` re-enters reads File 3, the files after k and the seal —
    /// nothing before k — and seals with the counts an uninterrupted
    /// confirm records.
    #[test]
    fn a_resumed_full_confirm_reads_only_what_the_interrupted_one_had_not() {
        let f = make_fixture();
        let volume_id = f.volume_id;
        let keys = f.keys.clone();
        let (conn, pending, store, fake, seal, _dirs) = sealed_on_a_fake_tape_keeping_dirs(f);
        let k = 5;
        let mut stopping = SignalAfter {
            inner: store,
            at: k,
        };
        let r = pending.confirm(&conn, &mut stopping, Tier::Integrity);
        crate::signal::interrupt_this_thread(false);
        assert!(
            matches!(r, Err(TapectlError::Interrupted(_))),
            "the first confirm stops on the signal"
        );
        let checkpointed: Vec<u32> = conn
            .prepare("SELECT position FROM readback_checkpoints ORDER BY position")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(checkpointed, vec![0, 1, 2, 4, 5]);

        // What the startup sweep does for a process that stopped mid-confirm.
        conn.execute(
            "UPDATE writes SET status = 'interrupted' WHERE volume_id = ?1",
            params![volume_id],
        )
        .unwrap();
        let mut store = TapeStore::from_ops(fake.boxed(), u64::MAX).unwrap();
        let pending = match InterruptedSession::rehydrate(&conn, volume_id)
            .unwrap()
            .expect("resumable")
            .resume_checking(&conn, &keys, SliceCheck::Size, &mut store, || false)
            .unwrap()
        {
            ResumeOutcome::Confirming(p) => p,
            _ => panic!("expected Confirming on this session's own sealed tape"),
        };
        fake.clear_ops();
        let outcome = pending.confirm(&conn, &mut store, Tier::Integrity).unwrap();
        assert!(matches!(outcome, ConfirmOutcome::Sealed(_)));

        let mut expected = vec![Op::Rewind, Op::Space(3), Op::Read(3), Op::Space(k + 1 - 4)];
        // The seal is not read again: this resume's contact check read it,
        // and confirm starts from those bytes (#403).
        expected.extend((k + 1..seal).map(Op::Read));
        assert_eq!(fake.ops(), expected, "nothing at or before k read again");

        let (outcome, checked, passed): (String, i64, i64) = conn
            .query_row(
                "SELECT outcome, slices_checked, slices_passed FROM verification_sessions
                 WHERE volume_id = ?1 ORDER BY id DESC LIMIT 1",
                params![volume_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(outcome, "passed");
        assert_eq!((checked, passed), ((seal + 1) as i64, (seal + 1) as i64));
        // ADR-0012 2026-10-06 item 24: the continued readback read the
        // whole tape between its two runs, so the write was read back.
        let verified: bool = conn
            .query_row(
                "SELECT write_verified FROM writes WHERE volume_id = ?1",
                params![volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(verified, "a continued full readback that passes is one");
    }

    /// Only an INTERRUPTED readback is continued: one that finished — here
    /// with a failed (inconclusive) outcome — is read again from the start,
    /// so a drive cleaned in between reads everything.
    #[test]
    fn a_readback_that_finished_is_never_continued() {
        let f = make_fixture();
        let conn = &f.conn;
        conn.execute(
            "INSERT INTO verification_sessions (volume_id, verify_type, outcome)
             VALUES (?1, 'full', 'failed')",
            params![f.volume_id],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO readback_checkpoints (session_id, position, sha256, front_index_sha256)
             VALUES (?1, 4, 'aa', 'bb')",
            params![id],
        )
        .unwrap();
        assert_eq!(interrupted_readback(conn, f.volume_id).unwrap(), None);
        conn.execute(
            "UPDATE verification_sessions SET outcome = 'aborted' WHERE id = ?1",
            params![id],
        )
        .unwrap();
        let r = interrupted_readback(conn, f.volume_id).unwrap().unwrap();
        assert_eq!(r.session_id, id);
        assert_eq!(r.checkpoints.front_index_sha256, "bb");
        assert_eq!(r.checkpoints.passed.get(&4).map(String::as_str), Some("aa"));
        // A quick one, even interrupted, read no content to continue from.
        conn.execute(
            "UPDATE verification_sessions SET verify_type = 'quick' WHERE id = ?1",
            params![id],
        )
        .unwrap();
        assert_eq!(interrupted_readback(conn, f.volume_id).unwrap(), None);
    }

    /// ADR-0012's 2026-09-23 adoption rule wants a full readback wholly
    /// after a write abort, so a readback holding a file read back at or
    /// before the volume's recorded abort is not continued (issue #410) —
    /// else a verify started after the abort would carry reads from before
    /// it, and `volume resume` would adopt on them.
    #[test]
    fn a_readback_with_a_file_read_before_the_recorded_abort_is_not_continued() {
        let f = make_fixture();
        let conn = &f.conn;
        conn.execute(
            "INSERT INTO verification_sessions (volume_id, verify_type, outcome)
             VALUES (?1, 'full', 'aborted')",
            params![f.volume_id],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO readback_checkpoints
                 (session_id, position, sha256, front_index_sha256, checked_at)
             VALUES (?1, 4, 'aa', 'bb', '2026-01-02 00:00:00')",
            params![id],
        )
        .unwrap();
        let abort_at = |at: &str| {
            conn.execute(
                "INSERT INTO events (timestamp, entity_type, entity_id, action)
                 VALUES (?1, 'volume', ?2, 'write_aborted')",
                params![at, f.volume_id],
            )
            .unwrap();
        };
        abort_at("2026-01-01 00:00:00");
        assert!(
            interrupted_readback(conn, f.volume_id).unwrap().is_some(),
            "positive control: every read postdates that abort"
        );
        abort_at("2026-01-02 00:00:00");
        assert_eq!(
            interrupted_readback(conn, f.volume_id).unwrap(),
            None,
            "a read in the abort's second is not after it"
        );
    }

    /// Issue #397's other order: a resume whose seal is RECORDED but does
    /// not read back re-enters confirm seal FIRST, so the unreadable seal
    /// is found at one read rather than after a forward pass over the whole
    /// tape. (The seal-last pass is only for a seal this session has just
    /// written or just parsed.) With issue #403: a seal whose bytes this
    /// resume's contact check already read (they came back, but do not
    /// parse) is not read again at all; one whose read failed is read once
    /// more, first.
    #[test]
    fn a_resume_whose_recorded_seal_does_not_read_confirms_seal_first() {
        for eio in [false, true] {
            let f = make_fixture();
            let volume_id = f.volume_id;
            let keys = f.keys.clone();
            let (conn, _pending, _store, fake, seal, _dirs) = sealed_on_a_fake_tape_keeping_dirs(f);
            conn.execute(
                "UPDATE writes SET status = 'interrupted' WHERE volume_id = ?1",
                params![volume_id],
            )
            .unwrap();
            // What `write::finish_session` records once `seal()` returns.
            conn.execute(
                "UPDATE volumes SET sealed_at = datetime('now') WHERE id = ?1",
                params![volume_id],
            )
            .unwrap();
            {
                let mut st = fake.state();
                if eio {
                    // The seal marker fails to read (EIO before a byte).
                    st.unreadable.push(seal);
                } else {
                    // The seal marker no longer parses (one block of garbage).
                    let block = st.block_size;
                    st.files[seal as usize] = vec![0xA5; block];
                }
            }
            let mut store = TapeStore::from_ops(fake.boxed(), u64::MAX).unwrap();
            let pending = match InterruptedSession::rehydrate(&conn, volume_id)
                .unwrap()
                .expect("resumable")
                .resume_checking(&conn, &keys, SliceCheck::Size, &mut store, || false)
                .unwrap()
            {
                ResumeOutcome::Confirming(p) => p,
                _ => panic!("a recorded seal re-enters confirm (eio: {eio})"),
            };
            fake.clear_ops();
            let outcome = pending.confirm(&conn, &mut store, Tier::Integrity).unwrap();
            assert!(
                matches!(outcome, ConfirmOutcome::Inconclusive(_)),
                "an unreadable seal is inconclusive, not quarantine (eio: {eio})"
            );
            let reads: Vec<u32> = fake
                .ops()
                .into_iter()
                .filter_map(|op| match op {
                    Op::Read(p) | Op::ReadHead(p) => Some(p),
                    _ => None,
                })
                .collect();
            if eio {
                assert_eq!(reads, vec![seal], "the seal, and nothing after it");
            } else {
                assert_eq!(
                    reads,
                    Vec::<u32>::new(),
                    "the seal the contact read is not read again, and nothing after it"
                );
            }
        }
    }

    /// Issue #408: a drive error partway through a file that is NOT a full
    /// medium — an EIO, a bus reset — leaves the session `interrupted`, not
    /// aborted: the files before it are whole, exactly the state a crash
    /// leaves and resume already repositions from. The slice goes back to
    /// `pending`, and `resume` writes it again and the session seals. It
    /// used to be a terminal abort.
    #[test]
    fn a_drive_error_mid_file_interrupts_and_resume_completes() {
        let f = make_fixture();
        let mut store = Recording::new();
        let slice = f.units[0].slices[0].clone();
        let position = slice_position(&f, slice.slice_id);
        store.fail_on = Some((position, 10));
        store.fail_eio = true;
        let planned = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap()
            .plan(&f.conn, f.volume_id, &f.units)
            .unwrap();
        let interrupted = match planned.execute(&f.conn, &mut store).unwrap() {
            ExecuteOutcome::Interrupted(i) => i,
            ExecuteOutcome::Aborted(a) => panic!("a drive error must not abort: {}", a.reason),
            ExecuteOutcome::Ready(_) => panic!("expected Interrupted"),
        };
        let reason = interrupted.reason().unwrap().to_string();
        assert!(
            reason.contains("tape I/O error") && reason.contains(&format!("position {position}")),
            "{reason}"
        );
        assert_eq!(statuses_on(&f.conn, f.volume_id), vec!["interrupted"]);
        let wp: String = f
            .conn
            .query_row(
                "SELECT status FROM write_positions WHERE stage_slice_id = ?1",
                params![slice.slice_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(wp, "pending");

        store.fail_on = None;
        let ready = match interrupted
            .resume_checking(&f.conn, &f.keys, SliceCheck::Size, &mut store, || false)
            .unwrap()
        {
            ResumeOutcome::Ready(r) => r,
            _ => panic!("expected the resume to finish the write"),
        };
        match ready
            .seal(&mut store)
            .unwrap()
            .confirm(&f.conn, &mut store, Tier::Integrity)
            .unwrap()
        {
            ConfirmOutcome::Sealed(_) => {}
            _ => panic!("expected Sealed"),
        }
        assert_eq!(statuses_on(&f.conn, f.volume_id), vec!["completed"]);
    }

    /// Issue #408: a STAGED file that cannot be read mid-write (here: the
    /// path now names a directory, so every read fails with EISDIR — a
    /// stand-in for an EIO from a failing staging disk) leaves the session
    /// `interrupted`, the error named as the source's, not the tape's. Once
    /// the file is back, `resume` completes the write. It used to abort the
    /// session for good and blame the tape.
    #[test]
    fn a_staged_file_read_error_interrupts_and_resume_completes() {
        let f = make_fixture();
        let mut store = MemStore::new(BS as usize);
        let planned = f
            .built
            .into_validated(&f.keys, SliceCheck::Size, &mut store)
            .unwrap()
            .plan(&f.conn, f.volume_id, &f.units)
            .unwrap();
        let path = f.units[0].slices[1].staging_path.clone();
        let good = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();

        let interrupted = match planned.execute(&f.conn, &mut store).unwrap() {
            ExecuteOutcome::Interrupted(i) => i,
            ExecuteOutcome::Aborted(a) => {
                panic!("a staged-file read error must not abort: {}", a.reason)
            }
            ExecuteOutcome::Ready(_) => panic!("expected Interrupted"),
        };
        let reason = interrupted.reason().unwrap().to_string();
        assert!(reason.contains("staged source read error"), "{reason}");
        assert!(!reason.contains("tape I/O error"), "{reason}");
        assert_eq!(statuses_on(&f.conn, f.volume_id), vec!["interrupted"]);

        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, &good).unwrap();
        // What `volume resume` does in a new process: the session comes
        // back from the catalog and the session directory, not from memory.
        drop(interrupted);
        let interrupted = InterruptedSession::rehydrate(&f.conn, f.volume_id)
            .unwrap()
            .expect("the interrupted session is resumable");
        let ready = match interrupted
            .resume_checking(&f.conn, &f.keys, SliceCheck::Size, &mut store, || false)
            .unwrap()
        {
            ResumeOutcome::Ready(r) => r,
            _ => panic!("expected the resume to finish the write"),
        };
        match ready
            .seal(&mut store)
            .unwrap()
            .confirm(&f.conn, &mut store, Tier::Integrity)
            .unwrap()
        {
            ConfirmOutcome::Sealed(_) => {}
            _ => panic!("expected Sealed"),
        }
        assert_eq!(statuses_on(&f.conn, f.volume_id), vec!["completed"]);
    }

    /// ADR-0012 2026-10-06 item 24 (#392): `writes.write_verified` means
    /// "this write was fully read back". A passing Integrity confirm sets
    /// it on the rows it completes; a passing quick (Navigable) confirm,
    /// which reads File 0, the front index and the seal only, leaves it 0.
    #[test]
    fn a_full_confirm_marks_the_write_verified_and_a_quick_one_does_not() {
        for (tier, want) in [(Tier::Integrity, true), (Tier::Navigable, false)] {
            let f = make_fixture();
            let mut store = MemStore::new(BS as usize);
            let planned = f
                .built
                .into_validated(&f.keys, SliceCheck::Size, &mut store)
                .unwrap()
                .plan(&f.conn, f.volume_id, &f.units)
                .unwrap();
            let ExecuteOutcome::Ready(ready) = planned.execute(&f.conn, &mut store).unwrap() else {
                panic!("expected Ready");
            };
            let outcome = ready
                .seal(&mut store)
                .unwrap()
                .confirm(&f.conn, &mut store, tier)
                .unwrap();
            assert!(matches!(outcome, ConfirmOutcome::Sealed(_)), "{tier:?}");
            let rows: Vec<(String, bool)> = f
                .conn
                .prepare("SELECT status, write_verified FROM writes WHERE volume_id = ?1")
                .unwrap()
                .query_map(params![f.volume_id], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(rows, vec![("completed".to_string(), want)], "{tier:?}");
        }
    }
}
