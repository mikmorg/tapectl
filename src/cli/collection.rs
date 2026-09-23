use clap::Subcommand;
use rusqlite::Connection;

use crate::collection;
use crate::collection::fingerprint::RefusedUnit;
use crate::config::{Config, TapectlPaths};
use crate::error::{Result, TapectlError};

/// Batch execution writes with the same fixed block size every other write
/// path uses (`cli::volume::DEFAULT_BLOCK_SIZE`) — 512 KiB, the format
/// constant (`docs/design/v2-open-questions.md` §8).
const DEFAULT_BLOCK_SIZE: usize = 512 * 1024;

// `cmd_run`'s budget line (issue #175) used to carry its own local
// `format_bytes`, decimal, duplicating (and diverging from)
// `cli::catalog`'s binary `format_size` — issue #204's "two divergent
// humanisers" finding. Both are gone now in favor of
// `crate::util::format_bytes_decimal`/`format_bytes_binary`.
//
// This call site (`budget.bytes`, `budget.binding_capacity_bytes`) is
// deliberately kept on the DECIMAL formatter, not moved to binary: both
// values are capacity-derived — `binding_capacity_bytes` IS
// `volumes.capacity_bytes` and `budget.bytes` is that same figure after the
// backend's usable-capacity factor and ENOSPC buffer, never a measured data
// size. ADR-0012 stores and markets capacity decimally (LTO-6 =
// `2_500_000_000_000`); rendering either through the binary formatter would
// understate them by ~7% at this scale — precisely the class-2 wrong-number
// bug issue #204 found live in `cartridge info` and `volume info`. Printing
// "budget X (capacity Y)" in matching decimal units also keeps the two
// halves of that one sentence comparable, which a binary/decimal split on
// the same line would not.

#[derive(Subcommand, Debug)]
pub enum CollectionCommands {
    /// Sync every configured collection: register new unit folders, resolve
    /// moved/renamed ones by dotfile uuid, mark vanished ones `missing`
    /// (never deleted or retired — those are operator acts).
    Sync {
        /// Report what would change without mutating anything.
        #[arg(long)]
        dry_run: bool,
    },

    /// Show pending/dirty/missing/under-copied counts for every configured
    /// collection.
    Status,

    /// Show the batch plan (alphabetical first-fit, §7) for every
    /// configured collection's pending units.
    Plan {
        /// Copies to plan for — informational only (batches don't change;
        /// this scales the printed cartridge-count estimate).
        #[arg(long, default_value = "2")]
        copies: i64,
        /// Plan against this media generation rather than the drive's own
        /// (ADR-0010) — e.g. sizing batches for LTO-5 stock that an LTO-6
        /// drive will write. No cartridge need be loaded.
        #[arg(long)]
        generation: Option<String>,
        /// Which configured drive to plan against, by its device path. Only
        /// needed when more than one `[[backends.lto]]` is configured —
        /// without it, planning errored outright on a multi-drive config
        /// rather than asking.
        #[arg(long)]
        device: Option<String>,
    },

    /// Execute one batch: stage every unit in it once, write one session to
    /// the destination label, then release staging IF that copy already
    /// satisfies every unit's resolved `min_copies` — otherwise staging is
    /// retained for the further copies still needed (issue #229). Targets a
    /// single collection (unlike `sync`/`status`/`plan`, which sweep every
    /// configured collection) since a batch write is a real, one-shot tape
    /// action.
    Run {
        /// Collection name.
        #[arg(long)]
        collection: String,
        /// Which batch to execute (0 = first). Numbered against THIS run's
        /// own budget — the destination `--label` volumes' recorded
        /// capacity (issue #175), not the drive's generation. This matches
        /// `collection plan`'s ordering only when `plan` was run
        /// (`--generation <GEN>`) for the generation those volumes were
        /// actually initialised as; otherwise the batch reviewed in `plan`
        /// is not necessarily batch N here.
        #[arg(long, default_value = "0")]
        batch: usize,
        /// Destination volume label — already `volume init`'d on its own
        /// cartridge. Exactly one: tapectl drives no changer, so it cannot
        /// write a second copy without a human swapping cartridges, and a
        /// batch run has no point where that swap could happen. More than
        /// one is refused (issue #229). For a second and further copy,
        /// swap in the next cartridge after this run finishes and use
        /// `tapectl volume write <label>` directly against the same
        /// staged data.
        #[arg(long = "label")]
        labels: Vec<String>,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
    },
}

/// Run a collection subcommand.
///
/// `global_dry_run` is the process-wide `--dry-run` (issue #230). `main.rs`
/// never handed it to this dispatcher, so `collection run --dry-run` staged
/// a whole batch and wrote a real tape — the worst instance of the gap,
/// since ADR-0003 makes a sealed volume immutable and the cartridge is
/// consumed. `Status`/`Plan` are reads with nothing to suppress.
///
/// Returns a process exit code (issue #45/H10 precedent, extended by issue
/// #285): 0 when every unit was processed cleanly, non-zero when at least
/// one unit anywhere was REFUSED because its own dotfile could not be
/// parsed (ADR-0012's 2026-09-22 amendment). A refused unit is never
/// reported by returning `Err` here — `Err` would abort before the healthy
/// units in the same collection ever ran, which is exactly the
/// whole-collection-abort bug this issue exists to fix. `main.rs` mirrors
/// the `Volume`/`Config::Check` arms: capture the code, then
/// `exit_if_nonzero`.
pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    command: &CollectionCommands,
    json_output: bool,
    global_dry_run: bool,
) -> Result<i32> {
    match command {
        // `Sync` declares its OWN `--dry-run` as well. The two are OR-ed,
        // but NOT because the global one was being dropped here: both args
        // carry the clap id `dry_run`, so clap propagates the global value
        // into the subcommand's own field and `tapectl --dry-run collection
        // sync` already behaved as a dry run before issue #230. That was
        // measured, not assumed — reverting this OR does not make
        // `a_global_dry_run_before_collection_sync_registers_nothing` fail.
        // The OR is here so the behaviour stops depending on that clap
        // detail: rename either field and the two spellings would silently
        // diverge. Neither can turn a dry run back into a real one.
        CollectionCommands::Sync { dry_run } => {
            cmd_sync(conn, paths, config, *dry_run || global_dry_run, json_output)
        }
        CollectionCommands::Status => cmd_status(conn, config, json_output),
        CollectionCommands::Plan {
            copies,
            generation,
            device,
        } => cmd_plan(
            conn,
            config,
            *copies,
            generation.as_deref(),
            device.as_deref(),
            json_output,
        ),
        CollectionCommands::Run {
            collection: name,
            batch,
            labels,
            device,
        } => cmd_run(
            conn,
            paths,
            config,
            name,
            *batch,
            labels,
            &crate::cli::write_device(config, device.as_deref())?,
            json_output,
            global_dry_run,
        ),
    }
}

fn no_libraries_configured(json_output: bool) {
    if json_output {
        println!("{}", serde_json::json!({"collections": []}));
    } else {
        println!("no collections configured — add a [[collections]] block to config.toml");
    }
}

/// Exit code for a `collection` command carrying a `refused`-unit list
/// (ADR-0012's 2026-09-22 amendment, issue #285). Mirrors the `audit`/`db
/// fsck` convention (0=clean, 1=warning, 2=violation): a refused unit is a
/// per-unit fault the command already worked around — every OTHER unit
/// still ran — so it is a warning, not a hard error. `EXIT_ERROR` stays
/// reserved for a genuine collection-wide failure (a bad root, a DB error)
/// that still propagates as `Err` and is handled by `main.rs`'s
/// `exit_with_error`, exactly as `db fsck`'s "the integrity check itself
/// failing is a violation, full stop" reasons about its own 1-vs-2 split.
fn refused_exit_code(any_refused: bool) -> i32 {
    if any_refused {
        crate::error::EXIT_WARNING
    } else {
        crate::error::EXIT_SUCCESS
    }
}

/// `refused`, shaped for `--json` output (issue #285: "if --json is set,
/// the refusals must appear in the JSON too, not only on stderr").
fn refused_json(refused: &[RefusedUnit]) -> Vec<serde_json::Value> {
    refused
        .iter()
        .map(|r| {
            serde_json::json!({
                "unit": r.unit_name,
                "path": r.path,
                "reason": r.reason,
            })
        })
        .collect()
}

/// Plain-text report for `refused`, shared by all four commands: names each
/// refused unit, marks it clearly as never archived (ADR-0012's 2026-09-22
/// amendment — refused, not best-effort). `r.reason` already carries the
/// dotfile's own path (issue #285's `read_dotfile` fix), so it is not
/// repeated here.
fn print_refused_plain(refused: &[RefusedUnit]) {
    for r in refused {
        println!(
            "  REFUSED (not archived): unit \"{}\" — its dotfile could not be parsed: {}",
            r.unit_name, r.reason
        );
    }
}

fn cmd_sync(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    dry_run: bool,
    json_output: bool,
) -> Result<i32> {
    if config.collections.is_empty() {
        no_libraries_configured(json_output);
        return Ok(crate::error::EXIT_SUCCESS);
    }

    let mut rows = Vec::new();
    let mut any_refused = false;
    for lib in &config.collections {
        let (report, errors) = collection::sync::sync_collection(
            conn,
            paths,
            lib,
            dry_run,
            &config.defaults.global_excludes,
        )?;
        // Issue #337: a directory step 1 could not register -- an
        // UNREGISTERED unit whose dotfile does not parse, or any other
        // registration failure -- is a unit that will never be archived,
        // exactly like a refused registered one (ADR-0012, 2026-09-22:
        // "refused, and the command exits non-zero"). Printed as `error:`
        // lines below; counted here so the exit code says so too.
        any_refused |= !report.refused.is_empty() || !errors.is_empty();
        rows.push((lib.name.clone(), report, errors));
    }

    if json_output {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|(name, r, errors)| {
                serde_json::json!({
                    "collection": name,
                    "dry_run": dry_run,
                    "created": r.created,
                    "moved": r.moved,
                    "reactivated": r.reactivated,
                    "missing": r.missing,
                    "pending": r.pending,
                    "dirty": r.dirty,
                    "errors": errors,
                    "refused": refused_json(&r.refused),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else {
        for (name, r, errors) in &rows {
            let mode = if dry_run { " (dry-run)" } else { "" };
            println!(
                "collection \"{name}\"{mode}: {} created, {} moved, {} reactivated, \
                 {} missing, {} pending, {} dirty",
                r.created, r.moved, r.reactivated, r.missing, r.pending, r.dirty
            );
            for e in errors {
                println!("  error: {e}");
            }
            print_refused_plain(&r.refused);
        }
    }
    Ok(refused_exit_code(any_refused))
}

fn cmd_status(conn: &Connection, config: &Config, json_output: bool) -> Result<i32> {
    if config.collections.is_empty() {
        no_libraries_configured(json_output);
        return Ok(crate::error::EXIT_SUCCESS);
    }

    let mut rows = Vec::new();
    let mut any_refused = false;
    for lib in &config.collections {
        let status = collection::status::status_for_collection(conn, config, lib)?;
        any_refused |= !status.refused.is_empty();
        rows.push((lib.name.clone(), status));
    }

    if json_output {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|(name, s)| {
                serde_json::json!({
                    "collection": name,
                    "pending": s.pending,
                    "dirty": s.dirty,
                    "missing": s.missing,
                    "under_copied": s.under_copied,
                    "refused": refused_json(&s.refused),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else {
        for (name, s) in &rows {
            println!(
                "collection \"{name}\": {} pending, {} dirty, {} missing, {} under-copied",
                s.pending, s.dirty, s.missing, s.under_copied
            );
            print_refused_plain(&s.refused);
        }
    }
    Ok(refused_exit_code(any_refused))
}

fn cmd_plan(
    conn: &Connection,
    config: &Config,
    copies: i64,
    generation: Option<&str>,
    device: Option<&str>,
    json_output: bool,
) -> Result<i32> {
    if config.collections.is_empty() {
        no_libraries_configured(json_output);
        return Ok(crate::error::EXIT_SUCCESS);
    }

    let mut rows = Vec::new();
    let mut any_refused = false;
    for lib in &config.collections {
        let (batches, refused) =
            collection::plan::plan_for_collection(conn, config, lib, generation, device)?;
        any_refused |= !refused.is_empty();
        rows.push((lib.name.clone(), batches, refused));
    }

    if json_output {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|(name, batches, refused)| {
                let batch_json: Vec<serde_json::Value> = batches
                    .iter()
                    .enumerate()
                    .map(|(i, b)| {
                        serde_json::json!({
                            "index": i,
                            "units": b.unit_names(),
                            "total_bytes": b.total_bytes,
                            "padded_bytes": b.padded_bytes,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "collection": name,
                    "copies": copies,
                    "batches": batch_json,
                    "cartridges_needed": batches.len() as i64 * copies,
                    "refused": refused_json(refused),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else {
        for (name, batches, refused) in &rows {
            if batches.is_empty() {
                println!("collection \"{name}\": nothing pending");
            } else {
                println!("collection \"{name}\" plan ({copies} copy/copies):");
                for (i, b) in batches.iter().enumerate() {
                    println!(
                        "  batch {i}: {} units, {} raw, {} on-tape (padded)",
                        b.units.len(),
                        crate::util::format_bytes_binary(b.total_bytes as i64),
                        crate::util::format_bytes_binary(b.padded_bytes as i64),
                    );
                    for u in b.unit_names() {
                        println!("    {u}");
                    }
                }
                println!(
                    "  {} batch(es) x {copies} copy/copies = {} cartridge(s) needed",
                    batches.len(),
                    batches.len() as i64 * copies,
                );
            }
            // Printed regardless of whether `batches` is empty — the
            // `continue` this replaced (issue #285) used to skip refusal
            // reporting whenever a collection's ENTIRE pending set was
            // refused, which is exactly the case that most needs it.
            print_refused_plain(refused);
        }
    }
    Ok(refused_exit_code(any_refused))
}

#[allow(clippy::too_many_arguments)]
fn cmd_run(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    collection_name: &str,
    batch_idx: usize,
    labels: &[String],
    device: &str,
    json_output: bool,
    dry_run: bool,
) -> Result<i32> {
    let lib = collection::find_collection(config, collection_name)?;
    // No `--generation` here: `collection run` writes to volumes that are
    // already `volume init`-ed, so each destination's real capacity is on
    // its own `volumes.capacity_bytes` row (ADR-0010) — `plan_for_run`
    // resolves that budget from every `--label` FIRST (`destination_budget`,
    // never `planning_capacity_bytes`) before planning a single batch, so an
    // unknown label or an empty `--label` list fails here, before
    // `execute_batch` stages anything (issue #175). The device IS given
    // though: `run` already resolved the drive it is writing to, and its
    // usable-capacity factor / ENOSPC buffer still come from that drive.
    //
    // `refused` (issue #285): units this same collection's scan excluded
    // because their own dotfile could not be parsed. They never appear in
    // `batches` — the batch that follows only ever contains healthy units —
    // so nothing below needs to special-case them; they are only reported,
    // and their presence is what makes this command's exit code non-zero.
    let (batches, budget, refused) =
        collection::plan::plan_for_run(conn, config, lib, device, labels)?;
    let exit_code = refused_exit_code(!refused.is_empty());

    if !json_output {
        println!(
            "collection \"{collection_name}\": budget {} from volume \"{}\" (capacity {}, \
             smallest of {} destination{})",
            crate::util::format_bytes_decimal(budget.bytes as i64),
            budget.binding_label,
            crate::util::format_bytes_decimal(budget.binding_capacity_bytes),
            budget.num_destinations,
            if budget.num_destinations == 1 {
                ""
            } else {
                "s"
            },
        );
        print_refused_plain(&refused);
    }

    let batch = batches.get(batch_idx).ok_or_else(|| {
        TapectlError::Other(format!(
            "collection \"{collection_name}\": batch {batch_idx} does not exist \
             ({} batch(es) currently pending — run `collection plan` to see them)",
            batches.len()
        ))
    })?;

    // Issue #230. Everything above is planning: `plan_for_run` resolved the
    // destination budget and refused an unknown or non-write-target label,
    // and the batch index was bounds-checked — so a dry run has already
    // surfaced every reason this run would fail, and is not merely silent.
    // `execute_batch` below is where it stops being reversible: it stages
    // the whole batch (dar + age, hours and a tape's worth of staging disk)
    // and then writes and SEALS a volume, which ADR-0003 makes immutable.
    // The cartridge is consumed; there is no undo. So the return goes here,
    // printing the plan the run would have executed: the budget line above,
    // the units in the chosen batch, and the destinations they would be
    // written to.
    if dry_run {
        if json_output {
            println!(
                "{}",
                serde_json::json!({
                    "collection": collection_name,
                    "batch": batch_idx,
                    "budget_bytes": budget.bytes,
                    "budget_from": budget.binding_label,
                    "units": batch.unit_names(),
                    "labels": labels,
                    "dry_run": true,
                    "refused": refused_json(&refused),
                })
            );
        } else {
            println!(
                "collection \"{collection_name}\" batch {batch_idx}: {} unit(s) would be staged \
                 and written to {} destination{} (DRY RUN — nothing staged, no tape written)",
                batch.units.len(),
                labels.len(),
                if labels.len() == 1 { "" } else { "s" },
            );
            for u in batch.unit_names() {
                println!("    {u}");
            }
            println!("  destination(s): {}", labels.join(", "));
        }
        return Ok(exit_code);
    }

    let report = collection::batch::execute_batch(
        conn,
        paths,
        config,
        batch,
        labels,
        device,
        DEFAULT_BLOCK_SIZE,
    )?;

    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "collection": collection_name,
                "batch": batch_idx,
                "budget_bytes": budget.bytes,
                "budget_from": budget.binding_label,
                "units_staged": report.units_staged,
                "copies_written": report.copies_written,
                "staging_released": report.cleaned.is_some(),
                "stage_sets_released": report.cleaned.as_ref().map_or(0, |c| c.sets_cleaned),
                "under_copied": report.under_copied.iter().map(|p| serde_json::json!({
                    "unit": p.unit_name,
                    "copies": p.copies,
                    "min_copies": p.min_copies,
                })).collect::<Vec<_>>(),
                "refused": refused_json(&refused),
            })
        );
    } else {
        match &report.cleaned {
            Some(cleaned) => println!(
                "collection \"{collection_name}\" batch {batch_idx}: {} unit(s) staged, {} copy/copies \
                 written, {} stage set(s) released",
                report.units_staged, report.copies_written, cleaned.sets_cleaned,
            ),
            None => {
                println!(
                    "collection \"{collection_name}\" batch {batch_idx}: {} unit(s) staged, {} \
                     copy/copies written; staging RETAINED — not every unit meets its policy's \
                     copy requirement yet:",
                    report.units_staged, report.copies_written,
                );
                for p in &report.under_copied {
                    println!(
                        "    {}: {}/{} copies — swap in the next cartridge and run \
                         `tapectl volume write <label>` to write another",
                        p.unit_name, p.copies, p.min_copies,
                    );
                }
            }
        }
    }
    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CollectionConfig, TapectlPaths};
    use crate::db;
    use rusqlite::params;

    /// Three units under `root`: alpha/beta/gamma, each with a healthy
    /// `f.txt` and no snapshot yet. When `malformed_beta` is true, beta
    /// additionally gets the real issue #263 typo (`[excludes] pattern`,
    /// singular, not `patterns`) — otherwise beta has no dotfile at all
    /// (equally healthy; `dotfiles: true` on the returned `CollectionConfig`
    /// does not require one to already exist).
    fn seed_three_unit_collection(
        conn: &Connection,
        root: &std::path::Path,
        malformed_beta: bool,
    ) -> CollectionConfig {
        // Canonicalized up front: on this VM `/tmp` is itself a symlink (to
        // `/scratch/root-offload/tmp`), and `collection::canonical_root`
        // resolves `CollectionConfig::root` through `std::fs::canonicalize`
        // before string-comparing it against each unit's `current_path` —
        // an un-canonicalized `root` here would make every unit vanish from
        // the scan (not just the malformed one), which a purely negative
        // ("exit 0") assertion could not tell apart from correct behaviour.
        let root = root.canonicalize().unwrap();
        let root = root.as_path();

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

        for name in ["alpha", "beta", "gamma"] {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("f.txt"), b"hello").unwrap();
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

        if malformed_beta {
            std::fs::write(
                root.join("beta/.tapectl-unit.toml"),
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
        }

        CollectionConfig {
            name: "testlib".into(),
            root: root.to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        }
    }

    /// Issue #285 / ADR-0012's 2026-09-22 amendment: the top-level
    /// dispatcher `cli::collection::run` — what `main.rs` actually calls —
    /// must return a non-zero code when one unit's dotfile is malformed,
    /// WHILE the other two units in the same collection are still fully
    /// processed by the underlying scan. If this fails by returning 0, the
    /// exit-code wiring from `pending_units_for_collection`'s `refused`
    /// list up through `cli::collection::run` is broken — an unattended
    /// `collection run` would report success while silently never
    /// archiving one unit forever.
    #[test]
    fn a_malformed_dotfile_makes_the_command_exit_non_zero() {
        let conn = db::open_memory().unwrap();
        let root = tempfile::tempdir().unwrap();
        let lib = seed_three_unit_collection(&conn, root.path(), true);

        let mut config = Config::default();
        config.collections.push(lib.clone());

        let home = tempfile::tempdir().unwrap();
        let paths = TapectlPaths::new(home.path().to_path_buf());

        let code = run(
            &conn,
            &paths,
            &config,
            &CollectionCommands::Status,
            false,
            false,
        )
        .unwrap();
        assert_ne!(
            code, 0,
            "a refused unit must make the command exit non-zero"
        );

        // Independently confirm the healthy units were still processed —
        // not just that the exit code happens to be non-zero for some
        // unrelated reason: alpha and gamma have no snapshot yet, so both
        // must still be counted `pending`, and beta must be the one named
        // refusal.
        let status = collection::status::status_for_collection(&conn, &config, &lib).unwrap();
        assert_eq!(
            status.pending, 2,
            "alpha and gamma must still be counted pending"
        );
        assert_eq!(status.refused.len(), 1);
        assert_eq!(status.refused[0].unit_name, "testlib/beta");
    }

    /// Issue #337: `collection sync` meets a directory the catalog does not
    /// know yet whose dotfile does not parse. Step 1 cannot register it, so
    /// it will never be archived -- the command must exit non-zero, not 0.
    /// Positive control: the same fresh collection without that directory
    /// syncs cleanly (alpha is registered) and exits 0, so the non-zero
    /// comes from the unregistrable unit. (Not `seed_three_unit_collection`:
    /// its units are catalog rows with no dotfiles, a state `dotfiles =
    /// true` never produces, and sync rightly reports each as an error.)
    #[test]
    fn sync_exits_non_zero_when_an_unregistered_units_dotfile_does_not_parse() {
        for with_bad_new_unit in [true, false] {
            let conn = db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
                [],
            )
            .unwrap();
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let alpha = root.join("alpha");
            std::fs::create_dir_all(&alpha).unwrap();
            std::fs::write(alpha.join("f.txt"), b"hello").unwrap();
            if with_bad_new_unit {
                let delta = root.join("delta");
                std::fs::create_dir_all(&delta).unwrap();
                std::fs::write(delta.join("f.txt"), b"hello").unwrap();
                std::fs::write(
                    delta.join(".tapectl-unit.toml"),
                    "[unit]\nuuid = \"u-delta\"\nname = \"testlib/delta\"\n\
                     created = \"2026-01-01T00:00:00Z\"\ntenant = \"media\"\n\n\
                     [excludes]\npattern = [\"*.tmp\"]\n",
                )
                .unwrap();
            }
            let mut config = Config::default();
            config.collections.push(CollectionConfig {
                name: "testlib".into(),
                root: root.to_string_lossy().to_string(),
                tenant: "media".into(),
                unit_depth: 1,
                exclude: vec![],
                archive_set: None,
                dotfiles: true,
            });
            let home = tempfile::tempdir().unwrap();
            let paths = TapectlPaths::new(home.path().to_path_buf());

            let code = run(
                &conn,
                &paths,
                &config,
                &CollectionCommands::Sync { dry_run: false },
                false,
                false,
            )
            .unwrap();
            let names: Vec<String> = conn
                .prepare("SELECT name FROM units ORDER BY name")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(
                names,
                vec!["testlib/alpha".to_string()],
                "alpha is registered; the unparseable delta never is"
            );
            if with_bad_new_unit {
                assert_ne!(
                    code, 0,
                    "an unregistrable unit must make sync exit non-zero"
                );
            } else {
                assert_eq!(code, 0, "positive control: a clean sync exits 0");
            }
        }
    }

    /// Positive control for the test above (issue #285's own point: a
    /// command that always returns non-zero would also pass
    /// `a_malformed_dotfile_makes_the_command_exit_non_zero`). Same
    /// three-unit fixture, no malformed dotfile anywhere — must exit 0 with
    /// no refusals.
    #[test]
    fn a_collection_with_no_malformed_dotfile_exits_zero() {
        let conn = db::open_memory().unwrap();
        let root = tempfile::tempdir().unwrap();
        let lib = seed_three_unit_collection(&conn, root.path(), false);

        let mut config = Config::default();
        config.collections.push(lib);

        let home = tempfile::tempdir().unwrap();
        let paths = TapectlPaths::new(home.path().to_path_buf());

        let code = run(
            &conn,
            &paths,
            &config,
            &CollectionCommands::Status,
            false,
            false,
        )
        .unwrap();
        assert_eq!(code, 0, "no unit was refused, so the command must exit 0");
    }
}
