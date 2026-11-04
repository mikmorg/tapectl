use std::collections::HashSet;

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
    /// Add a collection: a folder whose subfolders are each a unit of one
    /// tenant
    ///
    /// ROOT must be an absolute path to a directory that exists, neither
    /// inside another collection's root nor holding one. The tenant and the
    /// archive set must exist. The user tapectl runs as (the service user,
    /// under tapectl-op) must be able to read the whole tree and, unless
    /// --no-dotfiles, write a .tapectl-unit.toml into each unit folder; when
    /// it cannot, the setfacl lines that grant it are printed and nothing
    /// is added, since it cannot grant them itself. Then the
    /// [[collections]] table is written into config.toml, its comments
    /// kept, and the sync dry run is shown: `tapectl collection sync`
    /// registers the units.
    ///
    /// Exit status: 0 = added (with --dry-run: it would be); 2 = refused,
    /// nothing written.
    Add {
        /// The collection's name. It begins every unit name the collection
        /// registers: NAME/FOLDER
        name: String,
        /// The folder to walk: an absolute path
        root: String,
        /// The tenant the collection's units are registered under
        #[arg(long)]
        tenant: String,
        /// How deep the unit folders are: 1 = ROOT's own subfolders, 2 =
        /// theirs (show/season)
        #[arg(long, default_value = "1")]
        unit_depth: usize,
        /// A folder or file name to leave out, as a glob (repeat it for
        /// several), e.g. '*.partial'
        #[arg(long)]
        exclude: Vec<String>,
        /// The archive set the collection's units are registered in
        #[arg(long)]
        archive_set: Option<String>,
        /// Write no .tapectl-unit.toml: each unit is known by its path, for
        /// a source tapectl must not write. A renamed folder then reads as a
        /// new unit, and the old one as missing
        #[arg(long)]
        no_dotfiles: bool,
    },

    /// Take a collection out of config.toml; its units stay registered and
    /// untouched
    ///
    /// No sync walks its root after this, so none of its units is marked
    /// missing for it. They stay ordinary units, with their versions,
    /// copies and staged sets: `tapectl unit list` names them.
    Remove {
        /// The collection's name
        name: String,
    },

    /// Sync every configured collection: register new unit folders, resolve
    /// moved/renamed ones by dotfile uuid, mark vanished ones `missing`
    /// (never deleted or retired — those are operator acts).
    ///
    /// A unit registered before 1.9.0 whose .tapectl-unit.toml was never
    /// written is completed: the file is written with the unit's own
    /// identity. A dry run makes every check the real run makes, so it
    /// refuses what the real run would.
    ///
    /// Exit status: 0 = clean; 1 = warnings (a folder refused, an entry
    /// outside every unit, a unit marked missing); 2 = errors (a root that
    /// is not absolute or does not exist, a tenant or archive set that does
    /// not exist, permission denied, a .tapectl-unit.toml that could not be
    /// written). Every collection is synced whatever another's problems;
    /// the status is the worst seen.
    Sync {
        /// Report what would change without mutating anything.
        #[arg(long, short = 'n')]
        dry_run: bool,
    },

    /// Show each configured collection's pending, staged, under-copied and
    /// missing units
    ///
    /// Pending is a unit never archived or dirty (changed since its last
    /// version); staged is a version waiting in staging for a copy it is
    /// still owed, which `volume write` writes.
    ///
    /// Exit status: 0 = clean; 1 = warnings (a unit refused, an entry
    /// outside every unit, a unit missing); 2 = errors (a root that is not
    /// absolute or does not exist, a tenant or archive set that does not
    /// exist). Every collection is shown whatever another's problems; the
    /// status is the worst seen.
    Status,

    /// Show the batch plan (alphabetical next-fit, §7) for every
    /// configured collection's pending units.
    ///
    /// A unit that allows spanning (`allow_spanning`) is not held to one
    /// tape: it goes last in its batch, and the plan names the unit that
    /// spans onto the next cartridge. It is refused when it is larger than
    /// the staging area, or, on a drive that does not declare
    /// `post_warning_bytes`, larger than the room under the fill ceiling.
    ///
    /// The units already staged and still owed a copy are listed after the
    /// batches, with the command that writes them.
    ///
    /// Exit status: 0 = clean; 1 = a unit refused; 2 = errors (a
    /// collection's root, tenant or archive set, or no drive to plan
    /// against). Every collection is planned whatever another's problems.
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
    /// meets every unit's copy floor (the larger of its policy's
    /// `min_copies` and the sum of its `copies_at` targets) — otherwise staging is
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
        /// The collection to run. Defaults to the only configured
        /// collection; with several configured, name one.
        #[arg(value_name = "NAME")]
        collection: Option<String>,
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
        CollectionCommands::Add {
            name,
            root,
            tenant,
            unit_depth,
            exclude,
            archive_set,
            no_dotfiles,
        } => cmd_add(
            conn,
            paths,
            config,
            &NewCollection {
                name,
                root,
                tenant,
                unit_depth: *unit_depth,
                exclude,
                archive_set: archive_set.as_deref(),
                dotfiles: !*no_dotfiles,
            },
            json_output,
            global_dry_run,
        ),
        CollectionCommands::Remove { name } => {
            cmd_remove(conn, paths, config, name, json_output, global_dry_run)
        }
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
            name.as_deref(),
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

/// With no collection configured, `sync`, `status` and `plan` print an
/// empty list under `--json`, the shape they print for several (ADR-0012
/// item 220), and otherwise name the command that adds one.
fn no_collections(json_output: bool) {
    if json_output {
        crate::render::print_json(&serde_json::json!([]));
    } else {
        println!("no collection is configured; {}", collection::ADD_ONE);
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
/// `status`: a count, then each entry by its path under the root, then the
/// two ways out (ADR-0012 item 220). Nothing when there is none, so a clean
/// collection reads as before.
fn print_outside_plain(collection: &str, outside: &[OutsideEntry]) {
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
    println!(
        "    move each into a unit folder, or exclude it: tapectl config add \
         'collections[{collection}].exclude' PATTERN"
    );
}

/// `3 pending (2 never archived, 1 dirty)`: Pending as CONTEXT.md means it,
/// with its two forms beside it, a form that is zero left out (and the
/// parentheses with both). Always present, `0 pending` included, so the
/// line keeps its shape (ADR-0012 item 207).
fn pending_phrase(never_archived: usize, dirty: usize) -> String {
    let forms: Vec<String> = [(never_archived, "never archived"), (dirty, "dirty")]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, form)| format!("{n} {form}"))
        .collect();
    match forms.is_empty() {
        true => format!("{} pending", never_archived + dirty),
        false => format!("{} pending ({})", never_archived + dirty, forms.join(", ")),
    }
}

/// ` · 3 staged, awaiting a write`, or nothing when no unit is.
fn staged_phrase(staged: usize) -> String {
    match staged {
        0 => String::new(),
        n => format!(" · {n} staged, awaiting a write"),
    }
}

/// `collection status`'s line for one collection (ADR-0012 item 207):
/// `3 pending (2 never archived, 1 dirty) · 3 staged, awaiting a write · 3
/// under-copied · 1 missing`, each part after the first left out at zero.
fn status_line(name: &str, s: &collection::status::CollectionStatus, staged: usize) -> String {
    let mut line = format!(
        "collection \"{name}\": {}{}",
        pending_phrase(s.never_archived, s.dirty),
        staged_phrase(staged)
    );
    if s.under_copied > 0 {
        line.push_str(&format!(" · {} under-copied", s.under_copied));
    }
    if s.missing > 0 {
        line.push_str(&format!(" · {} missing", s.missing));
    }
    line
}

/// The unit names with a stage set held in staging that is still owed a
/// copy: Staged in CONTEXT.md's sense, and `stage list --owing`'s rows (the
/// one selection, `cli::stage::stage_set_rows`), read once per command.
fn owing_units(conn: &Connection, config: &Config) -> Result<HashSet<String>> {
    Ok(
        crate::cli::stage::stage_set_rows(conn, config, crate::cli::stage::Shown::Held)?
            .into_iter()
            .filter(|row| row.owing)
            .map(|row| row.unit)
            .collect(),
    )
}

/// The units of the collection whose canonical root is `root` that
/// `owing` names, in name order.
fn staged_under(conn: &Connection, root: &str, owing: &HashSet<String>) -> Result<Vec<String>> {
    let mut names: Vec<String> = collection::units_under_root(conn, root)?
        .into_iter()
        .map(|u| u.name)
        .filter(|name| owing.contains(name))
        .collect();
    names.sort();
    Ok(names)
}

/// The line for a collection that could not be read at all: one per
/// problem, under its name.
fn print_collection_errors(name: &str, errors: &[String]) {
    for e in errors {
        println!(
            "collection \"{name}\": {}: {e}",
            crate::render::verdict("error")
        );
    }
}

/// `collection sync`'s JSON object for one collection: the same keys
/// whatever happened, the counts `null` when its own problems stopped it.
fn sync_json(
    name: &str,
    r: &collection::sync::SyncReport,
    staged: Option<usize>,
    dry_run: bool,
) -> serde_json::Value {
    use collection::Severity;
    let stopped = r.problems.iter().any(|p| p.folder.is_none());
    let count = |n: usize| (!stopped).then_some(n);
    let messages = |severity: Severity| {
        r.problems
            .iter()
            .filter(|p| p.severity == severity)
            .map(|p| p.message.clone())
            .collect::<Vec<_>>()
    };
    serde_json::json!({
        "collection": name,
        "dry_run": dry_run,
        "created": count(r.created),
        "completed": count(r.completed),
        "refused_folders": count(r.refused_folders()),
        "moved": count(r.moved),
        "reactivated": count(r.reactivated),
        "missing": count(r.missing),
        "missing_units": r.missing_units.iter().map(|(unit, path)| {
            serde_json::json!({"unit": unit, "path": path})
        }).collect::<Vec<_>>(),
        // ADR-0012 item 207: pending as CONTEXT.md means it, and its forms.
        "pending": count(r.pending()),
        "never_archived": count(r.never_archived),
        "dirty": count(r.dirty),
        "staged": staged,
        "errors": messages(Severity::Error),
        "warnings": messages(Severity::Warning),
        "refused": refused_json(&r.refused),
        "outside": outside_json(&r.outside),
        "tenant_differs": r.tenant_differs.iter().map(|(unit, tenant)| {
            serde_json::json!({"unit": unit, "tenant": tenant})
        }).collect::<Vec<_>>(),
    })
}

/// `collection sync`'s text for one collection: `would create N; M
/// refused` in a dry run, `N created; M refused` in a real one (ADR-0012
/// item 220), then each problem, warning and entry outside every unit.
fn print_sync_plain(
    lib: &crate::config::CollectionConfig,
    r: &collection::sync::SyncReport,
    staged: usize,
    dry_run: bool,
) {
    use collection::Severity;
    let name = &lib.name;
    if r.problems.iter().any(|p| p.folder.is_none()) {
        let errors: Vec<String> = r.problems.iter().map(|p| p.message.clone()).collect();
        print_collection_errors(name, &errors);
        return;
    }
    let (mode, created) = match dry_run {
        true => (" (dry run)", format!("would create {}", r.created)),
        false => ("", format!("{} created", r.created)),
    };
    let completed = match r.completed {
        0 => String::new(),
        n => format!(", {n} completed"),
    };
    println!(
        "collection \"{name}\"{mode}: {created}{completed}; {} refused · {} moved, {} reactivated, \
         {} missing · {}{}",
        r.refused_folders(),
        r.moved,
        r.reactivated,
        r.missing,
        pending_phrase(r.never_archived, r.dirty),
        staged_phrase(staged),
    );
    if r.completed > 0 {
        println!(
            "  completed: {} registered before 1.9.0 without their {}; {} with each unit's own \
             identity",
            crate::render::plural(r.completed as i64, "unit", "units"),
            crate::unit::dotfile::UNIT_DOTFILE,
            if dry_run {
                "it would be written"
            } else {
                "written now"
            },
        );
    }
    for p in &r.problems {
        let word = match p.severity {
            Severity::Error => "error",
            Severity::Warning => "refused",
        };
        println!("  {}: {}", crate::render::verdict(word), p.message);
    }
    for (unit, tenant) in &r.tenant_differs {
        println!(
            "  {}: unit \"{unit}\" is owned by tenant \"{tenant}\", not the collection's \"{}\"; \
             it keeps its owner (new units go to \"{}\")",
            crate::render::verdict("warning"),
            lib.tenant,
            lib.tenant
        );
    }
    for (unit, path) in &r.missing_units {
        println!(
            "  {}: unit \"{unit}\": its folder {path} is gone; {} missing (never deleted)",
            crate::render::verdict("warning"),
            if dry_run {
                "it would be marked"
            } else {
                "marked"
            },
        );
    }
    print_refused_plain(&r.refused);
    print_outside_plain(name, &r.outside);
}

fn cmd_sync(conn: &Connection, config: &Config, dry_run: bool, json_output: bool) -> Result<i32> {
    if config.collections.is_empty() {
        no_collections(json_output);
        return Ok(crate::error::EXIT_SUCCESS);
    }

    let owing = owing_units(conn, config)?;
    let mut rows = Vec::new();
    let mut seen = Vec::new();
    for lib in &config.collections {
        // Issue #337, #382 and ADR-0012 item 220: every unit a sync could
        // not register, every entry no unit holds and every unit marked
        // missing is never archived, so each makes the exit status say so;
        // each collection's own problems are its own (OQ6).
        let report = collection::sync::sync_collection_with_config(conn, config, lib, dry_run)?;
        seen.extend(report.severities());
        let staged = match report.problems.iter().any(|p| p.folder.is_none()) {
            true => None,
            false => Some(staged_under(conn, &collection::canonical_root(lib)?, &owing)?.len()),
        };
        rows.push((lib, report, staged));
    }

    if json_output {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|(lib, r, staged)| sync_json(&lib.name, r, *staged, dry_run))
            .collect();
        crate::render::print_json(&serde_json::Value::Array(json));
    } else {
        for (lib, r, staged) in &rows {
            print_sync_plain(lib, r, staged.unwrap_or(0), dry_run);
        }
    }
    Ok(collection::Severity::exit_code(seen))
}

/// One collection's `collection status`: its scan, or the problems that
/// stopped it.
struct StatusRow<'a> {
    name: &'a str,
    status: Option<collection::status::CollectionStatus>,
    staged: usize,
    errors: Vec<String>,
}

fn cmd_status(conn: &Connection, config: &Config, json_output: bool) -> Result<i32> {
    use collection::Severity;
    if config.collections.is_empty() {
        no_collections(json_output);
        return Ok(crate::error::EXIT_SUCCESS);
    }

    let owing = owing_units(conn, config)?;
    let mut rows = Vec::new();
    let mut seen = Vec::new();
    for lib in &config.collections {
        // ADR-0012 item 220: a missing root or an unknown tenant is this
        // collection's error, said as one, and the next is still shown.
        let errors = collection::collection_problems(conn, lib)?;
        if !errors.is_empty() {
            seen.push(Severity::Error);
            rows.push(StatusRow {
                name: &lib.name,
                status: None,
                staged: 0,
                errors,
            });
            continue;
        }
        let status = collection::status::status_for_collection(conn, config, lib)?;
        if !status.refused.is_empty() || !status.outside.is_empty() || status.missing > 0 {
            seen.push(Severity::Warning);
        }
        let staged = staged_under(conn, &collection::canonical_root(lib)?, &owing)?.len();
        rows.push(StatusRow {
            name: &lib.name,
            status: Some(status),
            staged,
            errors: Vec::new(),
        });
    }

    if json_output {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                let s = row.status.as_ref();
                serde_json::json!({
                    "collection": row.name,
                    // ADR-0012 item 207: pending as CONTEXT.md means it, its
                    // two forms, and the staged units awaiting a write.
                    "pending": s.map(|s| s.pending()),
                    "never_archived": s.map(|s| s.never_archived),
                    "dirty": s.map(|s| s.dirty),
                    "staged": s.map(|_| row.staged),
                    "missing": s.map(|s| s.missing),
                    "under_copied": s.map(|s| s.under_copied),
                    "refused": s.map(|s| refused_json(&s.refused)).unwrap_or_default(),
                    "outside": s.map(|s| outside_json(&s.outside)).unwrap_or_default(),
                    "errors": row.errors,
                })
            })
            .collect();
        crate::render::print_json(&serde_json::Value::Array(json));
    } else {
        for row in &rows {
            match &row.status {
                None => print_collection_errors(row.name, &row.errors),
                Some(s) => {
                    println!("{}", status_line(row.name, s, row.staged));
                    print_refused_plain(&s.refused);
                    print_outside_plain(row.name, &s.outside);
                }
            }
        }
    }
    Ok(Severity::exit_code(seen))
}

/// `collection add`'s arguments.
struct NewCollection<'a> {
    name: &'a str,
    root: &'a str,
    tenant: &'a str,
    unit_depth: usize,
    exclude: &'a [String],
    archive_set: Option<&'a str>,
    dotfiles: bool,
}

impl NewCollection<'_> {
    fn config(&self) -> crate::config::CollectionConfig {
        crate::config::CollectionConfig {
            name: self.name.to_string(),
            root: self.root.to_string(),
            tenant: self.tenant.to_string(),
            unit_depth: self.unit_depth,
            exclude: self.exclude.to_vec(),
            archive_set: self.archive_set.map(str::to_string),
            dotfiles: self.dotfiles,
            snapshot_command: None,
            release_command: None,
        }
    }

    /// The `[[collections]]` table, as the `field=value` list
    /// `config_edit::plan_collection_add` takes: each value TOML text, so a
    /// name such as `2024` stays a string. A key at its default is left
    /// out, as an operator writing the table would.
    fn fields(&self) -> Vec<String> {
        let q = |v: &str| toml_edit::Value::from(v).to_string().trim().to_string();
        let mut fields = vec![
            format!("name={}", q(self.name)),
            format!("root={}", q(self.root)),
            format!("tenant={}", q(self.tenant)),
        ];
        if self.unit_depth != 1 {
            fields.push(format!("unit_depth={}", self.unit_depth));
        }
        if !self.exclude.is_empty() {
            let list: toml_edit::Array = self.exclude.iter().map(String::as_str).collect();
            fields.push(format!("exclude={}", list.to_string().trim()));
        }
        if let Some(set) = self.archive_set {
            fields.push(format!("archive_set={}", q(set)));
        }
        if !self.dotfiles {
            fields.push("dotfiles=false".to_string());
        }
        fields
    }
}

/// `collection add` (ADR-0012 item 206): refuse what would make the
/// collection unusable, check the access the user tapectl runs as needs
/// (`collection::access`), write the table, then show the sync dry run.
fn cmd_add(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    new: &NewCollection<'_>,
    json_output: bool,
    dry_run: bool,
) -> Result<i32> {
    let refuse = |why: String| Err(TapectlError::Other(why));
    collection::validate_collection_name(new.name)?;
    if config.collections.iter().any(|c| c.name == new.name) {
        return refuse(format!(
            "a collection named \"{}\" is already configured in {}; choose another name",
            new.name,
            paths.config_file.display()
        ));
    }
    if !std::path::Path::new(new.root).is_absolute() {
        return refuse(format!(
            "ROOT \"{}\" is not an absolute path: give the whole path, from /",
            new.root
        ));
    }
    let canonical = std::fs::canonicalize(new.root).map_err(|e| {
        TapectlError::Other(format!(
            "ROOT \"{}\" does not exist or cannot be reached: {e}",
            new.root
        ))
    })?;
    if !canonical.is_dir() {
        return refuse(format!("ROOT \"{}\" is not a directory", new.root));
    }
    if new.unit_depth == 0 {
        return refuse(
            "--unit-depth 0 would make ROOT itself the unit: give 1 for its subfolders, 2 for \
             theirs"
                .to_string(),
        );
    }
    if crate::db::queries::get_tenant_by_name(conn, new.tenant)?.is_none() {
        return refuse(format!(
            "tenant \"{}\" does not exist: `tapectl tenant list` names the tenants, and \
             `tapectl tenant add NAME` adds one",
            new.tenant
        ));
    }
    crate::db::queries::resolve_archive_set(conn, new.archive_set)?;
    if let Some(other) = config
        .collections
        .iter()
        .find(|c| collection::roots_overlap(new.root, &c.root))
    {
        return refuse(format!(
            "ROOT \"{}\" overlaps collection \"{}\" (root \"{}\"): a folder would belong to two \
             collections. Give a root outside it, and holding none",
            new.root, other.name, other.root
        ));
    }
    let lib = new.config();
    let access = collection::access::check(&canonical, &lib);
    if let Some(grant) = access.grant() {
        return refuse(grant);
    }

    let file = &paths.config_file;
    let original = std::fs::read_to_string(file)?;
    let planned = crate::config_edit::plan_collection_add(&original, file, new.fields())?;
    for w in &planned.warnings {
        eprintln!("{w}");
    }
    if !dry_run {
        crate::config_edit::replace_file(file, &original, &planned.new_text)?;
    }

    // The sync this collection would get now: nothing is registered by it.
    let mut with = config.clone();
    with.collections.push(lib.clone());
    let preview = collection::sync::sync_collection_with_config(conn, &with, &lib, true)?;
    if json_output {
        crate::render::print_json(&serde_json::json!({
            "collection": new.name,
            "file": file.display().to_string(),
            "added": !dry_run,
            "dry_run": dry_run,
            "table": planned.json["table"],
            "sync_preview": sync_json(new.name, &preview, Some(0), true),
        }));
    } else {
        let what = format!(
            "collection \"{}\" (root {}, tenant {}, a unit per folder {} below the root{})",
            new.name,
            new.root,
            new.tenant,
            crate::render::plural(new.unit_depth as i64, "level", "levels"),
            if new.dotfiles {
                ""
            } else {
                ", known by path: no dotfiles"
            },
        );
        match dry_run {
            true => println!(
                "would add {what} to {} (DRY RUN — no changes made)",
                file.display()
            ),
            false => println!("added {what} to {}", file.display()),
        }
        println!("its sync, as `tapectl collection sync` would run it now:");
        print_sync_plain(&lib, &preview, 0, true);
    }
    Ok(crate::error::EXIT_SUCCESS)
}

/// `collection remove` (ADR-0012 item 206, the coordinator's OQ23): the
/// table goes, and the collection's units stay registered and untouched. No
/// sync walks the root after this, so none of them is ever marked missing
/// for it; `collection status`, which shows the configured collections,
/// says nothing of them, and `unit list` and `audit` go on showing them as
/// the ordinary units they are.
fn cmd_remove(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    name: &str,
    json_output: bool,
    dry_run: bool,
) -> Result<i32> {
    let lib = collection::find_collection(config, name)?;
    let root = collection::canonical_root(lib)
        .unwrap_or_else(|_| lib.root.trim_end_matches('/').to_string());
    let units = collection::units_under_root(conn, &root)?.len();

    let file = &paths.config_file;
    let original = std::fs::read_to_string(file)?;
    let planned = crate::config_edit::plan_collection_remove(&original, file, name)?;
    if !dry_run {
        crate::config_edit::replace_file(file, &original, &planned.new_text)?;
    }
    if json_output {
        crate::render::print_json(&serde_json::json!({
            "collection": name,
            "file": file.display().to_string(),
            "removed": !dry_run,
            "dry_run": dry_run,
            "units_kept": units,
        }));
    } else {
        let kept = crate::render::plural(units as i64, "unit", "units");
        match dry_run {
            true => println!(
                "would remove collection \"{name}\" from {}; its {kept} would stay registered \
                 and untouched (DRY RUN — no changes made)",
                file.display()
            ),
            false => println!(
                "removed collection \"{name}\" from {}; its {kept} stay registered and \
                 untouched (`tapectl unit list` names them), and no sync walks {} now",
                file.display(),
                lib.root
            ),
        }
    }
    Ok(crate::error::EXIT_SUCCESS)
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
/// 95), and the one that spans (items 94 and 123), with the whole units that
/// go after its tail on its last tape when it is larger than one tape (item
/// 147). None when the batch fits under the ceiling, as every batch does
/// unless the drive declares `post_warning_bytes`.
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
        if at.after_tail.is_empty() {
            lines.push(format!(
                "spans: last on its batch's tape, continued on the next: {unit}"
            ));
        } else {
            lines.push(format!(
                "spans: larger than one tape, first on its batch's first tape, continued on \
                 the next: {unit}"
            ));
            lines.push(format!(
                "after the span's tail, on its last tape (ADR-0012 item 147): {}",
                at.after_tail.join(", ")
            ));
        }
    }
    lines
}

/// The line after `collection plan`'s cartridge count when a batch has a
/// unit that spans (#478): its continuation takes room the count does not.
const SPAN_COUNT_CAVEAT: &str = "the count is a minimum: a unit that spans continues on the next \
                                 cartridge, ahead of the units planned there";

/// Every file the run's release could not remove (issue #108): the
/// whole-set release's and the per-slice release's. Their `staging_path`
/// rows are already nulled, so no later `stage clean` can find them.
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
         file(s) could NOT be removed and no later `stage clean` can find them. Remove by hand:",
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
    use collection::Severity;
    if config.collections.is_empty() {
        no_collections(json_output);
        return Ok(crate::error::EXIT_SUCCESS);
    }
    // No drive to plan against is no collection's problem: refused here,
    // before any collection is read, as each plan below would refuse it.
    crate::config::resolve_lto_backend(config, device)?;

    let owing = owing_units(conn, config)?;
    let mut rows = Vec::new();
    let mut broken: Vec<(&str, Vec<String>)> = Vec::new();
    let mut seen = Vec::new();
    for lib in &config.collections {
        // ADR-0012 item 220: a collection's own problems are its error, and
        // the next is still planned.
        let errors = collection::collection_problems(conn, lib)?;
        if !errors.is_empty() {
            seen.push(Severity::Error);
            broken.push((&lib.name, errors));
            continue;
        }
        let (packed, tape) = collection::plan::plan_for_collection_with_budget(
            conn,
            config,
            lib,
            generation,
            device,
            policy_aware,
        )?;
        if !packed.refused.is_empty() {
            seen.push(Severity::Warning);
        }
        // Issue #456: the largest unit beside the tape, so ADR-0012 item 69's
        // trigger (a unit above a tenth of a cartridge) is visible.
        let largest = collection::plan::largest_unit(&packed.batches, &tape);
        // ADR-0012 item 207: the staged units, which no batch holds, and the
        // write they wait for.
        let staged = staged_under(conn, &collection::canonical_root(lib)?, &owing)?;
        rows.push((lib.name.clone(), packed, largest, tape, staged));
    }
    // `--policy-aware` (issue #144): what each prioritised unit falls short
    // of, printed beside it so the order explains itself.
    let findings = if policy_aware {
        shortfalls(
            conn,
            config,
            rows.iter().flat_map(|(_, p, _, _, _)| p.batches.iter()),
        )?
    } else {
        std::collections::HashMap::new()
    };

    if json_output {
        let mut json: Vec<serde_json::Value> = rows
            .iter()
            .map(|(name, packed, largest, tape, staged)| {
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
                            // #478 (items 83, 94, 95, 123 and 147): empty and
                            // null unless a unit allows spanning, and the
                            // drive declares post_warning_bytes.
                            "may_span": at.may_span,
                            "spans": at.spans,
                            "after_tail": at.after_tail,
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
                    // ADR-0012 item 207.
                    "staged": staged,
                    "write_staged": (!staged.is_empty()).then_some(WRITE_STAGED),
                    "errors": Vec::<String>::new(),
                })
            })
            .collect();
        // A collection its problems stopped: the same keys, nothing planned.
        json.extend(broken.iter().map(|(name, errors)| {
            serde_json::json!({
                "collection": name,
                "copies": copies,
                "policy_aware": policy_aware,
                "batches": null,
                "cartridges_needed": null,
                "largest_unit": null,
                "refused": [],
                "fill_bytes": null,
                "packing_bytes": null,
                "staged": [],
                "write_staged": null,
                "errors": errors,
            })
        }));
        crate::render::print_json(&serde_json::Value::Array(json));
    } else {
        for (name, packed, largest, tape, staged) in &rows {
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
            if !staged.is_empty() {
                println!(
                    "  staged, awaiting a write: {}: {}",
                    crate::render::plural(staged.len() as i64, "unit", "units"),
                    staged.join(", ")
                );
                println!("    write them: {WRITE_STAGED}");
            }
        }
        for (name, errors) in &broken {
            print_collection_errors(name, errors);
        }
    }
    Ok(Severity::exit_code(seen))
}

/// The command that writes what is staged (ADR-0012 item 207): the
/// one-command copy `volume write`'s help gives, onto a new cartridge.
const WRITE_STAGED: &str =
    "tapectl volume write <NEW-LABEL> --init --full-confirm --move-to <location>";

#[allow(clippy::too_many_arguments)]
fn cmd_run(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    collection_name: Option<&str>,
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
    let lib = collection::collection_to_run(config, collection_name)?;
    let collection_name = lib.name.as_str();
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
                    "after_tail": at.after_tail,
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
                "after_tail": at.after_tail,
                "units_staged": report.units_staged,
                "copies_written": report.copies_written,
                // ADR-0012 item 217: per unit. True when no unit of the batch
                // kept its staging (the release did not fail); the units
                // released beside a kept one are `units_released`.
                "staging_released": report.release_warning.is_none() && report.under_copied.is_empty(),
                "stage_sets_released": sets_released,
                "units_released": report.released_units,
                "held_by_pending_rewrite": report.cleaned.as_ref().map_or(0, |c| c.held_by_pending_rewrite.len()),
                "under_copied": report.under_copied.iter().map(|p| serde_json::json!({
                    "unit": p.unit_name,
                    "copies": p.copies,
                    // OQ5: the copy floor, named for what it is.
                    "copy_floor": p.min_copies,
                    // Additive (item 217): the per-location targets unmet.
                    "short_locations": p.short_at.iter().map(|l| serde_json::json!({
                        "location": l.location,
                        "copies": l.copies,
                        "target": l.target,
                    })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
                // #478 (ADR-0012 item 129): slices released on their own from
                // sets that span cartridges, and those still short of copies.
                // Always present, as `stage clean --json` gives them.
                "slices_released": crate::cli::staging::slice_runs_json(&report.slices.slices_released),
                "slices_short": crate::cli::staging::slice_runs_json(&report.slices.slices_short),
                // Issue #108: the paths, not a count; nothing will find them
                // again.
                "stranded": stranded,
                "release_warning": report.release_warning,
                "snapshot_path": report.snapshot_path,
                "snapshot_release_warning": report.snapshot_release_warning,
                "refused": refused_json(refused),
            })
        );
    } else {
        println!(
            "collection \"{collection_name}\" batch {batch_idx}: {} unit(s) staged, {} written \
             to \"{destination}\"",
            report.units_staged,
            crate::render::plural(report.copies_written as i64, "copy", "copies"),
        );
        // The release block `volume write` prints (ADR-0012 items 216 and
        // 217): the units released, the units kept and why, the slices of a
        // set that spans cartridges. Its stranded files were named above.
        let release = match report.release_warning {
            // Item 33: the copies are sealed and confirmed, so a failed
            // release is a warning and the run exits as before.
            Some(warning) => crate::cli::volume::StagingRelease::Failed(warning),
            None => crate::cli::volume::StagingRelease::of_outcome(
                crate::staging::release::ReleaseOutcome {
                    cleaned: report.cleaned,
                    released_units: report.released_units,
                    under_copied: report.under_copied,
                    slices: report.slices,
                },
                crate::staging::release::Hold::CopyTargets,
            ),
        };
        let (out, err) = crate::cli::volume::render_staging_release(destination, &release);
        for line in out {
            println!("{line}");
        }
        if let crate::cli::volume::StagingRelease::Failed(_) = release {
            for line in err {
                eprintln!("{line}");
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
            status.never_archived, 2,
            "alpha and gamma must still be counted pending, never archived"
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
        let report =
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
                copies_at: None,
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

    /// #478 (ADR-0012 items 94, 95, 123 and 147): the preview names the unit
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
        // Item 147: beside whole units, `lib/z`, larger than one tape, starts
        // its batch's first tape; by X11's estimate (10 blocks a tape) its
        // last tape holds its last 2, and `lib/a` goes after that tail.
        assert_eq!(
            lines(vec![unit("lib/a", 1), unit("lib/z", 12)]),
            [
                "spans: larger than one tape, first on its batch's first tape, continued on the \
                 next: lib/z",
                "after the span's tail, on its last tape (ADR-0012 item 147): lib/a",
            ]
        );
    }

    /// #478 (ADR-0012 item 129): `collection run` names each spanning set's
    /// slices released and kept in `stage clean`'s words, and nothing
    /// when there were none.
    #[test]
    fn the_run_names_the_per_slice_release_as_staging_clean_does() {
        let report = crate::staging::clean::CleanReport {
            slices_released: vec![(7, vec![1, 2, 3])],
            slices_short: vec![(7, vec![4, 5])],
            ..Default::default()
        };
        assert_eq!(
            crate::cli::staging::slice_release_notice(&report),
            "  stage set 7: slice(s) 1-3 released, each now has its copies (the set spans \
             cartridges and is released slice by slice)\n  stage set 7 kept in part: slice(s) \
             4-5 are short of copies and continue its span; `tapectl volume write <LABEL>` \
             onto another cartridge of the same generation writes them (--force would leave \
             the ranges already on tape unable to be continued)\n"
        );
        assert!(crate::cli::staging::slice_release_notice(&Default::default()).is_empty());
        assert_eq!(
            crate::cli::staging::slice_runs_json(&report.slices_released),
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
            released_units: Vec::new(),
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
