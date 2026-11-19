pub(crate) mod archive;
pub mod clean;
pub(crate) mod cut;
pub mod exclude;
mod files;
pub mod jobs;
pub mod lock;
pub(crate) mod pack;
pub(crate) use files::SLICE_SUFFIX;
#[cfg(test)]
mod plaintext_audit;
pub mod release;
pub mod tar_report;
#[cfg(test)]
mod tar_stage_tests;
pub mod validate;
pub mod view;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use tracing::info;

use crate::config::{Config, TapectlPaths};
use crate::db::busy::{self, BusyPolicy};
use crate::db::{events, models, queries};
use crate::error::{Result, TapectlError};
use crate::progress;

/// Outcome of `snapshot_create_detailed` (issue #159 / ADR-0012): whether a
/// new version was actually minted, and which row — new or reused — the
/// caller should report or act on.
#[derive(Debug, Clone)]
pub struct SnapshotOutcome {
    pub snapshot_id: i64,
    pub version: i64,
    /// `false` when the walk matched an existing snapshot's recorded
    /// content and nothing new was created — `snapshot_id`/`version`/
    /// `status` then describe that existing row, not a fresh one.
    pub minted: bool,
    /// The snapshot row's status (`created`, `staged`, or `current`) —
    /// lets a caller like `collection::batch::execute_batch` tell "exists
    /// but never staged" from "already staged" from "already on tape"
    /// apart on the `minted: false` path, where none of that is obvious
    /// from `snapshot_id`/`version` alone.
    pub status: String,
}

/// Create a snapshot: fast directory walk, manifest, files table — unless
/// the walk already matches the unit's most recent snapshot (ADR-0012,
/// issue #159): "a version is minted only when content changed." A version
/// number names content, so two versions of a unit never hold identical
/// content; on a match this reports the existing version and creates
/// nothing (`minted: false`) rather than mint a byte-identical sibling.
///
/// The comparison reuses `unit::content_match::matches_snapshot` — the
/// exact predicate `unit info --dirty`/`report dirty`/`audit` already use
/// to decide *Dirty* (`collection::fingerprint::classify`) — against the
/// walk this function has *already* performed (`walked`, below).
/// It is never re-walked: a second `walk_directory`/`WalkDir` pass here
/// would silently double every snapshot's wall-clock time, which is the
/// exact regression issue #159 warns against.
///
/// `config.defaults.global_excludes` (issue #49 item 5) is passed through
/// to `walk_directory` so the recorded `files` rows never include
/// a file the stage was never going to archive (the unit's own dotfile
/// excludes are read internally by `walk_directory`, keyed off
/// `source_path`). `config` also supplies `defaults.large_file_warn_threshold`
/// for the large-file warning (issue #52, design line 203).
pub fn snapshot_create_detailed(
    conn: &Connection,
    unit_name: &str,
    config: &Config,
) -> Result<SnapshotOutcome> {
    snapshot_create_detailed_reading(conn, unit_name, config, None)
}

/// [`snapshot_create_detailed`], reading the unit's files from `view`'s
/// snapshot of the collection root instead of the unit's own folder (ADR-0012
/// 2026-10-16 item 75, issue #455). Only the READS move: the walk and the
/// content match (which may hash files) read the snapshot's copy of the
/// folder; the nesting check, and every path the catalog records
/// (`snapshots.source_path`, the `files` rows), stay the unit's real ones.
pub fn snapshot_create_detailed_reading(
    conn: &Connection,
    unit_name: &str,
    config: &Config,
    view: Option<&view::SourceView>,
) -> Result<SnapshotOutcome> {
    let global_excludes = &config.defaults.global_excludes;
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;

    let source_path = unit
        .current_path
        .as_deref()
        .ok_or_else(|| TapectlError::Other(format!("unit \"{unit_name}\" has no path")))?;

    // Where the files are read from: the unit's folder, or its counterpart in
    // the run's snapshot. `source_path` is what gets recorded.
    let read_path = view::read_path_of(view, source_path)?;
    if !Path::new(&read_path).is_dir() {
        return Err(if view.is_some() {
            TapectlError::Other(format!(
                "unit \"{unit_name}\" ({source_path}) has no folder in the snapshot: \
                 {read_path} is not a directory"
            ))
        } else {
            TapectlError::UnitPathNotFound(source_path.to_string())
        });
    }

    // Nested unit detection (design line 184): "unit init and snapshot
    // create check parent/child. Both errors." Excludes the unit's own
    // row (change 3, issue #52) so a snapshot of an already-registered
    // unit doesn't trip on itself. Runs before the directory walk to fail
    // fast, before doing expensive work.
    crate::unit::nesting::check_nesting_excluding(conn, source_path, Some(unit.id))?;

    // #493 (ADR-0012 item 148): the source is present, so freeze the unit
    // dotfile's own `[policy.copies_at]` for when it is gone. Before the
    // content match below, so a run that mints nothing (a dotfile edit is
    // not content, #378) still records the edited targets. The unit's real
    // folder, as `policy::resolve` reads it, even when the files are read
    // from a snapshot of the collection root.
    crate::policy::freeze_copies_at(conn, &unit)?;

    // Walk directory and build manifest — the ONLY walk this function
    // performs. Both the content-match short-circuit immediately below and
    // the mint path that follows it reuse `walked` (issue #159).
    let Walk {
        total_size,
        file_count,
        entries: walked,
        left_out,
    } = walk_directory_reporting(&read_path, global_excludes).map_err(|e| match e {
        // #569: a refusal of the walk (a name that is not UTF-8, say)
        // names the unit it refuses.
        TapectlError::Other(msg) => TapectlError::Other(format!("unit \"{unit_name}\": {msg}")),
        other => other,
    })?;

    // ADR-0012 / issue #159: does this walk already match the unit's most
    // recent snapshot? `content_match::latest_snapshot` is the exact same
    // "latest" lookup `collection::fingerprint::classify` uses, so the two
    // can never pick different rows and disagree about what "latest" means
    // (the drift this predicate exists to prevent).
    if let Some((latest_id, latest_version, latest_status)) =
        crate::unit::content_match::latest_snapshot(conn, unit.id)?
    {
        let mut fresh_stamps: Vec<crate::unit::content_match::FileStamp> = walked
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| crate::unit::content_match::FileStamp {
                path: e.path.clone(),
                size_bytes: e.size,
                mtime_ns: e.mtime_ns,
            })
            .collect();
        fresh_stamps.sort();

        let diff = crate::unit::content_match::matches_snapshot(
            conn,
            latest_id,
            &unit.checksum_mode,
            Path::new(&read_path),
            &fresh_stamps,
            crate::staging::validate::hash_source_file,
        )?;

        if diff.is_empty() {
            match latest_status.as_str() {
                // Change 2: the live, on-tape version already IS this
                // content — report it, mint nothing.
                //
                // Change 3: an unwritten snapshot (never staged, or staged
                // but never written) already holds this exact content —
                // reuse that row rather than mint a byte-identical sibling
                // beside it. Either way the caller gets `minted: false`
                // and reads `status` to tell which case it was.
                "current" | "created" | "staged" => {
                    return Ok(SnapshotOutcome {
                        snapshot_id: latest_id,
                        version: latest_version,
                        minted: false,
                        status: latest_status,
                    });
                }
                // reclaimable/purged: a dead row. Content
                // happening to match it is coincidence, not identity —
                // mint fresh rather than resurrect it.
                _ => {}
            }
        }
    }

    // Empty units: warn but allow (design line 185). Gated on `file_count`,
    // not `total_size` — a unit full of zero-byte files is not empty.
    if file_count == 0 {
        tracing::warn!(unit = %unit_name, "unit has no files; snapshot will be empty");
    }

    // Large files: warn on any file exceeding `large_file_warn_threshold`
    // (design line 203). Threshold computed once, outside the loop.
    // `parse_size_to_bytes` (issue #59) now rejects a malformed threshold
    // rather than silently miscomputing it; `config.defaults.*` is also
    // validated at config load, so in practice this only fails if that
    // guard is ever bypassed.
    let large_file_threshold = parse_size_to_bytes(&config.defaults.large_file_warn_threshold)?;
    for entry in &walked {
        if !entry.is_dir && entry.size > large_file_threshold {
            tracing::warn!(
                path = %entry.path,
                size_bytes = entry.size,
                threshold_bytes = large_file_threshold,
                "file exceeds large_file_warn_threshold"
            );
        }
    }

    // Issue #374: the snapshot row and every `files` row land in ONE
    // IMMEDIATE transaction, or none of them does. Before, each INSERT was
    // its own autocommit, so a Ctrl-C or a busy catalog partway through left
    // a committed snapshot with a short file list that nothing detected.
    // The walk and the content match above stay outside it: the lock is held
    // only for the inserts (about 1-2 s per 50k files). The version number is
    // read inside it, since IMMEDIATE is what makes read-then-write safe
    // against a concurrent `snapshot create` of the same unit (`db::busy`,
    // rule 2). A busy catalog is retried (rule 3); a transaction dropped on
    // error rolls back, so each attempt starts clean.
    let (snapshot_id, next_version) =
        busy::retry(BusyPolicy::DEFAULT, "the snapshot's file list", || {
            let tx = busy::immediate_tx(conn)?;
            let next_version: i64 = tx.query_row(
                "SELECT COALESCE(MAX(version), 0) + 1 FROM snapshots WHERE unit_id = ?1",
                params![unit.id],
                |row| row.get(0),
            )?;
            tx.execute(
                "INSERT INTO snapshots (unit_id, version, source_path, total_size, file_count)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![unit.id, next_version, source_path, total_size, file_count],
            )?;
            let snapshot_id = tx.last_insert_rowid();

            // What the patterns and the socket rule left out (#542).
            crate::db::snapshot_left_out::insert(&tx, snapshot_id, &left_out)?;

            // The version's file list (`db::files`, migration 030): each
            // path interned once per unit, one narrow row per entry.
            // Mode/uid/gid/xattrs live in the tar stream, which is what
            // a restore reads (ADR-0012 item 7).
            crate::db::files::insert_version(
                &tx,
                snapshot_id,
                walked.iter().map(|entry| {
                    Ok(crate::db::files::FileEntry {
                        path: entry.path.clone(),
                        kind: crate::db::files::FileKind::from_name(entry.file_type)
                            .expect("walk_directory names only the four kinds"),
                        size_bytes: entry.size,
                        mtime_ns: entry.mtime_ns,
                        sha256: None,
                        link_target: entry.link_target.clone(),
                    })
                }),
            )?;

            events::log_created(
                &tx,
                "snapshot",
                snapshot_id,
                &format!("{unit_name} v{next_version}"),
                Some(unit.tenant_id),
            )?;
            tx.commit()?;
            Ok((snapshot_id, next_version))
        })?;

    Ok(SnapshotOutcome {
        snapshot_id,
        version: next_version,
        minted: true,
        status: "created".to_string(),
    })
}

/// Thin wrapper over `snapshot_create_detailed`, kept at its original
/// signature and return type for the ~40 existing call sites across the
/// tree (issue #159) that only ever wanted the new snapshot's id and must
/// keep compiling untouched. A caller that needs to tell "created" from
/// "unchanged, nothing minted" apart — the `snapshot` CLI command,
/// `collection::batch::execute_batch` — calls `snapshot_create_detailed`
/// directly instead.
pub fn snapshot_create(conn: &Connection, unit_name: &str, config: &Config) -> Result<i64> {
    Ok(snapshot_create_detailed(conn, unit_name, config)?.snapshot_id)
}

/// Stage set statuses that mean "live slices already exist for this
/// snapshot" — re-staging on top of one is pointless (the existing slices
/// should go to `volume write`) and would silently produce a second,
/// unrelated copy. Under migration 001's `CHECK(status IN ('staging',
/// 'staged','failed','cleaned'))`, this is exactly the complement of
/// `'cleaned'`/`'failed'`, but it's named after the blocking condition
/// (what `stage create --version`'s refusal message describes), not the
/// allowed one — the single place this status list is written (issue #96:
/// five inlined status lists is how that issue happened).
pub(crate) fn stage_set_has_live_slices(status: &str) -> bool {
    matches!(status, "staging" | "staged")
}

/// What is wrong with a `'staged'` stage set's slices ON DISK — empty when
/// every one is there (issue #402).
///
/// `status = 'staged'` says the slices were staged; it does not say they
/// are still there. Three things can make a staged set incomplete:
///
/// - a slice row whose `staging_path` is NULL — a `stage clean` that an
///   older build interrupted between its per-slice updates, or a
///   `compact-read` that skipped a slice and still promoted the set;
/// - fewer slice rows than the set's recorded `num_slices`;
/// - a recorded file that is missing, or not the size `encrypted_bytes`
///   recorded (the staging disk failed, or someone removed it by hand).
///
/// `volume write` used to keep the slices that still had a path and write
/// them as the whole unit, and coverage counts writes, not slices — a
/// partial unit sealed as a full copy. One description per problem, each
/// naming the slice; the caller names the set.
///
/// **A slice released on its own is not missing** (1.7.0, issue #478;
/// ADR-0012 item 129): a set that holds a range is released slice by slice
/// (`staging::clean::release_covered_slices`), and a released slice's
/// `staging_path` is cleared and its `released_at` set in one transaction
/// before its file is removed. So a NULL path or a missing file is accepted
/// only when `released_at` is set; any other is still refused, as #402
/// rules. Whether a released slice is one a write still needs is the
/// planner's question (it refuses an owed slice that was released).
pub(crate) fn staged_slice_problems(conn: &Connection, stage_set_id: i64) -> Result<Vec<String>> {
    let num_slices: Option<i64> = conn.query_row(
        "SELECT num_slices FROM stage_sets WHERE id = ?1",
        params![stage_set_id],
        |row| row.get(0),
    )?;
    let rows: Vec<(i64, i64, Option<String>, bool)> = conn
        .prepare(
            "SELECT slice_number, encrypted_bytes, staging_path, released_at IS NOT NULL
             FROM stage_slices
             WHERE stage_set_id = ?1 ORDER BY slice_number",
        )?
        .query_map(params![stage_set_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut problems = Vec::new();
    if let Some(n) = num_slices {
        if n != rows.len() as i64 {
            problems.push(format!(
                "{} slice row(s) recorded, {n} expected",
                rows.len()
            ));
        }
    }
    for (number, expected, path, released) in rows {
        if released {
            continue;
        }
        let Some(path) = path else {
            problems.push(format!("slice {number}: no staged file recorded"));
            continue;
        };
        match fs::metadata(&path) {
            Ok(m) if m.is_file() && m.len() as i64 == expected => {}
            Ok(m) if m.is_file() => problems.push(format!(
                "slice {number}: {path} is {} bytes, {expected} recorded",
                m.len()
            )),
            Ok(_) => problems.push(format!("slice {number}: {path} is not a file")),
            Err(e) => problems.push(format!("slice {number}: {path}: {e}")),
        }
    }
    Ok(problems)
}

/// The command that makes an incomplete staged set whole again (issue
/// #402) — `volume write` refuses such a set and `stage list` reports
/// it, and both name this. The identical ciphertext pulled back from an
/// in-service sealed copy when there is one (`read-slices` records every
/// slice's path again); otherwise, for an active unit, the set released and
/// the version staged again from source.
///
/// `failed_volume_id` is a volume the caller has just failed to read the
/// version from (issue #474: `compact-read` skipped a slice of it that did
/// not match its hash). It is never named as the place to read back from —
/// it would fail the same way — so the remedy names another sealed copy or,
/// with none, falls through to staging the version again. `None` where no
/// read has failed.
///
/// **A set that holds a range** (1.7.0, #478): with no volume holding the
/// version whole, a stage set of it that holds a range may still have every
/// missing slice on some volume, a range holder included
/// ([`span_slice_sources`]). The remedy then names, for those slices,
/// `volume read-slices --from` the volumes that hold them, one command per
/// volume. [`crate::policy::coverage::in_service_copy_of_version_excluding`]
/// stays whole-copy only and is asked first.
pub(crate) fn incomplete_set_remedy(
    conn: &Connection,
    unit_id: i64,
    unit_name: &str,
    version: i64,
    unit_status: &str,
    failed_volume_id: Option<i64>,
) -> Result<String> {
    let whole = crate::policy::coverage::in_service_copy_of_version_excluding(
        conn,
        unit_id,
        version,
        failed_volume_id,
    )?;
    let from = match whole {
        Some(from) => vec![from],
        None => span_slice_sources(conn, unit_id, version, failed_volume_id)?,
    };
    Ok(match from.as_slice() {
        [_, ..] => from
            .iter()
            .map(|from| {
                format!(
                    "tapectl volume read-slices {unit_name} --from {from} --version \
                     {version}"
                )
            })
            .collect::<Vec<_>>()
            .join(" && "),
        [] if unit_status == "active" => format!(
            "tapectl stage clean --unit {unit_name} --version {version} --force && \
             tapectl stage create {unit_name} --version {version}"
        ),
        [] => format!(
            "no sealed copy to read back, and unit \"{unit_name}\" is not active so it cannot \
             be staged again — investigate with `tapectl catalog locate {unit_name}`"
        ),
    })
}

/// The volumes `volume read-slices --from` brings back the missing slices of
/// `unit_id`'s version `version` from, when a stage set of it holds a range
/// (#478, a `range` write on any volume): empty when no such set has a
/// missing slice, or when any missing slice has no source, because then no
/// read makes the set whole.
///
/// A slice is missing as [`staged_slice_problems`] sees it: not released
/// (`released_at` unset; a released slice is the planner's question) and
/// with no staged file of its recorded size. Its sources are
/// [`crate::volume::slice_holders::holders_of`]'s in-service holders, a
/// range holder included, never `failed_volume_id`. In slice order, each
/// slice not yet covered takes the source that holds the most of the slices
/// still uncovered, the first such in `holders_of`'s order.
fn span_slice_sources(
    conn: &Connection,
    unit_id: i64,
    version: i64,
    failed_volume_id: Option<i64>,
) -> Result<Vec<String>> {
    use crate::volume::slice_holders::{self, Standing};

    let slices: Vec<(i64, Option<String>, String)> = conn
        .prepare(
            "SELECT sl.encrypted_bytes, sl.staging_path, sl.sha256_encrypted
             FROM stage_slices sl
             JOIN stage_sets ss ON ss.id = sl.stage_set_id
             JOIN snapshots s ON s.id = ss.snapshot_id
             WHERE s.unit_id = ?1 AND s.version = ?2 AND sl.released_at IS NULL
               AND EXISTS (SELECT 1 FROM writes w
                           WHERE w.stage_set_id = ss.id AND w.status = 'range')
             ORDER BY ss.id, sl.slice_number",
        )?
        .query_map(params![unit_id, version], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    let staged_whole = |path: &Option<String>, expected: i64| {
        path.as_deref()
            .and_then(|p| fs::metadata(p).ok())
            .is_some_and(|m| m.is_file() && m.len() as i64 == expected)
    };

    // Each missing slice and the labels of the volumes that may source it.
    let mut missing: Vec<Vec<String>> = Vec::new();
    for (expected, path, sha256) in &slices {
        if staged_whole(path, *expected) {
            continue;
        }
        let sources: Vec<String> = slice_holders::holders_of(conn, sha256)?
            .into_iter()
            .filter(|h| h.standing == Standing::InService)
            .filter(|h| Some(h.volume_id) != failed_volume_id)
            .map(|h| h.label)
            .collect();
        if sources.is_empty() {
            return Ok(Vec::new());
        }
        missing.push(sources);
    }

    let mut chosen: Vec<String> = Vec::new();
    for sources in &missing {
        if sources.iter().any(|s| chosen.contains(s)) {
            continue;
        }
        let uncovered = |label: &String| {
            missing
                .iter()
                .filter(|m| !m.iter().any(|s| chosen.contains(s)))
                .filter(|m| m.contains(label))
                .count()
        };
        // `max_by_key` keeps the last of equals; the first is wanted.
        let best = sources
            .iter()
            .rev()
            .max_by_key(|label| uncovered(label))
            .cloned();
        chosen.extend(best);
    }
    Ok(chosen)
}

/// Full stage pipeline: validate → tar → cut → pack → encrypt → checksums.
///
/// Everything that can refuse without touching the source runs first,
/// before the `stage_sets` INSERT (issue #354): the escrow recipient, the
/// tenant's and operator's keys, and the staging directory (created, then
/// proved writable). A staging directory that MAY be too small for the unit
/// — the need is an estimate made before tar runs ([`stream_estimate`]) — is
/// asked about (ADR-0008 Tier 2; [`check_staging_space`]): `assume_yes`
/// (the global `--yes`) is that consent given in advance, and without it a
/// non-interactive run refuses. What remains to fail after tar starts is
/// the work itself.
///
/// Thin wrapper around `stage_create_inner` (issue #54): on failure, best-effort cleanup runs before the original
/// error is returned unchanged — cleanup never masks the real error.
pub fn stage_create(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    snapshot_id: i64,
    assume_yes: bool,
) -> Result<i64> {
    stage_create_reporting(
        conn,
        paths,
        config,
        snapshot_id,
        assume_yes,
        &mut std::io::stderr(),
    )
}

/// [`stage_create`], hashing and archiving the unit's files from `view`'s
/// snapshot of the collection root instead of the unit's own folder (ADR-0012
/// 2026-10-16 item 75, issue #455): the source check, tar (run in the
/// snapshot's folder) and the recheck after it all read the snapshot's copy,
/// so a file that changes in the living source after the snapshot was taken
/// is not this stage's business. The recorded source path stays the unit's
/// real folder, and `stage_sets.tar_command` names no folder at all.
pub fn stage_create_reading(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    snapshot_id: i64,
    assume_yes: bool,
    view: Option<&view::SourceView>,
) -> Result<i64> {
    stage_create_admitting(
        conn,
        paths,
        config,
        snapshot_id,
        assume_yes,
        Admission::Unstaged,
        view,
        &mut std::io::stderr(),
    )
}

/// [`stage_create`] with its operator notices written to `notices` rather
/// than straight to stderr — the seam the tests read them through.
///
/// A notice is something the operator must see whatever `[logging] level`
/// says (issue #347: `encrypt = false` used to be reported only through
/// `tracing::warn!`, which `logging.level = "error"` silences). So notices
/// never go through `tracing`; `stage_create` hands this stderr, the way
/// `cli::key::print_escrow_secret_warning` bypasses the log level, and
/// stdout stays clean for `--json`.
pub(crate) fn stage_create_reporting(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    snapshot_id: i64,
    assume_yes: bool,
    notices: &mut dyn Write,
) -> Result<i64> {
    stage_create_admitting(
        conn,
        paths,
        config,
        snapshot_id,
        assume_yes,
        Admission::Unstaged,
        None,
        notices,
    )
}

/// Whether a stage is the version's first, or a deliberate re-stage of it
/// (`stage create --version`, issue #53) — what admission rechecks under its
/// lock (issue #368).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Admission {
    /// The caller picked a snapshot that had not been staged (`stage create`
    /// with no `--version`, `collection run`, `quick-archive`). A sibling
    /// that went `staged` between that lookup and admission refuses this
    /// stage: it would be the same version staged twice.
    #[default]
    Unstaged,
    /// `stage create --version`: the caller has already refused a version
    /// with live slices, and a `staged` sibling is not rechecked.
    Restage,
}

/// [`stage_create_reporting`], with what admission rechecks named
/// ([`Admission`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn stage_create_admitting(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    snapshot_id: i64,
    assume_yes: bool,
    admission: Admission,
    view: Option<&view::SourceView>,
    notices: &mut dyn Write,
) -> Result<i64> {
    let stage_set_id_holder: std::cell::Cell<Option<i64>> = std::cell::Cell::new(None);
    // Holds the stage set's flock guard for the entire lifetime of this
    // function call, success or error (issue #98) — `lock::StageLock`
    // unlocks on drop, and this `Cell` lives until `stage_create` returns,
    // so the lock is released only once the caller gets control back
    // (after `cleanup_failed_stage_set` has already run on the error path
    // below).
    let lock_holder: std::cell::Cell<Option<lock::StageLock>> = std::cell::Cell::new(None);
    match stage_create_inner(
        conn,
        paths,
        config,
        snapshot_id,
        assume_yes,
        admission,
        view,
        &stage_set_id_holder,
        &lock_holder,
        notices,
    ) {
        Ok(id) => Ok(id),
        Err(e) => {
            if let Some(stage_set_id) = stage_set_id_holder.get() {
                // Issue #386: the phases up to the failure, the failed one
                // included, are exactly the record a failed stage needs.
                crate::db::phase_timings::record_drained(
                    conn,
                    "stage create",
                    crate::db::phase_timings::Subject::StageSet(stage_set_id),
                );
                after_failed_stage(conn, paths, config, stage_set_id, &e);
            }
            // Issue #404: a stop is a clean end, and says what to run next.
            // Staging resumes by starting the unit over; everything this
            // run had written for it was just removed.
            if let TapectlError::Interrupted(at) = e {
                let unit: String = conn
                    .query_row(
                        "SELECT u.name FROM snapshots s JOIN units u ON u.id = s.unit_id
                         WHERE s.id = ?1",
                        params![snapshot_id],
                        |r| r.get(0),
                    )
                    .unwrap_or_else(|_| "<unit>".to_string());
                return Err(TapectlError::Interrupted(format!(
                    "stage create {unit}: {at}. The unfinished stage set was discarded; run \
                     `tapectl stage create {unit}` to stage it again."
                )));
            }
            Err(e)
        }
    }
}

/// What a failed `stage_create` does with its stage set's files: removes
/// them (`cleanup_failed_stage_set`, issue #54), with two exceptions where
/// they are kept.
///
/// - A busy catalog (issue #377) is not a failed stage. Every slice written
///   so far is recorded (`record_encrypted_slice` removes one it cannot
///   record), so keeping the files loses nothing, and
///   this command does not delete finished work over a lock wait. The next
///   open marks the set `failed` (its lock is free once `stage_create`
///   returns), and `stage clean` reclaims it.
/// - A set that reached `staged` before the error is complete: its slices
///   are the stage, never garbage.
fn after_failed_stage(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    stage_set_id: i64,
    e: &TapectlError,
) {
    let staged = conn
        .query_row(
            "SELECT status = 'staged' FROM stage_sets WHERE id = ?1",
            params![stage_set_id],
            |r| r.get::<_, bool>(0),
        )
        .unwrap_or(false);
    if busy::is_busy_error(e) || staged {
        tracing::warn!(
            stage_set_id,
            error = %e,
            "stage_create stopped; its staging files were left in place"
        );
    } else {
        cleanup_failed_stage_set(conn, paths, config, stage_set_id);
    }
}

#[allow(clippy::too_many_arguments)]
fn stage_create_inner(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    snapshot_id: i64,
    assume_yes: bool,
    admit_as: Admission,
    view: Option<&view::SourceView>,
    stage_set_id_holder: &std::cell::Cell<Option<i64>>,
    lock_holder: &std::cell::Cell<Option<lock::StageLock>>,
    notices: &mut dyn Write,
) -> Result<i64> {
    // ADR-0005 / issue #115. First thing, before the `stage_sets` INSERT and
    // long before tar: without a registered escrow recipient,
    // `recipient_list_with_escrow` below is a silent no-op and every slice
    // this run produces is encrypted to tenant + operator only. Those slices
    // are exactly what the escrow line exists to be able to open, and
    // `volume write`'s pre-flight now refuses them
    // (`LayoutError::StageSetLacksEscrow`) — so staging them at all would
    // spend hours of tar + age on material that cannot be written and cannot
    // be repaired in place. Fail here, cheaply, with the remedy.
    //
    // The check belongs to this caller, not to `recipient_list_with_escrow`:
    // that helper is also how `volume write` builds its ENVELOPE recipient
    // lists, and must keep working as a no-op for callers that are not
    // minting new ciphertext (issue #115's scope fence).
    if queries::escrow_public_key(conn)?.is_none() {
        return Err(TapectlError::Other(
            "no escrow recipient is registered — staged slices would be unrecoverable \
             with the escrow key and volume write would refuse them (ADR-0005). \
             Register one first: `tapectl key generate --escrow`, or adopt an existing \
             public key with `tapectl key import --escrow <age1...>`"
                .into(),
        ));
    }

    let snapshot = get_snapshot(conn, snapshot_id)?;
    let unit = get_unit_for_snapshot(conn, &snapshot)?;
    // Where this stage reads the unit's files: the recorded folder, or its
    // counterpart in the run's snapshot of the collection root (ADR-0012
    // 2026-10-16 item 75, issue #455). Everything the stage RECORDS keeps
    // `snapshot.source_path`; only the reads below use `read_root`. A folder
    // the snapshot lacks is refused here, before anything is staged.
    let read_root = view::read_path_of(view, &snapshot.source_path)?;
    if view.is_some() && !Path::new(&read_root).is_dir() {
        return Err(TapectlError::Other(format!(
            "unit \"{}\" ({}) has no folder in the snapshot: {read_root} is not a directory",
            unit.name, snapshot.source_path
        )));
    }
    check_file_list_complete(conn, &snapshot, &unit.name)?;
    // Resolve policy (dotfile > archive_set > defaults) — issue #47/#48:
    // stage_create used to read config.defaults.* unconditionally, so an
    // archive_set or dotfile override of slice_size/compression/preserve_*
    // was silently discarded even where #48 gives archive_set_id a writer.
    let resolved = crate::policy::resolve(conn, config, &unit)?;
    // #493 (ADR-0012 item 148): the policy resolved with the source present,
    // so freeze the dotfile's own `[policy.copies_at]` for when it is gone.
    // After `resolve`, so a dotfile it refuses is never frozen.
    crate::policy::freeze_copies_at(conn, &unit)?;
    check_unit_fits_staging(config, &unit.name, &snapshot, notices)?;
    let tenant = queries::get_tenant_by_id(conn, unit.tenant_id)?
        .ok_or_else(|| TapectlError::Other("tenant not found".into()))?;

    // Issue #354 (c): the recipient list is a pure database read, so it is
    // built — and every refusal it can raise is raised — here, before the
    // `stage_sets` INSERT and before tar, beside the escrow check above. It
    // used to be built at the top of the encryption loop, after tar had
    // archived the whole unit, so a tenant with no active keys cost a full
    // tar run to discover.
    let tenant_keys = queries::get_active_keys_for_tenant(conn, unit.tenant_id)?;
    // Refuse rather than silently encrypt operator-only: a tenant with zero
    // active keys (e.g. an interrupted rotation, pre-H13 fix) would otherwise
    // produce slices the tenant can never decrypt themselves.
    if tenant_keys.is_empty() {
        return Err(TapectlError::Other(format!(
            "tenant for unit \"{}\" has no active keys — refusing to encrypt \
             (the tenant could not decrypt its own data); run `tapectl key rotate` \
             or restore the tenant's keys first",
            unit.name
        )));
    }
    let operator = queries::get_operator_tenant(conn)?
        .ok_or_else(|| TapectlError::Other("no operator tenant".into()))?;
    let operator_keys = queries::get_active_keys_for_tenant(conn, operator.id)?;

    let all_pubkeys: Vec<String> = tenant_keys
        .iter()
        .chain(operator_keys.iter())
        .map(|k| k.public_key.clone())
        .collect();
    // ADR-0005: every recipient list gets the escrow public key appended
    // (a no-op if it's already present; its absence was refused above).
    let all_pubkeys = queries::recipient_list_with_escrow(conn, all_pubkeys)?;
    // Parse every recipient now, for the same reason: a malformed public key
    // in the database would otherwise surface from the first slice's
    // encryption, after tar had started. The encryptor itself is rebuilt
    // per slice (age draws a fresh file key each time); this one is only the
    // check.
    build_encryptor(&all_pubkeys)?;

    // #562: `slice_size` is a multiple of 512 wherever it is set, so that a
    // slice's cut is whole tar blocks (`volume-format-v3.md` §3.3). Refused
    // before anything is made.
    let slice_size = u64::try_from(resolved.slice_size).unwrap_or(0);
    if slice_size == 0 || !slice_size.is_multiple_of(512) {
        return Err(TapectlError::Other(format!(
            "the slice_size in force for unit \"{}\" (its dotfile's [policy], its archive set \
             or [defaults]) is {} bytes, which is not a positive multiple of 512: a slice holds \
             whole tar blocks (volume-format-v3.md §3.3). Set slice_size to a multiple of 512 \
             bytes, such as 1G. Nothing was staged.",
            unit.name, resolved.slice_size
        )));
    }

    // Items 284 to 287: the staging's algorithm and level, judged before
    // anything is made.
    let packing =
        pack::packing(&resolved.compression, resolved.compression_level).map_err(|e| {
            TapectlError::Other(format!("unit \"{}\": {e} Nothing was staged.", unit.name))
        })?;
    let compression = match packing {
        Some(p) => p.algorithm.name().to_string(),
        None => "none".to_string(),
    };

    // Issue #479 (ADR-0012 2026-10-17 item 100): the slice size this unit
    // will be cut at must leave a slice and the seal inside half the
    // measured post-warning room of the smallest configured drive. A packed
    // slice can be larger on tape than its cut, so it is held to slice_size
    // plus its algorithm's stated worst growth (`volume-format-v3.md` §3.4).
    // The config-level check (`Config::load`) sees only `[defaults]` and the
    // file's archive sets; this is the size in force after the unit's
    // dotfile and its archive set's database row have had their say.
    let (cap_what, cap_bytes) = match packing {
        None => (
            format!(
                "the slice_size in force for unit \"{}\" (its dotfile's [policy], its archive \
                 set or [defaults])",
                unit.name
            ),
            slice_size,
        ),
        Some(p) => (
            format!(
                "the slice_size in force for unit \"{}\" (its dotfile's [policy], its archive \
                 set or [defaults]) plus {}'s stated worst growth ({} bytes)",
                unit.name,
                p.algorithm.name(),
                pack::worst_growth(slice_size)
            ),
            files::packed_bound(slice_size),
        ),
    };
    if let Some(problem) = config.slice_cap_problem(&cap_what, cap_bytes) {
        return Err(TapectlError::Other(format!(
            "{problem} Nothing was staged."
        )));
    }

    // Step 1: the early refusal (ADR-0012 item 274). The writer's floor is
    // GNU tar 1.35 built with ACL support; the proof is the stream's own
    // check below, and this is the refusal before anything is made.
    let tar_check = {
        let work = home_work_dir(paths, ".tar-probe-")?;
        crate::tar::probe(&config.tar.binary, work.path())?
    };
    if let Some(refusal) = tar_check.refusal() {
        return Err(TapectlError::Other(format!(
            "{refusal}. Nothing was staged."
        )));
    }

    // #542: what the version left out is said here too, so a file on no tape
    // is never silent.
    let left_out = crate::db::snapshot_left_out::load(conn, snapshot_id)?;
    write_left_out(notices, &unit.name, snapshot.version, &left_out);

    // Issue #354: the staging directory is usable, and big enough or the
    // operator agreed to try, or the stage is refused here — before the
    // INSERT and tar. `staging` is the only way this stage creates a file
    // there, and the only files it can create are ciphertext (issue #370).
    let staging_dir = Path::new(&config.staging.directory);
    let staging = files::StagingDir::prepare(staging_dir)?;
    // Issue #368: stages may run at once (`stage create --jobs`, or two
    // commands). From here to the stage set's commit below, one stage at a
    // time: so the next one sees this unit as staging, and this stage's
    // space as spoken for, before it decides anything of its own.
    let admission = lock::acquire_admission(&paths.db_file)?;
    refuse_a_unit_already_staging(conn, &paths.db_file, &snapshot, &unit.name, admit_as)?;
    let in_flight = in_flight_staging(conn, &paths.db_file)?;
    let estimate = stream_estimate(conn, snapshot_id, &read_root, slice_size, packing)?;
    check_staging_space(
        &StagingSpaceInputs {
            staging_dir,
            unit_name: &unit.name,
            estimate,
            compression: &compression,
            assume_yes,
            in_flight,
        },
        notices,
    )?;

    // ADR-0005's escrow recipient participates in every write, and
    // pre-write validation refuses without one — encryption cannot be made
    // optional without contradicting that, and doing so would also breach
    // the sacred no-plaintext-tenant-identity-on-tape invariant. Coordinator
    // decision (issues #47/#48, 2026-07-29): never refuse the stage and
    // never silently ignore a `policy.encrypt = false`, but never honor it
    // either — warn loudly and encrypt regardless.
    //
    // Issue #347: "loudly" means a notice, not `tracing::warn!` — the log
    // level (`[logging] level`, default "warn") could silence it, and an
    // operator who set `encrypt = false` must never be left believing it
    // took effect.
    if !resolved.encrypt {
        let _ = writeln!(
            notices,
            "warning: unit \"{}\": policy sets encrypt = false, which tapectl never \
             honours — its slices are encrypted anyway, to the tenant, operator and escrow \
             recipients (ADR-0005). Remove `encrypt = false` from the unit's archive set \
             or from [defaults].",
            unit.name
        );
    }

    // Create stage_set record.
    //
    // Issue #98: the row must never be visible to another connection at
    // `status = 'staging'` without its flock already held, or a concurrent
    // `db::open()` sweep (`recover_orphaned_sessions`) could probe-lock it,
    // find it free, and mark a genuinely live stage_set `'failed'`. Under
    // WAL a row is invisible to other connections until commit, so this
    // whole sequence — INSERT, read back the new id, acquire the flock —
    // happens INSIDE one explicit transaction, and the flock is acquired
    // BEFORE that transaction commits. Acquiring a flock is microseconds,
    // so this does not reintroduce the long-running-transaction problem the
    // rest of this function's comments warn about (the tar run below stays
    // entirely outside any transaction).
    #[cfg(test)]
    failpoint::hit(failpoint::BEFORE_RECORD)?;
    //
    // Issue #368: other stages may hold the catalog meanwhile, so the whole
    // sequence is retried under the busy policy (#377); an attempt that
    // fails rolls back and drops its lock.
    let (stage_set_id, stage_lock) = busy::retry(BusyPolicy::DEFAULT, "the new stage set", || {
        let insert_tx = busy::immediate_tx(conn)?;
        let stage_set_id =
            insert_stage_set(&insert_tx, snapshot_id, resolved.slice_size, &compression)?;
        let stage_lock = lock::acquire(&paths.db_file, stage_set_id)?;
        insert_tx.commit()?;
        Ok((stage_set_id, stage_lock))
    })?;
    stage_set_id_holder.set(Some(stage_set_id));
    lock_holder.set(Some(stage_lock));
    // The stage set is committed and locked: the next stage to be admitted
    // sees it.
    drop(admission);

    // Step 2: the source against the snapshot, by metadata alone (issue
    // #364): a missing file or one at another size refuses before tar. The
    // content is read once, by tar, and hashed from its stream. Issue #386:
    // each step is a named phase — live progress, the session log, and a
    // `phase_timings` row plus a stage-report line.
    let phase = progress::phase("check", None);
    phase.item(unit.name.clone());
    let plan = validate::plan(
        conn,
        snapshot_id,
        &read_root,
        &config.defaults.global_excludes,
    )?;
    // Step 3: the list is the snapshot's own entries in bytewise path order,
    // never a new walk; each is looked at before tar runs.
    let scan = archive::scan(conn, snapshot_id, Path::new(&read_root), &unit.name)?;
    phase.done();

    // `archive_base` is per-*stage-set* (issue #53): it carries
    // `stage_set_id` so two stage sets of the SAME snapshot never write
    // identically-named `.age` files. `cleanup_failed_stage_set`'s prefix
    // derivation must move in lockstep with this — both go through
    // `archive_base_name`.
    let archive_base = archive_base_name(&unit.uuid, snapshot.version, stage_set_id);

    // fingerprint == public_key by construction for every key in this system
    // (see crypto::keys::generate_keypair and `key import`), so the recorded
    // fingerprints are exactly the (now escrow-augmented) recipient list.
    let key_fingerprints = all_pubkeys.clone();

    // Step 4: tar, its stream checked as it passes, cut into slices of whole
    // members, each packed alone when the staging is compressed and
    // age-encrypted in memory (#370: no plaintext on the staging device).
    // The file list holds file names, so it is written in the home's work
    // directory and removed with it.
    let phase = progress::phase("archive", Some(plan.total_bytes()));
    phase.item(unit.name.clone());
    let list_dir = home_work_dir(paths, ".tar-list-")?;
    let list = crate::tar::write_list(list_dir.path(), &scan.paths())?;
    let params = crate::tar::CreateParams {
        binary: &config.tar.binary,
        root: Path::new(&read_root),
        list: &list,
        xattrs: resolved.preserve_xattrs,
        acls: resolved.preserve_acls,
    };
    let mut run = crate::tar::spawn_archive(&params)?;
    let tar_command = run.tar_command.clone();
    let tar_version = tar_check.first_line.clone();
    let level = packing.map(|p| i64::from(p.level));
    // Issue #368: other stages may hold the catalog for a moment; a stage
    // records its progress through the busy policy (#377).
    busy::retry(BusyPolicy::DEFAULT, "the stage set's tar command", || {
        Ok(conn.execute(
            "UPDATE stage_sets SET tar_version = ?1, tar_command = ?2, compression_level = ?3
             WHERE id = ?4",
            params![tar_version, tar_command, level, stage_set_id],
        )?)
    })?;

    let on_slice = |n: u64| phase.item(format!("{} slice {n}", unit.name));
    let slices = archive::StagedSlices {
        conn,
        staging: &staging,
        stage_set_id,
        base: &archive_base,
        recipients: &all_pubkeys,
        packing,
        slice_size,
        unit: &unit.name,
        on_slice: &on_slice,
        open: None,
        totals: archive::Totals::default(),
    };
    let mut sink = archive::StageSink {
        cutter: cut::Cutter::new(slice_size, slices)?,
        rows: Vec::new(),
    };
    let read = crate::tar::read_stream(
        run.take_stdout(),
        scan.entries.iter().map(|e| e.listed.clone()),
        crate::tar::Rules {
            acls: resolved.preserve_acls,
        },
        crate::idmap::Collector::new(),
        &mut sink,
    );
    // tar's own report keeps the folder it ran in beside its command (issue
    // #455: the one place the operator sees what tar read, a snapshot's copy
    // of the folder included); `stage_sets.tar_command` names no folder.
    let ran_in = format!("{tar_command} (run in {read_root:?})");
    let report = |outcome: &'static str, finish: Option<&crate::tar::TarFinish>| {
        tar_report::TarCreateReport {
            outcome,
            command: ran_in.clone(),
            exit_code: finish.and_then(|f| f.code()),
            stderr: finish.map(|f| f.stderr().kept.clone()).unwrap_or_default(),
            tar_version: tar_version.clone(),
        }
    };
    let finished = match read {
        Ok(finished) => finished,
        Err(e) => {
            // tar's own exit, once its stream is no longer read, and what it
            // said: kept whichever way this ends (issue #343).
            let finish = run.finish().ok();
            let stopped = crate::signal::is_interrupted();
            let outcome = if stopped {
                tar_report::OUTCOME_STOPPED
            } else {
                tar_report::OUTCOME_FAILED
            };
            tar_report::record(
                conn,
                stage_set_id,
                &unit.name,
                snapshot.version,
                &report(outcome, finish.as_ref()),
            );
            if stopped {
                return Err(TapectlError::Interrupted(stopped_archiving(
                    &snapshot.source_path,
                )));
            }
            return Err(stream_refused(&unit.name, e, finish.as_ref()));
        }
    };
    let finish = run.finish()?;
    if !finish.is_clean() {
        let outcome = match finish {
            crate::tar::TarFinish::FilesChanged { .. } => tar_report::OUTCOME_FILES_CHANGED,
            _ => tar_report::OUTCOME_FAILED,
        };
        tar_report::record(
            conn,
            stage_set_id,
            &unit.name,
            snapshot.version,
            &report(outcome, Some(&finish)),
        );
        return Err(tar_failed(&unit.name, &finish));
    }
    tar_report::record(
        conn,
        stage_set_id,
        &unit.name,
        snapshot.version,
        &report(tar_report::OUTCOME_COMPLETE, Some(&finish)),
    );
    let archive::StageSink { cutter, rows } = sink;
    let (made, slices) = cutter.finish()?;
    let totals = slices.totals;
    debug_assert_eq!(made, totals.slices);
    phase.done();
    info!(
        slices = totals.slices,
        members = finished.members,
        stream_bytes = finished.stream_bytes,
        "tar stream staged"
    );

    #[cfg(test)]
    failpoint::note_staging(staging_dir);
    #[cfg(test)]
    failpoint::hit(failpoint::AFTER_TAR)?;

    // Step 5: each entry is what it was before tar ran (invariant 7), and a
    // file inside the racy window hashes on disk as it did in the stream.
    let phase = progress::phase("recheck", None);
    let stream_sha: std::collections::HashMap<&str, [u8; 32]> = rows
        .iter()
        .filter_map(|r| r.sha256.map(|h| (r.path.as_str(), h)))
        .collect();
    archive::recheck(Path::new(&read_root), &scan, &stream_sha)?;
    // Step 6: each stream hash against the version's recorded sha256, for
    // every path but the unit's dotfile (#562; issue #32/H6's commitment
    // point): the size equals the snapshot's, so a difference is content
    // drift at a constant size — bitrot, not an edit. A hash not yet
    // recorded is filled in.
    let baselines = plan.baselines();
    let mut checksums: Vec<(String, String)> = Vec::with_capacity(stream_sha.len());
    for row in &rows {
        let Some(sha) = row.sha256 else { continue };
        let hex = crate::db::files::sha256_to_hex(&sha);
        if !crate::unit::dotfile::is_unit_dotfile(&row.path) {
            if let Some(baseline) = baselines.get(row.path.as_str()) {
                if *baseline != hex {
                    return Err(TapectlError::Other(format!(
                        "BITROT suspected: {} — sha256 differs at an unchanged size ({} \
                         bytes): baseline={baseline}, current={hex}. Refusing to stage; \
                         investigate before re-staging (`tapectl unit check-integrity <unit>` \
                         checks every file against its recorded baseline).",
                        row.path, row.size
                    )));
                }
            }
        }
        checksums.push((row.path.clone(), hex));
    }
    busy::retry(BusyPolicy::DEFAULT, "the source check", || {
        Ok(conn.execute(
            "UPDATE stage_sets SET source_validated_at = datetime('now') WHERE id = ?1",
            params![stage_set_id],
        )?)
    })?;
    phase.done();

    // Step 7: one transaction at the end (issue #54), holding the stage set's
    // figures, the file table's rows and the id map's (ADR-0012 items 265,
    // 272, 277).
    //
    // Deliberately NOT wrapping the whole of `stage_create`: a DEFERRED
    // transaction takes SQLite's single write lock at its first write and
    // holds it until commit — around the tar run (which can take hours) that
    // would block every other tapectl invocation. Worse, a crash mid-tar
    // would roll back the `stage_sets` INSERT itself, destroying the
    // `status='staging'` row that `recover_orphaned_sessions` relies on to
    // mark the set `'failed'` on the next `db::open()`. So the incremental
    // progress writes above stay outside any transaction; only this
    // finalization — run once the pipeline has fully succeeded — is atomic.
    //
    // Issue #377: IMMEDIATE, and retried as a whole for minutes on a busy
    // catalog — every slice is already encrypted, and a 5-second lock wait
    // must not throw them away. A dropped transaction rolls back, so each
    // attempt starts clean.
    let phase = progress::phase("finalize", None);
    busy::retry(BusyPolicy::DEFAULT, "the stage set's finalization", || {
        let tx = busy::immediate_tx(conn)?;

        tx.execute(
            "UPDATE stage_sets SET status = 'staged', num_slices = ?1, stream_bytes = ?2,
             total_encrypted_size = ?3, key_fingerprints = ?4, staged_at = datetime('now')
             WHERE id = ?5",
            params![
                totals.slices as i64,
                finished.stream_bytes as i64,
                totals.encrypted,
                serde_json::to_string(&key_fingerprints).unwrap(),
                stage_set_id,
            ],
        )?;

        let updated = tx.execute(
            "UPDATE snapshots SET status = 'staged' WHERE id = ?1 AND status = 'created'",
            params![snapshot_id],
        )?;
        if updated > 0 {
            events::log_field_change(
                &tx,
                "snapshot",
                snapshot_id,
                &format!("{} v{}", unit.name, snapshot.version),
                "status_change",
                "status",
                Some("created"),
                "staged",
                Some(unit.tenant_id),
            )?;
        }

        // Backfill sha256 into files — establishes the baseline ONLY where
        // one doesn't already exist (issue #32/H6): `backfill_checksums`'s
        // own `sha256 IS NULL` guard on its UPDATE.
        if !checksums.is_empty() {
            backfill_checksums(&tx, snapshot_id, &checksums)?;
        }

        crate::db::stage_files::insert(&tx, stage_set_id, rows.iter().cloned().map(Ok))?;
        crate::db::stage_ids::insert(&tx, stage_set_id, &finished.ids)?;

        tx.commit()?;
        Ok(())
    })?;

    phase.done();

    // Issue #386: this stage's phases, recorded against the stage set and
    // written into its report. A session-less caller (a test, a library
    // user) records nothing, and the report then has no timings section.
    let timings = progress::drain();
    if let Err(e) = crate::db::phase_timings::record(
        conn,
        "stage create",
        crate::db::phase_timings::Subject::StageSet(stage_set_id),
        &timings,
    ) {
        tracing::warn!(error = %e, stage_set_id, "could not record the stage's phase timings");
    }

    // The stage report is filesystem work and must not sit inside a DB
    // transaction — done here, after commit, along with the creation event.
    let mut report = generate_stage_report(conn, stage_set_id, &unit, &snapshot, &tenant)?;
    report.push_str(&render_phase_timings(&timings));
    let _report_path = write_stage_report(paths, stage_set_id, &report)?;

    busy::retry(
        BusyPolicy::DEFAULT,
        "the stage set's creation event",
        || {
            events::log_created(
                conn,
                "stage_set",
                stage_set_id,
                &format!("{} v{}", unit.name, snapshot.version),
                Some(unit.tenant_id),
            )
        },
    )?;

    Ok(stage_set_id)
}

/// Item 287: why `compression_level = level` is refused beside the
/// algorithm `compression`, or `None` (for `config.rs`'s load checks).
pub(crate) fn pack_level_problem(compression: &str, level: i64) -> Option<String> {
    pack::packing(compression, Some(level))
        .err()
        .map(|e| e.to_string())
}

/// The `meta` key holding the highest `stage_sets.id` ever handed out.
const STAGE_SET_HIGH_WATER: &str = "stage_set_id_high_water";

/// INSERT a stage set in `staging` and return its id, which is never one
/// that was handed out before (issue #465).
///
/// `stage_sets.id` is a bare `INTEGER PRIMARY KEY`, so SQLite's own choice
/// is `MAX(id) + 1`: after `snapshot delete --force` removes the highest
/// row the next stage set would take its id back — and with it the stage's
/// lock path (`locks/stage-<id>.lock`), the `_s<id>` prefix of its files in
/// staging, the `events` entity id, and the stage report's name. AUTOINCREMENT
/// would fix that in the table, but adding it rebuilds the table
/// (a migration), so the highest id ever issued is kept in `meta` instead
/// and the new id is one above both it and the table's own maximum.
///
/// Call it inside the transaction that inserts the row: that transaction is
/// IMMEDIATE, so the write lock serialises two stages reading the mark.
fn insert_stage_set(
    tx: &Connection,
    snapshot_id: i64,
    slice_size: i64,
    compression: &str,
) -> Result<i64> {
    use rusqlite::OptionalExtension;
    let marked: i64 = tx
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![STAGE_SET_HIGH_WATER],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let held: i64 = tx.query_row("SELECT COALESCE(MAX(id), 0) FROM stage_sets", [], |row| {
        row.get(0)
    })?;
    let id = marked.max(held) + 1;
    tx.execute(
        "INSERT INTO stage_sets (id, snapshot_id, slice_size, compression, encrypted)
         VALUES (?1, ?2, ?3, ?4, 1)",
        params![id, snapshot_id, slice_size, compression],
    )?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![STAGE_SET_HIGH_WATER, id.to_string()],
    )?;
    Ok(id)
}

/// Record one encrypted slice: its `stage_slices` row (issue #377). The
/// INSERT is retried on a busy catalog for `policy`'s budget. If it cannot be
/// written, the `.age` is removed again: no row would ever name it. There is
/// no plaintext slice to delete (issue #370) — only ciphertext was written.
fn record_encrypted_slice(
    conn: &Connection,
    policy: BusyPolicy,
    stage_set_id: i64,
    slice_num: i64,
    continues: bool,
    encrypted_path: &Path,
    info: &EncryptedSliceInfo,
) -> Result<()> {
    let recorded = busy::retry(policy, "a staged slice", || {
        Ok(conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                       sha256_plain, sha256_encrypted, staging_path, continues)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                stage_set_id,
                slice_num,
                info.plain_size,
                info.encrypted_size,
                info.sha256_plain,
                info.sha256_encrypted,
                encrypted_path.to_string_lossy().to_string(),
                continues,
            ],
        )?)
    });
    if let Err(e) = recorded {
        let _ = fs::remove_file(encrypted_path);
        return Err(e);
    }
    #[cfg(test)]
    durability::note(durability::Event::Recorded(encrypted_path.to_path_buf()));
    Ok(())
}

/// The `archive_base` file-name stem for one stage set: `{uuid12}_v{version}_s{stage_set_id}`.
///
/// Per-*stage-set*, not per-snapshot (issue #53) — the single place this
/// shape is computed, so `stage_create_inner`'s slices and
/// `cleanup_failed_stage_set`'s prefix scan can never drift apart again.
/// The isolated catalogue is per stage set too since issue #419, by its
/// directory ([`stage_set_catalogue_dir`]).
pub(crate) fn archive_base_name(unit_uuid: &str, version: i64, stage_set_id: i64) -> String {
    format!(
        "{}_v{}_s{}",
        unit_uuid.replace('-', "").get(..12).unwrap_or(unit_uuid),
        version,
        stage_set_id,
    )
}

/// `archive_base_name(..)` plus the load-bearing trailing dot: every slice
/// is `{base}.{N}.tar.age`, so the real filesystem prefix is
/// `{base}.`, not the bare base — without the dot, `_s1` would prefix-match
/// `_s10.1.tar.age`.
pub(crate) fn archive_base_prefix(unit_uuid: &str, version: i64, stage_set_id: i64) -> String {
    format!("{}.", archive_base_name(unit_uuid, version, stage_set_id))
}

/// A fresh work directory under `<home>/tmp` (0700), removed when the
/// returned value drops — for the plaintext a stage or a write needs for a
/// moment (tar's file list and probe, the write's `catalog.db`), which must not
/// go under staging (ADR-0012 2026-10-06 item 4) nor to a world-readable
/// `/tmp`.
pub fn home_work_dir(paths: &TapectlPaths, prefix: &str) -> Result<tempfile::TempDir> {
    let tmp = paths.home.join("tmp");
    fs::create_dir_all(&tmp).map_err(|e| staging_io_error("cannot create", &tmp, e))?;
    crate::config::secure_path(&tmp, 0o700);
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(&tmp)
        .map_err(|e| staging_io_error("cannot make a work directory in", &tmp, e))
}

/// What a stop during the archive pass says (issue #404).
fn stopped_archiving(source: &str) -> String {
    format!("tar was stopped while archiving {source}")
}

/// The refusal of a stream that broke one of §3.2's rules, or that could
/// not be read, with tar's own exit beside it when tar did not exit 0.
fn stream_refused(
    unit: &str,
    e: crate::tar::StreamError,
    finish: Option<&crate::tar::TarFinish>,
) -> TapectlError {
    let tar_said = match finish {
        Some(f) if !f.is_clean() => format!(
            " tar exited {}: {}",
            f.code()
                .map_or_else(|| "by a signal".to_string(), |c| c.to_string()),
            f.stderr().text().trim()
        ),
        _ => String::new(),
    };
    match e {
        crate::tar::StreamError::Refused(r) => TapectlError::Other(format!(
            "unit \"{unit}\" is not staged: {r}.{tar_said} The stream's checks are tapectl's \
             proof of what reaches tape (volume-format-v3.md §3.2); nothing was staged."
        )),
        crate::tar::StreamError::Sink(e) => e,
        crate::tar::StreamError::Io(e) => TapectlError::Other(format!(
            "unit \"{unit}\" is not staged: reading tar's stream failed: {e}.{tar_said} \
             Nothing was staged."
        )),
    }
}

/// The refusal of a staging whose tar did not exit 0 (#562): exit 1 says a
/// file changed as tar read it; exit 2, or any other, that tar failed.
fn tar_failed(unit: &str, finish: &crate::tar::TarFinish) -> TapectlError {
    let said = finish.stderr().text();
    let said = said.trim();
    match finish {
        crate::tar::TarFinish::FilesChanged { .. } => TapectlError::Other(format!(
            "DIRTY: a source file of unit \"{unit}\" changed while tar was reading it (tar exit \
             1), so it is refused rather than archived half-changed. tar said:\n{said}\nNothing \
             was staged. Stage again once the source is quiet; if the change is real, take a \
             new snapshot (`tapectl snapshot create {unit}`) and stage that."
        )),
        _ => TapectlError::Other(format!(
            "tar failed archiving unit \"{unit}\" (exit {}). tar said:\n{said}\nNothing was \
             staged.",
            finish
                .code()
                .map_or_else(|| "by a signal".to_string(), |c| c.to_string()),
        )),
    }
}

/// How many left-out things `stage create` names before it counts the rest.
const LEFT_OUT_NAMED: usize = 20;

/// #542: the lines naming what the version's snapshot left out (a pattern's
/// match, or a socket), as `snapshot create` names them; nothing when
/// nothing was. Names are escaped as the index escapes them.
fn write_left_out(
    notices: &mut dyn Write,
    unit: &str,
    version: i64,
    rows: &[crate::db::snapshot_left_out::LeftOut],
) {
    use crate::db::snapshot_left_out::Reason;
    if rows.is_empty() {
        return;
    }
    let mut out = format!(
        "unit \"{unit}\" v{version}: its snapshot left out {}, so this stage does not \
         archive {}:",
        crate::render::plural(rows.len() as i64, "entry", "entries"),
        if rows.len() == 1 { "it" } else { "them" }
    );
    for row in rows.iter().take(LEFT_OUT_NAMED) {
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
        out.push_str(&format!("\n  left out: {} ({what})", row.path));
    }
    if rows.len() > LEFT_OUT_NAMED {
        out.push_str(&format!(
            "\n  and {} more left out; `tapectl snapshot create --json` lists them all",
            rows.len() - LEFT_OUT_NAMED
        ));
    }
    let _ = writeln!(notices, "{out}");
}

/// The directory that held one stage set's Layout 2 catalogue (issue #419):
/// `<home>/catalogs/{uuid8}/s{stage_set_id}/`. A staging on tar makes none;
/// `volume::rebuild` still names it until T4.9 ports it, and a failed stage
/// removes it when it is there.
pub(crate) fn stage_set_catalogue_dir(
    paths: &TapectlPaths,
    unit_uuid: &str,
    stage_set_id: i64,
) -> PathBuf {
    paths
        .catalogs_dir
        .join(unit_uuid.get(..8).unwrap_or(unit_uuid))
        .join(format!("s{stage_set_id}"))
}

/// Best-effort cleanup of a `stage_set` that `stage_create` failed to
/// finish, run from the `stage_create` wrapper's `Err` path (issue #54).
///
/// Two discovery strategies, both kept:
///
/// - **By filesystem prefix.** The `.tar.age` being written when the stage
///   failed has no `stage_slices` row yet (a slice is recorded once it is
///   complete and synced; issue #370 writes it straight from tar's stream).
///   [`files::SliceWriter`] removes its own file when dropped unfinished,
///   so this catches only what that could not. `archive_base_name` (issue
///   #53) carries `stage_set_id`, so the dot-terminated prefix is unique
///   per stage set and can never match a sibling stage set of the same
///   snapshot.
///
/// - **By DB row (`stage_slices.staging_path`).** Every recorded `.age`,
///   whatever its name — precise, and independent of the prefix rule.
///
/// The `stage_sets` row itself is deliberately left alone — not deleted,
/// not re-statused. Leaving it `status='staging'` is exactly what lets
/// `recover_orphaned_sessions` (`src/db/mod.rs`) mark it `'failed'` on the
/// next `db::open()`, which is the operator's only signal that this stage
/// attempt didn't complete.
fn cleanup_failed_stage_set(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    stage_set_id: i64,
) {
    let staging_dir = Path::new(&config.staging.directory);

    // Resolve this stage set's archive_base prefix via the SAME
    // `archive_base_name`/`archive_base_prefix` helpers `stage_create_inner`
    // used to build it — the lockstep issue #53 requires: this is not a
    // parallel re-derivation, it's the identical computation.
    let (prefix, uuid) = match conn
        .query_row(
            "SELECT u.uuid, sn.version
             FROM stage_sets ss
             JOIN snapshots sn ON sn.id = ss.snapshot_id
             JOIN units u ON u.id = sn.unit_id
             WHERE ss.id = ?1",
            params![stage_set_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .ok()
    {
        Some((uuid, version)) => (archive_base_prefix(&uuid, version, stage_set_id), uuid),
        None => {
            tracing::warn!(
                stage_set_id,
                "cleanup: could not resolve stage set, skipping"
            );
            return;
        }
    };

    let mut removed = 0u64;

    // Prefix-keyed (see the doc comment above).
    if let Ok(entries) = fs::read_dir(staging_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix)
                && name.ends_with(files::SLICE_SUFFIX)
                && fs::remove_file(entry.path()).is_ok()
            {
                removed += 1;
            }
        }
    }

    // Encrypted .age — DB-row-keyed to this stage_set_id only (see doc
    // comment above); never a prefix scan.
    let age_paths: Vec<String> = match conn
        .prepare("SELECT staging_path FROM stage_slices WHERE stage_set_id = ?1")
        .and_then(|mut stmt| {
            stmt.query_map(params![stage_set_id], |row| row.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()
        }) {
        Ok(paths) => paths,
        Err(e) => {
            tracing::warn!(stage_set_id, error = %e, "cleanup: could not list stage_slices");
            Vec::new()
        }
    };
    for p in &age_paths {
        if fs::remove_file(p).is_ok() {
            removed += 1;
        }
    }

    if let Err(e) = conn.execute(
        "DELETE FROM stage_slices WHERE stage_set_id = ?1",
        params![stage_set_id],
    ) {
        tracing::warn!(stage_set_id, error = %e, "cleanup: could not delete stage_slices rows");
    }

    // Issue #419: a Layout 2 stage set's own isolated catalogue (and its
    // on-the-fly one, if the run died before it was re-isolated). Nothing
    // else uses the directory: it is per stage set.
    let catalogue_dir = stage_set_catalogue_dir(paths, &uuid, stage_set_id);
    if catalogue_dir.exists() {
        match fs::remove_dir_all(&catalogue_dir) {
            Ok(()) => removed += 1,
            Err(e) => tracing::warn!(
                stage_set_id,
                path = %catalogue_dir.display(),
                error = %e,
                "cleanup: could not remove the stage set's catalogue directory"
            ),
        }
    }

    tracing::warn!(
        stage_set_id,
        files_removed = removed,
        "stage_create failed — removed orphaned staging files for this stage set; \
         the stage_sets row itself was left as status='staging' so the next \
         `db::open()` sweep marks it 'failed'"
    );
}

/// Write a stage report to `paths.stage_reports_dir` (creating it if
/// needed) and return the path written. Called a "receipt" until issue
/// #361 — that word now means only the recipient list a stage set was
/// encrypted to (CONTEXT.md).
///
/// Issue #41: stage reports hold the same plaintext content-metadata index
/// `tapectl.db` does (unit/tenant names, paths, sizes, checksums) — they
/// get `write_private_file`'s 0600-from-creation treatment instead of a
/// plain `fs::write` at whatever mode the process umask hands out.
/// Factored out of `stage_create` so it's testable without a real tar
/// binary or a full stage pipeline.
///
/// The name is `<YYYYMMDD>_<stage set id>.txt`. A report is never written over
/// another (issue #465): if that name is taken — an id reused before ids were
/// kept rising, or a restored catalog — the new report is `…_<id>-2.txt`, and
/// so on, and the older one keeps its bytes.
fn write_stage_report(paths: &TapectlPaths, stage_set_id: i64, report: &str) -> Result<PathBuf> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    fs::create_dir_all(&paths.stage_reports_dir)?;
    let stem = format!("{}_{}", chrono::Utc::now().format("%Y%m%d"), stage_set_id);
    for attempt in 1..=1000u32 {
        let name = if attempt == 1 {
            format!("{stem}.txt")
        } else {
            format!("{stem}-{attempt}.txt")
        };
        let report_path = paths.stage_reports_dir.join(name);
        // Created 0600 from the first `open()` (issue #41), and never over an
        // existing file.
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&report_path)
        {
            Ok(mut file) => {
                file.write_all(report.as_bytes())?;
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
                return Ok(report_path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(TapectlError::Other(format!(
        "cannot name a stage report for stage set {stage_set_id} in {}: 1000 reports of that \
         name and day exist",
        paths.stage_reports_dir.display()
    )))
}

/// Refuse to stage a unit bigger than the staging area (ADR-0012 items 85
/// and 288), before the `stage_sets` INSERT and before tar: such a unit could
/// be staged, hours of tar and age, and then never written. Every unit may
/// continue on the next cartridge at the early warning (item 288), so the size
/// of a tape no longer limits one (the refusal of a unit above one tape, issue
/// #395, is gone with the opt-in).
///
/// A unit is staged whole before its first tape is written, and its slices are
/// released only as each tape gives them their copies (items 85 and 129), so
/// it must fit the staging area: one larger than the staging filesystem's
/// TOTAL size (`f_blocks` x `f_frsize`, coordinator ruling X14) could never be
/// staged, and is refused by name. Free space is the pre-stage space check's
/// concern ([`check_staging_space`]), not this one's.
///
/// The size is the snapshot's own `total_size` (the walk's sum of file
/// sizes), the same figure `collection plan` sizes a unit by; it is checked
/// before compression and errs toward refusing (2026-10-07 item 8).
fn check_unit_fits_staging(
    config: &Config,
    unit_name: &str,
    snapshot: &models::Snapshot,
    notices: &mut dyn Write,
) -> Result<()> {
    let Some(size) = snapshot.total_size.map(|s| s.max(0) as u64) else {
        return Ok(());
    };
    use crate::util::format_bytes_binary as fmt;
    let staging_dir = Path::new(&config.staging.directory);
    let total = match staging_total_bytes(staging_dir) {
        Ok(total) => total,
        Err(e) => {
            let _ = writeln!(
                notices,
                "warning: could not read the size of staging directory {} ({e}); staging \
                 unit \"{unit_name}\" without checking that it fits the staging area",
                staging_dir.display()
            );
            return Ok(());
        }
    };
    if size <= total {
        return Ok(());
    }
    let as_i64 = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    Err(TapectlError::Other(format!(
        "unit \"{unit_name}\" is {size} bytes ({}), larger than the staging area: the \
         filesystem of staging directory {} is {total} bytes ({}) in all. A unit is staged \
         whole before its first tape is written, and its slices are released only as each \
         tape gives them their copies (ADR-0012 items 85 and 129), so it cannot be staged \
         here; nothing was staged. It can be once the staging area is larger than the unit: \
         grow that filesystem, or point `[staging] directory` in config.toml at a larger one.",
        fmt(as_i64(size)),
        staging_dir.display(),
        fmt(as_i64(total)),
    )))
}

/// Refuse to stage a snapshot whose file list is short (issue #374).
///
/// `snapshots.file_count` is the walk's own count of non-directory entries,
/// written in the same statement as the snapshot row; the `files` rows are
/// what `catalog search`, `locate` and the tape's `catalog.db` are built
/// from. Since #374 both land in one transaction, but a snapshot minted
/// before that could have committed its row and then lost the rest of its
/// file list to a Ctrl-C or a busy catalog, and nothing detected it: the archiver
/// would archive the files anyway, and the tape's catalog would carry the
/// gap. A NULL `file_count` (never written by `snapshot create`) is not a
/// count to compare, so it passes.
fn check_file_list_complete(
    conn: &Connection,
    snapshot: &models::Snapshot,
    unit_name: &str,
) -> Result<()> {
    let Some(expected) = snapshot.file_count else {
        return Ok(());
    };
    let recorded: i64 = conn.query_row(
        "SELECT COUNT(*) FROM file_versions WHERE snapshot_id = ?1 AND kind <> 0",
        params![snapshot.id],
        |r| r.get(0),
    )?;
    if recorded != expected {
        return Err(TapectlError::Other(format!(
            "snapshot {unit_name} v{} is incomplete: it records {expected} file(s) but the \
             catalog holds {recorded} file row(s) for it, so an earlier `snapshot create` \
             was interrupted partway. Staging it would put a short file list on tape. \
             Recover with `tapectl snapshot delete {unit_name} --version {}`, then \
             `tapectl snapshot create {unit_name}`",
            snapshot.version, snapshot.version
        )));
    }
    Ok(())
}

fn get_snapshot(conn: &Connection, id: i64) -> Result<models::Snapshot> {
    conn.query_row(
        "SELECT id, unit_id, version, snapshot_type, base_snapshot_id, status,
                source_path, total_size, file_count, created_at, superseded_at, notes
         FROM snapshots WHERE id = ?1",
        params![id],
        |row| {
            Ok(models::Snapshot {
                id: row.get(0)?,
                unit_id: row.get(1)?,
                version: row.get(2)?,
                snapshot_type: row.get(3)?,
                base_snapshot_id: row.get(4)?,
                status: row.get(5)?,
                source_path: row.get(6)?,
                total_size: row.get(7)?,
                file_count: row.get(8)?,
                created_at: row.get(9)?,
                superseded_at: row.get(10)?,
                notes: row.get(11)?,
            })
        },
    )
    .map_err(|_| TapectlError::Other(format!("snapshot {id} not found")))
}

fn get_unit_for_snapshot(conn: &Connection, snapshot: &models::Snapshot) -> Result<models::Unit> {
    queries::get_unit_by_name(conn, &{
        let name: String = conn.query_row(
            "SELECT name FROM units WHERE id = ?1",
            params![snapshot.unit_id],
            |row| row.get(0),
        )?;
        name
    })?
    .ok_or_else(|| TapectlError::Other("unit not found".into()))
}

/// An io failure on a staging path, as an error that names the operation
/// and the path (issue #354).
///
/// A bare `?` on an `io::Error` becomes `TapectlError::Io`, whose `#[from]`
/// makes the io error its `source()` as well as its Display — and
/// `error::exit_with_error` prints the whole chain (`{:#}`), so the operator
/// saw `Permission denied (os error 13): Permission denied (os error 13)`:
/// the same text twice and no path. `Other` carries no source, so the io
/// text appears exactly once, after the path it happened to.
fn staging_io_error(operation: &str, path: &Path, e: std::io::Error) -> TapectlError {
    TapectlError::Other(format!("{operation} {}: {e}", path.display()))
}

/// The space estimate's figures for each entry of the stream (T4.5,
/// determined; `volume-format-v3.md` §3.4): its headers, and its extension
/// headers when it has an ACL or an extended attribute. Measured: about
/// 1,951 bytes of stream for a small file, more with a long name or an ACL.
const ENTRY_HEADER_BYTES: u64 = 2560;
const ENTRY_XATTR_BYTES: u64 = 4096;
/// And for each slice the estimate comes to.
const SLICE_OVERHEAD_BYTES: u64 = 1024;
/// What another stage in flight is held to for each of its files, when only
/// its snapshot's counts are at hand: an entry's headers and a path of up to
/// 256 bytes (twice that, rounded up to 512).
const IN_FLIGHT_ENTRY_BYTES: i64 = (ENTRY_HEADER_BYTES + 512) as i64;

/// `n` rounded up to 512.
fn round_up_512(n: u64) -> u64 {
    n.div_ceil(512) * 512
}

/// The space a staging is expected to write before tar runs (T4.5,
/// determined): the files' bytes, then for each entry 2,560 bytes plus twice
/// the length of its path rounded up to 512, plus 4,096 for an entry that
/// has an ACL or an extended attribute, then 1,024 for each slice the
/// estimate comes to. It never counts on compression to make room: bytes
/// that do not shrink grow a little, so a packed staging is held to its
/// algorithm's worst growth for each slice too. The space accounting while
/// the stage runs still catches a stream that outgrows it.
fn stream_estimate(
    conn: &Connection,
    snapshot_id: i64,
    root: &str,
    slice_size: u64,
    packing: Option<pack::Packing>,
) -> Result<i64> {
    use std::os::unix::ffi::OsStrExt;
    let mut stmt = conn.prepare(&format!(
        "SELECT p.path, fv.kind, fv.size_bytes FROM {} WHERE fv.snapshot_id = ?1",
        crate::db::files::VERSION_FILES
    ))?;
    let rows = stmt.query_map(params![snapshot_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, crate::db::files::FileKind>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    let root = Path::new(root);
    let mut bytes: u64 = 0;
    for row in rows {
        let (path, kind, size) = row?;
        if kind == crate::db::files::FileKind::Regular {
            bytes = bytes.saturating_add(size.max(0) as u64);
        }
        bytes = bytes
            .saturating_add(ENTRY_HEADER_BYTES)
            .saturating_add(round_up_512(2 * path.len() as u64));
        let full = std::ffi::CString::new(
            root.join(
                crate::volume::index::unescape_os(&path).unwrap_or_else(|_| path.clone().into()),
            )
            .as_os_str()
            .as_bytes(),
        );
        // SAFETY: a NUL-terminated string; a null buffer of length 0 asks
        // only for the attribute list's size.
        let has_xattr = full.is_ok_and(|c| unsafe {
            nix::libc::llistxattr(c.as_ptr(), std::ptr::null_mut(), 0) > 0
        });
        if has_xattr {
            bytes = bytes.saturating_add(ENTRY_XATTR_BYTES);
        }
    }
    let slices = bytes.div_ceil(slice_size.max(1)).max(1);
    bytes = bytes.saturating_add(slices.saturating_mul(SLICE_OVERHEAD_BYTES));
    if packing.is_some() {
        bytes = bytes.saturating_add(slices.saturating_mul(pack::worst_growth(slice_size)));
    }
    Ok(StagingSpaceInputs::peak(
        i64::try_from(bytes).unwrap_or(i64::MAX),
    ))
}

/// What the staging-space check needs to know about the stage it guards.
struct StagingSpaceInputs<'a> {
    staging_dir: &'a Path,
    unit_name: &'a str,
    /// The most the stage is expected to write ([`stream_estimate`]).
    estimate: i64,
    /// The resolved compression (`"none"` or an algorithm).
    compression: &'a str,
    /// Tier-2 consent given in advance (the global `--yes`) for a stage
    /// that may not fit (issue #354).
    assume_yes: bool,
    /// What the other stages running now may still write (issue #368).
    in_flight: InFlight,
}

/// The other stages running now, and the most they may still write to
/// staging (issue #368): each one's upper bound less the slices it has
/// already written. Without it, stages started together each saw the whole
/// free space as theirs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct InFlight {
    stages: usize,
    bytes: i64,
}

/// The most another stage in flight, of a snapshot of `total_size` bytes in
/// `file_count` files, is taken to write to staging, from its snapshot's
/// counts alone.
fn stage_upper_bound(total_size: Option<i64>, file_count: Option<i64>) -> i64 {
    StagingSpaceInputs::peak(
        total_size.unwrap_or(0).saturating_add(
            file_count
                .unwrap_or(0)
                .saturating_mul(IN_FLIGHT_ENTRY_BYTES),
        ),
    )
}

/// Every stage set being staged by a live process right now (its lock is
/// held; issue #98), and what it may still write (issue #368).
fn in_flight_staging(conn: &Connection, db_file: &Path) -> Result<InFlight> {
    let rows: Vec<(i64, Option<i64>, Option<i64>, i64)> = conn
        .prepare(
            "SELECT ss.id, sn.total_size, sn.file_count,
                    (SELECT COALESCE(SUM(sl.encrypted_bytes), 0) FROM stage_slices sl
                     WHERE sl.stage_set_id = ss.id)
             FROM stage_sets ss JOIN snapshots sn ON sn.id = ss.snapshot_id
             WHERE ss.status = 'staging'",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut in_flight = InFlight::default();
    for (id, total_size, file_count, written) in rows {
        if lock::is_crashed(db_file, id) {
            continue;
        }
        in_flight.stages += 1;
        in_flight.bytes = in_flight.bytes.saturating_add(
            stage_upper_bound(total_size, file_count)
                .saturating_sub(written)
                .max(0),
        );
    }
    Ok(in_flight)
}

/// Refuse, under the admission lock, to stage a snapshot that a live
/// process is staging right now (issue #368) — or, for an
/// [`Admission::Unstaged`] stage, that is already `staged`. Two stage sets
/// of one snapshot made at once would be the same archive made twice, each
/// with its own catalogue, for one version.
///
/// The `staged` half closes a race. A caller with no `--version` picked a
/// snapshot that had not been staged (`stage create`: the latest `created`
/// one; `collection run`: a unit not yet staged), but that lookup is not
/// under this lock: a sibling `stage create` of the same unit could finish
/// between it and admission, and this stage would make the second copy. A
/// `--version` re-stage ([`Admission::Restage`], issue #53) has already
/// refused a version with live slices and is not rechecked here. A crashed
/// `staging` row is the startup sweep's to fail, not a holder.
fn refuse_a_unit_already_staging(
    conn: &Connection,
    db_file: &Path,
    snapshot: &models::Snapshot,
    unit_name: &str,
    admission: Admission,
) -> Result<()> {
    let sets: Vec<(i64, String)> = conn
        .prepare(
            "SELECT id, status FROM stage_sets
             WHERE snapshot_id = ?1 AND status IN ('staging', 'staged') ORDER BY id",
        )?
        .query_map(params![snapshot.id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (id, status) in sets {
        if status == "staged" && admission == Admission::Unstaged {
            return Err(TapectlError::Other(format!(
                "unit \"{unit_name}\" v{} is already staged (stage set {id}, most likely by \
                 another `stage create` that finished while this one waited) — refusing to \
                 stage it twice; nothing was recorded",
                snapshot.version
            )));
        }
        if status == "staging" && !lock::is_crashed(db_file, id) {
            return Err(TapectlError::Other(format!(
                "unit \"{unit_name}\" v{} is being staged right now (stage set {id}, by \
                 another `stage create`) — refusing to stage it twice at once",
                snapshot.version
            )));
        }
    }
    Ok(())
}

impl StagingSpaceInputs<'_> {
    /// Staging's peak for `stream` bytes of slices: the slices themselves,
    /// as ciphertext — only `.age` slices are written there (issue #370),
    /// each about 1/4096 longer than its plaintext (age's 16-byte tag per 64
    /// KiB chunk), plus its header.
    fn peak(stream: i64) -> i64 {
        stream.saturating_add(stream / 4096).saturating_add(4096)
    }

    fn free(&self, notices: &mut dyn Write) -> Option<i64> {
        match staging_free_bytes(self.staging_dir) {
            Ok(free) => Some(i64::try_from(free).unwrap_or(i64::MAX)),
            Err(e) => {
                let _ = writeln!(
                    notices,
                    "warning: could not read the free space of staging directory {} ({e}); \
                     staging unit \"{}\" without a space check",
                    self.staging_dir.display(),
                    self.unit_name
                );
                None
            }
        }
    }
}

/// The staging-space check (issue #354), against the upper bound only
/// (issue #364).
///
/// - free space covers the upper bound: nothing to say;
/// - otherwise the stage may or may not fit — a hard-linked file is stored
///   once, and a packed staging can shrink — so the operator is ASKED, with
///   the figures ([`ask_to_stage_anyway`]: ADR-0008 Tier 2; `--yes`
///   proceeds, a non-interactive run without it refuses). Never a hard
///   refusal.
///
/// The source is read once, by tar; there is no pass before it to count the
/// unit's bytes. A stage that goes ahead and runs out of space stops
/// cleanly.
fn check_staging_space(inputs: &StagingSpaceInputs, notices: &mut dyn Write) -> Result<()> {
    use crate::util::format_bytes_binary as fmt;

    let Some(free) = inputs.free(notices) else {
        return Ok(());
    };
    let upper = inputs.estimate;
    // Issue #368: what the stages running beside this one may still write
    // is not free for this one.
    let spoken_for = inputs.in_flight.bytes;
    if free.saturating_sub(spoken_for) >= upper {
        return Ok(());
    }
    let in_flight = if inputs.in_flight.stages > 0 {
        format!(
            " ({} of it may still be written by {} running now)",
            fmt(spoken_for),
            crate::render::plural(
                inputs.in_flight.stages as i64,
                "other stage",
                "other stages"
            )
        )
    } else {
        String::new()
    };
    let how = if inputs.compression == "none" {
        "a hard-linked file is stored once".to_string()
    } else {
        format!(
            "a hard-linked file is stored once, or its data may compress (compression = \"{}\")",
            inputs.compression
        )
    };
    ask_to_stage_anyway(
        inputs,
        format!(
            "staging directory {} may be too small for unit \"{}\": {} free{in_flight}, and \
             staging it needs up to {}, its encrypted slices at the snapshot's full size and \
             tar's headers (less if {how}); {STAGING_RUNS_OUT}",
            inputs.staging_dir.display(),
            inputs.unit_name,
            fmt(free),
            fmt(upper),
        ),
        notices,
    )
}

/// Ask whether to stage a unit that may not fit (issue #354, criterion
/// (b): "refused, or asked about"). It is ADR-0008 Tier 2 — the need is
/// uncertain, not proven short, so the operator may knowingly go ahead —
/// and goes through the one consent gate, `cli::consent::confirm`: a
/// terminal is shown `fact` and asked; `--yes` proceeds; a non-interactive
/// run without `--yes` refuses, carrying `fact`. `--yes` does not hide the
/// figures: they are written to `notices` before the stage goes on.
fn ask_to_stage_anyway(
    inputs: &StagingSpaceInputs,
    fact: String,
    notices: &mut dyn Write,
) -> Result<()> {
    if inputs.assume_yes {
        let _ = writeln!(notices, "warning: {fact} — staging anyway (--yes given)");
    }
    crate::cli::consent::confirm(
        &format!("stage unit \"{}\"", inputs.unit_name),
        &[fact],
        inputs.assume_yes,
    )
}

/// What happens to a stage that proceeds and then runs out of staging.
const STAGING_RUNS_OUT: &str =
    "if it runs out, the stage stops there and its partial slices are removed. \
     `tapectl stage list` shows what staging holds; a bare `tapectl stage clean` \
     frees what failed stages left and what is already on tape";

#[cfg(test)]
thread_local! {
    /// Test-only: the free space `staging_free_bytes` reports on this
    /// thread, so the space check's refusal can be driven end to end
    /// without filling a real filesystem. `None` = ask the filesystem.
    static STAGING_FREE_OVERRIDE: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

/// Test-only: pretend the staging filesystem has `bytes` free, on this
/// test's thread, until dropped. Crate-visible so that a caller's own tests
/// (`cli::stage`, `collection::batch`, `cli::operations`'s quick-archive)
/// can drive the staging-space consent gate through that caller, proving the
/// global `--yes` reaches it (issue #354) — `stage_create` runs on the
/// calling thread, so the override is seen.
#[cfg(test)]
pub(crate) struct FreeSpaceOverride;
#[cfg(test)]
impl FreeSpaceOverride {
    pub(crate) fn set(bytes: u64) -> Self {
        STAGING_FREE_OVERRIDE.with(|c| c.set(Some(bytes)));
        Self
    }
}
#[cfg(test)]
impl Drop for FreeSpaceOverride {
    fn drop(&mut self) {
        STAGING_FREE_OVERRIDE.with(|c| c.set(None));
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only: the total size `staging_total_bytes` reports on this
    /// thread, so the spanning admission's refusal (ADR-0012 items 85 and
    /// 94) can be driven without a filesystem smaller than a unit. `None` =
    /// ask the filesystem.
    static STAGING_TOTAL_OVERRIDE: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

/// Test-only: pretend the staging filesystem is `bytes` in all, on this
/// test's thread, until dropped ([`FreeSpaceOverride`]'s twin).
#[cfg(test)]
pub(crate) struct TotalSpaceOverride;
#[cfg(test)]
impl TotalSpaceOverride {
    pub(crate) fn set(bytes: u64) -> Self {
        STAGING_TOTAL_OVERRIDE.with(|c| c.set(Some(bytes)));
        Self
    }
}
#[cfg(test)]
impl Drop for TotalSpaceOverride {
    fn drop(&mut self) {
        STAGING_TOTAL_OVERRIDE.with(|c| c.set(None));
    }
}

/// The size of the filesystem staging is on, in all: `f_blocks` in units of
/// `f_frsize`, as [`staging_free_bytes`] counts free space (#478, ruling
/// X14: "larger than the staging area" is this, not the free space). The
/// size check runs before `stage create` creates the staging directory, so
/// it asks of the nearest directory that exists: the one `dir` will be
/// created in.
pub(crate) fn staging_total_bytes(dir: &Path) -> std::result::Result<u64, nix::Error> {
    #[cfg(test)]
    if let Some(total) = STAGING_TOTAL_OVERRIDE.with(|c| c.get()) {
        return Ok(total);
    }
    let probe = dir.ancestors().find(|p| p.exists()).unwrap_or(dir);
    let stat = nix::sys::statvfs::statvfs(probe)?;
    Ok((stat.blocks() as u64).saturating_mul(stat.fragment_size()))
}

/// Bytes an unprivileged process can still write under `dir`:
/// `f_bavail` (not `f_bfree`, which counts root's reserve) in units of
/// `f_frsize` (POSIX; `f_bsize` is only the preferred I/O size) — the same
/// arithmetic as `config check`'s staging-space line.
pub(crate) fn staging_free_bytes(dir: &Path) -> std::result::Result<u64, nix::Error> {
    #[cfg(test)]
    if let Some(free) = STAGING_FREE_OVERRIDE.with(|c| c.get()) {
        return Ok(free);
    }
    let stat = nix::sys::statvfs::statvfs(dir)?;
    Ok((stat.blocks_available() as u64).saturating_mul(stat.fragment_size()))
}

/// Build an `age::Encryptor` for the given recipient public keys — shared by
/// `encrypt_data` (small, buffered payloads: envelopes/manifests in
/// `src/volume/build.rs`) and `files::StagingDir::create_slice` (large,
/// streamed slices; H9 fix, issue #35), so the recipient parsing/boxing dance lives
/// in exactly one place. Pure extraction: same errors, same messages, same
/// order of operations as before — `age::Encryptor` doesn't retain any
/// reference into `pubkey_strings` or the intermediate boxed recipients
/// once constructed, so returning it by value is safe.
pub(crate) fn build_encryptor(pubkey_strings: &[String]) -> Result<age::Encryptor> {
    let recipients: Vec<age::x25519::Recipient> = pubkey_strings
        .iter()
        .map(|k| {
            k.parse::<age::x25519::Recipient>()
                .map_err(|e| TapectlError::Encryption(format!("invalid public key: {e}")))
        })
        .collect::<Result<Vec<_>>>()?;

    let recipient_refs: Vec<Box<dyn age::Recipient + Send>> = recipients
        .into_iter()
        .map(|r| Box::new(r) as Box<dyn age::Recipient + Send>)
        .collect();

    age::Encryptor::with_recipients(
        recipient_refs
            .iter()
            .map(|r| r.as_ref() as &dyn age::Recipient),
    )
    .map_err(|e| TapectlError::Encryption(format!("failed to create encryptor: {e}")))
}

/// Whole-buffer age encryption: holds the full plaintext AND full ciphertext
/// in RAM at once. Superseded on all production paths by
/// `files::SliceWriter` (slices, H9/#35 and #370) and `volume::build`'s streaming
/// envelope path (H9 residual, #87). Retained as a small, easy-to-audit
/// reference implementation for tests that want a one-shot encrypt/decrypt
/// round trip without standing up a file-backed streaming pipeline —
/// including several integration-test crates (`tests/tenant_isolation.rs`,
/// `tests/format_v2.rs`, `tests/failure_modes.rs`, `tests/integration.rs`)
/// that call it as `tapectl::staging::encrypt_data`.
///
/// NOT `#[cfg(test)]`-gated: `cfg(test)` only applies when this crate is
/// compiled in test mode for its own unit tests (`cargo test --lib`) — it
/// does not propagate to the integration-test binaries above, which link
/// the normally-built library. Gating this behind `cfg(test)` would make it
/// disappear for exactly those external callers and break the build; see
/// the issue #87 final report for the full account. The actual negative
/// control for the H9 residual this retirement was meant to guard against
/// is structural instead: `volume::build`'s envelope call sites
/// (`materialize_envelope_streaming`) no longer call this function at all,
/// so a regression toward whole-object envelope buffering would have to
/// reintroduce a call here, which is visible in review/diff even without a
/// compiler gate.
pub fn encrypt_data(data: &[u8], pubkey_strings: &[String]) -> Result<Vec<u8>> {
    let encryptor = build_encryptor(pubkey_strings)?;

    let mut encrypted = Vec::new();
    let mut writer = encryptor
        .wrap_output(&mut encrypted)
        .map_err(|e| TapectlError::Encryption(format!("wrap_output failed: {e}")))?;
    writer
        .write_all(data)
        .map_err(|e| TapectlError::Encryption(format!("write failed: {e}")))?;
    writer
        .finish()
        .map_err(|e| TapectlError::Encryption(format!("finish failed: {e}")))?;

    Ok(encrypted)
}

/// The age header [`encrypt_data_with_header_len`] gives a test file to one
/// X25519 recipient when two files must be of one length: 168 bytes (the
/// version line, the X25519 stanza and the MAC) and a 100-byte grease
/// stanza. age 0.11 adds a grease stanza of random size to every header, 13
/// to 190 bytes; each size from 41 to 148 is drawn about 0.7% of the time,
/// the extremes as rarely as 6e-8 (#489).
#[cfg(test)]
pub(crate) const ONE_RECIPIENT_HEADER_LEN: usize = 268;

/// `data` encrypted to `pubkey_strings` as [`encrypt_data`] does, with an age
/// header of exactly `header_len` bytes, for a test that needs two files of
/// one length or of a known difference. age writes its header and nonce when
/// the output is wrapped, before any payload, so a try costs a header and
/// never the payload. Near [`ONE_RECIPIENT_HEADER_LEN`] the bound misses with
/// a probability below e^-70; a search for the length of a file encrypted at
/// random could miss whenever that file drew a rare grease size (#489).
#[cfg(test)]
pub(crate) fn encrypt_data_with_header_len(
    data: &[u8],
    pubkey_strings: &[String],
    header_len: usize,
) -> Vec<u8> {
    use std::cell::RefCell;
    use std::rc::Rc;

    struct Shared(Rc<RefCell<Vec<u8>>>);
    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    const TRIES: usize = 10_000;
    for _ in 0..TRIES {
        let out = Rc::new(RefCell::new(Vec::new()));
        let mut writer = build_encryptor(pubkey_strings)
            .unwrap()
            .wrap_output(Shared(Rc::clone(&out)))
            .unwrap();
        // The header, then the payload's 16-byte nonce.
        if out.borrow().len() != header_len + 16 {
            continue;
        }
        writer.write_all(data).unwrap();
        writer.finish().unwrap();
        return out.take();
    }
    panic!("no age header of {header_len} bytes in {TRIES} tries");
}

/// Sizes and hashes recorded for one staged slice
/// ([`files::SliceWriter::finish`]): the cut's size and sha256 (before
/// packing, `volume-format-v3.md` §3.4), and its `.age` file's. Only the
/// cut's pair is a pure function of the content — age draws a fresh key,
/// nonce and grease stanza for every file.
pub struct EncryptedSliceInfo {
    pub plain_size: i64,
    pub sha256_plain: String,
    pub encrypted_size: i64,
    pub sha256_encrypted: String,
    /// The packed frame's length, for a packed slice.
    pub packed_size: Option<u64>,
}

/// Force `file`'s bytes, then the directory entry that names it, to stable
/// storage (issue #409). The entry matters as much as the bytes: a new file
/// whose data is synced but whose name is not can vanish whole after a crash.
fn make_durable(file: &fs::File, path: &Path) -> Result<()> {
    file.sync_all()
        .map_err(|e| staging_io_error("cannot sync", path, e))?;
    if let Some(dir) = path.parent() {
        fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| staging_io_error("cannot sync directory", dir, e))?;
    }
    #[cfg(test)]
    durability::note(durability::Event::Synced(path.to_path_buf()));
    Ok(())
}

/// Test-only: what the staging pipeline made durable and recorded, in
/// order, on this thread (issue #409) — how a test asserts that every `.age`
/// is synced before its row is written.
#[cfg(test)]
pub(crate) mod durability {
    use std::cell::RefCell;
    use std::path::PathBuf;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Event {
        /// The file's bytes and its directory entry were synced.
        Synced(PathBuf),
        /// The `stage_slices` row naming this `.age` was inserted.
        Recorded(PathBuf),
    }

    thread_local! {
        static LOG: RefCell<Vec<Event>> = const { RefCell::new(Vec::new()) };
    }

    pub(crate) fn note(event: Event) {
        LOG.with(|l| l.borrow_mut().push(event));
    }

    /// Everything recorded on this thread so far, clearing the log.
    pub(crate) fn take() -> Vec<Event> {
        LOG.with(|l| std::mem::take(&mut *l.borrow_mut()))
    }
}

/// Test-only: make `stage_create` fail at a named point on this thread, the
/// way a crash or an io error there would — how a test reaches the cleanup
/// of a stage set that died after tar.
#[cfg(test)]
pub(crate) mod failpoint {
    use std::cell::Cell;

    use crate::error::{Result, TapectlError};

    /// Right after tar has exited and its stream is staged.
    pub(crate) const AFTER_TAR: &str = "after tar";

    /// Just before the stage set is recorded.
    pub(crate) const BEFORE_RECORD: &str = "before the stage set is recorded";

    thread_local! {
        static ARMED: Cell<Option<&'static str>> = const { Cell::new(None) };
    }

    /// Fail the next time `point` is reached on this thread.
    pub(crate) fn arm(point: &'static str) {
        ARMED.with(|a| a.set(Some(point)));
    }

    /// A failure point and what to run when it is reached.
    type Hook = (&'static str, Box<dyn FnOnce()>);

    thread_local! {
        static HOOK: std::cell::RefCell<Option<Hook>> = const { std::cell::RefCell::new(None) };
    }

    /// Run `hook` the next time `point` is reached on this thread.
    pub(crate) fn on(point: &'static str, hook: impl FnOnce() + 'static) {
        HOOK.with(|h| *h.borrow_mut() = Some((point, Box::new(hook))));
    }

    pub(crate) fn hit(point: &'static str) -> Result<()> {
        let hook = HOOK.with(|h| {
            let mut h = h.borrow_mut();
            match h.take() {
                Some((p, f)) if p == point => Some(f),
                other => {
                    *h = other;
                    None
                }
            }
        });
        if let Some(hook) = hook {
            hook();
        }
        if ARMED.with(|a| a.get()) == Some(point) {
            ARMED.with(|a| a.set(None));
            return Err(TapectlError::Other(format!("injected failure: {point}")));
        }
        Ok(())
    }

    thread_local! {
        static STAGING_AFTER_TAR: std::cell::RefCell<Option<Vec<String>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Record the staging directory's file names when tar has exited,
    /// on this thread's next stage.
    pub(crate) fn observe_staging_after_tar() {
        STAGING_AFTER_TAR.with(|o| *o.borrow_mut() = Some(Vec::new()));
    }

    pub(crate) fn note_staging(dir: &std::path::Path) {
        STAGING_AFTER_TAR.with(|o| {
            if let Some(names) = o.borrow_mut().as_mut() {
                let mut found: Vec<String> = std::fs::read_dir(dir)
                    .map(|rd| {
                        rd.flatten()
                            .map(|e| e.file_name().to_string_lossy().into_owned())
                            .collect()
                    })
                    .unwrap_or_default();
                found.sort();
                *names = found;
            }
        });
    }

    /// What [`note_staging`] recorded, clearing it.
    pub(crate) fn staging_after_tar() -> Option<Vec<String>> {
        STAGING_AFTER_TAR.with(|o| o.borrow_mut().take())
    }
}

/// The stage report's "Phase timings" section (issue #386): one line per
/// phase this stage ran, or nothing when no session recorded any.
fn render_phase_timings(timings: &[progress::PhaseTiming]) -> String {
    if timings.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nPhase timings:\n");
    for t in timings {
        out.push_str(&format!(
            "  {}  {}\n",
            t.started_at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            t.summary()
        ));
    }
    out
}

/// Establish the sha256 baseline for every `(path, hash)` pair — but ONLY
/// where `files` doesn't already have one (issue #32/H6).
///
/// The `sha256 IS NULL` guard on the UPDATE is the actual enforcement:
/// it makes this function safe to call on every `stage_create` (including
/// a re-stage of an already-baselined snapshot) regardless of what
/// `checksums` contains, rather than relying on the caller to have
/// filtered it first. `validate_source` already refuses to stage (BITROT)
/// before this point if a hash disagrees with an existing baseline, so in
/// practice every row here either has no baseline yet (this call
/// establishes it) or already matches (a no-op rewrite of the identical
/// value) — but the guard holds even if that invariant is ever violated by
/// a future caller.
///
/// Issue #372: each UPDATE is one lookup in `paths`' UNIQUE(unit_id, path)
/// index and one in `file_versions`' primary key (`db::files::BACKFILL_SQL`,
/// pinned by `backfill_checksums_searches_the_indexes`), so finalization is
/// linear in the file count. A sha256 lives on the membership row, never on
/// the path, so a backfill never touches the search index.
fn backfill_checksums(
    conn: &Connection,
    snapshot_id: i64,
    checksums: &[(String, String)],
) -> Result<()> {
    crate::db::files::backfill_sha256(conn, snapshot_id, checksums)
}

fn generate_stage_report(
    conn: &Connection,
    stage_set_id: i64,
    unit: &models::Unit,
    snapshot: &models::Snapshot,
    tenant: &models::Tenant,
) -> Result<String> {
    let slices: Vec<(i64, i64, i64, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted
             FROM stage_slices WHERE stage_set_id = ?1 ORDER BY slice_number",
        )?;
        let rows = stmt.query_map(params![stage_set_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };

    let mut report = String::new();
    report.push_str("tapectl stage report\n");
    report.push_str("====================\n\n");
    report.push_str(&format!("Unit:     {} ({})\n", unit.name, unit.uuid));
    report.push_str(&format!("Tenant:   {}\n", tenant.name));
    report.push_str(&format!("Snapshot: v{}\n", snapshot.version));
    report.push_str(&format!("Stage:    {stage_set_id}\n"));
    report.push_str(&format!(
        "Date:     {}\n\n",
        chrono::Utc::now().to_rfc3339()
    ));
    report.push_str("Slices:\n");

    for (num, plain, enc, hash_p, hash_e) in &slices {
        report.push_str(&format!(
            "  #{num}: {plain} bytes -> {enc} bytes\n    plain:     {hash_p}\n    encrypted: {hash_e}\n",
        ));
    }

    Ok(report)
}

/// Parse an operator-facing size string (e.g. `"10G"`, `"500M"`, `"512MiB"`,
/// or a bare byte count like `"1024"`) into a byte count.
///
/// Binary throughout: `K`/`KB`=1024 ... `T`/`TB`=1024^4, as these fields have
/// always meant, and the IEC spellings `KiB`, `MiB`, `GiB`, `TiB` mean the
/// same (ADR-0012 item 212: size input accepts KiB to TiB, while a bare or SI
/// suffix keeps its meaning in each field). The suffix is case-insensitive.
/// The capacity fields' sibling, [`crate::media::parse_capacity_to_bytes`],
/// takes the same strings and reads the bare and SI suffixes decimally.
///
/// A bare number with no suffix is valid and means bytes. Anything else that
/// doesn't parse cleanly — an unparseable number, an unrecognized suffix, a
/// negative value, or a magnitude too large to represent — is an ERROR
/// (issue #59): this value feeds `stage_sets.slice_size`, capacity math, and
/// bin-packing, so a silent fallback (0 bytes for a bad number; multiplier-1
/// for a bad suffix, e.g. `"2500GG"` silently becoming 2500 bytes) is worse
/// than a loud rejection.
pub fn parse_size_to_bytes(s: &str) -> Result<i64> {
    let trimmed = s.trim();
    let invalid = || {
        TapectlError::Config(format!(
            "{trimmed:?} is not a valid size: a number with an optional unit, such as \
             2500G, 500M, 512MiB or a bare byte count (K, M, G and T are binary here)"
        ))
    };

    let (num_str, suffix) = trimmed
        .find(|c: char| c.is_alphabetic())
        .map(|i| (&trimmed[..i], &trimmed[i..]))
        .unwrap_or((trimmed, ""));

    let num: f64 = num_str.parse().map_err(|_| invalid())?;
    if num.is_nan() || num < 0.0 {
        return Err(invalid());
    }

    let multiplier: f64 = match suffix.to_uppercase().as_str() {
        "" => 1.0,
        "K" | "KB" | "KIB" => 1024.0,
        "M" | "MB" | "MIB" => 1024.0 * 1024.0,
        "G" | "GB" | "GIB" => 1024.0 * 1024.0 * 1024.0,
        "T" | "TB" | "TIB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return Err(invalid()),
    };

    let bytes = num * multiplier;
    if !bytes.is_finite() || bytes > i64::MAX as f64 {
        return Err(invalid());
    }
    Ok(bytes as i64)
}

#[cfg(test)]
mod parse_size_to_bytes_tests {
    //! Issue #59: `parse_size_to_bytes` used to silently paper over both an
    //! unparseable number (`.unwrap_or(0.0)` -> 0 bytes) and an unknown
    //! suffix (falls through to multiplier 1.0, so `"2500GG"` became 2500
    //! bytes instead of erroring). Both are now `Err`.
    use super::parse_size_to_bytes;

    #[test]
    fn bare_number_means_bytes() {
        assert_eq!(parse_size_to_bytes("1024").unwrap(), 1024);
        assert_eq!(parse_size_to_bytes("0").unwrap(), 0);
    }

    #[test]
    fn every_known_suffix_case_insensitive() {
        assert_eq!(parse_size_to_bytes("2K").unwrap(), 2 * 1024);
        assert_eq!(parse_size_to_bytes("2k").unwrap(), 2 * 1024);
        assert_eq!(parse_size_to_bytes("2KB").unwrap(), 2 * 1024);
        assert_eq!(parse_size_to_bytes("2kb").unwrap(), 2 * 1024);
        assert_eq!(parse_size_to_bytes("2M").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_size_to_bytes("2MB").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_size_to_bytes("2G").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size_to_bytes("2GB").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(
            parse_size_to_bytes("2T").unwrap(),
            2 * 1024 * 1024 * 1024 * 1024
        );
        assert_eq!(
            parse_size_to_bytes("2TB").unwrap(),
            2 * 1024 * 1024 * 1024 * 1024
        );
    }

    /// ADR-0012 item 212: the IEC spellings are accepted, in any case, and
    /// mean what the bare and SI suffixes already mean here (binary).
    #[test]
    fn iec_suffixes_are_binary_and_case_insensitive() {
        for (s, bytes) in [
            ("2KiB", 2i64 << 10),
            ("2kib", 2 << 10),
            ("512MiB", 512 << 20),
            ("512MIB", 512 << 20),
            ("1GiB", 1 << 30),
            ("1.5GiB", 3 << 29),
            ("2TiB", 2 << 40),
            ("2tib", 2 << 40),
        ] {
            assert_eq!(parse_size_to_bytes(s).unwrap(), bytes, "{s}");
        }
        // The field's own meaning of the bare and SI suffixes is unchanged.
        assert_eq!(
            parse_size_to_bytes("1G").unwrap(),
            parse_size_to_bytes("1GiB").unwrap()
        );
        assert_eq!(parse_size_to_bytes("99008M").unwrap(), 99_008 * 1024 * 1024);
        assert!(parse_size_to_bytes("1GiBB").is_err());
        assert!(parse_size_to_bytes("1Gi").is_err());
    }

    #[test]
    fn unknown_suffix_is_an_error() {
        let err = parse_size_to_bytes("2500GG").unwrap_err().to_string();
        assert!(err.contains("2500GG"), "error must name the value: {err}");
        assert!(
            err.contains("2500G") || err.to_lowercase().contains("valid size"),
            "error must show accepted forms: {err}"
        );

        assert!(parse_size_to_bytes("5X").is_err());
    }

    #[test]
    fn unknown_suffix_does_not_silently_become_bytes() {
        // The historic defect: "2500GG" silently became 2500 bytes via
        // multiplier 1.0. It must now be an error, never 2500.
        assert!(parse_size_to_bytes("2500GG").is_err());
    }

    #[test]
    fn unparseable_number_is_an_error() {
        assert!(parse_size_to_bytes("").is_err());
        assert!(parse_size_to_bytes("abc").is_err());
        assert!(parse_size_to_bytes("1.2.3").is_err());
    }

    #[test]
    fn negative_value_is_an_error() {
        assert!(parse_size_to_bytes("-5G").is_err());
        assert!(parse_size_to_bytes("-1").is_err());
    }

    #[test]
    fn absurd_magnitude_errors_rather_than_producing_garbage() {
        // Historically `(num * multiplier) as i64` on an out-of-range f64
        // silently saturates/truncates. Must error deliberately instead.
        assert!(parse_size_to_bytes("99999999999999999999G").is_err());
    }

    #[test]
    fn whitespace_is_trimmed() {
        assert_eq!(
            parse_size_to_bytes("  10G  ").unwrap(),
            10 * 1024 * 1024 * 1024
        );
    }
}

/// Walk a directory and collect manifest entries. `global_excludes` is
/// `config.defaults.global_excludes` (issue #49 item 5) — combined with the
/// unit's own dotfile `[excludes] patterns` (read internally, keyed off
/// `path`, exactly as before) via `exclude::effective_compiled`, the single
/// place both halves are merged so this walk and
/// `collection::fingerprint::walk_fingerprint` can never independently
/// disagree about the effective set.
fn walk_directory(
    path: &str,
    global_excludes: &[String],
) -> Result<(i64, i64, Vec<ManifestEntry>)> {
    let walk = walk_directory_reporting(path, global_excludes)?;
    Ok((walk.total_size, walk.file_count, walk.entries))
}

/// What one walk of a unit found, and what it left out (#542).
struct Walk {
    total_size: i64,
    file_count: i64,
    entries: Vec<ManifestEntry>,
    /// Everything a pattern or the socket rule kept out, by path.
    left_out: Vec<crate::db::snapshot_left_out::LeftOut>,
}

/// What lies under the folder `dir`, for the report of a folder a pattern
/// left out: every entry that is not a directory, and the bytes of the
/// regular files among them. Never follows a link; an entry it cannot read
/// counts for nothing.
fn count_under(dir: &Path) -> (i64, i64) {
    let (mut files, mut bytes) = (0i64, 0i64);
    for e in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if e.file_type().is_dir() {
            continue;
        }
        files += 1;
        if e.file_type().is_file() {
            bytes += e.metadata().map(|m| m.len() as i64).unwrap_or(0);
        }
    }
    (files, bytes)
}

/// [`walk_directory`], with what the walk left out (ADR-0012 items 252 and
/// 270, #542): each file an exclusion pattern kept out, each folder one
/// pruned (with what was under it; the folder's own entry stays, empty),
/// and each socket. Sockets are left out of every unit, pattern or none.
fn walk_directory_reporting(path: &str, global_excludes: &[String]) -> Result<Walk> {
    use crate::db::snapshot_left_out::{LeftOut, Reason};
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use walkdir::WalkDir;

    let base = Path::new(path);
    // Issue #49 items 3+5: global excludes + the unit's own dotfile exclude
    // patterns, compiled once per walk (not per entry). A plain pattern never
    // matches a directory; a directory pattern (`name/`, issue #359) prunes
    // the subtree below `name`. tar is given what this walk records (D14).
    let exclude_compiled = exclude::effective_compiled(base, global_excludes)?;
    let mut entries = Vec::new();
    let mut total_size: i64 = 0;
    let mut file_count: i64 = 0;
    let mut left_out: Vec<LeftOut> = Vec::new();

    // Issue #359 (c): a directory INSIDE a pruned subtree is neither
    // recorded nor descended — the pruned directory itself is kept, empty,
    // and nothing below it. The pruned directory's own entry
    // passes (`excludes_dir_entry` keeps it) and its non-directory children
    // are dropped by `is_excluded` below. `file_type()` here is the lstat
    // type (`follow_links(false)`), the same fact `is_dir` is below.
    let walker = WalkDir::new(base)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            !(e.file_type().is_dir() && exclude_compiled.excludes_dir_entry(e.path()))
        });
    for entry in walker {
        let entry = entry.map_err(|e| TapectlError::Other(e.to_string()))?;
        let rel = entry.path().strip_prefix(base).unwrap_or(entry.path());

        if rel.as_os_str().is_empty() {
            continue; // skip root
        }

        let meta = entry
            .metadata()
            .map_err(|e| TapectlError::Other(e.to_string()))?;
        let is_dir = meta.is_dir();

        // #542: a folder a directory pattern prunes keeps its own entry,
        // empty (below); what was under it is reported, with the pattern.
        if is_dir {
            if let Some(pattern) = exclude_compiled.dir_self_pattern(entry.path()) {
                let (files, bytes) = count_under(entry.path());
                left_out.push(LeftOut {
                    reason: Reason::Pattern,
                    pattern: Some(pattern.to_string()),
                    path: crate::volume::index::escape_os(rel.as_os_str()).into_owned(),
                    folder: true,
                    files,
                    bytes,
                });
            }
        }

        // Issue #49: a non-directory entry matching an exclude pattern is
        // dropped before any further work (symlink-target read, manifest
        // row) — it is never archived (tar is given what this walk records,
        // D14), so the manifest/files table must not record it either. Checked
        // before the file_type classification below so an excluded entry
        // costs nothing beyond the pattern match.
        if !is_dir {
            // Inside a folder a directory pattern pruned: the folder was
            // reported once, with all that lies under it.
            if exclude_compiled.excludes_dir_entry(entry.path()) {
                continue;
            }
            // #542: said, with the pattern, instead of dropped without a
            // word. A pattern is the operator's rule and is named before the
            // socket rule (item 270), which holds for every unit.
            let reported = if let Some(pattern) = exclude_compiled.file_pattern(entry.path()) {
                Some((Reason::Pattern, Some(pattern.to_string())))
            } else if entry.file_type().is_socket() {
                Some((Reason::Socket, None))
            } else {
                None
            };
            if let Some((reason, pattern)) = reported {
                left_out.push(LeftOut {
                    reason,
                    pattern,
                    path: crate::volume::index::escape_os(rel.as_os_str()).into_owned(),
                    folder: false,
                    files: 1,
                    bytes: if meta.is_file() { meta.len() as i64 } else { 0 },
                });
                continue;
            }
        }
        // A name is bytes (ADR-0012 item 297, #569): it is recorded in the
        // escaped form of `volume-format-v3.md` §1.5, the one the catalog, the
        // file table and the index hold, and nothing converts it lossily. A
        // name that is UTF-8 with no backslash, TAB, LF or CR is recorded as
        // it is.
        let rel_path = crate::volume::index::escape_os(rel.as_os_str()).into_owned();
        // lstat's own size for the entry — for a symlink this is the length
        // of the *target path string*, not any real content size. Kept
        // as-is (not zeroed for a symlink/special below): it's what
        // `collection::fingerprint`'s independent walk also computes from
        // the same never-follow metadata, and zeroing it here would make
        // that fingerprint comparison disagree with what's recorded,
        // falsely flagging every symlink-containing unit as perpetually
        // dirty. Only `total_size` below excludes it (see that comment).
        let size = if is_dir { 0 } else { meta.len() as i64 };
        // Whole seconds, as the walk has always recorded them (and as
        // `collection::fingerprint`'s walk does), so a version recorded
        // before migration 030 and a fresh walk still compare equal.
        let mtime_ns = crate::db::files::mtime_ns_from_secs(meta.mtime());

        // Classify by filesystem type (issue #33/H7) via entry.file_type(),
        // which — under WalkDir::follow_links(false), already set above —
        // reflects symlink_metadata (never follows), matching the size/mtime
        // computation above. This is the fact the validator
        // (staging/validate.rs) filters its content-validation set on: only
        // 'regular' files get a size check + sha256; symlinks and special
        // files (FIFO, socket, block/char device) are recorded here but
        // never opened or hashed.
        let ft = entry.file_type();
        let (file_type, link_target): (&'static str, Option<String>) = if is_dir {
            ("dir", None)
        } else if ft.is_symlink() {
            // Never followed: the target may not exist (a broken symlink
            // is still recorded, not an error) and reading the link
            // itself — unlike opening a FIFO — never risks blocking.
            let target = fs::read_link(entry.path()).map_err(|e| {
                TapectlError::Other(format!("cannot read symlink target: {rel_path} ({e})"))
            })?;
            (
                "symlink",
                Some(crate::volume::index::escape_os(target.as_os_str()).into_owned()),
            )
        } else if ft.is_file() {
            ("regular", None)
        } else {
            // FIFO, socket, block/char device: recorded (so restore and
            // `catalog list` still see them) but never content-validated —
            // opening a FIFO with no writer via File::open blocks forever
            // with no timeout, and none of these has "content" to
            // checksum in the first place.
            tracing::warn!(
                path = %rel_path,
                "special file (FIFO/socket/device) recorded but not content-validated"
            );
            ("special", None)
        };

        if !is_dir {
            // Every non-directory entry counts toward file_count, same as
            // before this fix (tar archives a symlink or special file as a
            // member of its own even though it carries no
            // content payload).
            file_count += 1;
            // Only real content bytes count as archival payload — a
            // symlink's "size" (from lstat, above) is the length of its
            // target *string*, not payload, and a special file has no
            // payload at all (issue #33/H7).
            if file_type == "regular" {
                total_size += size;
            }
        }

        entries.push(ManifestEntry {
            path: rel_path,
            size,
            mtime_ns,
            is_dir,
            file_type,
            link_target,
        });
    }

    left_out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Walk {
        total_size,
        file_count,
        entries,
        left_out,
    })
}

/// Test-only cross-module seam (issue #49): the relative, non-directory
/// paths `walk_directory` enumerates for `path`, without exposing
/// `ManifestEntry`'s internal shape outside this module. Used by
/// `collection::fingerprint`'s anti-regression test proving `walk_directory`
/// and `walk_fingerprint` enumerate an identical relative-path set for the
/// same exclude configuration — the property that keeps the two
/// independent `WalkDir`-based walks from silently drifting apart again
/// (issues #33/#36/#48 each hit exactly this failure shape once already).
#[cfg(test)]
pub(crate) fn walk_directory_relative_paths_for_test(
    path: &str,
    global_excludes: &[String],
) -> Result<Vec<String>> {
    let (_, _, entries) = walk_directory(path, global_excludes)?;
    Ok(entries
        .into_iter()
        .filter(|e| !e.is_dir)
        .map(|e| e.path)
        .collect())
}

struct ManifestEntry {
    path: String,
    size: i64,
    mtime_ns: Option<i64>,
    is_dir: bool,
    file_type: &'static str,
    link_target: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Register the permanent escrow recipient (ADR-0005) on a throwaway
    /// holder tenant. `stage_create` refuses without one (issue #115), so
    /// this is not fixture decoration — it is the state every real staging
    /// run requires. Public key only, exactly as production does: the secret
    /// half never touches the database. Mirrors `tests/mhvtl_e2e.rs`'s
    /// harness. Returns the escrow public key.
    fn register_test_escrow(conn: &Connection) -> String {
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('escrow-holder', 0, 'active')",
            [],
        )
        .unwrap();
        let holder_id = conn.last_insert_rowid();
        let kp = crate::crypto::keys::generate_keypair();
        queries::insert_escrow_key(
            conn,
            holder_id,
            "test-escrow",
            &kp.fingerprint,
            &kp.public_key,
            Some("test escrow recipient (ADR-0005)"),
        )
        .unwrap();
        kp.public_key
    }

    /// ADR-0012 item 297 (#569): the walk records a name's bytes in the escaped
    /// form (it used to refuse such a unit by name), a symbolic link's target
    /// likewise, and a pattern reads a name that is not UTF-8 by that form: a
    /// glob matches it, and what it leaves out is reported escaped.
    #[test]
    fn walk_directory_records_names_that_are_not_utf8_in_the_escaped_form() {
        use std::os::unix::ffi::OsStrExt;
        let os = std::ffi::OsStr::from_bytes;
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(os(b"caf\xe9.txt")), b"kept").unwrap();
        fs::write(tmp.path().join(os(b"caf\xe9.tmp")), b"junk").unwrap();
        fs::write(tmp.path().join("back\\slash.txt"), b"slash").unwrap();
        fs::create_dir(tmp.path().join(os(b"n\xe9"))).unwrap();
        fs::write(tmp.path().join(os(b"n\xe9")).join("inside"), b"x").unwrap();
        std::os::unix::fs::symlink(os(b"\xfe\xfdtarget"), tmp.path().join(os(b"l\xe9"))).unwrap();

        let walk = walk_directory_reporting(
            tmp.path().to_str().unwrap(),
            &["*.tmp".to_string(), "n*/".to_string()],
        )
        .unwrap();
        let mut paths: Vec<&str> = walk.entries.iter().map(|e| e.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            ["back\\\\slash.txt", "caf\\xe9.txt", "l\\xe9", "n\\xe9"],
            "the escaped forms; the .tmp is out, the pruned folder stays, empty"
        );
        let link = walk.entries.iter().find(|e| e.path == "l\\xe9").unwrap();
        assert_eq!(link.link_target.as_deref(), Some("\\xfe\\xfdtarget"));
        let mut left: Vec<(&str, bool)> = walk
            .left_out
            .iter()
            .map(|l| (l.path.as_str(), l.folder))
            .collect();
        left.sort_unstable();
        assert_eq!(left, [("caf\\xe9.tmp", false), ("n\\xe9", true)]);
    }

    #[test]
    fn walk_directory_records_symlink_file_type_target_and_excludes_it_from_total_size() {
        // Reproduces the mhvtl gate's exact fixture shape: target.txt holds
        // 7 bytes of real content; link-ok's target-string "target.txt" is
        // 10 characters — deliberately different, so a bug that conflates
        // "symlink size" with "content size" shows up immediately in
        // total_size.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("target.txt"), b"target\n").unwrap();
        std::os::unix::fs::symlink("target.txt", tmp.path().join("link-ok")).unwrap();

        let (total_size, file_count, entries) =
            walk_directory(tmp.path().to_str().unwrap(), &[]).unwrap();

        let link_entry = entries
            .iter()
            .find(|e| e.path == "link-ok")
            .expect("link-ok must be recorded in the manifest, not dropped");
        assert_eq!(link_entry.file_type, "symlink");
        assert_eq!(link_entry.link_target.as_deref(), Some("target.txt"));

        // Only target.txt's 7 real content bytes count as archival payload —
        // link-ok's target-string length (10) must never be added, whatever
        // its own recorded `size` field holds (that field stays lstat's raw
        // size — see the commit message for why it isn't zeroed).
        assert_eq!(
            total_size, 7,
            "a symlink's target-string length must not count as payload"
        );

        // Both non-directory entries count toward file_count — tar will
        // archive the symlink as a member of its own even though it
        // carries no content payload (see commit message for the
        // file_count-vs-total_size rationale: this preserves today's
        // `if !is_dir { file_count += 1 }` behavior verbatim; only
        // total_size's accounting changes).
        assert_eq!(file_count, 2);
    }

    #[test]
    fn walk_directory_records_broken_symlink_without_error() {
        // A broken symlink (target does not exist) must still be walked
        // and recorded, never treated as an error — `fs::read_link` reads
        // the symlink's own stored target string and never requires the
        // target to exist.
        let tmp = TempDir::new().unwrap();
        std::os::unix::fs::symlink("does-not-exist.txt", tmp.path().join("dangling")).unwrap();

        let (_, _, entries) = walk_directory(tmp.path().to_str().unwrap(), &[]).unwrap();

        let entry = entries
            .iter()
            .find(|e| e.path == "dangling")
            .expect("a broken symlink must still be walked and recorded, not error out");
        assert_eq!(entry.file_type, "symlink");
        assert_eq!(entry.link_target.as_deref(), Some("does-not-exist.txt"));
    }

    #[test]
    fn walk_directory_classifies_a_fifo_as_special_and_excludes_it_from_total_size() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("regular.txt"), b"real content").unwrap();
        let fifo_path = tmp.path().join("a.fifo");
        nix::unistd::mkfifo(
            &fifo_path,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();

        let (total_size, file_count, entries) =
            walk_directory(tmp.path().to_str().unwrap(), &[]).unwrap();

        let fifo_entry = entries
            .iter()
            .find(|e| e.path == "a.fifo")
            .expect("the FIFO must still be recorded in the manifest, not dropped");
        assert_eq!(fifo_entry.file_type, "special");
        assert_eq!(fifo_entry.link_target, None);

        assert_eq!(
            total_size,
            "real content".len() as i64,
            "the FIFO must not contribute to total_size"
        );
        assert_eq!(file_count, 2, "the FIFO still counts toward file_count");
    }

    // ── issue #542, ADR-0012 items 252 and 270: what a snapshot left out ──

    /// What `snapshot_left_out` holds for `snapshot_id`, in `seq` order:
    /// (reason, pattern, path, kind, files, bytes).
    type LeftOutRow = (String, Option<String>, String, String, i64, i64);

    fn left_out_rows(conn: &Connection, snapshot_id: i64) -> Vec<LeftOutRow> {
        let mut stmt = conn
            .prepare(
                "SELECT reason, pattern, path, kind, files, bytes
                 FROM snapshot_left_out WHERE snapshot_id = ?1 ORDER BY seq",
            )
            .unwrap();
        stmt.query_map(params![snapshot_id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
    }

    #[test]
    fn a_snapshot_reports_a_socket_an_excluded_file_and_an_excluded_folder() {
        let tmp = TempDir::new().unwrap();
        let (conn, _paths, config, src) =
            setup_unit_with_excludes(&tmp, vec!["*.nfo".to_string(), ".cache/".to_string()]);

        fs::write(src.join("keep.txt"), b"kept").unwrap();
        fs::write(src.join("a.nfo"), b"12345").unwrap();
        fs::create_dir_all(src.join(".cache/sub")).unwrap();
        fs::write(src.join(".cache/x"), b"123").unwrap();
        fs::write(src.join(".cache/sub/y"), b"1234").unwrap();
        let _listener = std::os::unix::net::UnixListener::bind(src.join("s.sock")).unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &config).unwrap();

        let mut rows = left_out_rows(&conn, snap_id);
        rows.sort_by(|a, b| a.2.cmp(&b.2));
        assert_eq!(
            rows,
            vec![
                (
                    "pattern".to_string(),
                    Some(".cache/".to_string()),
                    ".cache".to_string(),
                    "folder".to_string(),
                    2,
                    7
                ),
                (
                    "pattern".to_string(),
                    Some("*.nfo".to_string()),
                    "a.nfo".to_string(),
                    "file".to_string(),
                    1,
                    5
                ),
                (
                    "socket".to_string(),
                    None,
                    "s.sock".to_string(),
                    "file".to_string(),
                    1,
                    0
                ),
            ]
        );

        // Left out of the file list too; the pruned folder's own entry stays.
        for gone in ["a.nfo", "s.sock", ".cache/x", ".cache/sub/y"] {
            assert_eq!(
                crate::db::files::fixture::count(&conn, snap_id, Some(gone)),
                0,
                "{gone} must not be in the file list"
            );
        }
        for kept in ["keep.txt", ".cache"] {
            assert_eq!(
                crate::db::files::fixture::count(&conn, snap_id, Some(kept)),
                1,
                "{kept} must be in the file list"
            );
        }
    }

    #[test]
    fn a_socket_is_left_out_and_reported_with_no_pattern_configured() {
        let tmp = TempDir::new().unwrap();
        let (conn, _paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        fs::write(src.join("keep.txt"), b"kept").unwrap();
        fs::create_dir_all(src.join("d")).unwrap();
        let _listener = std::os::unix::net::UnixListener::bind(src.join("d/s.sock")).unwrap();

        let outcome = snapshot_create_detailed(&conn, "unit1", &config).unwrap();
        assert_eq!(
            left_out_rows(&conn, outcome.snapshot_id),
            vec![(
                "socket".to_string(),
                None,
                "d/s.sock".to_string(),
                "file".to_string(),
                1,
                0
            )]
        );
        // keep.txt and the unit's own dotfile; the socket is not counted.
        let count: i64 = conn
            .query_row(
                "SELECT file_count FROM snapshots WHERE id = ?1",
                params![outcome.snapshot_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2, "the socket is not counted");
        assert_eq!(
            crate::db::files::fixture::count(&conn, outcome.snapshot_id, Some("d/s.sock")),
            0
        );
    }

    // ── issue #53: archive_base is per-stage-set, not per-snapshot ──

    #[test]
    fn archive_base_name_differs_for_two_stage_sets_of_the_same_snapshot() {
        // The core anti-collision claim: same unit uuid, same version, two
        // different stage_set_ids must never produce the same file-name
        // stem — otherwise a re-stage of the same snapshot would silently
        // overwrite the first stage set's `.age` slices.
        let a = archive_base_name("abcdef0123456789", 3, 7);
        let b = archive_base_name("abcdef0123456789", 3, 8);
        assert_ne!(a, b);
        assert_eq!(a, "abcdef012345_v3_s7");
        assert_eq!(b, "abcdef012345_v3_s8");
    }

    #[test]
    fn archive_base_prefix_is_the_name_plus_a_trailing_dot_and_stays_lockstep_with_cleanup() {
        // `cleanup_failed_stage_set` derives its plaintext-scan prefix via
        // this exact helper (not a parallel re-derivation) — this test
        // proves the shape stays what the `{base}.{N}.tar.age` naming needs,
        // and that the dot prevents `_s1` from prefix-matching `_s10.1.tar.age`.
        let p1 = archive_base_prefix("abcdef0123456789", 3, 1);
        let p10 = archive_base_prefix("abcdef0123456789", 3, 10);
        assert_eq!(p1, "abcdef012345_v3_s1.");
        assert_eq!(p10, "abcdef012345_v3_s10.");
        assert!(
            !"abcdef012345_v3_s10.1.tar.age".starts_with(&p1),
            "trailing dot must stop _s1 from prefix-matching _s10's files"
        );
    }

    // ── issue #47: stage_create must resolve policy, not read config.defaults directly ──

    /// Negative control / regression guard for issue #263's headline
    /// defect: `[excludes] pattern = [...]` (singular — the real key is
    /// `patterns`) parses cleanly today because `ExcludesSection` carries
    /// no `#[serde(deny_unknown_fields)]`, so `read_dotfile` silently
    /// returns `exclude_patterns: []`. The walk then leaves out nothing,
    /// and the material the operator meant to exclude is
    /// archived, encrypted, and written to write-once media — inside a
    /// valid sha256, so nothing downstream ever notices. This must now
    /// fail LOUDLY instead: the whole point is that silently including the
    /// material is unacceptable, so refusing the stage (not quietly
    /// excluding it after the fact) is the only safe outcome once the
    /// dotfile cannot be trusted.
    #[test]
    fn stage_create_refuses_a_dotfile_with_a_misspelled_excludes_key() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();

        let conn = crate::db::open(&paths.db_file).unwrap();

        let staging_dir = tmp.path().join("staging");
        fs::create_dir_all(&staging_dir).unwrap();

        let mut config = Config::default();
        config.staging.directory = staging_dir.to_string_lossy().into_owned();

        crate::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        register_test_escrow(&conn);

        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("keep.txt"), b"keep me").unwrap();
        fs::write(src.join("secret.env"), b"do not archive me").unwrap();

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

        // Hand-edit the dotfile the way an operator actually would — there
        // is no CLI for `[excludes] patterns` — replacing the correctly
        // spelled empty array `init_unit` wrote with the singular typo,
        // while leaving `[unit]` (uuid/created/tenant) untouched.
        let dotfile_path = src.join(".tapectl-unit.toml");
        let content = std::fs::read_to_string(&dotfile_path).unwrap();
        assert!(
            content.contains("patterns = []"),
            "test assumption: init_unit writes an empty patterns array, got: {content}"
        );
        let content = content.replace("patterns = []", "pattern = [\"*.env\"]");
        std::fs::write(&dotfile_path, content).unwrap();

        // `snapshot_create` walks the source directory too (`walk_directory`
        // -> `exclude::effective_compiled` -> `dotfile_patterns`, the same
        // shared helper every walk uses), so the fix
        // actually closes this gap even earlier than `stage_create` — at
        // manifest-build time. Either stage failing loudly satisfies this
        // test; the one thing that must NOT happen is the whole pipeline
        // reaching a successful stage_set with secret.env silently included.
        let result = snapshot_create(&conn, "unit1", &Config::default())
            .and_then(|snap_id| stage_create(&conn, &paths, &config, snap_id, false));

        assert!(
            result.is_err(),
            "a dotfile with a misspelled [excludes] key must be refused, not silently \
             yield zero excludes and archive secret.env onto write-once media (issue #263)"
        );
    }

    /// The core claim of issue #47: `stage_create` must resolve
    /// `slice_size` through `policy::resolve` (dotfile > archive_set >
    /// default), not read `config.defaults.slice_size` unconditionally.
    /// Exercises the REAL pipeline end to end (real `tar`, real tenant
    /// keys via `tenant::add_tenant`) so the assertion is against what
    /// actually lands in `stage_sets`, not a mocked shortcut.
    ///
    /// Also asserts on `compression`: prior to issue #92's fix,
    /// `unit::init_unit` always wrote a dotfile whose `[policy]` section
    /// carried a concrete `compression` value (see the design doc's own
    /// §2.2 example), and `policy::resolve`'s dotfile layer unconditionally
    /// outranks archive_set whenever a dotfile is present — so for every
    /// real, dotfile-backed unit, `compression` resolved to the dotfile's
    /// hardcoded value regardless of any archive_set override, making the
    /// archive set's `compression` structurally unreachable. Per the CTO's
    /// ratified fix ("Recast of v4.0 §2.2" in docs/design-errata.md,
    /// issue #92), dotfile policy fields are now `Option` and are omitted
    /// from newly-written dotfiles unless the operator sets them
    /// explicitly — so `init_unit`'s dotfile no longer shadows this
    /// archive_set field, and this test proves that end to end. Because the
    /// archive set names a real compressor, it also packs each slice with
    /// it, the path #92 made reachable for the first time.
    #[test]
    fn stage_create_uses_archive_set_resolved_slice_size_not_global_default() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();

        let conn = crate::db::open(&paths.db_file).unwrap();

        let staging_dir = tmp.path().join("staging");
        fs::create_dir_all(&staging_dir).unwrap();

        let mut config = Config::default();
        config.staging.directory = staging_dir.to_string_lossy().into_owned();
        config.defaults.slice_size = "100M".to_string();
        // Leave the default at "none" and have the archive set override it to
        // a real compressor. That direction is the one that actually proves
        // the fix: pre-#92, `init_unit`'s dotfile hardcoded `compression =
        // "none"`, so an assertion expecting "none" would have passed for the
        // wrong reason. Expecting "gzip" can only succeed if the dotfile has
        // stopped shadowing. It also drives the real packing path.
        config.defaults.compression = "none".to_string();

        crate::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        // Issue #115: `stage_create` refuses without a registered escrow.
        register_test_escrow(&conn);

        // Archive set overriding both slice_size and compression away from
        // config.defaults' "100M"/"none" above.
        conn.execute(
            "INSERT INTO archive_sets (name, slice_size, compression) VALUES ('cold', ?1, ?2)",
            params![50i64 * 1024 * 1024, "gzip"],
        )
        .unwrap();

        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("f.txt"),
            b"hello world, this is stage_create resolver test content",
        )
        .unwrap();

        crate::unit::init_unit(
            &conn,
            &paths,
            src.to_str().unwrap(),
            "alice",
            Some("unit1"),
            &[],
            Some("cold"),
        )
        .unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let stage_set_id = stage_create(&conn, &paths, &config, snap_id, false).unwrap();

        let slice_size: i64 = conn
            .query_row(
                "SELECT slice_size FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(
            slice_size,
            50 * 1024 * 1024,
            "stage_sets.slice_size must reflect the archive_set's override (50M), \
             not config.defaults.slice_size (100M) — proves stage_create resolves \
             policy instead of reading config.defaults directly"
        );

        let compression: String = conn
            .query_row(
                "SELECT compression FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(
            compression, "gzip",
            "stage_sets.compression must reflect the archive_set's override (gzip), \
             not config.defaults.compression (none), and must not be shadowed by \
             a concrete value baked into the unit's dotfile — proves the \
             archive_set's compression is actually reachable (issue #92)"
        );
    }

    /// End-to-end proof (real tar, real encryption) that two stage sets of
    /// the SAME snapshot never collide: their `.age` slice paths differ,
    /// and both sets of ciphertext survive on disk simultaneously —
    /// something the old per-snapshot `archive_base` could not guarantee.
    #[test]
    fn two_stage_sets_of_the_same_snapshot_do_not_collide_on_disk() {
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

        crate::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        // Issue #115: `stage_create` refuses without a registered escrow.
        register_test_escrow(&conn);

        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("f.txt"), b"collision-test content").unwrap();

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

        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        // Two stage sets of the SAME snapshot — the flow issue #53 makes
        // reachable via `stage create --version`. Calling `stage_create`
        // directly here (bypassing the CLI gate) is deliberate: this test
        // is about `archive_base` collision, not about the gate.
        let stage_set_1 = stage_create(&conn, &paths, &config, snap_id, false).unwrap();
        let stage_set_2 = restage(&conn, &paths, &config, snap_id).unwrap();
        assert_ne!(stage_set_1, stage_set_2);

        let paths_for = |stage_set_id: i64| -> Vec<String> {
            conn.prepare("SELECT staging_path FROM stage_slices WHERE stage_set_id = ?1")
                .unwrap()
                .query_map(params![stage_set_id], |row| row.get(0))
                .unwrap()
                .collect::<std::result::Result<Vec<String>, _>>()
                .unwrap()
        };
        let slices_1 = paths_for(stage_set_1);
        let slices_2 = paths_for(stage_set_2);
        assert!(!slices_1.is_empty());
        assert!(!slices_2.is_empty());

        // Distinct paths, and both must still exist on disk — proof the
        // second stage set never overwrote the first's ciphertext.
        for p in slices_1.iter().chain(slices_2.iter()) {
            assert!(Path::new(p).exists(), "{p} must exist on disk");
        }
        let set1: std::collections::HashSet<_> = slices_1.iter().collect();
        let set2: std::collections::HashSet<_> = slices_2.iter().collect();
        assert!(
            set1.is_disjoint(&set2),
            "the two stage sets' slice paths must never overlap: {slices_1:?} vs {slices_2:?}"
        );
    }

    // ── issue #54: stage failure hygiene ──

    /// A failure after tar has finished — every slice written and recorded,
    /// the stage not yet finalized — leaves nothing in staging. Before issue
    /// #54 the plaintext slices written then were never cleaned up; since
    /// issue #370 there are none, and the recorded `.age`
    /// slices are what the cleanup must remove.
    ///
    /// The failure is injected right after tar (`failpoint::AFTER_TAR`).
    /// It used to be injected with a tenant that had no active keys, until
    /// issue #354 moved that refusal before tar, and then by making
    /// `catalogs_dir` a file, until issue #419 created the stage set's
    /// catalogue directory (Layout 2's; a staging on tar makes none).
    /// The cleanup also removes that directory when it is there.
    #[test]
    fn a_failure_after_tar_leaves_nothing_in_staging() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();

        let conn = crate::db::open(&paths.db_file).unwrap();

        let staging_dir = tmp.path().join("staging");
        fs::create_dir_all(&staging_dir).unwrap();

        let mut config = Config::default();
        config.staging.directory = staging_dir.to_string_lossy().into_owned();

        crate::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        // Issue #115: `stage_create` refuses without a registered escrow.
        register_test_escrow(&conn);

        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("f.txt"),
            b"content that will be orphaned unless cleaned up",
        )
        .unwrap();

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

        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        // The injected post-tar failure (see the doc comment).
        failpoint::arm(failpoint::AFTER_TAR);
        failpoint::observe_staging_after_tar();
        let result = stage_create(&conn, &paths, &config, snap_id, false);

        assert!(
            matches!(&result, Err(e) if e.to_string().contains("injected failure")),
            "expected stage_create to fail at the injected point, got {result:?}"
        );

        // Positive control: the failure must have come AFTER tar, with its
        // slices written, or the assertion below would pass vacuously.
        let tar_ran: bool = conn
            .query_row(
                "SELECT tar_command IS NOT NULL FROM stage_sets WHERE snapshot_id = ?1",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            tar_ran,
            "the injected failure must land after tar, got: {result:?}"
        );
        assert_eq!(
            failpoint::staging_after_tar().map(|names| names.len()),
            Some(1),
            "positive control: one slice was in staging when the failure hit"
        );

        let leaked: Vec<_> = fs::read_dir(&staging_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(
            leaked.is_empty(),
            "the failed stage set's files are all removed — found: {leaked:?}"
        );

        // Issue #419: no catalogue directory is left for this stage set.
        let unit_catalogues = paths.catalogs_dir.read_dir().unwrap().flatten();
        for unit_dir in unit_catalogues {
            let left: Vec<_> = fs::read_dir(unit_dir.path()).unwrap().flatten().collect();
            assert!(
                left.is_empty(),
                "the failed stage set's catalogue directory is removed: {:?}",
                left.iter().map(|e| e.path()).collect::<Vec<_>>()
            );
        }

        // The stage_sets row must survive the failure — that 'staging'
        // status is what recover_orphaned_sessions (src/db/mod.rs) sweeps
        // to 'failed' on the next db::open(), the operator's actual signal.
        let row_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM stage_sets WHERE snapshot_id = ?1",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            row_count, 1,
            "stage_sets row must survive stage_create's failure so the startup \
             sweep can still mark it 'failed' — cleanup must never delete or \
             re-status it"
        );
    }

    /// Cleanup must not reach across snapshot versions of the same unit.
    ///
    /// `archive_base` is `{uuid12}_v{version}_s{id}` and slices are
    /// `{base}.{N}.tar.age`, so the scan prefix must carry a trailing dot.
    /// Without it `..._s1` is a prefix of `..._s10.1.tar.age`, and a failed
    /// stage deletes the in-flight slices of a concurrent stage — silent
    /// cross-stage data destruction, the exact
    /// hazard the `.age` cleanup is deliberately DB-row-keyed to avoid.
    ///
    /// Drives `cleanup_failed_stage_set` directly against hand-placed decoy
    /// files, because provoking two concurrent real tar runs at versions 1
    /// and 10 is not worth the fixture cost to prove a string-prefix rule.
    #[test]
    fn cleanup_does_not_delete_a_higher_version_stage_of_the_same_unit() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();
        let conn = crate::db::open(&paths.db_file).unwrap();

        let staging_dir = tmp.path().join("staging");
        fs::create_dir_all(&staging_dir).unwrap();
        let mut config = Config::default();
        config.staging.directory = staging_dir.to_string_lossy().into_owned();

        crate::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();
        let tid = crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();

        // A unit whose uuid12 prefix we control, with snapshots at v1 and v10.
        let uuid = "aaaaaaaabbbbccccddddeeeeeeeeeeee";
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES (?1, 'u', ?2, 'mtime_size', 1, 'active')",
            params![uuid, tid],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, source_path, status)
             VALUES (?1, 1, '/src', 'created')",
            params![unit_id],
        )
        .unwrap();
        let snap_v1 = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, slice_size, compression, encrypted)
             VALUES (?1, 1024, 'none', 1)",
            params![snap_v1],
        )
        .unwrap();
        let failed_set = conn.last_insert_rowid();

        // A second stage set of the SAME snapshot (issue #53: two stage
        // sets can now coexist for one snapshot) whose files must survive
        // `failed_set`'s cleanup untouched.
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, slice_size, compression, encrypted)
             VALUES (?1, 1024, 'none', 1)",
            params![snap_v1],
        )
        .unwrap();
        let sibling_set = conn.last_insert_rowid();

        let base12 = &uuid[..12];
        let failed_slice = staging_dir.join(format!("{base12}_v1_s{failed_set}.2.tar.age"));
        let sibling_slice = staging_dir.join(format!("{base12}_v1_s{sibling_set}.2.tar.age"));
        let sibling_hash = staging_dir.join(format!("{base12}_v1_s{sibling_set}.3.tar.age"));
        // Issue #370: the `.tar.age` being written has no row yet, so it is
        // found by the same prefix rule.
        let failed_age = staging_dir.join(format!("{base12}_v1_s{failed_set}.1.tar.age"));
        let sibling_age = staging_dir.join(format!("{base12}_v1_s{sibling_set}.1.tar.age"));
        for f in [
            &failed_slice,
            &sibling_slice,
            &sibling_hash,
            &failed_age,
            &sibling_age,
        ] {
            fs::write(f, b"x").unwrap();
        }

        cleanup_failed_stage_set(&conn, &paths, &config, failed_set);

        assert!(
            !failed_slice.exists(),
            "the failed stage set's own orphaned slice must be removed"
        );
        assert!(
            sibling_slice.exists(),
            "a sibling stage set's in-flight slice must survive this \
             cleanup — a prefix without the stage_set_id/trailing-dot discipline \
             would have deleted it"
        );
        assert!(
            sibling_hash.exists(),
            "the sibling's other slice must survive this cleanup for the same reason"
        );
        assert!(
            !failed_age.exists(),
            "the failed set's unrecorded .age is removed"
        );
        assert!(
            sibling_age.exists(),
            "a sibling stage set's slice being written survives"
        );
    }

    /// Issue #377: a file-backed catalog with one `staging` stage set and a
    /// freshly written `.age` — the state `record_encrypted_slice` is called
    /// in. Returns `(tmp, db_path, conn, config, stage_set_id, age)`.
    #[allow(clippy::type_complexity)]
    fn slice_being_recorded() -> (TempDir, PathBuf, Connection, Config, i64, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("tapectl.db");
        let conn = crate::db::open(&db_path).unwrap();
        let staging_dir = tmp.path().join("staging");
        fs::create_dir_all(&staging_dir).unwrap();
        let mut config = Config::default();
        config.staging.directory = staging_dir.to_string_lossy().into_owned();
        let uuid = "aaaaaaaabbbbccccddddeeeeeeeeeeee";
        conn.execute_batch(&format!(
            "INSERT INTO tenants (name) VALUES ('alice');
             INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                 VALUES ('{uuid}', 'u', 1, 'mtime_size', 1, 'active');
             INSERT INTO snapshots (unit_id, version, source_path, status)
                 VALUES (1, 1, '/src', 'created');
             INSERT INTO stage_sets (snapshot_id, slice_size, compression, encrypted)
                 VALUES (1, 1024, 'none', 1);"
        ))
        .unwrap();
        let age = staging_dir.join(format!("{}.1.tar.age", archive_base_name(uuid, 1, 1)));
        fs::write(&age, b"ciphertext").unwrap();
        (tmp, db_path, conn, config, 1, age)
    }

    fn slice_info() -> EncryptedSliceInfo {
        EncryptedSliceInfo {
            plain_size: 9,
            sha256_plain: "p".into(),
            encrypted_size: 10,
            sha256_encrypted: "e".into(),
            packed_size: None,
        }
    }

    fn slice_rows(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM stage_slices", [], |r| r.get(0))
            .unwrap()
    }

    const QUICK: BusyPolicy = BusyPolicy {
        budget: std::time::Duration::from_millis(200),
        first_pause: std::time::Duration::from_millis(10),
        max_pause: std::time::Duration::from_millis(50),
    };

    /// Issue #377 item 1: a busy INSERT can never leave an `.age` that no
    /// row names. With the write lock held past the retry budget, the `.age`
    /// is removed; released — the positive control — the same call records
    /// the row.
    #[test]
    fn a_busy_slice_insert_leaves_no_age_file_without_a_row() {
        let (_tmp, db_path, conn, _config, stage_set_id, age) = slice_being_recorded();
        conn.pragma_update(None, "busy_timeout", 20).unwrap();
        let holder = Connection::open(&db_path).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();

        let err = record_encrypted_slice(&conn, QUICK, stage_set_id, 1, false, &age, &slice_info())
            .unwrap_err();
        assert!(matches!(err, TapectlError::CatalogBusy(_)), "{err:?}");
        assert_eq!(slice_rows(&conn), 0);
        assert!(
            !age.exists(),
            "an .age no row names must not be left behind"
        );

        holder.execute_batch("ROLLBACK").unwrap();
        fs::write(&age, b"ciphertext").unwrap();
        record_encrypted_slice(&conn, QUICK, stage_set_id, 1, false, &age, &slice_info()).unwrap();
        assert_eq!(slice_rows(&conn), 1);
        assert!(age.exists());
    }

    /// Issue #377 item 2: a lock error after slices are encrypted never
    /// reaches `cleanup_failed_stage_set` — the recorded `.age` and its row
    /// stay. The same call with any other error (the positive control)
    /// does clean them up.
    #[test]
    fn a_busy_error_keeps_the_stage_sets_files_and_any_other_error_cleans_them() {
        let (tmp, _db_path, conn, config, stage_set_id, age) = slice_being_recorded();
        let paths = TapectlPaths::new(tmp.path().join("home"));
        record_encrypted_slice(&conn, QUICK, stage_set_id, 1, false, &age, &slice_info()).unwrap();

        let busy_err = TapectlError::CatalogBusy("the stage set's finalization".into());
        after_failed_stage(&conn, &paths, &config, stage_set_id, &busy_err);
        assert!(
            age.exists(),
            "a busy catalog must not discard encrypted slices"
        );
        assert_eq!(slice_rows(&conn), 1);

        after_failed_stage(
            &conn,
            &paths,
            &config,
            stage_set_id,
            &TapectlError::Other("tar failed".into()),
        );
        assert!(!age.exists(), "any other failure still cleans up");
        assert_eq!(slice_rows(&conn), 0);
    }

    // ── issue #49: exclusions end-to-end (dotfile+global -> walk, list, validation) ──

    /// Shared setup for the issue #49 tests below: a real tenant + unit
    /// (via `unit::init_unit`, so a real dotfile + real tenant keys exist —
    /// `stage_create` refuses to encrypt without active tenant keys), with
    /// the unit's dotfile `[excludes] patterns` overwritten to
    /// `exclude_patterns` (empty = the "no excludes configured" case,
    /// `init_unit` itself always writes an empty list). Returns
    /// `(conn, paths, config, src_dir)`; the caller writes fixture files
    /// into `src_dir` and drives `snapshot_create`/`stage_create` itself,
    /// since each test needs different file content/timing.
    /// A second stage of a snapshot already staged, as `stage create
    /// --version` makes one (issue #53) — the tests that stage one snapshot
    /// twice model that, not a fresh stage (issue #368's admission).
    pub(super) fn restage(
        conn: &Connection,
        paths: &TapectlPaths,
        config: &Config,
        snapshot_id: i64,
    ) -> Result<i64> {
        stage_create_admitting(
            conn,
            paths,
            config,
            snapshot_id,
            false,
            Admission::Restage,
            None,
            &mut std::io::stderr(),
        )
    }

    /// A tar that passes `--version` and the probe (its archive of one file
    /// in the probe's own directory) through to the real tar, and refuses a
    /// real archive run, saying `tar-must-never-run`: how a test shows a
    /// refusal came before tar archived anything, now that the probe runs
    /// before anything is made.
    pub(super) fn never_archiving_tar() -> String {
        static TAR: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        TAR.get_or_init(|| {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::Builder::new()
                .prefix("never-archiving-tar-")
                .tempdir()
                .unwrap()
                .keep();
            let p = dir.join("tar");
            fs::write(
                &p,
                "#!/bin/sh\n\
                 if [ \"$1\" = --create ] && [ ! -e acl-probe ]; then \
                 echo tar-must-never-run >&2; exit 2; fi\n\
                 exec tar \"$@\"\n",
            )
            .unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
            p.to_string_lossy().into_owned()
        })
        .clone()
    }

    pub(super) fn setup_unit_with_excludes(
        tmp: &TempDir,
        exclude_patterns: Vec<String>,
    ) -> (Connection, TapectlPaths, Config, PathBuf) {
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();
        let conn = crate::db::open(&paths.db_file).unwrap();

        let staging_dir = tmp.path().join("staging");
        fs::create_dir_all(&staging_dir).unwrap();

        let mut config = Config::default();
        config.staging.directory = staging_dir.to_string_lossy().into_owned();

        crate::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();

        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();

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

        if !exclude_patterns.is_empty() {
            let dotfile_path = src.join(".tapectl-unit.toml");
            let mut df = crate::unit::dotfile::read_dotfile(&dotfile_path).unwrap();
            df.exclude_patterns = exclude_patterns;
            crate::unit::dotfile::write_dotfile(&dotfile_path, &df).unwrap();
        }

        // Issue #115: `stage_create` refuses without one.
        let _escrow_pk = register_test_escrow(&conn);

        (conn, paths, config, src)
    }

    /// Issue #115 / ADR-0005. Staging without a registered escrow recipient
    /// silently produces slices the escrow key can never open — the whole
    /// point of the escrow line — and `volume write` now refuses to put them
    /// on tape. So the refusal belongs here, before tar and age burn hours
    /// on material that cannot be written, not at the drive afterwards.
    ///
    /// The two halves that matter are both about *when*: no `stage_sets` row
    /// may be left behind, and tar must never start. `tar.binary` is pointed
    /// at a path that cannot exist, so reaching tar at all would produce a
    /// visibly different error.
    #[test]
    fn stage_create_refuses_when_no_escrow_recipient_is_registered() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        // The shared fixture registers an escrow, because that is now the
        // precondition every other staging test needs. This is the one test
        // that must run without one.
        conn.execute("DELETE FROM encryption_keys WHERE is_escrow = 1", [])
            .unwrap();
        config.tar.binary = never_archiving_tar();

        fs::write(src.join("f.txt"), b"content that must never reach tar").unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();

        let err = stage_create(&conn, &paths, &config, snap_id, false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no escrow recipient is registered"),
            "expected the escrow refusal, got: {msg}"
        );
        assert!(
            msg.contains("key generate --escrow") && msg.contains("key import --escrow"),
            "the refusal must name both ways to register one: {msg}"
        );
        assert!(
            !msg.contains("tar-must-never-run"),
            "the refusal must precede the tar run, not follow it: {msg}"
        );

        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            after, before,
            "the refusal must precede the stage_sets INSERT — no orphan row \
             for the startup sweep to find and mark 'failed'"
        );

        let left_in_staging: Vec<String> = fs::read_dir(&config.staging.directory)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            left_in_staging.is_empty(),
            "nothing may reach the staging directory before the refusal: {left_in_staging:?}"
        );
    }

    /// Issue #354 (c): a tenant with no active keys is refused BEFORE tar,
    /// the same way the escrow refusal above is. It used to be checked at
    /// the top of the encryption loop — after `tar` had archived the
    /// whole unit and the catalog had been extracted — so a large unit
    /// spent its entire archive run to reach a refusal that needed nothing but
    /// a database read.
    ///
    /// Same two "when" halves as the escrow test: tar never starts (its
    /// binary is a path that cannot exist, so reaching it would change the
    /// error), and no `stage_sets` row is left for the startup sweep.
    #[test]
    fn stage_create_refuses_a_tenant_with_no_active_keys_before_tar_runs() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        conn.execute(
            "UPDATE encryption_keys SET is_active = 0
             WHERE tenant_id = (SELECT id FROM tenants WHERE name = 'alice')",
            [],
        )
        .unwrap();
        config.tar.binary = never_archiving_tar();

        fs::write(src.join("f.txt"), b"content that must never reach tar").unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();

        let err = stage_create(&conn, &paths, &config, snap_id, false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("has no active keys"),
            "expected the no-active-keys refusal, got: {msg}"
        );
        assert!(
            !msg.contains("tar-must-never-run"),
            "the refusal must precede the tar run, not follow it: {msg}"
        );

        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            after, before,
            "the refusal must precede the stage_sets INSERT — no orphan row \
             for the startup sweep to find and mark 'failed'"
        );

        let left_in_staging: Vec<String> = fs::read_dir(&config.staging.directory)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            left_in_staging.is_empty(),
            "nothing may reach the staging directory before the refusal: {left_in_staging:?}"
        );
    }

    /// Render an error exactly as `error::exit_with_error` does for the
    /// operator: `{:#}` on the anyhow error, which walks the `source()`
    /// chain. Asserting on `err.to_string()` alone would never see issue
    /// #354's doubled io text — that is the outer Display only.
    fn as_operator_sees_it(err: TapectlError) -> String {
        format!("{:#}", anyhow::Error::from(err))
    }

    /// How many times an io error's `(os error N)` text appears — once is
    /// right; twice is the `TapectlError::Io` chain-doubling of issue #354.
    fn os_error_mentions(msg: &str) -> usize {
        msg.matches("(os error").count()
    }

    /// Issue #354 (a): a staging directory that cannot be created is named,
    /// with the operation, and the io text is printed once. It used to reach
    /// the operator as `error: Not a directory (os error 20): Not a directory
    /// (os error 20)` — twice, and with no path.
    ///
    /// The parent is a regular file (ENOTDIR), so no permission bits are
    /// involved and this holds under root too.
    #[test]
    fn an_uncreatable_staging_directory_is_named_with_the_operation() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        let not_a_dir = tmp.path().join("a-regular-file");
        fs::write(&not_a_dir, b"x").unwrap();
        let staging = not_a_dir.join("staging");
        config.staging.directory = staging.to_string_lossy().into_owned();
        config.tar.binary = never_archiving_tar();

        fs::write(src.join("f.txt"), b"content").unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        let err = stage_create(&conn, &paths, &config, snap_id, false).unwrap_err();
        let msg = as_operator_sees_it(err);
        assert!(
            msg.contains(&*staging.to_string_lossy()),
            "the error must name the staging directory: {msg}"
        );
        assert!(
            msg.contains("cannot create staging directory"),
            "the error must name the operation: {msg}"
        );
        assert_eq!(
            os_error_mentions(&msg),
            1,
            "the io error must be printed once, not doubled: {msg}"
        );
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "refused before the stage_sets INSERT");
    }

    /// Issue #354 (a): a staging directory that exists but cannot be written
    /// is refused before anything else touches it — named, with the
    /// operation. It used to pass every check, spend the whole sha256
    /// validation pass over the source, and only then fail inside the archiver.
    ///
    /// Permission bits do not bind root, so the test skips there (the
    /// ENOTDIR test above is the root-safe half of this behaviour).
    #[test]
    fn an_unwritable_staging_directory_is_refused_before_tar_naming_it() {
        use std::os::unix::fs::PermissionsExt;
        if nix::unistd::geteuid().is_root() {
            eprintln!("skipping: permission bits do not bind root");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        let staging = PathBuf::from(&config.staging.directory);
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o500)).unwrap();
        config.tar.binary = never_archiving_tar();

        fs::write(src.join("f.txt"), b"content").unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        let result = stage_create(&conn, &paths, &config, snap_id, false);
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700)).unwrap();
        let msg = as_operator_sees_it(result.unwrap_err());
        assert!(
            msg.contains("cannot write to staging directory")
                && msg.contains(&*staging.to_string_lossy()),
            "the error must name the operation and the directory: {msg}"
        );
        assert!(
            !msg.contains("tar-must-never-run"),
            "the refusal must precede the tar run: {msg}"
        );
        assert_eq!(os_error_mentions(&msg), 1, "printed once: {msg}");
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "refused before the stage_sets INSERT");
    }

    /// The complement of the refusal above, and the ordering story issue
    /// #115 turns on. Two claims:
    ///
    /// 1. `policy.encrypt = false` is still never honored and never fatal —
    ///    it warns and encrypts anyway (the coordinator decision recorded in
    ///    `stage_create_inner`). The new escrow precondition must not have
    ///    quietly turned that warning into a refusal.
    /// 2. The recipient list `stage_create` RECORDS on the stage set
    ///    contains the escrow public key. That column is the sole evidence
    ///    `volume write`'s pre-flight has (an age X25519 stanza names no
    ///    recipient), so if it were ever wrong, the check built on it would
    ///    be too.
    #[test]
    fn encrypt_false_still_warns_encrypts_and_records_the_escrow_recipient() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        let escrow_pk: String = conn
            .query_row(
                "SELECT public_key FROM encryption_keys WHERE is_escrow = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        config.defaults.encrypt = false;

        fs::write(
            src.join("f.txt"),
            b"content staged under policy.encrypt = false",
        )
        .unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let mut notices = Vec::new();
        let stage_set_id =
            stage_create_reporting(&conn, &paths, &config, snap_id, false, &mut notices)
                .expect("encrypt=false must warn, not refuse (ADR-0005 makes it un-honorable)");

        // Issue #347: the warning is a notice — written to what
        // `stage_create` makes stderr — not a `tracing::warn!` that
        // `logging.level = "error"` silences.
        let notices = String::from_utf8(notices).unwrap();
        assert!(
            notices.contains("encrypt = false") && notices.contains("\"unit1\""),
            "encrypt = false must produce a notice naming the unit: {notices:?}"
        );
        assert!(
            notices.contains("encrypted anyway") && notices.contains("ADR-0005"),
            "the notice must say what actually happens, and why: {notices:?}"
        );

        let (encrypted, fingerprints): (i64, String) = conn
            .query_row(
                "SELECT encrypted, key_fingerprints FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(encrypted, 1, "encryption cannot be disabled (ADR-0005)");

        let recipients: Vec<String> = serde_json::from_str(&fingerprints).unwrap();
        assert!(
            recipients.contains(&escrow_pk),
            "the recorded recipient list must contain the escrow public key — it is \
             what `volume write` checks the slices against (issue #115): {recipients:?}"
        );

        // And the bytes on disk really are age ciphertext, not plaintext tar.
        let slice_path: String = conn
            .query_row(
                "SELECT staging_path FROM stage_slices WHERE stage_set_id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            slice_path.ends_with(".age"),
            "slice must be the encrypted artifact: {slice_path}"
        );
        let head = fs::read(&slice_path).unwrap();
        assert!(
            head.starts_with(b"age-encryption.org/v1"),
            "slice must carry a real age header despite policy.encrypt = false"
        );
    }

    /// The negative half of the notice above, with its positive control in
    /// that test: an ordinary stage — `encrypt` left at its default, and a
    /// staging directory with plenty of room — writes no notice at all, so
    /// the one that does appear is never noise an operator learns to skip.
    #[test]
    fn an_ordinary_stage_writes_no_notices() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        assert!(config.defaults.encrypt, "fixture: the default policy");
        fs::write(src.join("f.txt"), b"ordinary content").unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        let mut notices = Vec::new();
        stage_create_reporting(&conn, &paths, &config, snap_id, false, &mut notices).unwrap();
        assert_eq!(
            String::from_utf8(notices).unwrap(),
            "",
            "an ordinary stage must be quiet"
        );
    }

    // ── issue #354 (b) and #364: staging space, asked about before staging ──

    /// `len` bytes with no zero byte anywhere — nothing an archiver could
    /// store as a hole — written and synced, so its allocation is on record.
    fn write_dense_file(path: &Path, len: usize) {
        let data: Vec<u8> = (0..len as u32)
            .map(|i| ((i.wrapping_mul(2_654_435_761) >> 13) as u8) | 1)
            .collect();
        let mut f = fs::File::create(path).unwrap();
        f.write_all(&data).unwrap();
        f.sync_all().unwrap();
    }

    /// The figure the space check states (`stream_estimate`), at the
    /// default slice size, uncompressed.
    fn upper_bound(conn: &Connection, snapshot: &models::Snapshot) -> i64 {
        stream_estimate(conn, snapshot.id, &snapshot.source_path, 1 << 30, None).unwrap()
    }

    /// Issue #364: with no pre-tar read there is no content lower bound, so
    /// a staging directory short of the snapshot's size is ASKED about, never
    /// refused outright: with `--yes` the stage goes ahead, with a notice
    /// carrying the figures. Before, free space under the non-zero bytes the
    /// hash pass counted was a hard refusal no flag could pass.
    #[test]
    fn short_staging_space_is_asked_about_and_staged_with_yes() {
        use crate::util::format_bytes_binary as fmt;
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        write_dense_file(&src.join("big.bin"), 256 * 1024);
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let free: u64 = 100 * 1024;
        let _free = FreeSpaceOverride::set(free);

        let mut notices = Vec::new();
        stage_create_reporting(&conn, &paths, &config, snap_id, true, &mut notices)
            .expect("short space is a question, and --yes answers it");
        let notices = String::from_utf8(notices).unwrap();
        assert!(
            notices.contains("may be too small for unit \"unit1\"")
                && notices.contains(&format!("{} free", fmt(free as i64)))
                && notices.contains("staging anyway (--yes given)"),
            "{notices}"
        );
    }

    /// Issue #368: a stage counts what the stages running beside it may still
    /// write. Free space enough for this unit alone, but not for it and the
    /// other stage's remaining slices, is asked about — refused without
    /// consent — while the other stage's lock is held; once that stage is
    /// gone (its lock free), the same space is this one's.
    #[test]
    fn staging_space_counts_the_stages_running_beside_it() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        write_dense_file(&src.join("a.bin"), 64 * 1024);
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let snapshot = get_snapshot(&conn, snap_id).unwrap();

        // Another stage in flight: a 10 MiB snapshot, 4 MiB of it written.
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path,
                                    total_size, file_count)
             VALUES (?1, 2, 'full', 'created', '/elsewhere', ?2, 1)",
            params![snapshot.unit_id, 10i64 << 20],
        )
        .unwrap();
        let other_snap = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, slice_size, compression, encrypted)
             VALUES (?1, 1048576, 'none', 1)",
            params![other_snap],
        )
        .unwrap();
        let other = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                       sha256_plain, sha256_encrypted, staging_path)
             VALUES (?1, 1, ?2, ?2, 'p', 'e', '/elsewhere/s.1.tar.age')",
            params![other, 4i64 << 20],
        )
        .unwrap();
        let other_lock = lock::acquire(&paths.db_file, other).unwrap();

        let still_to_write = stage_upper_bound(Some(10 << 20), Some(1)) - (4 << 20);
        let free = upper_bound(&conn, &snapshot) + still_to_write / 2;
        let _free = FreeSpaceOverride::set(free as u64);

        let mut notices = Vec::new();
        let err = stage_create_reporting(&conn, &paths, &config, snap_id, false, &mut notices)
            .expect_err("this unit and the other stage's rest do not both fit")
            .to_string();
        assert!(
            err.contains("may be too small for unit \"unit1\"")
                && err.contains("may still be written by 1 other stage running now")
                && err.contains("refused"),
            "{err}"
        );

        drop(other_lock);
        stage_create_reporting(&conn, &paths, &config, snap_id, false, &mut Vec::new())
            .expect("the other stage is gone: the space is this unit's");
    }

    /// Issue #368: a unit is never staged twice at once. While another
    /// process stages this snapshot (its stage set `staging`, its lock
    /// held), a second stage of it is refused before it records anything;
    /// once that holder is gone the row is a crashed stage, and staging
    /// goes ahead.
    #[test]
    fn a_unit_being_staged_right_now_is_not_staged_again() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        write_dense_file(&src.join("a.bin"), 64 * 1024);
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, slice_size, compression, encrypted)
             VALUES (?1, 1048576, 'none', 1)",
            params![snap_id],
        )
        .unwrap();
        let other = conn.last_insert_rowid();
        let other_lock = lock::acquire(&paths.db_file, other).unwrap();

        let err = stage_create_reporting(&conn, &paths, &config, snap_id, false, &mut Vec::new())
            .expect_err("the same snapshot is being staged")
            .to_string();
        assert!(
            err.contains("unit \"unit1\" v1 is being staged right now")
                && err.contains(&format!("stage set {other}")),
            "{err}"
        );
        let sets: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sets, 1, "nothing recorded for the refused stage");

        drop(other_lock);
        stage_create_reporting(&conn, &paths, &config, snap_id, false, &mut Vec::new())
            .expect("its holder is gone: staging goes ahead");
    }

    /// Issue #368's never-twice rule, the race it left open: two `stage
    /// create unit1` both look up v1 while it is still `created`; the first
    /// finishes (its set `staged`) before the second reaches admission. The
    /// second must not stage v1 again. Under the admission lock a `staged`
    /// sibling refuses, as a live `staging` one does — before anything is
    /// recorded and before tar.
    #[test]
    fn a_unit_staged_while_this_stage_waited_is_not_staged_again() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        write_dense_file(&src.join("a.bin"), 64 * 1024);
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        // The sibling's finished stage, as its finalize left it.
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, slice_size, compression, encrypted, status)
             VALUES (?1, 1048576, 'none', 1, 'staged')",
            params![snap_id],
        )
        .unwrap();
        let sibling = conn.last_insert_rowid();
        config.tar.binary = never_archiving_tar();

        let err = stage_create_reporting(&conn, &paths, &config, snap_id, false, &mut Vec::new())
            .expect_err("v1 is already staged")
            .to_string();
        assert!(
            err.contains("unit \"unit1\" v1 is already staged")
                && err.contains(&format!("stage set {sibling}")),
            "{err}"
        );
        assert!(!err.contains("tar-must-never-run"), "{err}");
        let sets: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sets, 1, "nothing recorded for the refused stage");
    }

    /// Issue #368 (and #377's rule 3): a stage's writes wait out a catalog
    /// another stage holds past `busy_timeout`, rather than fail — before
    /// tar (recording the stage set, which with `--jobs` would stop the
    /// run) and after it (a finished archive). Another connection takes the
    /// write lock at that point and keeps it for longer than the 5-second
    /// wait.
    #[test]
    fn a_catalog_held_past_busy_timeout_is_waited_out_before_tar() {
        catalog_held_past_busy_timeout_is_waited_out_at(failpoint::BEFORE_RECORD);
    }

    #[test]
    fn a_catalog_held_past_busy_timeout_is_waited_out_after_tar() {
        catalog_held_past_busy_timeout_is_waited_out_at(failpoint::AFTER_TAR);
    }

    fn catalog_held_past_busy_timeout_is_waited_out_at(point: &'static str) {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        write_dense_file(&src.join("a.bin"), 64 * 1024);
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let db = paths.db_file.clone();
        failpoint::on(point, move || {
            let (held, taken) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let other = rusqlite::Connection::open(&db).unwrap();
                other.execute_batch("BEGIN IMMEDIATE").unwrap();
                held.send(()).unwrap();
                // `db::open`'s busy_timeout is 5 s.
                std::thread::sleep(std::time::Duration::from_secs(7));
                other.execute_batch("COMMIT").unwrap();
            });
            taken.recv().unwrap();
        });
        let id = stage_create_reporting(&conn, &paths, &config, snap_id, false, &mut Vec::new())
            .expect("the lock is waited out");
        let status: String = conn
            .query_row(
                "SELECT status FROM stage_sets WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "staged");
    }

    /// A hard-linked file is one inode, which tar stores once (a link member), while the
    /// snapshot's recorded size counts every link — so the figure overstates
    /// the need, and a unit that fits must not be turned away: asked about,
    /// and with `--yes` staged, with a notice carrying the figure.
    #[test]
    fn a_hard_linked_unit_that_fits_is_staged_with_yes_not_refused() {
        use crate::util::format_bytes_binary as fmt;
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        write_dense_file(&src.join("a.bin"), 64 * 1024);
        fs::hard_link(src.join("a.bin"), src.join("b.bin")).unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let snapshot = get_snapshot(&conn, snap_id).unwrap();

        let upper = upper_bound(&conn, &snapshot);
        // Room for the inode once (64 KiB and change), not for both links.
        let free = 100 * 1024;
        assert!(
            free < upper,
            "fixture: the recorded size alone does not fit"
        );
        let _free = FreeSpaceOverride::set(free as u64);

        let mut notices = Vec::new();
        stage_create_reporting(&conn, &paths, &config, snap_id, true, &mut notices)
            .expect("a unit that fits must be staged with --yes, not refused");
        let notices = String::from_utf8(notices).unwrap();
        assert!(
            notices.contains("may be too small for unit \"unit1\"")
                && notices.contains(&format!("needs up to {}", fmt(upper)))
                && notices.contains("a hard-linked file is stored once")
                && notices.contains("staging anyway (--yes given)"),
            "the notice must carry the figure ({}): {notices:?}",
            fmt(upper)
        );
    }

    /// With compression on, the need is the same question: asked about, and
    /// with `--yes` the stage goes ahead with a notice carrying the
    /// uncompressed figure (the refusal without consent is
    /// `with_compression_short_space_is_asked_about_and_refused_without_consent`).
    #[test]
    fn with_compression_short_space_is_staged_with_yes_never_hard_refused() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.defaults.compression = "gzip".to_string();
        write_dense_file(&src.join("big.bin"), 64 * 1024);
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let _free = FreeSpaceOverride::set(1024);

        let mut notices = Vec::new();
        stage_create_reporting(&conn, &paths, &config, snap_id, true, &mut notices)
            .expect("compression makes the need unknowable: --yes proceeds");
        let notices = String::from_utf8(notices).unwrap();
        assert!(
            notices.contains("its data may compress")
                && notices.contains("compression = \"gzip\"")
                && notices.contains("staging anyway (--yes given)"),
            "the notice must say the figure assumes no compression: {notices:?}"
        );
    }

    /// Issue #354 (b), the compressed case: whether the unit fits cannot be
    /// known before tar runs, so a short staging directory is ASKED about
    /// (ADR-0008 Tier 2) — and a non-interactive run without `--yes`
    /// refuses, with the figures, rather than printing a notice and
    /// carrying on. Asked before the source is read: no `stage_sets` row,
    /// nothing in staging.
    #[test]
    fn with_compression_short_space_is_asked_about_and_refused_without_consent() {
        use crate::util::format_bytes_binary as fmt;
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.defaults.compression = "gzip".to_string();
        config.tar.binary = never_archiving_tar();
        write_dense_file(&src.join("big.bin"), 64 * 1024);
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let _free = FreeSpaceOverride::set(1024);

        let mut notices = Vec::new();
        let err = stage_create_reporting(&conn, &paths, &config, snap_id, false, &mut notices)
            .expect_err("no terminal and no --yes: a stage that may not fit is refused");
        let msg = as_operator_sees_it(err);
        assert!(
            msg.contains("stage unit \"unit1\" refused: non-interactive session")
                && msg.contains("re-run with --yes"),
            "through the consent gate: {msg}"
        );
        assert!(
            msg.contains("may be too small for unit \"unit1\"")
                && msg.contains(&format!("{} free", fmt(1024)))
                && msg.contains("its data may compress")
                && msg.contains("compression = \"gzip\""),
            "the refusal carries the figures it would have asked about: {msg}"
        );
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "asked before the stage_sets INSERT");
    }

    /// Issue #354 (b) and #364, `compression = none`: short space may or may
    /// not be enough — asked about before anything is read, and refused
    /// without consent in a non-interactive run: no `stage_sets` row, tar
    /// never ran, staging is left empty. `--yes` is the way past it.
    #[test]
    fn short_staging_space_is_refused_without_consent() {
        use crate::util::format_bytes_binary as fmt;
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.tar.binary = never_archiving_tar();
        write_dense_file(&src.join("a.bin"), 64 * 1024);
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let upper = upper_bound(&conn, &get_snapshot(&conn, snap_id).unwrap());
        let _free = FreeSpaceOverride::set(32 * 1024);

        let mut notices = Vec::new();
        let err = stage_create_reporting(&conn, &paths, &config, snap_id, false, &mut notices)
            .expect_err("no terminal and no --yes: refused");
        let msg = as_operator_sees_it(err);
        assert!(
            msg.contains("stage unit \"unit1\" refused: non-interactive session"),
            "through the consent gate: {msg}"
        );
        assert!(
            msg.contains(&format!("needs up to {}", fmt(upper)))
                && msg.contains(&format!("{} free", fmt(32 * 1024))),
            "the refusal carries the figures: {msg}"
        );
        assert!(!msg.contains("tar-must-never-run"), "before tar: {msg}");
        // Issue #409: it names the fix, since what fills staging is often a
        // failed stage's files or slices already on tape.
        assert!(
            msg.contains("`tapectl stage list`") && msg.contains("`tapectl stage clean`"),
            "names what frees staging: {msg}"
        );
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "asked before the stage_sets INSERT");
        let left: Vec<_> = fs::read_dir(&config.staging.directory)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(left.is_empty(), "nothing left in staging: {left:?}");

        // The way past the refusal it names: the same stage with `--yes`
        // (and a tar that runs).
        config.tar.binary = "tar".to_string();
        let mut notices = Vec::new();
        stage_create_reporting(&conn, &paths, &config, snap_id, true, &mut notices)
            .expect("re-run with --yes, the refused stage proceeds");
        assert_eq!(get_snapshot(&conn, snap_id).unwrap().status, "staged");
    }

    /// Issue #52 change 2 — the self-match trap. `snapshot_create` for an
    /// ordinary, already-registered unit must succeed: the nesting check
    /// must exclude the unit's own row, or `check_path.starts_with(existing)`
    /// is trivially true for a unit against itself (`check_path == existing`)
    /// and every snapshot would fail with "X is inside existing unit X".
    /// Written and run BEFORE the naive (non-excluding) nesting-check wire-up
    /// existed, to prove the trap: with a bare
    /// `nesting::check_nesting(conn, source_path)` call (no exclusion), this
    /// test failed with:
    ///
    /// ```text
    /// called `Result::unwrap()` on an `Err` value: NestedUnit(
    ///     "/tmp/.../src is inside existing unit \"unit1\" at /tmp/.../src",
    /// )
    /// ```
    ///
    /// After wiring `check_nesting_excluding(conn, source_path, Some(unit.id))`
    /// instead, it passes.
    #[test]
    fn snapshot_create_does_not_trip_nesting_check_against_its_own_unit() {
        let tmp = TempDir::new().unwrap();
        let (conn, _paths, _config, src) = setup_unit_with_excludes(&tmp, vec![]);
        fs::write(src.join("f.txt"), b"ordinary file").unwrap();

        let result = snapshot_create(&conn, "unit1", &Config::default());
        assert!(
            result.is_ok(),
            "snapshot_create must not match a unit against its own row: {result:?}"
        );
    }

    /// Issue #52 change 3, design line 184: "unit init and snapshot create
    /// check parent/child. Both errors." Built with raw `INSERT INTO units`
    /// rows (not `init_unit`, which would itself refuse to create the
    /// nested unit) so both a parent and a genuinely nested child unit
    /// exist, and `snapshot_create` on the child must error.
    #[test]
    fn snapshot_create_errors_on_genuinely_nested_unit() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();
        let conn = crate::db::open(&paths.db_file).unwrap();

        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        let tenant_id: i64 = conn
            .query_row("SELECT id FROM tenants WHERE name = 'alice'", [], |r| {
                r.get(0)
            })
            .unwrap();

        let parent = tmp.path().join("parent");
        let child = parent.join("child");
        fs::create_dir_all(&child).unwrap();

        conn.execute(
            "INSERT INTO units (uuid, tenant_id, name, current_path, status)
             VALUES (?1, ?2, 'parent-unit', ?3, 'active')",
            params![
                uuid::Uuid::new_v4().to_string(),
                tenant_id,
                parent.to_string_lossy().to_string()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO units (uuid, tenant_id, name, current_path, status)
             VALUES (?1, ?2, 'child-unit', ?3, 'active')",
            params![
                uuid::Uuid::new_v4().to_string(),
                tenant_id,
                child.to_string_lossy().to_string()
            ],
        )
        .unwrap();

        let result = snapshot_create(&conn, "child-unit", &Config::default());
        assert!(
            matches!(result, Err(TapectlError::NestedUnit(_))),
            "nested unit must error per design line 184, got: {result:?}"
        );
    }

    /// Design line 185: "Empty units: warn but allow." An empty unit still
    /// produces a snapshot row with `file_count = 0` — proving "allow", not
    /// "refuse" — since asserting on `tracing::warn!` output directly is
    /// awkward in this crate's existing test style.
    #[test]
    fn snapshot_create_allows_empty_unit_with_warning() {
        // Built with a raw `INSERT INTO units` row rather than
        // `setup_unit_with_excludes`/`init_unit` — `init_unit` always
        // writes a `.tapectl-unit.toml` dotfile into the unit's directory,
        // and that dotfile is itself a real file `walk_directory` (rightly)
        // counts, so a unit created that way is never genuinely empty.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();
        let conn = crate::db::open(&paths.db_file).unwrap();

        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        let tenant_id: i64 = conn
            .query_row("SELECT id FROM tenants WHERE name = 'alice'", [], |r| {
                r.get(0)
            })
            .unwrap();

        let src = tmp.path().join("empty-src");
        fs::create_dir_all(&src).unwrap();

        conn.execute(
            "INSERT INTO units (uuid, tenant_id, name, current_path, status)
             VALUES (?1, ?2, 'unit1', ?3, 'active')",
            params![
                uuid::Uuid::new_v4().to_string(),
                tenant_id,
                src.to_string_lossy().to_string()
            ],
        )
        .unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &Config::default())
            .expect("empty unit must be allowed, only warned about");

        let file_count: i64 = conn
            .query_row(
                "SELECT file_count FROM snapshots WHERE id = ?1",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            file_count, 0,
            "empty unit's snapshot must record file_count = 0"
        );
    }

    /// Design line 203: "Warns on files > large_file_warn_threshold." A
    /// file over threshold is only warned about, never blocking — the
    /// snapshot succeeds and the manifest still records the file (proving
    /// "warn", not "refuse" or "skip").
    #[test]
    fn snapshot_create_allows_large_file_with_warning() {
        let tmp = TempDir::new().unwrap();
        let (conn, _paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        // Issue #59: "10B" was never a documented valid suffix (only
        // K/KB/M/MB/G/GB/T/TB, or a bare number meaning bytes) — it only
        // "worked" before because an unrecognized suffix silently fell
        // back to multiplier 1.0. A bare "10" is the correct spelling for
        // ten bytes now that an unknown suffix is a hard error.
        config.defaults.large_file_warn_threshold = "10".to_string();
        fs::write(src.join("big.bin"), vec![0u8; 1024]).unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &config)
            .expect("a file over the large-file threshold must only warn, not fail");

        let recorded_size: i64 = crate::db::files::fixture::size(&conn, snap_id, "big.bin");
        assert_eq!(
            recorded_size, 1024,
            "the large file must still be recorded in the manifest, not skipped"
        );
    }

    /// THE core claim of issue #49 (its own re-triage escalation): a unit
    /// must not be permanently blocked from staging by content drift in a
    /// file the stage was never going to archive. Write this FIRST — it must
    /// fail against the pre-#49 code (see the PR report for the captured
    /// pre-fix failure): `walk_directory` used to record every file
    /// unfiltered, so `backfill_checksums` established a sha256 baseline
    /// for the excluded junk file too, and this re-stage's same-size
    /// content drift then tripped `validate_source`'s BITROT refusal for
    /// content the stage was never going to touch.
    #[test]
    fn excluded_junk_file_content_drift_at_stable_size_does_not_false_positive_bitrot() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec!["*.tmp".to_string()]);

        fs::write(src.join("keep.txt"), b"real archival content, kept").unwrap();
        fs::write(src.join("junk.tmp"), b"AAAA").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        stage_create(&conn, &paths, &config, snap_id, false).expect("first stage must succeed");

        // The excluded junk file's content drifts at an UNCHANGED size —
        // exactly the false-BITROT scenario the issue describes
        // (Thumbs.db/*.tmp regenerating at a stable size).
        fs::write(src.join("junk.tmp"), b"BBBB").unwrap();

        // Re-staging the SAME snapshot (a real "stage create" retry) must
        // succeed cleanly — never raise BITROT over content the stage was never
        // going to archive.
        let result = restage(&conn, &paths, &config, snap_id);
        assert!(
            result.is_ok(),
            "re-staging must succeed — an excluded file's content drift must \
             never raise BITROT: {result:?}"
        );
    }

    #[test]
    fn excluded_files_do_not_appear_in_files_table() {
        let tmp = TempDir::new().unwrap();
        let (conn, _paths, _config, src) =
            setup_unit_with_excludes(&tmp, vec!["*.tmp".to_string()]);

        fs::write(src.join("keep.txt"), b"kept content").unwrap();
        fs::write(src.join("junk.tmp"), b"excluded junk").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        let files_count: i64 = crate::db::files::fixture::count(&conn, snap_id, Some("junk.tmp"));
        assert_eq!(files_count, 0, "excluded file must not appear in `files`");

        let kept_count: i64 = crate::db::files::fixture::count(&conn, snap_id, Some("keep.txt"));
        assert_eq!(kept_count, 1, "non-excluded file must still be recorded");
    }

    #[test]
    fn excluded_files_never_receive_a_sha256_baseline() {
        // "Once (3) lands, backfill_checksums naturally stops seeing them
        // — verify that is true rather than assuming" (issue #49). Drives
        // the REAL stage_create pipeline (not just snapshot_create) so
        // backfill_checksums actually runs.
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec!["*.tmp".to_string()]);

        fs::write(src.join("keep.txt"), b"kept content, baselined").unwrap();
        fs::write(src.join("junk.tmp"), b"excluded junk").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        stage_create(&conn, &paths, &config, snap_id, false).unwrap();

        let junk_row_exists: i64 =
            crate::db::files::fixture::count(&conn, snap_id, Some("junk.tmp"));
        assert_eq!(
            junk_row_exists, 0,
            "excluded file must have no `files` row at all, so there is \
             nothing for backfill_checksums to baseline"
        );

        let kept_sha: Option<String> =
            crate::db::files::fixture::sha256(&conn, snap_id, "keep.txt");
        assert!(
            kept_sha.is_some(),
            "the non-excluded file must still get its baseline established"
        );
    }

    /// The paths of `stage_set_id`'s file table (the stream's members).
    fn stream_paths(conn: &Connection, stage_set_id: i64) -> Vec<String> {
        crate::db::stage_files::load(conn, stage_set_id)
            .unwrap()
            .into_iter()
            .map(|r| r.path)
            .collect()
    }

    /// Issue #49 items 2/5, D14: the unit dotfile's own patterns and the
    /// global ones both keep a file out of what is archived. tar is given
    /// the snapshot's exact list, so what they left out is in no member of
    /// the stream.
    #[test]
    fn dotfile_and_global_exclude_patterns_keep_files_out_of_the_stream() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) =
            setup_unit_with_excludes(&tmp, vec!["*.unusual-dotfile-pattern".to_string()]);
        // Item 252: `init` no longer writes a default list; the test names one.
        config.defaults.global_excludes = vec!["Thumbs.db".to_string()];

        fs::write(src.join("keep.txt"), b"kept content").unwrap();
        fs::write(src.join("x.unusual-dotfile-pattern"), b"left out").unwrap();
        fs::write(src.join("Thumbs.db"), b"left out too").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &config).unwrap();
        let stage_set_id = stage_create(&conn, &paths, &config, snap_id, false).unwrap();

        let members = stream_paths(&conn, stage_set_id);
        assert!(
            members.contains(&"keep.txt".to_string()),
            "positive control: {members:?}"
        );
        assert!(
            !members
                .iter()
                .any(|p| p == "x.unusual-dotfile-pattern" || p == "Thumbs.db"),
            "the dotfile's and the global pattern keep their files out of the stream: \
             {members:?}"
        );
    }

    /// Issue #359 (c): a directory pattern (`name/`) keeps the subtree out
    /// of the archive, not only out of the manifest, the `files` table and
    /// the dirty scan. The stream's members are the file table's rows: the
    /// pruned directory itself stays, empty, in the stream and in the
    /// manifest alike; a directory INSIDE it is in neither.
    #[test]
    fn a_directory_exclude_keeps_the_subtree_out_of_the_stream() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) =
            setup_unit_with_excludes(&tmp, vec![".cache/".to_string()]);
        fs::write(src.join("keep.txt"), b"kept content").unwrap();
        fs::create_dir_all(src.join(".cache/deepdir")).unwrap();
        fs::write(src.join(".cache/cached-top.bin"), b"cache bytes").unwrap();
        fs::write(src.join(".cache/deepdir/cached-deep.bin"), b"deeper").unwrap();
        fs::create_dir_all(src.join("sub/.cache")).unwrap();
        fs::write(src.join("sub/.cache/cached-nested.bin"), b"nested").unwrap();
        fs::write(src.join("sub/kept-nested.txt"), b"kept too").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let stage_set_id = stage_create(&conn, &paths, &config, snap_id, false).unwrap();
        let listing = stream_paths(&conn, stage_set_id).join("\n");

        // Positive control: the listing is real and holds what was kept.
        for kept in ["keep.txt", "kept-nested.txt", ".cache"] {
            assert!(listing.contains(kept), "{kept} must be archived: {listing}");
        }
        for pruned in [
            "cached-top.bin",
            "cached-deep.bin",
            "cached-nested.bin",
            "deepdir",
        ] {
            assert!(
                !listing.contains(pruned),
                "`.cache/` must keep {pruned} out of the archive: {listing}"
            );
        }

        // The manifest agrees with the archive, directories included.
        let recorded =
            |path: &str| -> i64 { crate::db::files::fixture::count(&conn, snap_id, Some(path)) };
        assert_eq!(recorded(".cache"), 1, "the pruned directory itself is kept");
        assert_eq!(recorded("sub/.cache"), 1, "at any depth");
        assert_eq!(
            recorded(".cache/deepdir"),
            0,
            "a directory inside a pruned subtree is not recorded, nor archived"
        );
        assert_eq!(recorded(".cache/cached-top.bin"), 0);
    }

    /// ADR-0012 item 297 (#569), which replaces issue #465's refusal: a file
    /// name that is not UTF-8 is recorded by its bytes in the escaped form
    /// (`volume-format-v3.md` 1.5), never lossily, so two names cannot collapse
    /// to one and every stage finds the file. A name inside an excluded
    /// subtree is not recorded.
    #[test]
    fn snapshot_create_records_a_file_name_that_is_not_utf8_escaped() {
        use std::os::unix::ffi::OsStrExt;
        let latin1 = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
        let other = std::ffi::OsStr::from_bytes(b"caf\xe8.txt");
        for (dir, recorded) in [("", "caf\\xe9.txt"), ("sub", "sub/caf\\xe9.txt")] {
            let tmp = TempDir::new().unwrap();
            let (conn, _paths, _config, src) = setup_unit_with_excludes(&tmp, vec![]);
            fs::write(src.join("keep.txt"), b"kept").unwrap();
            fs::create_dir_all(src.join(dir)).unwrap();
            fs::write(src.join(dir).join(latin1), b"latin one").unwrap();
            // Two names that a lossy conversion would make one.
            fs::write(src.join(dir).join(other), b"latin two").unwrap();

            let snap = snapshot_create(&conn, "unit1", &Config::default())
                .expect("a name that is not UTF-8 is recorded");
            assert_eq!(
                crate::db::files::fixture::count(&conn, snap, Some(recorded)),
                1,
                "{dir:?}: recorded escaped"
            );
            let twin = recorded.replace("xe9", "xe8");
            assert_eq!(
                crate::db::files::fixture::count(&conn, snap, Some(&twin)),
                1,
                "{dir:?}: the other name is its own row"
            );
        }

        // The same name under an excluded directory is neither archived nor
        // recorded.
        let tmp = TempDir::new().unwrap();
        let (conn, _paths, _config, src) =
            setup_unit_with_excludes(&tmp, vec![".cache/".to_string()]);
        fs::write(src.join("keep.txt"), b"kept").unwrap();
        fs::create_dir_all(src.join(".cache")).unwrap();
        fs::write(src.join(".cache").join(latin1), b"cache").unwrap();
        let snap = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        assert_eq!(
            crate::db::files::fixture::count(&conn, snap, Some("keep.txt")),
            1
        );
        assert_eq!(
            crate::db::files::fixture::count(&conn, snap, Some(".cache/caf\\xe9.txt")),
            0
        );
    }

    /// A snapshot row to hang stage sets on, in a bare in-memory catalog.
    fn bare_snapshot(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES ('uuid', 'u', ?1, '/tmp/u', 'active')",
            params![tenant_id],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'created', '/tmp/u', 1, 10)",
            params![unit_id],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// Issue #465 item 6: `stage_sets.id` is a bare `INTEGER PRIMARY KEY`, so
    /// SQLite hands out `MAX(id) + 1` and deleting the highest row (`snapshot
    /// delete --force`) made the next stage set reuse its id — and with it
    /// the lock path, the `_s<id>` file prefix, the `events` entity id and
    /// the stage report name of the one that was deleted. A high-water mark
    /// in `meta` (no AUTOINCREMENT: that would rebuild the table) keeps ids
    /// rising.
    #[test]
    fn a_stage_set_id_is_never_reused_after_the_highest_is_deleted() {
        let conn = crate::db::open_memory().unwrap();
        let snapshot_id = bare_snapshot(&conn);
        let insert = || insert_stage_set(&conn, snapshot_id, 1 << 20, "none").unwrap();

        assert_eq!((insert(), insert(), insert()), (1, 2, 3));
        // The highest goes, then the next two: the old behaviour gave 3 back.
        conn.execute("DELETE FROM stage_sets WHERE id = 3", [])
            .unwrap();
        assert_eq!(insert(), 4, "3 was handed out once");
        conn.execute("DELETE FROM stage_sets", []).unwrap();
        assert_eq!(insert(), 5, "an empty table does not restart the count");
    }

    /// The mark never lowers an id the table already holds: rows inserted
    /// another way (a catalog rebuild, a restored backup from before the
    /// mark existed) still win, so ids stay unique.
    #[test]
    fn a_stage_set_id_is_above_every_row_already_in_the_table() {
        let conn = crate::db::open_memory().unwrap();
        let snapshot_id = bare_snapshot(&conn);
        conn.execute(
            "INSERT INTO stage_sets (id, snapshot_id, status, slice_size)
             VALUES (41, ?1, 'staged', 524288)",
            params![snapshot_id],
        )
        .unwrap();
        assert_eq!(
            insert_stage_set(&conn, snapshot_id, 1 << 20, "none").unwrap(),
            42,
            "no mark yet: one above the table's highest"
        );
        conn.execute(
            "UPDATE meta SET value = '10' WHERE key = 'stage_set_id_high_water'",
            [],
        )
        .unwrap();
        assert_eq!(
            insert_stage_set(&conn, snapshot_id, 1 << 20, "none").unwrap(),
            43,
            "a stale mark below the table's highest changes nothing"
        );
    }

    /// Rewrite the unit dotfile's `[excludes]` after the snapshot was taken:
    /// what an operator does between `snapshot create` and `stage create`.
    /// A dotfile edit mints no version (ADR-0012 2026-10-07 item 24).
    fn set_dotfile_excludes(src: &Path, patterns: &[&str]) {
        let dotfile_path = src.join(".tapectl-unit.toml");
        let mut df = crate::unit::dotfile::read_dotfile(&dotfile_path).unwrap();
        df.exclude_patterns = patterns.iter().map(|p| p.to_string()).collect();
        crate::unit::dotfile::write_dotfile(&dotfile_path, &df).unwrap();
    }

    /// Issue #464: a file the snapshot listed and the stage now excludes was
    /// hashed (`backfill_checksums` recorded its sha256) while the archiver skipped it,
    /// so `files` and the on-tape `catalog.db` said the version held a file
    /// the tape did not. The stage is refused before tar starts, naming the
    /// file, and the remedy is a new snapshot, which then stages. A literal
    /// name and a `*.ext` pattern are the two shapes the issue names.
    #[test]
    fn a_file_the_stage_now_excludes_but_the_snapshot_lists_refuses_the_stage() {
        for (file, pattern) in [("secret.txt", "secret.txt"), ("note.ext", "*.ext")] {
            let tmp = TempDir::new().unwrap();
            let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
            fs::write(src.join("keep.dat"), b"kept content").unwrap();
            fs::write(src.join(file), b"listed by the snapshot").unwrap();

            let first = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
            assert_eq!(
                crate::db::files::fixture::count(&conn, first, Some(file)),
                1,
                "{file}: positive control: the snapshot lists it"
            );
            set_dotfile_excludes(&src, &[pattern]);

            let err = stage_create(&conn, &paths, &config, first, false)
                .expect_err("a stage of a snapshot that no longer matches the excludes");
            let msg = err.to_string();
            assert!(msg.contains(file), "{pattern}: names the file: {msg}");
            assert!(
                msg.contains("snapshot create"),
                "{pattern}: names the remedy: {msg}"
            );
            assert_eq!(
                crate::db::files::fixture::sha256(&conn, first, file),
                None,
                "{pattern}: a refused stage baselines nothing for the file"
            );
            let staged: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM stage_sets WHERE snapshot_id = ?1 AND status = 'staged'",
                    params![first],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(staged, 0, "{pattern}: nothing was staged");

            // The remedy: a snapshot under the current excludes stages.
            let second = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
            assert_ne!(
                second, first,
                "{pattern}: the removed file minted a version"
            );
            assert_eq!(
                crate::db::files::fixture::count(&conn, second, Some(file)),
                0,
                "{pattern}: the new snapshot does not list it"
            );
            stage_create(&conn, &paths, &config, second, false)
                .unwrap_or_else(|e| panic!("{pattern}: the fresh snapshot stages: {e}"));
        }
    }

    /// The same refusal for `[defaults] global_excludes`, the other layer of
    /// the stage-time patterns: added to the config after the snapshot.
    #[test]
    fn a_file_the_global_excludes_now_exclude_refuses_the_stage_too() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        fs::write(src.join("keep.dat"), b"kept content").unwrap();
        fs::write(src.join("draft.globalext"), b"listed by the snapshot").unwrap();
        let snap = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        config
            .defaults
            .global_excludes
            .push("*.globalext".to_string());
        let msg = stage_create(&conn, &paths, &config, snap, false)
            .expect_err("the snapshot lists a file the global excludes now exclude")
            .to_string();
        assert!(msg.contains("draft.globalext"), "{msg}");
        assert!(msg.contains("snapshot create"), "{msg}");

        // The remedy, under the same config: the new snapshot stages.
        let fresh = snapshot_create(&conn, "unit1", &config).unwrap();
        stage_create(&conn, &paths, &config, fresh, false).expect("a fresh snapshot stages");
    }

    /// Issue #464, B15 of #542: tapectl's matcher is the only one (D14). A
    /// directory pattern with a wildcard inside its name, `x*z/`, matched
    /// one path component at a time, leaves `xdir/sub/z` in the walk, and
    /// tar, given the exact list, archives exactly what the walk recorded:
    /// the file is a member of the stream and its hash is the stream's.
    /// (The archiver before 2.0 matched `*` across `/`, pruned it, and the
    /// stage was refused.)
    #[test]
    fn a_wildcard_directory_pattern_archives_exactly_what_the_walk_records() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        fs::write(src.join("keep.txt"), b"kept content").unwrap();
        fs::create_dir_all(src.join("xdir/sub/z")).unwrap();
        fs::write(src.join("xdir/sub/z/inside.txt"), b"the walk keeps this").unwrap();

        let snap = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        set_dotfile_excludes(&src, &["x*z/"]);

        let id = stage_create(&conn, &paths, &config, snap, false)
            .expect("one matcher: what the walk records is what tar archives");
        let members = stream_paths(&conn, id);
        assert!(
            members.contains(&"xdir/sub/z/inside.txt".to_string()),
            "{members:?}"
        );
    }

    #[test]
    fn a_unit_with_no_excludes_configured_behaves_exactly_as_before() {
        // Issue #49 trap: "do NOT break units with no excludes configured
        // — the empty-pattern case must behave exactly as today, and is
        // the common case." No dotfile override (init_unit's own default)
        // AND an empty `global_excludes` slice passed explicitly — the true
        // "nothing configured anywhere" case, which must record everything.
        // (The case where `global_excludes` is non-empty but no dotfile
        // exists — the ticket's own headline scenario — is covered
        // separately below, in the "second half" test block; this test's
        // fixture is deliberately a neutral filename, not one that
        // resembles a real default global-exclude pattern, so it stays a
        // clean proof of the empty/empty case rather than depending on
        // `Config::default()`'s specific pattern list.)
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);

        fs::write(src.join("keep.txt"), b"kept content").unwrap();
        fs::write(src.join("media_file.dat"), b"ordinary archival content").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let file_count: i64 = crate::db::files::fixture::count(&conn, snap_id, None);
        // 2 files recorded (keep.txt, media_file.dat) + the dotfile itself
        // (.tapectl-unit.toml, swept up like any other regular file —
        // pre-existing, unrelated behavior this fix does not change).
        assert_eq!(
            file_count, 3,
            "with no dotfile exclude_patterns and no global_excludes, nothing \
             is filtered from the walk — matches pre-#49 behavior exactly"
        );

        stage_create(&conn, &paths, &config, snap_id, false)
            .expect("staging a unit with no excludes at all must succeed exactly as before");
    }

    // ── issue #49 (second half): global excludes must reach both walks ──
    //
    // The ticket's own headline example: `config.defaults.global_excludes`
    // (Thumbs.db/.DS_Store/*.nfo/*.tmp by default) reached the archiver only —
    // neither walk saw it. For a unit with NO dotfile override (the common
    // case), `walk_directory` recorded the globally-excluded file into
    // `files`, `backfill_checksums` gave it a sha256 baseline, and a
    // same-size content regeneration then tripped `validate_source`'s
    // BITROT refusal for content the stage was never going to archive — see the
    // PR report for the captured pre-fix failure. These tests use
    // `setup_unit_with_excludes(&tmp, vec![])` (no dotfile override) and
    // pass the REAL `config.defaults.global_excludes` returned by that
    // helper, so `Thumbs.db` here is the actual default pattern, not a
    // stand-in.

    /// Write this FIRST — it must fail against the pre-fix code (see the PR
    /// report for the captured pre-fix failure output).
    #[test]
    fn global_default_excluded_file_content_drift_at_stable_size_does_not_false_positive_bitrot() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        // Item 252: `init` no longer writes these; the test names its own.
        config.defaults.global_excludes = vec!["Thumbs.db".to_string()];

        fs::write(src.join("keep.txt"), b"real archival content, kept").unwrap();
        fs::write(src.join("Thumbs.db"), b"AAAA").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &config).unwrap();
        stage_create(&conn, &paths, &config, snap_id, false).expect("first stage must succeed");

        // Thumbs.db regenerates at an UNCHANGED size — exactly the
        // false-BITROT scenario the issue describes.
        fs::write(src.join("Thumbs.db"), b"BBBB").unwrap();

        let result = restage(&conn, &paths, &config, snap_id);
        assert!(
            result.is_ok(),
            "re-staging must succeed — a globally-excluded file's content drift \
             must never raise BITROT, even with no dotfile override: {result:?}"
        );
    }

    #[test]
    fn global_default_excluded_files_do_not_appear_in_files_table() {
        let tmp = TempDir::new().unwrap();
        let (conn, _paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        // Item 252: `init` no longer writes these; the test names its own.
        config.defaults.global_excludes = vec!["Thumbs.db".to_string()];

        fs::write(src.join("keep.txt"), b"kept content").unwrap();
        fs::write(src.join("Thumbs.db"), b"thumbnail cache junk").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &config).unwrap();

        let files_count: i64 = crate::db::files::fixture::count(&conn, snap_id, Some("Thumbs.db"));
        assert_eq!(
            files_count, 0,
            "a globally-excluded file must not appear in `files`, even with no \
             dotfile override"
        );

        let kept_count: i64 = crate::db::files::fixture::count(&conn, snap_id, Some("keep.txt"));
        assert_eq!(kept_count, 1, "non-excluded file must still be recorded");
    }

    #[test]
    fn global_default_excluded_files_never_receive_a_sha256_baseline() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        // Item 252: `init` no longer writes these; the test names its own.
        config.defaults.global_excludes = vec!["Thumbs.db".to_string()];

        fs::write(src.join("keep.txt"), b"kept content, baselined").unwrap();
        fs::write(src.join("Thumbs.db"), b"thumbnail cache junk").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &config).unwrap();
        stage_create(&conn, &paths, &config, snap_id, false).unwrap();

        let junk_row_exists: i64 =
            crate::db::files::fixture::count(&conn, snap_id, Some("Thumbs.db"));
        assert_eq!(
            junk_row_exists, 0,
            "a globally-excluded file must have no `files` row at all, so there \
             is nothing for backfill_checksums to baseline"
        );

        let kept_sha: Option<String> =
            crate::db::files::fixture::sha256(&conn, snap_id, "keep.txt");
        assert!(
            kept_sha.is_some(),
            "the non-excluded file must still get its baseline established"
        );
    }

    // ── issue #159 / ADR-0012: a version is minted only when content changed ──

    /// Bare-bones tenant + active unit for the acceptance tests below — no
    /// escrow key, no staging directory, no dotfile: only `snapshot_create`/
    /// `snapshot_create_detailed` is under test here, and neither needs any
    /// of that (only `stage_create` requires a registered escrow
    /// recipient). Each call gets its own unique unit name so a test that
    /// seeds two units in one `conn` never collides.
    fn seed_snapshot_test_unit(conn: &Connection, path: &Path, checksum_mode: &str) -> String {
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t159', 0, 'active')",
            [],
        )
        .ok(); // may already exist across two seeds in one test; ignore
        let tenant_id: i64 = conn
            .query_row("SELECT id FROM tenants WHERE name = 't159'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let name = format!("unit-{}", uuid::Uuid::new_v4());
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, checksum_mode, status)
             VALUES (?1, ?2, ?3, ?4, ?5, 'active')",
            params![
                uuid::Uuid::new_v4().to_string(),
                name,
                tenant_id,
                path.to_string_lossy().to_string(),
                checksum_mode,
            ],
        )
        .unwrap();
        name
    }

    /// Sets a file's mtime back to `mtime` after its content has already
    /// been rewritten — constructs the one case `mtime_size` cannot see:
    /// identical size, identical mtime, different bytes. Mirrors
    /// `collection::fingerprint`'s test helper of the same shape.
    fn restore_mtime_for_snapshot_test(path: &Path, mtime: std::time::SystemTime) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    #[test]
    fn snapshot_create_detailed_reports_the_existing_version_when_content_is_unchanged() {
        let conn = crate::db::open_memory().unwrap();
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), b"hello").unwrap();
        let unit_name = seed_snapshot_test_unit(&conn, tmp.path(), "mtime_size");

        let first = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();
        assert!(first.minted);
        assert_eq!(first.version, 1);

        let second = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();
        assert!(
            !second.minted,
            "unchanged content must not mint a new version"
        );
        assert_eq!(second.snapshot_id, first.snapshot_id);
        assert_eq!(second.version, 1);

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM snapshots WHERE unit_id = \
                 (SELECT id FROM units WHERE name = ?1)",
                params![unit_name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "no second snapshot row must be created");
    }

    /// Issue #206 / the 2026-09-17 pre-production review: this pins what the
    /// minting rule actually guarantees, which is NARROWER than ADR-0012's
    /// concluding sentence claims.
    ///
    /// ADR-0012's rule is scoped to "its latest CURRENT snapshot", and that is
    /// exactly what `snapshot_create_detailed` implements. Its next sentence
    /// then generalises to "two versions of a unit never hold identical
    /// content" — which does not follow. Revert a unit's bytes to those of an
    /// older, superseded version and a fresh version is minted holding content
    /// byte-identical to that old one.
    ///
    /// Recorded as a PIN, not a bug report: the consequence is an under-count
    /// (the new version genuinely has no slices of its own, and
    /// `copy_count_expr`'s minimum is over CURRENT snapshots, so the dead
    /// sibling is correctly not credited). ADR-0012 itself calls an under-count
    /// "the safe direction, but still wrong". Whether the rule should widen to
    /// compare against every live version is a design question, not something
    /// to decide by changing this test.
    #[test]
    fn reverting_to_a_superseded_versions_content_mints_an_identical_sibling() {
        let conn = crate::db::open_memory().unwrap();
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("f.txt");
        std::fs::write(&file, b"content-A").unwrap();
        let unit_name = seed_snapshot_test_unit(&conn, tmp.path(), "mtime_size");

        let v1 = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();
        assert!(v1.minted);

        std::fs::write(&file, b"content-B-which-differs-in-size").unwrap();
        let v2 = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();
        assert!(v2.minted);
        assert_eq!(v2.version, 2);

        // v1 is now superseded; put the source back to exactly v1's bytes.
        std::fs::write(&file, b"content-A").unwrap();
        let v3 = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();

        assert!(
            v3.minted,
            "a fresh version IS minted -- the short-circuit only ever compares \
             against the LATEST snapshot, and that is v2, which differs"
        );
        assert_eq!(v3.version, 3);
        assert_ne!(
            v3.snapshot_id, v1.snapshot_id,
            "v1 is not resurrected; v3 is a distinct row holding identical content"
        );

        // The two rows really do record the same content.
        let stamps =
            |sid: i64| -> Vec<(String, i64)> { crate::db::files::fixture::non_dirs(&conn, sid) };
        assert_eq!(
            stamps(v1.snapshot_id),
            stamps(v3.snapshot_id),
            "v1 and v3 hold byte-identical content, which ADR-0012's concluding \
             sentence says cannot happen"
        );
    }

    #[test]
    fn snapshot_create_detailed_mints_a_new_version_when_a_file_changes() {
        let conn = crate::db::open_memory().unwrap();
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("f.txt");
        std::fs::write(&file, b"hello").unwrap();
        let unit_name = seed_snapshot_test_unit(&conn, tmp.path(), "mtime_size");

        let first = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();
        assert!(first.minted);

        std::fs::write(&file, b"hello, world! now a different size").unwrap();
        let second = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();
        assert!(second.minted, "changed content must mint a new version");
        assert_eq!(second.version, 2);
        assert_ne!(second.snapshot_id, first.snapshot_id);
    }

    #[test]
    fn snapshot_create_detailed_mints_a_new_version_when_a_file_is_removed() {
        // ADR-0012: removal counts as change, not just add/modify.
        let conn = crate::db::open_memory().unwrap();
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("f.txt");
        std::fs::write(&file, b"hello").unwrap();
        std::fs::write(tmp.path().join("keep.txt"), b"keep").unwrap();
        let unit_name = seed_snapshot_test_unit(&conn, tmp.path(), "mtime_size");

        let first = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();
        assert!(first.minted);

        std::fs::remove_file(&file).unwrap();
        let second = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();
        assert!(
            second.minted,
            "a removed file is a content change and must mint a new version"
        );
        assert_eq!(second.version, 2);
    }

    #[test]
    fn snapshot_create_detailed_reuses_an_unwritten_matching_snapshot_instead_of_minting_beside_it()
    {
        // Change 3: the latest snapshot is 'created' (never staged), not
        // 'current' — a match must reuse that row, not mint a
        // byte-identical sibling beside it.
        let conn = crate::db::open_memory().unwrap();
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), b"hello").unwrap();
        let unit_name = seed_snapshot_test_unit(&conn, tmp.path(), "mtime_size");

        let first = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();
        assert!(first.minted);
        assert_eq!(first.status, "created");
        let status: String = conn
            .query_row(
                "SELECT status FROM snapshots WHERE id = ?1",
                params![first.snapshot_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "created", "never staged — status stays 'created'");

        let second = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();
        assert!(!second.minted);
        assert_eq!(second.snapshot_id, first.snapshot_id);
        assert_eq!(second.status, "created");

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM snapshots WHERE unit_id = \
                 (SELECT id FROM units WHERE name = ?1)",
                params![unit_name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "reuse must not leave a second row beside the unwritten one"
        );
    }

    #[test]
    fn snapshot_create_detailed_honors_checksum_mode_on_a_same_size_same_mtime_content_change() {
        // The one edit `mtime_size` cannot catch (issue #36, reused here
        // for #159): same path, same size, same mtime, different bytes.
        // `mtime_size` and `sha256` must disagree about it — each unit
        // follows its own `checksum_mode`, not a global rule.
        let conn = crate::db::open_memory().unwrap();

        let mtime_tmp = TempDir::new().unwrap();
        let mtime_file = mtime_tmp.path().join("f.txt");
        std::fs::write(&mtime_file, b"original content!").unwrap();
        let mtime_unit = seed_snapshot_test_unit(&conn, mtime_tmp.path(), "mtime_size");

        let sha_tmp = TempDir::new().unwrap();
        let sha_file = sha_tmp.path().join("f.txt");
        std::fs::write(&sha_file, b"original content!").unwrap();
        let sha_unit = seed_snapshot_test_unit(&conn, sha_tmp.path(), "sha256");

        let m1 = snapshot_create_detailed(&conn, &mtime_unit, &Config::default()).unwrap();
        let s1 = snapshot_create_detailed(&conn, &sha_unit, &Config::default()).unwrap();
        assert!(m1.minted && s1.minted);

        // Establish the sha256 baseline for the sha-mode unit — what a
        // real `stage_create` would have backfilled via the same
        // `hash_source_file` this scan reuses.
        let (hash, _) = crate::staging::validate::hash_source_file(&sha_file, "f.txt").unwrap();
        crate::db::files::fixture::set_sha256(&conn, s1.snapshot_id, "f.txt", Some(&hash));

        // Same-size, same-mtime content swap on both units.
        let mtime_before = std::fs::metadata(&mtime_file).unwrap().modified().unwrap();
        std::fs::write(&mtime_file, b"REPLACED content!").unwrap();
        restore_mtime_for_snapshot_test(&mtime_file, mtime_before);

        let sha_before = std::fs::metadata(&sha_file).unwrap().modified().unwrap();
        std::fs::write(&sha_file, b"REPLACED content!").unwrap();
        restore_mtime_for_snapshot_test(&sha_file, sha_before);

        let m2 = snapshot_create_detailed(&conn, &mtime_unit, &Config::default()).unwrap();
        let s2 = snapshot_create_detailed(&conn, &sha_unit, &Config::default()).unwrap();

        assert!(
            !m2.minted,
            "mtime_size mode must stay blind to a same-size-same-mtime content \
             change — that tradeoff is documented, not a bug"
        );
        assert!(
            s2.minted,
            "sha256 mode must catch a content change mtime_size cannot see"
        );
    }

    /// ADR-0012, 2026-10-07 item 24 (#378): stage `unit1` once, let
    /// `change` rewrite its dotfile the way an operator command does, then
    /// require that the unit is not Dirty, that `snapshot create` mints
    /// nothing, and that the same Version stages again.
    fn dotfile_change_is_not_content(
        checksum_mode: &str,
        change: impl FnOnce(&Connection, &models::Unit, &Path),
    ) {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        conn.execute(
            "UPDATE units SET checksum_mode = ?1 WHERE name = 'unit1'",
            params![checksum_mode],
        )
        .unwrap();
        fs::write(src.join("a.txt"), b"the unit's real content").unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        stage_create(&conn, &paths, &config, snap_id, false).expect("first stage");
        assert!(
            crate::db::files::fixture::sha256(&conn, snap_id, ".tapectl-unit.toml").is_some(),
            "the dotfile is recorded and hashed like any file at the first stage"
        );

        let unit = queries::get_unit_by_name(&conn, "unit1").unwrap().unwrap();
        // Whole-second mtimes: make sure the rewrite lands in a later second.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        change(&conn, &unit, &src);

        let unit: models::Unit = conn
            .query_row(
                "SELECT name FROM units WHERE id = ?1",
                params![unit.id],
                |r| r.get::<_, String>(0),
            )
            .map(|name| queries::get_unit_by_name(&conn, &name).unwrap().unwrap())
            .unwrap();
        let dirty = crate::collection::fingerprint::classify(&conn, &unit, &[]).unwrap();
        assert!(
            dirty.is_none(),
            "a dotfile rewrite must not make the unit Dirty: {:?}",
            dirty.map(|p| p.changes)
        );
        let again = snapshot_create_detailed(&conn, &unit.name, &Config::default()).unwrap();
        assert!(!again.minted, "a dotfile rewrite mints no Version");
        assert_eq!(again.snapshot_id, snap_id);
        restage(&conn, &paths, &config, snap_id)
            .expect("the same Version stages again after a dotfile rewrite");
    }

    #[test]
    fn a_unit_tag_mints_no_version_and_the_version_restages() {
        dotfile_change_is_not_content("mtime_size", |conn, unit, _| {
            crate::unit::tag_unit(conn, unit, &["keep".to_string()], &[]).unwrap();
        });
    }

    #[test]
    fn a_unit_rename_mints_no_version_and_the_version_restages() {
        dotfile_change_is_not_content("mtime_size", |conn, unit, _| {
            crate::unit::rename_unit(conn, &unit.name, "unit1-renamed").unwrap();
        });
    }

    #[test]
    fn a_policy_edit_mints_no_version_and_the_version_restages() {
        dotfile_change_is_not_content("mtime_size", |_, _, src| {
            let path = src.join(".tapectl-unit.toml");
            let mut df = crate::unit::dotfile::read_dotfile(&path).unwrap();
            df.compression = Some("gzip".to_string());
            df.slice_size = Some("64M".to_string());
            crate::unit::dotfile::write_dotfile(&path, &df).unwrap();
        });
    }

    /// The sha256 checksum mode hashes every file once mtime and size
    /// agree, so a same-size dotfile edit with its mtime put back is the
    /// case only that mode's loop would see.
    #[test]
    fn a_same_size_dotfile_edit_is_not_content_in_sha256_mode() {
        dotfile_change_is_not_content("sha256", |_, _, src| {
            let path = src.join(".tapectl-unit.toml");
            let before = fs::metadata(&path).unwrap().modified().unwrap();
            let text = fs::read_to_string(&path).unwrap();
            let edited = text.replacen("unit1", "UNIT1", 1);
            assert_ne!(text, edited);
            assert_eq!(text.len(), edited.len());
            fs::write(&path, edited).unwrap();
            restore_mtime_for_snapshot_test(&path, before);
        });
    }

    /// A recorded dotfile hash unlike the file on disk is not BITROT: the
    /// file is rewritten by tapectl itself, and is not content.
    #[test]
    fn a_stale_dotfile_baseline_does_not_refuse_a_restage() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        fs::write(src.join("a.txt"), b"the unit's real content").unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        crate::db::files::fixture::set_sha256(
            &conn,
            snap_id,
            ".tapectl-unit.toml",
            Some(&"0".repeat(64)),
        );
        restage(&conn, &paths, &config, snap_id)
            .expect("a stale dotfile baseline must not read as BITROT");
    }

    /// Issue #361: what `stage create` leaves in `<home>/stage-reports/` is
    /// a stage report, in its header as well as its directory — "receipt"
    /// now means only the recipient list a stage set was encrypted to
    /// (CONTEXT.md). The `write_stage_report` tests below pass their own
    /// body; this drives the real producer through a real stage (real tar).
    #[test]
    fn stage_create_writes_a_stage_report_not_a_receipt() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        fs::write(src.join("f.txt"), b"content for the stage report").unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let stage_set_id = stage_create(&conn, &paths, &config, snap_id, false).unwrap();

        let reports: Vec<PathBuf> = fs::read_dir(&paths.stage_reports_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(reports.len(), 1, "one stage, one report: {reports:?}");
        let text = fs::read_to_string(&reports[0]).unwrap();
        // The whole first line, newline included — which is also the form
        // tests/refusal_recipes.rs's command scan passes over (it reads a
        // bare "tapectl stage report" as a call to a `stage report`
        // subcommand that does not exist).
        assert!(
            text.starts_with("tapectl stage report\n"),
            "the header names what the file is: {text}"
        );
        assert!(
            text.contains(&format!("Stage:    {stage_set_id}\n")),
            "positive control: this is the report of the stage just made: {text}"
        );
        assert!(!text.to_lowercase().contains("receipt"), "{text}");
        assert!(
            !text.contains("Phase timings"),
            "no session, no timings section: {text}"
        );
    }

    /// Issue #386: inside a progress session, `stage create` records its
    /// phases in order against the stage set and writes them into the stage
    /// report; archive (tar, the stream's checks and hashes, slicing and
    /// encryption in one pass) counts the bytes of tar's stream.
    #[test]
    fn stage_create_records_its_phases_in_the_catalog_and_the_report() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        let content = b"content whose phases are timed";
        fs::write(src.join("f.txt"), content).unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let session = progress::start_capture();
        let stage_set_id = stage_create(&conn, &paths, &config, snap_id, false).unwrap();
        drop(session);

        let rows = crate::db::phase_timings::latest_for_stage_set(&conn, stage_set_id).unwrap();
        let names: Vec<&str> = rows.iter().map(|r| r.phase.as_str()).collect();
        assert_eq!(names, ["check", "archive", "recheck", "finalize"]);
        assert!(rows
            .iter()
            .all(|r| r.outcome == "ok" && r.operation == "stage create"));
        // tar's stream holds the file and the unit's dotfile, at least.
        assert!(
            rows[1].bytes.unwrap_or(0) >= content.len() as i64,
            "archive counts the stream it reads: {:?}",
            rows[1]
        );

        let report = fs::read_dir(&paths.stage_reports_dir)
            .unwrap()
            .map(|e| fs::read_to_string(e.unwrap().path()).unwrap())
            .next()
            .unwrap();
        let section = report
            .split("\nPhase timings:\n")
            .nth(1)
            .unwrap_or_else(|| panic!("the report has a timings section: {report}"));
        for name in names {
            assert!(section.contains(name), "{name} in:\n{section}");
        }
    }

    /// Issue #370: tar writes nothing under the staging directory. When tar
    /// has finished, the staging directory holds only `.age` ciphertext.
    #[test]
    fn when_tar_finishes_staging_holds_only_ciphertext() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.defaults.slice_size = "64K".to_string();
        fs::write(src.join("noise.bin"), noise(200 * 1024)).unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        failpoint::observe_staging_after_tar();
        stage_create(&conn, &paths, &config, snap_id, false).unwrap();
        let names = failpoint::staging_after_tar().expect("the stage reached tar's end");
        assert!(
            names.len() >= 2,
            "positive control: several slices exist when tar ends: {names:?}"
        );
        for name in &names {
            assert!(
                name.ends_with(".tar.age"),
                "only ciphertext may be in staging when tar ends, found {name}: {names:?}"
            );
        }
    }

    /// Issue #370, §3.1: the recorded tar command is §3.1's line, the
    /// archive on standard output (`--file=-`), the list written `<list>`
    /// (its real path is a name in the home's work directory), and it names
    /// no path under the staging directory, nor the folder tar ran in (a
    /// transient snapshot path means nothing on tape, ADR-0012 item 75).
    #[test]
    fn the_recorded_tar_command_writes_to_stdout_and_names_no_staging_path() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        fs::write(src.join("f.txt"), b"command content").unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let id = stage_create(&conn, &paths, &config, snap_id, false).unwrap();
        let cmd: String = conn
            .query_row(
                "SELECT tar_command FROM stage_sets WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        for arg in [
            r#""--create""#,
            r#""--file=-""#,
            r#""--format=pax""#,
            r#""--numeric-owner""#,
            r#""--files-from=<list>""#,
        ] {
            assert!(cmd.contains(arg), "{arg} in {cmd}");
        }
        let staging = Path::new(&config.staging.directory);
        for spelling in [staging.to_path_buf(), staging.canonicalize().unwrap()] {
            assert!(
                !cmd.contains(&*spelling.to_string_lossy()),
                "tar is given no path under the staging directory: {cmd}"
            );
        }
        assert!(!cmd.contains(&*src.to_string_lossy()), "{cmd}");
    }

    /// Issue #370, invariant 7: a source file that changes while tar reads
    /// it refuses the stage as DIRTY (tar's exit 1, or tapectl's own
    /// before-and-after comparison, whichever sees it), and no slice is left
    /// in staging.
    #[test]
    fn a_file_changing_while_tar_reads_it_refuses_the_stage_as_dirty() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        let busy = src.join("busy.bin");
        fs::write(&busy, noise(48 << 20)).unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        // Touch the file without changing its size or content, as fast as
        // possible, for as long as the stage runs.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let toucher = {
            let stop = stop.clone();
            let busy = busy.clone();
            std::thread::spawn(move || {
                let f = fs::File::options().write(true).open(&busy).unwrap();
                let mut t = std::time::SystemTime::now();
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    t += std::time::Duration::from_millis(1);
                    let _ = f.set_modified(t);
                }
            })
        };
        let result = stage_create(&conn, &paths, &config, snap_id, false);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        toucher.join().unwrap();

        let err = result.expect_err("a file changing under tar refuses the stage");
        assert!(err.to_string().contains("DIRTY"), "{err}");
        // Issue #343: tar's report of the refused run is kept — the run an
        // operator most needs to read later. Which of the two checks saw
        // the change first decides how tar ended: tar's own (exit 1), or
        // tapectl's comparison after a clean exit.
        let (outcome, code, stderr): (String, Option<i64>, Vec<u8>) = conn
            .query_row(
                "SELECT outcome, tar_exit_code, tar_stderr FROM tar_create_reports",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("one tar report row");
        assert!(
            ["files_changed", "complete"].contains(&outcome.as_str()),
            "{outcome}"
        );
        if outcome == "files_changed" {
            assert_eq!(code, Some(1));
            assert!(!stderr.is_empty(), "tar said why");
        }
        let left: Vec<_> = fs::read_dir(&config.staging.directory)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(left.is_empty(), "nothing is left in staging: {left:?}");
    }

    /// Issue #343: a clean stage keeps tar's report too — exit 0, the
    /// command, the version — against its stage set.
    #[test]
    fn a_clean_stage_keeps_tars_report_against_its_stage_set() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        fs::write(src.join("a.txt"), b"some content").unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let stage_set_id = stage_create(&conn, &paths, &config, snap_id, false).unwrap();
        let row: (Option<i64>, String, String, Option<i64>, String, String) = conn
            .query_row(
                "SELECT stage_set_id, unit_name, outcome, tar_exit_code, tar_command,
                        tar_version
                   FROM tar_create_reports",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .expect("one tar report row");
        assert_eq!(row.0, Some(stage_set_id));
        assert_eq!(row.1, "unit1");
        assert_eq!((row.2.as_str(), row.3), ("complete", Some(0)));
        assert!(row.4.contains("\"--create\""), "{}", row.4);
        assert!(row.5.starts_with("tar (GNU tar) "), "tar's version is kept");
    }

    /// Issue #364, invariant 7: the sha256 recorded for a file must be of
    /// the bytes archived. A file rewritten in place once tar has read it
    /// (same size, other bytes: a test hook at the member's row, not a race)
    /// is refused as DIRTY by tapectl's own before-and-after comparison,
    /// whatever tar said.
    #[test]
    fn a_file_rewritten_after_it_is_hashed_refuses_the_stage() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        let target = src.join("f.bin");
        fs::write(&target, noise(64 * 1024)).unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        let hook_target = target.clone();
        archive::row_hook::on("f.bin", move || {
            // Same size, other bytes: an in-place edit.
            let mut other = noise(64 * 1024);
            other.reverse();
            std::thread::sleep(std::time::Duration::from_millis(20));
            fs::write(&hook_target, other).unwrap();
        });
        let result = stage_create(&conn, &paths, &config, snap_id, false);

        assert_ne!(
            fs::read(&target).unwrap(),
            noise(64 * 1024),
            "positive control: the hook rewrote the file"
        );
        let err = result.expect_err("a file changed after its hash must refuse the stage");
        let msg = err.to_string();
        assert!(msg.contains("DIRTY") && msg.contains("f.bin"), "{msg}");
        let left: Vec<_> = fs::read_dir(&config.staging.directory)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(left.is_empty(), "nothing is left in staging: {left:?}");
    }

    /// Issue #364: BITROT is found while tar runs now, not before it — and
    /// still refuses the stage, stops tar, and leaves staging empty, with
    /// the baseline untouched.
    #[test]
    fn bitrot_found_while_tar_runs_refuses_the_stage() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.defaults.slice_size = "64K".to_string();
        fs::write(src.join("a.bin"), noise(256 * 1024)).unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let stale = "0".repeat(64);
        crate::db::files::fixture::set_sha256(&conn, snap_id, "a.bin", Some(&stale));

        let err = stage_create(&conn, &paths, &config, snap_id, false)
            .expect_err("a hash unlike its baseline at the same size refuses the stage");
        assert!(err.to_string().contains("BITROT suspected: a.bin"), "{err}");
        let left: Vec<_> = fs::read_dir(&config.staging.directory)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(left.is_empty(), "nothing is left in staging: {left:?}");
        let baseline: String = crate::db::files::fixture::sha256(&conn, snap_id, "a.bin").unwrap();
        assert_eq!(baseline, stale, "the baseline is untouched");
    }

    /// Incompressible bytes, deterministic.
    fn noise(len: usize) -> Vec<u8> {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    /// Issue #409: every `.age` reaches stable storage BEFORE its
    /// `stage_slices` row is written. Driven through a real stage (real tar)
    /// cut into several slices; the order is read off the pipeline's own
    /// durability log. Since issue #370 there is no plaintext slice to
    /// delete: none is ever written (`when_tar_finishes_staging_holds_only_ciphertext`).
    #[test]
    fn every_slice_is_synced_before_it_is_recorded() {
        use durability::Event;

        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.defaults.slice_size = "64K".to_string();
        // Incompressible, so the archive spans several slices.
        fs::write(src.join("noise.bin"), noise(200 * 1024)).unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        let _ = durability::take();
        let stage_set_id = stage_create(&conn, &paths, &config, snap_id, false).unwrap();
        let log = durability::take();

        let ages: Vec<PathBuf> = conn
            .prepare(
                "SELECT staging_path FROM stage_slices WHERE stage_set_id = ?1
                 ORDER BY slice_number",
            )
            .unwrap()
            .query_map(params![stage_set_id], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|p| PathBuf::from(p.unwrap()))
            .collect();
        assert!(
            ages.len() >= 2,
            "positive control: the stage was cut into several slices, got {}",
            ages.len()
        );
        let at = |e: &Event| {
            log.iter()
                .position(|x| x == e)
                .unwrap_or_else(|| panic!("{e:?} is not in the durability log: {log:?}"))
        };
        for age in &ages {
            let synced = at(&Event::Synced(age.clone()));
            let recorded = at(&Event::Recorded(age.clone()));
            assert!(
                synced < recorded,
                "{}: synced at {synced}, recorded at {recorded} — the sync must come \
                 first: {log:?}",
                age.display()
            );
        }
    }

    /// Issue #41: `write_stage_report` tested directly and hermetically — no
    /// archiver, no full `stage_create`
    /// pipeline — per the same reasoning `crypto::keys`'s tests already
    /// apply to secret keys: a permission bug belongs to the function that
    /// sets (or fails to set) the mode, not to everything that happens to
    /// call it three layers up.
    mod file_custody {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn mode_of(path: &Path) -> u32 {
            fs::metadata(path).unwrap().permissions().mode() & 0o777
        }

        fn test_paths(tmp: &TempDir) -> TapectlPaths {
            let paths = TapectlPaths::new(tmp.path().join(".tapectl"));
            paths.ensure_dirs().unwrap();
            paths
        }

        #[test]
        fn write_stage_report_creates_file_at_0600() {
            let tmp = TempDir::new().unwrap();
            let paths = test_paths(&tmp);

            let path = write_stage_report(&paths, 42, "report body\n").unwrap();

            assert!(path.exists());
            assert_eq!(fs::read_to_string(&path).unwrap(), "report body\n");
            assert_eq!(mode_of(&path), 0o600, "stage report should be 0600");
        }

        /// Issue #465 item 6: a second report for the same stage set id on
        /// the same day (an id reused after `snapshot delete --force`, or a
        /// restored catalog) used to truncate the first. The first report
        /// keeps its name and its bytes; the second takes a suffix.
        #[test]
        fn a_second_stage_report_for_the_same_id_and_day_does_not_truncate_the_first() {
            let tmp = TempDir::new().unwrap();
            let paths = test_paths(&tmp);

            let first = write_stage_report(&paths, 5, "first report\n").unwrap();
            let second = write_stage_report(&paths, 5, "second report\n").unwrap();
            let third = write_stage_report(&paths, 5, "third report\n").unwrap();

            assert_eq!(fs::read_to_string(&first).unwrap(), "first report\n");
            assert_eq!(fs::read_to_string(&second).unwrap(), "second report\n");
            assert_eq!(fs::read_to_string(&third).unwrap(), "third report\n");
            let names: std::collections::BTreeSet<_> = [&first, &second, &third]
                .iter()
                .map(|p| p.file_name().unwrap().to_owned())
                .collect();
            assert_eq!(names.len(), 3, "three reports, three names");
            // The documented name (`<YYYYMMDD>_<stage set id>.txt`) is the
            // first one's.
            let first_name = first.file_name().unwrap().to_string_lossy().into_owned();
            assert!(first_name.ends_with("_5.txt"), "{first_name}");
            for p in [&first, &second, &third] {
                assert_eq!(mode_of(p), 0o600);
            }
        }

        #[test]
        fn write_stage_report_creates_stage_reports_dir_if_missing() {
            let tmp = TempDir::new().unwrap();
            // Deliberately do NOT call ensure_dirs — this exercises
            // write_stage_report's own fs::create_dir_all.
            let paths = TapectlPaths::new(tmp.path().join(".tapectl"));
            assert!(!paths.stage_reports_dir.exists());

            let path = write_stage_report(&paths, 7, "body").unwrap();

            assert!(path.exists());
            // Issue #361: under <home>/stage-reports/, not <home>/receipts/.
            assert_eq!(
                path.parent().unwrap(),
                tmp.path().join(".tapectl").join("stage-reports")
            );
            assert_eq!(mode_of(&path), 0o600);
        }
    }

    // --- issue #372: the checksum backfill is linear ---

    /// `EXPLAIN QUERY PLAN` detail lines for `sql`, with every parameter
    /// bound to NULL.
    fn query_plan(conn: &Connection, sql: &str) -> Vec<String> {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let n = stmt.parameter_count();
        let nulls: Vec<Option<i64>> = vec![None; n];
        stmt.query_map(rusqlite::params_from_iter(nulls), |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// The pin: each backfill UPDATE finds its row by the version's primary
    /// key, after one lookup of the path in `paths`' UNIQUE(unit_id, path)
    /// index. In 1.x a second UPDATE, into the per-version manifest, searched
    /// by the version alone and scanned the whole manifest per file.
    #[test]
    fn backfill_checksums_searches_the_indexes() {
        let conn = crate::db::open_memory().unwrap();
        let plan = query_plan(&conn, crate::db::files::BACKFILL_SQL);
        assert_eq!(
            plan,
            vec![
                "SEARCH file_versions USING PRIMARY KEY (snapshot_id=? AND path_id=?)",
                "SCALAR SUBQUERY 1",
                "SEARCH paths USING COVERING INDEX sqlite_autoindex_paths_1 (unit_id=? AND path=?)",
            ],
            "the backfill must be indexed lookups only"
        );
    }

    /// Timing-independent linearity: the backfill never takes a full-scan
    /// step, and the virtual-machine steps it spends per file do not grow
    /// with the snapshot's size. A per-file scan (1.x's manifest shape) makes
    /// the per-file cost proportional to the file count.
    #[test]
    fn backfill_checksums_costs_the_same_per_file_at_any_snapshot_size() {
        fn steps_per_file(files: i64) -> (f64, i32) {
            let conn = crate::db::open_memory().unwrap();
            conn.execute_batch(
                "INSERT INTO tenants (id, name, is_operator, status) VALUES (1, 't', 1, 'active');
                 INSERT INTO units (id, uuid, name, tenant_id) VALUES (1, 'u', 'u', 1);
                 INSERT INTO snapshots (id, unit_id, version, source_path) VALUES (1, 1, 1, '/s');
                 INSERT INTO snapshots (id, unit_id, version, source_path) VALUES (2, 1, 2, '/s');",
            )
            .unwrap();
            let tx = conn.unchecked_transaction().unwrap();
            let mut checksums = Vec::new();
            for sid in [1i64, 2] {
                for f in 0..files {
                    let path = format!("d{}/f{f:06}", f % 17);
                    crate::db::files::fixture::insert(&tx, sid, &path, 1, "regular", None);
                    if sid == 2 {
                        checksums.push((path, format!("{f:064x}")));
                    }
                }
            }
            tx.commit().unwrap();

            let mut stmt = conn.prepare(crate::db::files::BACKFILL_SQL).unwrap();
            for (path, hash) in &checksums {
                let hash = crate::db::files::sha256_from_hex(hash).unwrap();
                stmt.execute(params![hash.as_slice(), 2i64, 1i64, path])
                    .unwrap();
            }
            let vm = stmt.get_status(rusqlite::StatementStatus::VmStep);
            let fullscan = stmt.get_status(rusqlite::StatementStatus::FullscanStep);
            let done: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM file_versions WHERE snapshot_id = 2 AND sha256 IS NOT NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(done, files, "positive control: every file was baselined");
            (vm as f64 / files as f64, fullscan)
        }

        let (small, small_scan) = steps_per_file(500);
        let (large, large_scan) = steps_per_file(4_000);
        assert_eq!((small_scan, large_scan), (0, 0), "no full-scan step, ever");
        assert!(
            large <= small * 1.25,
            "per-file cost grew with the snapshot: {small:.1} steps/file at 500 files, \
             {large:.1} at 4000"
        );

        // Positive control: the same measurement on an UPDATE that finds its
        // row by the version alone and scans the version's files for the
        // path (the shape of the per-version list this replaced, searched
        // by its version only) sees the per-file cost grow with the list.
        fn old_steps_per_file(files: i64) -> f64 {
            let conn = crate::db::open_memory().unwrap();
            conn.execute_batch(
                "CREATE TABLE version_list (snapshot_id INTEGER NOT NULL, path TEXT NOT NULL,
                                            sha256 TEXT);
                 CREATE INDEX version_list_snapshot ON version_list(snapshot_id);",
            )
            .unwrap();
            let tx = conn.unchecked_transaction().unwrap();
            for f in 0..files {
                tx.execute(
                    "INSERT INTO version_list (snapshot_id, path) VALUES (1, ?1)",
                    params![format!("f{f:06}")],
                )
                .unwrap();
            }
            tx.commit().unwrap();
            let mut stmt = conn
                .prepare(
                    "UPDATE version_list SET sha256 = ?1
                     WHERE snapshot_id = ?2 AND path = ?3 AND sha256 IS NULL",
                )
                .unwrap();
            for f in 0..files {
                stmt.execute(params!["h", 1i64, format!("f{f:06}")])
                    .unwrap();
            }
            stmt.get_status(rusqlite::StatementStatus::VmStep) as f64 / files as f64
        }
        let (old_small, old_large) = (old_steps_per_file(500), old_steps_per_file(4_000));
        assert!(
            old_large > old_small * 4.0,
            "the measurement must see a per-file scan: {old_small:.1} vs {old_large:.1}"
        );
    }

    // --- issue #374: a snapshot is all of its rows or none ---

    /// A failure injected partway through the insert loop (a trigger that
    /// aborts the third `files` INSERT, standing in for a Ctrl-C or a busy
    /// catalog) leaves no snapshot row, no `files` row and no event: the
    /// next `snapshot create` mints v1 as if nothing had happened.
    #[test]
    fn snapshot_create_failing_mid_insert_leaves_no_snapshot() {
        let tmp = TempDir::new().unwrap();
        let (conn, _paths, _config, src) = setup_unit_with_excludes(&tmp, vec![]);
        for i in 0..6 {
            fs::write(src.join(format!("f{i}.txt")), format!("content {i}")).unwrap();
        }
        conn.execute_batch(
            "CREATE TEMP TRIGGER inject_mid_insert BEFORE INSERT ON file_versions
             WHEN (SELECT COUNT(*) FROM file_versions WHERE snapshot_id = NEW.snapshot_id) >= 2
             BEGIN SELECT RAISE(ABORT, 'injected mid-insert failure'); END;",
        )
        .unwrap();

        let err = snapshot_create(&conn, "unit1", &Config::default()).unwrap_err();
        assert!(err.to_string().contains("injected"), "{err}");
        for (what, sql) in [
            ("snapshots", "SELECT COUNT(*) FROM snapshots"),
            ("file_versions", "SELECT COUNT(*) FROM file_versions"),
            ("paths", "SELECT COUNT(*) FROM paths"),
            (
                "snapshot events",
                "SELECT COUNT(*) FROM events WHERE entity_type = 'snapshot'",
            ),
        ] {
            let n: i64 = conn.query_row(sql, [], |r| r.get(0)).unwrap();
            assert_eq!(n, 0, "a failed snapshot create left {n} {what} row(s)");
        }

        conn.execute_batch("DROP TRIGGER inject_mid_insert")
            .unwrap();
        let outcome = snapshot_create_detailed(&conn, "unit1", &Config::default()).unwrap();
        assert!(outcome.minted);
        assert_eq!(
            outcome.version, 1,
            "the failed attempt must not burn a version"
        );
        let files: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM file_versions WHERE snapshot_id = ?1 AND kind <> 0",
                params![outcome.snapshot_id],
                |r| r.get(0),
            )
            .unwrap();
        let recorded: i64 = conn
            .query_row(
                "SELECT file_count FROM snapshots WHERE id = ?1",
                params![outcome.snapshot_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(files, recorded, "the retry records the whole file list");
        assert!(files >= 6, "positive control: {files} files");
    }

    /// `stage create` refuses a snapshot whose `files` rows are fewer than
    /// its `file_count` (one minted before #374 and interrupted), naming
    /// both numbers and the recovery, before any stage set or tar run.
    #[test]
    fn stage_create_refuses_a_snapshot_with_a_short_file_list() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.tar.binary = never_archiving_tar();
        for i in 0..4 {
            fs::write(src.join(format!("f{i}.txt")), format!("content {i}")).unwrap();
        }
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let file_count: i64 = conn
            .query_row(
                "SELECT file_count FROM snapshots WHERE id = ?1",
                params![snap_id],
                |r| r.get(0),
            )
            .unwrap();
        // What an interrupted pre-#374 `snapshot create` left behind.
        crate::db::files::fixture::delete(&conn, snap_id, "f3.txt");

        let msg = stage_create(&conn, &paths, &config, snap_id, false)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("is incomplete"), "{msg}");
        assert!(
            msg.contains(&format!("records {file_count} file(s)")),
            "{msg}"
        );
        assert!(
            msg.contains(&format!("holds {} file row(s)", file_count - 1)),
            "{msg}"
        );
        assert!(
            msg.contains("tapectl snapshot delete unit1 --version 1")
                && msg.contains("tapectl snapshot create unit1"),
            "the message names the recovery: {msg}"
        );
        assert!(!msg.contains("tar-must-never-run"), "{msg}");
        let stage_sets: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stage_sets, 0, "the refusal precedes the stage_sets INSERT");
    }

    /// Issue #402 as item 129 amends it: a slice released on its own
    /// (`released_at` set, its path cleared, its file gone) is not missing;
    /// one with no `released_at` whose path is NULL, or whose file is gone,
    /// still is.
    #[test]
    fn staged_slice_problems_accepts_a_released_slice_and_refuses_an_unreleased_missing_one() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES ('u', 'u', 1, '/tmp/u', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path) VALUES (1, 1, 'staged', '/tmp/u')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size, num_slices)
             VALUES (1, 'staged', 524288, 3)",
            [],
        )
        .unwrap();
        let dir = TempDir::new().unwrap();
        let present = dir.path().join("s3.age");
        fs::write(&present, b"abc").unwrap();
        let gone = dir.path().join("s2.age");
        for (n, path, released) in [
            (1, None, true),
            (2, Some(gone.to_string_lossy().to_string()), false),
            (3, Some(present.to_string_lossy().to_string()), false),
        ] {
            conn.execute(
                "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                            sha256_plain, sha256_encrypted, staging_path,
                                            released_at)
                 VALUES (1, ?1, 3, 3, 'p', ?2, ?3,
                         CASE WHEN ?4 THEN datetime('now') END)",
                params![n, format!("e{n}"), path, released],
            )
            .unwrap();
        }

        let problems = staged_slice_problems(&conn, 1).unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].starts_with("slice 2:"),
            "an unreleased missing file is still refused: {problems:?}"
        );

        conn.execute(
            "UPDATE stage_slices SET staging_path = NULL WHERE slice_number = 2",
            [],
        )
        .unwrap();
        assert_eq!(
            staged_slice_problems(&conn, 1).unwrap(),
            ["slice 2: no staged file recorded"],
            "a NULL path with no released_at is still refused"
        );

        conn.execute(
            "UPDATE stage_slices SET released_at = datetime('now') WHERE slice_number = 2",
            [],
        )
        .unwrap();
        assert!(
            staged_slice_problems(&conn, 1).unwrap().is_empty(),
            "released slices are accepted, and slice 3's file is whole"
        );
    }

    /// The one-drive config of the #478 admission tests: a virtual LTO-6 of
    /// 1 MB at a 100% ceiling and no reserve, so a 2 MiB unit is larger than
    /// one tape.
    fn one_small_drive(config: &mut Config) {
        config.backends.lto.push(crate::config::LtoBackendConfig {
            name: "small".into(),
            device_tape: "/dev/null".into(),
            device_sg: "/dev/null".into(),
            generation: "LTO-6".into(),
            capacity_override: Some("1M".into()),
            fill_ceiling: 1.0,
            enospc_buffer: "0".into(),
            post_warning_bytes: None,
        });
    }

    /// #493 (ADR-0012 item 148, the coordinator's freeze mechanics):
    /// `snapshot create` and `stage create` freeze the unit dotfile's own
    /// `[policy.copies_at]` into `units.copies_at_frozen`, including on a
    /// snapshot that mints nothing (a dotfile edit is not content, #378). The
    /// frozen map is what `policy::resolve` answers while the dotfile is
    /// absent, an absent dotfile at a freeze point leaves it as it is, a
    /// present dotfile without the key outranks it at once and clears it at
    /// the next freeze point. Each change is one `copies_at_frozen` event.
    #[test]
    fn snapshot_and_stage_create_freeze_the_dotfiles_own_copies_at() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        conn.execute_batch(
            "INSERT INTO locations (name) VALUES ('home');
             INSERT INTO locations (name) VALUES ('offsite');",
        )
        .unwrap();
        fs::write(src.join("a.txt"), b"alpha").unwrap();
        let dotfile = src.join(".tapectl-unit.toml");
        let frozen = || {
            conn.query_row(
                "SELECT copies_at_frozen FROM units WHERE name = 'unit1'",
                [],
                |r| r.get::<_, Option<String>>(0),
            )
            .unwrap()
        };
        let set_copies_at = |pairs: Option<&[(&str, i64)]>| {
            let mut df = crate::unit::dotfile::read_dotfile(&dotfile).unwrap();
            df.copies_at = pairs.map(|m| m.iter().map(|(n, c)| (n.to_string(), *c)).collect());
            crate::unit::dotfile::write_dotfile(&dotfile, &df).unwrap();
        };
        let resolved = || {
            let unit = queries::get_unit_by_name(&conn, "unit1").unwrap().unwrap();
            crate::policy::resolve(&conn, &config, &unit)
                .unwrap()
                .copies_at
        };
        let map = |pairs: &[(&str, i64)]| -> std::collections::BTreeMap<String, i64> {
            pairs.iter().map(|(n, c)| (n.to_string(), *c)).collect()
        };
        assert_eq!(frozen(), None, "nothing is frozen before a freeze point");

        set_copies_at(Some(&[("home", 1), ("offsite", 1)]));
        let snap = snapshot_create(&conn, "unit1", &config).unwrap();
        assert_eq!(
            frozen().as_deref(),
            Some(r#"{"home":1,"offsite":1}"#),
            "snapshot"
        );

        set_copies_at(Some(&[("home", 2)]));
        let again = snapshot_create_detailed(&conn, "unit1", &config).unwrap();
        assert!(
            !again.minted,
            "positive control: a dotfile edit mints nothing"
        );
        assert_eq!(
            frozen().as_deref(),
            Some(r#"{"home":2}"#),
            "an unminted snapshot"
        );

        set_copies_at(Some(&[("offsite", 3)]));
        stage_create(&conn, &paths, &config, snap, true).unwrap();
        assert_eq!(frozen().as_deref(), Some(r#"{"offsite":3}"#), "stage");

        let saved = fs::read_to_string(&dotfile).unwrap();
        fs::remove_file(&dotfile).unwrap();
        assert_eq!(
            resolved(),
            map(&[("offsite", 3)]),
            "absent: the frozen layer"
        );
        snapshot_create(&conn, "unit1", &config).unwrap();
        assert_eq!(
            frozen().as_deref(),
            Some(r#"{"offsite":3}"#),
            "an absent dotfile at a freeze point changes nothing"
        );

        fs::write(&dotfile, saved).unwrap();
        set_copies_at(None);
        assert_eq!(
            resolved(),
            map(&[]),
            "present without the key: silent at once"
        );
        assert!(frozen().is_some(), "...and not yet cleared");
        snapshot_create(&conn, "unit1", &config).unwrap();
        assert_eq!(frozen(), None, "cleared at the next freeze point");
        snapshot_create(&conn, "unit1", &config).unwrap();

        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE action = 'copies_at_frozen'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            events, 4,
            "set, changed twice, cleared; an unchanged map is not rewritten"
        );
    }

    /// ADR-0012 items 94 and 288: a unit is admitted whatever its size
    /// against one tape — here larger than the only
    /// configured drive's cartridge, a staging area larger than it — and
    /// stages.
    #[test]
    fn stage_create_admits_a_unit_larger_than_one_tape() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        fs::write(src.join("big.dat"), vec![7u8; 2 * 1024 * 1024]).unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        one_small_drive(&mut config);
        let _total = TotalSpaceOverride::set(1 << 40);

        stage_create(&conn, &paths, &config, snap_id, false)
            .expect("a unit larger than one tape is admitted and staged");
        let status: String = conn
            .query_row(
                "SELECT status FROM stage_sets WHERE snapshot_id = ?1",
                params![snap_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "staged");
    }

    /// ADR-0012 items 85 and 288 (ruling X14): a unit larger than the
    /// staging filesystem's TOTAL size is
    /// refused by name, before the INSERT and before tar, saying why (it is
    /// staged whole) and what would change it (staging grows). The positive
    /// control is the same unit with a staging area one byte larger than it,
    /// which passes this check (and tar, pointed nowhere, then fails).
    #[test]
    fn stage_create_refuses_a_unit_larger_than_the_staging_area() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        fs::write(src.join("big.dat"), vec![7u8; 2 * 1024 * 1024]).unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        config.tar.binary = never_archiving_tar();
        let size: u64 = conn
            .query_row(
                "SELECT total_size FROM snapshots WHERE id = ?1",
                params![snap_id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap() as u64;

        let total = TotalSpaceOverride::set(size - 1);
        let msg = stage_create(&conn, &paths, &config, snap_id, false)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains(&format!(
                "unit \"unit1\" is {size} bytes"
            )),
            "{msg}"
        );
        assert!(
            msg.contains("larger than the staging area")
                && msg.contains(&format!("is {} bytes", size - 1))
                && msg.contains("in all"),
            "names the staging area's total size: {msg}"
        );
        assert!(msg.contains("staged whole"), "says why: {msg}");
        assert!(
            msg.contains("once the staging area is larger than the unit"),
            "says what would change it: {msg}"
        );
        assert!(!msg.contains("tar-must-never-run"), "{msg}");
        let stage_sets: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stage_sets, 0, "the refusal precedes the stage_sets INSERT");

        drop(total);
        let _total = TotalSpaceOverride::set(size);
        let msg = stage_create(&conn, &paths, &config, snap_id, false)
            .unwrap_err()
            .to_string();
        assert!(
            !msg.contains("larger than the staging area"),
            "a unit no larger than the staging area passes the check: {msg}"
        );
    }

    /// The real figure behind the override: the staging filesystem's total
    /// size is at least its free space, and a directory not yet created is
    /// measured by the one it will be created in.
    #[test]
    fn staging_total_bytes_reads_the_filesystem_of_the_nearest_existing_directory() {
        let tmp = TempDir::new().unwrap();
        let total = staging_total_bytes(tmp.path()).unwrap();
        let free = staging_free_bytes(tmp.path()).unwrap();
        assert!(total > 0 && total >= free, "total {total}, free {free}");
        assert_eq!(
            staging_total_bytes(&tmp.path().join("not/yet/made")).unwrap(),
            total
        );
    }

    /// Issue #479 (ADR-0012 2026-10-17 item 100): with a measured
    /// post-warning room on a configured drive, `stage create` refuses a
    /// slice size whose slice plus the seal does not fit in half of it,
    /// before the `stage_sets` INSERT and before tar. Two routes to the same
    /// refusal: `[defaults]`, and the unit's own dotfile (which the
    /// config-level check never sees). Absent key and a slice size that fits
    /// are the controls.
    #[test]
    fn stage_create_refuses_a_slice_size_the_post_warning_room_cannot_hold() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        fs::write(src.join("f.dat"), vec![7u8; 4096]).unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let drive = |room: Option<&str>| crate::config::LtoBackendConfig {
            name: "hp".into(),
            device_tape: "/dev/null".into(),
            device_sg: "/dev/null".into(),
            generation: "LTO-6".into(),
            capacity_override: None,
            fill_ceiling: 1.0,
            enospc_buffer: "0".into(),
            post_warning_bytes: room.map(str::to_string),
        };
        // 60 MB of room: half is 30,000,000 bytes, of which the seal takes
        // 16 MiB; a 100 MiB slice cannot be sealed after the warning.
        config.backends.lto.push(drive(Some("60M")));
        config.defaults.slice_size = "100M".into();
        let real_tar = std::mem::replace(&mut config.tar.binary, never_archiving_tar());

        let msg = stage_create(&conn, &paths, &config, snap_id, false)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("unit \"unit1\"")
                && msg.contains("backends.lto[\"hp\"].post_warning_bytes = 60M (60000000 bytes)")
                && msg.contains("30000000 bytes")
                && msg.contains("Lower it to at most")
                && msg.contains("Nothing was staged."),
            "names the unit, the key, the drive and the numbers: {msg}"
        );
        assert!(!msg.contains("tar-must-never-run"), "{msg}");
        let stage_sets: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stage_sets, 0, "the refusal precedes the stage_sets INSERT");

        // The unit's own dotfile overrides a `[defaults]` that fits: the
        // size in force is what is checked.
        config.defaults.slice_size = "1M".into();
        let dotfile = src.join(".tapectl-unit.toml");
        let mut df = crate::unit::dotfile::read_dotfile(&dotfile).unwrap();
        df.slice_size = Some("100M".into());
        crate::unit::dotfile::write_dotfile(&dotfile, &df).unwrap();
        let msg = stage_create(&conn, &paths, &config, snap_id, false)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("unit \"unit1\"") && msg.contains("post_warning_bytes"),
            "a dotfile slice_size is refused too: {msg}"
        );

        // Control: the same 100M slice size with the key absent stages (real
        // tar), so the refusals above were the key's and nothing else's.
        config.defaults.slice_size = "100M".into();
        df.slice_size = None;
        crate::unit::dotfile::write_dotfile(&dotfile, &df).unwrap();
        config.tar.binary = real_tar;
        config.backends.lto.clear();
        config.backends.lto.push(drive(None));
        stage_create(&conn, &paths, &config, snap_id, false)
            .expect("with no post_warning_bytes nothing is checked");
    }
}
