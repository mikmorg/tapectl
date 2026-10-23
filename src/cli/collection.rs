use clap::Subcommand;
use rusqlite::Connection;

use crate::collection;
use crate::collection::fingerprint::RefusedUnit;
use crate::collection::outside::OutsideEntry;
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

    /// Show the batch plan (alphabetical next-fit, §7) for every
    /// configured collection's pending units.
    ///
    /// A unit that allows spanning (`allow_spanning`) is not held to one
    /// tape: it goes last in its batch, and the plan names the unit that
    /// spans onto the next cartridge. It is refused when it is larger than
    /// the staging area, or, on a drive that does not declare
    /// `post_warning_bytes`, larger than the room under the fill ceiling.
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
        /// Fill each tape to at most this fraction of its capacity, for this
        /// command only: `0.99` or `99%`. Overrides the drive's
        /// `fill_ceiling` (default 97%, ADR-0012).
        #[arg(long, value_parser = crate::config::parse_fill_ceiling)]
        fill_ceiling: Option<f64>,
        /// Pack first the units a write would help: those whose audit
        /// finds too few copies or a required location with no copy (a
        /// never-archived unit always does). Each group stays in name order.
        /// Batch numbers then follow this order, so give `collection run`
        /// the same flag to run the batch reviewed here.
        #[arg(long)]
        policy_aware: bool,
    },

    /// Execute one batch: stage every unit in it once, write one session to
    /// the destination label, then release staging IF that copy already
    /// satisfies every unit's resolved `min_copies` — otherwise staging is
    /// retained for the further copies still needed. Targets a
    /// single collection (unlike `sync`/`status`/`plan`, which sweep every
    /// configured collection) since a batch write is a real, one-shot tape
    /// action.
    ///
    /// A unit that spans cartridges is released slice by slice, each slice
    /// once it has its copies, and the run names the slices released and the
    /// slices kept for the next `volume write`.
    ///
    /// Exit status: `volume write`'s, the worst outcome among its copies in
    /// the order 6 > 5 > 4 > 3 > 2 > 0 (6 quarantined, 5 aborted, 4
    /// interrupted, 3 confirm inconclusive, 2 nothing written, 0 sealed and
    /// confirmed); 1 when every copy sealed but a unit was refused for its
    /// dotfile; 75 when the catalog was busy before anything was written.
    Run {
        /// Collection name.
        #[arg(long)]
        collection: String,
        /// Which batch to execute (0 = first). Numbered against THIS run's
        /// own budget — the destination `--label` volumes' recorded
        /// capacity, not the drive's generation. This matches
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
        /// one is refused. For a second and further copy,
        /// swap in the next cartridge after this run finishes and use
        /// `tapectl volume write <label>` directly against the same
        /// staged data.
        #[arg(long = "label")]
        labels: Vec<String>,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
        /// See `volume write --prewrite-hash`: full-hash every staged slice
        /// before the tape moves, for every copy this run writes.
        #[arg(long)]
        prewrite_hash: bool,
        /// Fill each tape to at most this fraction of its capacity, for this
        /// command only: `0.99` or `99%`. Overrides the drive's
        /// `fill_ceiling` (default 97%, ADR-0012).
        #[arg(long, value_parser = crate::config::parse_fill_ceiling)]
        fill_ceiling: Option<f64>,
        /// How many of the batch's units to stage at once (1 to 16), as
        /// `stage create --jobs`. Defaults to `[staging] jobs`, itself 1.
        #[arg(long)]
        jobs: Option<usize>,
        #[command(flatten)]
        confirm: RunConfirmArgs,
        /// Number the batches as `collection plan --policy-aware` does: the
        /// units a write would help (too few copies, a required location
        /// with no copy) are packed first.
        #[arg(long)]
        policy_aware: bool,
    },
}

/// `collection run`'s readback choice (ADR-0012 2026-10-07 item 31).
#[derive(clap::Args, Debug, Clone, Default)]
pub struct RunConfirmArgs {
    /// See `volume write --full-confirm`: read every copy this run writes
    /// back in full after sealing it.
    #[arg(long, conflicts_with = "full_confirm_first")]
    pub full_confirm: bool,
    /// Read the run's first copy back in full after sealing it and confirm
    /// the others quickly: the one full readback to have before the run
    /// releases staging. A run writes one copy today, so this reads back the
    /// same copy `--full-confirm` does.
    #[arg(long)]
    pub full_confirm_first: bool,
}

impl RunConfirmArgs {
    /// Which copies get the full readback.
    pub fn confirm(&self) -> collection::batch::RunConfirm {
        use collection::batch::RunConfirm;
        match (self.full_confirm, self.full_confirm_first) {
            (true, _) => RunConfirm::FullEvery,
            (false, true) => RunConfirm::FullFirst,
            (false, false) => RunConfirm::Quick,
        }
    }
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
    assume_yes: bool,
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
            cmd_sync(conn, config, *dry_run || global_dry_run, json_output)
        }
        CollectionCommands::Status => cmd_status(conn, config, json_output),
        CollectionCommands::Plan {
            copies,
            generation,
            device,
            fill_ceiling,
            policy_aware,
        } => cmd_plan(
            conn,
            &config.with_fill_ceiling(*fill_ceiling),
            *copies,
            generation.as_deref(),
            device.as_deref(),
            *policy_aware,
            json_output,
        ),
        CollectionCommands::Run {
            collection: name,
            batch,
            labels,
            device,
            prewrite_hash,
            fill_ceiling,
            jobs,
            confirm,
            policy_aware,
        } => cmd_run(
            conn,
            paths,
            &config.with_fill_ceiling(*fill_ceiling),
            name,
            *batch,
            labels,
            &crate::cli::write_device(config, device.as_deref())?,
            *prewrite_hash,
            *jobs,
            confirm.confirm(),
            *policy_aware,
            json_output,
            global_dry_run,
            assume_yes,
        ),
    }
}

/// What each unit in `batches` that a write would help falls short of
/// (`coverage::Shortfall::describe`), keyed by unit name — only the units
/// that fall short. JSON strings, so `--json` and the plain listing print
/// the same words.
fn shortfalls<'a>(
    conn: &Connection,
    config: &Config,
    batches: impl Iterator<Item = &'a collection::selector::Batch>,
) -> Result<std::collections::HashMap<String, serde_json::Value>> {
    let mut out = std::collections::HashMap::new();
    for name in batches.flat_map(|b| b.unit_names()) {
        let Some(unit) = crate::db::queries::get_unit_by_name(conn, name)? else {
            continue;
        };
        let short = crate::policy::coverage::unit_shortfall(conn, config, &unit)?;
        if short.any() {
            out.insert(
                name.to_string(),
                serde_json::Value::String(short.describe()),
            );
        }
    }
    Ok(out)
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
            "  REFUSED (not archived): unit \"{}\" — its dotfile was refused: {}",
            r.unit_name, r.reason
        );
    }
}

/// `outside` (issue #382), shaped for `--json`.
fn outside_json(outside: &[OutsideEntry]) -> Vec<serde_json::Value> {
    outside
        .iter()
        .map(|e| serde_json::json!({"path": e.path, "kind": e.kind.label()}))
        .collect()
}

/// Plain-text report for `outside` (issue #382), shared by `sync` and
/// `status`: a count, then each entry by its path under the root. Nothing
/// when there is none, so a clean collection reads as before.
fn print_outside_plain(outside: &[OutsideEntry]) {
    let n = outside.len();
    match n {
        0 => return,
        1 => {
            println!("  OUTSIDE ANY UNIT (not archived): 1 entry under the root belongs to no unit")
        }
        _ => println!(
            "  OUTSIDE ANY UNIT (not archived): {n} entries under the root belong to no unit"
        ),
    }
    for e in outside {
        println!("    {} ({})", e.path, e.kind.label());
    }
}

fn cmd_sync(conn: &Connection, config: &Config, dry_run: bool, json_output: bool) -> Result<i32> {
    if config.collections.is_empty() {
        no_libraries_configured(json_output);
        return Ok(crate::error::EXIT_SUCCESS);
    }

    let mut rows = Vec::new();
    let mut any_refused = false;
    for lib in &config.collections {
        let (report, errors) =
            collection::sync::sync_collection_with_config(conn, config, lib, dry_run)?;
        // Issue #337: a directory step 1 could not register -- an
        // UNREGISTERED unit whose dotfile does not parse, or any other
        // registration failure -- is a unit that will never be archived,
        // exactly like a refused registered one (ADR-0012, 2026-09-22:
        // "refused, and the command exits non-zero"). Printed as `error:`
        // lines below; counted here so the exit code says so too.
        // Issue #382: content no unit holds is never archived either.
        any_refused |=
            !report.refused.is_empty() || !errors.is_empty() || !report.outside.is_empty();
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
                    "outside": outside_json(&r.outside),
                    "tenant_differs": r.tenant_differs.iter().map(|(unit, tenant)| {
                        serde_json::json!({"unit": unit, "tenant": tenant})
                    }).collect::<Vec<_>>(),
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
            if let Some(lib) = config.collections.iter().find(|l| &l.name == name) {
                for (unit, tenant) in &r.tenant_differs {
                    println!(
                        "  warning: unit \"{unit}\" is owned by tenant \"{tenant}\", not the \
                         collection's \"{}\"; it keeps its owner (new units go to \"{}\")",
                        lib.tenant, lib.tenant
                    );
                }
            }
            print_refused_plain(&r.refused);
            print_outside_plain(&r.outside);
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
        any_refused |= !status.refused.is_empty() || !status.outside.is_empty();
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
                    "outside": outside_json(&s.outside),
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
            print_outside_plain(&s.outside);
        }
    }
    Ok(refused_exit_code(any_refused))
}

/// `collection plan`'s `largest_unit` JSON object, or `null` when nothing is
/// pending (issue #456).
fn largest_unit_json(largest: Option<&collection::plan::LargestUnit>) -> serde_json::Value {
    match largest {
        None => serde_json::Value::Null,
        Some(l) => serde_json::json!({
            "name": l.name,
            "bytes": l.size_bytes,
            "tape_budget_bytes": l.tape_budget_bytes,
            "percent_of_tape_budget": l.percent_of_tape_budget,
            "above_a_tenth_of_a_cartridge": l.above_a_tenth_of_a_cartridge,
        }),
    }
}

/// `collection plan`'s largest-unit line (issue #456): the unit, its size and
/// its share of the tape; past a tenth of a cartridge it says why that
/// matters (ADR-0012 2026-10-15 item 69).
fn largest_unit_line(l: &collection::plan::LargestUnit) -> String {
    let mut line = format!(
        "largest unit: {}, {} ({:.1}% of the tape's {} budget)",
        l.name,
        crate::util::format_bytes_binary(i64::try_from(l.size_bytes).unwrap_or(i64::MAX)),
        l.percent_of_tape_budget,
        crate::util::format_bytes_binary(i64::try_from(l.tape_budget_bytes).unwrap_or(i64::MAX)),
    );
    if l.above_a_tenth_of_a_cartridge {
        line.push_str(
            " — above a tenth of a cartridge: the packing order (alphabetical next-fit) \
             is due a re-examination (ADR-0012 2026-10-15 item 69)",
        );
    }
    line
}

/// The lines naming where a batch's units sit on its tape past the fill
/// ceiling ([`collection::plan::BatchPlacement`]): the whole units past it
/// (ADR-0012 item 113), the units that allow spanning past it (items 83 and
/// 95), and the one that spans (items 94 and 123). None when the batch fits
/// under the ceiling, as every batch does unless the drive declares
/// `post_warning_bytes`.
fn placement_lines(at: &collection::plan::BatchPlacement) -> Vec<String> {
    let mut lines = Vec::new();
    if !at.past_ceiling.is_empty() {
        lines.push(format!(
            "past the fill ceiling (the tape may end before them; a unit it ends before is cut \
             and stays staged for the next cartridge): {}",
            at.past_ceiling.join(", ")
        ));
    }
    if !at.may_span.is_empty() {
        lines.push(format!(
            "may span: last on its batch's tape, past the fill ceiling; if the tape ends inside \
             it, continued on the next: {}",
            at.may_span.join(", ")
        ));
    }
    if let Some(unit) = &at.spans {
        lines.push(format!(
            "spans: last on its batch's tape, continued on the next: {unit}"
        ));
    }
    lines
}

/// The line after `collection plan`'s cartridge count when a batch has a
/// unit that spans (#478): its continuation takes room the count does not.
const SPAN_COUNT_CAVEAT: &str = "the count is a minimum: a unit that spans continues on the next \
                                 cartridge, ahead of the units planned there";

/// The lines naming what the per-slice release did (#478; ADR-0012 item
/// 129): each set's slices released, and each set's slices kept because they
/// are short of copies. The words of `staging clean`'s
/// (`cli::staging::slice_release_notice`, private to that module), kept
/// identical. Empty when it did nothing.
fn slice_release_notice(report: &crate::staging::clean::CleanReport) -> String {
    let describe = crate::staging::clean::describe_slice_numbers;
    let mut msg = String::new();
    for (id, slices) in &report.slices_released {
        msg.push_str(&format!(
            "  stage set {id}: slice(s) {} released, each now has its copies (the set spans \
             cartridges and is released slice by slice)\n",
            describe(slices)
        ));
    }
    for (id, slices) in &report.slices_short {
        msg.push_str(&format!(
            "  stage set {id} kept in part: slice(s) {} are short of copies and continue its \
             span; `tapectl volume write <LABEL>` onto another cartridge of the same \
             generation writes them (--force would leave the ranges already on tape unable \
             to be continued)\n",
            describe(slices)
        ));
    }
    msg
}

/// `[{"stage_set_id": N, "slices": [..]}, ..]`, as `staging clean --json`
/// gives its per-slice lists (#478).
fn slice_runs_json(runs: &[(i64, Vec<i64>)]) -> serde_json::Value {
    runs.iter()
        .map(|(id, slices)| serde_json::json!({ "stage_set_id": id, "slices": slices }))
        .collect::<Vec<_>>()
        .into()
}

/// Every file the run's release could not remove (issue #108): the
/// whole-set release's and the per-slice release's. Their `staging_path`
/// rows are already nulled, so no later `staging clean` can find them.
fn stranded_paths(report: &collection::batch::BatchExecutionReport) -> Vec<&std::path::Path> {
    report
        .cleaned
        .iter()
        .flat_map(|c| c.stranded.iter())
        .chain(report.slices.stranded.iter())
        .map(|p| p.as_path())
        .collect()
}

/// The stderr warning naming [`stranded_paths`], as `volume write` words it,
/// or `None` when every file went.
fn stranded_warning(label: &str, stranded: &[&std::path::Path]) -> Option<String> {
    if stranded.is_empty() {
        return None;
    }
    let mut msg = format!(
        "warning: volume \"{label}\" is sealed and confirmed and counts as a copy, but {} staged \
         file(s) could NOT be removed and no later `staging clean` can find them. Remove by hand:",
        stranded.len()
    );
    for p in stranded {
        msg.push_str(&format!("\n  {}", p.display()));
    }
    Some(msg)
}

fn cmd_plan(
    conn: &Connection,
    config: &Config,
    copies: i64,
    generation: Option<&str>,
    device: Option<&str>,
    policy_aware: bool,
    json_output: bool,
) -> Result<i32> {
    if config.collections.is_empty() {
        no_libraries_configured(json_output);
        return Ok(crate::error::EXIT_SUCCESS);
    }

    let mut rows = Vec::new();
    let mut any_refused = false;
    for lib in &config.collections {
        let (packed, tape) = collection::plan::plan_for_collection_with_budget(
            conn,
            config,
            lib,
            generation,
            device,
            policy_aware,
        )?;
        any_refused |= !packed.refused.is_empty();
        // Issue #456: the largest unit beside the tape, so ADR-0012 item 69's
        // trigger (a unit above a tenth of a cartridge) is visible.
        let largest = collection::plan::largest_unit(&packed.batches, &tape);
        rows.push((lib.name.clone(), packed, largest, tape));
    }
    // `--policy-aware` (issue #144): what each prioritised unit falls short
    // of, printed beside it so the order explains itself.
    let findings = if policy_aware {
        shortfalls(
            conn,
            config,
            rows.iter().flat_map(|(_, p, _, _)| p.batches.iter()),
        )?
    } else {
        std::collections::HashMap::new()
    };

    if json_output {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|(name, packed, largest, tape)| {
                let batches = &packed.batches;
                let refused = &packed.refused;
                let batch_json: Vec<serde_json::Value> = batches
                    .iter()
                    .enumerate()
                    .map(|(i, b)| {
                        let at = packed.placement(b);
                        serde_json::json!({
                            "index": i,
                            "units": b.unit_names(),
                            "total_bytes": b.total_bytes,
                            "padded_bytes": b.padded_bytes,
                            // ADR-0012 item 113: empty unless the drive
                            // declares post_warning_bytes.
                            "past_ceiling": at.past_ceiling,
                            // #478 (items 83, 94, 95 and 123): empty and
                            // null unless a unit allows spanning, and the
                            // drive declares post_warning_bytes.
                            "may_span": at.may_span,
                            "spans": at.spans,
                            "findings": b
                                .unit_names()
                                .into_iter()
                                .filter_map(|u| findings.get(u).map(|f| (u.to_string(), f.clone())))
                                .collect::<serde_json::Map<String, serde_json::Value>>(),
                        })
                    })
                    .collect();
                serde_json::json!({
                    "collection": name,
                    "copies": copies,
                    "policy_aware": policy_aware,
                    "batches": batch_json,
                    "cartridges_needed": batches.len() as i64 * copies,
                    "largest_unit": largest_unit_json(largest.as_ref()),
                    "refused": refused_json(refused),
                    // ADR-0012 item 113: what one tape takes under the fill
                    // ceiling, and what a batch is packed to (the same
                    // figure unless the drive declares post_warning_bytes).
                    "fill_bytes": tape.bytes,
                    "packing_bytes": tape.packing_bytes(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else {
        for (name, packed, largest, tape) in &rows {
            let (batches, refused) = (&packed.batches, &packed.refused);
            if batches.is_empty() {
                println!("collection \"{name}\": nothing pending");
            } else {
                println!("collection \"{name}\" plan ({copies} copy/copies):");
                // ADR-0012 item 113: printed only when the drive declares
                // post_warning_bytes; every other line is as before.
                if tape.plan_bound_bytes.is_some() {
                    println!("  packing: {}", tape.describe_packing());
                }
                for (i, b) in batches.iter().enumerate() {
                    println!(
                        "  batch {i}: {} units, {} raw, {} on-tape (padded)",
                        b.units.len(),
                        crate::util::format_bytes_binary(b.total_bytes as i64),
                        crate::util::format_bytes_binary(b.padded_bytes as i64),
                    );
                    for u in b.unit_names() {
                        match findings.get(u) {
                            Some(serde_json::Value::String(f)) => println!("    {u}  ({f})"),
                            _ => println!("    {u}"),
                        }
                    }
                    for line in placement_lines(&packed.placement(b)) {
                        println!("    {line}");
                    }
                }
                println!(
                    "  {} batch(es) x {copies} copy/copies = {} cartridge(s) needed",
                    batches.len(),
                    batches.len() as i64 * copies,
                );
                if batches.iter().any(|b| packed.placement(b).spans.is_some()) {
                    println!("  {SPAN_COUNT_CAVEAT}");
                }
                if let Some(l) = largest {
                    println!("  {}", largest_unit_line(l));
                }
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
    prewrite_hash: bool,
    jobs: Option<usize>,
    confirm: collection::batch::RunConfirm,
    policy_aware: bool,
    json_output: bool,
    dry_run: bool,
    assume_yes: bool,
) -> Result<i32> {
    // `--jobs` stands in for `[staging] jobs` for this run (issue #368).
    let mut with_jobs;
    let config = match jobs {
        Some(jobs) => {
            if !(1..=crate::staging::jobs::MAX_JOBS).contains(&jobs) {
                return Err(TapectlError::Other(format!(
                    "--jobs {jobs}: stage between 1 and {} units at once",
                    crate::staging::jobs::MAX_JOBS
                )));
            }
            with_jobs = config.clone();
            with_jobs.staging.jobs = jobs;
            &with_jobs
        }
        None => config,
    };
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
    let (packed, budget) =
        collection::plan::plan_for_run(conn, config, lib, device, labels, policy_aware)?;
    let (batches, refused) = (&packed.batches, &packed.refused);
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
        // ADR-0012 item 113: only when the drive declares
        // post_warning_bytes; the line above is unchanged.
        if budget.plan_bound_bytes.is_some() {
            println!("  packing: {}", budget.describe());
        }
        print_refused_plain(refused);
    }

    let batch = batches.get(batch_idx).ok_or_else(|| {
        TapectlError::Other(format!(
            "collection \"{collection_name}\": batch {batch_idx} does not exist \
             ({} batch(es) currently pending — run `collection plan` to see them)",
            batches.len()
        ))
    })?;
    // ADR-0012 item 113: the units of this batch planned past the fill
    // ceiling, and (#478, items 83, 94, 95 and 123) the units that allow
    // spanning past it and the one that spans, named before anything is
    // staged (`volume write` names the exact ones again before the tape
    // moves). Empty without a plan bound.
    let at = packed.placement(batch);
    if !json_output {
        for line in placement_lines(&at) {
            println!("  batch {batch_idx}: {line}");
        }
    }

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
                    "packing_bytes": budget.packing_bytes(),
                    "past_ceiling": at.past_ceiling,
                    "may_span": at.may_span,
                    "spans": at.spans,
                    "units": batch.unit_names(),
                    "labels": labels,
                    "dry_run": true,
                    "snapshot_command": lib.snapshot_command,
                    "release_command": lib.release_command,
                    "refused": refused_json(refused),
                    "warnings": budget.warnings,
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
            if let Some(command) = &lib.snapshot_command {
                println!(
                    "  snapshot_command (would run before any unit is read; not run now): {command}"
                );
            }
        }
        // Copy-distinctness warnings (issue #144). Only a dry run prints
        // them here: a real run's `volume write` names them as it writes.
        for warning in &budget.warnings {
            eprintln!("{warning}");
        }
        return Ok(exit_code);
    }

    let report = collection::batch::execute_batch(
        conn,
        paths,
        config,
        lib,
        batch,
        labels,
        device,
        DEFAULT_BLOCK_SIZE,
        prewrite_hash,
        confirm,
        assume_yes,
    )?;

    // Issue #108: a file the release could not remove is named, whatever
    // else is printed — nothing will ever find it again.
    let stranded = stranded_paths(&report);
    let destination = labels.last().map(String::as_str).unwrap_or("?");
    if let Some(warning) = stranded_warning(destination, &stranded) {
        eprintln!("{warning}");
    }
    // A stage set released whole, or slice by slice to its last slice (#478).
    let sets_released =
        report.cleaned.as_ref().map_or(0, |c| c.sets_cleaned) + report.slices.sets_cleaned;
    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "collection": collection_name,
                "batch": batch_idx,
                "budget_bytes": budget.bytes,
                "budget_from": budget.binding_label,
                "packing_bytes": budget.packing_bytes(),
                "past_ceiling": at.past_ceiling,
                "may_span": at.may_span,
                "spans": at.spans,
                "units_staged": report.units_staged,
                "copies_written": report.copies_written,
                "staging_released": report.cleaned.is_some(),
                "stage_sets_released": sets_released,
                "held_by_pending_rewrite": report.cleaned.as_ref().map_or(0, |c| c.held_by_pending_rewrite.len()),
                "under_copied": report.under_copied.iter().map(|p| serde_json::json!({
                    "unit": p.unit_name,
                    "copies": p.copies,
                    "min_copies": p.min_copies,
                })).collect::<Vec<_>>(),
                // #478 (ADR-0012 item 129): slices released on their own from
                // sets that span cartridges, and those still short of copies.
                // Always present, as `staging clean --json` gives them.
                "slices_released": slice_runs_json(&report.slices.slices_released),
                "slices_short": slice_runs_json(&report.slices.slices_short),
                // Issue #108: the paths, not a count; nothing will find them
                // again.
                "stranded": stranded,
                "release_warning": report.release_warning,
                "snapshot_path": report.snapshot_path,
                "snapshot_release_warning": report.snapshot_release_warning,
                "refused": refused_json(refused),
            })
        );
    } else if let Some(warning) = &report.release_warning {
        // ADR-0012 2026-10-07 item 33: the copies are sealed and confirmed,
        // so a failed release is a warning and the run exits as before.
        println!(
            "collection \"{collection_name}\" batch {batch_idx}: {} unit(s) staged, {} \
             copy/copies written; staging NOT released",
            report.units_staged, report.copies_written,
        );
        eprintln!("{warning}");
    } else {
        match &report.cleaned {
            Some(cleaned) => {
                println!(
                    "collection \"{collection_name}\" batch {batch_idx}: {} unit(s) staged, {} copy/copies \
                     written, {} stage set(s) released",
                    report.units_staged, report.copies_written, sets_released,
                );
                // Issue #483: a pending `volume rewrite` holds its staged
                // slices; say why a set was not released.
                for (id, hold) in &cleaned.held_by_pending_rewrite {
                    println!("  {}", hold.describe(*id));
                }
            }
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
        // #478 (ADR-0012 item 129): a set that spans cartridges is released
        // slice by slice, whatever the whole-set release decided.
        print!("{}", slice_release_notice(&report.slices));
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
            snapshot_command: None,
            release_command: None,
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
                snapshot_command: None,
                release_command: None,
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
            false,
        )
        .unwrap();
        assert_eq!(code, 0, "no unit was refused, so the command must exit 0");
    }

    /// Issue #382: a loose file and a symlinked directory under the root
    /// belong to no unit and are never archived. `status` and `sync` both
    /// name them and exit non-zero, the way they do for a refused unit; the
    /// test above, the same fixture with nothing outside a unit, is the
    /// positive control that they exit 0 otherwise.
    #[test]
    fn content_outside_every_unit_makes_status_and_sync_exit_non_zero() {
        let conn = db::open_memory().unwrap();
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let lib = seed_three_unit_collection(&conn, root.path(), false);
        let root_path = std::path::Path::new(&lib.root).to_path_buf();
        std::fs::write(root_path.join("loose.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), root_path.join("delta")).unwrap();

        let status =
            collection::status::status_for_collection(&conn, &Config::default(), &lib).unwrap();
        let named: Vec<&str> = status.outside.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(named, vec!["delta", "loose.txt"]);

        let mut config = Config::default();
        config.collections.push(lib.clone());
        let (report, _) =
            collection::sync::sync_collection_with_config(&conn, &config, &lib, true).unwrap();
        assert_eq!(report.outside.len(), 2);

        let home = tempfile::tempdir().unwrap();
        let paths = TapectlPaths::new(home.path().to_path_buf());
        for command in [
            CollectionCommands::Status,
            CollectionCommands::Sync { dry_run: true },
        ] {
            let code = run(&conn, &paths, &config, &command, false, false, false).unwrap();
            assert_ne!(code, 0, "{command:?} must exit non-zero");
        }
    }
}

#[cfg(test)]
mod checksum_mode_tests {
    //! Issue #347: `collection sync` registers a unit with the resolved
    //! policy's checksum mode (dotfile `[policy]` > archive set >
    //! `[defaults]`) -- a fresh folder, a path-keyed one, and an adopted
    //! dotfile alike -- and never rewrites an existing unit's stored mode.
    use super::*;
    use crate::config::{CollectionConfig, TapectlPaths};
    use crate::db;

    struct Fixture {
        conn: Connection,
        root: tempfile::TempDir,
        home: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        Fixture {
            conn,
            root: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn lib(root: &std::path::Path, dotfiles: bool) -> CollectionConfig {
        CollectionConfig {
            name: "lib".into(),
            root: root.canonicalize().unwrap().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles,
            snapshot_command: None,
            release_command: None,
        }
    }

    fn sync(f: &Fixture, config: &Config) {
        let paths = TapectlPaths::new(f.home.path().to_path_buf());
        let code = run(
            &f.conn,
            &paths,
            config,
            &CollectionCommands::Sync { dry_run: false },
            false,
            false,
            false,
        )
        .unwrap();
        assert_eq!(code, 0);
    }

    fn stored_mode(conn: &Connection, name: &str) -> String {
        crate::db::queries::get_unit_by_name(conn, name)
            .unwrap()
            .unwrap_or_else(|| panic!("unit {name} must exist"))
            .checksum_mode
    }

    fn sha256_defaults(lib: CollectionConfig) -> Config {
        let mut config = Config::default();
        config.defaults.checksum_mode = "sha256".into();
        config.collections.push(lib);
        config
    }

    #[test]
    fn a_fresh_folder_is_registered_with_the_defaults_checksum_mode() {
        let f = fixture();
        std::fs::create_dir_all(f.root.path().join("alpha")).unwrap();
        sync(&f, &sha256_defaults(lib(f.root.path(), true)));
        assert_eq!(stored_mode(&f.conn, "lib/alpha"), "sha256");
    }

    #[test]
    fn a_path_keyed_folder_is_registered_with_the_defaults_checksum_mode() {
        let f = fixture();
        std::fs::create_dir_all(f.root.path().join("alpha")).unwrap();
        sync(&f, &sha256_defaults(lib(f.root.path(), false)));
        assert_eq!(stored_mode(&f.conn, "lib/alpha"), "sha256");
    }

    #[test]
    fn a_fresh_folder_takes_the_collections_archive_set_checksum_mode() {
        let f = fixture();
        f.conn
            .execute(
                "INSERT INTO archive_sets (name, checksum_mode) VALUES ('cold', 'sha256_on_archive')",
                [],
            )
            .unwrap();
        std::fs::create_dir_all(f.root.path().join("alpha")).unwrap();
        let mut l = lib(f.root.path(), true);
        l.archive_set = Some("cold".into());
        sync(&f, &sha256_defaults(l));
        assert_eq!(stored_mode(&f.conn, "lib/alpha"), "sha256_on_archive");
    }

    #[test]
    fn an_adopted_dotfile_without_a_checksum_mode_takes_the_defaults() {
        let f = fixture();
        let dir = f.root.path().join("alpha");
        std::fs::create_dir_all(&dir).unwrap();
        crate::unit::dotfile::write_dotfile(
            &dir.join(".tapectl-unit.toml"),
            &crate::unit::dotfile::UnitDotfile {
                uuid: uuid::Uuid::new_v4().to_string(),
                name: "lib/alpha".into(),
                created: "2026-01-01T00:00:00Z".into(),
                tags: vec![],
                tenant: "media".into(),
                archive_set: None,
                checksum_mode: None,
                compression: None,
                slice_size: None,
                warehouse_copies: None,
                allow_spanning: None,
                exclude_patterns: vec![],
            },
        )
        .unwrap();
        sync(&f, &sha256_defaults(lib(f.root.path(), true)));
        assert_eq!(stored_mode(&f.conn, "lib/alpha"), "sha256");
    }

    /// A production home already has units: changing `[defaults]` later must
    /// not rewrite the mode an existing unit was registered with.
    #[test]
    fn an_existing_unit_keeps_its_stored_checksum_mode() {
        let f = fixture();
        std::fs::create_dir_all(f.root.path().join("alpha")).unwrap();
        let l = lib(f.root.path(), true);
        let mut before = Config::default();
        before.collections.push(l.clone());
        sync(&f, &before);
        assert_eq!(stored_mode(&f.conn, "lib/alpha"), "mtime_size");

        sync(&f, &sha256_defaults(l));
        assert_eq!(
            stored_mode(&f.conn, "lib/alpha"),
            "mtime_size",
            "a later [defaults] change must not be applied retroactively"
        );
    }

    fn largest(bytes: u64, percent: f64, above: bool) -> collection::plan::LargestUnit {
        collection::plan::LargestUnit {
            name: "testlib/beta".into(),
            size_bytes: bytes,
            tape_budget_bytes: 10 * 1024 * 1024,
            percent_of_tape_budget: percent,
            above_a_tenth_of_a_cartridge: above,
        }
    }

    /// Issue #456: the human line names the unit, its size and its share of
    /// the tape; past a tenth of a cartridge it also says the packing order
    /// is due a re-examination, and below it does not.
    #[test]
    fn the_largest_unit_line_names_the_unit_its_size_and_its_share() {
        let line = largest_unit_line(&largest(3 * 1024 * 1024, 30.0, true));
        assert!(
            line.starts_with(
                "largest unit: testlib/beta, 3.0 MiB (30.0% of the tape's 10.0 MiB budget)"
            ),
            "{line}"
        );
        assert!(line.contains("above a tenth of a cartridge"), "{line}");
        assert!(line.contains("item 69"), "{line}");

        let line = largest_unit_line(&largest(512 * 1024, 5.0, false));
        assert!(line.contains("(5.0% of the tape's"), "{line}");
        assert!(!line.contains("above a tenth"), "{line}");
    }

    /// ADR-0012 item 113: `collection plan` and `collection run` name a
    /// batch's units past the fill ceiling, in the batch's order, and say
    /// nothing for a batch under it — every batch, without a plan bound.
    #[test]
    fn a_batch_names_its_units_past_the_fill_ceiling_and_only_those() {
        let bs = 512 * 1024u64;
        let unit = |name: &str, blocks: u64| collection::selector::PendingUnit {
            name: name.into(),
            size_bytes: blocks * bs,
        };
        let batch = collection::selector::Batch {
            units: vec![unit("lib/a", 3), unit("lib/b", 3), unit("lib/c", 2)],
            ..Default::default()
        };
        let lines = |fill: u64| {
            let room = collection::selector::BatchRoom {
                fill,
                bound: Some(u64::MAX),
            };
            placement_lines(&collection::plan::placement(
                &batch,
                room,
                &Default::default(),
            ))
        };
        assert!(lines(8 * bs).is_empty(), "all under");
        let lines = lines(4 * bs);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with("past the fill ceiling (the tape may end before them;"),
            "{lines:?}"
        );
        assert!(lines[0].ends_with(": lib/b, lib/c"), "{lines:?}");
    }

    /// #478 (ADR-0012 items 94, 95 and 123): the preview names the unit
    /// that spans, last on its batch's tape and continued on the next, and
    /// a unit that allows spanning past the fill ceiling as one that may. A
    /// 10-block tape at a 6-block ceiling: `lib/a` (3) and `lib/m` (4,
    /// spannable, past the ceiling, ending at 7) on one; `lib/z` (12,
    /// spannable) alone crosses the bound.
    #[test]
    fn the_preview_names_the_unit_that_spans_and_one_that_may() {
        let bs = 512 * 1024u64;
        let unit = |name: &str, blocks: u64| collection::selector::PendingUnit {
            name: name.into(),
            size_bytes: blocks * bs,
        };
        let room = collection::selector::BatchRoom {
            fill: 6 * bs,
            bound: Some(10 * bs),
        };
        let spannable: std::collections::HashSet<String> =
            ["lib/m".to_string(), "lib/z".to_string()].into();
        let lines = |units: Vec<collection::selector::PendingUnit>| {
            let batch = collection::selector::Batch {
                units,
                ..Default::default()
            };
            placement_lines(&collection::plan::placement(&batch, room, &spannable))
        };
        assert_eq!(
            lines(vec![unit("lib/z", 12)]),
            ["spans: last on its batch's tape, continued on the next: lib/z"]
        );
        assert_eq!(
            lines(vec![unit("lib/a", 3), unit("lib/m", 4)]),
            [
                "may span: last on its batch's tape, past the fill ceiling; if the tape ends \
                 inside it, continued on the next: lib/m"
            ]
        );
        assert!(lines(vec![unit("lib/a", 1), unit("lib/m", 1)]).is_empty());
    }

    /// #478 (ADR-0012 item 129): `collection run` names each spanning set's
    /// slices released and kept in `staging clean`'s words, and nothing
    /// when there were none.
    #[test]
    fn the_run_names_the_per_slice_release_as_staging_clean_does() {
        let report = crate::staging::clean::CleanReport {
            slices_released: vec![(7, vec![1, 2, 3])],
            slices_short: vec![(7, vec![4, 5])],
            ..Default::default()
        };
        assert_eq!(
            slice_release_notice(&report),
            "  stage set 7: slice(s) 1-3 released, each now has its copies (the set spans \
             cartridges and is released slice by slice)\n  stage set 7 kept in part: slice(s) \
             4-5 are short of copies and continue its span; `tapectl volume write <LABEL>` \
             onto another cartridge of the same generation writes them (--force would leave \
             the ranges already on tape unable to be continued)\n"
        );
        assert!(slice_release_notice(&Default::default()).is_empty());
        assert_eq!(
            slice_runs_json(&report.slices_released),
            serde_json::json!([{ "stage_set_id": 7, "slices": [1, 2, 3] }])
        );
    }

    /// Issue #108 and #478: the files a run's release could not remove are
    /// named on stderr, the whole-set release's and the per-slice release's
    /// alike, and nothing is said when every file went.
    #[test]
    fn the_run_names_every_stranded_file_of_both_releases() {
        let whole = crate::staging::clean::CleanReport {
            stranded: vec!["/staging/a.age".into()],
            ..Default::default()
        };
        let slices = crate::staging::clean::CleanReport {
            stranded: vec!["/staging/span_2.age".into()],
            ..Default::default()
        };
        let report = collection::batch::BatchExecutionReport {
            units_staged: 2,
            copies_written: 1,
            cleaned: Some(whole),
            under_copied: Vec::new(),
            slices,
            release_warning: None,
            snapshot_path: None,
            snapshot_release_warning: None,
        };
        let stranded = stranded_paths(&report);
        assert_eq!(
            stranded,
            [
                std::path::Path::new("/staging/a.age"),
                std::path::Path::new("/staging/span_2.age")
            ]
        );
        let warning = stranded_warning("L6-0003", &stranded).unwrap();
        assert!(
            warning.starts_with(
                "warning: volume \"L6-0003\" is sealed and confirmed and counts as a copy, but 2 \
                 staged file(s) could NOT be removed"
            ),
            "{warning}"
        );
        assert!(
            warning.ends_with("Remove by hand:\n  /staging/a.age\n  /staging/span_2.age"),
            "{warning}"
        );
        assert_eq!(stranded_warning("L6-0003", &[]), None);
    }

    /// `--json` carries the same facts, and `null` when nothing is pending.
    #[test]
    fn the_largest_unit_json_carries_the_share_and_is_null_when_nothing_is_pending() {
        let json = largest_unit_json(Some(&largest(3 * 1024 * 1024, 30.0, true)));
        assert_eq!(json["name"], "testlib/beta");
        assert_eq!(json["bytes"], 3 * 1024 * 1024);
        assert_eq!(json["tape_budget_bytes"], 10 * 1024 * 1024);
        assert_eq!(json["percent_of_tape_budget"], 30.0);
        assert_eq!(json["above_a_tenth_of_a_cartridge"], true);
        assert!(largest_unit_json(None).is_null());
    }
}
