pub mod clean;
pub mod exclude;
mod files;
pub mod lock;
#[cfg(test)]
mod plaintext_audit;
pub mod validate;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use tracing::info;

use crate::config::{Config, TapectlPaths};
use crate::dar;
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
/// exact predicate `unit status --dirty`/`report dirty`/`audit` already use
/// to decide *Dirty* (`collection::fingerprint::classify`) — against the
/// walk this function has *already* performed (`walked`, below).
/// It is never re-walked: a second `walk_directory`/`WalkDir` pass here
/// would silently double every snapshot's wall-clock time, which is the
/// exact regression issue #159 warns against.
///
/// `config.defaults.global_excludes` (issue #49 item 5) is passed through
/// to `walk_directory` so the recorded `files` rows never include
/// a file dar itself was never going to archive (the unit's own dotfile
/// excludes are read internally by `walk_directory`, keyed off
/// `source_path`). `config` also supplies `defaults.large_file_warn_threshold`
/// for the large-file warning (issue #52, design line 203).
pub fn snapshot_create_detailed(
    conn: &Connection,
    unit_name: &str,
    config: &Config,
) -> Result<SnapshotOutcome> {
    let global_excludes = &config.defaults.global_excludes;
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;

    let source_path = unit
        .current_path
        .as_deref()
        .ok_or_else(|| TapectlError::Other(format!("unit \"{unit_name}\" has no path")))?;

    if !Path::new(source_path).is_dir() {
        return Err(TapectlError::UnitPathNotFound(source_path.to_string()));
    }

    // Nested unit detection (design line 184): "unit init and snapshot
    // create check parent/child. Both errors." Excludes the unit's own
    // row (change 3, issue #52) so a snapshot of an already-registered
    // unit doesn't trip on itself. Runs before the directory walk to fail
    // fast, before doing expensive work.
    crate::unit::nesting::check_nesting_excluding(conn, source_path, Some(unit.id))?;

    // Walk directory and build manifest — the ONLY walk this function
    // performs. Both the content-match short-circuit immediately below and
    // the mint path that follows it reuse `walked` (issue #159).
    let (total_size, file_count, walked) = walk_directory(source_path, global_excludes)?;

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
                modified_at: e.mtime.clone(),
            })
            .collect();
        fresh_stamps.sort();

        let diff = crate::unit::content_match::matches_snapshot(
            conn,
            latest_id,
            &unit.checksum_mode,
            Path::new(source_path),
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

            // `files` is the one per-file record (migration 027 dropped the
            // write-only `manifests`/`manifest_entries` duplicate, issue
            // #372). Mode/uid/gid live in dar's own catalogue, which is what
            // a restore reads.
            {
                let mut file_insert = tx.prepare(
                    "INSERT INTO files (snapshot_id, path, size_bytes, modified_at, is_directory,
                                        file_type, link_target)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?;
                for entry in &walked {
                    file_insert.execute(params![
                        snapshot_id,
                        entry.path,
                        entry.size,
                        entry.mtime,
                        entry.is_dir,
                        entry.file_type,
                        entry.link_target,
                    ])?;
                }
            }

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
/// - a slice row whose `staging_path` is NULL — a `staging clean` that an
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
pub(crate) fn staged_slice_problems(conn: &Connection, stage_set_id: i64) -> Result<Vec<String>> {
    let num_slices: Option<i64> = conn.query_row(
        "SELECT num_slices FROM stage_sets WHERE id = ?1",
        params![stage_set_id],
        |row| row.get(0),
    )?;
    let rows: Vec<(i64, i64, Option<String>)> = conn
        .prepare(
            "SELECT slice_number, encrypted_bytes, staging_path FROM stage_slices
             WHERE stage_set_id = ?1 ORDER BY slice_number",
        )?
        .query_map(params![stage_set_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
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
    for (number, expected, path) in rows {
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
/// #402) — `volume write` refuses such a set and `staging status` reports
/// it, and both name this. The identical ciphertext pulled back from an
/// in-service sealed copy when there is one (`read-slices` records every
/// slice's path again); otherwise, for an active unit, the set released and
/// the version staged again from source.
pub(crate) fn incomplete_set_remedy(
    conn: &Connection,
    unit_id: i64,
    unit_name: &str,
    version: i64,
    unit_status: &str,
) -> Result<String> {
    let from = crate::policy::coverage::in_service_copy_of_version(conn, unit_id, version)?;
    Ok(match from {
        Some(from) => format!("tapectl volume read-slices --from {from} --unit {unit_name}"),
        None if unit_status == "active" => format!(
            "tapectl staging clean --unit {unit_name} --version {version} --force && \
             tapectl stage create {unit_name} --version {version}"
        ),
        None => format!(
            "no sealed copy to read back, and unit \"{unit_name}\" is not active so it cannot \
             be staged again — investigate with `tapectl catalog locate {unit_name}`"
        ),
    })
}

/// Full stage pipeline: validate → dar → encrypt → checksums.
///
/// Everything that can refuse without touching the source runs first,
/// before the `stage_sets` INSERT (issue #354): the escrow recipient, the
/// tenant's and operator's keys, and the staging directory (created, then
/// proved writable). A staging directory too small for the unit is refused
/// after the sha256 pass and before dar: that pass reads every byte, and
/// nothing short of reading the content bounds what dar stores (see
/// [`check_staging_space`]). One that MAY be too small — the need cannot be
/// pinned down — is asked about (ADR-0008 Tier 2): `assume_yes` (the global
/// `--yes`) is that consent given in advance, and without it a
/// non-interactive run refuses. What remains to fail after dar is the work
/// itself.
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
///   returns), and `staging clean` reclaims it.
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
    stage_set_id_holder: &std::cell::Cell<Option<i64>>,
    lock_holder: &std::cell::Cell<Option<lock::StageLock>>,
    notices: &mut dyn Write,
) -> Result<i64> {
    // ADR-0005 / issue #115. First thing, before the `stage_sets` INSERT and
    // long before dar: without a registered escrow recipient,
    // `recipient_list_with_escrow` below is a silent no-op and every slice
    // this run produces is encrypted to tenant + operator only. Those slices
    // are exactly what the escrow line exists to be able to open, and
    // `volume write`'s pre-flight now refuses them
    // (`LayoutError::StageSetLacksEscrow`) — so staging them at all would
    // spend hours of dar + age on material that cannot be written and cannot
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
    check_file_list_complete(conn, &snapshot, &unit.name)?;
    let tenant = queries::get_tenant_by_id(conn, unit.tenant_id)?
        .ok_or_else(|| TapectlError::Other("tenant not found".into()))?;

    // Issue #354 (c): the recipient list is a pure database read, so it is
    // built — and every refusal it can raise is raised — here, before the
    // `stage_sets` INSERT and before dar, beside the escrow check above. It
    // used to be built at the top of the encryption loop, after dar had
    // archived the whole unit, so a tenant with no active keys cost a full
    // dar run to discover.
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
    // encryption, after dar had started. The encryptor itself is rebuilt
    // per slice (age draws a fresh file key each time); this one is only the
    // check.
    build_encryptor(&all_pubkeys)?;

    // Resolve policy (dotfile > archive_set > defaults) — issue #47/#48:
    // stage_create used to read config.defaults.* unconditionally, so an
    // archive_set or dotfile override of slice_size/compression/preserve_*
    // was silently discarded even where #48 gives archive_set_id a writer.
    let resolved = crate::policy::resolve(conn, config, &unit)?;

    // `ResolvedPolicy.slice_size` is bytes-only at every layer (even
    // `policy::resolve`'s own default layer runs `config.defaults.slice_size`
    // through `parse_size_to_bytes`), but dar's `-s` argument must keep
    // receiving a *string dar parses itself* — see `resolve_slice_size_string`
    // for why this can't simply be `resolved.slice_size.to_string()`
    // unconditionally (issue #59's known parser defects must never reach
    // real on-tape slicing — only the bookkeeping column below, which is
    // `resolved.slice_size` directly now, with no second parse needed).
    let slice_size = resolve_slice_size_string(conn, config, &unit, resolved.slice_size);
    let compression = resolved.compression.clone();

    // Issue #354: the staging directory is usable, and big enough or the
    // operator agreed to try, or the stage is refused here — before the
    // INSERT and dar. `staging` is the only way this stage creates a file
    // there, and the only files it can create are ciphertext (issue #370).
    let staging_dir = Path::new(&config.staging.directory);
    let staging = files::StagingDir::prepare(staging_dir)?;
    check_staging_space(
        &StagingSpaceInputs {
            staging_dir,
            unit_name: &unit.name,
            snapshot: &snapshot,
            compression: &compression,
            assume_yes,
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
    // rest of this function's comments warn about (the dar run below stays
    // entirely outside any transaction, exactly as before).
    let insert_tx = conn.unchecked_transaction()?;
    insert_tx.execute(
        "INSERT INTO stage_sets (snapshot_id, slice_size, compression, encrypted)
         VALUES (?1, ?2, ?3, 1)",
        params![snapshot_id, resolved.slice_size, compression],
    )?;
    let stage_set_id = insert_tx.last_insert_rowid();
    stage_set_id_holder.set(Some(stage_set_id));

    let stage_lock = lock::acquire(&paths.db_file, stage_set_id)?;
    lock_holder.set(Some(stage_lock));

    insert_tx.commit()?;

    // Step 1: the source against the snapshot, by metadata alone (issue
    // #364). A missing file or one at another size still refuses before dar.
    // The content is read once, hashed as dar reads it (step 2), and the two
    // reads are tied together afterwards (step 3). Issue #386: each step is a
    // named phase — live progress, the session log, and a `phase_timings`
    // row plus a stage-report line.
    let phase = progress::phase("check", None);
    phase.item(unit.name.clone());
    let plan = validate::plan(
        conn,
        snapshot_id,
        &snapshot.source_path,
        &config.defaults.global_excludes,
    )?;
    phase.done();

    // Step 2: Run dar
    //
    // `archive_base` is per-*stage-set* (issue #53): it carries
    // `stage_set_id` so two stage sets of the SAME snapshot (a re-stage
    // after `staging clean` released the first one) never write
    // identically-named `.age` files and silently overwrite each other.
    // `cleanup_failed_stage_set`'s prefix derivation below must move in
    // lockstep with this — both go through `archive_base_name`.
    let archive_base = archive_base_name(&unit.uuid, snapshot.version, stage_set_id);

    // Issue #49 items 2/5: dar's -X masks must see BOTH layers of
    // "effective excludes" — config.defaults.global_excludes (today's only
    // source) AND the unit's own dotfile `[excludes] patterns` (until this
    // fix, read/written but never consumed here). stage_create already has
    // both `config` and the snapshot's own `source_path` in scope, so this
    // merge is fully local — no threading through other callers needed
    // (contrast walk_directory/walk_fingerprint's dotfile-only interim
    // state; see exclude::dotfile_patterns's doc comment for why).
    let mut dar_exclude_patterns = config.defaults.global_excludes.clone();
    dar_exclude_patterns.extend(exclude::dotfile_patterns(Path::new(&snapshot.source_path))?);
    // Issue #359 (c): split into dar's two mask kinds, so what dar archives
    // is exactly what `walk_directory` recorded — a plain pattern is a `-X`
    // basename mask as before, and a directory pattern (`name/`) becomes the
    // `-P` prune masks `name` and `*/name`. Handed to `-X` raw, `name/`
    // matched nothing in dar and the subtree reached tape uncatalogued.
    let dar_masks = exclude::dar_masks(&dar_exclude_patterns);

    // Issue #419: one isolated catalogue per STAGE SET, produced by this
    // stage set's own dar run. dar draws a fresh random data-name label for
    // every run and refuses (`dar -A`) a catalogue whose label is another
    // run's, so the catalogue this run's envelopes carry must be this run's.
    // It used to be extracted once per snapshot and reused by every later
    // stage set of it, so a re-staged unit's tape carried a catalogue that
    // could not rescue its slices. dar writes it on the fly (`-@`) into the
    // tapectl home — never the staging directory — and it is re-isolated
    // uncompressed below. The file name inside the per-stage-set directory
    // is the one envelopes have always carried (`{uuid8}_v{V}.1.dar`).
    let catalog_dir = stage_set_catalogue_dir(paths, &unit.uuid, stage_set_id);
    let catalog_base = catalog_dir.join(format!("{}_v{}", &unit.uuid[..8], snapshot.version));
    let on_fly_base = catalog_dir.join(ON_FLY_CATALOGUE);
    // Issue #41: a fresh, nested `create_dir_all` gets whatever the umask
    // hands out; `catalogs_dir` itself is secured by `ensure_dirs`, but a
    // parent's mode does not propagate to children it didn't create.
    fs::create_dir_all(&catalog_dir)?;
    if let Some(unit_dir) = catalog_dir.parent() {
        crate::config::secure_path(unit_dir, 0o700);
    }
    crate::config::secure_path(&catalog_dir, 0o700);

    // Step 2: dar, slices and encryption in one pass, with no plaintext on
    // the staging device (issue #370; ADR-0012, 2026-10-06 amendment item
    // 4). dar writes its archive to standard output; `dar::slice::cut_stream`
    // frames it into dar slices exactly as dar frames its own (what
    // `dar_xform -s` makes of the same stream), and each slice is encrypted
    // in memory straight into its `.age` (`files::StagingDir`). Memory is a
    // 1 MiB read buffer and age's 64 KiB chunk, whatever the slice size.
    // The slice header is dar's own: from a tiny archive of an empty
    // directory at the same `-s`, made in the tapectl home, never staging.
    let template = {
        let work = home_work_dir(paths, ".dar-slice-template-")?;
        dar::slice::template(&config.dar.binary, &slice_size, work.path())?
    };

    // fingerprint == public_key by construction for every key in this system
    // (see crypto::keys::generate_keypair and `key import`), so the recorded
    // fingerprints are exactly the (now escrow-augmented) recipient list —
    // keeping this audit record honest about who can actually decrypt the
    // slices it describes, rather than a second, silently-divergent list.
    let key_fingerprints = all_pubkeys.clone();

    let phase = progress::phase("archive", Some(plan.total_bytes()));
    phase.item(unit.name.clone());
    // The source's sha256s, read beside dar and within
    // `validate::READ_AHEAD_BYTES` of it, so each file leaves the disk once
    // (issue #364). Stopped and joined on every way out.
    // `[staging] hash_threads` files at once (issue #366), handed out in dar's
    // read order and recorded in it.
    let hashing = validate::ConcurrentHash::spawn(
        PathBuf::from(&snapshot.source_path),
        plan,
        validate::hash_threads(config.staging.hash_threads),
    )?;
    let mut dar_run = dar::create::spawn_archive(&dar::create::DarCreateParams {
        dar_binary: &config.dar.binary,
        source_path: Path::new(&snapshot.source_path),
        compression: &compression,
        exclude_patterns: &dar_masks.exclude,
        exclude_paths: &dar_masks.prune,
        preserve_xattrs: resolved.preserve_xattrs,
        preserve_fsa: resolved.preserve_fsa,
        on_fly_catalogue: &on_fly_base,
    })?;
    // MANIFEST.toml carries this to tape. The slicing is tapectl's, so the
    // record says so, in terms an heir can reproduce with dar's own tools.
    let dar_command = format!(
        "{} | tapectl cuts the archive into dar slices of -s {slice_size} \
         (the framing `dar_xform -s {slice_size} - <base>` gives) and age-encrypts each",
        dar_run.dar_command
    );
    conn.execute(
        "UPDATE stage_sets SET dar_version = ?1, dar_command = ?2 WHERE id = ?3",
        params![dar_run.dar_version, dar_command, stage_set_id],
    )?;

    let mut total_dar_size: i64 = 0;
    let mut total_encrypted_size: i64 = 0;
    let mut streamed: u64 = 0;
    let dar_pid = dar_run.pid();
    if let Some(pid) = dar_pid {
        hashing.ahead().dar_started(pid);
    }
    let mut last_pace = std::time::Instant::now();
    let stdout = dar_run.take_stdout();
    let cut = dar::slice::cut_stream(
        stdout,
        &template,
        |n| {
            phase.item(format!("{} slice {n}", unit.name));
            staging.create_slice(&archive_base, n, &all_pubkeys)
        },
        |n, slice| {
            let path = slice.path().to_path_buf();
            let info = slice.finish()?;
            record_encrypted_slice(
                conn,
                BusyPolicy::DEFAULT,
                stage_set_id,
                i64::from(n),
                &path,
                &info,
            )?;
            total_dar_size += info.plain_size;
            total_encrypted_size += info.encrypted_size;
            info!(
                slice = n,
                plain_mb = info.plain_size / (1024 * 1024),
                encrypted_mb = info.encrypted_size / (1024 * 1024),
                "staged slice"
            );
            Ok(())
        },
        |bytes| {
            progress::add_bytes(bytes.saturating_sub(streamed));
            streamed = bytes;
            // Keep dar within the read-ahead of the hasher (issue #364): how
            // much of the source dar has read is its own read count when the
            // kernel shows it, else the archive's length (equal without
            // compression). Waiting here fills dar's pipe, and dar waits.
            if last_pace.elapsed() >= std::time::Duration::from_millis(20) {
                last_pace = std::time::Instant::now();
                let read = dar_pid.and_then(dar::create::bytes_read).unwrap_or(bytes);
                hashing.ahead().dar_has_read(read);
            }
            if hashing.ahead().hasher_failed() {
                return Err(TapectlError::Other("the source check failed".into()));
            }
            // Issue #404: a stop is honoured within one read of the stream
            // (and, while dar is silent, within `DarOutput`'s poll).
            crate::signal::check(|| stopped_archiving(&snapshot.source_path))
        },
    );
    let summary = match cut {
        Ok(summary) => summary,
        Err(e) => {
            // dar's own failure, if it had one, is the cause to report: a
            // short archive is how it reaches the slicer. A refusal from
            // the source check outranks both.
            let dar_failed = dar_run.abort();
            if crate::signal::is_interrupted() {
                return Err(TapectlError::Interrupted(stopped_archiving(
                    &snapshot.source_path,
                )));
            }
            if hashing.ahead().hasher_failed() {
                hashing.finish()?;
            }
            return Err(dar_failed.unwrap_or(e));
        }
    };
    if let dar::create::DarFinish::FilesChanged { detail } = dar_run.finish()? {
        return Err(TapectlError::Other(format!(
            "DIRTY: a source file of unit \"{}\" changed while dar was reading it \
             (dar exit 11). tapectl runs dar with --retry-on-change 0, so a file \
             caught mid-change is refused rather than archived half-changed. dar \
             said:\n{detail}\nNothing was staged. Stage again once the source is \
             quiet; if the change is real, take a new snapshot (`tapectl snapshot \
             create {}`) and stage that.",
            unit.name, unit.name
        )));
    }
    // dar has read everything; the hasher finishes on its own.
    hashing.ahead().dar_finished();
    let hashed = hashing.finish()?;
    phase.done();
    info!(slices = summary.slices, "dar archive staged");

    #[cfg(test)]
    failpoint::note_staging(staging_dir);
    #[cfg(test)]
    failpoint::hit(failpoint::AFTER_DAR)?;

    // Step 3 (issue #364): tie the hash to what dar read. Every hashed file
    // must still have the size and change time it had when it was hashed —
    // a write between the two reads moves the change time — and dar ran
    // with `--retry-on-change 0`, so a write during its own read was exit
    // 11 above. Only then is the source validated.
    let phase = progress::phase("recheck", None);
    validate::recheck(Path::new(&snapshot.source_path), &hashed)?;
    conn.execute(
        "UPDATE stage_sets SET source_validated_at = datetime('now') WHERE id = ?1",
        params![stage_set_id],
    )?;
    let checksums = hashed.checksums();
    phase.done();

    // Step 3: this run's catalogue, uncompressed (ADR-0012, 2026-10-06
    // amendment item 3: envelope catalogues stay uncompressed, so an heir's
    // dar needs no bzip2). `-@` always compresses; isolating the isolated
    // catalogue again keeps its label and drops the compression.
    let phase = progress::phase("catalog", None);
    info!("isolating this stage set's dar catalogue");
    dar::create::reisolate_catalogue(&config.dar.binary, &on_fly_base, &catalog_base)?;
    let on_fly_file = PathBuf::from(format!("{}.1.dar", on_fly_base.display()));
    fs::remove_file(&on_fly_file)
        .map_err(|e| staging_io_error("cannot remove", &on_fly_file, e))?;
    // dar wrote the catalog file itself via subprocess, with no mode of its
    // own — tighten what it produced after the fact.
    secure_catalog_files(&catalog_dir);
    // ...and nor did it sync it (issue #409).
    sync_catalogue(&catalog_base)?;
    conn.execute(
        "UPDATE stage_sets SET catalog_path = ?1 WHERE id = ?2",
        params![catalog_base.to_string_lossy().to_string(), stage_set_id],
    )?;
    phase.done();

    // Issue #54: finalization only, not the whole pipeline, runs inside a
    // transaction — matching the `conn.unchecked_transaction()` pattern
    // already established at `src/volume/session.rs:778`,
    // `src/volume/write.rs:1262`, `src/cli/operations.rs:38`
    // (`snapshot_purge`), and `src/cli/key.rs:224`.
    //
    // Deliberately NOT wrapping the whole of `stage_create`: `unchecked_transaction`
    // is DEFERRED, so it takes SQLite's single write lock at the first write
    // inside it and holds it until commit — around the dar run (which can
    // take hours) that would block every other tapectl invocation for the
    // duration. Worse, a crash mid-dar would roll back the `stage_sets`
    // INSERT itself, destroying the `status='staging'` row that
    // `recover_orphaned_sessions` (`src/db/mod.rs`) relies on to mark the
    // set `'failed'` on the next `db::open()` — the operator's only signal
    // that something went wrong. So the incremental progress writes above
    // (the initial INSERT, the `source_validated_at`/`dar_version`/
    // `catalog_path` UPDATEs, and the per-slice `stage_slices` INSERTs)
    // stay outside any transaction; only the finalization below — which
    // only ever runs once the pipeline has fully succeeded — is atomic.
    //
    // Issue #377: IMMEDIATE (the snapshot guard reads before it writes),
    // and retried as a whole for minutes on a busy catalog — every slice is
    // already encrypted, and a 5-second lock wait must not throw them away.
    // A dropped transaction rolls back, and every statement here is guarded
    // or idempotent, so each attempt starts clean.
    let phase = progress::phase("finalize", None);
    busy::retry(BusyPolicy::DEFAULT, "the stage set's finalization", || {
        let tx = busy::immediate_tx(conn)?;

        // Update stage_set
        tx.execute(
            "UPDATE stage_sets SET status = 'staged', num_slices = ?1, total_dar_size = ?2,
             total_encrypted_size = ?3, key_fingerprints = ?4, staged_at = datetime('now')
             WHERE id = ?5",
            params![
                i64::from(summary.slices),
                total_dar_size,
                total_encrypted_size,
                serde_json::to_string(&key_fingerprints).unwrap(),
                stage_set_id,
            ],
        )?;

        // Update snapshot status
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

        // Backfill sha256 into files — establishes
        // the baseline ONLY where one doesn't already exist (issue #32/H6).
        // `validate_source` now refuses to stage (BITROT) before we ever
        // get here if a hash disagrees with an existing baseline, but the
        // thing that actually makes "(first stage only)" true is
        // `backfill_checksums`'s own `sha256 IS NULL` guard on its
        // UPDATE — not this `is_empty()` check, which only skips a no-op
        // call.
        if !checksums.is_empty() {
            backfill_checksums(&tx, snapshot_id, &checksums)?;
        }

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

/// Record one encrypted slice: its `stage_slices` row (issue #377). The
/// INSERT is retried on a busy catalog for `policy`'s budget. If it cannot be
/// written, the `.age` is removed again: no row would ever name it. There is
/// no plaintext slice to delete (issue #370) — only ciphertext was written.
fn record_encrypted_slice(
    conn: &Connection,
    policy: BusyPolicy,
    stage_set_id: i64,
    slice_num: i64,
    encrypted_path: &Path,
    info: &EncryptedSliceInfo,
) -> Result<()> {
    let recorded = busy::retry(policy, "a staged slice", || {
        Ok(conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                       sha256_plain, sha256_encrypted, staging_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                stage_set_id,
                slice_num,
                info.plain_size,
                info.encrypted_size,
                info.sha256_plain,
                info.sha256_encrypted,
                encrypted_path.to_string_lossy().to_string(),
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
/// shape is computed, so `stage_create_inner`'s dar run and
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

/// `archive_base_name(..)` plus the load-bearing trailing dot: dar names
/// every slice `{base}.{N}.dar`, so the real filesystem prefix is
/// `{base}.`, not the bare base — without the dot, `_s1` would prefix-match
/// `_s10.1.dar`.
pub(crate) fn archive_base_prefix(unit_uuid: &str, version: i64, stage_set_id: i64) -> String {
    format!("{}.", archive_base_name(unit_uuid, version, stage_set_id))
}

/// A fresh work directory under `<home>/tmp` (0700), removed when the
/// returned value drops — for the plaintext a stage or a write needs for a
/// moment (dar's slice template, the write's `catalog.db`), which must not
/// go under staging (ADR-0012 2026-10-06 item 4) nor to a world-readable
/// `/tmp`.
pub(crate) fn home_work_dir(paths: &TapectlPaths, prefix: &str) -> Result<tempfile::TempDir> {
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
    format!("dar was stopped while archiving {source}")
}

/// The base name dar's on-the-fly catalogue (`-@`) is written under, inside
/// [`stage_set_catalogue_dir`]; removed once it is re-isolated.
const ON_FLY_CATALOGUE: &str = "onfly";

/// The directory holding one stage set's isolated dar catalogue (issue
/// #419): `<home>/catalogs/{uuid8}/s{stage_set_id}/`. Per stage set, because
/// each dar run labels its archive afresh and dar accepts only that run's
/// catalogue against it.
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
/// - **By filesystem prefix.** The `.dar.age` being written when the stage
///   failed has no `stage_slices` row yet (a slice is recorded once it is
///   complete and synced; issue #370 writes it straight from dar's stream).
///   [`files::SliceWriter`] removes its own file when dropped unfinished,
///   so this catches only what that could not. Plaintext `.dar`/`.sha512`
///   are matched too: tapectl no longer writes them, but a staging
///   directory can still hold them from a version that did.
///   `archive_base_name` (issue #53) carries `stage_set_id`, so the
///   dot-terminated prefix is unique per stage set and can never match a
///   sibling stage set of the same snapshot.
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
                && (name.ends_with(".dar.age")
                    || name.ends_with(".dar")
                    || name.ends_with(".sha512"))
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

    // Issue #419: this stage set's own isolated catalogue (and dar's
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
/// Factored out of `stage_create` so it's testable without a real dar
/// binary or a full stage pipeline.
fn write_stage_report(paths: &TapectlPaths, stage_set_id: i64, report: &str) -> Result<PathBuf> {
    let report_path = paths.stage_reports_dir.join(format!(
        "{}_{}.txt",
        chrono::Utc::now().format("%Y%m%d"),
        stage_set_id
    ));
    fs::create_dir_all(&paths.stage_reports_dir)?;
    crate::config::write_private_file(&report_path, report.as_bytes(), 0o600)?;
    Ok(report_path)
}

/// Best-effort tighten every regular file dar's `-C` catalog extraction
/// wrote inside `catalog_dir` to 0600.
///
/// dar writes these itself via subprocess, so unlike `write_private_file`
/// there's no `open()` call under our control to set the mode at creation
/// time — this tightens what dar produced after the fact instead. Same
/// content-metadata exposure as issue #41's `tapectl.db`/stage reports, just
/// produced by an external process. Non-fatal by design (`secure_path`):
/// a directory listing failure here must not sink an otherwise-successful
/// stage.
fn secure_catalog_files(catalog_dir: &Path) {
    let Ok(entries) = fs::read_dir(catalog_dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            crate::config::secure_path(&entry.path(), 0o600);
        }
    }
}

/// The literal STRING to hand `dar -s` for this unit's stage, resolved
/// through the SAME dotfile > archive_set > default priority as
/// `policy::resolve` — but never by reformatting an already-parsed byte
/// count back into a suffixed string. `resolved_bytes` must be
/// `policy::resolve(..).slice_size` for the same `unit`, so the
/// archive_set fallback below is always consistent with whatever the
/// resolver already decided.
///
/// Why this isn't simply `resolved_bytes.to_string()` unconditionally:
/// `parse_size_to_bytes` (issue #59) silently maps any suffix outside
/// K/KB/M/MB/G/GB/T/TB to a multiplier of 1 — a real defect that today is
/// confined to the `stage_sets.slice_size` *bookkeeping* column, which
/// nothing downstream trusts for the real cut (dar re-parses its own `-s`
/// argument independently). Routing dar's actual argument through that
/// same parser — even indirectly, via a byte count computed from it —
/// would let that defect reach real on-tape slice boundaries for every
/// unit, including the overwhelmingly common case with no override at
/// all. So: whichever layer has a native operator-facing string (the
/// dotfile's raw TOML value, or the system default's config string) hands
/// that string to dar untouched, exactly as before issue #47. Only the
/// archive_set layer has no string to fall back on — `archive_sets.slice_size`
/// has been byte-typed in the schema since M6, so `resolved_bytes` is its
/// only representation — but handing dar that exact byte count with no
/// suffix is lossless and valid syntax: dar's own manual states a bare
/// `-s` number means exactly that many bytes ("'20M' means 20 megabytes,
/// by default, it is the same as giving 20971520 as argument").
fn resolve_slice_size_string(
    conn: &Connection,
    config: &Config,
    unit: &models::Unit,
    resolved_bytes: i64,
) -> String {
    // Layer 1 (highest priority): the unit dotfile's own [policy] slice_size.
    //
    // Still read as raw TOML here, but NOT for the reason this comment gave
    // until 2026-09-17 (issue #212 residual). It claimed `slice_size` "isn't
    // part of the structured `UnitDotfile`/`PolicySection` model" — it is,
    // since #212 added it so `unit rename`'s read/write round trip stopped
    // silently dropping it. The raw read is now merely redundant, not
    // required.
    //
    // It is also harmless: `policy::resolve` runs first (see the caller) and
    // its `?` means a dotfile with a bad `[policy]` never reaches here at all
    // (#211) — including, since issue #263, a misspelled TOP-LEVEL table
    // name (`[polcy]` instead of `[policy]`), which `policy::resolve` now
    // refuses by name too rather than letting it look like an absent
    // `[policy]` section. Left as-is rather than rewired, because
    // `slice_arg_for_dar`'s doc explains at length why this function hands
    // dar the operator's RAW string, and changing how the string is
    // obtained is a different, riskier change than correcting a comment: it
    // moves real on-tape slice boundaries for every unit.
    if let Some(ref path) = unit.current_path {
        let dotfile_path = Path::new(path).join(".tapectl-unit.toml");
        if let Ok(contents) = fs::read_to_string(&dotfile_path) {
            if let Ok(toml) = contents.parse::<toml::Table>() {
                if let Some(v) = toml
                    .get("policy")
                    .and_then(|p| p.as_table())
                    .and_then(|p| p.get("slice_size"))
                    .and_then(|v| v.as_str())
                {
                    return v.to_string();
                }
            }
        }
    }

    // Layer 2: archive_set. Byte-only column — the resolved byte count
    // (already computed by `policy::resolve`) is the only faithful string.
    if let Some(as_id) = unit.archive_set_id {
        let has_override: bool = conn
            .query_row(
                "SELECT slice_size IS NOT NULL FROM archive_sets WHERE id = ?1",
                params![as_id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if has_override {
            return resolved_bytes.to_string();
        }
    }

    // Layer 3: system default — unchanged from before issue #47: dar
    // receives the config string verbatim, never round-tripped through
    // `parse_size_to_bytes`.
    config.defaults.slice_size.clone()
}

/// Refuse to stage a snapshot whose file list is short (issue #374).
///
/// `snapshots.file_count` is the walk's own count of non-directory entries,
/// written in the same statement as the snapshot row; the `files` rows are
/// what `catalog search`, `locate` and the tape's `catalog.db` are built
/// from. Since #374 both land in one transaction, but a snapshot minted
/// before that could have committed its row and then lost the rest of its
/// file list to a Ctrl-C or a busy catalog, and nothing detected it: dar
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
        "SELECT COUNT(*) FROM files WHERE snapshot_id = ?1 AND is_directory = 0",
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

/// dar's per-entry overhead in an archive: each entry's inline header plus
/// its record in the catalog dar appends at the end. Measured at ~290 bytes
/// per file on dar 2.7.13 (2,001 files, `compression = none`); rounded up,
/// because this only ever widens the "may not fit" band, never a refusal.
const DAR_ENTRY_OVERHEAD_BYTES: i64 = 1024;

/// What the staging-space check needs to know about the stage it guards.
struct StagingSpaceInputs<'a> {
    staging_dir: &'a Path,
    unit_name: &'a str,
    snapshot: &'a models::Snapshot,
    /// The resolved dar compression (`"none"` or an algorithm).
    compression: &'a str,
    /// Tier-2 consent given in advance (the global `--yes`) for a stage
    /// that may not fit (issue #354).
    assume_yes: bool,
}

impl StagingSpaceInputs<'_> {
    /// Staging's peak for a dar archive of `archive` bytes: the archive
    /// itself, as ciphertext — only `.age` slices are written there (issue
    /// #370), each about 1/4096 longer than its plaintext (age's 16-byte tag
    /// per 64 KiB chunk), plus its header.
    fn peak(archive: i64) -> i64 {
        archive.saturating_add(archive / 4096).saturating_add(4096)
    }

    /// The most the stage is expected to need with `compression = none`,
    /// as near as it can be told without reading the files: the snapshot's
    /// recorded size — the sum of its regular files' apparent sizes — plus
    /// [`DAR_ENTRY_OVERHEAD_BYTES`] a file.
    fn upper(&self) -> i64 {
        let files = self.snapshot.file_count.unwrap_or(0);
        Self::peak(
            self.snapshot
                .total_size
                .unwrap_or(0)
                .saturating_add(files.saturating_mul(DAR_ENTRY_OVERHEAD_BYTES)),
        )
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
/// - otherwise the stage may or may not fit — dar stores runs of zeros as
///   holes and a hard-linked file once, and compression can shrink anything
///   — so the operator is ASKED, with the figures ([`ask_to_stage_anyway`]:
///   ADR-0008 Tier 2; `--yes` proceeds, a non-interactive run without it
///   refuses). Never a hard refusal.
///
/// It used to refuse outright below a lower bound, the unit's non-zero
/// bytes, which the sha256 pass before dar counted. Since issue #364 there
/// is no pass before dar to count them: the source is read once, beside
/// dar. A stage that goes ahead and runs out of space stops cleanly.
fn check_staging_space(inputs: &StagingSpaceInputs, notices: &mut dyn Write) -> Result<()> {
    use crate::util::format_bytes_binary as fmt;

    let Some(free) = inputs.free(notices) else {
        return Ok(());
    };
    let upper = inputs.upper();
    if free >= upper {
        return Ok(());
    }
    let how = if inputs.compression == "none" {
        "dar stores runs of zeros as holes and a hard-linked file once".to_string()
    } else {
        format!(
            "its data may compress (compression = \"{}\")",
            inputs.compression
        )
    };
    ask_to_stage_anyway(
        inputs,
        format!(
            "staging directory {} may be too small for unit \"{}\": {} free, and staging it \
             needs up to {}, its encrypted slices at the snapshot's full size (less if {how}); \
             {STAGING_RUNS_OUT}",
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
    "if it runs out, the stage stops there and its partial slices are removed";

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

/// Bytes an unprivileged process can still write under `dir`:
/// `f_bavail` (not `f_bfree`, which counts root's reserve) in units of
/// `f_frsize` (POSIX; `f_bsize` is only the preferred I/O size) — the same
/// arithmetic as `config check`'s staging-space line.
fn staging_free_bytes(dir: &Path) -> std::result::Result<u64, nix::Error> {
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

/// Sizes and hashes recorded for one staged slice
/// ([`files::SliceWriter::finish`]): the plaintext dar slice's size and
/// sha256, and its `.age` file's. Only the plaintext pair is a pure function
/// of the content — age draws a fresh key, nonce and grease stanza for
/// every file.
pub struct EncryptedSliceInfo {
    pub plain_size: i64,
    pub sha256_plain: String,
    pub encrypted_size: i64,
    pub sha256_encrypted: String,
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

/// Sync the isolated dar catalogue `dar -C` just wrote at `catalog_base`
/// (`{catalog_base}.N.dar`; other snapshots' catalogues share the directory
/// and are left alone) — issue #409. Every later stage set of the snapshot
/// reuses this file instead of extracting it again, so it is written once
/// and must survive a power loss like the slices it indexes.
fn sync_catalogue(catalog_base: &Path) -> Result<()> {
    let (Some(dir), Some(base)) = (catalog_base.parent(), catalog_base.file_name()) else {
        return Ok(());
    };
    let prefix = format!("{}.", base.to_string_lossy());
    let entries = fs::read_dir(dir)
        .map_err(|e| staging_io_error("cannot read catalogue directory", dir, e))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| staging_io_error("cannot read catalogue directory", dir, e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && name.ends_with(".dar") {
            let path = entry.path();
            let file = fs::File::open(&path)
                .map_err(|e| staging_io_error("cannot open catalogue", &path, e))?;
            make_durable(&file, &path)?;
        }
    }
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
/// of a stage set that died after dar.
#[cfg(test)]
pub(crate) mod failpoint {
    use std::cell::Cell;

    use crate::error::{Result, TapectlError};

    /// Right after dar has finished and its command is recorded.
    pub(crate) const AFTER_DAR: &str = "after dar";

    thread_local! {
        static ARMED: Cell<Option<&'static str>> = const { Cell::new(None) };
    }

    /// Fail the next time `point` is reached on this thread.
    pub(crate) fn arm(point: &'static str) {
        ARMED.with(|a| a.set(Some(point)));
    }

    pub(crate) fn hit(point: &'static str) -> Result<()> {
        if ARMED.with(|a| a.get()) == Some(point) {
            ARMED.with(|a| a.set(None));
            return Err(TapectlError::Other(format!("injected failure: {point}")));
        }
        Ok(())
    }

    thread_local! {
        static STAGING_AFTER_DAR: std::cell::RefCell<Option<Vec<String>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Record the staging directory's file names when dar has finished,
    /// on this thread's next stage.
    pub(crate) fn observe_staging_after_dar() {
        STAGING_AFTER_DAR.with(|o| *o.borrow_mut() = Some(Vec::new()));
    }

    pub(crate) fn note_staging(dir: &std::path::Path) {
        STAGING_AFTER_DAR.with(|o| {
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
    pub(crate) fn staging_after_dar() -> Option<Vec<String>> {
        STAGING_AFTER_DAR.with(|o| o.borrow_mut().take())
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
/// Issue #372: each UPDATE is keyed by `(snapshot_id, path)`, which the
/// `UNIQUE(snapshot_id, path)` index serves, so finalization is linear in
/// the file count. The second UPDATE this used to run, into
/// `manifest_entries`, could only search by `manifest_id` and scanned the
/// whole manifest per file; migration 027 dropped that table. The plan is
/// pinned by `backfill_checksums_searches_the_unique_index`. The `files_au`
/// FTS trigger fires only on a `path` change since 027, so a sha256-only
/// UPDATE no longer rewrites the search index either.
pub(crate) const BACKFILL_SQL: &str =
    "UPDATE files SET sha256 = ?1 WHERE snapshot_id = ?2 AND path = ?3 AND sha256 IS NULL";

fn backfill_checksums(
    conn: &Connection,
    snapshot_id: i64,
    checksums: &[(String, String)],
) -> Result<()> {
    let mut file_update = conn.prepare(BACKFILL_SQL)?;
    for (path, hash) in checksums {
        file_update.execute(params![hash, snapshot_id, path])?;
    }
    Ok(())
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

/// Parse an operator-facing size string (e.g. `"10G"`, `"500M"`, or a bare
/// byte count like `"1024"`) into a byte count.
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
            "{trimmed:?} is not a valid size (expected e.g. 2500G, 500M, or a bare byte count)"
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
        "K" | "KB" => 1024.0,
        "M" | "MB" => 1024.0 * 1024.0,
        "G" | "GB" => 1024.0 * 1024.0 * 1024.0,
        "T" | "TB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
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
    use std::os::unix::fs::MetadataExt;
    use walkdir::WalkDir;

    let base = Path::new(path);
    // Issue #49 items 3+5: global excludes + the unit's own dotfile exclude
    // patterns, compiled once per walk (not per entry). A plain pattern never
    // matches a directory (dar's own `-X` rule); a directory pattern
    // (`name/`, issue #359) prunes the subtree below `name`, which dar
    // receives as `-P` masks (`exclude::dar_masks`, in `stage_create`).
    let exclude_compiled = exclude::effective_compiled(base, global_excludes)?;
    let mut entries = Vec::new();
    let mut total_size: i64 = 0;
    let mut file_count: i64 = 0;

    // Issue #359 (c): a directory INSIDE a pruned subtree is neither
    // recorded nor descended — dar's `-P` + `-D` store the pruned directory
    // itself, empty, and nothing below it. The pruned directory's own entry
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
        let rel_path = entry
            .path()
            .strip_prefix(base)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .to_string();

        if rel_path.is_empty() {
            continue; // skip root
        }

        let meta = entry
            .metadata()
            .map_err(|e| TapectlError::Other(e.to_string()))?;
        let is_dir = meta.is_dir();

        // Issue #49: a non-directory entry matching an exclude pattern is
        // dropped before any further work (symlink-target read, manifest
        // row) — dar will never archive it (`stage_create` hands dar the
        // same patterns, as `-X` and `-P` masks via `exclude::dar_masks`),
        // so the manifest/files table must not record it either. Checked
        // before the file_type classification below so an excluded entry
        // costs nothing beyond the pattern match.
        if !is_dir && exclude::is_excluded(entry.path(), &exclude_compiled) {
            continue;
        }
        // lstat's own size for the entry — for a symlink this is the length
        // of the *target path string*, not any real content size. Kept
        // as-is (not zeroed for a symlink/special below): it's what
        // `collection::fingerprint`'s independent walk also computes from
        // the same never-follow metadata, and zeroing it here would make
        // that fingerprint comparison disagree with what's recorded,
        // falsely flagging every symlink-containing unit as perpetually
        // dirty. Only `total_size` below excludes it (see that comment).
        let size = if is_dir { 0 } else { meta.len() as i64 };
        let mtime = chrono::DateTime::from_timestamp(meta.mtime(), 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default();

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
            ("symlink", Some(target.to_string_lossy().to_string()))
        } else if ft.is_file() {
            ("regular", None)
        } else {
            // FIFO, socket, block/char device: recorded (so restore and
            // `catalog ls` still see them) but never content-validated —
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
            // before this fix (dar will archive and catalog a symlink or
            // special file as a real entry even though it carries no
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
            mtime,
            is_dir,
            file_type,
            link_target,
        });
    }

    Ok((total_size, file_count, entries))
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
    mtime: String,
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

        // Both non-directory entries count toward file_count — dar will
        // archive and catalog the symlink as a real entry even though it
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
        // proves the shape stays what dar's `{base}.{N}.dar` naming needs,
        // and that the dot prevents `_s1` from prefix-matching `_s10.1.dar`.
        let p1 = archive_base_prefix("abcdef0123456789", 3, 1);
        let p10 = archive_base_prefix("abcdef0123456789", 3, 10);
        assert_eq!(p1, "abcdef012345_v3_s1.");
        assert_eq!(p10, "abcdef012345_v3_s10.");
        assert!(
            !"abcdef012345_v3_s10.1.dar".starts_with(&p1),
            "trailing dot must stop _s1 from prefix-matching _s10's files"
        );
    }

    // ── issue #47: stage_create must resolve policy, not read config.defaults directly ──

    /// Negative control / regression guard for issue #263's headline
    /// defect: `[excludes] pattern = [...]` (singular — the real key is
    /// `patterns`) parses cleanly today because `ExcludesSection` carries
    /// no `#[serde(deny_unknown_fields)]`, so `read_dotfile` silently
    /// returns `exclude_patterns: []`. `stage_create` then hands dar zero
    /// `-X` masks, and the material the operator meant to exclude is
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
        config.dar.binary = "dar".to_string();
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
        // shared helper `stage_create`'s dar `-X` masks use), so the fix
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
    /// Exercises the REAL pipeline end to end (real `dar`, real tenant
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
    /// archive set names a real compressor, it also runs dar's `-z` codepath,
    /// which #92 made reachable for the first time.
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
        config.dar.binary = "dar".to_string(); // PATH, not a hardcoded distro path (issue #43)
        config.staging.directory = staging_dir.to_string_lossy().into_owned();
        config.defaults.slice_size = "100M".to_string();
        // Leave the default at "none" and have the archive set override it to
        // a real compressor. That direction is the one that actually proves
        // the fix: pre-#92, `init_unit`'s dotfile hardcoded `compression =
        // "none"`, so an assertion expecting "none" would have passed for the
        // wrong reason. Expecting "gzip" can only succeed if the dotfile has
        // stopped shadowing. It also drives dar's real `-z` codepath.
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

    /// End-to-end proof (real dar, real encryption) that two stage sets of
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
        config.dar.binary = "dar".to_string();

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
        let stage_set_2 = stage_create(&conn, &paths, &config, snap_id, false).unwrap();
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

    /// Decrypt every `.age` slice of `stage_set_id` with the tenant `alice`'s
    /// keys into `out/arch.N.dar` — the plaintext dar archive an heir gets
    /// after `age -d` — and return its base, `out/arch`, for `dar -t`/`-x`.
    pub(super) fn decrypt_staged_set(
        conn: &Connection,
        paths: &TapectlPaths,
        stage_set_id: i64,
        out: &Path,
    ) -> PathBuf {
        let identities =
            crate::crypto::keys::load_tenant_identities(conn, &paths.keys_dir, "alice").unwrap();
        assert!(!identities.is_empty(), "alice has keys");
        let slices: Vec<(i64, String)> = conn
            .prepare(
                "SELECT slice_number, staging_path FROM stage_slices
                 WHERE stage_set_id = ?1 ORDER BY slice_number",
            )
            .unwrap()
            .query_map(params![stage_set_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert!(!slices.is_empty(), "stage set {stage_set_id} has slices");
        fs::create_dir_all(out).unwrap();
        for (n, path) in slices {
            let decryptor = age::Decryptor::new(fs::File::open(&path).unwrap()).unwrap();
            let mut reader = decryptor
                .decrypt(identities.iter().map(|i| i as &dyn age::Identity))
                .unwrap();
            let mut plain = fs::File::create(out.join(format!("arch.{n}.dar"))).unwrap();
            std::io::copy(&mut reader, &mut plain).unwrap();
        }
        out.join("arch")
    }

    /// `dar -t <archive> -A <catalogue>`: does dar accept `catalogue` as
    /// the isolated catalogue of `archive`? dar compares the archive's
    /// random data-name label with the catalogue's.
    pub(super) fn dar_accepts_catalogue(archive: &Path, catalogue: &str) -> (bool, String) {
        let out = std::process::Command::new("dar")
            .arg("-t")
            .arg(archive)
            .arg("-A")
            .arg(catalogue)
            .arg("-Q")
            .output()
            .expect("dar must be on PATH (tests/test_dependencies.rs)");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// Issue #419: every stage set has its own isolated catalogue, produced
    /// by its own dar run, so dar's catalogue rescue (`-A`) works against
    /// that stage set's slices. Before, the catalogue was extracted once per
    /// snapshot and every later stage set reused the first run's — whose
    /// random data-name label dar refuses against a later run's slices.
    #[test]
    fn a_restaged_snapshots_catalogue_matches_its_own_slices() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        fs::write(src.join("f.txt"), b"restaged content").unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let set_1 = stage_create(&conn, &paths, &config, snap_id, false).unwrap();
        let set_2 = stage_create(&conn, &paths, &config, snap_id, false).unwrap();

        let catalogue = |id: i64| -> String {
            conn.query_row(
                "SELECT catalog_path FROM stage_sets WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap()
        };
        let archive_2 = decrypt_staged_set(&conn, &paths, set_2, &tmp.path().join("plain2"));

        let (ok, err) = dar_accepts_catalogue(&archive_2, &catalogue(set_2));
        assert!(
            ok,
            "the second stage set's catalogue must be its own archive's: {err}"
        );
        // Positive control: dar does refuse a catalogue from another run.
        let (ok, _) = dar_accepts_catalogue(&archive_2, &catalogue(set_1));
        assert!(
            !ok,
            "dar refuses the first run's catalogue for the second run's slices"
        );
    }

    // ── issue #54: stage failure hygiene ──

    /// A failure after dar has finished — every slice written and recorded,
    /// the stage not yet finalized — leaves nothing in staging. Before issue
    /// #54 the plaintext `.dar` slices dar had written then were never
    /// cleaned up; since issue #370 there are none, and the recorded `.age`
    /// slices are what the cleanup must remove.
    ///
    /// The failure is injected right after dar (`failpoint::AFTER_DAR`).
    /// It used to be injected with a tenant that had no active keys, until
    /// issue #354 moved that refusal before dar, and then by making
    /// `catalogs_dir` a file, until issue #419 created the stage set's
    /// catalogue directory before dar (dar writes the catalogue into it).
    /// The cleanup also removes that directory.
    #[test]
    fn a_failure_after_dar_leaves_nothing_in_staging() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();

        let conn = crate::db::open(&paths.db_file).unwrap();

        let staging_dir = tmp.path().join("staging");
        fs::create_dir_all(&staging_dir).unwrap();

        let mut config = Config::default();
        config.dar.binary = "dar".to_string(); // PATH, not a hardcoded distro path (issue #43)
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
        // The injected post-dar failure (see the doc comment).
        failpoint::arm(failpoint::AFTER_DAR);
        failpoint::observe_staging_after_dar();
        let result = stage_create(&conn, &paths, &config, snap_id, false);

        assert!(
            matches!(&result, Err(e) if e.to_string().contains("injected failure")),
            "expected stage_create to fail at the injected point, got {result:?}"
        );

        // Positive control: the failure must have come AFTER dar, with its
        // slices written, or the assertion below would pass vacuously.
        let dar_ran: bool = conn
            .query_row(
                "SELECT dar_command IS NOT NULL FROM stage_sets WHERE snapshot_id = ?1",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            dar_ran,
            "the injected failure must land after dar -c, got: {result:?}"
        );
        assert_eq!(
            failpoint::staging_after_dar().map(|names| names.len()),
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

        // Issue #419: dar wrote this stage set's on-the-fly catalogue into its
        // own catalogue directory; the cleanup removes that too.
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
    /// `archive_base` is `{uuid12}_v{version}` and dar names slices
    /// `{base}.{N}.dar`, so the scan prefix must carry a trailing dot.
    /// Without it `..._v1` is a prefix of `..._v10.1.dar`, and a failed
    /// stage of version 1 deletes the in-flight plaintext of a concurrent
    /// stage of version 10 — silent cross-stage data destruction, the exact
    /// hazard the `.age` cleanup is deliberately DB-row-keyed to avoid.
    ///
    /// Drives `cleanup_failed_stage_set` directly against hand-placed decoy
    /// files, because provoking two concurrent real dar runs at versions 1
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
        let failed_slice = staging_dir.join(format!("{base12}_v1_s{failed_set}.1.dar"));
        let sibling_slice = staging_dir.join(format!("{base12}_v1_s{sibling_set}.1.dar"));
        let sibling_hash = staging_dir.join(format!("{base12}_v1_s{sibling_set}.1.dar.sha512"));
        // Issue #370: the `.dar.age` being written has no row yet, so it is
        // found by the same prefix rule.
        let failed_age = staging_dir.join(format!("{base12}_v1_s{failed_set}.1.dar.age"));
        let sibling_age = staging_dir.join(format!("{base12}_v1_s{sibling_set}.1.dar.age"));
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
            "the failed stage set's own orphaned plaintext slice must be removed"
        );
        assert!(
            sibling_slice.exists(),
            "a sibling stage set's in-flight plaintext slice must survive this \
             cleanup — a prefix without the stage_set_id/trailing-dot discipline \
             would have deleted it"
        );
        assert!(
            sibling_hash.exists(),
            "the sibling's hash file must survive this cleanup for the same reason"
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
        let age = staging_dir.join(format!("{}.1.dar.age", archive_base_name(uuid, 1, 1)));
        fs::write(&age, b"ciphertext").unwrap();
        (tmp, db_path, conn, config, 1, age)
    }

    fn slice_info() -> EncryptedSliceInfo {
        EncryptedSliceInfo {
            plain_size: 9,
            sha256_plain: "p".into(),
            encrypted_size: 10,
            sha256_encrypted: "e".into(),
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

        let err =
            record_encrypted_slice(&conn, QUICK, stage_set_id, 1, &age, &slice_info()).unwrap_err();
        assert!(matches!(err, TapectlError::CatalogBusy(_)), "{err:?}");
        assert_eq!(slice_rows(&conn), 0);
        assert!(
            !age.exists(),
            "an .age no row names must not be left behind"
        );

        holder.execute_batch("ROLLBACK").unwrap();
        fs::write(&age, b"ciphertext").unwrap();
        record_encrypted_slice(&conn, QUICK, stage_set_id, 1, &age, &slice_info()).unwrap();
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
        record_encrypted_slice(&conn, QUICK, stage_set_id, 1, &age, &slice_info()).unwrap();

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
            &TapectlError::Other("dar failed".into()),
        );
        assert!(!age.exists(), "any other failure still cleans up");
        assert_eq!(slice_rows(&conn), 0);
    }

    // ── issue #49: exclusions end-to-end (dotfile+global -> dar, walk, validation) ──

    /// Shared setup for the issue #49 tests below: a real tenant + unit
    /// (via `unit::init_unit`, so a real dotfile + real tenant keys exist —
    /// `stage_create` refuses to encrypt without active tenant keys), with
    /// the unit's dotfile `[excludes] patterns` overwritten to
    /// `exclude_patterns` (empty = the "no excludes configured" case,
    /// `init_unit` itself always writes an empty list). Returns
    /// `(conn, paths, config, src_dir)`; the caller writes fixture files
    /// into `src_dir` and drives `snapshot_create`/`stage_create` itself,
    /// since each test needs different file content/timing.
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
        config.dar.binary = "dar".to_string(); // PATH, not a hardcoded distro path (issue #43)
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
    /// on tape. So the refusal belongs here, before dar and age burn hours
    /// on material that cannot be written, not at the drive afterwards.
    ///
    /// The two halves that matter are both about *when*: no `stage_sets` row
    /// may be left behind, and dar must never start. `dar.binary` is pointed
    /// at a path that cannot exist, so reaching dar at all would produce a
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
        config.dar.binary = "/nonexistent/dar-must-never-run".to_string();

        fs::write(src.join("f.txt"), b"content that must never reach dar").unwrap();
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
            !msg.contains("dar-must-never-run"),
            "the refusal must precede the dar run, not follow it: {msg}"
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

    /// Issue #354 (c): a tenant with no active keys is refused BEFORE dar,
    /// the same way the escrow refusal above is. It used to be checked at
    /// the top of the encryption loop — after `dar -c` had archived the
    /// whole unit and the catalog had been extracted — so a large unit
    /// spent its entire dar run to reach a refusal that needed nothing but
    /// a database read.
    ///
    /// Same two "when" halves as the escrow test: dar never starts (its
    /// binary is a path that cannot exist, so reaching it would change the
    /// error), and no `stage_sets` row is left for the startup sweep.
    #[test]
    fn stage_create_refuses_a_tenant_with_no_active_keys_before_dar_runs() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        conn.execute(
            "UPDATE encryption_keys SET is_active = 0
             WHERE tenant_id = (SELECT id FROM tenants WHERE name = 'alice')",
            [],
        )
        .unwrap();
        config.dar.binary = "/nonexistent/dar-must-never-run".to_string();

        fs::write(src.join("f.txt"), b"content that must never reach dar").unwrap();
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
            !msg.contains("dar-must-never-run"),
            "the refusal must precede the dar run, not follow it: {msg}"
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
        config.dar.binary = "/nonexistent/dar-must-never-run".to_string();

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
    /// validation pass over the source, and only then fail inside dar.
    ///
    /// Permission bits do not bind root, so the test skips there (the
    /// ENOTDIR test above is the root-safe half of this behaviour).
    #[test]
    fn an_unwritable_staging_directory_is_refused_before_dar_naming_it() {
        use std::os::unix::fs::PermissionsExt;
        if nix::unistd::geteuid().is_root() {
            eprintln!("skipping: permission bits do not bind root");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        let staging = PathBuf::from(&config.staging.directory);
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o500)).unwrap();
        config.dar.binary = "/nonexistent/dar-must-never-run".to_string();

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
            !msg.contains("dar-must-never-run"),
            "the refusal must precede the dar run: {msg}"
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

        // And the bytes on disk really are age ciphertext, not plaintext dar.
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

    /// `len` bytes with no zero byte anywhere — nothing dar could store as a
    /// hole — written and synced, so its allocation is on record.
    fn write_dense_file(path: &Path, len: usize) {
        let data: Vec<u8> = (0..len as u32)
            .map(|i| ((i.wrapping_mul(2_654_435_761) >> 13) as u8) | 1)
            .collect();
        let mut f = fs::File::create(path).unwrap();
        f.write_all(&data).unwrap();
        f.sync_all().unwrap();
    }

    /// A file's allocation on disk, capped at its length — what a bound read
    /// off `st_blocks` would count for it.
    fn allocated(path: &Path) -> i64 {
        use std::os::unix::fs::MetadataExt;
        let m = fs::metadata(path).unwrap();
        m.len().min(m.blocks() * 512) as i64
    }

    /// The figure the space check states: the snapshot's recorded size plus
    /// dar's per-file overhead, as ciphertext.
    fn upper_bound(snapshot: &models::Snapshot) -> i64 {
        StagingSpaceInputs::peak(
            snapshot.total_size.unwrap() + snapshot.file_count.unwrap() * DAR_ENTRY_OVERHEAD_BYTES,
        )
    }

    /// Issue #364: with no pre-dar read there is no content lower bound, so
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

    /// A hard-linked file is one inode, which dar stores once, while the
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

        let upper = upper_bound(&snapshot);
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
                && notices.contains("a hard-linked file once")
                && notices.contains("staging anyway (--yes given)"),
            "the notice must carry the figure ({}): {notices:?}",
            fmt(upper)
        );
    }

    /// The adversarial review's probe (issue #354): a DENSE file of zeros —
    /// written, not seeked, so every block is allocated — costs dar almost
    /// nothing, because dar stores zero runs as holes by default
    /// (`--sparse-file-min-size` 15; measured: 16 MiB of zeros made a
    /// 752-byte archive). A bound read off the file's allocation would refuse
    /// a stage that needs a few KiB, with no way past it — preallocated disk
    /// images, fallocate'd databases and zero-padded ISOs all look like this.
    /// It is asked about, and with `--yes` it runs.
    #[test]
    fn a_dense_file_of_zeros_that_fits_is_staged_not_refused() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        let zeros = src.join("disk.img");
        {
            let mut f = fs::File::create(&zeros).unwrap();
            f.write_all(&vec![0u8; 8 * 1024 * 1024]).unwrap();
            f.sync_all().unwrap();
        }
        assert!(
            allocated(&zeros) >= 8 * 1024 * 1024,
            "fixture: the zeros must be allocated on disk (dense, not sparse), \
             or this test cannot tell an allocation bound from a content one"
        );
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let free: u64 = 4 * 1024 * 1024;
        let _free = FreeSpaceOverride::set(free);

        let mut notices = Vec::new();
        let stage_set_id =
            stage_create_reporting(&conn, &paths, &config, snap_id, true, &mut notices).expect(
                "8 MiB of zeros needs a few KiB of staging: it must be staged, not refused",
            );

        // It really did fit: dar's archive is a sliver of the 4 MiB "free".
        let dar_size: i64 = conn
            .query_row(
                "SELECT total_dar_size FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            dar_size < 64 * 1024,
            "dar stored the zeros as holes: {dar_size} bytes"
        );
        let notices = String::from_utf8(notices).unwrap();
        assert!(
            notices.contains("may be too small for unit \"unit1\"")
                && notices.contains("runs of zeros as holes"),
            "the notice says why the figure may overstate the need: {notices:?}"
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
    /// known before dar runs, so a short staging directory is ASKED about
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
        config.dar.binary = "/nonexistent/dar-must-never-run".to_string();
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
    /// without consent in a non-interactive run: no `stage_sets` row, dar
    /// never ran, staging is left empty. `--yes` is the way past it.
    #[test]
    fn short_staging_space_is_refused_without_consent() {
        use crate::util::format_bytes_binary as fmt;
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.dar.binary = "/nonexistent/dar-must-never-run".to_string();
        write_dense_file(&src.join("a.bin"), 64 * 1024);
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let upper = upper_bound(&get_snapshot(&conn, snap_id).unwrap());
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
        assert!(!msg.contains("dar-must-never-run"), "before dar: {msg}");
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

        // The way past the refusal it names: the same stage with `--yes`.
        config.dar.binary = "dar".to_string();
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

        let recorded_size: i64 = conn
            .query_row(
                "SELECT size_bytes FROM files WHERE snapshot_id = ?1 AND path = 'big.bin'",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            recorded_size, 1024,
            "the large file must still be recorded in the manifest, not skipped"
        );
    }

    /// THE core claim of issue #49 (its own re-triage escalation): a unit
    /// must not be permanently blocked from staging by content drift in a
    /// file dar was never going to archive. Write this FIRST — it must
    /// fail against the pre-#49 code (see the PR report for the captured
    /// pre-fix failure): `walk_directory` used to record every file
    /// unfiltered, so `backfill_checksums` established a sha256 baseline
    /// for the excluded junk file too, and this re-stage's same-size
    /// content drift then tripped `validate_source`'s BITROT refusal for
    /// content dar was never going to touch.
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
        // succeed cleanly — never raise BITROT over content dar was never
        // going to archive.
        let result = stage_create(&conn, &paths, &config, snap_id, false);
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

        let files_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE snapshot_id = ?1 AND path = 'junk.tmp'",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(files_count, 0, "excluded file must not appear in `files`");

        let kept_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE snapshot_id = ?1 AND path = 'keep.txt'",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
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

        let junk_row_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE snapshot_id = ?1 AND path = 'junk.tmp'",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            junk_row_exists, 0,
            "excluded file must have no `files` row at all, so there is \
             nothing for backfill_checksums to baseline"
        );

        let kept_sha: Option<String> = conn
            .query_row(
                "SELECT sha256 FROM files WHERE snapshot_id = ?1 AND path = 'keep.txt'",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            kept_sha.is_some(),
            "the non-excluded file must still get its baseline established"
        );
    }

    #[test]
    fn dotfile_exclude_patterns_reach_dars_constructed_arguments() {
        // "Dotfile patterns reach dar (assert on the constructed dar
        // arguments)" — stage_sets.dar_command records the exact
        // Command::Debug-formatted string create_archive ran.
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) =
            setup_unit_with_excludes(&tmp, vec!["*.unusual-dotfile-pattern".to_string()]);

        fs::write(src.join("keep.txt"), b"kept content").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let stage_set_id = stage_create(&conn, &paths, &config, snap_id, false).unwrap();

        let dar_command: String = conn
            .query_row(
                "SELECT dar_command FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            dar_command.contains("*.unusual-dotfile-pattern"),
            "the dotfile's own exclude pattern must reach dar's constructed \
             -X arguments, got: {dar_command}"
        );
        // The pre-existing global excludes must still be present too (this
        // fix merges, not replaces).
        assert!(
            dar_command.contains("Thumbs.db"),
            "config.defaults.global_excludes must still reach dar, got: {dar_command}"
        );
    }

    /// Issue #359 (c): a directory pattern (`name/`) keeps the subtree out
    /// of the DAR ARCHIVE, not only out of the manifest, the `files` table
    /// and the dirty scan. Before, `stage_create` handed dar every pattern
    /// as a raw `-X` basename mask and no `-P` prune, so `.cache/` matched
    /// nothing in dar and the cache's bytes were archived, encrypted and
    /// written to tape while the catalog said they were not there.
    ///
    /// The archive is read back through the isolated catalog dar extracted
    /// from it (`stage_sets.catalog_path` — the plaintext slices are gone
    /// once encrypted): `dar -l` lists every entry the archive holds. The
    /// pruned directory itself stays, empty (`-D`), in the archive and in
    /// the manifest alike; a directory INSIDE it is in neither.
    #[test]
    fn a_directory_exclude_keeps_the_subtree_out_of_the_dar_archive() {
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

        let catalog: String = conn
            .query_row(
                "SELECT catalog_path FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();
        let out = std::process::Command::new("dar")
            .arg("-l")
            .arg(&catalog)
            .arg("-Q")
            .output()
            .expect("dar must be on PATH (tests/test_dependencies.rs)");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let listing = String::from_utf8_lossy(&out.stdout).into_owned();

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
        let recorded = |path: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM files WHERE snapshot_id = ?1 AND path = ?2",
                params![snap_id, path],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(recorded(".cache"), 1, "the pruned directory itself is kept");
        assert_eq!(recorded("sub/.cache"), 1, "at any depth");
        assert_eq!(
            recorded(".cache/deepdir"),
            0,
            "a directory inside a pruned subtree is not recorded, as dar -D does not store it"
        );
        assert_eq!(recorded(".cache/cached-top.bin"), 0);
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
        let file_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE snapshot_id = ?1",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
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
    // (Thumbs.db/.DS_Store/*.nfo/*.tmp by default) reached dar only —
    // neither walk saw it. For a unit with NO dotfile override (the common
    // case), `walk_directory` recorded the globally-excluded file into
    // `files`, `backfill_checksums` gave it a sha256 baseline, and a
    // same-size content regeneration then tripped `validate_source`'s
    // BITROT refusal for content dar was never going to archive — see the
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
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);

        fs::write(src.join("keep.txt"), b"real archival content, kept").unwrap();
        fs::write(src.join("Thumbs.db"), b"AAAA").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &config).unwrap();
        stage_create(&conn, &paths, &config, snap_id, false).expect("first stage must succeed");

        // Thumbs.db regenerates at an UNCHANGED size — exactly the
        // false-BITROT scenario the issue describes.
        fs::write(src.join("Thumbs.db"), b"BBBB").unwrap();

        let result = stage_create(&conn, &paths, &config, snap_id, false);
        assert!(
            result.is_ok(),
            "re-staging must succeed — a globally-excluded file's content drift \
             must never raise BITROT, even with no dotfile override: {result:?}"
        );
    }

    #[test]
    fn global_default_excluded_files_do_not_appear_in_files_table() {
        let tmp = TempDir::new().unwrap();
        let (conn, _paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);

        fs::write(src.join("keep.txt"), b"kept content").unwrap();
        fs::write(src.join("Thumbs.db"), b"thumbnail cache junk").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &config).unwrap();

        let files_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE snapshot_id = ?1 AND path = 'Thumbs.db'",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            files_count, 0,
            "a globally-excluded file must not appear in `files`, even with no \
             dotfile override"
        );

        let kept_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE snapshot_id = ?1 AND path = 'keep.txt'",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kept_count, 1, "non-excluded file must still be recorded");
    }

    #[test]
    fn global_default_excluded_files_never_receive_a_sha256_baseline() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);

        fs::write(src.join("keep.txt"), b"kept content, baselined").unwrap();
        fs::write(src.join("Thumbs.db"), b"thumbnail cache junk").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &config).unwrap();
        stage_create(&conn, &paths, &config, snap_id, false).unwrap();

        let junk_row_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE snapshot_id = ?1 AND path = 'Thumbs.db'",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            junk_row_exists, 0,
            "a globally-excluded file must have no `files` row at all, so there \
             is nothing for backfill_checksums to baseline"
        );

        let kept_sha: Option<String> = conn
            .query_row(
                "SELECT sha256 FROM files WHERE snapshot_id = ?1 AND path = 'keep.txt'",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
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
        let stamps = |sid: i64| -> Vec<(String, i64)> {
            let mut st = conn
                .prepare(
                    "SELECT path, size_bytes FROM files
                     WHERE snapshot_id = ?1 AND is_directory = 0 ORDER BY path",
                )
                .unwrap();
            st.query_map(params![sid], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
        };
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
        conn.execute(
            "UPDATE files SET sha256 = ?1 WHERE snapshot_id = ?2 AND path = 'f.txt'",
            params![hash, s1.snapshot_id],
        )
        .unwrap();

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

    /// Issue #361: what `stage create` leaves in `<home>/stage-reports/` is
    /// a stage report, in its header as well as its directory — "receipt"
    /// now means only the recipient list a stage set was encrypted to
    /// (CONTEXT.md). The `write_stage_report` tests below pass their own
    /// body; this drives the real producer through a real stage (real dar).
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
    /// report; archive (dar, slicing, encryption and the source's hashing in
    /// one pass since issues #370 and #364) counts the bytes of dar's
    /// stream.
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
        assert_eq!(
            names,
            ["check", "archive", "recheck", "catalog", "finalize"]
        );
        assert!(rows
            .iter()
            .all(|r| r.outcome == "ok" && r.operation == "stage create"));
        // dar's stream holds the file and the unit's dotfile, at least.
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

    /// Issue #370: dar writes nothing under the staging directory. When dar
    /// has finished, the staging directory holds only `.age` ciphertext —
    /// before, it held the whole plaintext archive as `.dar` slices until
    /// each was encrypted and deleted.
    #[test]
    fn when_dar_finishes_staging_holds_only_ciphertext() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.defaults.slice_size = "64K".to_string();
        fs::write(src.join("noise.bin"), noise(200 * 1024)).unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        failpoint::observe_staging_after_dar();
        stage_create(&conn, &paths, &config, snap_id, false).unwrap();
        let names = failpoint::staging_after_dar().expect("the stage reached dar's end");
        assert!(
            names.len() >= 2,
            "positive control: several slices exist when dar ends: {names:?}"
        );
        for name in &names {
            assert!(
                name.ends_with(".dar.age"),
                "only ciphertext may be in staging when dar ends, found {name}: {names:?}"
            );
        }
    }

    /// Issue #370: the recorded dar command (MANIFEST.toml's `dar_command`)
    /// is dar's archive on standard output, with retry-on-change off and the
    /// on-the-fly catalogue, and names no path under the staging directory.
    #[test]
    fn the_recorded_dar_command_writes_to_stdout_and_names_no_staging_path() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        fs::write(src.join("f.txt"), b"command content").unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let id = stage_create(&conn, &paths, &config, snap_id, false).unwrap();
        let cmd: String = conn
            .query_row(
                "SELECT dar_command FROM stage_sets WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(cmd.contains(r#""-c" "-""#), "{cmd}");
        assert!(cmd.contains(r#""--retry-on-change" "0""#), "{cmd}");
        assert!(cmd.contains(r#""-@""#), "{cmd}");
        assert!(
            cmd.contains(&src.to_string_lossy().into_owned()),
            "positive control: the source is named: {cmd}"
        );
        let staging = Path::new(&config.staging.directory);
        for spelling in [staging.to_path_buf(), staging.canonicalize().unwrap()] {
            assert!(
                !cmd.contains(&*spelling.to_string_lossy()),
                "dar is given no path under the staging directory: {cmd}"
            );
        }
    }

    /// Issue #370: a source file that changes while dar reads it refuses the
    /// stage as DIRTY (dar runs with `--retry-on-change 0` and exits 11),
    /// and no slice is left in staging. Before, dar retried the file and a
    /// change that outlasted the retries failed as a bare dar error.
    #[test]
    fn a_file_changing_while_dar_reads_it_refuses_the_stage_as_dirty() {
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

        let err = result.expect_err("a file changing under dar refuses the stage");
        assert!(err.to_string().contains("DIRTY"), "{err}");
        let left: Vec<_> = fs::read_dir(&config.staging.directory)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(left.is_empty(), "nothing is left in staging: {left:?}");
    }

    /// Issue #370: slices cut from dar's stream are dar slices. Decrypted
    /// (what an heir does with `age -d`), `dar -t` accepts them, and
    /// `dar -x` restores the source exactly — the path RESTORE.sh takes,
    /// unchanged. Several slices, a subdirectory, a symlink and an empty file.
    #[test]
    fn streamed_slices_restore_with_dar_alone() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.defaults.slice_size = "64K".to_string();
        fs::create_dir_all(src.join("sub/deeper")).unwrap();
        fs::write(src.join("noise.bin"), noise(300 * 1024)).unwrap();
        fs::write(src.join("sub/deeper/text.txt"), b"some text\n".repeat(500)).unwrap();
        fs::write(src.join("sub/empty"), b"").unwrap();
        std::os::unix::fs::symlink("deeper/text.txt", src.join("sub/link")).unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let id = stage_create(&conn, &paths, &config, snap_id, false).unwrap();

        let archive = decrypt_staged_set(&conn, &paths, id, &tmp.path().join("plain"));
        assert!(
            tmp.path().join("plain/arch.3.dar").exists(),
            "positive control: several slices"
        );
        let dar = |args: &[&std::ffi::OsStr]| {
            let out = std::process::Command::new("dar")
                .args(args)
                .output()
                .expect("dar must be on PATH (tests/test_dependencies.rs)");
            assert!(
                out.status.success(),
                "dar {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        dar(&["-t".as_ref(), archive.as_os_str(), "-Q".as_ref()]);
        let restored = tmp.path().join("restored");
        fs::create_dir_all(&restored).unwrap();
        dar(&[
            "-x".as_ref(),
            archive.as_os_str(),
            "-R".as_ref(),
            restored.as_os_str(),
            "-O".as_ref(),
            "-Q".as_ref(),
        ]);
        for rel in [
            "noise.bin",
            "sub/deeper/text.txt",
            "sub/empty",
            ".tapectl-unit.toml",
        ] {
            assert_eq!(
                fs::read(restored.join(rel)).unwrap(),
                fs::read(src.join(rel)).unwrap(),
                "{rel} restored exactly"
            );
        }
        assert_eq!(
            fs::read_link(restored.join("sub/link")).unwrap(),
            Path::new("deeper/text.txt")
        );
    }

    /// Issue #364: the sha256 recorded for a file must be of the bytes dar
    /// archived. A file rewritten after it was hashed — here at once, before
    /// dar can have read it — is refused as DIRTY. Before, the hash pass and
    /// dar were two reads with nothing tying them together, and the stage
    /// succeeded with a baseline that did not match the archive.
    #[test]
    fn a_file_rewritten_after_it_is_hashed_refuses_the_stage() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        let target = src.join("f.bin");
        fs::write(&target, noise(64 * 1024)).unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        // The snapshot records the canonical path (/tmp may be a symlink).
        let src = src.canonicalize().unwrap();
        let hook_target = target.canonicalize().unwrap();
        validate::hash_hook::set(&src, move |path| {
            if path == hook_target {
                // Same size, other bytes: an in-place edit.
                let mut other = noise(64 * 1024);
                other.reverse();
                fs::write(path, other).unwrap();
            }
        });
        let result = stage_create(&conn, &paths, &config, snap_id, false);
        validate::hash_hook::clear(&src);

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

    /// Issue #364: BITROT is found while dar runs now, not before it — and
    /// still refuses the stage, stops dar, and leaves staging empty, with
    /// the baseline untouched.
    #[test]
    fn bitrot_found_while_dar_runs_refuses_the_stage() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.defaults.slice_size = "64K".to_string();
        fs::write(src.join("a.bin"), noise(256 * 1024)).unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let stale = "0".repeat(64);
        conn.execute(
            "UPDATE files SET sha256 = ?1 WHERE snapshot_id = ?2 AND path = 'a.bin'",
            params![stale, snap_id],
        )
        .unwrap();

        let err = stage_create(&conn, &paths, &config, snap_id, false)
            .expect_err("a hash unlike its baseline at the same size refuses the stage");
        assert!(err.to_string().contains("BITROT suspected: a.bin"), "{err}");
        let left: Vec<_> = fs::read_dir(&config.staging.directory)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(left.is_empty(), "nothing is left in staging: {left:?}");
        let baseline: String = conn
            .query_row(
                "SELECT sha256 FROM files WHERE snapshot_id = ?1 AND path = 'a.bin'",
                params![snap_id],
                |r| r.get(0),
            )
            .unwrap();
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
    /// `stage_slices` row is written, and the isolated dar catalogue is
    /// synced once it is re-isolated. Driven through a real stage (real dar)
    /// cut into several slices; the order is read off the pipeline's own
    /// durability log. Since issue #370 there is no plaintext slice to
    /// delete: none is ever written (`when_dar_finishes_staging_holds_only_ciphertext`).
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

        let catalogue: String = conn
            .query_row(
                "SELECT catalog_path FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            log.iter().any(|e| matches!(e, Event::Synced(p)
                if p.to_string_lossy().starts_with(&format!("{catalogue}."))
                    && p.to_string_lossy().ends_with(".dar"))),
            "the dar catalogue was never synced: {log:?}"
        );
    }

    /// Issue #41: `write_stage_report` and `secure_catalog_files` tested
    /// directly and hermetically — no dar binary, no full `stage_create`
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

        #[test]
        fn secure_catalog_files_tightens_a_loose_file_dar_wrote() {
            let tmp = TempDir::new().unwrap();
            let catalog_dir = tmp.path().join("catalogs").join("abcd1234");
            fs::create_dir_all(&catalog_dir).unwrap();
            // Simulate what dar's `-C` extraction leaves behind: a file
            // written by an external subprocess, at whatever mode the
            // process umask handed out — not tapectl's own `OpenOptions`.
            let catalog_file = catalog_dir.join("abcd1234_v1.1.dar");
            fs::write(&catalog_file, b"fake dar catalog bytes").unwrap();
            fs::set_permissions(&catalog_file, fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(mode_of(&catalog_file), 0o644, "fixture must start loose");

            secure_catalog_files(&catalog_dir);

            assert_eq!(
                mode_of(&catalog_file),
                0o600,
                "a catalog file dar wrote should be tightened to 0600"
            );
        }

        #[test]
        fn secure_catalog_files_tightens_every_file_present() {
            let tmp = TempDir::new().unwrap();
            let catalog_dir = tmp.path().join("catalogs").join("multi");
            fs::create_dir_all(&catalog_dir).unwrap();
            for name in ["a.1.dar", "a.2.dar", "a.3.dar"] {
                let p = catalog_dir.join(name);
                fs::write(&p, b"slice").unwrap();
                fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
            }

            secure_catalog_files(&catalog_dir);

            for name in ["a.1.dar", "a.2.dar", "a.3.dar"] {
                assert_eq!(
                    mode_of(&catalog_dir.join(name)),
                    0o600,
                    "{name} should be 0600"
                );
            }
        }

        #[test]
        fn secure_catalog_files_on_missing_dir_does_not_panic() {
            let tmp = TempDir::new().unwrap();
            let ghost = tmp.path().join("does-not-exist");
            // Best-effort: must not panic even if the directory somehow
            // isn't there (e.g. dar failed before this is ever reached in
            // the real call site, though that path already returns `?`
            // earlier and never gets here).
            secure_catalog_files(&ghost);
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

    /// The pin: each backfill UPDATE finds its row through the
    /// UNIQUE(snapshot_id, path) index. Before migration 027 the second
    /// UPDATE, into `manifest_entries`, searched by `manifest_id` alone and
    /// scanned the whole manifest per file.
    #[test]
    fn backfill_checksums_searches_the_unique_index() {
        let conn = crate::db::open_memory().unwrap();
        let plan = query_plan(&conn, BACKFILL_SQL);
        assert_eq!(
            plan,
            vec!["SEARCH files USING INDEX sqlite_autoindex_files_1 (snapshot_id=? AND path=?)"],
            "the backfill must be one indexed lookup per file"
        );
    }

    /// Timing-independent linearity: the backfill never takes a full-scan
    /// step, and the virtual-machine steps it spends per file do not grow
    /// with the snapshot's size. A per-file scan (the pre-027 shape) makes
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
                    tx.execute(
                        "INSERT INTO files (snapshot_id, path, size_bytes) VALUES (?1, ?2, 1)",
                        params![sid, path],
                    )
                    .unwrap();
                    if sid == 2 {
                        checksums.push((path, format!("{f:064x}")));
                    }
                }
            }
            tx.commit().unwrap();

            let mut stmt = conn.prepare(BACKFILL_SQL).unwrap();
            for (path, hash) in &checksums {
                stmt.execute(params![hash, 2i64, path]).unwrap();
            }
            let vm = stmt.get_status(rusqlite::StatementStatus::VmStep);
            let fullscan = stmt.get_status(rusqlite::StatementStatus::FullscanStep);
            let done: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM files WHERE snapshot_id = 2 AND sha256 IS NOT NULL",
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

        // Positive control: the same measurement on the UPDATE this
        // replaced (schema 26, `manifest_entries` searched by `manifest_id`
        // alone) sees the per-file cost grow with the manifest.
        fn old_steps_per_file(files: i64) -> f64 {
            let conn = crate::db::open_memory_at_version(26);
            conn.execute_batch(
                "INSERT INTO tenants (id, name, is_operator, status) VALUES (1, 't', 1, 'active');
                 INSERT INTO units (id, uuid, name, tenant_id) VALUES (1, 'u', 'u', 1);
                 INSERT INTO snapshots (id, unit_id, version, source_path) VALUES (1, 1, 1, '/s');
                 INSERT INTO manifests (id, snapshot_id) VALUES (1, 1);",
            )
            .unwrap();
            let tx = conn.unchecked_transaction().unwrap();
            for f in 0..files {
                tx.execute(
                    "INSERT INTO manifest_entries (manifest_id, path, size_bytes, mtime)
                     VALUES (1, ?1, 1, 'm')",
                    params![format!("f{f:06}")],
                )
                .unwrap();
            }
            tx.commit().unwrap();
            let mut stmt = conn
                .prepare(
                    "UPDATE manifest_entries SET sha256 = ?1
                     WHERE manifest_id = (SELECT id FROM manifests WHERE snapshot_id = ?2 LIMIT 1)
                     AND path = ?3 AND sha256 IS NULL",
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
            "the measurement must see the pre-027 quadratic: {old_small:.1} vs {old_large:.1}"
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
            "CREATE TEMP TRIGGER inject_mid_insert BEFORE INSERT ON files
             WHEN (SELECT COUNT(*) FROM files WHERE snapshot_id = NEW.snapshot_id) >= 2
             BEGIN SELECT RAISE(ABORT, 'injected mid-insert failure'); END;",
        )
        .unwrap();

        let err = snapshot_create(&conn, "unit1", &Config::default()).unwrap_err();
        assert!(err.to_string().contains("injected"), "{err}");
        for (what, sql) in [
            ("snapshots", "SELECT COUNT(*) FROM snapshots"),
            ("files", "SELECT COUNT(*) FROM files"),
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
                "SELECT COUNT(*) FROM files WHERE snapshot_id = ?1 AND is_directory = 0",
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
    /// both numbers and the recovery, before any stage set or dar run.
    #[test]
    fn stage_create_refuses_a_snapshot_with_a_short_file_list() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.dar.binary = "/nonexistent/dar-must-never-run".to_string();
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
        conn.execute(
            "DELETE FROM files WHERE snapshot_id = ?1 AND path = 'f3.txt'",
            params![snap_id],
        )
        .unwrap();

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
        assert!(!msg.contains("dar-must-never-run"), "{msg}");
        let stage_sets: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stage_sets, 0, "the refusal precedes the stage_sets INSERT");
    }
}
