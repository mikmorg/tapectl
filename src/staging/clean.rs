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
/// at nothing if the process died between the unlink and the DB update —
/// worse than leaving an orphaned file behind for a later `staging clean`
/// to pick up.
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
            warn!(path = %path.display(), error = %e, "failed to remove staged file");
            report.errors += 1;
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
