//! Batch execution (`docs/design/v2-open-questions.md` §11): "snapshot +
//! stage each unit once → session (§9) on cartridge A → seal + confirm →
//! session on cartridge B → seal + confirm → release staging (GC rule §3.5:
//! only after every planned copy is sealed). Stage once, write N times."
//!
//! Deliberately thin — this reuses `staging::snapshot_create`/
//! `stage_create`, `volume::write::volume_write`, and
//! `staging::clean::clean_staging` wholesale rather than reimplementing any
//! of the write session or the §3.5 GC rule
//! (`docs/design/v2-implementation-plan.md` T10: "Reuse `volume_write`; do
//! NOT reimplement the session").
//!
//! Not exercised by the automated test suite: every copy is a real tape
//! write (`volume::write::volume_write` opens a real `TapeStore` device),
//! same as `volume_write` itself today (only its pre-flight-error paths are
//! unit-tested; the mhvtl e2e suite is what actually drives a device, and
//! this workspace's guardrails forbid touching tape devices from here).

use rusqlite::Connection;

use crate::config::{Config, TapectlPaths};
use crate::error::{Result, TapectlError};
use crate::staging::clean::CleanReport;

use super::selector::Batch;

/// Outcome of executing one batch.
#[derive(Debug)]
pub struct BatchExecutionReport {
    pub units_staged: usize,
    pub copies_written: usize,
    pub cleaned: CleanReport,
}

/// Execute one batch: stage every unit once, write one session per
/// destination label, then release staging.
///
/// `copy_labels` names N **pre-existing** volumes (already `volume init`'d
/// onto their own cartridges) — one per planned copy. This deliberately
/// does not auto-`volume_init`: that writes tape, and choosing/labelling
/// destination cartridges is an operator act, not something a batch driver
/// should do silently.
///
/// Copies run strictly sequentially, and the first copy that returns an
/// error aborts the whole batch immediately (via `?`) rather than
/// continuing to the next label. This matters beyond the obvious: `volume
/// write`'s `find_staged_data` scoops up *every* `'staged'` stage_set in the
/// database, not just this batch's — so if this function pressed on to a
/// later batch after leaving a failed copy's stage_sets un-released, that
/// later batch's own `volume_write` call would pick up this batch's still-
/// staged (because-not-yet-fully-sealed) data too. Stopping here, with
/// nothing cleaned, keeps that entanglement from ever happening; the
/// operator investigates the `writes`/`write_positions` rows for the failed
/// copy before retrying (same posture `volume_write` already takes for an
/// unresolved session — see its own doc comment).
///
/// Release (the last step) is exactly the existing GC guard
/// (`staging::clean::clean_staging`, non-force): it independently
/// re-checks, per stage_set, that every `writes` row is `'completed'`
/// (`docs/design/v2-open-questions.md` §3.5) before removing anything, so
/// calling it here needs no batch-scoped bookkeeping of its own — by the
/// time this line runs, every copy in `copy_labels` sealed (or this
/// function already returned), so this batch's stage_sets now qualify.
pub fn execute_batch(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    batch: &Batch,
    copy_labels: &[String],
    device: &str,
    block_size: usize,
) -> Result<BatchExecutionReport> {
    if batch.units.is_empty() {
        return Err(TapectlError::Other(
            "execute_batch: batch has no units".into(),
        ));
    }
    if copy_labels.is_empty() {
        return Err(TapectlError::Other(
            "execute_batch: at least one destination volume label is required \
             (one per planned copy)"
                .into(),
        ));
    }

    // Stage once: snapshot + stage every unit in this batch, in the
    // batch's own (name-ordered) sequence.
    for u in &batch.units {
        let snapshot_id = crate::staging::snapshot_create(conn, &u.name)?;
        crate::staging::stage_create(conn, paths, config, snapshot_id)?;
    }

    // Session per copy — sequential, abort-on-first-failure (see doc
    // comment above).
    for label in copy_labels {
        crate::volume::write::volume_write(conn, paths, config, label, device, block_size)?;
    }

    // Release staging: only reachable once every copy above sealed.
    let cleaned = crate::staging::clean::clean_staging(conn, false)?;

    Ok(BatchExecutionReport {
        units_staged: batch.units.len(),
        copies_written: copy_labels.len(),
        cleaned,
    })
}
