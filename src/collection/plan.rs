//! `collection plan` (`docs/design/v2-open-questions.md` §11): batches one
//! collection's pending units against its resolved LTO backend capacity, using
//! the pure `selector::plan_batches`.
//!
//! This is the seam between "planning numbers as pure arithmetic"
//! (`selector`, drilled directly with synthetic sizes at production scale)
//! and "planning numbers as tapectl actually has them" (config's backend
//! capacity figures, and each pending unit's fresh on-disk size estimate
//! from `fingerprint`).
//!
//! Sizes here are a PREVIEW, not a commitment: a pending unit's
//! `estimated_bytes` comes from a live plaintext filesystem walk, not the
//! eventual encrypted/sliced on-tape bytes. The real, authoritative
//! capacity gate is `Layout::validate` at actual write time
//! (`docs/design/v2-implementation-plan.md` T5b) — this is advisory, for
//! review before committing to a stage/write run ("Emit batch manifests for
//! review").
//!
//! Two different callers need two different per-tape budgets (issue #175,
//! ADR-0012's ruling on it): `collection plan` sizes ahead of a cartridge —
//! nothing is loaded yet, so a media generation (the drive's own, or an
//! explicit `--generation`) is the only figure there IS
//! (`config::LtoBackendConfig::planning_capacity_bytes`). `collection run`
//! writes to volumes that are already `volume init`-ed, so each
//! destination's real capacity is on its own `volumes.capacity_bytes` row
//! (ADR-0010 decision 3: decided once at init, config never consulted for
//! it again) — using the drive's generation there is exactly issue #175
//! (an LTO-5 cartridge in an LTO-6 drive gets planned as a 2.5 TB batch).
//! [`plan_for_collection`] is the first (unchanged); [`plan_for_run`] +
//! [`destination_budget`] are the second. Both funnel through the same
//! budget-agnostic core, [`batches_for_budget`], so the packing logic itself
//! never has to know which kind of budget it was handed.

use rusqlite::{Connection, OptionalExtension};

use crate::config::{CollectionConfig, Config};
use crate::error::{Result, TapectlError};
use crate::policy::coverage;

use super::fingerprint::RefusedUnit;
use super::selector::{self, Batch};
use crate::volume::layout_model::pad_to_blocks;

/// The format-constant block size every write path pads against
/// (`docs/design/v2-open-questions.md` §8: "block size — format constant,
/// never scales"). There is deliberately no config knob for it:
/// `LtoBackendConfig.block_size` existed, was read by nothing, and was
/// deleted in spec W4 — `src/volume/layout.rs` bakes 512 KiB into the
/// on-tape recovery text an heir reads, so a per-drive value could only
/// ever disagree with the tape. Every real call site hardcodes it (see
/// `cli::volume::DEFAULT_BLOCK_SIZE`); this mirrors that.
const BLOCK_SIZE: u64 = 512 * 1024;

/// The budget-agnostic core: pack one collection's pending units against an
/// already-resolved per-tape budget in bytes. Neither the pending-unit
/// lookup nor `selector::plan_batches`' first-fit packing cares where the
/// budget came from — that seam is exactly what lets [`plan_for_collection`]
/// (a media generation) and [`plan_for_run`] (a destination volume's own
/// row) share one implementation instead of two copies that could drift.
///
/// Issue #285 / ADR-0012's 2026-09-22 amendment: `pending_units_for_collection`
/// now refuses a per-unit dotfile fault instead of aborting the whole scan,
/// so its `refused` list is carried straight through here rather than
/// dropped — every caller of this function (`plan_for_collection`,
/// `plan_for_run`) must keep reporting it and exit non-zero.
fn batches_for_budget(
    conn: &Connection,
    config: &Config,
    lib: &CollectionConfig,
    budget: u64,
) -> Result<(Vec<Batch>, Vec<RefusedUnit>)> {
    let scan = super::fingerprint::pending_units_for_collection(
        conn,
        lib,
        &config.defaults.global_excludes,
    )?;
    let synthetic: Vec<selector::PendingUnit> = scan
        .pending
        .iter()
        .map(|p| selector::PendingUnit {
            name: p.unit.name.clone(),
            size_bytes: p.estimated_bytes,
        })
        .collect();

    let batches = selector::plan_batches(synthetic, budget, BLOCK_SIZE).map_err(|oversized| {
        TapectlError::Other(format!(
            "collection \"{}\": {} unit(s) exceed the per-tape budget and can never be \
             batched (units are never split across tapes): {}",
            lib.name,
            oversized.len(),
            oversized
                .iter()
                .map(|o| o.to_string())
                .collect::<Vec<_>>()
                .join("; "),
        ))
    })?;
    Ok((batches, scan.refused))
}

/// Compute one collection's batches against its resolved LTO backend
/// capacity — `collection plan`'s budget: a media generation, since no
/// cartridge need be loaded to plan ahead of one. Behaviour unchanged by
/// issue #175; `collection run` no longer calls this (see [`plan_for_run`]).
pub fn plan_for_collection(
    conn: &Connection,
    config: &Config,
    lib: &CollectionConfig,
    // `--generation <GEN>`: plan for a generation other than the drive's own
    // (ADR-0010) — sizing batches for LTO-5 stock in an LTO-6 drive, say.
    // `None` means the drive's native generation.
    media: Option<&str>,
    // `--device`: WHICH drive to plan against. Resolved strictly, like every
    // other write-adjacent command (ADR-0010, "Backends resolve by device").
    // Before it existed this passed `None` unconditionally, so planning
    // errored outright the moment a second drive was configured rather than
    // asking which one was meant.
    device: Option<&str>,
) -> Result<(Vec<Batch>, Vec<RefusedUnit>)> {
    let backend = crate::config::resolve_lto_backend(config, device)?;
    // `.max(0)` dropped (issue #59): `parse_size_to_bytes` now rejects a
    // negative value with `Err` rather than letting one flow through as a
    // valid byte count, so a successfully parsed `Ok` is already guaranteed
    // non-negative here.
    let nominal = backend.planning_capacity_bytes(media)?;
    let usable = (nominal as f64 * backend.usable_capacity_factor) as u64;
    let enospc_buffer = crate::staging::parse_size_to_bytes(&backend.enospc_buffer)? as u64;
    let budget = usable.saturating_sub(enospc_buffer);

    batches_for_budget(conn, config, lib, budget)
}

/// `collection run`'s per-tape budget (issue #175): resolved from the
/// destination volumes it will actually write to, never the drive's
/// generation — the type that replaces threading `None` through
/// `plan_for_collection`'s `media` parameter and landing on
/// `planning_capacity_bytes` by default.
///
/// `bytes` is the number `batches_for_budget` packs against. The rest is
/// purely for `cmd_run` to explain the number to the operator (the printed
/// line + `budget_bytes`/`budget_from` in its JSON) — it plays no part in
/// the arithmetic.
#[derive(Debug)]
pub struct DestinationBudget {
    pub bytes: u64,
    /// The `--label` whose `capacity_bytes` is the minimum across every
    /// label given — the binding constraint on the whole batch (see
    /// [`destination_budget`]'s doc comment for why the minimum).
    pub binding_label: String,
    /// That label's own recorded `capacity_bytes`, before the
    /// usable-capacity-factor / ENOSPC-buffer arithmetic.
    pub binding_capacity_bytes: i64,
    /// How many `--label` destinations were given.
    pub num_destinations: usize,
}

/// Sum of the on-tape (block-padded) footprint every currently `'staged'`
/// stage set would add if `volume_write` ran right now — the exact set
/// `volume::write::find_staged_data` selects (`WHERE ss.status = 'staged'`,
/// joined to its slices `WHERE staging_path IS NOT NULL`), with no batch,
/// collection or tenant scope, because that function has none either
/// (issue #232 item 1: the whole point is that its selection is unscoped,
/// so the budget must account for exactly what it will pick up, not a
/// narrower guess). A stage set with zero live slices contributes nothing,
/// matching `find_staged_data`'s own "skip if slices is empty" rule.
///
/// Per-slice bytes are padded with [`pad_to_blocks`] at the same
/// [`BLOCK_SIZE`] `Layout::on_tape_bytes` pads every slice entry to
/// (`src/volume/layout_model.rs`: `LayoutEntry::on_tape_bytes`, which does
/// `size_bytes.map(|s| pad_to_blocks(s, block_size))` where a slice's
/// `size_bytes` is set from `BuildSlice::encrypted_bytes`,
/// `src/volume/build.rs`) — summing padded slices is exactly
/// `Layout::on_tape_bytes`'s own per-entry arithmetic, just computed here
/// from the database ahead of a `BuiltLayout` existing at all, since a
/// second retained batch's `Layout` is never built until its own `volume
/// write` runs.
fn already_staged_on_tape_bytes(conn: &Connection) -> Result<u64> {
    // Padding is applied per-slice, not to the aggregate sum: summing first
    // and padding once would round a batch of small slices up only a single
    // time instead of once per slice, understating the total in a way
    // `Layout::on_tape_bytes` never does (it pads each `LayoutEntry` before
    // summing). So this reads every live slice's raw bytes and pads each
    // individually before adding it in.
    let mut stmt = conn.prepare(
        "SELECT sl.encrypted_bytes
         FROM stage_sets ss
         JOIN stage_slices sl ON sl.stage_set_id = ss.id
         WHERE ss.status = 'staged' AND sl.staging_path IS NOT NULL",
    )?;
    let total: u64 = stmt
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .map(|bytes| pad_to_blocks(bytes.max(0) as u64, BLOCK_SIZE))
        .sum();
    Ok(total)
}

/// Resolve `collection run`'s budget from the destination volumes' own
/// `capacity_bytes` rows — the ADR-0010 authority, decided once at `volume
/// init` from the medium actually loaded and never re-derived from config
/// after. This must never call `planning_capacity_bytes`, which documents
/// itself as off-limits to every write path (`config.rs`: "No write path
/// may call this").
///
/// The budget is the MINIMUM `capacity_bytes` across every `--label`:
/// `--label` repeats once per planned copy and `batch::execute_batch`
/// stages the batch once and writes it to every label, so the only batch
/// size that fits every copy is one sized to the smallest destination — the
/// largest or the first-named would silently overflow a smaller one.
///
/// **This function still accepts N labels and still computes that minimum
/// (ADR-0012's 2026-09-17 amendment, issue #229): the rule is unaffected by
/// that amendment and stays, in case the one-label-per-run decision is ever
/// revisited.** What changed is the caller: [`plan_for_run`] — the only
/// production call site, and the one `cli::collection::cmd_run` actually
/// uses — refuses more than one label BEFORE calling this function at all,
/// so today this multi-label arithmetic is reachable only from a direct
/// test call, never from the CLI. Do not move that refusal down into this
/// function: doing so would make it impossible to exercise (or keep) the
/// minimum-across-destinations logic at all, which is exactly what the
/// amendment says not to delete.
///
/// Each `--label` is also checked here for write-targetness (issue #224),
/// not just existence: `policy::coverage::is_write_target` is the declared
/// sole owner of "may this volume be written?" (the issue #96/#187
/// derivation-discipline rule), and a bare existence check let a `sealed`,
/// `retired`, `erased` or `quarantined` label pass, only for
/// `batch::execute_batch` to stage the whole batch — hours of dar plus age,
/// a tape's worth of staging disk — before `volume write` finally refused
/// it. Both halves of `volume_write`'s own guard are applied here, in the
/// same order (`volume::write::volume_write`, right after its own `SELECT
/// id, status ...`): the status check ([`coverage::is_write_target`]) AND
/// the recorded-write check ([`coverage::has_completed_write`]) for the
/// ADR-0012 2026-09-16 amendment (issue #199) case where a status of
/// `initialized` alone is not sufficient (a `catalog rebuild --from-volume`
/// row can hold a completed write while never leaving `initialized`).
/// Checking both is deliberately NOT stricter than `volume_write` itself —
/// it is the same predicate pair, in the same order — so no label that
/// would pass `volume_write` can be refused here.
///
/// The usable-capacity factor and ENOSPC buffer still come from the DRIVE
/// (ADR-0010, "Read paths stay usable without a configured drive": write
/// paths "genuinely need the drive's factor, ENOSPC buffer" — only the
/// nominal figure moves to the row), and no capacity override is re-applied
/// here — the row already absorbed `capacity_override` at init, and
/// re-applying it would double-count. The arithmetic that follows is
/// genuinely identical to `volume::write::volume_write`'s own gate: the same
/// `usable_bytes = nominal * usable_capacity_factor`, the same
/// `enospc_buffer = parse_size_to_bytes(...)` (`src/volume/write.rs`, right
/// after `resolve_lto_backend`) — but this function reads `capacity_bytes`
/// with its own inline query above (`SELECT id, status, capacity_bytes FROM
/// volumes WHERE label = ?1`), not `volume_media` (`src/volume/write.rs`:
/// `SELECT capacity_bytes, media_type FROM volumes WHERE id = ?1`). The one
/// behavioural difference is deliberate, not a gap to close: `volume_media`
/// also reads `media_type` so `volume_write` can refuse a drive that cannot
/// write the recorded generation (ADR-0010) — a check that belongs at
/// contact, when a cartridge is actually loaded, never at planning time when
/// no drive has touched anything yet. `destination_budget` has no business
/// asking that question, so it has no reason to share the query that asks
/// it.
pub fn destination_budget(
    conn: &Connection,
    config: &Config,
    device: &str,
    labels: &[String],
) -> Result<DestinationBudget> {
    if labels.is_empty() {
        return Err(TapectlError::Other(
            "collection run: at least one destination volume label is required \
             (one per planned copy, via --label)"
                .into(),
        ));
    }

    let mut smallest: Option<(String, i64)> = None;
    for label in labels {
        let (volume_id, status, observed_condition, capacity_bytes): (i64, String, String, i64) =
            conn.query_row(
                "SELECT id, status, observed_condition, capacity_bytes FROM volumes \
                 WHERE label = ?1",
                [label.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
            .ok_or_else(|| TapectlError::VolumeNotFound(label.clone()))?;

        // ADR-0012 (issue #224): refuse a non-write-target label HERE,
        // before a single unit is staged — the same two-part guard
        // `volume_write` applies, in the same order, so this can never
        // reject a label `volume_write` itself would accept.
        //
        // ADR-0012's 2026-09-17 amendment (issue #242): same distinguishable
        // two-part refusal as `volume_write` -- see its comment for why.
        if !coverage::is_write_target(&status, &observed_condition) {
            if status != "initialized" {
                return Err(TapectlError::VolumeNotWriteTarget {
                    label: label.clone(),
                    status,
                });
            }
            return Err(TapectlError::VolumeQuarantined {
                label: label.clone(),
            });
        }
        if coverage::has_completed_write(conn, volume_id)? {
            return Err(TapectlError::VolumeHasRecordedWrite {
                label: label.clone(),
            });
        }

        let replace = match &smallest {
            None => true,
            Some((_, current)) => capacity_bytes < *current,
        };
        if replace {
            smallest = Some((label.clone(), capacity_bytes));
        }
    }
    // Unreachable given the empty check above (the loop runs at least
    // once), kept rather than `.unwrap()` so a future refactor that drops
    // that guard fails loudly instead of panicking.
    let (binding_label, binding_capacity_bytes) = smallest
        .ok_or_else(|| TapectlError::Other("collection run: no destination labels given".into()))?;

    let backend = crate::config::resolve_lto_backend(config, Some(device))?;
    let usable = (binding_capacity_bytes as f64 * backend.usable_capacity_factor) as u64;
    let enospc_buffer = crate::staging::parse_size_to_bytes(&backend.enospc_buffer)? as u64;
    // Issue #232 item 1: `volume_write`'s `find_staged_data` selects every
    // `'staged'` stage set in the database, not just the batch this run is
    // about to plan — under #238's retain-until-`min_copies` design, live
    // `'staged'` sets from an already-planned batch are the NORMAL state
    // between copies, not wreckage. Whatever is already staged will ride
    // along with this batch onto whichever destination `execute_batch`
    // writes to, so it must come out of the budget before packing runs, or
    // the number gates capacity while `volume_write` gates payload.
    let already_staged = already_staged_on_tape_bytes(conn)?;
    let bytes = usable
        .saturating_sub(enospc_buffer)
        .saturating_sub(already_staged);

    Ok(DestinationBudget {
        bytes,
        binding_label,
        binding_capacity_bytes,
        num_destinations: labels.len(),
    })
}

/// `collection run`'s planning entry point (issue #175). Resolves the
/// destination-volume budget FIRST — before `batches_for_budget` ever runs
/// `pending_units_for_collection`'s filesystem walk — so an unknown
/// `--label` or an empty `--label` list fails immediately, long before
/// `batch::execute_batch` stages a single unit (hours of dar + age, a
/// tape's worth of staging disk).
///
/// **More than one `--label` is refused HERE, before `destination_budget`
/// runs at all** (ADR-0012's 2026-09-17 amendment, issue #229): `collection
/// run` drives no changer, and nothing in the tree calls `mtx`/an
/// autoloader — a second `--label` used to loop straight into a second
/// `volume_write` against a device that still held the FIRST cartridge,
/// which `binding::corroborate_volume` refused as `claim_mismatch_label`
/// ("wrong tape ... There is no --force for this"). So the documented
/// primary route to `min_copies = 2` could never complete. `len() > 1` also
/// catches a duplicated label (`--label L1 --label L1`) for free: clap does
/// not dedup a repeated `long`, so that used to slip through every check
/// that follows. This refusal lives here and not inside
/// [`destination_budget`] so that function's own minimum-across-
/// destinations arithmetic stays reachable (and tested) even though no
/// production caller can hand it more than one label today.
pub fn plan_for_run(
    conn: &Connection,
    config: &Config,
    lib: &CollectionConfig,
    device: &str,
    labels: &[String],
) -> Result<(Vec<Batch>, DestinationBudget, Vec<RefusedUnit>)> {
    if labels.len() > 1 {
        return Err(TapectlError::Other(format!(
            "collection run: refuses more than one destination label ({} given: {}) — \
             tapectl drives no changer, so writing a second copy needs a human to swap \
             cartridges, and there is no point inside a `collection run` batch where \
             that swap could happen. Run `collection run` with exactly one --label for \
             the first copy; for each further copy your policy requires, swap in the \
             next cartridge and run `tapectl volume write <label>` directly against the \
             same staged data (issue #229).",
            labels.len(),
            labels.join(", "),
        )));
    }
    let budget = destination_budget(conn, config, device, labels)?;
    let (batches, refused) = batches_for_budget(conn, config, lib, budget.bytes)?;
    Ok((batches, budget, refused))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LtoBackendConfig, TapectlPaths};
    use crate::db;
    use rusqlite::params;

    fn config_with_tiny_backend() -> Config {
        let mut config = Config::default();
        config.backends.lto.push(LtoBackendConfig {
            name: "p".into(),
            device_tape: "/dev/null".into(),
            device_sg: "/dev/null".into(),
            generation: "LTO-8".into(),
            capacity_override: Some("10M".into()),
            usable_capacity_factor: 1.0,
            enospc_buffer: "0".into(),
        });
        config
    }

    #[test]
    fn plan_batches_new_units_by_estimated_on_disk_size() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        // Two ~3 MiB units — both fit a 10 MiB tape as one batch.
        for name in ["alpha", "beta"] {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("f.dat"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        }
        let lib = CollectionConfig {
            name: "testlib".into(),
            root: root.path().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };
        let paths = TapectlPaths::new(home.path().to_path_buf());
        super::super::sync::sync_collection(&conn, &paths, &lib, false, &[]).unwrap();

        let config = config_with_tiny_backend();
        let (batches, refused) = plan_for_collection(&conn, &config, &lib, None, None).unwrap();
        assert!(refused.is_empty());
        assert_eq!(batches.len(), 1, "two 3 MiB units must fit one 10 MiB tape");
        assert_eq!(
            batches[0].unit_names(),
            vec!["testlib/alpha", "testlib/beta"]
        );
    }

    /// Spec W4 / ADR-0010: `collection plan` resolved with `None`, so a
    /// second configured drive made it error outright instead of asking
    /// which one. `--device` picks, and the batch sizes follow THAT drive's
    /// capacity — not whichever backend happened to be first.
    #[test]
    fn plan_with_two_drives_sizes_batches_for_the_one_device_names() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        for name in ["alpha", "beta"] {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("f.dat"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        }
        let lib = CollectionConfig {
            name: "testlib".into(),
            root: root.path().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };
        let paths = TapectlPaths::new(home.path().to_path_buf());
        super::super::sync::sync_collection(&conn, &paths, &lib, false, &[]).unwrap();

        // Two drives: a 10 MiB one (both units fit as one batch) and a 4 MiB
        // one (they cannot share a tape). The batch COUNT is the assertion,
        // so picking the wrong drive cannot pass by coincidence.
        let mut config = config_with_tiny_backend();
        config.backends.lto.push(LtoBackendConfig {
            name: "small".into(),
            device_tape: "/dev/zero".into(),
            device_sg: "/dev/null".into(),
            generation: "LTO-8".into(),
            capacity_override: Some("4M".into()),
            usable_capacity_factor: 1.0,
            enospc_buffer: "0".into(),
        });

        // Without a device, two drives is an error asking for one — not a
        // silent pick.
        let err = plan_for_collection(&conn, &config, &lib, None, None).unwrap_err();
        assert!(err.to_string().contains("--device"), "{err}");

        let (big, _refused) =
            plan_for_collection(&conn, &config, &lib, None, Some("/dev/null")).unwrap();
        assert_eq!(big.len(), 1, "10 MiB tape holds both 3 MiB units");

        let (small, _refused) =
            plan_for_collection(&conn, &config, &lib, None, Some("/dev/zero")).unwrap();
        assert_eq!(small.len(), 2, "4 MiB tape cannot hold both 3 MiB units");
    }

    /// Issue #175: `collection run` must budget against the destination
    /// volume's own `capacity_bytes` (ADR-0010), never the drive's
    /// generation. Same fixture as `config_with_tiny_backend` (10 MiB
    /// generation-planned capacity), but the destination volume itself is a
    /// 4 MiB row — a real cartridge smaller than what the drive would plan
    /// for. Two 3 MiB units: fit one 10 MiB (generation) tape, but not one
    /// 4 MiB (destination) tape. The batch COUNT is the assertion, so
    /// budgeting from the wrong source cannot pass by coincidence.
    #[test]
    fn run_budgets_against_the_destination_volume_not_the_drive() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status) \
             VALUES ('L1', 'lto', 'p', ?1, 'initialized')",
            [4 * 1024 * 1024_i64],
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        for name in ["alpha", "beta"] {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("f.dat"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        }
        let lib = CollectionConfig {
            name: "testlib".into(),
            root: root.path().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };
        let paths = TapectlPaths::new(home.path().to_path_buf());
        super::super::sync::sync_collection(&conn, &paths, &lib, false, &[]).unwrap();

        let config = config_with_tiny_backend();

        let (volume_batches, _budget, _refused) =
            plan_for_run(&conn, &config, &lib, "/dev/null", &["L1".to_string()]).unwrap();
        assert_eq!(
            volume_batches.len(),
            2,
            "a 4 MiB destination volume cannot hold both 3 MiB units in one batch"
        );

        let (generation_batches, _refused) =
            plan_for_collection(&conn, &config, &lib, None, None).unwrap();
        assert_eq!(
            generation_batches.len(),
            1,
            "the drive's 10 MiB generation-planned capacity fits both units in one batch — \
             proving the two budgets really do disagree here"
        );
    }

    /// Issue #175: `--label` repeats once per planned copy and `batch::
    /// execute_batch` stages once and writes to every label, so the batch
    /// must be sized to the SMALLEST destination, not the largest or the
    /// first one named.
    #[test]
    fn run_budgets_against_the_smallest_of_several_destinations() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status) \
             VALUES ('big', 'lto', 'p', ?1, 'initialized')",
            [10 * 1024 * 1024_i64],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status) \
             VALUES ('small', 'lto', 'p', ?1, 'initialized')",
            [4 * 1024 * 1024_i64],
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        for name in ["alpha", "beta"] {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("f.dat"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        }
        let lib = CollectionConfig {
            name: "testlib".into(),
            root: root.path().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };
        let paths = TapectlPaths::new(home.path().to_path_buf());
        super::super::sync::sync_collection(&conn, &paths, &lib, false, &[]).unwrap();

        let config = config_with_tiny_backend();

        // Calls `destination_budget` directly, NOT `plan_for_run`: since
        // issue #229 (ADR-0012's 2026-09-17 amendment), `plan_for_run`
        // refuses more than one `--label` before ever reaching this
        // function, so this test's two labels would only ever exercise
        // that refusal through the real entry point. The minimum-across-
        // destinations arithmetic itself is unaffected by the amendment
        // and stays (see `destination_budget`'s own doc comment) — this is
        // that logic's regression test, run against the lower-level
        // function that still carries it.
        let budget = destination_budget(
            &conn,
            &config,
            "/dev/null",
            &["big".to_string(), "small".to_string()],
        )
        .unwrap();
        assert_eq!(budget.binding_label, "small");
        assert_eq!(budget.binding_capacity_bytes, 4 * 1024 * 1024);
        assert_eq!(budget.num_destinations, 2);

        let (batches, refused) = batches_for_budget(&conn, &config, &lib, budget.bytes).unwrap();
        assert!(refused.is_empty());
        assert_eq!(
            batches.len(),
            2,
            "the 4 MiB \"small\" destination is the binding constraint, not the 10 MiB \"big\" one"
        );
    }

    /// Issue #175: an unknown `--label` must fail before any unit is staged
    /// — `plan_for_run` resolves the destination budget FIRST, so this
    /// never reaches `pending_units_for_collection`, let alone
    /// `batch::execute_batch`'s staging loop. Asserting `snapshots` is
    /// untouched is the check that actually proves it, since a
    /// `VolumeNotFound` returned only after staging would still look like a
    /// correct error to a test that just matched on the error variant.
    #[test]
    fn run_refuses_an_unknown_destination_label_before_staging() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let dir = root.path().join("alpha");
        std::fs::create_dir_all(&dir).unwrap();
        // 20 MiB — bigger than `config_with_tiny_backend`'s 10 MiB
        // generation-planned capacity. This makes the ordering claim in
        // `plan_for_run`'s doc comment an assertion, not just a comment: if
        // the budget were ever resolved AFTER `batches_for_budget` ran
        // (i.e. the bug this issue fixes, reintroduced), this oversized
        // unit would surface as the "exceed the per-tape budget" error
        // instead of `VolumeNotFound`, and the `matches!` below would catch
        // that regression.
        std::fs::write(dir.join("f.dat"), vec![0u8; 20 * 1024 * 1024]).unwrap();
        let lib = CollectionConfig {
            name: "testlib".into(),
            root: root.path().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };
        let paths = TapectlPaths::new(home.path().to_path_buf());
        super::super::sync::sync_collection(&conn, &paths, &lib, false, &[]).unwrap();

        let config = config_with_tiny_backend();

        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
            .unwrap();

        let err = plan_for_run(
            &conn,
            &config,
            &lib,
            "/dev/null",
            &["nonexistent".to_string()],
        )
        .unwrap_err();
        assert!(
            matches!(&err, TapectlError::VolumeNotFound(l) if l == "nonexistent"),
            "{err}"
        );

        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            before, after,
            "an unknown label must fail before staging ever touches snapshots"
        );
    }

    /// Issue #224: `destination_budget` resolved a `--label` with a bare
    /// existence check (`SELECT capacity_bytes FROM volumes WHERE label =
    /// ?1`) and never asked whether the row was a write target at all.
    /// `policy::coverage::is_write_target` is the declared sole owner of
    /// that question (issue #96/#187's derivation-discipline rule) and this
    /// is a REGRESSION test for the fix: every non-`initialized` status
    /// `volumes.status` permits must be refused here, before
    /// `batches_for_budget` ever runs `pending_units_for_collection`'s
    /// filesystem walk, let alone before `batch::execute_batch` stages
    /// anything. Modelled directly on `volume::write`'s own
    /// `volume_write_refuses_every_non_initialized_status_before_touching_the_device`
    /// so the pinned status set can never drift between the two call sites.
    ///
    /// `quarantined` is deliberately absent (issue #242): it left the
    /// `status` CHECK entirely when it became a value of
    /// `observed_condition` instead. See
    /// `run_refuses_a_quarantined_destination_label_before_staging` for
    /// that dimension's own regression test.
    #[test]
    fn run_refuses_a_non_write_target_destination_label_before_staging() {
        let statuses = [
            "sealed", "retired", "erased", "active", "full", "blank", "missing",
        ];
        for status in statuses {
            let conn = db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status) \
                 VALUES ('L1', 'lto', 'p', ?1, ?2)",
                params![10 * 1024 * 1024_i64, status],
            )
            .unwrap();
            let root = tempfile::tempdir().unwrap();
            let home = tempfile::tempdir().unwrap();
            let dir = root.path().join("alpha");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("f.dat"), vec![0u8; 3 * 1024 * 1024]).unwrap();
            let lib = CollectionConfig {
                name: "testlib".into(),
                root: root.path().to_string_lossy().to_string(),
                tenant: "media".into(),
                unit_depth: 1,
                exclude: vec![],
                archive_set: None,
                dotfiles: true,
            };
            let paths = TapectlPaths::new(home.path().to_path_buf());
            super::super::sync::sync_collection(&conn, &paths, &lib, false, &[]).unwrap();

            let before: i64 = conn
                .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
                .unwrap();

            let config = config_with_tiny_backend();
            let err =
                plan_for_run(&conn, &config, &lib, "/dev/null", &["L1".to_string()]).unwrap_err();

            match &err {
                TapectlError::VolumeNotWriteTarget {
                    label,
                    status: got_status,
                } => {
                    assert_eq!(label, "L1", "status {status}");
                    assert_eq!(got_status, status, "status {status}");
                }
                other => panic!("status {status}: expected VolumeNotWriteTarget, got: {other:?}"),
            }
            let msg = err.to_string();
            assert!(
                msg.contains("L1"),
                "status {status}: message must name the label: {msg}"
            );
            assert!(
                msg.contains(status),
                "status {status}: message must name the status: {msg}"
            );

            let after: i64 = conn
                .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                before, after,
                "status {status}: a non-write-target label must fail before staging \
                 ever touches snapshots"
            );
        }
    }

    /// Issue #242's `is_write_target` regression, for this call site: an
    /// `initialized` destination label whose `observed_condition` is
    /// `quarantined` must be refused here too, before a single unit is
    /// staged -- the same third gate `volume_write` applies (see
    /// `volume::write::volume_write_refuses_an_initialized_volume_with_a_quarantined_condition`).
    #[test]
    fn run_refuses_a_quarantined_destination_label_before_staging() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status, \
             observed_condition) VALUES ('L1', 'lto', 'p', ?1, 'initialized', 'quarantined')",
            params![10 * 1024 * 1024_i64],
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let dir = root.path().join("alpha");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.dat"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        let lib = CollectionConfig {
            name: "testlib".into(),
            root: root.path().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };
        let paths = TapectlPaths::new(home.path().to_path_buf());
        super::super::sync::sync_collection(&conn, &paths, &lib, false, &[]).unwrap();

        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
            .unwrap();

        let config = config_with_tiny_backend();
        let err = plan_for_run(&conn, &config, &lib, "/dev/null", &["L1".to_string()]).unwrap_err();

        match &err {
            TapectlError::VolumeQuarantined { label } => {
                assert_eq!(label, "L1");
            }
            other => panic!("expected VolumeQuarantined, got: {other:?}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("L1"), "message must name the label: {msg}");
        assert!(
            msg.contains("ADR-0012"),
            "message must cite ADR-0012: {msg}"
        );

        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            before, after,
            "a quarantined destination label must fail before staging ever touches snapshots"
        );
    }

    /// Issue #224, the sibling half (ADR-0012's 2026-09-16 amendment, issue
    /// #199): a volume whose `status` still reads `initialized` but which
    /// already has a `completed` write recorded (as `catalog rebuild
    /// --from-volume` can leave one, per `policy::coverage::
    /// has_completed_write`'s doc comment) must also be refused here, not
    /// just by `is_write_target`'s status test. This is intentionally NOT
    /// stricter than `volume_write` itself: `volume_write` checks exactly
    /// this same fact, in exactly this order, right after its own status
    /// check -- so a label refused here would also be refused there, never
    /// the other way around.
    #[test]
    fn run_refuses_a_destination_label_with_a_recorded_write_before_staging() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status) \
             VALUES ('u1', 'u1', (SELECT id FROM tenants WHERE name = 'media'), \
                     'mtime_size', 1, 'active')",
            [],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, \
                                    total_size) VALUES (?1, 1, 'staged', '/tmp', 1, 10)",
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
             VALUES ('L1-REBUILT', 'lto', 'p', ?1, 'initialized')",
            [10 * 1024 * 1024_i64],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, write_verified, \
                                 completed_at, notes) \
             VALUES (?1, ?2, ?3, 'completed', 0, datetime('now'), \
                     'rebuilt from the volume itself; never verified by a read-back')",
            params![stage_set_id, snap_id, volume_id],
        )
        .unwrap();

        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let dir = root.path().join("alpha");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.dat"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        let lib = CollectionConfig {
            name: "testlib".into(),
            root: root.path().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };
        let paths = TapectlPaths::new(home.path().to_path_buf());
        super::super::sync::sync_collection(&conn, &paths, &lib, false, &[]).unwrap();

        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
            .unwrap();

        let config = config_with_tiny_backend();
        let err = plan_for_run(
            &conn,
            &config,
            &lib,
            "/dev/null",
            &["L1-REBUILT".to_string()],
        )
        .unwrap_err();

        match &err {
            TapectlError::VolumeHasRecordedWrite { label } => {
                assert_eq!(label, "L1-REBUILT");
            }
            other => panic!("expected VolumeHasRecordedWrite, got: {other:?}"),
        }

        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            before, after,
            "a recorded-write label must fail before staging ever touches snapshots"
        );
    }

    /// Issue #224: the negative-space check for the fix above -- a label
    /// that IS a legitimate write target (`initialized`, no completed write
    /// recorded) must still pass `destination_budget` and reach batch
    /// planning, so the new gate cannot be so strict it rejects runs that
    /// `volume_write` itself would accept.
    #[test]
    fn run_accepts_an_initialized_destination_label_with_no_recorded_write() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status) \
             VALUES ('L1', 'lto', 'p', ?1, 'initialized')",
            [10 * 1024 * 1024_i64],
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let dir = root.path().join("alpha");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.dat"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        let lib = CollectionConfig {
            name: "testlib".into(),
            root: root.path().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };
        let paths = TapectlPaths::new(home.path().to_path_buf());
        super::super::sync::sync_collection(&conn, &paths, &lib, false, &[]).unwrap();

        let config = config_with_tiny_backend();
        let (batches, budget, refused) =
            plan_for_run(&conn, &config, &lib, "/dev/null", &["L1".to_string()]).unwrap();
        assert!(refused.is_empty());
        assert_eq!(
            batches.len(),
            1,
            "one 3 MiB unit fits the 10 MiB destination"
        );
        assert_eq!(budget.binding_label, "L1");
    }

    #[test]
    fn plan_refuses_a_unit_larger_than_the_whole_tape() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let dir = root.path().join("huge");
        std::fs::create_dir_all(&dir).unwrap();
        // 20 MiB unit against a 10 MiB tape (0 usable-factor loss, 0 enospc
        // buffer, per `config_with_tiny_backend`) — must refuse, not split.
        std::fs::write(dir.join("f.dat"), vec![0u8; 20 * 1024 * 1024]).unwrap();

        let lib = CollectionConfig {
            name: "testlib".into(),
            root: root.path().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };
        let paths = TapectlPaths::new(home.path().to_path_buf());
        super::super::sync::sync_collection(&conn, &paths, &lib, false, &[]).unwrap();

        let config = config_with_tiny_backend();
        let err = plan_for_collection(&conn, &config, &lib, None, None).unwrap_err();
        assert!(
            err.to_string().contains("testlib/huge"),
            "error must name the offending unit: {err}"
        );
    }

    /// Issue #229 (ADR-0012's 2026-09-17 amendment): `collection run` drives
    /// no changer, so a second `--label` always looped straight into a
    /// second `volume_write` against a device that still held the FIRST
    /// cartridge, which `binding::corroborate_volume` refused as
    /// `claim_mismatch_label` — the documented primary route to
    /// `min_copies = 2` could never complete. This must be refused HERE,
    /// through `plan_for_run` (the entry point `cmd_run` actually calls),
    /// before `batches_for_budget` ever runs the pending-unit filesystem
    /// walk — proven, as with the unknown-label case above, by asserting
    /// `snapshots` is untouched rather than merely matching the error.
    #[test]
    fn run_refuses_more_than_one_destination_label_before_staging() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        for label in ["L1", "L2"] {
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status) \
                 VALUES (?1, 'lto', 'p', ?2, 'initialized')",
                params![label, 10 * 1024 * 1024_i64],
            )
            .unwrap();
        }
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let dir = root.path().join("alpha");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.dat"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        let lib = CollectionConfig {
            name: "testlib".into(),
            root: root.path().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };
        let paths = TapectlPaths::new(home.path().to_path_buf());
        super::super::sync::sync_collection(&conn, &paths, &lib, false, &[]).unwrap();

        let config = config_with_tiny_backend();

        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
            .unwrap();

        let err = plan_for_run(
            &conn,
            &config,
            &lib,
            "/dev/null",
            &["L1".to_string(), "L2".to_string()],
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("volume write"),
            "refusal must name a next step that actually runs (issue #214): {msg}"
        );

        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            before, after,
            "more than one destination label must fail before staging ever touches snapshots"
        );
    }

    /// Issue #232 item 1 (2026-09-17 correction): `destination_budget` used
    /// to size the batch purely from the destination volume's own capacity,
    /// with no regard for stage sets that are ALREADY `'staged'` and will
    /// also land on whichever volume `execute_batch` eventually calls
    /// `volume_write` against — `find_staged_data`
    /// (`src/volume/write.rs`) selects every `'staged'` stage set in the
    /// database with no batch/collection/tenant scope, so those bytes are
    /// written alongside the new batch, not instead of it. Under #238's
    /// retain-staging-until-`min_copies` design this is the NORMAL state
    /// between copies, not leftover wreckage — so the fix must shrink the
    /// budget, never refuse outright.
    ///
    /// Fixture: a 10 MiB destination, and 6 MiB already sitting in a
    /// `'staged'` stage set that belongs to no collection this run will
    /// touch (no `current_path`, so `pending_units_for_collection` cannot
    /// see it — it is exactly the kind of already-staged data
    /// `find_staged_data` would still pick up). The two pending units this
    /// run WOULD plan are 3 MiB each: they fit together in one batch against
    /// the raw 10 MiB destination capacity, but not against the 4 MiB
    /// (10 MiB − 6 MiB) that is actually free once the retained set is
    /// accounted for.
    #[test]
    fn destination_budget_subtracts_bytes_already_retained_by_other_staged_sets() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status) \
             VALUES ('L1', 'lto', 'p', ?1, 'initialized')",
            [10 * 1024 * 1024_i64],
        )
        .unwrap();

        // The pre-existing retained stage set: a unit with no `current_path`
        // (so no collection walk will ever see it as pending), already
        // `'staged'`, with one live slice (`staging_path` set, matching
        // `find_staged_data`'s own filter) whose `encrypted_bytes` is a
        // clean multiple of the 512 KiB block size so padding does not
        // perturb the arithmetic.
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status) \
             VALUES ('retained', 'retained', (SELECT id FROM tenants WHERE name = 'media'), \
                     'mtime_size', 1, 'active')",
            [],
        )
        .unwrap();
        let retained_unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size) \
             VALUES (?1, 1, 'staged', '/tmp/retained', 1, 6291456)",
            params![retained_unit_id],
        )
        .unwrap();
        let retained_snap_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 6291456)",
            params![retained_snap_id],
        )
        .unwrap();
        let retained_ss_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_slices \
                (stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain, \
                 sha256_encrypted, staging_path) \
             VALUES (?1, 1, 6291456, 6291456, 'a', 'b', '/tmp/retained-slice')",
            params![retained_ss_id],
        )
        .unwrap();

        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        for name in ["alpha", "beta"] {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("f.dat"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        }
        let lib = CollectionConfig {
            name: "testlib".into(),
            root: root.path().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };
        let paths = TapectlPaths::new(home.path().to_path_buf());
        super::super::sync::sync_collection(&conn, &paths, &lib, false, &[]).unwrap();

        let config = config_with_tiny_backend();

        let budget = destination_budget(&conn, &config, "/dev/null", &["L1".to_string()]).unwrap();
        assert_eq!(
            budget.bytes,
            4 * 1024 * 1024,
            "budget must be the 10 MiB destination minus the 6 MiB already retained by \
             the other staged set, not the raw 10 MiB capacity"
        );

        let (batches, refused) = batches_for_budget(&conn, &config, &lib, budget.bytes).unwrap();
        assert!(refused.is_empty());
        assert_eq!(
            batches.len(),
            2,
            "two 3 MiB units fit together under the raw 10 MiB capacity but must NOT fit \
             under the 4 MiB actually free once the retained 6 MiB set is accounted for"
        );
    }

    /// Issue #229's second finding on the same path: clap does not dedup a
    /// repeated `long`, so `--label L1 --label L1` used to pass every check
    /// that followed it (both labels resolve to the same, perfectly valid
    /// write target). The `len() > 1` refusal above catches this for free —
    /// this is the regression test proving it, not a second, redundant
    /// dedup check.
    #[test]
    fn run_refuses_duplicate_destination_labels_via_the_same_path() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status) \
             VALUES ('L1', 'lto', 'p', ?1, 'initialized')",
            [10 * 1024 * 1024_i64],
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let dir = root.path().join("alpha");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.dat"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        let lib = CollectionConfig {
            name: "testlib".into(),
            root: root.path().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };
        let paths = TapectlPaths::new(home.path().to_path_buf());
        super::super::sync::sync_collection(&conn, &paths, &lib, false, &[]).unwrap();

        let config = config_with_tiny_backend();

        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
            .unwrap();

        let err = plan_for_run(
            &conn,
            &config,
            &lib,
            "/dev/null",
            &["L1".to_string(), "L1".to_string()],
        )
        .unwrap_err();
        assert!(err.to_string().contains("volume write"), "{err}");

        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            before, after,
            "a duplicated destination label must fail before staging ever touches snapshots"
        );
    }

    /// Issue #285 / ADR-0012's 2026-09-22 amendment: "the same typo is
    /// reported three different ways ... `collection plan` names neither
    /// [unit nor path]." Driven through `plan_for_collection` itself — the
    /// function `collection plan` calls — rather than through
    /// `pending_units_for_collection` directly (that's
    /// `a_malformed_dotfile_refuses_only_its_own_unit` in
    /// `fingerprint.rs`), to prove the fix (and `read_dotfile`'s
    /// path-qualified error) actually reaches this call path and not just
    /// the funnel underneath it. If this fails with an `Err` instead of an
    /// `Ok(refused-non-empty)`, or with `refused[0].path` missing, the old
    /// per-collection abort (or the old path-less error) is back.
    #[test]
    fn a_dotfile_error_names_the_file_on_the_collection_path() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id: i64 = conn
            .query_row("SELECT id FROM tenants WHERE name = 'media'", [], |r| {
                r.get(0)
            })
            .unwrap();

        let root = tempfile::tempdir().unwrap();
        // Canonicalized up front (same reasoning as the fingerprint-level
        // regression test): on this VM `/tmp` is itself a symlink, and
        // `canonical_root` resolves `lib.root` through
        // `std::fs::canonicalize` before string-comparing it against each
        // unit's `current_path` — an un-canonicalized path here would make
        // every unit vanish from the scan, not just the malformed one.
        let root_path = root.path().canonicalize().unwrap();
        for name in ["alpha", "beta", "gamma"] {
            let dir = root_path.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("f.dat"), vec![0u8; 1024]).unwrap();
            conn.execute(
                "INSERT INTO units (uuid, name, tenant_id, current_path, status)
                 VALUES (?1, ?2, ?3, ?4, 'active')",
                params![
                    format!("u-{name}"),
                    format!("testlib/{name}"),
                    tenant_id,
                    dir.to_string_lossy().to_string()
                ],
            )
            .unwrap();
        }

        // beta's dotfile carries the real #263 typo, exactly as the
        // fingerprint-level regression test does.
        let beta_dotfile = root_path.join("beta/.tapectl-unit.toml");
        std::fs::write(
            &beta_dotfile,
            r#"
[unit]
uuid = "u-beta"
name = "testlib/beta"
created = "2026-01-01T00:00:00Z"
tenant = "media"

[excludes]
pattern = ["*.tmp"]
"#,
        )
        .unwrap();

        let lib = CollectionConfig {
            name: "testlib".into(),
            root: root_path.to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };

        let config = config_with_tiny_backend();
        let (batches, refused) = plan_for_collection(&conn, &config, &lib, None, None)
            .expect("a per-unit dotfile fault must not abort `collection plan`'s own scan");

        assert_eq!(refused.len(), 1, "exactly one unit must be refused");
        assert_eq!(refused[0].unit_name, "testlib/beta", "must name the unit");
        assert_eq!(
            refused[0].path,
            beta_dotfile.to_string_lossy().to_string(),
            "must name the dotfile's own path"
        );
        assert!(
            refused[0]
                .reason
                .contains(&beta_dotfile.to_string_lossy().to_string())
                && refused[0].reason.contains("pattern"),
            "the reason text itself (read_dotfile's own error) must also carry the path \
             and the offending key: {}",
            refused[0].reason
        );

        let planned_names: Vec<String> = batches
            .iter()
            .flat_map(|b| b.unit_names())
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            planned_names.len(),
            2,
            "alpha and gamma must still be planned, got {planned_names:?}"
        );
        assert!(!planned_names.iter().any(|n| n == "testlib/beta"));
    }
}
