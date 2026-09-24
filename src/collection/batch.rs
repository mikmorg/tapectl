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

use rusqlite::{params, Connection};

use crate::config::{Config, TapectlPaths};
use crate::error::{Result, TapectlError};
use crate::policy::coverage::{copy_count_expr, CoverageQuery};
use crate::staging::clean::CleanReport;

use super::selector::Batch;

/// One batch unit's copy count against its own resolved `min_copies`,
/// computed AFTER this call's write(s) landed. Only populated when release
/// did NOT happen (see [`BatchExecutionReport::cleaned`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyProgress {
    pub unit_name: String,
    pub copies: i64,
    pub min_copies: i64,
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
    /// deliberately retained — see [`under_copied_units`].
    pub cleaned: Option<CleanReport>,
    /// Non-empty exactly when `cleaned` is `None`: which units are still
    /// short, and by how much.
    pub under_copied: Vec<CopyProgress>,
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
/// resolved `min_copies`** ([`under_copied_units`]). This gate did not
/// exist before issue #229 and its absence was a real defect, not a
/// theoretical one: `copy_labels` is now always exactly one label (`collection
/// run` refuses more, `docs/adr/0012-...md`'s 2026-09-17 amendment), so this
/// function writes ONE copy and returns. `clean_staging`'s non-force guard
/// only checks that no `writes` row for a stage_set is non-`completed` — it
/// has no notion of "more copies are still coming" — so with exactly one
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
/// units'), by design — see [`under_copied_units`].
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
/// `assume_yes` answers `volume_write`'s quiet-host pre-flight for every
/// copy (ADR-0012, 2026-09-24 amendment, item 7) — the global `--yes`.
#[allow(clippy::too_many_arguments)]
pub fn execute_batch(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    batch: &Batch,
    copy_labels: &[String],
    device: &str,
    block_size: usize,
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
    // batch's own (name-ordered) sequence.
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
    let mut units_staged = 0usize;
    for u in &batch.units {
        let outcome = crate::staging::snapshot_create_detailed(conn, &u.name, config)?;
        match (outcome.minted, outcome.status.as_str()) {
            // A fresh version was minted — always needs staging.
            (true, _) => {
                crate::staging::stage_create(conn, paths, config, outcome.snapshot_id)?;
                units_staged += 1;
            }
            // Existing but never-staged content (ADR-0012 reuse, Change 3)
            // — the row already exists, but its slices don't yet.
            (false, "created") => {
                crate::staging::stage_create(conn, paths, config, outcome.snapshot_id)?;
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
                crate::staging::stage_create(conn, paths, config, outcome.snapshot_id)?;
                units_staged += 1;
            }
        }
    }

    // Session per copy — sequential, abort-on-first-failure (see doc
    // comment above). `force=false`: each `label` is documented as "already
    // `volume init`'d on its own cartridge" (`CollectionCommands::Run`'s own
    // doc comment) — a contact-check refusal here means the wrong physical
    // cartridge got loaded for this batch, which must hard-refuse, not
    // silently override (issue #27).
    for label in copy_labels {
        crate::volume::write::volume_write(
            conn, paths, config, label, device, block_size, false, false, assume_yes,
        )?;
    }

    // Release staging only when this batch's copy just sealed is enough —
    // see `release_if_covered`'s own doc comment for the gate and the #248
    // scoping it applies.
    let (cleaned, under_copied) = release_if_covered(conn, config, batch)?;

    Ok(BatchExecutionReport {
        units_staged,
        copies_written: copy_labels.len(),
        cleaned,
        under_copied,
    })
}

/// The release decision for one batch, extracted out of [`execute_batch`]
/// (issue #284) so it can be driven by a test without `execute_batch`'s
/// device-bound write loop ahead of it — `volume::write::volume_write` has
/// no store injection (see its own doc comment) and so cannot complete
/// without a real tape drive, which this workspace's guardrails forbid.
/// Before this extraction, a regression test for the #248 scoping below
/// hand-copied this tail instead of calling it — meaning a revert of the
/// scope back to `CleanScope::Whole` would have gone undetected, since the
/// test exercised its own copy of the logic, not this one. This is now the
/// only copy.
///
/// Releases the batch's staging only when this batch's copy just sealed is
/// enough: every unit in it must now meet its own resolved `min_copies`
/// (issue #229's second half — see [`execute_batch`]'s doc comment and
/// [`under_copied_units`] for why `clean_staging`'s own guard cannot tell
/// this on its own).
///
/// Issue #248: `clean_staging`'s own eligibility SQL has no batch/unit
/// predicate at all — passing `CleanScope::Whole` here (as the CLI's
/// `staging clean` still does) would release EVERY eligible `'staged'`
/// stage_set in the database the moment this batch's gate above passed,
/// including a different batch's unit that is still below its own
/// min_copies. `CleanScope::Units(&this_batch_unit_ids)` narrows the
/// release to exactly this batch, matching the gate it sits behind — the
/// batch's blast radius stays equal to its own scope, per this module's
/// doc comment.
fn release_if_covered(
    conn: &Connection,
    config: &Config,
    batch: &Batch,
) -> Result<(Option<CleanReport>, Vec<CopyProgress>)> {
    let under_copied = under_copied_units(conn, config, batch)?;
    let cleaned = if under_copied.is_empty() {
        let unit_ids = batch_unit_ids(conn, batch)?;
        Some(crate::staging::clean::clean_staging(
            conn,
            config,
            false,
            crate::staging::clean::CleanScope::Units(&unit_ids),
        )?)
    } else {
        None
    };
    Ok((cleaned, under_copied))
}

/// Which of this batch's units still have fewer eligible copies than their
/// own resolved `min_copies`, computed AFTER the write loop above landed.
///
/// Routed through `policy::coverage::copy_count_expr` — the declared sole
/// owner of "how many copies does this unit have" (issue #96's derivation-
/// discipline rule) — with the exact same call shape
/// `collection::status::status_for_collection` already uses for its
/// `under_copied` count, so the two surfaces cannot silently disagree about
/// what "under-copied" means. `CoverageQuery::current_unit` compares the
/// unit's CURRENT snapshot(s) against eligible (`sealed`) volumes; the write
/// loop above promotes the snapshot this batch just wrote to `'current'` and
/// its volume to `'sealed'` at seal/confirm time (`volume::session`), so a
/// unit whose one-and-only copy this call just wrote is correctly counted
/// here, not missed.
fn under_copied_units(
    conn: &Connection,
    config: &Config,
    batch: &Batch,
) -> Result<Vec<CopyProgress>> {
    let mut under = Vec::new();
    for u in &batch.units {
        let unit = crate::db::queries::get_unit_by_name(conn, &u.name)?.ok_or_else(|| {
            TapectlError::Other(format!(
                "execute_batch: unit \"{}\" is missing from the catalog right after its \
                 own batch wrote it — the catalog is inconsistent",
                u.name
            ))
        })?;
        let resolved = crate::policy::resolve(conn, config, &unit)?;
        let sql = format!(
            "SELECT {}",
            copy_count_expr(&CoverageQuery::current_unit("?1"))
        );
        let copies: i64 = conn.query_row(&sql, params![unit.id], |row| row.get(0))?;
        if copies < resolved.min_copies {
            under.push(CopyProgress {
                unit_name: u.name.clone(),
                copies,
                min_copies: resolved.min_copies,
            });
        }
    }
    Ok(under)
}

/// This batch's own unit ids, in `batch.units` order — the `CleanScope::
/// Units` selection [`execute_batch`] hands to `staging::clean::
/// clean_staging` (issue #248) so a release can never reach past this
/// batch into a different batch's still-staged data.
///
/// This is a lookup, not a decision: it says which stage_sets
/// `clean_staging` is allowed to consider, never whether any of them may
/// actually be released — that call is [`under_copied_units`], made
/// separately, immediately before this one runs (see `execute_batch`'s own
/// call site). Keeping the two as distinct calls, rather than folding unit
/// ids into `CopyProgress`, avoids growing that already-public,
/// CLI-displayed struct with a field only this internal wiring needs.
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
        config.defaults.min_copies_for_tape_only = 1;

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
        config.defaults.min_copies_for_tape_only = 2;

        let batch = one_unit_batch("testlib/alpha");

        let under = under_copied_units(&conn, &config, &batch).unwrap();
        assert_eq!(under.len(), 1, "{under:?}");
        assert_eq!(under[0].unit_name, "testlib/alpha");
        assert_eq!(under[0].copies, 1);
        assert_eq!(under[0].min_copies, 2);
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
            },
            ..Default::default()
        };
        config.defaults.min_copies_for_tape_only = min_copies;
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
        assert!(
            body.contains("release_if_covered("),
            "execute_batch no longer delegates its release step to release_if_covered \
             -- the #248 scope decision must live in exactly one place (issue #284)"
        );
        assert!(
            !body.contains("CleanScope::"),
            "execute_batch references CleanScope directly -- an unscoped clean_staging \
             call has been inlined back into it, bypassing release_if_covered's own \
             scoping (issue #248/#284)"
        );
    }
}
