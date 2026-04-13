use std::fs;
use std::path::Path;

use rusqlite::{params, Connection};
use tracing::warn;

use crate::db::events;
use crate::error::Result;

/// Clean staged files from disk and update DB.
/// Only cleans stage_sets that have at least one completed write (safe default)
/// or all staged sets if `force` is true.
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
         AND EXISTS (SELECT 1 FROM writes w WHERE w.stage_set_id = ss.id AND w.status = 'completed')
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
