use clap::Subcommand;
use rusqlite::{params, Connection};
use serde::Serialize;
use tabled::{Table, Tabled};

use crate::config::{Config, TapectlPaths};
use crate::error::{Result, TapectlError};
use crate::staging;

#[derive(Subcommand, Debug)]
pub enum StageCommands {
    /// Create staged slices (validate → dar → encrypt → checksums)
    ///
    /// Several units may be named; `--jobs` stages that many at once.
    Create {
        /// Unit name(s). Each unit's latest unstaged snapshot is staged
        /// (or `--version`'s, with one unit). Every name is checked before
        /// any unit is staged.
        #[arg(required = true, value_name = "NAME")]
        name: Vec<String>,

        /// Re-stage a specific snapshot version instead of the latest
        /// unstaged one — for when `tapectl staging clean --unit <name>
        /// --version <N>` already released that stage set's slices and
        /// another copy is wanted. Refuses if a stage set for
        /// that version already has live slices; use `volume write` to
        /// consume them, or `staging clean --unit <name> --version <N>`
        /// to release them first. A plain clean releases it only when it
        /// already has a completed write AND the unit is not currently
        /// below its policy's min_copies; add `--force` otherwise —
        /// including for a stage set that was never written anywhere,
        /// which a plain clean leaves untouched regardless of min_copies.
        #[arg(long)]
        version: Option<i64>,

        /// How many units to stage at once (1 to 16). Defaults to
        /// `[staging] jobs`, itself 1 by default. The largest units start
        /// first, each stage prints a line as it starts and ends, and once one
        /// fails no further unit is started. Fewer run when the host's
        /// available memory cannot hold that many (each keeps up to 128 MiB
        /// of its source in the page cache).
        #[arg(long)]
        jobs: Option<usize>,
    },

    /// Stage what a location needs, packed into one volume bound there
    ///
    /// Works out each unit's need at LOCATION (ADR-0012 items 151-159): group
    /// 1, its copy target there is unmet; group 2, it is short of its copy
    /// floor with no target there and no copy there yet. A new or changed
    /// unit counts as a new version with no copies. The units are packed into
    /// `--volume`'s room in priority order (the largest shortfall at LOCATION
    /// first, the fewest copies in all breaking ties, then the name), whole
    /// units before units that may span, next-fit: the first that does not
    /// fit closes the tape, and the rest wait for the next tape for LOCATION.
    /// What is already staged for LOCATION is planned first and never staged
    /// again. A span in progress continues only on a tape bound for the
    /// location of its earlier range.
    ///
    /// New and changed units are snapshotted and staged, an unchanged unit's
    /// version is staged from its source, and a tape-only unit's version is
    /// read back with `volume read-slices`: the commands are printed in the
    /// order to run them, and this command moves no tape. Then write the
    /// volume with `volume write LABEL --for-location LOCATION`. `--dry-run`
    /// prints the plan and stages nothing. A warehouse location is refused:
    /// a copy there is a recorded deposit (`volume deposit add`).
    ForLocation {
        /// The location the volume will be kept at (a registered location,
        /// not a warehouse).
        #[arg(value_name = "LOCATION")]
        location: String,
        /// The volume to fill: `volume init` done, nothing written yet.
        #[arg(long, value_name = "LABEL")]
        volume: String,
        /// How many units to stage at once (1 to 16), as `stage create
        /// --jobs`. Defaults to `[staging] jobs`.
        #[arg(long)]
        jobs: Option<usize>,
        /// The drive whose fill ceiling and end-of-tape reserve size the
        /// volume's room (no drive is touched). Defaults to the only
        /// configured drive; required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
    },

    /// List stage sets
    List {
        /// Filter by status (staging, staged, failed, cleaned)
        #[arg(long)]
        status: Option<String>,
    },

    /// Show details for a stage set
    Info {
        /// Unit name
        name: String,
        /// Snapshot version
        #[arg(long)]
        version: i64,
    },
}

#[derive(Tabled, Serialize)]
struct StageRow {
    #[tabled(rename = "ID")]
    id: i64,
    #[tabled(rename = "Unit")]
    unit: String,
    #[tabled(rename = "Ver")]
    version: i64,
    #[tabled(rename = "Status")]
    status: String,
    /// Raw `Option<i64>`, not a pre-rendered string (issue #205's audit).
    /// A slice COUNT is a quantity; `SnapshotRow.files` next door already
    /// carried the correct shape. `display_opt_i64` keeps the table's `-`
    /// for NULL unchanged.
    #[tabled(rename = "Slices", display_with = "display_opt_i64")]
    slices: Option<i64>,
    /// Table-only until CTO decision 2026-09-11 (architecture review C2
    /// follow-up, C2b). Raw bytes; renamed to `total_encrypted_size` to
    /// match `stage info --json`'s existing key for the same
    /// `ss.total_encrypted_size` column (see `StageCommands::Info` below).
    #[tabled(rename = "Encrypted", display_with = "display_encrypted_mb")]
    #[serde(rename = "total_encrypted_size")]
    encrypted_size: Option<i64>,
    /// Table-only until CTO decision 2026-09-11 (architecture review C2
    /// follow-up, C2b).
    #[tabled(rename = "Staged At", display_with = "display_opt_string")]
    staged_at: Option<String>,
}

fn display_encrypted_mb(v: &Option<i64>) -> String {
    v.map(crate::util::format_bytes_binary).unwrap_or_default()
}

fn display_opt_i64(v: &Option<i64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "-".to_string())
}

fn display_opt_string(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

/// `stage list --json` shape. `encrypted_size`/`staged_at` were table-only
/// until CTO decision 2026-09-11 (architecture review C2 follow-up, C2b).
fn stage_rows_to_json(rows: &[StageRow]) -> serde_json::Value {
    serde_json::to_value(rows).unwrap()
}

/// `stage_sets.status`'s CHECK constraint (`src/db/migrations/001_initial.sql`).
const STAGE_SET_STATUSES: &[&str] = &["staging", "staged", "failed", "cleaned"];

/// `stage list --status` is a usage error when it names anything other than
/// one of `STAGE_SET_STATUSES` (issue #171, ADR-0012).
fn validate_stage_status(value: &str) -> Result<()> {
    crate::config::validate_closed_set("--status", value, STAGE_SET_STATUSES)
        .map_err(TapectlError::Other)
}

pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    command: &StageCommands,
    json_output: bool,
    dry_run: bool,
    assume_yes: bool,
) -> Result<()> {
    match command {
        StageCommands::List { status } => {
            if let Some(st) = status {
                validate_stage_status(st)?;
            }
            let mut sql = String::from(
                "SELECT ss.id, u.name, s.version, ss.status, ss.num_slices,
                        ss.total_encrypted_size, ss.staged_at
                 FROM stage_sets ss
                 JOIN snapshots s ON s.id = ss.snapshot_id
                 JOIN units u ON u.id = s.unit_id
                 WHERE 1=1",
            );
            let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            if let Some(st) = status {
                sql.push_str(" AND ss.status = ?");
                param_values.push(Box::new(st.clone()));
            }
            sql.push_str(" ORDER BY ss.created_at DESC");

            let params_ref: Vec<&dyn rusqlite::types::ToSql> =
                param_values.iter().map(|p| p.as_ref()).collect();
            let mut stmt = conn.prepare(&sql)?;
            let rows: Vec<StageRow> = stmt
                .query_map(params_ref.as_slice(), |row| {
                    let enc_size: Option<i64> = row.get(5)?;
                    Ok(StageRow {
                        id: row.get(0)?,
                        unit: row.get(1)?,
                        version: row.get(2)?,
                        status: row.get(3)?,
                        slices: row.get::<_, Option<i64>>(4)?,
                        encrypted_size: enc_size,
                        staged_at: row.get::<_, Option<String>>(6)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&stage_rows_to_json(&rows)).unwrap()
                );
            } else if rows.is_empty() {
                println!("no stage sets found");
            } else {
                println!("{}", Table::new(rows));
            }
        }

        StageCommands::Info { name, version } => {
            let unit = crate::db::queries::get_unit_by_name(conn, name)?
                .ok_or_else(|| TapectlError::UnitNotFound(name.clone()))?;

            type Row = (
                i64,
                String,
                Option<String>,
                Option<String>,
                Option<i64>,
                Option<i64>,
                Option<i64>,
                Option<String>,
            );
            let (ss_id, status, dar_ver, dar_cmd, num_slices, dar_size, enc_size, staged_at): Row =
                conn.query_row(
                    "SELECT ss.id, ss.status, ss.dar_version, ss.dar_command,
                            ss.num_slices, ss.total_dar_size, ss.total_encrypted_size, ss.staged_at
                     FROM stage_sets ss
                     JOIN snapshots s ON s.id = ss.snapshot_id
                     WHERE s.unit_id = ?1 AND s.version = ?2
                     ORDER BY ss.created_at DESC, ss.id DESC LIMIT 1",
                    params![unit.id, version],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                        ))
                    },
                )
                .map_err(|_| {
                    TapectlError::Other(format!("no stage set for \"{name}\" v{version}"))
                })?;

            // Once issue #53 lets a snapshot carry several stage sets (a
            // re-stage after `staging clean`), the query above still shows
            // only the newest — this count lets both output modes say so
            // without hiding the others' existence.
            let stage_set_count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM stage_sets ss
                 JOIN snapshots s ON s.id = ss.snapshot_id
                 WHERE s.unit_id = ?1 AND s.version = ?2",
                params![unit.id, version],
                |row| row.get(0),
            )?;

            // Get slices
            let mut stmt = conn.prepare(
                "SELECT slice_number, size_bytes, encrypted_bytes, sha256_encrypted
                 FROM stage_slices WHERE stage_set_id = ?1 ORDER BY slice_number",
            )?;
            let slices: Vec<(i64, i64, Option<i64>, Option<String>)> = stmt
                .query_map(params![ss_id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                let slice_json: Vec<serde_json::Value> = slices
                    .iter()
                    .map(|(num, size, enc, sha)| {
                        serde_json::json!({
                            "slice": num, "dar_bytes": size,
                            "encrypted_bytes": enc, "sha256": sha,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({
                        "unit": name, "version": version, "stage_set_id": ss_id,
                        "status": status, "dar_version": dar_ver,
                        "num_slices": num_slices, "total_dar_size": dar_size,
                        "total_encrypted_size": enc_size, "slices": slice_json,
                        "stage_set_count": stage_set_count,
                    })
                );
            } else {
                println!("Stage set for {name} v{version} (id={ss_id})");
                if stage_set_count > 1 {
                    println!(
                        "  ({stage_set_count} stage sets exist for this version; showing the newest)"
                    );
                }
                println!("  Status:    {status}");
                if let Some(dv) = &dar_ver {
                    println!("  dar:       {dv}");
                }
                if let Some(dc) = &dar_cmd {
                    println!("  command:   {dc}");
                }
                println!("  Slices:    {}", num_slices.unwrap_or(0));
                println!(
                    "  dar size:  {}",
                    crate::util::format_bytes_binary(dar_size.unwrap_or(0))
                );
                println!(
                    "  encrypted: {}",
                    crate::util::format_bytes_binary(enc_size.unwrap_or(0))
                );
                if let Some(sa) = &staged_at {
                    println!("  Staged at: {sa}");
                }
                if !slices.is_empty() {
                    println!("  Slices:");
                    for (num, size, enc, sha) in &slices {
                        println!(
                            "    #{num}: {} dar, {} enc, sha256={}",
                            crate::util::format_bytes_binary(*size),
                            crate::util::format_bytes_binary(enc.unwrap_or(0)),
                            sha.as_deref().unwrap_or("(none)"),
                        );
                    }
                }
            }
        }

        StageCommands::ForLocation {
            location,
            volume,
            jobs,
            device,
        } => {
            // #493 (ADR-0012 item 151): `--dry-run` is honoured — the plan
            // is made from the catalog and the sources alone, and nothing is
            // snapshotted or staged.
            let jobs = jobs.unwrap_or(config.staging.jobs);
            if !(1..=staging::jobs::MAX_JOBS).contains(&jobs) {
                return Err(TapectlError::Other(format!(
                    "--jobs {jobs}: stage between 1 and {} units at once",
                    staging::jobs::MAX_JOBS
                )));
            }
            let device = crate::cli::write_device(config, device.as_deref())?;
            let plan = crate::for_location::plan(conn, config, location, volume, &device)?;
            let staged = if dry_run {
                None
            } else {
                crate::progress::stderr_println(&format!(
                    "stage for-location {} --volume {}: {} unit(s) planned; staging what is \
                     not staged yet",
                    plan.location,
                    plan.volume,
                    plan.planned.len()
                ));
                Some(crate::for_location::stage(
                    conn, paths, config, &plan, jobs, assume_yes,
                )?)
            };
            if json_output {
                println!("{}", crate::for_location::to_json(&plan, staged.as_deref()));
            } else {
                let (out, err) = crate::for_location::text_lines(&plan, staged.as_deref());
                for line in out {
                    println!("{line}");
                }
                for line in err {
                    eprintln!("{line}");
                }
            }
        }

        StageCommands::Create {
            name,
            version,
            jobs,
        } => {
            // Issue #241: the dar archive + age encryption pipeline IS
            // the work (hours and a tape's worth of staging disk, per
            // `collection run`'s own dry-run comment) — most of the
            // command with none of the safety if run "dry".
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "stage create",
                    "the dar archive, sha256 validation and age encryption pipeline IS the \
                     work — there is no cheaper way to know what would be staged.",
                ));
            }
            if name.len() > 1 && version.is_some() {
                return Err(TapectlError::Other(
                    "--version names one snapshot, so it takes one unit; stage the others \
                     with their own `tapectl stage create`"
                        .into(),
                ));
            }
            let jobs = jobs.unwrap_or(config.staging.jobs);
            if !(1..=staging::jobs::MAX_JOBS).contains(&jobs) {
                return Err(TapectlError::Other(format!(
                    "--jobs {jobs}: stage between 1 and {} units at once",
                    staging::jobs::MAX_JOBS
                )));
            }
            // Every name is resolved before any unit is staged, so a typo or
            // a unit with nothing to stage costs nothing (issue #368).
            let mut to_stage: Vec<staging::jobs::StageJob> = Vec::with_capacity(name.len());
            for n in name {
                if to_stage.iter().any(|j| &j.unit_name == n) {
                    return Err(TapectlError::Other(format!(
                        "unit \"{n}\" is named twice; a unit is staged once"
                    )));
                }
                to_stage.push(staging::jobs::StageJob {
                    unit_name: n.clone(),
                    snapshot_id: snapshot_to_stage(conn, config, n, *version)?,
                    admission: if version.is_some() {
                        staging::Admission::Restage
                    } else {
                        staging::Admission::Unstaged
                    },
                    view: None,
                });
            }
            let outcomes = staging::jobs::stage_many(
                conn,
                paths,
                config,
                &to_stage,
                jobs,
                assume_yes,
                &mut std::io::stderr(),
                &crate::progress::stderr_println,
            )?;
            // On any failure nothing goes to stdout — no `[]` a script would
            // read as success — and the error, on stderr, names every unit
            // that was staged, failed or never started (`first_failure`).
            let done = staging::jobs::first_failure(outcomes)?;
            let mut rows = Vec::with_capacity(done.len());
            for (unit_name, stage_set_id) in &done {
                let (num_slices, total_dar, total_enc): (Option<i64>, Option<i64>, Option<i64>) =
                    conn.query_row(
                        "SELECT num_slices, total_dar_size, total_encrypted_size
                         FROM stage_sets WHERE id = ?1",
                        params![stage_set_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )?;
                rows.push(serde_json::json!({
                    "stage_set_id": stage_set_id,
                    "unit": unit_name,
                    "num_slices": num_slices,
                    "total_dar_size": total_dar,
                    "total_encrypted_size": total_enc,
                }));
                if !json_output {
                    println!(
                        "staged: {} ({} slices, {} dar, {} encrypted)",
                        unit_name,
                        num_slices.unwrap_or(0),
                        crate::util::format_bytes_binary(total_dar.unwrap_or(0)),
                        crate::util::format_bytes_binary(total_enc.unwrap_or(0)),
                    );
                }
            }
            if json_output {
                // One unit: the object it always was. Several: an array.
                let out = if name.len() == 1 && rows.len() == 1 {
                    rows.remove(0)
                } else {
                    serde_json::Value::Array(rows)
                };
                println!("{out}");
            }
        }
    }
    Ok(())
}

/// The status of the live `writes` row behind a
/// [`staging::clean::ReleaseBlocker::WriteInFlight`] refusal: `in_progress`
/// when any of `stage_set_ids`' rows on that volume is, else `planned`
/// (`release_blocker` ranks them the same way). `label` is the volume the
/// blocker named; with none, every live row of the sets counts. `None` only
/// if the row vanished between the two reads.
fn write_in_flight_status(
    conn: &Connection,
    stage_set_ids: &[i64],
    label: Option<&str>,
) -> Result<Option<String>> {
    let mut found: Vec<String> = Vec::new();
    for id in stage_set_ids {
        let mut stmt = conn.prepare(
            "SELECT w.status FROM writes w JOIN volumes v ON v.id = w.volume_id
             WHERE w.stage_set_id = ?1 AND w.status IN ('in_progress', 'planned')
               AND (?2 IS NULL OR v.label = ?2)",
        )?;
        let rows = stmt
            .query_map(params![id, label], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        found.extend(rows);
    }
    Ok(["in_progress", "planned"]
        .into_iter()
        .find(|s| found.iter().any(|f| f == s))
        .map(str::to_string))
}

/// What to do when a write session `staging clean` / `stage create` found
/// "running" is in fact not (#468). `tapectl db fsck` checks integrity and
/// foreign keys; it sweeps no session. The two live states differ:
///
/// - `in_progress`: a dead process's row is marked `interrupted` by the next
///   `db::open` (`recover_orphaned_sessions`, lock-aware), so one still
///   `in_progress` here is held by a live process. Once swept, `volume
///   resume` continues the session from these staged files and `volume
///   abort` abandons it.
/// - `planned`: the process was killed after planning, before it began to
///   write. Nothing sweeps that row and nothing reached the tape, so it is
///   cleared with `volume abort` (`write::nothing_to_resume` says the same).
fn write_in_flight_remedy(label: Option<&str>, status: Option<String>) -> String {
    let label = label.unwrap_or("<LABEL>");
    match status.as_deref() {
        Some("planned") => format!(
            "If no write is actually running, the session was killed after it was planned \
             and before it began to write: nothing reached the tape and the catalog still \
             records it as `planned`, which nothing sweeps. Clear it with `tapectl volume \
             abort {label}`, then write again."
        ),
        _ => format!(
            "If no write is actually running, the session died without recording an \
             outcome: the next tapectl command to open the catalog marks it `interrupted`, \
             and then `tapectl volume resume {label}` continues it from these staged files \
             or `tapectl volume abort {label}` abandons it."
        ),
    }
}

/// The snapshot `stage create NAME [--version V]` stages: the unit's latest
/// unstaged snapshot, or version `V` when no stage set of it has live slices
/// (issue #53's re-stage, with the refusals that name what to do instead).
fn snapshot_to_stage(
    conn: &Connection,
    config: &Config,
    name: &String,
    version: Option<i64>,
) -> Result<i64> {
    let unit = crate::db::queries::get_unit_by_name(conn, name)?
        .ok_or_else(|| TapectlError::UnitNotFound(name.clone()))?;

    let snapshot_id: i64 = match version {
        None => {
            // Unchanged: the latest 'created' (never-yet-staged)
            // snapshot for this unit.
            conn.query_row(
                "SELECT id FROM snapshots WHERE unit_id = ?1 AND status = 'created'
                 ORDER BY version DESC LIMIT 1",
                params![unit.id],
                |row| row.get(0),
            )
            .map_err(|_| {
                TapectlError::Other(format!(
                    "no unstaged snapshot for unit \"{name}\" — run `tapectl snapshot create {name}` first if the contents changed, or re-stage an existing version with `tapectl stage create {name} --version <N>` (`tapectl snapshot list --unit {name}` shows them)"
                ))
            })?
        }
        Some(v) => {
            // Re-stage: select that unit's snapshot at version `v`
            // regardless of its status, then gate on whether any
            // existing stage set for it already has live slices.
            let snapshot_id: i64 = conn
                .query_row(
                    "SELECT id FROM snapshots WHERE unit_id = ?1 AND version = ?2",
                    params![unit.id, v],
                    |row| row.get(0),
                )
                .map_err(|_| {
                    TapectlError::Other(format!("unit \"{name}\" has no snapshot at version {v}"))
                })?;

            let stage_sets: Vec<(i64, String)> = conn
                .prepare("SELECT id, status FROM stage_sets WHERE snapshot_id = ?1")?
                .query_map(params![snapshot_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            let live_ids: Vec<i64> = stage_sets
                .iter()
                .filter(|(_, status)| staging::stage_set_has_live_slices(status))
                .map(|(id, _)| *id)
                .collect();

            if !live_ids.is_empty() {
                // Issues #279 and #292: a bare `staging clean`
                // leaves a live stage set alone for one of FOUR
                // reasons, and the refusal must name the one that
                // actually applies. #279 split this two ways and
                // recommended `--force` for everything that was
                // not a release candidate; `staging::clean::
                // release_blocker` answers the finer question,
                // because for an interrupted session `--force`
                // deletes the very slices `volume resume` needs.
                //
                // Worst blocker across this version's live sets
                // wins, for the same reason `release_blocker`
                // orders its own variants: a set may be bin-packed
                // across volumes, and the costliest thing to be
                // wrong about is telling someone to delete slices
                // something is still using.
                //
                // #478 (ADR-0012 item 129): `release_blocker_judged` adds
                // the policy `release_blocker` leaves out, the slices of a
                // set holding a range still short of copies.
                let mut worst = staging::clean::ReleaseBlocker::None;
                for id in &live_ids {
                    let b = staging::release::release_blocker_judged(conn, config, *id)?;
                    let rank = |x: &staging::clean::ReleaseBlocker| match x {
                        staging::clean::ReleaseBlocker::WriteInFlight { .. } => 6,
                        staging::clean::ReleaseBlocker::Interrupted { .. } => 5,
                        // #478: the short slices of a span in progress are
                        // the only source its next tape can continue from:
                        // staging the version again makes a different stage
                        // set, whose ciphertext cannot continue the ranges
                        // already sealed. So `--force` costs more here than
                        // for an abandoned session (a re-confirmation, and
                        // its remedy text recommends --force) or a cut set
                        // (written nowhere, nothing on tape to orphan), and
                        // it outranks both; a live or resumable session
                        // still outranks it, being in use now or next.
                        staging::clean::ReleaseBlocker::SlicesShort { .. } => 4,
                        staging::clean::ReleaseBlocker::Abandoned { .. } => 3,
                        // #420: never written whole, as `NeverWritten`, but
                        // it names the volume that cut it.
                        staging::clean::ReleaseBlocker::Cut { .. } => 2,
                        staging::clean::ReleaseBlocker::NeverWritten => 1,
                        staging::clean::ReleaseBlocker::None => 0,
                    };
                    if rank(&b) > rank(&worst) {
                        worst = b;
                    }
                }

                let head = format!("unit \"{name}\" v{v} already has a stage set with live slices");
                let msg = match &worst {
                    // A candidate: a bare clean WOULD release it,
                    // unless the unit is below its policy's
                    // min_copies. Today's wording, unchanged.
                    staging::clean::ReleaseBlocker::None => format!(
                        "{head} — use `tapectl volume write` to consume them, or \
                         `tapectl staging clean --unit {name}` to release them first \
                         (retained by default if \"{name}\" is below its policy's \
                         min_copies — add --force to release it anyway)"
                    ),
                    // Never written: #279's wording, unchanged.
                    staging::clean::ReleaseBlocker::NeverWritten => format!(
                        "{head} — use `tapectl volume write` to consume them; it has no \
                         completed write backing it, so `tapectl staging clean \
                         --unit {name} --version {v}` would leave it untouched — \
                         pass --force to release it"
                    ),
                    // #420 (ADR-0012 item 112): a short seal cut it. The
                    // slices are the input to the write that puts it on
                    // tape whole, which is an ordinary `volume write` onto
                    // another cartridge.
                    staging::clean::ReleaseBlocker::Cut { volume_label } => {
                        let on = match volume_label {
                            Some(l) => format!("volume \"{l}\""),
                            None => "its volume".to_string(),
                        };
                        format!(
                            "{head}, and it is written whole nowhere: {on} sealed short before \
                             it, so it was cut there and is not a copy. Write it whole onto \
                             another cartridge with `tapectl volume write <LABEL>` (`--init` \
                             initialises a new one); these slices are what that write \
                             reads. `tapectl staging clean --unit {name} --version {v}` \
                             leaves it untouched until then — pass --force only to give the \
                             version up"
                        )
                    }
                    // #478 (ADR-0012 items 85 and 129): a span in
                    // progress. Its released slices are on tape; the short
                    // ones continue it, and are released one by one as
                    // their copies land.
                    staging::clean::ReleaseBlocker::SlicesShort { slices } => format!(
                        "{head}, and it spans cartridges: slice(s) {} are not yet on as many \
                         tapes as the unit's policy requires, so they stay staged and are \
                         released one by one as each gets its copies. They are what continues \
                         the span: an ordinary `tapectl volume write <LABEL>` onto another \
                         cartridge of the same generation writes them (`--init` initialises a \
                         new one). Do not give them up with --force: staging the version again \
                         makes a different stage set, which cannot continue the ranges already \
                         sealed on tape. `tapectl staging clean --unit {name} --version {v}` \
                         releases each slice once it has its copies",
                        staging::clean::describe_slice_numbers(slices)
                    ),
                    // #292: `--force` here destroys the recovery.
                    // Name `volume resume` first, as the Tier-3
                    // refusal does for a sealed-but-unconfirmed
                    // volume (`operations::refuse_last_eligible_copy`).
                    staging::clean::ReleaseBlocker::Interrupted { volume_label } => {
                        let resume = match volume_label {
                            Some(l) => format!("`tapectl volume resume {l}`"),
                            None => "`tapectl volume resume <LABEL>`".to_string(),
                        };
                        format!(
                            "{head}, and they are the input to an INTERRUPTED write \
                             session — {resume} continues that session from these \
                             exact staged files rather than rebuilding them, so \
                             releasing them is what would make it unrecoverable. \
                             Reload the same cartridge and resume it, or write these \
                             slices to another volume. Only give them up on purpose \
                             (`tapectl volume abort`, then `staging clean --force`) \
                             once you have decided the interrupted write is not \
                             worth finishing."
                        )
                    }
                    // #292: something is reading them right now.
                    staging::clean::ReleaseBlocker::WriteInFlight { volume_label } => {
                        let onto = match volume_label {
                            Some(l) => format!(" onto volume \"{l}\""),
                            None => String::new(),
                        };
                        // #468: which command clears a session that is not
                        // really running depends on the state its row is in.
                        let status =
                            write_in_flight_status(conn, &live_ids, volume_label.as_deref())?;
                        let remedy = write_in_flight_remedy(volume_label.as_deref(), status);
                        format!(
                            "{head}, and a write session is using them RIGHT NOW{onto}. \
                             Let it finish — `tapectl volume write` is consuming these \
                             slices, and releasing them under a running session is not \
                             something --force should be pointed at. {remedy}"
                        )
                    }
                    // Issue #325: an aborted session whose seal is
                    // recorded can still be re-confirmed by `volume
                    // resume` after a clean full verify, and that
                    // re-confirm needs these frozen files — the same
                    // facts `volume abort`'s sealed-session text
                    // states (`write::abort_consent_facts`). Resume
                    // first, then what --force costs.
                    staging::clean::ReleaseBlocker::Abandoned {
                        reconfirm_on: Some(l),
                    } => format!(
                        "{head}, left behind by a write session on volume \"{l}\" that \
                         was aborted after its seal was recorded. Keep staging if you \
                         intend to verify and resume: after a clean full verify \
                         (`tapectl volume verify {l}`), `tapectl volume resume {l}` \
                         re-confirms that sealed tape against these frozen staged \
                         files, and once it passes that session no longer holds them. \
                         Releasing them now (`tapectl staging clean --unit {name} \
                         --version {v} --force`; a bare `tapectl staging clean` will \
                         not) forfeits that re-confirmation."
                    ),
                    // Nothing can adopt the session: --force is the
                    // only release. Issue #325: writing the slices
                    // to another volume adds a completed row but
                    // leaves this one, so it does not unblock a bare
                    // clean — never advise it as if it did.
                    staging::clean::ReleaseBlocker::Abandoned { reconfirm_on: None } => {
                        format!(
                            "{head}, left behind by a write session that was aborted \
                             or failed. A bare `tapectl staging clean` will not release \
                             them, and writing them to another volume does not change \
                             that — the abandoned session's `writes` row keeps blocking \
                             it. Release them with `tapectl staging clean --unit {name} \
                             --version {v} --force`."
                        )
                    }
                };
                return Err(TapectlError::Other(msg));
            }

            snapshot_id
        }
    };
    Ok(snapshot_id)
}

#[cfg(test)]
mod tests {
    //! Tests for issue #53's `stage create --version` re-staging flow.
    //!
    //! Shared setup builds a real tenant + unit (via `unit::init_unit`, so
    //! a real dotfile + real tenant keys exist — `stage_create` refuses to
    //! encrypt without active tenant keys) and runs the real `dar` binary,
    //! matching the pattern already established in
    //! `staging::tests::stage_create_uses_archive_set_resolved_slice_size_not_global_default`.

    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Issue #171 / ADR-0012: `stage list --status` must be a usage error
    /// naming the accepted set for anything outside `stage_sets.status`'s
    /// CHECK constraint.
    #[test]
    fn validate_stage_status_rejects_a_typo_naming_accepted_values() {
        let err = validate_stage_status("stagng").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("stagng"), "{msg}");
        assert!(msg.contains("staging"), "{msg}");
        assert!(msg.contains("cleaned"), "{msg}");
    }

    #[test]
    fn validate_stage_status_accepts_every_real_status() {
        for s in STAGE_SET_STATUSES {
            assert!(validate_stage_status(s).is_ok(), "{s} should be accepted");
        }
    }

    /// `stage list --json` shape (issue: C2 row-listing drift).
    /// `encrypted_size`/`staged_at` are additive since CTO decision
    /// 2026-09-11 (architecture review C2 follow-up, C2b). The byte count
    /// (125_829_121) is deliberately not an even multiple of 1 MiB, proving
    /// the JSON carries the raw fact rather than a value recomputed from
    /// the table's "120 MiB" text.
    #[test]
    fn pin_stage_rows_json_shape() {
        let rows = vec![
            StageRow {
                id: 1,
                unit: "backups".to_string(),
                version: 2,
                status: "staged".to_string(),
                slices: Some(3),
                encrypted_size: Some(125_829_121),
                staged_at: Some("2026-07-01T00:00:00Z".to_string()),
            },
            StageRow {
                id: 2,
                unit: "photos".to_string(),
                version: 1,
                status: "staging".to_string(),
                slices: None,
                encrypted_size: None,
                staged_at: None,
            },
        ];
        let value = stage_rows_to_json(&rows);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"[{"id":1,"slices":3,"staged_at":"2026-07-01T00:00:00Z","status":"staged","total_encrypted_size":125829121,"unit":"backups","version":2},{"id":2,"slices":null,"staged_at":null,"status":"staging","total_encrypted_size":null,"unit":"photos","version":1}]"#
        );
    }

    /// (conn, paths, config) with a tenant "alice" and a unit "unit1"
    /// registered, source content already written. Caller drives
    /// `snapshot create`/`stage create` itself.
    fn setup() -> (Connection, TapectlPaths, Config, TempDir) {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();
        let conn = crate::db::open(&paths.db_file).unwrap();

        let staging_dir = tmp.path().join("staging");
        fs::create_dir_all(&staging_dir).unwrap();
        let mut config = Config::default();
        config.staging.directory = staging_dir.to_string_lossy().to_string();
        config.dar.binary = "dar".to_string();

        crate::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        // Issue #115 / ADR-0005: `stage_create` refuses without a registered
        // escrow recipient. Public key only, exactly as production does.
        {
            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('escrow-holder', 0, 'active')",
                [],
            )
            .unwrap();
            let holder_id = conn.last_insert_rowid();
            let kp = crate::crypto::keys::generate_keypair();
            crate::db::queries::insert_escrow_key(
                &conn,
                holder_id,
                "test-escrow",
                &kp.fingerprint,
                &kp.public_key,
                Some("test escrow recipient (ADR-0005)"),
            )
            .unwrap();
        }

        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("f.txt"), b"restage test content").unwrap();

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

        (conn, paths, config, tmp)
    }

    #[test]
    fn create_without_version_behaves_exactly_as_before() {
        let (conn, paths, config, _tmp) = setup();

        // No snapshot yet at all: same error message as before this change.
        let err = run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: vec!["unit1".to_string()],
                jobs: None,
                version: None,
            },
            false,
            false,
            false,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("no unstaged snapshot for unit"),
            "unexpected error message: {err}"
        );

        // With a 'created' snapshot present, no-version staging still
        // finds and stages it exactly as before.
        let snap_id = crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: vec!["unit1".to_string()],
                jobs: None,
                version: None,
            },
            false,
            false,
            false,
        )
        .unwrap();

        let status: String = conn
            .query_row(
                "SELECT status FROM snapshots WHERE id = ?1",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "staged");
    }

    /// Issue #368: every name is checked before any unit is staged — a
    /// second name with nothing to stage, a name given twice, `--version`
    /// with several units and a `--jobs` out of range all refuse with
    /// nothing staged.
    #[test]
    fn create_with_several_names_checks_them_all_before_staging_any() {
        let (conn, paths, config, _tmp) = setup();
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let create = |name: &[&str], version: Option<i64>, jobs: Option<usize>| {
            run(
                &conn,
                &paths,
                &config,
                &StageCommands::Create {
                    name: name.iter().map(|n| n.to_string()).collect(),
                    version,
                    jobs,
                },
                false,
                false,
                false,
            )
            .unwrap_err()
            .to_string()
        };
        let err = create(&["unit1", "nosuch"], None, None);
        assert!(err.contains("nosuch"), "{err}");
        let err = create(&["unit1", "unit1"], None, None);
        assert!(err.contains("named twice"), "{err}");
        let err = create(&["unit1", "unit1"], Some(1), None);
        assert!(err.contains("--version names one snapshot"), "{err}");
        let err = create(&["unit1"], None, Some(0));
        assert!(err.contains("--jobs 0"), "{err}");
        let staged: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(staged, 0, "nothing was staged");
    }

    #[test]
    fn create_with_version_on_nonexistent_version_errors() {
        let (conn, paths, config, _tmp) = setup();
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        let err = run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: vec!["unit1".to_string()],
                jobs: None,
                version: Some(99),
            },
            false,
            false,
            false,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("no snapshot at version 99"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn create_with_version_refuses_when_a_stage_set_is_staged() {
        let (conn, paths, config, _tmp) = setup();
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: vec!["unit1".to_string()],
                jobs: None,
                version: None,
            },
            false,
            false,
            false,
        )
        .unwrap();

        // The stage set from the first `create` is 'staged' (live slices)
        // — a re-stage of the same version must be refused.
        let err = run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: vec!["unit1".to_string()],
                jobs: None,
                version: Some(1),
            },
            false,
            false,
            false,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("already has a stage set with live slices"),
            "unexpected error message: {msg}"
        );
        assert!(msg.contains("volume write"));
        assert!(msg.contains("staging clean"));
    }

    /// Issue #279, one of the two branches: the live stage set above has
    /// NEVER been written anywhere (no `writes` row exists for it at all)
    /// -- a plain `tapectl staging clean --unit <name>` is a silent no-op
    /// on it (`cleaned 0 stage set(s)`), so `--force` is the only thing
    /// that releases it, regardless of the unit's min_copies standing.
    /// The refusal must say exactly that, and must NOT mention
    /// min_copies at all -- mentioning it here would misdirect an
    /// operator whose unit is not under min_copies into concluding
    /// `--force` does not apply to them, when it is the only thing that
    /// would help. The positive control for this test is
    /// Issue #292: a write row in a NON-`completed` state is not one fact
    /// but four, and issue #279's fix collapsed them. This helper stages
    /// `unit1`, attaches one `writes` row in `status`, and returns the
    /// refusal an operator then sees from `stage create --version 1`.
    fn refusal_with_write_status(status: &str) -> String {
        refusal_with_write_on_volume(status, "initialized", false)
    }

    /// [`refusal_with_write_status`] with the volume's shape chosen (issue
    /// #325): `volume_status` is `volumes.status`, and `sealed` records a
    /// seal (`sealed_at`) plus the `write_aborted` event `volume abort`
    /// writes — the recorded rows `volume resume`'s adoption of an aborted
    /// session reads (ADR-0012, 2026-09-23).
    fn refusal_with_write_on_volume(status: &str, volume_status: &str, sealed: bool) -> String {
        let (conn, paths, config, _tmp) = staged_unit_with_write(status, volume_status, sealed);
        stage_version_1_refusal(&conn, &paths, &config)
    }

    /// `unit1` staged once, with one `writes` row in `status` on volume
    /// `L6-0007` (the setup [`refusal_with_write_on_volume`] describes).
    fn staged_unit_with_write(
        status: &str,
        volume_status: &str,
        sealed: bool,
    ) -> (Connection, TapectlPaths, Config, TempDir) {
        let (conn, paths, config, tmp) = setup();
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: vec!["unit1".to_string()],
                jobs: None,
                version: None,
            },
            false,
            false,
            false,
        )
        .unwrap();
        let stage_set_id: i64 = conn
            .query_row("SELECT id FROM stage_sets LIMIT 1", [], |r| r.get(0))
            .unwrap();
        let snapshot_id: i64 = conn
            .query_row(
                "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
             VALUES ('L6-0007', 'lto', 'lto0', 2500000000000, ?1)",
            params![volume_status],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, ?4)",
            params![stage_set_id, snapshot_id, volume_id, status],
        )
        .unwrap();
        if sealed {
            conn.execute(
                "UPDATE volumes SET sealed_at = datetime('now') WHERE id = ?1",
                params![volume_id],
            )
            .unwrap();
            crate::db::events::log_event(
                &conn,
                "volume",
                volume_id,
                Some("L6-0007"),
                "write_aborted",
                None,
                None,
                Some("operator abandoned the unfinished write session (`volume abort`)"),
                None,
                None,
            )
            .unwrap();
        }
        (conn, paths, config, tmp)
    }

    /// The refusal `stage create unit1 --version 1` gives now.
    fn stage_version_1_refusal(conn: &Connection, paths: &TapectlPaths, config: &Config) -> String {
        run(
            conn,
            paths,
            config,
            &StageCommands::Create {
                name: vec!["unit1".to_string()],
                jobs: None,
                version: Some(1),
            },
            false,
            false,
            false,
        )
        .unwrap_err()
        .to_string()
    }

    /// **The one that matters (issue #292).** `volume resume` continues an
    /// interrupted session from its frozen staging files, so these slices
    /// ARE the recovery. Issue #279's refusal said "pass --force to release
    /// it" here, which destroys it. The refusal must lead with resume and
    /// must not present --force as the first act.
    #[test]
    fn create_with_version_refusal_names_resume_not_force_for_an_interrupted_write() {
        let msg = refusal_with_write_status("interrupted");
        assert!(
            msg.contains("volume resume L6-0007"),
            "must name resume, with the volume to reload: {msg}"
        );
        assert!(
            msg.contains("INTERRUPTED"),
            "must say which state this is: {msg}"
        );
        let force_at = msg.find("--force");
        let resume_at = msg.find("volume resume");
        assert!(
            resume_at < force_at || force_at.is_none(),
            "resume must come BEFORE any mention of --force -- an operator \
             acts on the first command they read: {msg}"
        );
        assert!(
            !msg.contains("min_copies"),
            "min_copies is not why a clean leaves this one alone: {msg}"
        );
    }

    /// Issue #292: a session running RIGHT NOW is reading these slices.
    /// Releasing them under it is not something `--force` should be aimed
    /// at, so the refusal must not offer it as the remedy.
    #[test]
    fn create_with_version_refusal_says_a_live_write_is_using_the_slices() {
        for status in ["in_progress", "planned"] {
            let msg = refusal_with_write_status(status);
            assert!(
                msg.contains("RIGHT NOW"),
                "{status}: must say a session is using them now: {msg}"
            );
            assert!(
                msg.contains("L6-0007"),
                "{status}: must name the volume being written: {msg}"
            );
            assert!(
                !msg.contains("pass --force"),
                "{status}: must not recommend forcing a release under a \
                 running session: {msg}"
            );
        }
    }

    /// #468: the live-write refusal used to end "`tapectl db fsck` sweeps
    /// that", which is false (`db fsck` checks integrity and foreign keys). A
    /// `planned` session is cleared by `volume abort`, and an `in_progress`
    /// one is swept to `interrupted` by the next open of the catalog; each
    /// refusal names the command for ITS state, and the CLI accepts it.
    #[test]
    fn a_live_write_refusal_names_the_command_for_its_state() {
        use crate::cli::operations::remedy_commands::assert_names_parsing_commands;
        let planned = refusal_with_write_status("planned");
        assert!(!planned.contains("db fsck"), "{planned}");
        assert_names_parsing_commands(&planned, &[], &["tapectl volume abort L6-0007"]);
        assert!(
            !planned.contains("volume resume"),
            "nothing reached the tape, so there is nothing to resume: {planned}"
        );

        let running = refusal_with_write_status("in_progress");
        assert!(!running.contains("db fsck"), "{running}");
        assert!(running.contains("marks it `interrupted`"), "{running}");
        assert_names_parsing_commands(
            &running,
            &[],
            &[
                "tapectl volume resume L6-0007",
                "tapectl volume abort L6-0007",
            ],
        );
    }

    /// #468, the named commands in the state they are named for. A `planned`
    /// row from a dead process: `volume abort` clears it, and the refusal
    /// then becomes the abandoned-session one (`--force` is the release).
    #[test]
    fn volume_abort_clears_the_planned_session_the_refusal_names() {
        let (conn, paths, config, _tmp) = staged_unit_with_write("planned", "initialized", false);
        let before = stage_version_1_refusal(&conn, &paths, &config);
        assert!(before.contains("RIGHT NOW"), "{before}");
        crate::volume::write::volume_abort(&conn, "L6-0007", true)
            .expect("the named `volume abort` works on a planned session");
        let after = stage_version_1_refusal(&conn, &paths, &config);
        assert!(!after.contains("RIGHT NOW"), "{after}");
        assert!(after.contains("--force"), "{after}");
    }

    /// #468: an `in_progress` row whose process is gone is marked
    /// `interrupted` the next time the catalog is opened, and the refusal then
    /// names `volume resume`, which is what the first refusal said would
    /// follow; `volume abort` abandons it.
    #[test]
    fn a_dead_in_progress_session_becomes_interrupted_at_the_next_open() {
        let (conn, paths, config, _tmp) =
            staged_unit_with_write("in_progress", "initialized", false);
        let before = stage_version_1_refusal(&conn, &paths, &config);
        assert!(before.contains("RIGHT NOW"), "{before}");
        let reopened = crate::db::open(&paths.db_file).unwrap();
        let status: String = reopened
            .query_row("SELECT status FROM writes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "interrupted", "the next open sweeps it");
        let after = stage_version_1_refusal(&reopened, &paths, &config);
        assert!(after.contains("INTERRUPTED"), "{after}");
        assert!(after.contains("`tapectl volume resume L6-0007`"), "{after}");
        crate::volume::write::volume_abort(&reopened, "L6-0007", true)
            .expect("the named `volume abort` works on the swept session");
    }

    /// Issue #292's POSITIVE CONTROL for `--force`: an aborted or failed
    /// session is exactly where `--force` IS the documented act, and
    /// `volume abort`'s own consent text says so. Without this, the two
    /// tests above cannot distinguish "stopped recommending --force where
    /// it is wrong" from "stopped recommending --force at all".
    #[test]
    fn create_with_version_refusal_still_offers_force_for_an_abandoned_write() {
        for status in ["aborted", "failed"] {
            let msg = refusal_with_write_status(status);
            assert!(
                msg.contains("--force"),
                "{status}: --force is the right act here: {msg}"
            );
            assert!(
                !msg.contains("volume resume"),
                "{status}: resume cannot adopt this session, so naming it \
                 would hand over a command that refuses: {msg}"
            );
        }
    }

    /// Issue #325: "or write them to another volume first" was never a way
    /// to release an abandoned stage set — a second, completed write leaves
    /// the aborted row in place, and it still blocks a bare `staging clean`
    /// (`staging::clean`'s `default_guard_refuses_when_a_second_planned_copy_aborted_or_failed`).
    /// The refusal names the one act that does release them.
    #[test]
    fn create_with_version_refusal_for_an_unsealed_abandoned_write_names_only_force() {
        for status in ["aborted", "failed"] {
            let msg = refusal_with_write_status(status);
            assert!(
                !msg.contains("or write them to another volume first"),
                "{status}: writing elsewhere does not unblock a bare clean: {msg}"
            );
            assert!(
                msg.contains("writing them to another volume does not change that"),
                "{status}: {msg}"
            );
            assert!(
                msg.contains("tapectl staging clean --unit unit1 --version 1 --force"),
                "{status}: must name the release that works: {msg}"
            );
            assert!(
                !msg.contains("forfeits"),
                "{status}: nothing to forfeit: {msg}"
            );
        }
    }

    /// Issue #325: an aborted session whose seal is recorded is one `volume
    /// resume` re-confirms after a clean full verify (ADR-0012, 2026-09-23;
    /// #280), against these very staged files. The refusal must say what
    /// `volume abort`'s sealed-session text says (`write::abort_consent_facts`):
    /// keep staging to verify and resume; `--force` forfeits that. Resume
    /// comes first, as in the interrupted refusal.
    #[test]
    fn create_with_version_refusal_for_a_sealed_aborted_write_says_force_forfeits_reconfirm() {
        let msg = refusal_with_write_on_volume("aborted", "initialized", true);
        assert!(
            msg.contains("Keep staging if you intend to verify and resume"),
            "{msg}"
        );
        assert!(msg.contains("tapectl volume verify L6-0007"), "{msg}");
        assert!(msg.contains("tapectl volume resume L6-0007"), "{msg}");
        assert!(msg.contains("forfeits"), "{msg}");
        assert!(
            msg.contains("tapectl staging clean --unit unit1 --version 1 --force"),
            "the release is still named, with its cost: {msg}"
        );
        assert!(
            !msg.contains("another volume"),
            "no write-elsewhere advice on this path either: {msg}"
        );
        let resume_at = msg.find("volume resume").unwrap();
        let force_at = msg.find("--force").unwrap();
        assert!(
            resume_at < force_at,
            "resume must come before --force: {msg}"
        );
    }

    /// Issue #325's positive control for the predicate: a recorded seal alone
    /// does not make an aborted session re-confirmable. On a volume that is
    /// no longer `initialized`, `volume resume` refuses outright
    /// (`VolumeNotWriteTarget`), and a `'failed'` row is never adopted — so
    /// neither may be answered by naming `volume resume`.
    #[test]
    fn create_with_version_refusal_names_resume_only_where_resume_would_accept() {
        for (status, volume_status) in [("aborted", "retired"), ("failed", "initialized")] {
            let msg = refusal_with_write_on_volume(status, volume_status, true);
            assert!(
                !msg.contains("volume resume"),
                "{status} on a {volume_status} sealed volume: resume would refuse: {msg}"
            );
            assert!(!msg.contains("forfeits"), "{status}/{volume_status}: {msg}");
            assert!(
                msg.contains("tapectl staging clean --unit unit1 --version 1 --force"),
                "{status}/{volume_status}: {msg}"
            );
        }
    }

    /// #420 (ADR-0012 item 112): a version whose only write was CUT by a
    /// short seal is written whole nowhere. The refusal names the volume
    /// that cut it and the write that finishes it — `volume write` onto
    /// another cartridge — and not `volume resume` (nothing is resumable)
    /// nor the abandoned-session advice its `writes` row would otherwise
    /// fall into.
    #[test]
    fn create_with_version_refusal_names_the_volume_that_cut_it_and_the_next_write() {
        let (conn, paths, config, _tmp) = staged_unit_with_write("cut", "sealed", false);
        conn.execute(
            "UPDATE volumes SET ended_after = 9, sealed_at = datetime('now') WHERE label = 'L6-0007'",
            [],
        )
        .unwrap();
        let msg = stage_version_1_refusal(&conn, &paths, &config);
        assert!(
            msg.contains("already has a stage set with live slices"),
            "{msg}"
        );
        assert!(
            msg.contains("volume \"L6-0007\" sealed short before it"),
            "{msg}"
        );
        assert!(msg.contains("`tapectl volume write <LABEL>`"), "{msg}");
        assert!(msg.contains("another cartridge"), "{msg}");
        assert!(
            msg.contains("tapectl staging clean --unit unit1 --version 1"),
            "{msg}"
        );
        assert!(!msg.contains("volume resume"), "{msg}");
        assert!(!msg.contains("aborted"), "{msg}");
        assert!(!msg.contains("min_copies"), "{msg}");
    }

    /// #478 (ADR-0012 items 85 and 129): a version spanning cartridges,
    /// whose tape so far holds its slice 1 as a `range` row and whose slice
    /// 2 is still short of copies. The refusal names the short slice, says
    /// what continues the span (an ordinary `volume write` onto another
    /// cartridge) and warns that `--force` would orphan the sealed range,
    /// instead of reading the set as releasable or abandoned.
    #[test]
    fn create_with_version_refusal_names_the_short_slices_of_a_span() {
        let (conn, paths, config, tmp) = staged_unit_with_write("completed", "sealed", true);
        let stage_set_id: i64 = conn
            .query_row("SELECT id FROM stage_sets LIMIT 1", [], |r| r.get(0))
            .unwrap();
        // Read the completed write as tape 1 of a span: a `range` row that
        // holds slice 1, and a second slice planned after the cut.
        let extra = tmp.path().join("slice-2.age");
        fs::write(&extra, b"slice two").unwrap();
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                        sha256_plain, sha256_encrypted, staging_path)
             VALUES (?1, 2, 9, 9, 'p', 'e2', ?2)",
            params![stage_set_id, extra.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "UPDATE writes SET status = 'range', may_span = 1 WHERE stage_set_id = ?1",
            params![stage_set_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO write_positions (write_id, stage_slice_id, position, status)
             SELECT w.id, sl.id, CAST(5 + sl.slice_number AS TEXT),
                    CASE WHEN sl.slice_number = 1 THEN 'written' ELSE 'pending' END
               FROM writes w JOIN stage_slices sl ON sl.stage_set_id = w.stage_set_id
              WHERE w.stage_set_id = ?1",
            params![stage_set_id],
        )
        .unwrap();

        let msg = stage_version_1_refusal(&conn, &paths, &config);
        assert!(
            msg.contains("already has a stage set with live slices"),
            "{msg}"
        );
        assert!(
            msg.contains("spans cartridges: slice(s) 1-2 are"),
            "at the default min_copies of 2 both are short: {msg}"
        );
        assert!(msg.contains("`tapectl volume write <LABEL>`"), "{msg}");
        assert!(
            msg.contains("another cartridge of the same generation"),
            "{msg}"
        );
        assert!(
            msg.contains("Do not give them up with --force"),
            "--force orphans the sealed range: {msg}"
        );
        assert!(
            msg.contains("tapectl staging clean --unit unit1 --version 1"),
            "{msg}"
        );
        assert!(!msg.contains("volume resume"), "{msg}");
        assert!(!msg.contains("aborted"), "{msg}");
    }

    /// `create_with_version_refusal_keeps_min_copies_wording_for_a_written_under_copied_set`
    /// below: without it, this test alone cannot distinguish "says the
    /// right thing" from "stopped mentioning min_copies anywhere".
    #[test]
    fn create_with_version_refusal_names_never_written_unconditionally() {
        let (conn, paths, config, _tmp) = setup();
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: vec!["unit1".to_string()],
                jobs: None,
                version: None,
            },
            false,
            false,
            false,
        )
        .unwrap();

        // No `writes` row exists for this stage_set at all -- the exact
        // fixture `create_with_version_refuses_when_a_stage_set_is_staged`
        // above builds, made explicit here as the precondition this test
        // depends on.
        let stage_set_id: i64 = conn
            .query_row("SELECT id FROM stage_sets LIMIT 1", [], |r| r.get(0))
            .unwrap();
        let write_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM writes WHERE stage_set_id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(write_count, 0, "precondition: never written anywhere");

        let err = run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: vec!["unit1".to_string()],
                jobs: None,
                version: Some(1),
            },
            false,
            false,
            false,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("already has a stage set with live slices"),
            "{msg}"
        );
        assert!(msg.contains("volume write"), "{msg}");
        assert!(
            msg.contains("--force"),
            "must name --force unconditionally: {msg}"
        );
        assert!(
            !msg.contains("min_copies"),
            "a never-written stage set has nothing to do with min_copies -- \
             mentioning it here misdirects an operator whose unit is not \
             under min_copies into thinking --force does not apply: {msg}"
        );
    }

    /// Issue #279, the other branch and this test module's positive
    /// control: the live stage set DOES have a completed write, and the
    /// unit is below its policy's min_copies -- a plain
    /// `tapectl staging clean --unit <name>` retains it for exactly that
    /// reason, so the refusal keeps today's wording, min_copies mention
    /// included.
    #[test]
    fn create_with_version_refusal_keeps_min_copies_wording_for_a_written_under_copied_set() {
        let (conn, paths, config, _tmp) = setup();
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: vec!["unit1".to_string()],
                jobs: None,
                version: None,
            },
            false,
            false,
            false,
        )
        .unwrap();

        let stage_set_id: i64 = conn
            .query_row("SELECT id FROM stage_sets LIMIT 1", [], |r| r.get(0))
            .unwrap();
        let snapshot_id: i64 = conn
            .query_row(
                "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
             VALUES ('V1', 'lto', 'lto0', 2500000000000, 'sealed')",
            [],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'completed')",
            params![stage_set_id, snapshot_id, volume_id],
        )
        .unwrap();

        // `Config::default()`'s resolved min_copies is 2 (src/config.rs);
        // one completed write leaves "unit1" under-copied.
        let err = run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: vec!["unit1".to_string()],
                jobs: None,
                version: Some(1),
            },
            false,
            false,
            false,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("already has a stage set with live slices"),
            "{msg}"
        );
        assert!(msg.contains("volume write"), "{msg}");
        assert!(msg.contains("staging clean"), "{msg}");
        assert!(
            msg.contains("min_copies"),
            "a written, under-copied set keeps today's min_copies wording: {msg}"
        );
        assert!(msg.contains("--force"), "{msg}");
    }

    /// The granularity check the two tests above cannot tell apart on
    /// their own: `is_release_candidate` is evaluated per LIVE STAGE_SET,
    /// not per unit and not against the `stage_sets` table as a whole. A
    /// second, unrelated unit's stage_set has a completed write (so it
    /// alone would read as a release candidate); "unit1"'s own stage_set
    /// still has none. If the refusal wrongly asked "does ANY stage_set
    /// have a completed write" instead of "does unit1's OWN live
    /// stage_set", it would emit the min_copies-flavoured message here --
    /// this test fails on that wrong composition even though both of the
    /// tests above still pass.
    #[test]
    fn create_with_version_refusal_checks_this_units_own_stage_set_not_any_in_the_table() {
        let (conn, paths, config, _tmp) = setup();
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: vec!["unit1".to_string()],
                jobs: None,
                version: None,
            },
            false,
            false,
            false,
        )
        .unwrap();

        // A second, unrelated unit with a completed write on its own
        // stage_set -- present in `stage_sets` and `writes` alongside
        // unit1's, so a table-wide (rather than per-stage_set) check
        // would wrongly find it.
        let tenant_id: i64 = conn
            .query_row("SELECT id FROM tenants WHERE name = 'alice'", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES ('unit2', 'unit2', ?1, '/tmp/u2', 'active')",
            params![tenant_id],
        )
        .unwrap();
        let unit2_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'current', '/tmp/u2', 1, 10)",
            params![unit2_id],
        )
        .unwrap();
        let snap2_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size)
             VALUES (?1, 'staged', 524288)",
            params![snap2_id],
        )
        .unwrap();
        let stage_set2_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
             VALUES ('V2', 'lto', 'lto0', 2500000000000, 'sealed')",
            [],
        )
        .unwrap();
        let volume2_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'completed')",
            params![stage_set2_id, snap2_id, volume2_id],
        )
        .unwrap();

        // unit1's own stage_set is still never-written -- re-staging it
        // must still get the never-written message, unaffected by unit2's
        // unrelated completed write.
        let err = run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: vec!["unit1".to_string()],
                jobs: None,
                version: Some(1),
            },
            false,
            false,
            false,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--force"),
            "unit1's own stage_set is still never-written: {msg}"
        );
        assert!(
            !msg.contains("min_copies"),
            "a different unit's completed write must not leak into unit1's \
             refusal message: {msg}"
        );
    }

    #[test]
    fn create_with_version_succeeds_once_the_only_stage_set_is_cleaned() {
        let (conn, paths, config, _tmp) = setup();
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: vec!["unit1".to_string()],
                jobs: None,
                version: None,
            },
            false,
            false,
            false,
        )
        .unwrap();

        // Release the first stage set's slices (force, since there's no
        // completed write backing it in this test).
        crate::staging::clean::clean_staging(
            &conn,
            &config,
            true,
            crate::staging::clean::CleanScope::Whole,
        )
        .unwrap();
        let status: String = conn
            .query_row("SELECT status FROM stage_sets LIMIT 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(status, "cleaned");

        // Re-stage must now succeed and produce a second stage set.
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: vec!["unit1".to_string()],
                jobs: None,
                version: Some(1),
            },
            false,
            false,
            false,
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 2,
            "re-staging must add a second stage set, not replace the first"
        );
    }

    /// Issue #53 change 4: once a snapshot can have several stage sets,
    /// `stage info` must not silently hide the siblings. It still shows
    /// only the newest (via `ORDER BY created_at DESC, id DESC`, so two
    /// stage sets created in the same wall-clock second still resolve
    /// deterministically to the just-created one), but must be able to
    /// report how many exist. `run()` only prints to stdout, so this
    /// exercises the same count query `Info`'s handler runs, against the
    /// same fixture, rather than scraping process stdout (which would
    /// race other tests' output under parallel `cargo test`).
    #[test]
    fn info_query_counts_sibling_stage_sets_after_a_restage() {
        let (conn, paths, config, _tmp) = setup();
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: vec!["unit1".to_string()],
                jobs: None,
                version: None,
            },
            false,
            false,
            false,
        )
        .unwrap();

        // Only one stage set yet — count must be 1, and both output modes
        // of `Info` must succeed without printing a sibling note.
        let unit = crate::db::queries::get_unit_by_name(&conn, "unit1")
            .unwrap()
            .unwrap();
        let count_before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM stage_sets ss
                 JOIN snapshots s ON s.id = ss.snapshot_id
                 WHERE s.unit_id = ?1 AND s.version = ?2",
                params![unit.id, 1i64],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count_before, 1);
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Info {
                name: "unit1".to_string(),
                version: 1,
            },
            false,
            false,
            false,
        )
        .unwrap();
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Info {
                name: "unit1".to_string(),
                version: 1,
            },
            true,
            false,
            false,
        )
        .unwrap();

        crate::staging::clean::clean_staging(
            &conn,
            &config,
            true,
            crate::staging::clean::CleanScope::Whole,
        )
        .unwrap();
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: vec!["unit1".to_string()],
                jobs: None,
                version: Some(1),
            },
            false,
            false,
            false,
        )
        .unwrap();

        let count_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM stage_sets ss
                 JOIN snapshots s ON s.id = ss.snapshot_id
                 WHERE s.unit_id = ?1 AND s.version = ?2",
                params![unit.id, 1i64],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count_after, 2,
            "the exact query `stage info` uses to build stage_set_count must \
             see both stage sets once a re-stage has happened"
        );

        // `Info` must still resolve to a single row (the newest) and not
        // error, in both output modes, now that two exist for this version.
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Info {
                name: "unit1".to_string(),
                version: 1,
            },
            false,
            false,
            false,
        )
        .unwrap();
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Info {
                name: "unit1".to_string(),
                version: 1,
            },
            true,
            false,
            false,
        )
        .unwrap();
    }

    /// Issue #354 (b): a staging directory that may be too small is ASKED
    /// about through `cli::consent::confirm`, whose non-interactive refusal
    /// says "re-run with --yes to proceed". That is true only if the global
    /// `--yes` reaches `stage_create`, so this goes through `cli::stage::run`
    /// — the dispatch `main` calls with `cli.yes` — not `stage_create`
    /// itself. With compression on, the question comes before the source is
    /// read, so the refused run leaves no stage set behind. (`cfg(test)`
    /// makes stdin a non-terminal, so the refusal never prompts.)
    #[test]
    fn create_carries_the_global_yes_to_the_staging_space_question() {
        let (conn, paths, mut config, _tmp) = setup();
        config.defaults.compression = "gzip".to_string();
        let snap_id = crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let _free = crate::staging::FreeSpaceOverride::set(1024);
        let create = StageCommands::Create {
            name: vec!["unit1".to_string()],
            jobs: None,
            version: None,
        };

        let err = run(&conn, &paths, &config, &create, false, false, false)
            .expect_err("no terminal and no --yes: a stage that may not fit is refused");
        let msg = err.to_string();
        assert!(
            msg.contains("stage unit \"unit1\" refused: non-interactive session")
                && msg.contains("re-run with --yes")
                && msg.contains("may be too small for unit \"unit1\""),
            "the staging-space consent refusal: {msg}"
        );
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "refused before anything was staged");

        run(&conn, &paths, &config, &create, false, false, true)
            .expect("the global --yes must reach the staging-space question and answer it");
        let status: String = conn
            .query_row(
                "SELECT status FROM snapshots WHERE id = ?1",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "staged");
    }
}
