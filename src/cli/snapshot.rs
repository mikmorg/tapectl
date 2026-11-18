use clap::Subcommand;
use rusqlite::{params, Connection};
use serde::Serialize;

use crate::config::{Config, TapectlPaths};
use crate::error::{Result, TapectlError};
use crate::staging;

/// `snapshot create --help`'s examples (ADR-0012 items 211 and 225).
const CREATE_EXAMPLES: &str = "\
Examples:
  Snapshot two units by name:
    tapectl snapshot create photos-2024 taxes
  Snapshot every unit that is pending (never archived, or dirty):
    tapectl snapshot create --all-pending";

#[derive(Subcommand, Debug)]
pub enum SnapshotCommands {
    /// Record what each unit holds now: a new version when its content changed
    ///
    /// Walks each unit's directory and hashes its files; a unit unchanged
    /// since its newest version is reported and gets none. Each unit is
    /// walked and reported on its own: one that fails does not stop the
    /// others. Under --json, an array of one object per unit, printed only
    /// when every unit was snapshotted. --all-pending takes every active unit
    /// that is pending (never archived, or dirty), in name order, and ends
    /// with a total.
    ///
    /// Exit status: 0 = every unit snapshotted (or unchanged), or nothing
    /// pending; 2 = a unit failed, each named, the others snapshotted.
    #[command(after_long_help = CREATE_EXAMPLES)]
    Create {
        /// The units to snapshot
        #[arg(required_unless_present = "all_pending", value_name = "NAME")]
        name: Vec<String>,
        /// Snapshot every pending unit: never archived, or dirty
        ///
        /// Every active unit whose source has no snapshot or no longer
        /// matches its newest one, in name order, each as if named. The same
        /// scan `status` and `report dirty` count pending by. With nothing
        /// pending it says so and exits 0.
        #[arg(long, conflicts_with = "name")]
        all_pending: bool,
    },

    /// List the snapshots, newest version first
    List {
        /// Only this unit's snapshots
        #[arg(long)]
        unit: Option<String>,
        /// Only the snapshots of this status
        ///
        /// One of created, staged, current, reclaimable, purged.
        #[arg(long)]
        status: Option<String>,
    },

    /// Show the files added, removed and changed between two versions
    Diff {
        /// The unit
        name: String,
        /// The first version
        #[arg(long)]
        v1: i64,
        /// The second version
        #[arg(long)]
        v2: i64,
    },

    /// Delete a snapshot no tape holds
    ///
    /// Refused for a version on tape (a completed write or a range), for one
    /// a write session holds (running now, interrupted, or aborted with its
    /// seal recorded, which `volume resume` can still take up), and for one
    /// with staged data unless --force.
    Delete {
        /// The unit
        name: String,
        /// The version to delete
        #[arg(long)]
        version: i64,
        /// Delete it though it is staged: its staged data goes too
        ///
        /// Never deletes a version on tape, nor one a write session holds.
        #[arg(long)]
        force: bool,
    },

    /// Remove a reclaimable snapshot's catalog records
    Purge {
        /// The unit
        name: String,
        /// The version, which must be reclaimable
        #[arg(long)]
        version: i64,
    },

    /// Mark a superseded version reclaimable, so compaction may drop it
    ///
    /// Refused while the version superseding it is short of its copy floor
    /// or a copy target, unless confirmed (--force, --yes, or a terminal's
    /// prompt); a version no current version supersedes only with --force.
    MarkReclaimable {
        /// The unit
        name: String,
        /// The version to mark
        #[arg(long)]
        version: i64,
        /// Mark it though its successor is short, or nothing supersedes it
        ///
        /// Confirm in advance, as the global --yes does: mark the version
        /// even though the version superseding it is short of its copy floor
        /// (the larger of its policy's min_copies and the sum of its
        /// copies_at targets) or of a copy target at a location of its
        /// copies_at map (an archive set's required_locations counts as one
        /// copy at each name); for a tape-only unit, the multiplied floors.
        /// Without either flag a terminal asks, and a
        /// non-interactive run refuses, naming the shortfall. Only --force
        /// (not --yes, not a prompt) marks a version no current version
        /// supersedes — that releases the unit's only current version
        #[arg(long)]
        force: bool,
    },
}

#[derive(Serialize)]
struct SnapshotRow {
    id: i64,
    unit: String,
    version: i64,
    status: String,
    /// Table-only until CTO decision 2026-09-11 (architecture review C2
    /// follow-up, C2b). Renamed to `file_count` to match `snapshot create
    /// --json`'s existing key for the same `file_count` column.
    #[serde(rename = "file_count")]
    files: Option<i64>,
    /// Table-only until CTO decision 2026-09-11 (architecture review C2
    /// follow-up, C2b). Raw bytes; renamed to `total_size` to match
    /// `snapshot create --json`'s existing key for the same `total_size`
    /// column.
    #[serde(rename = "total_size")]
    size: Option<i64>,
    /// Table-only until CTO decision 2026-09-11 (architecture review C2
    /// follow-up, C2b). `created_at` is NOT NULL, so this stays a plain
    /// `String` (not `Option`) -- renamed to match the DB column name.
    /// RFC 3339 with `Z` in the JSON (ADR-0012 item 212; the stored value
    /// is unchanged).
    #[serde(rename = "created_at", serialize_with = "json_time")]
    created: String,
}

fn json_time<S: serde::Serializer>(t: &str, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(&crate::render::json_time(t))
}

/// `snapshot list --json` shape. `files`/`size`/`created` were table-only
/// until CTO decision 2026-09-11 (architecture review C2 follow-up, C2b).
fn snapshot_rows_to_json(rows: &[SnapshotRow]) -> serde_json::Value {
    serde_json::to_value(rows).unwrap()
}

/// `snapshot list`'s table (ADR-0012 item 212): borderless, sizes human
/// (binary: a data size), times in UTC.
fn snapshot_table(rows: &[SnapshotRow], width: Option<usize>) -> String {
    crate::render::table_with(
        &[
            "id", "unit", "version", "status", "files", "size", "created",
        ],
        rows.iter()
            .map(|r| {
                vec![
                    r.id.to_string(),
                    r.unit.clone(),
                    format!("v{}", r.version),
                    r.status.clone(),
                    r.files.map(|n| n.to_string()).unwrap_or_default(),
                    r.size
                        .map(crate::util::format_bytes_binary)
                        .unwrap_or_default(),
                    crate::render::table_time(&r.created),
                ]
            })
            .collect(),
        width,
    )
}

/// `snapshots.status`'s CHECK constraint
/// (`src/db/migrations/026_drop_unwritten_states.sql`, which dropped
/// `superseded` and `failed`: nothing ever wrote either, issue #362). Pinned
/// to the live schema by `snapshot_statuses_equal_the_live_check`.
const SNAPSHOT_STATUSES: &[&str] = &["created", "staged", "current", "reclaimable", "purged"];

/// `snapshot list --status` is a usage error when it names anything other
/// than one of `SNAPSHOT_STATUSES` (issue #171, ADR-0012).
fn validate_snapshot_status(value: &str) -> Result<()> {
    crate::config::validate_closed_set("--status", value, SNAPSHOT_STATUSES)
        .map_err(TapectlError::Other)
}

/// What one unit's `snapshot create` printed: its line and its JSON object.
struct Created {
    line: String,
    json: serde_json::Value,
}

/// `snapshot create` of one unit.
fn create_one(conn: &Connection, config: &Config, name: &str) -> Result<Created> {
    // ADR-0012 / issue #159: `snapshot_create_detailed` may report an
    // existing version instead of minting one — `outcome.minted` says which.
    // Success either way, not a refusal. `outcome.snapshot_id` names the row
    // (new or reused) to look up below regardless.
    let outcome = staging::snapshot_create_detailed(conn, name, config)?;

    let (total_size, file_count): (Option<i64>, Option<i64>) = conn.query_row(
        "SELECT total_size, file_count FROM snapshots WHERE id = ?1",
        params![outcome.snapshot_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    // #542: what the version left out (the newest version's, when nothing
    // was minted), so a file on no tape is never silent.
    let left_out = crate::db::snapshot_left_out::load(conn, outcome.snapshot_id)?;

    let json = serde_json::json!({
        "snapshot_id": outcome.snapshot_id,
        "unit": name,
        "version": outcome.version,
        "total_size": total_size,
        "file_count": file_count,
        // Same shape whether minted or not (issue #159): one key, `true` on
        // the created path, `false` when an existing version was reported
        // instead — never two different JSON shapes for the same command.
        "minted": outcome.minted,
        "left_out": left_out.iter().map(left_out_json).collect::<Vec<_>>(),
    });
    let mut line = if outcome.minted {
        format!(
            "snapshot created: {} v{} ({}, {})",
            name,
            outcome.version,
            crate::render::plural(file_count.unwrap_or(0), "file", "files"),
            crate::util::format_bytes_binary(total_size.unwrap_or(0)),
        )
    } else {
        format!(
            "unit \"{name}\" is unchanged since v{}; no snapshot created",
            outcome.version,
        )
    };
    line.push_str(&left_out_lines(&left_out));
    Ok(Created { line, json })
}

/// One thing a version left out, as `--json` carries it (#542).
fn left_out_json(row: &crate::db::snapshot_left_out::LeftOut) -> serde_json::Value {
    serde_json::json!({
        "reason": row.reason.as_str(),
        "pattern": row.pattern,
        "path": row.path,
        "kind": row.kind(),
        "files": row.files,
        "bytes": row.bytes,
    })
}

/// How many left-out things `snapshot create` names in its text before it
/// counts the rest; `--json` and the catalog carry them all.
const LEFT_OUT_LINES: usize = 20;

/// The lines naming what a version left out: the pattern or the socket
/// rule, the path, and for a folder what was under it (#542). Empty when
/// nothing was left out. Names are escaped as the index escapes them, so a
/// name with a newline stays on its line.
fn left_out_lines(rows: &[crate::db::snapshot_left_out::LeftOut]) -> String {
    use crate::db::snapshot_left_out::Reason;
    let mut out = String::new();
    for row in rows.iter().take(LEFT_OUT_LINES) {
        let why = match (row.reason, &row.pattern) {
            (Reason::Pattern, Some(p)) => format!("pattern {p}"),
            _ => "socket".to_string(),
        };
        let what = if row.folder {
            format!(
                "folder, {why}, {} under it, {}",
                crate::render::plural(row.files, "file", "files"),
                crate::util::format_bytes_binary(row.bytes)
            )
        } else if row.reason == Reason::Socket {
            why
        } else {
            format!("{why}, {}", crate::util::format_bytes_binary(row.bytes))
        };
        out.push_str(&format!(
            "\n  left out: {} ({what})",
            crate::volume::index::escape(&row.path)
        ));
    }
    if rows.len() > LEFT_OUT_LINES {
        out.push_str(&format!(
            "\n  and {} more left out; `--json` lists them all",
            rows.len() - LEFT_OUT_LINES
        ));
    }
    out
}

/// The error a `snapshot create` of `named` units ends with, `None` when
/// every unit was snapshotted. One unit named: its own error, exactly as
/// before. Several: one error naming each unit that failed and why; and
/// when a signal stopped the run before the units in `not_started`, a stop
/// (`Interrupted`) that names them and the command that snapshots them.
///
/// `done` is each unit that was snapshotted, by its line. The text output
/// printed those lines as each unit finished, so the error says they are
/// above; under `--json` nothing went to stdout (one document, item 212: the
/// error is it), so the error names them itself.
fn end_of_run(
    named: usize,
    mut failed: Vec<(&str, TapectlError)>,
    not_started: &[&str],
    done: &[String],
    json_output: bool,
) -> Option<TapectlError> {
    if failed.is_empty() && not_started.is_empty() {
        return None;
    }
    if named == 1 && not_started.is_empty() {
        return Some(failed.remove(0).1);
    }
    let mut failures = String::new();
    for (name, e) in &failed {
        failures.push_str(&format!("\n  {name}: {e}"));
    }
    let snapshotted = if !json_output {
        String::new()
    } else if done.is_empty() {
        "\nnone was snapshotted".to_string()
    } else {
        format!("\nsnapshotted:\n  {}", done.join("\n  "))
    };
    if !not_started.is_empty() {
        return Some(TapectlError::Interrupted(format!(
            "snapshot create stopped on a signal before {}; run `tapectl snapshot create {}` \
             to snapshot {}{}{failures}{snapshotted}",
            not_started.join(", "),
            not_started.join(" "),
            if not_started.len() == 1 { "it" } else { "them" },
            if failed.is_empty() { "" } else { ". Failed:" },
        )));
    }
    let head = if failed.len() == named {
        format!("all {named} units failed:")
    } else if json_output {
        format!(
            "{} of {named} failed:",
            crate::render::plural(failed.len() as i64, "unit", "units")
        )
    } else {
        format!(
            "{} of {named} failed (the others are reported above):",
            crate::render::plural(failed.len() as i64, "unit", "units")
        )
    };
    Some(TapectlError::Other(format!(
        "{head}{failures}{snapshotted}"
    )))
}

/// `snapshot create` of `names`, each judged and reported on its own (ADR-0012
/// item 210): the named form, and `--all-pending` once it has the list.
fn create_named(
    conn: &Connection,
    config: &Config,
    names: &[String],
    json_output: bool,
) -> Result<()> {
    let rows = create_named_rows(conn, config, names, json_output)?;
    if json_output {
        crate::render::print_json(&serde_json::Value::Array(rows));
    }
    Ok(())
}

/// [`create_named`] without its closing JSON document: the rows, for a caller
/// that has more to say in the same document (`--all-pending`, #539).
fn create_named_rows(
    conn: &Connection,
    config: &Config,
    names: &[String],
    json_output: bool,
) -> Result<Vec<serde_json::Value>> {
    for (i, n) in names.iter().enumerate() {
        if names[..i].contains(n) {
            return Err(TapectlError::Other(format!(
                "unit \"{n}\" is named twice; a unit is snapshotted once"
            )));
        }
    }
    // ADR-0012 item 210: several units, each judged and reported on
    // its own as it finishes; a unit that fails does not stop the
    // ones after it.
    let mut rows = Vec::with_capacity(names.len());
    let mut done: Vec<String> = Vec::with_capacity(names.len());
    let mut failed: Vec<(&str, TapectlError)> = Vec::new();
    let mut not_started: Vec<&str> = Vec::new();
    for (i, name) in names.iter().enumerate() {
        // Issue #404: a signal stops the run between units, the
        // next unit's walk not started.
        if i > 0 && crate::signal::is_interrupted() {
            not_started = names[i..].iter().map(String::as_str).collect();
            break;
        }
        match create_one(conn, config, name) {
            Ok(created) => {
                if !json_output {
                    println!("{}", created.line);
                }
                rows.push(created.json);
                done.push(created.line);
            }
            Err(e) => failed.push((name, e)),
        }
    }
    // ADR-0012 item 212, one shape and one document: under --json an
    // array, one object per unit, whether one unit was named or
    // several (as `stage create` prints); and only when every unit
    // was snapshotted. On any failure the error is the one document
    // (JSON on stdout under --json), naming what was snapshotted.
    if let Some(err) = end_of_run(names.len(), failed, &not_started, &done, json_output) {
        return Err(err);
    }
    Ok(rows)
}

/// `snapshot create --all-pending` (ADR-0012 item 225): every active unit
/// that needs a snapshot (CONTEXT.md: never archived, or dirty), in name
/// order, snapshotted as `create_named` snapshots named units, then a total.
/// An unstaged unit (#529) is Pending too but has its snapshot: `snapshot
/// create` would mint nothing, so the run names it with the `stage create`
/// that it needs, and never says "nothing pending" while one exists.
///
/// Pending is decided by `report::dirty_rows`, the one scan `status` and
/// `report dirty` count it by (`fingerprint::classify` per active unit). A
/// unit that scan could not read (a dotfile it refuses) is not pending and
/// not snapshotted, and the run does not end clean (ADR-0012 item 249, #539):
/// it names each such unit on stderr, finishes the units it could scan, and
/// then exits non-zero, never saying "nothing pending" about a unit it did
/// not scan.
fn create_all_pending(conn: &Connection, config: &Config, json_output: bool) -> Result<()> {
    let mut pending: Vec<String> = Vec::new();
    let mut unstaged: Vec<String> = Vec::new();
    let mut not_scanned: Vec<(String, String)> = Vec::new();
    let (mut never_archived, mut dirty) = (0i64, 0i64);
    for row in crate::cli::report::dirty_rows(conn, None, &config.defaults.global_excludes)? {
        match row.state {
            "new" => never_archived += 1,
            "dirty" => dirty += 1,
            // Snapshotted already: it needs `stage create`, which the run
            // says instead of minting nothing for it.
            "unstaged" => {
                unstaged.push(row.name);
                continue;
            }
            "unreadable" => {
                let why = row.error.unwrap_or_else(|| "unreadable".to_string());
                eprintln!(
                    "warning: unit \"{}\" was not scanned, so it is not snapshotted: {why}",
                    row.name
                );
                not_scanned.push((row.name, why));
                continue;
            }
            _ => continue,
        }
        pending.push(row.name);
    }
    let skipped_error = |rows: &[serde_json::Value]| -> Option<TapectlError> {
        if not_scanned.is_empty() {
            return None;
        }
        let names: Vec<&str> = not_scanned.iter().map(|(n, _)| n.as_str()).collect();
        let error = TapectlError::Other(format!(
            "{} not scanned, so {} not snapshotted and may be pending: {}; \
             `tapectl report dirty` says why",
            crate::render::plural(names.len() as i64, "unit was", "units were"),
            if names.len() == 1 {
                "it was"
            } else {
                "they were"
            },
            names.join(", ")
        ));
        Some(TapectlError::with_json(
            serde_json::json!({
                "snapshotted": rows,
                "not_scanned": not_scanned
                    .iter()
                    .map(|(n, why)| serde_json::json!({"unit": n, "error": why}))
                    .collect::<Vec<_>>(),
            }),
            error,
        ))
    };
    let total_line = || {
        format!(
            "total: {} ({never_archived} never archived, {dirty} dirty)",
            crate::render::plural(pending.len() as i64, "pending unit", "pending units"),
        )
    };
    let unstaged_line = || {
        format!(
            "{} never staged: tapectl stage create {}",
            crate::render::plural(
                unstaged.len() as i64,
                "unit has a snapshot",
                "units have a snapshot"
            ),
            unstaged.join(" ")
        )
    };
    if pending.is_empty() {
        if let Some(err) = skipped_error(&[]) {
            return Err(err);
        }
        if json_output {
            crate::render::print_json(&serde_json::Value::Array(Vec::new()));
        } else if unstaged.is_empty() {
            println!("nothing pending: every active unit's source matches its newest snapshot");
        } else {
            println!("nothing to snapshot: {}", unstaged_line());
        }
        return Ok(());
    }
    let rows = create_named_rows(conn, config, &pending, json_output)?;
    if let Some(err) = skipped_error(&rows) {
        if !json_output {
            println!("{}", total_line());
        }
        return Err(err);
    }
    if json_output {
        crate::render::print_json(&serde_json::Value::Array(rows));
    } else {
        println!("{}", total_line());
        if !unstaged.is_empty() {
            println!("{}", unstaged_line());
        }
    }
    Ok(())
}

pub fn run(
    conn: &Connection,
    _paths: &TapectlPaths,
    config: &Config,
    command: &SnapshotCommands,
    json_output: bool,
    dry_run: bool,
    assume_yes: bool,
) -> Result<()> {
    match command {
        SnapshotCommands::Create {
            name: names,
            all_pending,
        } => {
            // Issue #241: the sha256 directory walk IS the work — the
            // only way to know whether content changed (and so whether a
            // new version would be minted) is to run it, so there is
            // nothing cheaper to offer as a preview.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "snapshot create",
                    "the directory walk and hash comparison that decide whether a new \
                     version would be minted ARE the command's own work.",
                ));
            }
            if *all_pending {
                create_all_pending(conn, config, json_output)?;
            } else {
                create_named(conn, config, names, json_output)?;
            }
        }

        SnapshotCommands::Diff { name, v1, v2 } => {
            crate::cli::operations::snapshot_diff(conn, name, *v1, *v2, json_output)?;
        }

        SnapshotCommands::Delete {
            name,
            version,
            force,
        } => {
            // Issue #241: reproduces `snapshot_delete`'s refusals (on tape,
            // `operations::snapshot_on_tape_refusal`: a completed write or a
            // range; a write session holding the staged slices, running or
            // resumable, `operations::snapshot_write_hold_refusal`, #540;
            // staged data without --force) so a dry run
            // refuses exactly what the real delete would refuse, without
            // touching a row or a staged file.
            if dry_run {
                let unit = crate::db::queries::get_unit_by_name(conn, name)?
                    .ok_or_else(|| TapectlError::UnitNotFound(name.clone()))?;
                let (snap_id, _status): (i64, String) = conn
                    .query_row(
                        "SELECT id, status FROM snapshots WHERE unit_id = ?1 AND version = ?2",
                        params![unit.id, version],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(|_| {
                        TapectlError::Other(format!("snapshot v{version} not found for \"{name}\""))
                    })?;
                if let Some(refusal) =
                    crate::cli::operations::snapshot_on_tape_refusal(conn, snap_id, *version)?
                {
                    return Err(TapectlError::Other(refusal));
                }
                if let Some(refusal) = crate::cli::operations::snapshot_write_hold_refusal(
                    conn, snap_id, name, *version,
                )? {
                    return Err(TapectlError::Other(refusal));
                }
                let staged_count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM stage_sets WHERE snapshot_id = ?1 AND status = \
                     'staged'",
                    params![snap_id],
                    |row| row.get(0),
                )?;
                if staged_count > 0 && !force {
                    return Err(TapectlError::Other(format!(
                        "snapshot v{version} has staged data — use --force to delete anyway"
                    )));
                }
                if json_output {
                    crate::render::print_json(
                        &serde_json::json!({"unit": name, "version": version, "dry_run": true}),
                    );
                } else {
                    println!("would delete snapshot {name} v{version} (DRY RUN — no changes made)");
                }
                return Ok(());
            }
            crate::cli::operations::snapshot_delete(conn, name, *version, *force, json_output)?;
        }

        SnapshotCommands::Purge { name, version } => {
            // Issue #241: reproduces `snapshot_purge`'s own precondition
            // (status must be 'reclaimable') so a dry run refuses exactly
            // what the real purge would refuse.
            if dry_run {
                let unit = crate::db::queries::get_unit_by_name(conn, name)?
                    .ok_or_else(|| TapectlError::UnitNotFound(name.clone()))?;
                let status: String = conn
                    .query_row(
                        "SELECT status FROM snapshots WHERE unit_id = ?1 AND version = ?2",
                        params![unit.id, version],
                        |row| row.get(0),
                    )
                    .map_err(|_| {
                        TapectlError::Other(format!("snapshot v{version} not found for \"{name}\""))
                    })?;
                if status != "reclaimable" {
                    return Err(TapectlError::Other(format!(
                        "snapshot v{version} status is \"{status}\", must be \"reclaimable\" \
                         to purge"
                    )));
                }
                if json_output {
                    crate::render::print_json(
                        &serde_json::json!({"unit": name, "version": version, "dry_run": true}),
                    );
                } else {
                    println!("would purge snapshot {name} v{version} (DRY RUN — no changes made)");
                }
                return Ok(());
            }
            crate::cli::operations::snapshot_purge(conn, name, *version, json_output)?;
        }

        SnapshotCommands::MarkReclaimable {
            name,
            version,
            force,
        } => {
            // Issue #241: enforces policy preconditions including the
            // tape-only 2x copy multiplier (Milestone 6) — a gate a dry
            // run must reproduce exactly or it lies about what the real
            // run would refuse. Refuse rather than risk that divergence.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "snapshot mark-reclaimable",
                    "it enforces the resolved policy's coverage preconditions (including the \
                     tape-only 2x copy multiplier), a gate a preview would have to reproduce \
                     exactly or risk being wrong.",
                ));
            }
            // Issue #348: the global `--yes` is the same advance Tier-2
            // consent `--force` is here — `consent::confirm`'s
            // non-interactive refusal tells the operator to re-run with
            // `--yes`, which is only true if it arrives.
            crate::cli::operations::snapshot_mark_reclaimable(
                conn,
                config,
                name,
                *version,
                *force,
                assume_yes,
                json_output,
            )?;
        }

        SnapshotCommands::List { unit, status } => {
            if let Some(st) = status {
                validate_snapshot_status(st)?;
            }
            // ADR-0012 item 212: an unknown --unit is an error, never an
            // empty list.
            if let Some(name) = unit {
                if crate::db::queries::get_unit_by_name(conn, name)?.is_none() {
                    return Err(TapectlError::UnitNotFound(name.clone()));
                }
            }
            let mut sql = String::from(
                "SELECT s.id, u.name, s.version, s.status, s.file_count,
                        s.total_size, s.created_at
                 FROM snapshots s JOIN units u ON u.id = s.unit_id WHERE 1=1",
            );
            let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

            if let Some(unit_name) = unit {
                sql.push_str(" AND u.name = ?");
                param_values.push(Box::new(unit_name.clone()));
            }
            if let Some(st) = status {
                sql.push_str(" AND s.status = ?");
                param_values.push(Box::new(st.clone()));
            }
            sql.push_str(" ORDER BY u.name, s.version DESC");

            let params: Vec<&dyn rusqlite::types::ToSql> =
                param_values.iter().map(|p| p.as_ref()).collect();
            let mut stmt = conn.prepare(&sql)?;
            let rows: Vec<SnapshotRow> = stmt
                .query_map(params.as_slice(), |row| {
                    let size: Option<i64> = row.get(5)?;
                    Ok(SnapshotRow {
                        id: row.get(0)?,
                        unit: row.get(1)?,
                        version: row.get(2)?,
                        status: row.get(3)?,
                        files: row.get::<_, Option<i64>>(4)?,
                        size,
                        created: row.get(6)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                crate::render::print_json(&snapshot_rows_to_json(&rows));
            } else if rows.is_empty() {
                println!("no snapshots found");
            } else {
                println!("{}", snapshot_table(&rows, crate::render::terminal_width()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A catalog with units `unit1` and `unit2`, each a directory of one
    /// file under `root`.
    fn two_units(root: &std::path::Path) -> Connection {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        for u in ["unit1", "unit2"] {
            let dir = root.join(u);
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("f.txt"), u).unwrap();
            conn.execute(
                "INSERT INTO units (uuid, name, tenant_id, current_path, checksum_mode, encrypt,
                                    status)
                 VALUES (?1, ?1, ?2, ?3, 'mtime_size', 1, 'active')",
                params![u, tid, dir.to_str().unwrap()],
            )
            .unwrap();
        }
        conn
    }

    fn versions(conn: &Connection, unit: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM snapshots s JOIN units u ON u.id = s.unit_id
             WHERE u.name = ?1",
            params![unit],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// #542: `snapshot create` names what the patterns and the socket rule
    /// left out, in its line and in `--json`'s new `left_out` key; an
    /// unchanged unit reports what its newest version left out.
    #[test]
    fn create_names_what_was_left_out_in_its_line_and_its_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        let conn = two_units(tmp.path());
        let dir = tmp.path().join("unit1");
        std::fs::write(dir.join("a.nfo"), b"12345").unwrap();
        std::fs::create_dir_all(dir.join(".cache")).unwrap();
        std::fs::write(dir.join(".cache/x"), b"123").unwrap();
        let _l = std::os::unix::net::UnixListener::bind(dir.join("s.sock")).unwrap();
        let mut config = Config::default();
        config.defaults.global_excludes = vec!["*.nfo".into(), ".cache/".into()];

        for pass in ["minted", "unchanged"] {
            let created = create_one(&conn, &config, "unit1").unwrap();
            let left = created.json["left_out"].as_array().unwrap_or_else(|| {
                panic!("{pass}: no left_out key in {}", created.json);
            });
            let mut seen: Vec<(String, String, String, i64, i64)> = left
                .iter()
                .map(|r| {
                    (
                        r["reason"].as_str().unwrap().to_string(),
                        r["path"].as_str().unwrap().to_string(),
                        r["kind"].as_str().unwrap().to_string(),
                        r["files"].as_i64().unwrap(),
                        r["bytes"].as_i64().unwrap(),
                    )
                })
                .collect();
            seen.sort();
            assert_eq!(
                seen,
                vec![
                    ("pattern".into(), ".cache".into(), "folder".into(), 1, 3),
                    ("pattern".into(), "a.nfo".into(), "file".into(), 1, 5),
                    ("socket".into(), "s.sock".into(), "file".into(), 1, 0),
                ],
                "{pass}"
            );
            for needle in ["a.nfo", ".cache", "s.sock", "*.nfo", "socket"] {
                assert!(
                    created.line.contains(needle),
                    "{pass}: {needle} in {}",
                    created.line
                );
            }
        }
    }

    /// Issue #404 with ADR-0012 item 210: a signal stops a `snapshot create`
    /// of several units between units. The unit being walked finishes, the
    /// next is not started, and the stop names it and the command to run.
    #[test]
    fn a_signal_stops_several_units_between_units() {
        let tmp = tempfile::TempDir::new().unwrap();
        let conn = two_units(tmp.path());
        let paths = TapectlPaths::new(tmp.path().join("home"));
        let create = SnapshotCommands::Create {
            name: vec!["unit1".into(), "unit2".into()],
            all_pending: false,
        };
        crate::signal::interrupt_this_thread(true);
        let err = run(
            &conn,
            &paths,
            &Config::default(),
            &create,
            true,
            false,
            false,
        );
        crate::signal::interrupt_this_thread(false);
        let err = err.unwrap_err();
        assert!(matches!(err, TapectlError::Interrupted(_)), "{err}");
        let text = err.to_string();
        assert!(
            text.contains("before unit2") && text.contains("`tapectl snapshot create unit2`"),
            "{text}"
        );
        assert_eq!((versions(&conn, "unit1"), versions(&conn, "unit2")), (1, 0));

        // Positive control: with no signal both are snapshotted.
        run(
            &conn,
            &paths,
            &Config::default(),
            &create,
            true,
            false,
            false,
        )
        .unwrap();
        assert_eq!(versions(&conn, "unit2"), 1);
    }

    /// Issue #171 / ADR-0012: `snapshot list --status` must be a usage
    /// error naming the accepted set for anything outside
    /// `snapshots.status`'s CHECK constraint.
    #[test]
    fn validate_snapshot_status_rejects_a_typo_naming_accepted_values() {
        let err = validate_snapshot_status("curent").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("curent"), "{msg}");
        assert!(msg.contains("current"), "{msg}");
        assert!(msg.contains("reclaimable"), "{msg}");
    }

    /// Issue #362: the `--status` closed set is the live CHECK, not a copy
    /// of a migration that a later one superseded (`superseded`/`failed`
    /// sat here long after anything could have set them).
    #[test]
    fn snapshot_statuses_equal_the_live_check() {
        let mut ours: Vec<String> = SNAPSHOT_STATUSES.iter().map(|s| s.to_string()).collect();
        ours.sort();
        assert_eq!(ours, crate::db::live_status_check("snapshots"));
    }

    #[test]
    fn validate_snapshot_status_accepts_every_real_status() {
        for s in SNAPSHOT_STATUSES {
            assert!(
                validate_snapshot_status(s).is_ok(),
                "{s} should be accepted"
            );
        }
    }

    /// `snapshot list --json` shape (issue: C2 row-listing drift).
    /// `files`/`size`/`created` are additive since CTO decision 2026-09-11
    /// (architecture review C2 follow-up, C2b). The byte count
    /// (125_829_121) is deliberately not an even multiple of 1 MiB, proving
    /// the JSON carries the raw fact rather than a value recomputed from
    /// the table's "120 MiB" text.
    #[test]
    fn pin_snapshot_rows_json_shape() {
        let rows = vec![
            SnapshotRow {
                id: 10,
                unit: "backups".to_string(),
                version: 3,
                status: "current".to_string(),
                files: Some(42),
                size: Some(125_829_121),
                // As SQLite's datetime('now') stores it; the JSON says it in
                // RFC 3339 with `Z` (ADR-0012 item 212).
                created: "2026-07-01 00:00:00".to_string(),
            },
            SnapshotRow {
                id: 11,
                unit: "photos".to_string(),
                version: 1,
                status: "reclaimable".to_string(),
                files: None,
                size: None,
                created: String::new(),
            },
        ];
        let value = snapshot_rows_to_json(&rows);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"[{"created_at":"2026-07-01T00:00:00Z","file_count":42,"id":10,"status":"current","total_size":125829121,"unit":"backups","version":3},{"created_at":"","file_count":null,"id":11,"status":"reclaimable","total_size":null,"unit":"photos","version":1}]"#
        );

        // The table (ADR-0012 item 212): borderless, one header case, the
        // size human and binary, the time in UTC.
        let table = snapshot_table(&rows, None);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>(),
            ["Id", "Unit", "Version", "Status", "Files", "Size", "Created"],
            "one header case: {table}"
        );
        assert!(
            lines[1].contains("120.0 MiB") && lines[1].contains("2026-07-01 00:00 UTC"),
            "{table}"
        );
        assert!(!table.contains('|') && !table.contains('+'), "{table}");
    }

    /// #539 (B11): a unit whose dotfile the scan refuses was skipped with a
    /// stderr warning, and with nothing else pending the run printed
    /// "nothing pending" and exited 0. Now it finishes the units it could scan
    /// and ends in an error naming the unit (non-zero), whether or not
    /// anything else was pending; under `--json` the error carries what was
    /// snapshotted.
    #[test]
    fn all_pending_ends_in_an_error_naming_a_unit_it_could_not_scan() {
        let root = tempfile::tempdir().unwrap();
        let conn = two_units(root.path());
        std::fs::write(
            root.path().join("unit2").join(".tapectl-unit.toml"),
            "[excludes]\npattern = [\"*.env\"]\n",
        )
        .unwrap();
        let config = Config::default();

        // unit1 is never archived: snapshotted; unit2 is skipped; the run errs.
        let err = create_all_pending(&conn, &config, false).expect_err("a skip is not success");
        assert!(err.to_string().contains("unit2"), "{err}");
        assert_eq!(versions(&conn, "unit1"), 1, "the scannable unit was done");
        assert_eq!(versions(&conn, "unit2"), 0);

        // Nothing else pending: the same error, never the all-clear.
        conn.execute("UPDATE snapshots SET status = 'current'", [])
            .unwrap();
        let err = create_all_pending(&conn, &config, true).expect_err("still not success");
        assert!(err.to_string().contains("unit2"), "{err}");
        assert!(
            matches!(err, TapectlError::WithJson { .. }),
            "--json carries the document: {err:?}"
        );
    }
}
