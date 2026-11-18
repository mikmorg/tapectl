//! tar's report for each archive run a stage set made, kept verbatim in
//! `tar_create_reports` (issue #343; the table D11 renames): what tar was
//! run with, how it exited and what it wrote on its standard error.
//!
//! Best-effort, like every journal: a refused insert is a warning, never a
//! reason for a stage to fail or to report a different failure than the one
//! it had. The busy policy waits out a catalog another stage holds (#377).

use rusqlite::{params, Connection};
use tracing::warn;

use crate::db::busy::{self, BusyPolicy};

/// tar exited 0 and the stream passed every check.
pub const OUTCOME_COMPLETE: &str = "complete";
/// tar exited 1: a file changed as it read it.
pub const OUTCOME_FILES_CHANGED: &str = "files_changed";
/// tar exited 2 or another status, or the stream was refused.
pub const OUTCOME_FAILED: &str = "failed";
/// tapectl was asked to stop.
pub const OUTCOME_STOPPED: &str = "stopped";

/// One archive run's report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TarCreateReport {
    pub outcome: &'static str,
    /// The command as `stage_sets.tar_command` records it.
    pub command: String,
    pub exit_code: Option<i32>,
    pub stderr: Vec<u8>,
    /// tar's `--version` first line.
    pub tar_version: String,
}

/// Append `report` for stage set `stage_set_id` of `unit_name` v`version`.
/// Returns the row id, or `None` when the insert was refused.
pub fn record(
    conn: &Connection,
    stage_set_id: i64,
    unit_name: &str,
    version: i64,
    report: &TarCreateReport,
) -> Option<i64> {
    let inserted = busy::retry(BusyPolicy::DEFAULT, "tar's report", || {
        conn.execute(
            "INSERT INTO tar_create_reports
                 (stage_set_id, unit_name, snapshot_version, outcome, tar_command,
                  tar_exit_code, tar_stderr, tar_version, tapectl_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                stage_set_id,
                unit_name,
                version,
                report.outcome,
                report.command,
                report.exit_code,
                report.stderr,
                report.tar_version,
                crate::build_info::VERSION,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    });
    match inserted {
        Ok(id) => Some(id),
        Err(e) => {
            warn!(err = %e, unit = unit_name, "tar_create_reports insert failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The row keeps tar's bytes exactly (not decoded, not trimmed) and
    /// survives its stage set's deletion (`ON DELETE SET NULL`), names kept.
    #[test]
    fn a_report_is_kept_verbatim_and_outlives_its_stage_set() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO tenants (id, name, is_operator, status) VALUES (1, 't', 0, 'active');
             INSERT INTO units (id, uuid, name, tenant_id, status)
             VALUES (1, 'u-1', 'photos', 1, 'active');
             INSERT INTO snapshots (id, unit_id, version, status, source_path)
             VALUES (1, 1, 3, 'created', '/src');
             INSERT INTO stage_sets (id, snapshot_id, status, slice_size)
             VALUES (7, 1, 'staging', 512);",
        )
        .unwrap();
        let stderr = b"\xfftar: ./a: Cannot open: Permission denied\n  \n".to_vec();
        let report = TarCreateReport {
            outcome: OUTCOME_FAILED,
            command: "\"tar\" \"--create\" \"--file=-\"".into(),
            exit_code: Some(2),
            stderr: stderr.clone(),
            tar_version: "tar (GNU tar) 1.35".into(),
        };
        let id = record(&conn, 7, "photos", 3, &report).expect("recorded");
        conn.execute("DELETE FROM stage_sets WHERE id = 7", [])
            .expect("a report never blocks the delete");
        let row: (
            Option<i64>,
            String,
            i64,
            String,
            Option<i64>,
            Vec<u8>,
            String,
            String,
        ) = conn
            .query_row(
                "SELECT stage_set_id, unit_name, snapshot_version, outcome, tar_exit_code,
                        tar_stderr, tapectl_version, tar_version
                   FROM tar_create_reports WHERE id = ?1",
                [id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(row.0, None, "the stage set went; the row stayed");
        assert_eq!((row.1.as_str(), row.2), ("photos", 3));
        assert_eq!(row.3, "failed");
        assert_eq!(row.4, Some(2));
        assert_eq!(row.5, stderr, "byte for byte");
        assert_eq!(row.6, crate::build_info::VERSION);
        assert_eq!(row.7, "tar (GNU tar) 1.35");
    }
}
