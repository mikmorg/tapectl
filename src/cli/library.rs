use clap::Subcommand;
use rusqlite::Connection;

use crate::config::{Config, TapectlPaths};
use crate::error::{Result, TapectlError};
use crate::library;

/// Batch execution writes with the same fixed block size every other write
/// path uses (`cli::volume::DEFAULT_BLOCK_SIZE`) — 512 KiB, the format
/// constant (`docs/design/v2-open-questions.md` §8).
const DEFAULT_BLOCK_SIZE: usize = 512 * 1024;

#[derive(Subcommand, Debug)]
pub enum LibraryCommands {
    /// Sync every configured library: register new unit folders, resolve
    /// moved/renamed ones by dotfile uuid, mark vanished ones `missing`
    /// (never deleted or retired — those are operator acts).
    Sync {
        /// Report what would change without mutating anything.
        #[arg(long)]
        dry_run: bool,
    },

    /// Show pending/dirty/missing/under-copied counts for every configured
    /// library.
    Status,

    /// Show the batch plan (alphabetical first-fit, §7) for every
    /// configured library's pending units.
    Plan {
        /// Copies to plan for — informational only (batches don't change;
        /// this scales the printed cartridge-count estimate).
        #[arg(long, default_value = "2")]
        copies: i64,
    },

    /// Execute one batch: stage every unit in it once, write one session
    /// per destination label, then release staging. Targets a single
    /// library (unlike `sync`/`status`/`plan`, which sweep every
    /// configured library) since a batch write is a real, one-shot tape
    /// action.
    Run {
        /// Library name.
        #[arg(long)]
        library: String,
        /// Which batch from `library plan`'s ordering to execute (0 =
        /// first).
        #[arg(long, default_value = "0")]
        batch: usize,
        /// Destination volume label — already `volume init`'d on its own
        /// cartridge. Repeat once per planned copy (e.g. `--label L1
        /// --label L2` for two copies).
        #[arg(long = "label")]
        labels: Vec<String>,
        /// Tape device path.
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
    },
}

pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    command: &LibraryCommands,
    json_output: bool,
) -> Result<()> {
    match command {
        LibraryCommands::Sync { dry_run } => cmd_sync(conn, paths, config, *dry_run, json_output),
        LibraryCommands::Status => cmd_status(conn, config, json_output),
        LibraryCommands::Plan { copies } => cmd_plan(conn, config, *copies, json_output),
        LibraryCommands::Run {
            library: name,
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
            device,
            json_output,
        ),
    }
}

fn no_libraries_configured(json_output: bool) {
    if json_output {
        println!("{}", serde_json::json!({"libraries": []}));
    } else {
        println!("no libraries configured — add a [[libraries]] block to config.toml");
    }
}

fn cmd_sync(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    dry_run: bool,
    json_output: bool,
) -> Result<()> {
    if config.libraries.is_empty() {
        no_libraries_configured(json_output);
        return Ok(());
    }

    let mut rows = Vec::new();
    for lib in &config.libraries {
        let (report, errors) = library::sync::sync_library(conn, paths, lib, dry_run)?;
        rows.push((lib.name.clone(), report, errors));
    }

    if json_output {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|(name, r, errors)| {
                serde_json::json!({
                    "library": name,
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
                "library \"{name}\"{mode}: {} created, {} moved, {} reactivated, \
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
    if config.libraries.is_empty() {
        no_libraries_configured(json_output);
        return Ok(());
    }

    let mut rows = Vec::new();
    for lib in &config.libraries {
        let status = library::status::status_for_library(conn, config, lib)?;
        rows.push((lib.name.clone(), status));
    }

    if json_output {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|(name, s)| {
                serde_json::json!({
                    "library": name,
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
                "library \"{name}\": {} pending, {} dirty, {} missing, {} under-copied",
                s.pending, s.dirty, s.missing, s.under_copied
            );
        }
    }
    Ok(())
}

fn cmd_plan(conn: &Connection, config: &Config, copies: i64, json_output: bool) -> Result<()> {
    if config.libraries.is_empty() {
        no_libraries_configured(json_output);
        return Ok(());
    }

    let mut rows = Vec::new();
    for lib in &config.libraries {
        let batches = library::plan::plan_for_library(conn, config, lib)?;
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
                    "library": name,
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
                println!("library \"{name}\": nothing pending");
                continue;
            }
            println!("library \"{name}\" plan ({copies} copy/copies):");
            for (i, b) in batches.iter().enumerate() {
                println!(
                    "  batch {i}: {} units, {} MB raw, {} MB on-tape (padded)",
                    b.units.len(),
                    b.total_bytes / (1024 * 1024),
                    b.padded_bytes / (1024 * 1024),
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
    library_name: &str,
    batch_idx: usize,
    labels: &[String],
    device: &str,
    json_output: bool,
) -> Result<()> {
    let lib = library::find_library(config, library_name)?;
    let batches = library::plan::plan_for_library(conn, config, lib)?;
    let batch = batches.get(batch_idx).ok_or_else(|| {
        TapectlError::Other(format!(
            "library \"{library_name}\": batch {batch_idx} does not exist \
             ({} batch(es) currently pending — run `library plan` to see them)",
            batches.len()
        ))
    })?;

    let report = library::batch::execute_batch(
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
                "library": library_name,
                "batch": batch_idx,
                "units_staged": report.units_staged,
                "copies_written": report.copies_written,
                "stage_sets_released": report.cleaned.sets_cleaned,
            })
        );
    } else {
        println!(
            "library \"{library_name}\" batch {batch_idx}: {} unit(s) staged, {} copy/copies \
             written, {} stage set(s) released",
            report.units_staged, report.copies_written, report.cleaned.sets_cleaned,
        );
    }
    Ok(())
}
