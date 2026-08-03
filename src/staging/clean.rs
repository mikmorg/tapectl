use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use tracing::warn;

use crate::config::Config;
use crate::db::events;
use crate::error::Result;

/// Clean staged files from disk and update DB.
///
/// Cleans two kinds of stage_sets:
///
/// - `'staged'` sets where EVERY planned copy has sealed (safe default), or
///   all `'staged'` sets regardless if `force` is true.
/// - `'failed'` sets (issue #98) — unconditionally, `force` or not. A
///   `'failed'` set never reached `'staged'`, so it by construction has no
///   `writes` row referencing it; there is no "wait for every planned copy"
///   invariant to protect, unlike the `'staged'` guard below.
///
/// The `'staged'` default guard requires (a) at least one `writes` row
/// exists for the stage_set (it was actually planned onto some volume), AND
/// (b) no `writes` row for it is anything other than `'completed'`. This is
/// stricter than "at least one completed write": a stage_set batched onto
/// two cartridges (2 copies) with one sealed and the other still
/// `'planned'`/`'in_progress'`/`'interrupted'` — or terminally `'aborted'`/
/// `'failed'`, meaning that copy never happened — must NOT be cleaned,
/// because that non-terminal-success session's rewrite source is exactly
/// these staged files (`docs/design/v2-open-questions.md` §3.5: "GC refuses
/// to delete anything referenced by a `writes` row not in a terminal-success
/// state"; §11: "release staging only after *every planned copy* is
/// sealed"). `--force` is the deliberate operator override for a stage_set
/// stuck behind a copy that will never complete (e.g. the operator
/// re-planned onto a different volume entirely) — it is unaffected by this
/// change and has no bearing on `'failed'` sets, which are always eligible.
///
/// Two different discovery strategies for orphaned files, matching
/// `staging::cleanup_failed_stage_set` (issue #54) exactly:
///
/// - `.age` files: strictly by `stage_slices.stage_set_id` DB rows — never a
///   prefix glob.
/// - Plaintext `.dar`/`.sha512` orphans of a `'failed'` set (dar writes all
///   slices up front, so most such orphans have no `stage_slices` row at
///   all — the crash typically happens before any slice is encrypted): by
///   dot-terminated filesystem prefix, via `staging::archive_base_prefix`,
///   scanned under `config.staging.directory`. `'staged'` sets need no such
///   scan — their plaintext slices were already deleted, one by one, as
///   each was encrypted.
///
/// Ordering is load-bearing: `stage_slices.staging_path` is the ONLY handle
/// on a staged `.age` file, so every DB change (nulling `staging_path`,
/// marking the stage_set `'cleaned'`) is collected and committed BEFORE any
/// file is unlinked. Unlinking first would strand a live snapshot pointing
/// at nothing if the process died between the unlink and the DB update.
/// Do NOT invert this.
///
/// **The cost of that ordering, stated honestly (issue #108).** An earlier
/// version of this comment justified the ordering as "better than leaving an
/// orphaned file behind for a later `staging clean` to pick up". That was
/// false, and the falsehood mattered: a later clean **cannot** pick it up.
/// `.age` discovery is strictly `staging_path IS NOT NULL`, which is the
/// column this function has just nulled, and `find_plaintext_orphans` runs
/// only for `'failed'` sets and only matches `.dar`/`.sha512`. So a file
/// whose unlink fails is invisible to every future cleanup path, **forever**,
/// and only a human can remove it.
///
/// The ordering is still right — a row pointing at a file that is gone is
/// worse than a file no row points at — so the fix is not to reorder but to
/// make the stranding *visible at the moment it happens*. Every failed unlink
/// is recorded in [`CleanReport::stranded`] with its path, and the CLI prints
/// those paths and says plainly that they are permanent. A bare error count
/// (what this used to report) tells an operator that something is wrong but
/// not which file, which for a one-shot, never-repeated notice is the same as
/// telling them nothing.
///
/// A filesystem sweep to rediscover such files was considered and rejected as
/// disproportionate: #95 established that an orphan with no referencing row
/// cannot be deleted without `--force` anyway (a live `build()` is
/// indistinguishable from crash garbage), so a sweep would produce a report
/// rather than a cleanup — which is what naming the path here already does,
/// without a new scan that could race a writer.
pub fn clean_staging(conn: &Connection, config: &Config, force: bool) -> Result<CleanReport> {
    let mut report = CleanReport::default();

    let candidate_sql = if force {
        "SELECT id, status FROM stage_sets WHERE status IN ('staged', 'failed')"
    } else {
        "SELECT id, status FROM stage_sets
         WHERE status = 'failed'
            OR (status = 'staged'
                AND EXISTS (SELECT 1 FROM writes w WHERE w.stage_set_id = stage_sets.id)
                AND NOT EXISTS (
                    SELECT 1 FROM writes w
                    WHERE w.stage_set_id = stage_sets.id AND w.status <> 'completed'
                ))"
    };

    let candidates: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(candidate_sql)?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };

    if candidates.is_empty() {
        return Ok(report);
    }

    struct SetPlan {
        stage_set_id: i64,
        age_files: Vec<(i64, String)>,
        plaintext_orphans: Vec<PathBuf>,
    }

    let staging_dir = Path::new(&config.staging.directory);
    let mut plans = Vec::with_capacity(candidates.len());

    for (stage_set_id, status) in &candidates {
        let age_files: Vec<(i64, String)> = {
            let mut stmt = conn.prepare(
                "SELECT id, staging_path FROM stage_slices
                 WHERE stage_set_id = ?1 AND staging_path IS NOT NULL",
            )?;
            let rows = stmt
                .query_map(params![stage_set_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };

        // Only a 'failed' set can have plaintext .dar/.sha512 orphans left
        // behind — a 'staged' set's dar output was already removed slice by
        // slice during encryption (see stage_create_inner).
        let plaintext_orphans = if status == "failed" {
            find_plaintext_orphans(conn, staging_dir, *stage_set_id)?
        } else {
            Vec::new()
        };

        plans.push(SetPlan {
            stage_set_id: *stage_set_id,
            age_files,
            plaintext_orphans,
        });
    }

    // Commit the DB change FIRST — null every collected staging_path and
    // move the stage_set to 'cleaned' — before any file is unlinked below.
    for plan in &plans {
        for (slice_id, _) in &plan.age_files {
            conn.execute(
                "UPDATE stage_slices SET staging_path = NULL WHERE id = ?1",
                params![slice_id],
            )?;
        }

        let old_status: Option<String> = conn
            .query_row(
                "SELECT status FROM stage_sets WHERE id = ?1",
                params![plan.stage_set_id],
                |row| row.get(0),
            )
            .ok();
        conn.execute(
            "UPDATE stage_sets SET status = 'cleaned', cleaned_at = datetime('now')
             WHERE id = ?1",
            params![plan.stage_set_id],
        )?;
        events::log_field_change(
            conn,
            "stage_set",
            plan.stage_set_id,
            &format!("stage_set_{}", plan.stage_set_id),
            "status_change",
            "status",
            old_status.as_deref(),
            "cleaned",
            None,
        )?;
        report.sets_cleaned += 1;
    }

    // THEN unlink — the DB no longer points at any of these paths.
    for plan in &plans {
        for (_, staging_path) in &plan.age_files {
            remove_and_account(Path::new(staging_path), &mut report);
        }
        for path in &plan.plaintext_orphans {
            remove_and_account(path, &mut report);
        }
    }

    Ok(report)
}

fn remove_and_account(path: &Path, report: &mut CleanReport) {
    if !path.exists() {
        return;
    }
    let file_size = fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0);
    match fs::remove_file(path) {
        Ok(()) => {
            report.files_removed += 1;
            report.bytes_freed += file_size;
        }
        Err(e) => {
            // Issue #108: record the PATH, not just a tally. `staging_path`
            // has already been nulled by the time this runs, so nothing will
            // ever rediscover this file — this notice is the operator's only
            // chance to learn it exists.
            warn!(
                path = %path.display(),
                error = %e,
                "failed to remove staged file — it is now stranded permanently \
                 and must be removed by hand"
            );
            report.errors += 1;
            report.stranded.push(path.to_path_buf());
        }
    }
}

/// Plaintext `.dar`/`.sha512` files left in `staging_dir` by a crashed dar
/// run for `stage_set_id` — same dot-terminated prefix rule as
/// `staging::cleanup_failed_stage_set`'s `archive_base_prefix` (issue #54):
/// dar names every slice `{base}.{N}.dar`, so `{base}.` (not the bare base)
/// is the real filesystem prefix, keeping `_s1` from prefix-matching
/// `_s10.1.dar`.
fn find_plaintext_orphans(
    conn: &Connection,
    staging_dir: &Path,
    stage_set_id: i64,
) -> Result<Vec<PathBuf>> {
    let resolved = conn
        .query_row(
            "SELECT u.uuid, sn.version
             FROM stage_sets ss
             JOIN snapshots sn ON sn.id = ss.snapshot_id
             JOIN units u ON u.id = sn.unit_id
             WHERE ss.id = ?1",
            params![stage_set_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .ok();

    let Some((uuid, version)) = resolved else {
        return Ok(Vec::new());
    };

    let prefix = crate::staging::archive_base_prefix(&uuid, version, stage_set_id);
    let mut found = Vec::new();
    if let Ok(entries) = fs::read_dir(staging_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix) && (name.ends_with(".dar") || name.ends_with(".sha512")) {
                found.push(entry.path());
            }
        }
    }
    Ok(found)
}

/// Show staging status.
pub fn staging_status(conn: &Connection) -> Result<Vec<StagingInfo>> {
    let mut stmt = conn.prepare(
        "SELECT ss.id, u.name, s.version, ss.status, ss.num_slices,
                ss.total_encrypted_size, ss.staged_at,
                COUNT(w.id) as write_count
         FROM stage_sets ss
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN units u ON u.id = s.unit_id
         LEFT JOIN writes w ON w.stage_set_id = ss.id AND w.status = 'completed'
         WHERE ss.status IN ('staged', 'staging')
         GROUP BY ss.id
         ORDER BY ss.staged_at DESC",
    )?;

    let rows = stmt
        .query_map([], |row| {
            Ok(StagingInfo {
                stage_set_id: row.get(0)?,
                unit_name: row.get(1)?,
                version: row.get(2)?,
                status: row.get(3)?,
                num_slices: row.get(4)?,
                total_encrypted_size: row.get(5)?,
                staged_at: row.get(6)?,
                write_count: row.get(7)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(rows)
}

#[derive(Debug, Default)]
pub struct CleanReport {
    pub sets_cleaned: usize,
    pub files_removed: usize,
    pub bytes_freed: i64,
    pub errors: usize,
    /// Files whose unlink failed (issue #108). Their `staging_path` rows are
    /// already nulled, so no future `staging clean` can rediscover them —
    /// these paths are the operator's ONLY notice that the files exist, and
    /// removing them is a manual job. Always a subset of `errors`; carried
    /// separately because a count alone does not say *which* file.
    pub stranded: Vec<std::path::PathBuf>,
    /// `sessions/*` directories removed because every `writes` row
    /// referencing them reached a terminal state (`completed`, `failed`,
    /// `aborted`) — see `reclaim_session_dirs`.
    pub session_dirs_reclaimed: usize,
    /// `sessions/*` directories left untouched because at least one
    /// referencing `writes` row is still `planned`, `in_progress`, or
    /// `interrupted` (§3.5: a resumable session's rewrite source).
    pub session_dirs_retained: usize,
    /// `sessions/*` directories with NO referencing `writes` row at all —
    /// could be crash garbage from a `build()` that never reached `plan()`,
    /// or a concurrent write's session dir created moments ago. Never
    /// removed unless `force` is set.
    pub session_dirs_orphaned: usize,
    /// `locks/stage-<id>.lock` files removed because their stage_set
    /// reached a terminal state (`staged`, `failed`, `cleaned`).
    pub lockfiles_reclaimed: usize,
}

/// Reclaim `{staging.directory}/sessions/*` directories and terminal
/// `locks/stage-<id>.lock` files (issue #95).
///
/// ## Session directories
///
/// `build()` (`src/volume/write.rs`) materializes every generated zone of a
/// volume write into `{staging.directory}/sessions/{label}-{uuid}/`, and
/// nothing else in the write path ever deletes that directory. Since #25 it
/// is also the **only** rehydrate source for a resumable session (`writes`
/// rows carry it in `session_dir`), so it cannot be swept blindly.
///
/// A session dir is matched to `writes` rows by an EXACT equality compare —
/// `writes.session_dir = <dir path>` — never a prefix or label match: two
/// write attempts for the same volume label differ only by the trailing
/// uuid, and a prefix match would silently reap the live one.
///
/// - **RETAIN** (untouched) while any referencing row is `planned`,
///   `in_progress`, or `interrupted`. `v2-open-questions.md` §3.5's guard is
///   "not in a terminal-success state" read literally, but that wording
///   would also retain `failed`/`aborted` rows forever, recreating exactly
///   the leak this issue exists to fix. §3.5's own parenthetical states the
///   real intent: "a GC that reaps an **interrupted** session's slices
///   silently converts 'resumable' into 'aborted'" — only `interrupted`
///   (plus the still-live `planned`/`in_progress`) is resumable
///   (`layout-session.md`'s rehydrate adopts only `interrupted` rows), so
///   those are the only statuses that must block reclamation.
/// - **RECLAIM** (remove recursively, size added to `bytes_freed`) once at
///   least one row references the dir and every one of them is
///   `completed`, `failed`, or `aborted` — none of those states leave a
///   resumable session behind.
/// - **ORPHAN** (report only, `session_dirs_orphaned`) when no `writes` row
///   references the dir at all. This is indistinguishable from a
///   concurrent `volume write` whose `build()` has created the directory
///   but whose `plan()` has not yet committed its `writes` rows — the same
///   race #98 eliminated from the staging sweep. Deleting it here would
///   destroy a live session's frozen zones mid-build, so an orphan is only
///   removed when the operator explicitly passes `force` (a leaked
///   directory is strictly better than racing a live writer).
///
/// ## Lockfiles
///
/// `lock::lock_path` (issue #98) never has a matching remover, so one
/// lockfile accumulates per stage_set forever. A lockfile is safe to
/// unlink only once its stage_set is terminal (`staged`, `failed`,
/// `cleaned` — the full non-`staging` set of `stage_sets.status`'s CHECK
/// constraint): `stage_set_id`s are unique and monotonic, so a terminal
/// set's lock can never be re-acquired by a future stage attempt, and thus
/// a stale inode can never be raced by a fresh flock on a same-named file
/// the way it theoretically could for a still-`staging` set (a live holder
/// plus a second process recreating the path would each lock a different
/// inode and both believe they hold it). `staging` is deliberately excluded
/// even though the CHECK only has four values.
pub fn reclaim_session_dirs_and_lockfiles(
    conn: &Connection,
    config: &Config,
    db_file: &Path,
    force: bool,
    report: &mut CleanReport,
) -> Result<()> {
    reclaim_session_dirs(conn, config, force, report)?;
    reclaim_lockfiles(conn, db_file, report)?;
    Ok(())
}

fn reclaim_session_dirs(
    conn: &Connection,
    config: &Config,
    force: bool,
    report: &mut CleanReport,
) -> Result<()> {
    let sessions_root = Path::new(&config.staging.directory).join("sessions");
    let entries = match fs::read_dir(&sessions_root) {
        Ok(e) => e,
        Err(_) => return Ok(()), // no sessions dir yet — nothing to do
    };

    const NON_TERMINAL: [&str; 3] = ["planned", "in_progress", "interrupted"];

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let path_str = path.to_string_lossy().to_string();

        let statuses: Vec<String> = {
            let mut stmt = conn.prepare("SELECT status FROM writes WHERE session_dir = ?1")?;
            let rows = stmt
                .query_map(params![path_str], |row| row.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };

        if statuses.is_empty() {
            report.session_dirs_orphaned += 1;
            if force {
                remove_dir_and_account(&path, report);
            }
            continue;
        }

        let any_non_terminal = statuses.iter().any(|s| NON_TERMINAL.contains(&s.as_str()));
        if any_non_terminal {
            report.session_dirs_retained += 1;
            continue;
        }

        // Every referencing row is completed/failed/aborted — reclaim.
        remove_dir_and_account(&path, report);
        report.session_dirs_reclaimed += 1;
    }

    Ok(())
}

fn remove_dir_and_account(path: &Path, report: &mut CleanReport) {
    let size = dir_size(path);
    match fs::remove_dir_all(path) {
        Ok(()) => {
            report.bytes_freed += size;
        }
        Err(e) => {
            warn!(path = %path.display(), error = %e, "failed to remove session dir");
            report.errors += 1;
        }
    }
}

fn dir_size(path: &Path) -> i64 {
    let mut total = 0i64;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                total += dir_size(&p);
            } else if let Ok(meta) = fs::metadata(&p) {
                total += meta.len() as i64;
            }
        }
    }
    total
}

fn reclaim_lockfiles(conn: &Connection, db_file: &Path, report: &mut CleanReport) -> Result<()> {
    let terminal_sets: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT id FROM stage_sets WHERE status IN ('staged', 'failed', 'cleaned')")?;
        let rows = stmt
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };

    for stage_set_id in terminal_sets {
        let path = crate::staging::lock::lock_path(db_file, stage_set_id);
        if !path.exists() {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => report.lockfiles_reclaimed += 1,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to remove stage lockfile");
                report.errors += 1;
            }
        }
    }

    Ok(())
}

#[derive(Debug)]
pub struct StagingInfo {
    pub stage_set_id: i64,
    pub unit_name: String,
    pub version: i64,
    pub status: String,
    pub num_slices: Option<i64>,
    pub total_encrypted_size: Option<i64>,
    pub staged_at: Option<String>,
    pub write_count: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use std::io::Write;

    /// A `Config` whose `staging.directory` points at `dir` — the only
    /// field `clean_staging` reads from it.
    fn config_for(dir: &Path) -> Config {
        Config {
            staging: crate::config::StagingConfig {
                directory: dir.to_string_lossy().to_string(),
            },
            ..Default::default()
        }
    }

    /// Seed one tenant/unit/snapshot/stage_set with one staged slice (real
    /// bytes on disk, so `clean_staging` has something to remove), returning
    /// `(conn, stage_set_id, staging_path, TempDir guard)`.
    fn seed_stage_set() -> (Connection, i64, std::path::PathBuf, tempfile::TempDir) {
        seed_stage_set_with_status("staged")
    }

    fn seed_stage_set_with_status(
        status: &str,
    ) -> (Connection, i64, std::path::PathBuf, tempfile::TempDir) {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES ('u-uuid', 'u', ?1, '/tmp/u', 'active')",
            params![tenant_id],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'staged', '/tmp/u', 1, 10)",
            params![unit_id],
        )
        .unwrap();
        let snapshot_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, ?2, 524288)",
            params![snapshot_id, status],
        )
        .unwrap();
        let stage_set_id = conn.last_insert_rowid();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("slice_1.age");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(b"staged slice bytes")
            .unwrap();
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                        sha256_plain, sha256_encrypted, staging_path)
             VALUES (?1, 1, 19, 19, 'deadbeef', 'deadbeef', ?2)",
            params![stage_set_id, path.to_string_lossy()],
        )
        .unwrap();

        (conn, stage_set_id, path, dir)
    }

    /// A `'failed'` stage_set with NO `stage_slices` rows (the common shape
    /// for a crash before the encryption loop starts) but real plaintext
    /// `.dar`/`.sha512` files sitting under `staging_dir`, named exactly the
    /// way `stage_create_inner`'s dar run would have named them. Returns
    /// `(conn, stage_set_id, staging_dir, dar_path, sha512_path, TempDir guard)`.
    fn seed_failed_stage_set_with_plaintext_orphan() -> (
        Connection,
        i64,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
        tempfile::TempDir,
    ) {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        let unit_uuid = "0123456789abcdef0123456789abcdef";
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES (?1, 'u', ?2, '/tmp/u', 'active')",
            params![unit_uuid, tenant_id],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'created', '/tmp/u', 1, 10)",
            params![unit_id],
        )
        .unwrap();
        let snapshot_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'failed', 524288)",
            params![snapshot_id],
        )
        .unwrap();
        let stage_set_id = conn.last_insert_rowid();

        let dir = tempfile::tempdir().unwrap();
        let prefix = crate::staging::archive_base_prefix(unit_uuid, 1, stage_set_id);
        let dar_path = dir.path().join(format!("{prefix}1.dar"));
        let sha_path = dir.path().join(format!("{prefix}1.sha512"));
        std::fs::write(&dar_path, b"plaintext dar slice").unwrap();
        std::fs::write(&sha_path, b"deadbeef").unwrap();

        (
            conn,
            stage_set_id,
            dir.path().to_path_buf(),
            dar_path,
            sha_path,
            dir,
        )
    }

    /// A volume + a `writes` row for `stage_set_id` at the given status.
    fn seed_write(conn: &Connection, stage_set_id: i64, label: &str, status: &str) {
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
             VALUES (?1, 'lto', 'lto0', 2500000000000, 'active')",
            params![label],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();
        let snapshot_id: i64 = conn
            .query_row(
                "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, ?4)",
            params![stage_set_id, snapshot_id, volume_id, status],
        )
        .unwrap();
    }

    #[test]
    fn default_guard_cleans_when_the_only_planned_copy_completed() {
        let (conn, _stage_set_id, path, dir) = seed_stage_set();
        seed_write(&conn, _stage_set_id, "V1", "completed");

        let report = clean_staging(&conn, &config_for(dir.path()), false).unwrap();
        assert_eq!(report.sets_cleaned, 1);
        assert_eq!(report.files_removed, 1);
        assert!(!path.exists(), "staged file should have been removed");
    }

    #[test]
    fn default_guard_refuses_when_no_write_exists_at_all() {
        let (conn, _stage_set_id, path, dir) = seed_stage_set();

        let report = clean_staging(&conn, &config_for(dir.path()), false).unwrap();
        assert_eq!(report.sets_cleaned, 0);
        assert_eq!(report.files_removed, 0);
        assert!(path.exists(), "never-written stage_set must not be cleaned");
    }

    /// The §3.5 regression case: a stage_set planned onto TWO cartridges (2
    /// copies) where one copy sealed but the other has not yet — the default
    /// guard must refuse, because the still-unsealed copy's session may need
    /// to re-read these exact staged files. The pre-T6 guard (`EXISTS
    /// completed`) would have wrongly cleaned this, since it only checked
    /// that AT LEAST ONE write was completed.
    #[test]
    fn default_guard_refuses_when_a_second_planned_copy_is_not_yet_sealed() {
        for blocking_status in ["planned", "in_progress", "interrupted"] {
            let (conn, stage_set_id, path, dir) = seed_stage_set();
            seed_write(&conn, stage_set_id, "V-SEALED", "completed");
            seed_write(&conn, stage_set_id, "V-PENDING", blocking_status);

            let report = clean_staging(&conn, &config_for(dir.path()), false).unwrap();
            assert_eq!(
                report.sets_cleaned, 0,
                "must not clean while a copy is '{blocking_status}'"
            );
            assert!(
                path.exists(),
                "staged file must survive while a copy is '{blocking_status}'"
            );
        }
    }

    /// Terminal-but-not-success copies (aborted/failed) also block the
    /// default guard: that planned copy never happened, so per §3.5/§11 the
    /// staging inputs are still "on the hook" until the operator either
    /// completes it or explicitly forces cleanup.
    #[test]
    fn default_guard_refuses_when_a_second_planned_copy_aborted_or_failed() {
        for terminal_non_success in ["aborted", "failed"] {
            let (conn, stage_set_id, path, dir) = seed_stage_set();
            seed_write(&conn, stage_set_id, "V-SEALED", "completed");
            seed_write(&conn, stage_set_id, "V-DEAD", terminal_non_success);

            let report = clean_staging(&conn, &config_for(dir.path()), false).unwrap();
            assert_eq!(
                report.sets_cleaned, 0,
                "must not clean while a copy is '{terminal_non_success}'"
            );
            assert!(path.exists());
        }
    }

    #[test]
    fn force_cleans_regardless_of_write_status() {
        let (conn, stage_set_id, path, dir) = seed_stage_set();
        seed_write(&conn, stage_set_id, "V-PENDING", "planned");

        let report = clean_staging(&conn, &config_for(dir.path()), true).unwrap();
        assert_eq!(report.sets_cleaned, 1);
        assert!(!path.exists());
    }

    /// Issue #98: a `'failed'` stage_set is cleaned unconditionally — no
    /// `writes` row can exist for it (it never reached `'staged'`), so the
    /// `'staged'` guard's "wait for every planned copy" logic doesn't apply.
    /// `--force` is not required.
    #[test]
    fn failed_set_is_cleaned_unconditionally_without_force() {
        let (conn, stage_set_id, path, dir) = seed_stage_set_with_status("failed");

        let report = clean_staging(&conn, &config_for(dir.path()), false).unwrap();
        assert_eq!(report.sets_cleaned, 1);
        assert!(!path.exists(), "failed set's .age file should be removed");

        let status: String = conn
            .query_row(
                "SELECT status FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "cleaned");
    }

    /// Issue #108: when an unlink FAILS, the file is stranded permanently —
    /// `staging_path` was nulled before the unlink, and `.age` discovery is
    /// strictly `staging_path IS NOT NULL`, so no later `staging clean` can
    /// ever find it. The report must therefore carry the PATH, not just bump
    /// an error counter: this is the operator's one and only notice.
    ///
    /// The failure is induced by making the file's parent directory
    /// read-only, which is the realistic shape (a read-only mount, an ACL, a
    /// permissions mistake) and needs no fault injection in the code itself.
    #[test]
    fn a_failed_unlink_is_reported_with_its_path_not_just_an_error_count() {
        let (conn, _stage_set_id, path, dir) = seed_stage_set_with_status("failed");

        // Move the .age into a subdirectory we can seal, so the rest of the
        // staging dir stays writable (session-dir reclamation walks it).
        let locked = dir.path().join("locked");
        fs::create_dir_all(&locked).unwrap();
        let moved = locked.join(path.file_name().unwrap());
        fs::rename(&path, &moved).unwrap();
        conn.execute(
            "UPDATE stage_slices SET staging_path = ?1",
            params![moved.to_str().unwrap()],
        )
        .unwrap();

        let mut perms = fs::metadata(&locked).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o500);
        fs::set_permissions(&locked, perms).unwrap();

        // Verify the seal actually blocks US before asserting on it. Root
        // ignores directory write permission, and so do some filesystems, so
        // without this probe the test would silently assert nothing on those
        // hosts. Probing the real precondition beats guessing at it from a
        // uid — and it needs no new dependency.
        if fs::write(locked.join(".probe"), b"x").is_ok() {
            let _ = fs::remove_file(locked.join(".probe"));
            let mut p = fs::metadata(&locked).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut p, 0o700);
            fs::set_permissions(&locked, p).unwrap();
            eprintln!(
                "SKIPPED: a read-only directory does not block writes here \
                 (running as root, or a filesystem that ignores the mode), so \
                 an unlink failure cannot be induced"
            );
            return;
        }

        let report = clean_staging(&conn, &config_for(dir.path()), false).unwrap();

        // Restore write permission first, so the TempDir can clean up even
        // if an assertion below fails.
        let mut perms = fs::metadata(&locked).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o700);
        fs::set_permissions(&locked, perms).unwrap();

        assert_eq!(report.errors, 1, "the unlink must have failed");
        assert_eq!(
            report.stranded.len(),
            1,
            "a failed unlink must be recorded with its path — nothing will \
             ever rediscover this file, so a bare count tells the operator \
             that something is wrong but not which file"
        );
        assert_eq!(report.stranded[0], moved);
        assert!(moved.exists(), "the file really is still there");

        // And the row is already nulled, which is exactly why no later clean
        // can find it — the property that makes naming the path essential.
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM stage_slices WHERE staging_path IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            remaining, 0,
            "staging_path is nulled before the unlink, so re-running clean \
             finds nothing — this is the permanence #108 is about"
        );
    }

    /// The happy path must not report anything as stranded, or the notice
    /// above becomes noise that gets ignored.
    #[test]
    fn a_successful_clean_strands_nothing() {
        let (conn, _id, path, dir) = seed_stage_set_with_status("failed");
        let report = clean_staging(&conn, &config_for(dir.path()), false).unwrap();
        assert!(!path.exists());
        assert_eq!(report.errors, 0);
        assert!(report.stranded.is_empty());
    }

    /// The common crash shape: a `'failed'` stage_set with zero
    /// `stage_slices` rows (the process died before the encryption loop
    /// ever inserted one) but real plaintext `.dar`/`.sha512` files dar left
    /// behind. `clean_staging` must find and remove them by dot-terminated
    /// filesystem prefix, exactly as `cleanup_failed_stage_set` (#54) would
    /// have if the process had lived long enough to run its own Err path.
    #[test]
    fn failed_set_with_no_slice_rows_removes_plaintext_orphans_by_prefix() {
        let (conn, stage_set_id, staging_dir, dar_path, sha_path, _dir) =
            seed_failed_stage_set_with_plaintext_orphan();

        let report = clean_staging(&conn, &config_for(&staging_dir), false).unwrap();
        assert_eq!(report.sets_cleaned, 1);
        assert_eq!(report.files_removed, 2, "both .dar and .sha512 orphans");
        assert!(!dar_path.exists(), "orphaned .dar must be removed");
        assert!(!sha_path.exists(), "orphaned .sha512 must be removed");

        let status: String = conn
            .query_row(
                "SELECT status FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "cleaned");
    }

    // --- Change 1: session directory reclamation (issue #95) ---

    /// A `writes` row referencing a session dir, at the given status, plus a
    /// small file inside the dir to prove size accounting.
    fn seed_session_dir_with_write(conn: &Connection, staging_dir: &Path, status: &str) -> PathBuf {
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES ('u-uuid-s', 'u', ?1, '/tmp/u', 'active')",
            params![tenant_id],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'staged', '/tmp/u', 1, 10)",
            params![unit_id],
        )
        .unwrap();
        let snapshot_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
            params![snapshot_id],
        )
        .unwrap();
        let stage_set_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
             VALUES ('V-SESS', 'lto', 'lto0', 2500000000000, 'active')",
            [],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();

        let session_dir = staging_dir
            .join("sessions")
            .join(format!("V-SESS-{status}"));
        fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("0000_id_thunk"), b"frozen bytes").unwrap();

        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, session_dir)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                stage_set_id,
                snapshot_id,
                volume_id,
                status,
                session_dir.to_string_lossy(),
            ],
        )
        .unwrap();

        session_dir
    }

    /// §3.5's explicit requirement: a session dir referenced by an
    /// `interrupted`/`planned`/`in_progress` row must survive reclamation —
    /// deleting it would silently convert a resumable session into an
    /// unrecoverable one.
    #[test]
    fn session_dir_survives_while_any_write_is_non_terminal() {
        for status in ["planned", "in_progress", "interrupted"] {
            let conn = db::open_memory().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let session_dir = seed_session_dir_with_write(&conn, dir.path(), status);

            let mut report = CleanReport::default();
            reclaim_session_dirs(&conn, &config_for(dir.path()), false, &mut report).unwrap();

            assert!(
                session_dir.exists(),
                "session dir must survive while a write is '{status}'"
            );
            assert_eq!(report.session_dirs_reclaimed, 0);
            assert_eq!(report.session_dirs_retained, 1);
        }
    }

    /// A session dir whose only referencing write is `completed` is removed
    /// and its bytes counted.
    #[test]
    fn session_dir_reclaimed_when_write_is_completed() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let session_dir = seed_session_dir_with_write(&conn, dir.path(), "completed");

        let mut report = CleanReport::default();
        reclaim_session_dirs(&conn, &config_for(dir.path()), false, &mut report).unwrap();

        assert!(!session_dir.exists());
        assert_eq!(report.session_dirs_reclaimed, 1);
        assert_eq!(report.bytes_freed, 12); // "frozen bytes".len()
    }

    /// Pins the wording-vs-intent decision: `failed` (with no non-terminal
    /// row) is reclaimable, not retained forever under a literal
    /// "terminal-success" reading.
    #[test]
    fn session_dir_reclaimed_when_write_is_failed() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let session_dir = seed_session_dir_with_write(&conn, dir.path(), "failed");

        let mut report = CleanReport::default();
        reclaim_session_dirs(&conn, &config_for(dir.path()), false, &mut report).unwrap();

        assert!(!session_dir.exists());
        assert_eq!(report.session_dirs_reclaimed, 1);
    }

    /// A session dir with NO referencing `writes` row is an orphan: left
    /// alone by default (indistinguishable from a concurrent build() that
    /// hasn't reached plan() yet), removed only with `force`.
    #[test]
    fn orphan_session_dir_survives_without_force_and_is_removed_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions").join("V-ORPHAN-uuid");
        fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("stray"), b"orphan bytes").unwrap();

        let conn = db::open_memory().unwrap();

        let mut report = CleanReport::default();
        reclaim_session_dirs(&conn, &config_for(dir.path()), false, &mut report).unwrap();
        assert!(session_dir.exists(), "orphan must survive without force");
        assert_eq!(report.session_dirs_orphaned, 1);

        let mut report = CleanReport::default();
        reclaim_session_dirs(&conn, &config_for(dir.path()), true, &mut report).unwrap();
        assert!(!session_dir.exists(), "orphan must be removed with force");
    }

    // --- Change 2: lockfile reclamation (issue #95) ---

    /// A lockfile for a `staging` (non-terminal) stage_set must survive —
    /// the load-bearing precondition: unlinking a lockfile that a live
    /// process might still hold (or re-acquire) risks a second flock
    /// succeeding on a fresh inode at the same path while the original
    /// holder's flock is still live on the old one. `staging` sets are the
    /// only ones this can happen to, since `stage_set_id`s are unique and
    /// monotonic — a terminal set's lock can never be contended again.
    #[test]
    fn lockfile_for_staging_stage_set_survives() {
        let (conn, stage_set_id, _path, _dir) = seed_stage_set_with_status("staging");

        let tmp = tempfile::TempDir::new().unwrap();
        let db_file = tmp.path().join("tapectl.db");
        let _lock = crate::staging::lock::acquire(&db_file, stage_set_id).unwrap();
        let path = crate::staging::lock::lock_path(&db_file, stage_set_id);
        assert!(path.exists());

        let mut report = CleanReport::default();
        reclaim_lockfiles(&conn, &db_file, &mut report).unwrap();
        assert!(path.exists(), "lockfile for a 'staging' set must survive");
        assert_eq!(report.lockfiles_reclaimed, 0);
    }

    /// Lockfiles for terminal stage_sets (`staged`, `failed`, `cleaned`) are
    /// removed.
    #[test]
    fn lockfile_for_terminal_stage_set_is_removed() {
        for status in ["staged", "failed", "cleaned"] {
            let (conn, stage_set_id, _path, _dir) = seed_stage_set_with_status(status);

            let tmp = tempfile::TempDir::new().unwrap();
            let db_file = tmp.path().join("tapectl.db");
            let _lock = crate::staging::lock::acquire(&db_file, stage_set_id).unwrap();
            drop(_lock);
            let path = crate::staging::lock::lock_path(&db_file, stage_set_id);
            assert!(path.exists());

            let mut report = CleanReport::default();
            reclaim_lockfiles(&conn, &db_file, &mut report).unwrap();
            assert!(
                !path.exists(),
                "lockfile for a '{status}' set must be removed"
            );
            assert_eq!(report.lockfiles_reclaimed, 1);
        }
    }

    /// A sibling stage_set's plaintext files must never be swept up by
    /// another stage_set's prefix scan — the same collision this repo's
    /// `archive_base_prefix` trailing-dot convention already guards against
    /// for `cleanup_failed_stage_set`. Uses stage_set ids 1 and 10 so a
    /// naive (non-dot-terminated) prefix match would wrongly conflate them.
    #[test]
    fn failed_set_prefix_scan_does_not_touch_a_sibling_stage_sets_files() {
        let (conn, stage_set_id, staging_dir, dar_path, _sha_path, dir) =
            seed_failed_stage_set_with_plaintext_orphan();

        // A sibling stage_set (different id) sharing the same staging_dir,
        // deliberately NOT matched by this test's clean_staging call (it's
        // still 'staging', not 'failed' — never a cleanup candidate here).
        let other_uuid = "0123456789abcdef0123456789abcdef";
        let other_prefix = crate::staging::archive_base_prefix(other_uuid, 1, stage_set_id * 10);
        let other_path = dir.path().join(format!("{other_prefix}1.dar"));
        std::fs::write(&other_path, b"sibling's plaintext slice").unwrap();

        let report = clean_staging(&conn, &config_for(&staging_dir), false).unwrap();
        assert_eq!(report.sets_cleaned, 1);
        assert!(!dar_path.exists());
        assert!(
            other_path.exists(),
            "a sibling stage_set's plaintext files must survive"
        );
    }
}
