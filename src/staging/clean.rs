use std::fs;
use std::path::Path;

use rusqlite::{params, Connection};
use tracing::warn;

use crate::db::events;
use crate::error::Result;

/// Clean staged files from disk and update DB.
///
/// Only cleans stage_sets where EVERY planned copy has sealed (safe
/// default), or all staged sets regardless if `force` is true.
///
/// The default guard requires (a) at least one `writes` row exists for the
/// stage_set (it was actually planned onto some volume), AND (b) no
/// `writes` row for it is anything other than `'completed'`. This is
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
/// change.
pub fn clean_staging(conn: &Connection, force: bool) -> Result<CleanReport> {
    let mut report = CleanReport::default();

    let sql = if force {
        "SELECT ss.id, ss.total_encrypted_size, sl.staging_path, sl.id
         FROM stage_sets ss
         JOIN stage_slices sl ON sl.stage_set_id = ss.id
         WHERE ss.status = 'staged' AND sl.staging_path IS NOT NULL
         ORDER BY ss.id, sl.slice_number"
    } else {
        "SELECT ss.id, ss.total_encrypted_size, sl.staging_path, sl.id
         FROM stage_sets ss
         JOIN stage_slices sl ON sl.stage_set_id = ss.id
         WHERE ss.status = 'staged' AND sl.staging_path IS NOT NULL
         AND EXISTS (SELECT 1 FROM writes w WHERE w.stage_set_id = ss.id)
         AND NOT EXISTS (
             SELECT 1 FROM writes w WHERE w.stage_set_id = ss.id AND w.status <> 'completed'
         )
         ORDER BY ss.id, sl.slice_number"
    };

    let mut stmt = conn.prepare(sql)?;
    let rows: Vec<(i64, Option<i64>, String, i64)> = stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut cleaned_sets = std::collections::HashSet::new();

    for (stage_set_id, _size, staging_path, slice_id) in &rows {
        let path = Path::new(staging_path);
        if path.exists() {
            let file_size = fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0);
            match fs::remove_file(path) {
                Ok(()) => {
                    report.files_removed += 1;
                    report.bytes_freed += file_size;
                }
                Err(e) => {
                    warn!(path = %staging_path, error = %e, "failed to remove staged file");
                    report.errors += 1;
                    continue;
                }
            }
        }

        conn.execute(
            "UPDATE stage_slices SET staging_path = NULL WHERE id = ?1",
            params![slice_id],
        )?;

        cleaned_sets.insert(*stage_set_id);
    }

    for stage_set_id in &cleaned_sets {
        let old_status: Option<String> = conn
            .query_row(
                "SELECT status FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |row| row.get(0),
            )
            .ok();
        conn.execute(
            "UPDATE stage_sets SET status = 'cleaned', cleaned_at = datetime('now')
             WHERE id = ?1",
            params![stage_set_id],
        )?;
        events::log_field_change(
            conn,
            "stage_set",
            *stage_set_id,
            &format!("stage_set_{stage_set_id}"),
            "status_change",
            "status",
            old_status.as_deref(),
            "cleaned",
            None,
        )?;
        report.sets_cleaned += 1;
    }

    Ok(report)
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

    /// Seed one tenant/unit/snapshot/stage_set with one staged slice (real
    /// bytes on disk, so `clean_staging` has something to remove), returning
    /// `(conn, stage_set_id, staging_path, TempDir guard)`.
    fn seed_stage_set() -> (Connection, i64, std::path::PathBuf, tempfile::TempDir) {
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
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
            params![snapshot_id],
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
        let (conn, _stage_set_id, path, _dir) = seed_stage_set();
        seed_write(&conn, _stage_set_id, "V1", "completed");

        let report = clean_staging(&conn, false).unwrap();
        assert_eq!(report.sets_cleaned, 1);
        assert_eq!(report.files_removed, 1);
        assert!(!path.exists(), "staged file should have been removed");
    }

    #[test]
    fn default_guard_refuses_when_no_write_exists_at_all() {
        let (conn, _stage_set_id, path, _dir) = seed_stage_set();

        let report = clean_staging(&conn, false).unwrap();
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
            let (conn, stage_set_id, path, _dir) = seed_stage_set();
            seed_write(&conn, stage_set_id, "V-SEALED", "completed");
            seed_write(&conn, stage_set_id, "V-PENDING", blocking_status);

            let report = clean_staging(&conn, false).unwrap();
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
            let (conn, stage_set_id, path, _dir) = seed_stage_set();
            seed_write(&conn, stage_set_id, "V-SEALED", "completed");
            seed_write(&conn, stage_set_id, "V-DEAD", terminal_non_success);

            let report = clean_staging(&conn, false).unwrap();
            assert_eq!(
                report.sets_cleaned, 0,
                "must not clean while a copy is '{terminal_non_success}'"
            );
            assert!(path.exists());
        }
    }

    #[test]
    fn force_cleans_regardless_of_write_status() {
        let (conn, stage_set_id, path, _dir) = seed_stage_set();
        seed_write(&conn, stage_set_id, "V-PENDING", "planned");

        let report = clean_staging(&conn, true).unwrap();
        assert_eq!(report.sets_cleaned, 1);
        assert!(!path.exists());
    }
}
