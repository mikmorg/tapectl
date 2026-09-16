use clap::Subcommand;
use rusqlite::Connection;

use crate::collection;
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

    /// Execute one batch: stage every unit in it once, write one session
    /// per destination label, then release staging. Targets a single
    /// collection (unlike `sync`/`status`/`plan`, which sweep every
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
        /// cartridge. Repeat once per planned copy (e.g. `--label L1
        /// --label L2` for two copies).
        #[arg(long = "label")]
        labels: Vec<String>,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
    },
}

pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    command: &CollectionCommands,
    json_output: bool,
) -> Result<()> {
    match command {
        CollectionCommands::Sync { dry_run } => {
            cmd_sync(conn, paths, config, *dry_run, json_output)
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

fn cmd_sync(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    dry_run: bool,
    json_output: bool,
) -> Result<()> {
    if config.collections.is_empty() {
        no_libraries_configured(json_output);
        return Ok(());
    }

    let mut rows = Vec::new();
    for lib in &config.collections {
        let (report, errors) = collection::sync::sync_collection(
            conn,
            paths,
            lib,
            dry_run,
            &config.defaults.global_excludes,
        )?;
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
        }
    }
    Ok(())
}

fn cmd_status(conn: &Connection, config: &Config, json_output: bool) -> Result<()> {
    if config.collections.is_empty() {
        no_libraries_configured(json_output);
        return Ok(());
    }

    let mut rows = Vec::new();
    for lib in &config.collections {
        let status = collection::status::status_for_collection(conn, config, lib)?;
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
        }
    }
    Ok(())
}

fn cmd_plan(
    conn: &Connection,
    config: &Config,
    copies: i64,
    generation: Option<&str>,
    device: Option<&str>,
    json_output: bool,
) -> Result<()> {
    if config.collections.is_empty() {
        no_libraries_configured(json_output);
        return Ok(());
    }

    let mut rows = Vec::new();
    for lib in &config.collections {
        let batches = collection::plan::plan_for_collection(conn, config, lib, generation, device)?;
        rows.push((lib.name.clone(), batches));
    }

    if json_output {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|(name, batches)| {
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
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else {
        for (name, batches) in &rows {
            if batches.is_empty() {
                println!("collection \"{name}\": nothing pending");
                continue;
            }
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
    }
    Ok(())
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
) -> Result<()> {
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
    let (batches, budget) = collection::plan::plan_for_run(conn, config, lib, device, labels)?;

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
    }

    let batch = batches.get(batch_idx).ok_or_else(|| {
        TapectlError::Other(format!(
            "collection \"{collection_name}\": batch {batch_idx} does not exist \
             ({} batch(es) currently pending — run `collection plan` to see them)",
            batches.len()
        ))
    })?;

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
                "stage_sets_released": report.cleaned.sets_cleaned,
            })
        );
    } else {
        println!(
            "collection \"{collection_name}\" batch {batch_idx}: {} unit(s) staged, {} copy/copies \
             written, {} stage set(s) released",
            report.units_staged, report.copies_written, report.cleaned.sets_cleaned,
        );
    }
    Ok(())
}
