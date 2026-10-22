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

use crate::config::{CollectionConfig, Config, TapectlPaths};
use crate::error::{Result, TapectlError};
use crate::staging::clean::CleanReport;
pub use crate::staging::release::CopyProgress;
use crate::staging::view::SourceView;

use super::selector::Batch;

/// Which copies of a `collection run` its confirm reads back in full
/// (ADR-0012 2026-10-07 item 31). The default confirm is quick (the front
/// index and the seal), and a run releases staging right after its confirms
/// (item 2), so a full readback is the operator's choice here, per run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunConfirm {
    /// No flag: every copy gets the quick confirm.
    #[default]
    Quick,
    /// `--full-confirm`: every copy is read back in full.
    FullEvery,
    /// `--full-confirm-first`: the run's first copy is read back in full,
    /// the others confirmed quickly — the one-copy readback item 2
    /// recommends when the data matters.
    FullFirst,
}

impl RunConfirm {
    /// Whether copy `index` (0 = the run's first) gets the full readback.
    pub fn full_for_copy(self, index: usize) -> bool {
        match self {
            RunConfirm::Quick => false,
            RunConfirm::FullEvery => true,
            RunConfirm::FullFirst => index == 0,
        }
    }
}

/// Outcome of executing one batch.
#[derive(Debug)]
pub struct BatchExecutionReport {
    /// How many units in the batch actually had `stage_create` called for
    /// them this run — NOT `batch.units.len()` (issue #232 item 2): a unit
    /// already staged for this exact content, or one whose on-disk state
    /// reverted back to the live version between plan and run, is a
    /// deliberate no-op in the loop above and must not be counted as
    /// "staged" here.
    pub units_staged: usize,
    pub copies_written: usize,
    /// `Some` when staging was released this call (every unit in the batch
    /// now meets its own resolved `min_copies`); `None` when it was
    /// deliberately retained — see [`crate::staging::release`].
    pub cleaned: Option<CleanReport>,
    /// Non-empty exactly when `cleaned` is `None`: which units are still
    /// short, and by how much.
    pub under_copied: Vec<CopyProgress>,
    /// `Some` when releasing staging failed after every copy was sealed and
    /// confirmed (a catalog error): the warning naming the unfinished step
    /// and `tapectl staging clean`, which finishes it. The copies stand, so
    /// the run exits as if the release had been retained (ADR-0012
    /// 2026-10-07 item 33); `cleaned` is `None` and `under_copied` empty.
    pub release_warning: Option<String>,
    /// The snapshot of the collection root this run's snapshot and stage read
    /// in place of the live one, when the collection has a `snapshot_command`
    /// (ADR-0012 2026-10-16 item 75).
    pub snapshot_path: Option<String>,
    /// `Some` when the collection's `release_command` failed: the warning,
    /// already said on stderr. The run's work stands.
    pub snapshot_release_warning: Option<String>,
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
/// Release (the last step) calls the existing GC guard
/// (`staging::clean::clean_staging`, non-force) — but **only when every unit
/// in this batch already has enough eligible copies to satisfy its own
/// resolved `min_copies`** ([`crate::staging::release::release_if_covered`]).
/// This gate did not exist before issue #229 and its absence was a real defect, not a
/// theoretical one: `copy_labels` is now always exactly one label (`collection
/// run` refuses more, `docs/adr/0012-...md`'s 2026-09-17 amendment), so this
/// function writes ONE copy and returns. `clean_staging`'s non-force guard
/// only checks that no `writes` row for a stage_set is non-`completed` (a
/// `cut` row, #420, counting as no write at all) — it has no notion of
/// "more copies are still coming" — so with exactly one
/// `completed` row it passes *vacuously*
/// (`default_guard_cleans_when_the_only_planned_copy_completed` pins this).
/// Calling it unconditionally here would release — and unlink — a unit's
/// staged bytes the instant its first copy landed, even when policy wants a
/// second, leaving nothing for the `tapectl volume write <label2>` this
/// module's caller (`cli::collection::cmd_run`) now tells the operator to
/// run next. So this function computes each unit's post-write copy count
/// itself and only calls `clean_staging` when none are still short — and
/// even then, scopes that call to exactly this batch's own units
/// (`staging::clean::CleanScope::Units`, via [`batch_unit_ids`], issue
/// #248): `clean_staging`'s own eligibility SQL has no notion of "batch" at
/// all, so passing `CleanScope::Whole` here (as `tapectl staging clean`
/// deliberately still does) would release every eligible stage_set in the
/// WHOLE database the instant this batch's own gate passed — including a
/// different batch's unit that is still below its own `min_copies`. A
/// still-short batch retains ALL of its own staging (not merely the short
/// units'), by design — see [`crate::staging::release`].
///
/// The gate-and-release decision itself lives in [`release_if_covered`]
/// (extracted from this function, issue #284): `volume_write` above has no
/// store injection and so cannot complete without a real tape drive, which
/// meant nothing after it could be driven by an ungated test either — a
/// prior regression test for this exact scope narrowing hand-copied this
/// tail instead of calling this function, and so silently stopped testing
/// it. `release_if_covered` is now the one place the scope decision is
/// made, callable directly by a test; `execute_batch_still_delegates_
/// release_to_the_scoped_helper` pins that this function still calls it.
///
/// `assume_yes` is the global `--yes`: it answers `volume_write`'s
/// quiet-host pre-flight for every copy (ADR-0012, 2026-09-24 amendment,
/// item 7) and `stage_create`'s question about a staging directory that may
/// be too small (issue #354) for every unit staged.
///
/// `prewrite_hash` is `collection run --prewrite-hash`, handed to every
/// copy's `volume_write` (ADR-0012, 2026-09-30 later amendment), and
/// `confirm` is `collection run --full-confirm` / `--full-confirm-first`,
/// asked per copy ([`RunConfirm::full_for_copy`]; ADR-0012, 2026-10-06 item
/// 1 and 2026-10-07 item 31).
#[allow(clippy::too_many_arguments)]
pub fn execute_batch(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    lib: &CollectionConfig,
    batch: &Batch,
    copy_labels: &[String],
    device: &str,
    block_size: usize,
    prewrite_hash: bool,
    confirm: RunConfirm,
    assume_yes: bool,
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
    // batch's own (name-ordered) sequence — from a snapshot of the root when
    // the collection has a `snapshot_command` (see [`stage_batch`]), released
    // before any tape is touched.
    let staged = stage_batch(conn, paths, config, lib, batch, assume_yes)?;

    // Session per copy — sequential, abort-on-first-failure (see doc
    // comment above). `force=false`: each `label` is documented as "already
    // `volume init`'d on its own cartridge" (`CollectionCommands::Run`'s own
    // doc comment) — a contact-check refusal here means the wrong physical
    // cartridge got loaded for this batch, which must hard-refuse, not
    // silently override (issue #27).
    for (copy, label) in copy_labels.iter().enumerate() {
        crate::volume::write::volume_write(
            conn,
            paths,
            config,
            label,
            device,
            block_size,
            false,
            false,
            prewrite_hash,
            confirm.full_for_copy(copy),
            assume_yes,
        )?;
    }

    // Release staging only when this batch's copy just sealed is enough —
    // see `release_if_covered`'s own doc comment for the gate and the #248
    // scoping it applies.
    let (cleaned, under_copied, release_warning) =
        release_after_copies(conn, config, batch, copy_labels);

    Ok(BatchExecutionReport {
        units_staged: staged.units_staged,
        copies_written: copy_labels.len(),
        cleaned,
        under_copied,
        release_warning,
        snapshot_path: staged.snapshot_path,
        snapshot_release_warning: staged.release_warning,
    })
}

/// What [`stage_batch`] did.
#[derive(Debug)]
pub(crate) struct StagedBatch {
    /// How many units had `stage_create` called for them.
    pub units_staged: usize,
    /// The snapshot the run read, when the collection has a `snapshot_command`.
    pub snapshot_path: Option<String>,
    /// The `release_command`'s failure, if it failed (already said on stderr).
    pub release_warning: Option<String>,
}

/// The staging half of a batch: take the collection's snapshot if it has a
/// `snapshot_command`, snapshot + stage every unit (reading the snapshot's
/// copy of each folder while recording the real one), then run the
/// `release_command` whether the staging succeeded or not (ADR-0012
/// 2026-10-16 item 75, issue #455).
///
/// A `snapshot_command` that fails refuses the run before any unit is read,
/// and nothing is released: nothing was taken. The release runs here, before
/// `execute_batch` touches a tape, so a snapshot is held for the staging and
/// not for the hours of writing after it. `execute_batch` cannot be driven
/// without a drive; this can, so it is where the hook is tested.
pub(crate) fn stage_batch(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    lib: &CollectionConfig,
    batch: &Batch,
    assume_yes: bool,
) -> Result<StagedBatch> {
    let held = super::hook::take_snapshot(lib)?;
    if let Some(held) = &held {
        crate::progress::stderr_println(&format!(
            "collection \"{}\": reading each unit from the snapshot at {}; every recorded \
             path stays the unit's real one",
            lib.name,
            held.snapshot_path().display()
        ));
    }
    let view = held.as_ref().map(super::hook::Held::view);
    let staged = stage_units(conn, paths, config, batch, assume_yes, view.as_ref());
    let snapshot_path = held
        .as_ref()
        .map(|h| h.snapshot_path().display().to_string());
    let release_warning = held.and_then(super::hook::Held::release);
    Ok(StagedBatch {
        units_staged: staged?,
        snapshot_path,
        release_warning,
    })
}

/// Snapshot + stage every unit of `batch`, reading each from `view`'s
/// snapshot of the root when there is one. Returns how many units had
/// `stage_create` called for them.
fn stage_units(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    batch: &Batch,
    assume_yes: bool,
    view: Option<&SourceView>,
) -> Result<usize> {
    // Snapshot + stage every unit in this batch, in the batch's own
    // (name-ordered) sequence.
    //
    // ADR-0012 / issue #159: `snapshot_create_detailed` may report an
    // existing version instead of minting one. `selector`/`plan` picked
    // this batch from units that looked pending at plan time, but a
    // unit's on-disk state can still match an existing row by the time
    // this loop actually runs — so `minted: false` is a normal outcome
    // here, not an error, and each of its `status` values means something
    // different for staging: explicit arms rather than one blanket
    // "skip if unchanged", which would silently stop staging a snapshot
    // that was created but never staged.
    // Issue #232 item 2: two arms below are deliberate no-ops (already
    // staged; reverted back to current) — `batch.units.len()` counts the
    // whole batch regardless, so a batch of 40 where 3 hit either no-op
    // still reported "40 unit(s) staged". Count only the arms that actually
    // call `stage_create`, and report that instead.
    //
    // Issue #368: with `[staging] jobs` above 1, every unit's snapshot is
    // taken first and the units to stage are then staged that many at once
    // (`staging::jobs::stage_many`). With 1, each unit is staged as soon as
    // its snapshot is taken, as before.
    let parallel = config.staging.jobs > 1;
    let mut to_stage: Vec<crate::staging::jobs::StageJob> = Vec::new();
    let mut stage = |name: &str, snapshot_id: i64| -> Result<()> {
        if parallel {
            to_stage.push(crate::staging::jobs::StageJob {
                unit_name: name.to_string(),
                snapshot_id,
                admission: crate::staging::Admission::Unstaged,
                view: view.cloned(),
            });
        } else {
            crate::staging::stage_create_reading(
                conn,
                paths,
                config,
                snapshot_id,
                assume_yes,
                view,
            )?;
        }
        Ok(())
    };
    let mut units_staged = 0usize;
    for u in &batch.units {
        let outcome =
            crate::staging::snapshot_create_detailed_reading(conn, &u.name, config, view)?;
        match (outcome.minted, outcome.status.as_str()) {
            // A fresh version was minted — always needs staging.
            (true, _) => {
                stage(&u.name, outcome.snapshot_id)?;
                units_staged += 1;
            }
            // Existing but never-staged content (ADR-0012 reuse, Change 3)
            // — the row already exists, but its slices don't yet.
            (false, "created") => {
                stage(&u.name, outcome.snapshot_id)?;
                units_staged += 1;
            }
            // Already staged: a stage_set with live slices exists for this
            // exact content (`stage_set_has_live_slices`) — re-staging
            // would silently produce a second, unrelated copy. Nothing to
            // do for this unit in this batch.
            (false, "staged") => {}
            // The plan saw this unit as pending, but by the time this
            // batch actually ran, its on-disk content matched the already-
            // written live version again (e.g. a revert) — a race, not an
            // error. Nothing to stage.
            (false, "current") => {
                tracing::warn!(
                    unit = %u.name,
                    version = outcome.version,
                    "planned as pending, but on-disk content now matches the \
                     live version — nothing to stage (race)"
                );
            }
            // Not reachable today (`snapshot_create_detailed` only returns
            // `minted: false` for created/staged/current) — staged
            // defensively rather than silently dropping the unit if that
            // contract ever grows another status.
            (false, other) => {
                tracing::warn!(
                    unit = %u.name,
                    status = other,
                    "unminted snapshot with an unexpected status — staging anyway"
                );
                stage(&u.name, outcome.snapshot_id)?;
                units_staged += 1;
            }
        }
    }
    if !to_stage.is_empty() {
        let outcomes = crate::staging::jobs::stage_many(
            conn,
            paths,
            config,
            &to_stage,
            config.staging.jobs,
            assume_yes,
            &mut std::io::stderr(),
            &crate::progress::stderr_println,
        )?;
        crate::staging::jobs::first_failure(outcomes)?;
    }

    Ok(units_staged)
}

/// The release step after every copy of the batch is sealed and confirmed:
/// [`release_if_covered`], with its failure turned into a warning instead
/// of an error (ADR-0012 2026-10-07 item 33). The copies stand, so a
/// catalog error here must not make `collection run` exit 2; the warning
/// names the step and `tapectl staging clean`, which releases what this
/// did not (it retains, per unit, whatever is still below its policy, as
/// this release would have).
fn release_after_copies(
    conn: &Connection,
    config: &Config,
    batch: &Batch,
    copy_labels: &[String],
) -> (Option<CleanReport>, Vec<CopyProgress>, Option<String>) {
    match release_if_covered(conn, config, batch) {
        Ok((cleaned, under_copied)) => (cleaned, under_copied, None),
        Err(e) => {
            let volume = copy_labels.last().map(String::as_str).unwrap_or("?");
            let warning = crate::error::unfinished_after_seal(
                volume,
                "releasing the batch's staging",
                &e,
                Some("tapectl staging clean"),
            );
            (None, Vec::new(), Some(warning))
        }
    }
}

/// The release decision for one batch, extracted out of [`execute_batch`]
/// (issue #284) so it can be driven by a test without `execute_batch`'s
/// device-bound write loop ahead of it — `volume::write::volume_write` has
/// no store injection (see its own doc comment) and so cannot complete
/// without a real tape drive, which this workspace's guardrails forbid.
/// Before this extraction, a regression test for the #248 scoping hand-copied
/// this tail instead of calling it, so a revert of the scope back to
/// `CleanScope::Whole` would have gone undetected.
///
/// The rule itself — release only when every unit now meets its own resolved
/// `min_copies`, scoped to exactly those units (issues #229, #248) — lives in
/// [`crate::staging::release::release_if_covered`] since issue #440, shared
/// with `volume write` and `volume resume`; this only names the batch's units
/// for it, in `batch.units` order. [`execute_batch`]'s doc comment says why
/// the gate cannot be `clean_staging`'s own guard.
fn release_if_covered(
    conn: &Connection,
    config: &Config,
    batch: &Batch,
) -> Result<(Option<CleanReport>, Vec<CopyProgress>)> {
    let unit_ids = batch_unit_ids(conn, batch)?;
    let outcome = crate::staging::release::release_if_covered(conn, config, &unit_ids)?;
    Ok((outcome.cleaned, outcome.under_copied))
}

/// This batch's own unit ids, in `batch.units` order — the `CleanScope::
/// Units` selection [`execute_batch`] hands to `staging::clean::
/// clean_staging` (issue #248) so a release can never reach past this
/// batch into a different batch's still-staged data.
///
/// This is a lookup, not a decision: it says which stage_sets
/// `clean_staging` is allowed to consider, never whether any of them may
/// actually be released — that call is
/// [`crate::staging::release::under_copied_units`], made on the same ids
/// by [`release_if_covered`] immediately before the release.
fn batch_unit_ids(conn: &Connection, batch: &Batch) -> Result<Vec<i64>> {
    let mut ids = Vec::with_capacity(batch.units.len());
    for u in &batch.units {
        let unit = crate::db::queries::get_unit_by_name(conn, &u.name)?.ok_or_else(|| {
            TapectlError::Other(format!(
                "execute_batch: unit \"{}\" is missing from the catalog right after its \
                 own batch wrote it — the catalog is inconsistent",
                u.name
            ))
        })?;
        ids.push(unit.id);
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::selector::PendingUnit;

    /// The batch's units judged against their own `min_copies` — the shared
    /// derivation ([`crate::staging::release::under_copied_units`]) over the
    /// batch's unit ids, which is what `release_if_covered` asks.
    fn under_copied_units(
        conn: &Connection,
        config: &Config,
        batch: &Batch,
    ) -> Result<Vec<CopyProgress>> {
        let unit_ids = batch_unit_ids(conn, batch)?;
        crate::staging::release::under_copied_units(conn, config, &unit_ids)
    }

    /// ADR-0012 2026-10-07 item 33: a catalog error releasing staging after
    /// the copies are sealed comes back as a warning naming the step and
    /// `tapectl staging clean`, not as an error — here the batch names a
    /// unit the catalog does not have, so the release's first query fails.
    #[test]
    fn a_failed_release_after_the_copies_is_a_warning() {
        let conn = db::open_memory().unwrap();
        let batch = Batch {
            units: vec![PendingUnit {
                name: "gone".into(),
                size_bytes: 1,
            }],
            total_bytes: 1,
            padded_bytes: 1,
        };
        let (cleaned, under, warning) =
            release_after_copies(&conn, &Config::default(), &batch, &["L6-0007".to_string()]);
        assert!(cleaned.is_none() && under.is_empty());
        let warning = warning.expect("the failure is reported as a warning");
        assert!(
            warning.starts_with(
                "warning: volume \"L6-0007\" is sealed and confirmed and counts as a copy, but \
                 releasing the batch's staging did not finish: "
            ),
            "{warning}"
        );
        assert!(warning.contains("\"gone\""), "the cause: {warning}");
        assert!(
            warning.ends_with("To finish it: tapectl staging clean"),
            "{warning}"
        );
    }

    /// ADR-0012 2026-10-07 item 31: which copies of a run get the full
    /// readback. `--full-confirm-first` reads the first only, the one-copy
    /// readback item 2 recommends; `--full-confirm` every one; neither, none.
    #[test]
    fn full_confirm_first_reads_back_only_the_first_copy() {
        let full = |c: RunConfirm| (0..3).map(|i| c.full_for_copy(i)).collect::<Vec<_>>();
        assert_eq!(full(RunConfirm::Quick), [false, false, false]);
        assert_eq!(full(RunConfirm::FullEvery), [true, true, true]);
        assert_eq!(full(RunConfirm::FullFirst), [true, false, false]);
    }

    /// The write loop asks [`RunConfirm::full_for_copy`] for each copy by
    /// its index, rather than handing one flag to every copy.
    #[test]
    fn execute_batch_asks_the_confirm_choice_per_copy() {
        const SRC: &str = include_str!("batch.rs");
        let f = "pub fn execute_batch(";
        let start = SRC.find(f).unwrap();
        let end = SRC[start..].find("\n}\n").unwrap() + start;
        let body = &SRC[start..end];
        assert!(
            !body[f.len()..].contains("\nfn ") && !body[f.len()..].contains("\npub fn "),
            "body extraction overran into another function"
        );
        assert!(body.contains("confirm.full_for_copy(copy)"), "{body}");
    }
    use crate::config::Config;
    use crate::db;
    use rusqlite::params;

    /// Seeds one active unit with exactly one `completed` write on a
    /// `sealed` volume — the DB state left behind right after
    /// `execute_batch`'s write loop lands a single copy (`volume::session`
    /// promotes the just-written snapshot to `'current'` and its volume to
    /// `'sealed'` at seal/confirm). Returns the unit's row id (unused by
    /// callers today, kept for a future test that wants it).
    fn seed_unit_with_one_completed_copy(conn: &Connection, unit_name: &str) -> i64 {
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status) \
             VALUES (?1, ?1, (SELECT id FROM tenants WHERE name = 'media'), 'mtime_size', 1, 'active')",
            params![unit_name],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size) \
             VALUES (?1, 1, 'current', '/tmp', 1, 10)",
            params![unit_id],
        )
        .unwrap();
        let snap_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
            params![snap_id],
        )
        .unwrap();
        let stage_set_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status) \
             VALUES (?1, 'lto', 'p', 10485760, 'sealed')",
            params![format!("V-{unit_name}")],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status) \
             VALUES (?1, ?2, ?3, 'completed')",
            params![stage_set_id, snap_id, volume_id],
        )
        .unwrap();
        unit_id
    }

    /// A collection with no snapshot hook: `execute_batch` reads its units
    /// where they live, as it did before the hook existed.
    fn no_hook_lib() -> CollectionConfig {
        toml::from_str("name = 'testlib'\nroot = '/nonexistent-root'\ntenant = 'alice'\n").unwrap()
    }

    fn one_unit_batch(name: &str) -> Batch {
        Batch {
            units: vec![PendingUnit {
                name: name.into(),
                size_bytes: 10,
            }],
            total_bytes: 10,
            padded_bytes: 10,
        }
    }

    /// Issue #229, change 7: a collection whose units resolve `min_copies =
    /// 1` (the config default) must still read as fully covered by the ONE
    /// copy `execute_batch` just wrote — this is the existing, correct
    /// behaviour the fix below must not break.
    #[test]
    fn under_copied_units_is_empty_when_min_copies_one_is_already_met() {
        let conn = db::open_memory().unwrap();
        seed_unit_with_one_completed_copy(&conn, "testlib/alpha");

        let mut config = Config::default();
        config.defaults.min_copies = 1;

        let batch = one_unit_batch("testlib/alpha");

        let under = under_copied_units(&conn, &config, &batch).unwrap();
        assert!(
            under.is_empty(),
            "one completed copy must satisfy min_copies=1: {under:?}"
        );
    }

    /// Issue #229's second half (the pre-existing defect the ruling
    /// exposed): a unit whose resolved `min_copies` is 2 is NOT satisfied
    /// by the one copy `execute_batch` just wrote. Before this fix,
    /// `execute_batch` called `clean_staging(force = false)`
    /// unconditionally right after that same single write, and the
    /// non-force guard passed vacuously (exactly one `writes` row, and it
    /// is `'completed'`) — so the staged bytes were released regardless of
    /// policy.
    #[test]
    fn under_copied_units_reports_progress_when_min_copies_two_is_not_yet_met() {
        let conn = db::open_memory().unwrap();
        seed_unit_with_one_completed_copy(&conn, "testlib/alpha");

        let mut config = Config::default();
        config.defaults.min_copies = 2;

        let batch = one_unit_batch("testlib/alpha");

        let under = under_copied_units(&conn, &config, &batch).unwrap();
        assert_eq!(under.len(), 1, "{under:?}");
        assert_eq!(under[0].unit_name, "testlib/alpha");
        assert_eq!(under[0].copies, 1);
        assert_eq!(under[0].min_copies, 2);
    }

    /// ADR-0012 2026-10-12 item 43: the release gate asks the floor
    /// `staging clean` retains by, the one tape-only rule's. One copy meets
    /// `min_copies = 1` for an active unit (the positive control); tape-only
    /// at 3x it needs 3.
    #[test]
    fn under_copied_units_takes_the_tape_only_multiplier() {
        let conn = db::open_memory().unwrap();
        seed_unit_with_one_completed_copy(&conn, "testlib/alpha");
        let mut config = Config::default();
        config.defaults.min_copies = 1;
        config.compaction.tape_only_safety_multiplier = 3;
        let batch = one_unit_batch("testlib/alpha");

        assert!(under_copied_units(&conn, &config, &batch)
            .unwrap()
            .is_empty());

        conn.execute("UPDATE units SET status = 'tape_only'", [])
            .unwrap();
        let under = under_copied_units(&conn, &config, &batch).unwrap();
        assert_eq!(under.len(), 1, "{under:?}");
        assert_eq!((under[0].copies, under[0].min_copies), (1, 3));
    }

    /// Like [`seed_unit_with_one_completed_copy`], but also creates a real
    /// staged `.age` file on disk with a `stage_slices` row pointing at it
    /// — what `clean_staging` actually reads and unlinks. Returns `(conn,
    /// stage_set_id, staged_file_path, TempDir guard)`; the guard must
    /// outlive the assertions or the directory is removed early.
    fn seed_unit_with_one_completed_copy_and_staged_file(
        unit_name: &str,
    ) -> (Connection, i64, std::path::PathBuf, tempfile::TempDir) {
        let conn = db::open_memory().unwrap();
        seed_unit_with_one_completed_copy(&conn, unit_name);
        let stage_set_id: i64 = conn
            .query_row("SELECT id FROM stage_sets", [], |r| r.get(0))
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("slice_1.age");
        std::fs::write(&path, b"staged slice bytes").unwrap();
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                        sha256_plain, sha256_encrypted, staging_path)
             VALUES (?1, 1, 19, 19, 'deadbeef', 'deadbeef', ?2)",
            params![stage_set_id, path.to_string_lossy()],
        )
        .unwrap();

        (conn, stage_set_id, path, dir)
    }

    fn config_with_staging_dir(dir: &std::path::Path, min_copies: i32) -> Config {
        let mut config = Config {
            staging: crate::config::StagingConfig {
                directory: dir.to_string_lossy().to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        config.defaults.min_copies = min_copies;
        config
    }

    /// Issue #229, change 6: the recipe the refusal message now names
    /// (swap cartridges, then `tapectl volume write <label2>`) is verified
    /// END TO END here, not reasoned about. This calls [`release_if_covered`]
    /// directly — the ONE place `execute_batch`'s own release/scope decision
    /// now lives (issue #284) — rather than hand-copying its body, and then
    /// checks the two facts a second `volume write` actually depends on:
    /// `stage_sets.status` is still `'staged'` (the exact precondition
    /// `find_staged_data`'s `WHERE ss.status = 'staged'`,
    /// `src/volume/write.rs`, requires), and the physical `.age` file
    /// `stage_slices.staging_path` points at is still on disk. Before issue
    /// #229's fix, `execute_batch` called `clean_staging(force = false)`
    /// unconditionally right after the single write this batch just did,
    /// and the non-force guard passed vacuously (one `writes` row,
    /// `'completed'`) — see
    /// `staging::clean::tests::default_guard_cleans_when_the_only_planned_copy_completed`
    /// for that same guard pinned green on exactly this shape.
    #[test]
    fn release_gate_retains_staged_bytes_when_a_unit_is_still_under_copied() {
        let (conn, stage_set_id, staged_file, dir_guard) =
            seed_unit_with_one_completed_copy_and_staged_file("testlib/alpha");
        let config = config_with_staging_dir(dir_guard.path(), 2);
        let batch = one_unit_batch("testlib/alpha");

        let (cleaned, under_copied) = release_if_covered(&conn, &config, &batch).unwrap();
        assert_eq!(under_copied.len(), 1, "{under_copied:?}");
        assert!(
            cleaned.is_none(),
            "must not release while a unit is still under-copied"
        );

        let status: String = conn
            .query_row(
                "SELECT status FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "staged",
            "a still-under-copied unit's stage_set must stay 'staged' so a second \
             `tapectl volume write <label2>` can find it"
        );
        assert!(
            staged_file.exists(),
            "the staged .age file must survive on disk for a second copy to consume"
        );
    }

    /// Issue #229, change 7 — the negative control for the test above: a
    /// collection whose units resolve `min_copies = 1` must still
    /// auto-release after its single copy, exactly as before this fix. This
    /// is the behaviour the retention gate above could most easily break.
    /// Calls [`release_if_covered`] directly (issue #284) — with only one
    /// unit in the whole database here, the #248 scoping cannot change the
    /// outcome, but the test now exercises the actual function
    /// `execute_batch` calls, not a hand-copied stand-in.
    #[test]
    fn release_gate_cleans_staged_bytes_when_min_copies_one_is_already_met() {
        let (conn, stage_set_id, staged_file, dir_guard) =
            seed_unit_with_one_completed_copy_and_staged_file("testlib/alpha");
        let config = config_with_staging_dir(dir_guard.path(), 1);
        let batch = one_unit_batch("testlib/alpha");

        let (cleaned, under_copied) = release_if_covered(&conn, &config, &batch).unwrap();
        assert!(under_copied.is_empty(), "{under_copied:?}");
        assert!(
            cleaned.is_some(),
            "must release once every unit meets its own min_copies"
        );

        let status: String = conn
            .query_row(
                "SELECT status FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "cleaned",
            "min_copies=1 is already met by the one copy just written -- this must \
             still auto-release exactly as it did before issue #229"
        );
        assert!(
            !staged_file.exists(),
            "the staged .age file must be removed"
        );
    }

    /// Seeds one unit with `copies` distinct `completed` writes (each on its
    /// own `sealed` volume) and one real staged `.age` file physically
    /// inside `dir`, backed by a `stage_slices` row — the exact thing
    /// `clean_staging` reads and unlinks. Unlike
    /// [`seed_unit_with_one_completed_copy_and_staged_file`] (which always
    /// opens its own fresh `conn`), this takes an existing `conn`/tenant so
    /// TWO units can be seeded side by side in one database — required to
    /// reproduce issue #248, where the defect is specifically about a
    /// second unit OUTSIDE the batch under test. Every identifier that must
    /// be unique across two calls on the same `conn` (unit uuid/name, the
    /// staged file's name, volume labels) is namespaced by `unit_name`.
    /// Returns `(stage_set_id, staged_file_path)`.
    fn seed_unit_with_n_completed_copies_and_staged_file(
        conn: &Connection,
        tenant_id: i64,
        dir: &std::path::Path,
        unit_name: &str,
        copies: i64,
    ) -> (i64, std::path::PathBuf) {
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status) \
             VALUES (?1, ?1, ?2, 'mtime_size', 1, 'active')",
            params![unit_name, tenant_id],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size) \
             VALUES (?1, 1, 'current', '/tmp', 1, 10)",
            params![unit_id],
        )
        .unwrap();
        let snap_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
            params![snap_id],
        )
        .unwrap();
        let stage_set_id = conn.last_insert_rowid();

        let safe_name = unit_name.replace('/', "_");
        let staged_path = dir.join(format!("{safe_name}_slice_1.age"));
        std::fs::write(&staged_path, b"staged slice bytes").unwrap();
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                        sha256_plain, sha256_encrypted, staging_path)
             VALUES (?1, 1, 19, 19, 'deadbeef', 'deadbeef', ?2)",
            params![stage_set_id, staged_path.to_string_lossy()],
        )
        .unwrap();

        for i in 0..copies {
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status) \
                 VALUES (?1, 'lto', 'p', 10485760, 'sealed')",
                params![format!("V-{safe_name}-{i}")],
            )
            .unwrap();
            let volume_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status) \
                 VALUES (?1, ?2, ?3, 'completed')",
                params![stage_set_id, snap_id, volume_id],
            )
            .unwrap();
        }

        (stage_set_id, staged_path)
    }

    /// Issue #248 — the negative control: the release gate is computed only
    /// over THIS batch's units (`under_copied_units(batch)`), but an
    /// unscoped release call, `staging::clean::clean_staging(conn, config,
    /// false, CleanScope::Whole)`, has no batch/unit predicate at all — it
    /// sweeps every eligible `'staged'` stage_set in the whole database. So
    /// a batch containing only a fully-covered unit can still discard a
    /// DIFFERENT batch's still-under-copied staged ciphertext, silently
    /// destroying the only cheap route to the copy that unit's own policy
    /// still requires.
    ///
    /// Fixture: one `conn`/`config` (both units resolve the SAME
    /// `min_copies = 2` off the config default — there is no per-unit
    /// override here) holding two units:
    /// - `testlib/alpha`, THE ONLY unit in `batch`: 2/2 completed copies —
    ///   fully covered.
    /// - `testlib/beta`, NOT in `batch` and never named to
    ///   `under_copied_units`: 1/2 completed copies — still under policy.
    ///
    /// Issue #284: this now calls [`release_if_covered`] directly — the
    /// single place `execute_batch`'s release/scope decision lives — rather
    /// than hand-copying its body (its own tests previously called
    /// `under_copied_units` then `clean_staging` inline, which meant this
    /// test exercised a SEPARATE, hand-maintained copy of the scoping logic
    /// and would have stayed green even if `execute_batch` itself regressed
    /// to an unscoped release; see `execute_batch_still_delegates_release_
    /// to_the_scoped_helper` below for the complementary proof that
    /// `execute_batch` still calls this function at all). Against an
    /// unscoped release, beta's staged file is deleted too — its lone write
    /// is `'completed'` and no other write references its stage_set, so the
    /// non-force guard passes vacuously, identical to the pre-#229 defect
    /// this module already pins, except now the "other planned copy still
    /// pending" is a DIFFERENT unit's batch rather than a second write on
    /// the same stage_set.
    #[test]
    fn release_gate_must_not_release_another_batchs_under_copied_unit() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();

        let dir_guard = tempfile::tempdir().unwrap();
        let config = config_with_staging_dir(dir_guard.path(), 2);

        // alpha: THE batch under test, 2/2 copies -- fully covered.
        let (_alpha_stage_set_id, alpha_staged_file) =
            seed_unit_with_n_completed_copies_and_staged_file(
                &conn,
                tenant_id,
                dir_guard.path(),
                "testlib/alpha",
                2,
            );

        // beta: a DIFFERENT batch's unit, 1/2 copies -- still under policy.
        // Deliberately never passed to `one_unit_batch` below, exactly as
        // it would not be if it belonged to some other batch entirely.
        let (_beta_stage_set_id, beta_staged_file) =
            seed_unit_with_n_completed_copies_and_staged_file(
                &conn,
                tenant_id,
                dir_guard.path(),
                "testlib/beta",
                1,
            );

        let batch = one_unit_batch("testlib/alpha");

        let (cleaned, under_copied) = release_if_covered(&conn, &config, &batch).unwrap();
        assert!(
            under_copied.is_empty(),
            "alpha alone must look fully covered to its own batch's gate: {under_copied:?}"
        );
        assert!(
            cleaned.is_some(),
            "alpha's batch is fully covered and must release"
        );

        assert!(
            !alpha_staged_file.exists(),
            "alpha is fully covered and IS this batch -- its staged bytes should be released"
        );
        assert!(
            beta_staged_file.exists(),
            "beta is NOT in this batch and is still under its own min_copies (1/2) -- \
             a release scoped to alpha's batch must not touch beta's staged bytes (issue #248)"
        );
    }

    /// Issue #440, the pin taken BEFORE the release rule was lifted out of
    /// this module: the batch's release is all-or-nothing. Two units in one
    /// batch, `alpha` covered (2/2) and `beta` short (1/2): NEITHER is
    /// released and only `beta` is named. Stage once, write N times (§11) —
    /// a copy two that finds only `beta` staged would not carry the same
    /// content as copy one.
    #[test]
    fn a_batch_with_one_short_unit_releases_none_of_its_units() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        let dir_guard = tempfile::tempdir().unwrap();
        let config = config_with_staging_dir(dir_guard.path(), 2);
        let (_, alpha_file) = seed_unit_with_n_completed_copies_and_staged_file(
            &conn,
            tenant_id,
            dir_guard.path(),
            "testlib/alpha",
            2,
        );
        let (_, beta_file) = seed_unit_with_n_completed_copies_and_staged_file(
            &conn,
            tenant_id,
            dir_guard.path(),
            "testlib/beta",
            1,
        );
        let batch = Batch {
            units: vec![
                PendingUnit {
                    name: "testlib/alpha".into(),
                    size_bytes: 10,
                },
                PendingUnit {
                    name: "testlib/beta".into(),
                    size_bytes: 10,
                },
            ],
            total_bytes: 20,
            padded_bytes: 20,
        };

        let (cleaned, under_copied) = release_if_covered(&conn, &config, &batch).unwrap();
        assert!(cleaned.is_none(), "one short unit keeps the whole batch");
        assert_eq!(under_copied.len(), 1, "{under_copied:?}");
        assert_eq!(
            (
                under_copied[0].unit_name.as_str(),
                under_copied[0].copies,
                under_copied[0].min_copies
            ),
            ("testlib/beta", 1, 2)
        );
        assert!(
            alpha_file.exists() && beta_file.exists(),
            "neither unit's staged bytes are released while any unit of the batch is short"
        );
    }

    /// Issue #284, P2: what the three [`release_if_covered`] tests above
    /// cannot prove — that [`execute_batch`] still actually calls
    /// `release_if_covered` for its release step, rather than an unscoped
    /// `clean_staging` call inlined back into it (which would leave every
    /// test above green while reintroducing exactly the #248 defect they
    /// exist to catch). Same source-scan shape as
    /// `src/volume/write.rs`'s `the_two_write_contacts_still_corroborate`
    /// and `volume_write_records_mam_facts_only_after_the_tape_side_
    /// refusals` (write.rs:8574, :8612) — a house pattern in this codebase
    /// for pinning an ordering/wiring property of a function that cannot be
    /// driven end-to-end without a tape drive, not an improvisation here.
    #[test]
    fn execute_batch_still_delegates_release_to_the_scoped_helper() {
        const SRC: &str = include_str!("batch.rs");
        let f = "pub fn execute_batch(";
        let start = SRC.find(f).unwrap_or_else(|| panic!("no fn {f}"));
        // Function bodies end at the first `\n}` in column 0.
        let end = SRC[start..].find("\n}\n").unwrap() + start;
        let body = &SRC[start..end];
        // Guard the FALSE PASS: if the `\n}\n` scan ever ran past the end of
        // `execute_batch`, `body` could pick up a sibling function's own
        // content and report a deleted call as present. Checked against
        // both `fn ` and `pub fn ` (unlike the write.rs precedent, the
        // functions immediately after `execute_batch` in this file --
        // `under_copied_units`, `release_if_covered`, `batch_unit_ids` --
        // are private, not `pub`).
        assert!(
            !body[f.len()..].contains("\nfn ") && !body[f.len()..].contains("\npub fn "),
            "body extraction overran into another function; fix this test's scan \
             before trusting its verdict"
        );
        // ADR-0012 2026-10-07 item 33: the release goes through
        // `release_after_copies`, which turns a failure into a warning, and
        // that calls `release_if_covered`.
        assert!(
            body.contains("release_after_copies(")
                && SRC.contains("    match release_if_covered(conn, config, batch) {")
                // Issue #440: and that one is the shared rule `volume write`
                // and `volume resume` use, not a copy of it.
                && SRC.contains(
                    "crate::staging::release::release_if_covered(conn, config, &unit_ids)?"
                ),
            "execute_batch no longer delegates its release step to release_if_covered \
             -- the #248 scope decision must live in exactly one place (issue #284)"
        );
        assert!(
            !body.contains("release_if_covered("),
            "called past the warning path"
        );
        assert!(
            !body.contains("CleanScope::"),
            "execute_batch references CleanScope directly -- an unscoped clean_staging \
             call has been inlined back into it, bypassing release_if_covered's own \
             scoping (issue #248/#284)"
        );
    }

    // ── issue #354 (b): the global --yes reaches the staging-space question ──

    /// A real, stageable unit `unit1` (source on disk, tenant, escrow
    /// recipient) with compression on, staging reported as 1 KiB free —
    /// so `stage_create` asks about staging space before it reads the
    /// source — and one `sealed` destination, which `volume_write` refuses
    /// as not a write target before it resolves a backend or touches a
    /// device (ADR-0012). Returns the tempdir guard and a device path inside
    /// it that does not exist.
    fn stageable_batch_fixture() -> (Connection, TapectlPaths, Config, tempfile::TempDir, String) {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();
        let conn = db::open(&paths.db_file).unwrap();
        crate::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('escrow-holder', 0, 'active')",
            [],
        )
        .unwrap();
        let holder = conn.last_insert_rowid();
        let kp = crate::crypto::keys::generate_keypair();
        crate::db::queries::insert_escrow_key(
            &conn,
            holder,
            "test-escrow",
            &kp.fingerprint,
            &kp.public_key,
            None,
        )
        .unwrap();

        let staging = tmp.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let mut config = config_with_staging_dir(&staging, 1);
        config.dar.binary = "dar".to_string();
        config.defaults.compression = "gzip".to_string();

        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("f.txt"), b"collection batch content").unwrap();
        crate::unit::init_unit(
            &conn,
            &paths,
            src.to_str().unwrap(),
            "alice",
            Some("unit1"),
            &[],
            None,
        )
        .unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status) \
             VALUES ('VOL-B', 'lto', 'lto0', 10485760, 'sealed')",
            [],
        )
        .unwrap();
        let device = tmp
            .path()
            .join("no-such-tape")
            .to_string_lossy()
            .into_owned();
        (conn, paths, config, tmp, device)
    }

    fn stage_set_statuses(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT status FROM stage_sets ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    }

    /// Issue #368: with `[staging] jobs = 2`, `collection run` stages the
    /// batch's units two at a time, and each is staged exactly as one at a
    /// time would stage it.
    #[test]
    fn execute_batch_stages_jobs_units_at_once() {
        let (conn, paths, mut config, tmp, device) = stageable_batch_fixture();
        // A second unit, beside `unit1`; both large enough to overlap.
        let src2 = tmp.path().join("src2");
        std::fs::create_dir_all(&src2).unwrap();
        for (dir, n) in [(tmp.path().join("src"), 6), (src2.clone(), 6)] {
            for f in 0..n {
                let data: Vec<u8> = (0..400_000usize)
                    .map(|j| ((j * 7 + f) % 251) as u8)
                    .collect();
                std::fs::write(dir.join(format!("big{f}.bin")), data).unwrap();
            }
        }
        crate::unit::init_unit(
            &conn,
            &paths,
            src2.to_str().unwrap(),
            "alice",
            Some("unit2"),
            &[],
            None,
        )
        .unwrap();
        config.staging.jobs = 2;
        config.defaults.compression = "none".to_string();
        crate::staging::jobs::MEMORY_OVERRIDE.with(|m| m.set(Some(1 << 40)));
        let batch = Batch {
            units: ["unit1", "unit2"]
                .iter()
                .map(|n| PendingUnit {
                    name: (*n).into(),
                    size_bytes: 10,
                })
                .collect(),
            total_bytes: 20,
            padded_bytes: 20,
        };
        let err = execute_batch(
            &conn,
            &paths,
            &config,
            &no_hook_lib(),
            &batch,
            &["VOL-B".to_string()],
            &device,
            512 * 1024,
            false,
            RunConfirm::Quick,
            true,
        )
        .expect_err("the sealed destination is not a write target");
        assert!(
            matches!(err, TapectlError::VolumeNotWriteTarget { .. }),
            "both units staged, then the write: {err}"
        );
        assert_eq!(
            stage_set_statuses(&conn),
            vec!["staged".to_string(), "staged".to_string()]
        );
        assert_eq!(
            crate::staging::jobs::in_flight::most(
                &crate::staging::lock::db_file_of(&conn).unwrap()
            ),
            2,
            "the two units were staged at once"
        );
    }

    /// Issue #354 (b): `execute_batch` carries the global `--yes` to
    /// `stage_create` for a freshly minted version (the `(true, _)` arm).
    /// It gets past the staging question — the unit is staged — and stops
    /// at the sealed destination, before any device.
    #[test]
    fn execute_batch_carries_yes_to_the_staging_space_question_for_a_new_version() {
        let (conn, paths, config, _tmp, device) = stageable_batch_fixture();
        let _free = crate::staging::FreeSpaceOverride::set(1024);

        let err = execute_batch(
            &conn,
            &paths,
            &config,
            &no_hook_lib(),
            &one_unit_batch("unit1"),
            &["VOL-B".to_string()],
            &device,
            512 * 1024,
            false,
            RunConfirm::Quick,
            true,
        )
        .expect_err("the sealed destination is not a write target");
        assert!(
            matches!(err, TapectlError::VolumeNotWriteTarget { .. }),
            "--yes answered the staging question, so the batch reached the write: {err}"
        );
        assert_eq!(stage_set_statuses(&conn), vec!["staged".to_string()]);
    }

    /// Issue #354 (b), the `(false, "created")` arm: without `--yes` a
    /// non-interactive batch is refused at the staging question, leaving
    /// its version `created` and unstaged; re-run with `--yes`, the same
    /// version is staged. (`cfg(test)` makes stdin a non-terminal.)
    #[test]
    fn execute_batch_carries_yes_to_the_staging_space_question_for_an_unstaged_version() {
        let (conn, paths, config, _tmp, device) = stageable_batch_fixture();
        let _free = crate::staging::FreeSpaceOverride::set(1024);
        let batch = one_unit_batch("unit1");
        let labels = ["VOL-B".to_string()];

        let err = execute_batch(
            &conn,
            &paths,
            &config,
            &no_hook_lib(),
            &batch,
            &labels,
            &device,
            512 * 1024,
            false,
            RunConfirm::Quick,
            false,
        )
        .expect_err("no terminal and no --yes: a stage that may not fit is refused");
        let msg = err.to_string();
        assert!(
            msg.contains("stage unit \"unit1\" refused: non-interactive session")
                && msg.contains("re-run with --yes"),
            "the staging-space consent refusal: {msg}"
        );
        assert!(stage_set_statuses(&conn).is_empty(), "nothing staged");
        let status: String = conn
            .query_row("SELECT status FROM snapshots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            status, "created",
            "fixture: the second run takes the unstaged arm"
        );

        let err = execute_batch(
            &conn,
            &paths,
            &config,
            &no_hook_lib(),
            &batch,
            &labels,
            &device,
            512 * 1024,
            false,
            RunConfirm::Quick,
            true,
        )
        .expect_err("the sealed destination is not a write target");
        assert!(
            matches!(err, TapectlError::VolumeNotWriteTarget { .. }),
            "--yes answered the staging question, so the batch reached the write: {err}"
        );
        assert_eq!(stage_set_statuses(&conn), vec!["staged".to_string()]);
    }

    // ── ADR-0012 2026-10-16 item 75 (#455): the per-collection snapshot hook ──

    /// The hook's world: `stageable_batch_fixture`'s tenants, escrow and
    /// staging directory, plus a collection root `root/` holding the units
    /// `alpha` (two files, one in a subfolder) and `beta`, and a separate
    /// directory `snaps/` for the snapshot a fake `snapshot_command` makes —
    /// never under the root and never under staging.
    struct HookWorld {
        conn: Connection,
        paths: TapectlPaths,
        config: Config,
        tmp: tempfile::TempDir,
        root: std::path::PathBuf,
        snaps: tempfile::TempDir,
    }

    impl HookWorld {
        fn new() -> Self {
            let (conn, paths, config, tmp, _device) = stageable_batch_fixture();
            let root = tmp.path().join("root");
            for (unit, files) in [
                (
                    "alpha",
                    &[("f.txt", "alpha original"), ("sub/g.txt", "alpha nested")][..],
                ),
                ("beta", &[("f.txt", "beta original")][..]),
            ] {
                for (name, text) in files {
                    let path = root.join(unit).join(name);
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, text).unwrap();
                }
                crate::unit::init_unit(
                    &conn,
                    &paths,
                    root.join(unit).to_str().unwrap(),
                    "alice",
                    Some(unit),
                    &[],
                    None,
                )
                .unwrap();
            }
            let root = std::fs::canonicalize(root).unwrap();
            Self {
                conn,
                paths,
                config,
                tmp,
                root,
                snaps: tempfile::TempDir::new().unwrap(),
            }
        }

        /// Where the fake snapshot command puts its copy.
        fn copy(&self) -> std::path::PathBuf {
            self.snaps.path().join("copy")
        }

        /// A collection over the root. `then` runs after the copy is made
        /// and before the path is printed (a live-source change, a damaged
        /// copy); `release` is the `release_command`, if any.
        fn lib(&self, then: &str, release: Option<&str>) -> CollectionConfig {
            let copy = self.copy();
            let mut lib = no_hook_lib();
            lib.name = "coll".into();
            lib.root = self.root.to_string_lossy().into_owned();
            lib.snapshot_command = Some(format!(
                "rm -rf '{c}' && cp -a \"$TAPECTL_COLLECTION_ROOT\" '{c}' && {then} printf '%s\\n' '{c}'",
                c = copy.display()
            ));
            lib.release_command = release.map(str::to_string);
            lib
        }

        /// The `release_command` that appends one line to `releases.log`.
        fn recording_release(&self) -> (String, std::path::PathBuf) {
            let log = self.snaps.path().join("releases.log");
            (
                format!(
                    "printf '%s|%s|%s\\n' \"$TAPECTL_COLLECTION\" \"$TAPECTL_COLLECTION_ROOT\" \
                     \"$TAPECTL_SNAPSHOT_PATH\" >> '{}'",
                    log.display()
                ),
                log,
            )
        }

        fn batch(&self) -> Batch {
            Batch {
                units: ["alpha", "beta"]
                    .iter()
                    .map(|n| PendingUnit {
                        name: (*n).into(),
                        size_bytes: 10,
                    })
                    .collect(),
                total_bytes: 20,
                padded_bytes: 20,
            }
        }

        fn stage(&self, lib: &CollectionConfig, assume_yes: bool) -> Result<StagedBatch> {
            stage_batch(
                &self.conn,
                &self.paths,
                &self.config,
                lib,
                &self.batch(),
                assume_yes,
            )
        }

        fn count(&self, table: &str) -> i64 {
            self.conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap()
        }

        fn snapshot_id_of(&self, unit: &str) -> i64 {
            self.conn
                .query_row(
                    "SELECT s.id FROM snapshots s JOIN units u ON u.id = s.unit_id
                      WHERE u.name = ?1",
                    [unit],
                    |r| r.get(0),
                )
                .unwrap()
        }
    }

    fn sha256_of(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// Acceptance (a): a living file changed AFTER the hook took its snapshot
    /// does not refuse the stage, and what is staged is the snapshot's
    /// content: the recorded sha256 is the original bytes', and the change
    /// (a different size) would have been refused had the live file been read.
    #[test]
    fn a_live_file_changed_after_the_snapshot_does_not_refuse_the_stage() {
        let w = HookWorld::new();
        let lib = w.lib(
            "echo 'changed after the snapshot was taken, and longer' > \
             \"$TAPECTL_COLLECTION_ROOT/alpha/f.txt\" && \
             echo 'beta changed too, and longer' > \"$TAPECTL_COLLECTION_ROOT/beta/f.txt\" &&",
            None,
        );
        let staged = w.stage(&lib, true).expect("the snapshot is what is staged");
        assert_eq!(staged.units_staged, 2);
        assert_eq!(
            staged.snapshot_path.as_deref(),
            Some(std::fs::canonicalize(w.copy()).unwrap().to_str().unwrap())
        );
        assert_eq!(staged.release_warning, None);
        assert_eq!(
            stage_set_statuses(&w.conn),
            vec!["staged".to_string(), "staged".to_string()]
        );

        for (unit, original) in [("alpha", "alpha original"), ("beta", "beta original")] {
            let live = std::fs::read(w.root.join(unit).join("f.txt")).unwrap();
            assert_ne!(
                live,
                original.as_bytes(),
                "fixture: the live file did change"
            );
            let recorded =
                crate::db::files::fixture::sha256(&w.conn, w.snapshot_id_of(unit), "f.txt");
            assert_eq!(
                recorded,
                Some(sha256_of(original.as_bytes())),
                "{unit}: the staged bytes are the snapshot's"
            );
            assert_ne!(recorded, Some(sha256_of(&live)));
        }
    }

    /// The control for the test above: with no snapshot, the same change
    /// between snapshot and stage is refused as DIRTY. The change is real; it
    /// is the hook that made it irrelevant.
    #[test]
    fn without_a_snapshot_the_same_change_refuses_the_stage() {
        let w = HookWorld::new();
        let id = crate::staging::snapshot_create(&w.conn, "alpha", &w.config).unwrap();
        std::fs::write(
            w.root.join("alpha/f.txt"),
            "changed after the snapshot was taken, and longer",
        )
        .unwrap();
        let err = crate::staging::stage_create(&w.conn, &w.paths, &w.config, id, true)
            .expect_err("a live file at another size is refused");
        assert!(err.to_string().contains("DIRTY"), "{err}");
    }

    /// Acceptance (b): every path the run RECORDS is the unit's real one. The
    /// unit's path, the snapshot's source path, the file list's relative
    /// paths, `stage_sets.dar_command` (MANIFEST.toml carries it to tape) and
    /// the stage report never name the snapshot's directory. dar's own report
    /// keeps the command as it ran, and does: that is the one place the
    /// operator can see what dar read.
    #[test]
    fn every_recorded_path_stays_the_units_real_one() {
        let w = HookWorld::new();
        let lib = w.lib("", None);
        w.stage(&lib, true).unwrap();
        let snapshot_dir = std::fs::canonicalize(w.copy()).unwrap();
        let snapshot_dir = snapshot_dir.to_str().unwrap();

        for unit in ["alpha", "beta"] {
            let live = w.root.join(unit).to_string_lossy().into_owned();
            let current: String = w
                .conn
                .query_row(
                    "SELECT current_path FROM units WHERE name = ?1",
                    [unit],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(current, live);
            let (source, dar_command): (String, String) = w
                .conn
                .query_row(
                    "SELECT s.source_path, ss.dar_command FROM snapshots s
                       JOIN units u ON u.id = s.unit_id
                       JOIN stage_sets ss ON ss.snapshot_id = s.id
                      WHERE u.name = ?1",
                    [unit],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(source, live, "{unit}: snapshots.source_path");
            assert!(
                dar_command.contains(&format!("\"-R\" {live:?}")),
                "{unit}: dar -R names the real folder: {dar_command}"
            );
            assert!(
                !dar_command.contains(snapshot_dir),
                "{unit}: dar_command: {dar_command}"
            );
            let files = crate::db::files::fixture::non_dirs(&w.conn, w.snapshot_id_of(unit));
            assert!(!files.is_empty());
            for (path, _) in &files {
                assert!(
                    !path.starts_with('/') && !path.contains(snapshot_dir),
                    "{unit}: a relative path in the files table: {path}"
                );
            }
        }
        let alpha_files: Vec<String> =
            crate::db::files::fixture::non_dirs(&w.conn, w.snapshot_id_of("alpha"))
                .into_iter()
                .map(|(p, _)| p)
                .collect();
        assert!(
            alpha_files.contains(&"sub/g.txt".to_string()),
            "{alpha_files:?}"
        );

        let reports: Vec<_> = std::fs::read_dir(&w.paths.stage_reports_dir)
            .unwrap()
            .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
            .collect();
        assert_eq!(reports.len(), 2);
        assert!(
            reports.iter().all(|r| !r.contains(snapshot_dir)),
            "{reports:?}"
        );

        let ran: String = w
            .conn
            .query_row(
                "SELECT dar_command FROM dar_create_reports ORDER BY id LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(ran.contains(snapshot_dir), "dar's own record: {ran}");
    }

    /// Acceptance (c): `release_command` runs once the staging is over, with
    /// the collection's name, its (canonical) root and the snapshot's path in
    /// its environment, on success...
    #[test]
    fn the_release_command_runs_after_staging_with_the_snapshot_path() {
        let w = HookWorld::new();
        let (release, log) = w.recording_release();
        w.stage(&w.lib("", Some(&release)), true).unwrap();
        let lines = std::fs::read_to_string(&log).unwrap();
        let snapshot = std::fs::canonicalize(w.copy()).unwrap();
        assert_eq!(
            lines,
            format!("coll|{}|{}\n", w.root.display(), snapshot.display())
        );
    }

    /// ...and when the stage is refused (here at the staging-space question,
    /// which a non-interactive run without `--yes` cannot answer, after the
    /// snapshot was taken and the version minted), or a unit's folder is not
    /// in the snapshot. The error is the stage's own and the release still
    /// ran.
    #[test]
    fn the_release_command_runs_when_the_stage_is_refused() {
        let w = HookWorld::new();
        let (release, log) = w.recording_release();
        let _free = crate::staging::FreeSpaceOverride::set(1024);
        let err = w
            .stage(&w.lib("", Some(&release)), false)
            .expect_err("no terminal and no --yes: a stage that may not fit is refused");
        assert!(err.to_string().contains("non-interactive session"), "{err}");
        assert_eq!(w.count("stage_sets"), 0, "nothing staged");
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);

        let w = HookWorld::new();
        let (release, log) = w.recording_release();
        // A copy that lost a unit's folder, so the stage of `beta` finds none.
        let mut lib = w.lib("", Some(&release));
        lib.snapshot_command = Some(format!(
            "rm -rf '{c}' && cp -a \"$TAPECTL_COLLECTION_ROOT\" '{c}' && \
             rm -rf '{c}/beta' && printf '%s\\n' '{c}'",
            c = w.copy().display()
        ));
        let err = w
            .stage(&lib, true)
            .expect_err("beta has no folder in the snapshot");
        assert!(
            err.to_string().contains("unit \"beta\"")
                && err.to_string().contains("has no folder in the snapshot"),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
    }

    /// A `release_command` that fails is a warning, not an error: the units
    /// are staged, the warning names the command, its exit status and stderr,
    /// and where the snapshot is.
    #[test]
    fn a_failing_release_command_is_a_warning() {
        let w = HookWorld::new();
        let staged = w
            .stage(&w.lib("", Some("echo 'zfs busy' >&2; exit 3")), true)
            .expect("the staged work stands");
        assert_eq!(staged.units_staged, 2);
        let warning = staged.release_warning.expect("a warning");
        for part in [
            "release_command",
            "exit status: 3",
            "zfs busy",
            "may still exist",
            w.copy().to_str().unwrap(),
        ] {
            assert!(warning.contains(part), "{part}: {warning}");
        }
        assert_eq!(w.count("stage_sets"), 2);
    }

    /// Acceptance (d): each way a `snapshot_command` can fail refuses the run
    /// before any unit is read: the error names the command and what it said,
    /// no snapshot is minted, nothing is staged, staging is untouched, and the
    /// `release_command` is not run (nothing was taken to release).
    #[test]
    fn a_failing_snapshot_command_refuses_before_any_unit_is_read() {
        let w = HookWorld::new();
        let (release, log) = w.recording_release();
        let file = w.snaps.path().join("a-file");
        std::fs::write(&file, "x").unwrap();
        let cases: Vec<(&str, String, Vec<String>)> = vec![
            (
                "non-zero exit",
                "echo 'lvcreate: no space left in the volume group' >&2; exit 5".into(),
                vec![
                    "exit status: 5".into(),
                    "lvcreate: no space left in the volume group".into(),
                ],
            ),
            (
                "no line",
                "true".into(),
                vec!["printed nothing on stdout".into()],
            ),
            (
                "two lines",
                "echo /one; echo /two".into(),
                vec!["printed 2 lines".into()],
            ),
            (
                "relative path",
                "echo snap/dir".into(),
                vec!["not an absolute path".into()],
            ),
            (
                "missing path",
                "echo /nonexistent/snapshot".into(),
                vec!["which cannot be read".into()],
            ),
            (
                "not a directory",
                format!("echo '{}'", file.display()),
                vec!["which is not a directory".into()],
            ),
            (
                "inside the live root",
                "echo \"$TAPECTL_COLLECTION_ROOT/alpha\"".into(),
                vec!["inside the live root".into()],
            ),
            (
                "the live root itself",
                "echo \"$TAPECTL_COLLECTION_ROOT\"".into(),
                vec!["inside the live root".into()],
            ),
        ];
        for (what, command, needles) in cases {
            let mut lib = w.lib("", Some(&release));
            lib.snapshot_command = Some(command.clone());
            let err = w.stage(&lib, true).expect_err(what).to_string();
            for needle in needles.iter().map(String::as_str).chain([
                "snapshot_command",
                command.as_str(),
                "nothing was snapshotted or staged",
            ]) {
                assert!(err.contains(needle), "{what}: {needle}: {err}");
            }
            assert_eq!(w.count("snapshots"), 0, "{what}: no unit was read");
            assert_eq!(w.count("stage_sets"), 0, "{what}");
            assert_eq!(
                std::fs::read_dir(&w.config.staging.directory)
                    .unwrap()
                    .count(),
                0,
                "{what}: staging untouched"
            );
            assert!(!log.exists(), "{what}: nothing taken, nothing released");
        }
    }

    /// Acceptance (e): a collection with no hook is staged exactly as before:
    /// each unit from where it lives, no snapshot path, no release.
    #[test]
    fn a_collection_without_a_hook_reads_its_units_where_they_live() {
        let w = HookWorld::new();
        let mut lib = no_hook_lib();
        lib.root = w.root.to_string_lossy().into_owned();
        let staged = w.stage(&lib, true).unwrap();
        assert_eq!(staged.units_staged, 2);
        assert_eq!(staged.snapshot_path, None);
        assert_eq!(staged.release_warning, None);
        assert_eq!(
            stage_set_statuses(&w.conn),
            vec!["staged".to_string(), "staged".to_string()]
        );
        let ran: String = w
            .conn
            .query_row(
                "SELECT dar_command FROM dar_create_reports ORDER BY id LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(ran.contains(&w.root.to_string_lossy().to_string()), "{ran}");
        assert!(!w.snaps.path().join("copy").exists());
    }

    /// `[staging] jobs` above 1 stages the units on worker threads; the
    /// snapshot reaches them (each job carries its view), or the changed live
    /// files would be refused there.
    #[test]
    fn units_staged_in_parallel_still_read_the_snapshot() {
        let mut w = HookWorld::new();
        w.config.staging.jobs = 2;
        crate::staging::jobs::MEMORY_OVERRIDE.with(|m| m.set(Some(1 << 40)));
        let lib = w.lib(
            "echo 'changed after the snapshot was taken, and longer' > \
             \"$TAPECTL_COLLECTION_ROOT/alpha/f.txt\" &&",
            None,
        );
        let staged = w.stage(&lib, true).expect("both read the snapshot");
        assert_eq!(staged.units_staged, 2);
        assert_eq!(
            crate::db::files::fixture::sha256(&w.conn, w.snapshot_id_of("alpha"), "f.txt"),
            Some(sha256_of(b"alpha original"))
        );
        let _ = &w.tmp;
    }
}
