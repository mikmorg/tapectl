//! The `restores` row — one per restore, written once its contact has
//! opened, on success and on failure alike (migration 024, issue #306;
//! ADR-0013 §§2, 5, 7; ADR-0012 amendment of 2026-09-24, item 2).
//!
//! A restore is the one operation that proves the archive works, and until
//! this table it was the one operation that left no record: the contact row
//! said a cartridge was in a drive, the health row said what the counters
//! read afterwards, and nothing said what the restore DID or what dar made
//! of the archive. This module is the writer and the reader; the migration
//! header is the column-by-column contract.
//!
//! **Bookkeeping never refuses the command.** [`record`] warns and returns
//! `None` when the insert fails, exactly as `cartridge_contacts`' own insert
//! does. The restore's result is decided before this row is written and is
//! not changed by it.

use rusqlite::types::Value;
use rusqlite::{params, Connection};
use tracing::warn;

use crate::dar::restore::DarReport;

/// `restores.kind` for `restore unit`.
pub const KIND_UNIT: &str = "unit";
/// `restores.kind` for `restore file`.
pub const KIND_FILE: &str = "file";
/// `restores.kind` for `restore raw-volume`.
pub const KIND_RAW_VOLUME: &str = "raw-volume";

/// Every kind code writes — free TEXT in the schema (ADR-0013 §4), pinned
/// here and by `the_kind_vocabulary_is_what_code_writes`.
pub const ALL_KINDS: &[&str] = &[KIND_UNIT, KIND_FILE, KIND_RAW_VOLUME];

/// The current UTC time in `datetime('now')`'s spelling — the contact row's
/// clock, so a restore's `started_at` compares with its contact's
/// `opened_at` without conversion.
pub fn now_sqlite() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// One restore, as [`record`] writes it. Field meanings, per kind, are in
/// migration 024's header.
#[derive(Debug)]
pub struct RestoreRecord<'a> {
    /// The contact the restore was made under — `None` when its INSERT
    /// failed, never because no contact happened.
    pub contact_id: Option<i64>,
    pub volume_id: Option<i64>,
    pub volume_label: Option<&'a str>,
    pub unit_id: Option<i64>,
    pub unit_name: Option<&'a str>,
    pub version: Option<i64>,
    /// One of [`ALL_KINDS`].
    pub kind: &'static str,
    /// The one path asked for; `Some` only for [`KIND_FILE`].
    pub file_path: Option<&'a str>,
    /// The directory the operator named — never a scratch or temp dir.
    pub destination: &'a str,
    /// From [`now_sqlite`], taken when the contact opened.
    pub started_at: &'a str,
    /// `tape::contact::OUTCOME_OK` or `OUTCOME_FAILED`.
    pub outcome: &'a str,
    pub error: Option<&'a str>,
    pub slices_read: Option<i64>,
    pub bytes_restored: Option<i64>,
    pub files_restored: Option<i64>,
    /// `None` when dar never ran.
    pub dar: Option<&'a DarReport>,
    pub dar_version: Option<&'a str>,
}

/// dar's stream as the row stores it: TEXT when valid UTF-8, a BLOB of the
/// exact bytes otherwise — never a lossy conversion (022's rule for
/// `mam_journal.raw`).
fn verbatim(bytes: &[u8]) -> Value {
    match std::str::from_utf8(bytes) {
        Ok(s) => Value::Text(s.to_string()),
        Err(_) => Value::Blob(bytes.to_vec()),
    }
}

/// Append the row. `finished_at` is now; `tapectl_version` is this build.
///
/// Best-effort: a failed insert is a `warn!` naming the kind and unit, and
/// `None`. Returns the row id otherwise.
pub fn record(conn: &Connection, rec: &RestoreRecord<'_>) -> Option<i64> {
    let (dar_argv, dar_exit_code, dar_stdout, dar_stderr) = match rec.dar {
        Some(d) => (
            Value::Text(d.argv_json()),
            d.exit_code
                .map_or(Value::Null, |c| Value::Integer(c.into())),
            verbatim(&d.stdout),
            verbatim(&d.stderr),
        ),
        None => (Value::Null, Value::Null, Value::Null, Value::Null),
    };
    let inserted = conn.execute(
        "INSERT INTO restores
             (contact_id, volume_id, volume_label, unit_id, unit_name, version,
              kind, file_path, destination, started_at, finished_at, outcome, error,
              slices_read, bytes_restored, files_restored,
              dar_argv, dar_exit_code, dar_stdout, dar_stderr, dar_version,
              tapectl_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                 ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
        params![
            rec.contact_id,
            rec.volume_id,
            rec.volume_label,
            rec.unit_id,
            rec.unit_name,
            rec.version,
            rec.kind,
            rec.file_path,
            rec.destination,
            rec.started_at,
            now_sqlite(),
            rec.outcome,
            rec.error,
            rec.slices_read,
            rec.bytes_restored,
            rec.files_restored,
            dar_argv,
            dar_exit_code,
            dar_stdout,
            dar_stderr,
            rec.dar_version,
            env!("CARGO_PKG_VERSION"),
        ],
    );
    match inserted {
        Ok(_) => Some(conn.last_insert_rowid()),
        Err(e) => {
            warn!(
                err = %e,
                kind = rec.kind,
                unit = rec.unit_name.unwrap_or("-"),
                "restores insert failed; the restore's outcome is unchanged"
            );
            None
        }
    }
}

/// A `restores` row read back — what a report or a test queries.
///
/// `dar_stdout`/`dar_stderr` are `None` when dar never ran; a stream that
/// was stored as a BLOB (non-UTF-8) is returned lossily here, which is fine
/// for display and is why the column, not this reader, is the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreRow {
    pub id: i64,
    pub contact_id: Option<i64>,
    pub volume_id: Option<i64>,
    pub volume_label: Option<String>,
    pub unit_id: Option<i64>,
    pub unit_name: Option<String>,
    pub version: Option<i64>,
    pub kind: String,
    pub file_path: Option<String>,
    pub destination: String,
    pub started_at: String,
    pub finished_at: String,
    pub outcome: String,
    pub error: Option<String>,
    pub slices_read: Option<i64>,
    pub bytes_restored: Option<i64>,
    pub files_restored: Option<i64>,
    pub dar_argv: Option<String>,
    pub dar_exit_code: Option<i64>,
    pub dar_stdout: Option<String>,
    pub dar_stderr: Option<String>,
    pub dar_version: Option<String>,
    pub tapectl_version: String,
}

/// Every row, oldest first.
pub fn rows(conn: &Connection) -> rusqlite::Result<Vec<RestoreRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, contact_id, volume_id, volume_label, unit_id, unit_name, version,
                kind, file_path, destination, started_at, finished_at, outcome, error,
                slices_read, bytes_restored, files_restored,
                dar_argv, dar_exit_code, dar_stdout, dar_stderr, dar_version,
                tapectl_version
           FROM restores ORDER BY id",
    )?;
    let text = |v: Value| -> Option<String> {
        match v {
            Value::Null => None,
            Value::Text(s) => Some(s),
            Value::Blob(b) => Some(String::from_utf8_lossy(&b).into_owned()),
            other => Some(format!("{other:?}")),
        }
    };
    let rows = stmt
        .query_map([], |r| {
            Ok(RestoreRow {
                id: r.get(0)?,
                contact_id: r.get(1)?,
                volume_id: r.get(2)?,
                volume_label: r.get(3)?,
                unit_id: r.get(4)?,
                unit_name: r.get(5)?,
                version: r.get(6)?,
                kind: r.get(7)?,
                file_path: r.get(8)?,
                destination: r.get(9)?,
                started_at: r.get(10)?,
                finished_at: r.get(11)?,
                outcome: r.get(12)?,
                error: r.get(13)?,
                slices_read: r.get(14)?,
                bytes_restored: r.get(15)?,
                files_restored: r.get(16)?,
                dar_argv: r.get(17)?,
                dar_exit_code: r.get(18)?,
                dar_stdout: text(r.get(19)?),
                dar_stderr: text(r.get(20)?),
                dar_version: r.get(21)?,
                tapectl_version: r.get(22)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape::contact::{OUTCOME_FAILED, OUTCOME_OK};

    fn report(stdout: &[u8], stderr: &[u8]) -> DarReport {
        DarReport {
            argv: vec!["dar".into(), "-x".into(), "/d/restore".into()],
            exit_code: Some(0),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    fn rec<'a>(started_at: &'a str, dar: Option<&'a DarReport>) -> RestoreRecord<'a> {
        RestoreRecord {
            contact_id: None,
            volume_id: None,
            volume_label: Some("L6-0001"),
            unit_id: None,
            unit_name: Some("photos"),
            version: Some(1),
            kind: KIND_UNIT,
            file_path: None,
            destination: "/restore/photos",
            started_at,
            outcome: OUTCOME_OK,
            error: None,
            slices_read: Some(1),
            bytes_restored: Some(4096),
            files_restored: Some(3),
            dar,
            dar_version: Some("2.7.13"),
        }
    }

    // ── migration 024, to the #227/#264 standard ──

    #[test]
    fn migration_024_creates_restores_with_its_columns_and_indexes() {
        let conn = crate::db::open_memory().unwrap();
        let cols: Vec<(String, String, i64, Option<String>)> = {
            let mut stmt = conn.prepare("PRAGMA table_info(restores)").unwrap();
            let v = stmt
                .query_map([], |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                .unwrap()
                .map(|c| c.unwrap())
                .collect();
            v
        };
        let expect = |name: &str, ty: &str, notnull: i64| {
            (name.to_string(), ty.to_string(), notnull, None::<String>)
        };
        assert_eq!(
            cols,
            vec![
                expect("id", "INTEGER", 0),
                expect("contact_id", "INTEGER", 0),
                expect("volume_id", "INTEGER", 0),
                expect("volume_label", "TEXT", 0),
                expect("unit_id", "INTEGER", 0),
                expect("unit_name", "TEXT", 0),
                expect("version", "INTEGER", 0),
                expect("kind", "TEXT", 1),
                expect("file_path", "TEXT", 0),
                expect("destination", "TEXT", 1),
                expect("started_at", "TEXT", 1),
                expect("finished_at", "TEXT", 1),
                expect("outcome", "TEXT", 1),
                expect("error", "TEXT", 0),
                expect("slices_read", "INTEGER", 0),
                expect("bytes_restored", "INTEGER", 0),
                expect("files_restored", "INTEGER", 0),
                expect("dar_argv", "TEXT", 0),
                expect("dar_exit_code", "INTEGER", 0),
                expect("dar_stdout", "TEXT", 0),
                expect("dar_stderr", "TEXT", 0),
                expect("dar_version", "TEXT", 0),
                expect("tapectl_version", "TEXT", 1),
            ],
            "no drive column (ADR-0013 §1): the drive is reached through contact_id"
        );

        let fks: Vec<(String, String, String)> = {
            let mut stmt = conn.prepare("PRAGMA foreign_key_list(restores)").unwrap();
            let mut v: Vec<(String, String, String)> = stmt
                .query_map([], |r| Ok((r.get(3)?, r.get(2)?, r.get(4)?)))
                .unwrap()
                .map(|c| c.unwrap())
                .collect();
            v.sort();
            v
        };
        assert_eq!(
            fks,
            vec![
                (
                    "contact_id".into(),
                    "cartridge_contacts".into(),
                    "id".into()
                ),
                ("unit_id".into(), "units".into(), "id".into()),
                ("volume_id".into(), "volumes".into(), "id".into()),
            ]
        );

        let indexes: Vec<(String, Vec<String>)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'index' \
                     AND tbl_name = 'restores' AND name NOT LIKE 'sqlite_%' ORDER BY name",
                )
                .unwrap();
            let names: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|n| n.unwrap())
                .collect();
            names
                .into_iter()
                .map(|n| {
                    let mut s = conn
                        .prepare(&format!("PRAGMA index_info(\"{n}\")"))
                        .unwrap();
                    let cols = s
                        .query_map([], |r| r.get::<_, String>(2))
                        .unwrap()
                        .map(|c| c.unwrap())
                        .collect();
                    (n, cols)
                })
                .collect()
        };
        assert_eq!(
            indexes,
            vec![
                ("idx_restores_contact".into(), vec!["contact_id".into()]),
                (
                    "idx_restores_unit".into(),
                    vec!["unit_name".into(), "started_at".into()]
                ),
                ("idx_restores_volume".into(), vec!["volume_id".into()]),
            ]
        );
    }

    /// The kind vocabulary is free TEXT in the schema (ADR-0013 §4) and
    /// pinned here instead: a spelling code writes must be one of these.
    #[test]
    fn the_kind_vocabulary_is_what_code_writes() {
        assert_eq!(ALL_KINDS, &["unit", "file", "raw-volume"]);
        const SRC: &str = include_str!("restore.rs");
        for k in ["KIND_UNIT", "KIND_FILE", "KIND_RAW_VOLUME"] {
            assert!(SRC.contains(k), "restore.rs writes no row of kind {k}");
        }
    }

    // ── the row ──

    #[test]
    fn record_writes_every_field_and_the_report_verbatim() {
        let conn = crate::db::open_memory().unwrap();
        let started = now_sqlite();
        let rep = report(b" 3 inode(s) restored\n", b"a warning\n");
        let id = record(&conn, &rec(&started, Some(&rep))).expect("row written");
        let rows = rows(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.id, id);
        assert_eq!(r.kind, "unit");
        assert_eq!(r.volume_label.as_deref(), Some("L6-0001"));
        assert_eq!(r.unit_name.as_deref(), Some("photos"));
        assert_eq!(r.version, Some(1));
        assert_eq!(r.destination, "/restore/photos");
        assert_eq!(r.started_at, started);
        assert!(r.finished_at >= r.started_at);
        assert_eq!(r.outcome, "ok");
        assert_eq!(r.error, None);
        assert_eq!(r.slices_read, Some(1));
        assert_eq!(r.bytes_restored, Some(4096));
        assert_eq!(r.files_restored, Some(3));
        assert_eq!(r.dar_argv.as_deref(), Some(r#"["dar","-x","/d/restore"]"#));
        assert_eq!(r.dar_exit_code, Some(0));
        assert_eq!(r.dar_stdout.as_deref(), Some(" 3 inode(s) restored\n"));
        assert_eq!(r.dar_stderr.as_deref(), Some("a warning\n"));
        assert_eq!(r.dar_version.as_deref(), Some("2.7.13"));
        assert_eq!(r.tapectl_version, env!("CARGO_PKG_VERSION"));
    }

    /// NULL means dar never ran; "" means it ran and said nothing. The two
    /// are different facts and the row keeps them apart.
    #[test]
    fn no_dar_is_null_and_a_silent_dar_is_empty() {
        let conn = crate::db::open_memory().unwrap();
        let started = now_sqlite();
        let mut failed = rec(&started, None);
        failed.outcome = OUTCOME_FAILED;
        failed.error = Some("no secret keys found");
        failed.slices_read = None;
        failed.bytes_restored = None;
        failed.files_restored = None;
        failed.dar_version = None;
        record(&conn, &failed).unwrap();
        let silent = report(b"", b"");
        record(&conn, &rec(&started, Some(&silent))).unwrap();

        let rows = rows(&conn).unwrap();
        assert_eq!(rows[0].outcome, "failed");
        assert_eq!(rows[0].error.as_deref(), Some("no secret keys found"));
        assert_eq!(rows[0].dar_stdout, None, "dar never ran: NULL");
        assert_eq!(rows[0].dar_stderr, None);
        assert_eq!(rows[0].dar_argv, None);
        assert_eq!(rows[0].dar_exit_code, None);
        assert_eq!(
            rows[1].dar_stdout.as_deref(),
            Some(""),
            "ran, said nothing: empty"
        );
        assert_eq!(rows[1].dar_stderr.as_deref(), Some(""));
    }

    /// A non-UTF-8 file name in dar's output is stored as the exact bytes,
    /// not rewritten (022's rule for `mam_journal.raw`).
    #[test]
    fn a_non_utf8_stream_is_kept_as_its_exact_bytes() {
        let conn = crate::db::open_memory().unwrap();
        let started = now_sqlite();
        let bytes = b"/dest/caf\xe9.txt not restored (user choice)\n";
        let rep = report(bytes, b"");
        record(&conn, &rec(&started, Some(&rep))).unwrap();
        let raw: Value = conn
            .query_row("SELECT dar_stdout FROM restores", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, Value::Blob(bytes.to_vec()));
        // The UTF-8 sibling is TEXT, so the type is chosen per stream.
        let stderr: Value = conn
            .query_row("SELECT dar_stderr FROM restores", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stderr, Value::Text(String::new()));
    }

    /// Bookkeeping never refuses the command: with the table missing the
    /// insert fails, `record` returns `None`, and nothing panics.
    #[test]
    fn a_failed_insert_is_none_not_a_panic() {
        let conn = Connection::open_in_memory().unwrap();
        let started = now_sqlite();
        assert_eq!(record(&conn, &rec(&started, None)), None);
    }

    #[test]
    fn now_sqlite_is_datetime_now_spelling() {
        let s = now_sqlite();
        assert_eq!(s.len(), 19, "{s}");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], " ");
        let db: String = crate::db::open_memory()
            .unwrap()
            .query_row("SELECT datetime('now')", [], |r| r.get(0))
            .unwrap();
        assert_eq!(db.len(), s.len());
    }
}
