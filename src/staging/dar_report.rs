//! dar's report for each `dar -c` a stage set ran, kept verbatim in
//! `dar_create_reports` (migration 034, issue #343) — the staging half of
//! what `restores` (024) keeps for `dar -x`.
//!
//! Best-effort, like every journal: a refused insert is a warning, never a
//! reason for a stage to fail or to report a different failure than the one
//! it had. The busy policy waits out a catalog another stage holds (#377).

use rusqlite::{params, Connection};
use tracing::warn;

use crate::dar::create::DarCreateReport;
use crate::db::busy::{self, BusyPolicy};

/// Append `report` for stage set `stage_set_id` of `unit_name` v`version`.
/// Returns the row id, or `None` when the insert was refused.
pub fn record(
    conn: &Connection,
    stage_set_id: i64,
    unit_name: &str,
    version: i64,
    report: &DarCreateReport,
) -> Option<i64> {
    let inserted = busy::retry(BusyPolicy::DEFAULT, "dar's report", || {
        conn.execute(
            "INSERT INTO dar_create_reports
                 (stage_set_id, unit_name, snapshot_version, outcome, dar_command,
                  dar_exit_code, dar_stderr, dar_version, tapectl_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                stage_set_id,
                unit_name,
                version,
                report.outcome,
                report.command,
                report.exit_code,
                report.stderr,
                report.dar_version,
                crate::build_info::VERSION,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    });
    match inserted {
        Ok(id) => Some(id),
        Err(e) => {
            warn!(err = %e, unit = unit_name, "dar_create_reports insert failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The row keeps dar's bytes exactly — not decoded, not trimmed — and
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
             VALUES (7, 1, 'staging', 1);",
        )
        .unwrap();
        let stderr = b"\xffWARNING: cannot read /src/a: Permission denied\n  \n".to_vec();
        let report = DarCreateReport {
            outcome: crate::dar::create::OUTCOME_FAILED,
            command: "\"dar\" \"-c\" \"-\"".into(),
            exit_code: Some(5),
            stderr: stderr.clone(),
            dar_version: "dar version 2.7.20".into(),
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
        ) = conn
            .query_row(
                "SELECT stage_set_id, unit_name, snapshot_version, outcome, dar_exit_code,
                        dar_stderr, tapectl_version
                   FROM dar_create_reports WHERE id = ?1",
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
                    ))
                },
            )
            .unwrap();
        assert_eq!(row.0, None, "the stage set went; the row stayed");
        assert_eq!((row.1.as_str(), row.2), ("photos", 3));
        assert_eq!(row.3, "failed");
        assert_eq!(row.4, Some(5));
        assert_eq!(row.5, stderr, "byte for byte");
        assert_eq!(row.6, crate::build_info::VERSION);
    }
}
