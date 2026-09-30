//! `phase_timings` (migration 028, issue #386): each phase of a long
//! operation, its duration and its bytes, grouped by session.
//!
//! Written from [`crate::progress::drain`] at the end of an operation, never
//! during one: the rows are bookkeeping, and a failure to write them is a
//! warning, never the operation's failure.

use rusqlite::{params, Connection};

use crate::db::busy::{self, BusyPolicy};
use crate::error::Result;
use crate::progress::PhaseTiming;

/// The subject a session's phases are recorded against.
#[derive(Clone, Copy, Debug)]
pub enum Subject {
    StageSet(i64),
    Volume(i64),
}

/// One recorded phase, as `volume info` and the stage report read it back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhaseRow {
    pub session: String,
    pub operation: String,
    pub phase: String,
    pub started_at: String,
    pub duration_ms: i64,
    pub bytes: Option<i64>,
    pub outcome: String,
}

impl PhaseRow {
    /// The same one-line summary the live display prints.
    pub fn summary(&self) -> String {
        PhaseTiming {
            phase: self.phase.clone(),
            started_at: chrono::Utc::now(),
            duration: std::time::Duration::from_millis(self.duration_ms.max(0) as u64),
            bytes: self.bytes.map(|b| b.max(0) as u64),
            ok: self.outcome == "ok",
        }
        .summary()
    }
}

/// Record `phases` for `subject` under the current session, in one
/// transaction. Nothing to record (no session, no phases) is a no-op.
pub fn record(
    conn: &Connection,
    operation: &str,
    subject: Subject,
    phases: &[PhaseTiming],
) -> Result<()> {
    let Some(session) = crate::progress::session_id() else {
        return Ok(());
    };
    record_for_session(conn, &session, operation, subject, phases)
}

/// [`record`] with the session id given — the testable half.
pub fn record_for_session(
    conn: &Connection,
    session: &str,
    operation: &str,
    subject: Subject,
    phases: &[PhaseTiming],
) -> Result<()> {
    if phases.is_empty() {
        return Ok(());
    }
    let (volume_id, stage_set_id) = match subject {
        Subject::StageSet(id) => (None, Some(id)),
        Subject::Volume(id) => (Some(id), None),
    };
    busy::retry(BusyPolicy::DEFAULT, "the phase timings", || {
        let tx = busy::immediate_tx(conn)?;
        let base: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM phase_timings WHERE session = ?1",
            params![session],
            |r| r.get(0),
        )?;
        for (i, p) in phases.iter().enumerate() {
            tx.execute(
                "INSERT INTO phase_timings
                     (session, operation, volume_id, stage_set_id, seq, phase,
                      started_at, duration_ms, bytes, outcome)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    session,
                    operation,
                    volume_id,
                    stage_set_id,
                    base + i as i64 + 1,
                    p.phase,
                    p.started_at_sql(),
                    p.duration.as_millis().min(i64::MAX as u128) as i64,
                    p.bytes.map(|b| b.min(i64::MAX as u64) as i64),
                    p.outcome(),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
}

/// Drain this thread's ended phases and record them for `subject`, warning
/// (never failing) when the catalog will not take them. The operation's own
/// result is what the caller returns.
pub fn record_drained(conn: &Connection, operation: &str, subject: Subject) {
    let phases = crate::progress::drain();
    if let Err(e) = record(conn, operation, subject, &phases) {
        tracing::warn!(error = %e, operation, "could not record the phase timings");
    }
}

/// The phases of the most recent session recorded against `volume_id`, in
/// order. Empty when none was recorded.
pub fn latest_for_volume(conn: &Connection, volume_id: i64) -> Result<Vec<PhaseRow>> {
    latest(conn, "volume_id", volume_id)
}

/// The phases of the most recent session recorded against `stage_set_id`.
pub fn latest_for_stage_set(conn: &Connection, stage_set_id: i64) -> Result<Vec<PhaseRow>> {
    latest(conn, "stage_set_id", stage_set_id)
}

fn latest(conn: &Connection, column: &str, id: i64) -> Result<Vec<PhaseRow>> {
    let sql = format!(
        "SELECT session, operation, phase, started_at, duration_ms, bytes, outcome
         FROM phase_timings
         WHERE {column} = ?1
           AND session = (SELECT session FROM phase_timings WHERE {column} = ?1
                          ORDER BY id DESC LIMIT 1)
         ORDER BY seq"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![id], |r| {
        Ok(PhaseRow {
            session: r.get(0)?,
            operation: r.get(1)?,
            phase: r.get(2)?,
            started_at: r.get(3)?,
            duration_ms: r.get(4)?,
            bytes: r.get(5)?,
            outcome: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// `"Phase timings (volume write, session S):"` and one indented line per
/// phase — shared by `volume info` and the stage report. Empty input renders
/// nothing.
pub fn render(rows: &[PhaseRow]) -> String {
    let Some(first) = rows.first() else {
        return String::new();
    };
    let mut out = format!(
        "Phase timings ({}, session {}):\n",
        first.operation, first.session
    );
    for r in rows {
        out.push_str(&format!("    {}  {}\n", r.started_at, r.summary()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn timing(name: &str, ms: u64, bytes: Option<u64>, ok: bool) -> PhaseTiming {
        PhaseTiming {
            phase: name.into(),
            started_at: chrono::Utc::now(),
            duration: Duration::from_millis(ms),
            bytes,
            ok,
        }
    }

    fn volume(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO volumes (label, uuid, backend_type, backend_name, status, capacity_bytes)
             VALUES ('V1', 'u-1', 'lto', 'lto6', 'initialized', 100)",
            [],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn latest_returns_only_the_newest_session_in_order() {
        let tmp = tempfile::TempDir::new().unwrap();
        let conn = crate::db::open(&tmp.path().join("t.db")).unwrap();
        let v = volume(&conn);
        record_for_session(
            &conn,
            "old",
            "volume write",
            Subject::Volume(v),
            &[timing("write", 5, Some(1), true)],
        )
        .unwrap();
        record_for_session(
            &conn,
            "new",
            "volume resume",
            Subject::Volume(v),
            &[
                timing("write", 2000, Some(4096), true),
                timing("confirm", 10, Some(4096), false),
            ],
        )
        .unwrap();
        let rows = latest_for_volume(&conn, v).unwrap();
        let names: Vec<&str> = rows.iter().map(|r| r.phase.as_str()).collect();
        assert_eq!(names, ["write", "confirm"]);
        assert!(rows.iter().all(|r| r.session == "new"));
        assert_eq!(rows[1].outcome, "failed");
        let text = render(&rows);
        assert!(
            text.starts_with("Phase timings (volume resume, session new):\n"),
            "{text}"
        );
        assert!(text.contains("4.0 KiB"), "{text}");
        assert!(text.contains("(failed)"), "{text}");
    }

    /// Migration 028's promise: a snapshot delete (which deletes its stage
    /// sets) is never refused over a timing row — the row keeps its
    /// measurement and lets go of the subject.
    #[test]
    fn deleting_a_stage_set_keeps_its_timings_with_the_subject_nulled() {
        let tmp = tempfile::TempDir::new().unwrap();
        let conn = crate::db::open(&tmp.path().join("t.db")).unwrap();
        conn.execute_batch(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active');
             INSERT INTO units (uuid, name, tenant_id, current_path, status)
                 VALUES ('u1', 'photos', 1, '/tmp/photos', 'active');
             INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
                 VALUES (1, 1, 'staged', '/tmp/photos', 1, 32);
             INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (1, 'staged', 1);",
        )
        .unwrap();
        let ss: i64 = conn
            .query_row("SELECT id FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        record_for_session(
            &conn,
            "s",
            "stage create",
            Subject::StageSet(ss),
            &[timing("dar", 1500, Some(9), true)],
        )
        .unwrap();
        assert_eq!(latest_for_stage_set(&conn, ss).unwrap().len(), 1);
        conn.execute("DELETE FROM stage_sets WHERE id = ?1", params![ss])
            .expect("a timing row never blocks the delete");
        let (subject, ms): (Option<i64>, i64) = conn
            .query_row(
                "SELECT stage_set_id, duration_ms FROM phase_timings",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((subject, ms), (None, 1500));
    }

    #[test]
    fn nothing_recorded_renders_nothing() {
        assert_eq!(render(&[]), "");
    }
}
