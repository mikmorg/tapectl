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
/// itself and only calls `clean_staging` when none are still short; a
/// still-short batch retains ALL its staging (not merely the short units'),
/// since `clean_staging`'s own selection is a global sweep with no
/// batch-scoped filter to hand it — see [`under_copied_units`].
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
            conn, paths, config, label, device, block_size, false, false,
        )?;
    }

    // Release staging only when this batch's copy just sealed is enough:
    // every unit in it must now meet its own resolved `min_copies` (issue
    // #229's second half — see this function's doc comment and
    // `under_copied_units` for why `clean_staging`'s own guard cannot tell
    // this on its own).
    let under_copied = under_copied_units(conn, config, batch)?;
    let cleaned = if under_copied.is_empty() {
        Some(crate::staging::clean::clean_staging(conn, config, false)?)
    } else {
        None
    };

    Ok(BatchExecutionReport {
        units_staged,
        copies_written: copy_labels.len(),
        cleaned,
        under_copied,
    })
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
    /// END TO END here, not reasoned about. This reproduces `execute_batch`'s
    /// own tail exactly — call [`under_copied_units`], and call
    /// `clean_staging` ONLY when it comes back empty — and then checks the
    /// two facts a second `volume write` actually depends on:
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

        let under = under_copied_units(&conn, &config, &batch).unwrap();
        assert_eq!(under.len(), 1, "{under:?}");
        if under.is_empty() {
            crate::staging::clean::clean_staging(&conn, &config, false).unwrap();
        }

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
    #[test]
    fn release_gate_cleans_staged_bytes_when_min_copies_one_is_already_met() {
        let (conn, stage_set_id, staged_file, dir_guard) =
            seed_unit_with_one_completed_copy_and_staged_file("testlib/alpha");
        let config = config_with_staging_dir(dir_guard.path(), 1);
        let batch = one_unit_batch("testlib/alpha");

        let under = under_copied_units(&conn, &config, &batch).unwrap();
        assert!(under.is_empty(), "{under:?}");
        if under.is_empty() {
            crate::staging::clean::clean_staging(&conn, &config, false).unwrap();
        }

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
}
