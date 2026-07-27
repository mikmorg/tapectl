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
//!     'current', volumes 'sealed'. fail => volumes 'quarantined', session
//!     aborted, staging kept.
//! ```
//!
//! Each phase's operations exist only on its type, so an invalid order is
//! unrepresentable: e.g. there is no `BuiltLayout::seal`, no
//! `PlannedSession::confirm` — you can only call the next step for the type
//! you actually hold. In particular, no code path other than
//! [`ReadyToSeal::seal`] can ever produce a `SealMarker` write (sacred
//! invariant 1, `v2-implementation-plan.md`): sealing is unreachable unless
//! `execute` ran every non-seal entry to completion.
//!
//! `ExecuteOutcome`/`ResumeOutcome` stand in for the flow block's single
//! "SessionEnd" box: Rust has no single type for "one of several typed
//! successor states," so each fan-out point (execute can end Ready,
//! Interrupted, or Aborted; confirm can end Sealed or Quarantined; resume
//! adds Quarantined to execute's three) is its own small enum. The state
//! *names* match `layout-session.md`'s table exactly; only the Rust-level
//! packaging is invented here.
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
        for u in units {
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, 'planned')",
                params![u.stage_set_id, u.snapshot_id, volume_id],
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
/// `Quarantined` (the File-0 identity check found a divergent tape).
pub enum ResumeOutcome {
    Ready(ReadyToSeal),
    Interrupted(InterruptedSession),
    Aborted(AbortedSession),
    Quarantined(QuarantinedSession),
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
/// report vs. a label/uuid disagreement — so they are not forced into the
/// same shape.
pub enum QuarantineReason {
    ConfirmFailed(Evidence),
    IdentityMismatch {
        expected_label: String,
        expected_uuid: String,
        /// `None` if File 0 was present but did not even parse as a valid
        /// ID thunk (still a mismatch — never overwrite on ambiguity).
        found: Option<format::IdThunkIdentity>,
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

/// What `confirm` ended with.
pub enum ConfirmOutcome {
    Sealed(SealedSession),
    Quarantined(QuarantinedSession),
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
        )
    }
}

impl InterruptedSession {
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
    ///
    /// Caller note: this resumes the SAME in-memory session value (e.g.
    /// after a caught SIGINT within one process, or in a test). Reconstructing
    /// an `InterruptedSession` after an actual process restart means
    /// re-reading the ORIGINAL session_dir's frozen files — never calling
    /// `build()` again, which is not reproducible (`ContentSource::Materialized`'s
    /// doc comment) — and is the CLI orchestrator's job (T8), not this
    /// module's; this method only requires that its caller supply a valid
    /// `BuiltLayout` (carried on `self` from whenever this `InterruptedSession`
    /// was constructed), however they sourced it.
    pub fn resume_checking(
        self,
        conn: &Connection,
        keys: &KeyAvailability,
        store: &mut dyn Store,
        mut is_interrupted: impl FnMut() -> bool,
    ) -> Result<ResumeOutcome> {
        // 1. Revalidate: staged slices unchanged, frozen zones re-hash
        // byte-identical (re-runs the same tri-layer-L1 + materialized-zone
        // checks `into_validated` ran originally). Failure here is
        // unrecoverable — Aborted, not Quarantined (this isn't "wrong tape,"
        // it's "this tape's own inputs no longer check out").
        if let Err(errs) = self.built.validate(keys) {
            mark_writes(conn, &self.write_ids, "aborted")?;
            return Ok(ResumeOutcome::Aborted(AbortedSession {
                volume_id: self.volume_id,
                reason: format!("resume: revalidation failed: {errs:?}"),
            }));
        }

        // 2. File-0 identity check. A read failure (nothing recorded at
        // position 0 at all) means legitimately nothing was written yet —
        // NOT a mismatch, since a session crashed before File 0 landed is
        // exactly the "zero slices written" cursor case below, which
        // restarts from BOT anyway. Only a File 0 that IS readable but
        // disagrees (or fails to parse — ambiguous, so treated as a
        // mismatch: never overwrite on ambiguity) is a divergence.
        let mut id_thunk_bytes = Vec::new();
        if store.read_file(0, &mut id_thunk_bytes).is_ok() {
            let text = String::from_utf8_lossy(&id_thunk_bytes);
            let identity = format::parse_id_thunk_identity(&text);
            let matches = matches!(
                &identity,
                Ok(id) if id.label == self.built.layout.label
                    && id.uuid == self.built.layout.volume_uuid
            );
            if !matches {
                conn.execute(
                    "UPDATE volumes SET status = 'quarantined' WHERE id = ?1",
                    params![self.volume_id],
                )?;
                mark_writes(conn, &self.write_ids, "aborted")?;
                return Ok(ResumeOutcome::Quarantined(QuarantinedSession {
                    volume_id: self.volume_id,
                    label: self.built.layout.label.clone(),
                    reason: QuarantineReason::IdentityMismatch {
                        expected_label: self.built.layout.label.clone(),
                        expected_uuid: self.built.layout.volume_uuid.clone(),
                        found: identity.ok(),
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
            &mut is_interrupted,
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
    /// Integrity -> 'full', Navigable -> 'quick', ADR-0001), then: pass => ONE
    /// transaction flipping `writes` 'completed', `snapshots` 'current',
    /// `volumes` 'sealed'; fail => `volumes` 'quarantined', `writes` 'aborted'
    /// (staging kept either way — this method never touches staging).
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

        if passed {
            let tx = conn.unchecked_transaction()?;
            for (write_id, _) in &self.write_ids {
                tx.execute(
                    "UPDATE writes SET status = 'completed', completed_at = datetime('now')
                     WHERE id = ?1",
                    params![write_id],
                )?;
            }
            for (_, snapshot_id) in &self.write_ids {
                tx.execute(
                    "UPDATE snapshots SET status = 'current'
                     WHERE id = ?1 AND status IN ('created', 'staged')",
                    params![snapshot_id],
                )?;
            }
            tx.execute(
                "UPDATE volumes SET status = 'sealed' WHERE id = ?1",
                params![self.volume_id],
            )?;
            tx.commit()?;
            Ok(ConfirmOutcome::Sealed(SealedSession {
                volume_id: self.volume_id,
                label: self.built.layout.label.clone(),
            }))
        } else {
            conn.execute(
                "UPDATE volumes SET status = 'quarantined' WHERE id = ?1",
                params![self.volume_id],
            )?;
            mark_writes(conn, &self.write_ids, "aborted")?;
            Ok(ConfirmOutcome::Quarantined(QuarantinedSession {
                volume_id: self.volume_id,
                label: self.built.layout.label.clone(),
                reason: QuarantineReason::ConfirmFailed(evidence),
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
fn run_entries(
    conn: &Connection,
    store: &mut dyn Store,
    built: BuiltLayout,
    volume_id: i64,
    write_ids: Vec<(i64, i64)>,
    slice_write_id: HashMap<i64, i64>,
    start_index: usize,
    is_interrupted: &mut dyn FnMut() -> bool,
) -> Result<ExecuteOutcome> {
    let content_entries: Vec<&LayoutEntry> = built
        .layout
        .entries
        .iter()
        .filter(|e| !matches!(e.kind, ZoneKind::SealMarker))
        .collect();

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
        };

        let session_dir = tempfile::tempdir().unwrap();
        let built = build::build(&inputs, session_dir.path()).unwrap();

        let keys = KeyAvailability {
            tenant_ids: vec![tenant_id],
            tenants_with_active_key: [tenant_id].into_iter().collect(),
            operator_key_present: true,
            escrow_recipient_present: None,
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
                    QuarantineReason::IdentityMismatch { .. } => "identity mismatch".to_string(),
                }
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

    /// Bonus coverage: the File-0 identity check's divergence path. Simulates
    /// "wrong cartridge in the drive" — after an interruption, File 0 on the
    /// (mock) tape disagrees with the Layout being resumed — and asserts
    /// `docs/design/layout-session.md`'s "mismatch = divergence = quarantine,
    /// not overwrite": the volume is quarantined, the session is aborted, and
    /// critically the session never reaches `run_entries` at all (no risk of
    /// silently overwriting the wrong tape).
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
            QuarantineReason::ConfirmFailed(_) => panic!("expected IdentityMismatch"),
        }

        let volume_status: String = f
            .conn
            .query_row(
                "SELECT status FROM volumes WHERE id = ?1",
                params![f.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(volume_status, "quarantined");

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
            QuarantineReason::IdentityMismatch { .. } => {
                panic!("expected ConfirmFailed, got IdentityMismatch")
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
        assert_eq!(volume_status, "quarantined");

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
}
