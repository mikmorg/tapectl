//! What a snapshot left out: `snapshot_left_out` (ADR-0012 items 252 and 270;
//! `volume-format-v3.md` Appendix A.3; #542).
//!
//! One row for each file or folder an exclusion pattern kept out of the
//! snapshot, and for each socket. The path is escaped as the cross-volume
//! index escapes a field, so a name with a tab or a newline cannot break a
//! line of anything that prints it. The rows go with the snapshot's own row
//! (`ON DELETE CASCADE`).

use rusqlite::{params, Connection};

use crate::error::Result;

/// Why a path is not in the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// An exclusion pattern matched it.
    Pattern,
    /// It is a socket: tar skips one at exit 0, so a listed socket would
    /// have no member (item 270).
    Socket,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Pattern => "pattern",
            Reason::Socket => "socket",
        }
    }
}

/// One thing a snapshot left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeftOut {
    pub reason: Reason,
    /// The pattern as written (`*.nfo`, `.cache/`); `None` for a socket.
    pub pattern: Option<String>,
    /// Relative to the unit, unescaped (the table stores it escaped).
    pub path: String,
    /// A folder is left out with what was under it: its own entry stays in
    /// the snapshot, empty.
    pub folder: bool,
    /// What was under a folder; 1 for a file.
    pub files: i64,
    /// Bytes of content under a folder; the file's size for a file.
    pub bytes: i64,
}

impl LeftOut {
    /// `"file"` or `"folder"`, as the table and `--json` say it.
    pub fn kind(&self) -> &'static str {
        if self.folder {
            "folder"
        } else {
            "file"
        }
    }
}

/// Record `rows` as `snapshot_id`'s left-out list, in order, with one
/// prepared statement. Run it inside the caller's transaction. Returns the
/// rows written.
pub fn insert(conn: &Connection, snapshot_id: i64, rows: &[LeftOut]) -> Result<usize> {
    let mut stmt = conn.prepare(
        "INSERT INTO snapshot_left_out
             (snapshot_id, seq, reason, pattern, path, kind, files, bytes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for (seq, row) in rows.iter().enumerate() {
        stmt.execute(params![
            snapshot_id,
            seq as i64,
            row.reason.as_str(),
            row.pattern,
            crate::volume::index::escape(&row.path),
            row.kind(),
            row.files,
            row.bytes,
        ])?;
    }
    Ok(rows.len())
}

/// `snapshot_id`'s left-out list in `seq` order, paths unescaped.
pub fn load(conn: &Connection, snapshot_id: i64) -> Result<Vec<LeftOut>> {
    let mut stmt = conn.prepare(
        "SELECT reason, pattern, path, kind, files, bytes FROM snapshot_left_out
         WHERE snapshot_id = ?1 ORDER BY seq",
    )?;
    let raw: Vec<(String, Option<String>, String, String, i64, i64)> = stmt
        .query_map(params![snapshot_id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<std::result::Result<_, _>>()?;
    raw.into_iter()
        .map(|(reason, pattern, path, kind, files, bytes)| {
            let path = crate::volume::index::unescape(&path).map_err(|e| {
                crate::error::TapectlError::Other(format!(
                    "snapshot {snapshot_id}'s left-out path {path:?} is not escaped as the \
                     index escapes a field: {e}"
                ))
            })?;
            Ok(LeftOut {
                reason: if reason == "socket" {
                    Reason::Socket
                } else {
                    Reason::Pattern
                },
                pattern,
                path,
                folder: kind == "folder",
                files,
                bytes,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_round_trip_with_an_escaped_path_and_in_order() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute("INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, checksum_mode, encrypt, status)
             VALUES ('u', 'u', 1, '/x', 'mtime_size', 1, 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, source_path, total_size, file_count)
             VALUES (1, 1, '/x', 0, 0)",
            [],
        )
        .unwrap();
        let rows = vec![
            LeftOut {
                reason: Reason::Pattern,
                pattern: Some(".cache/".into()),
                path: "a\tb\\c".into(),
                folder: true,
                files: 3,
                bytes: 99,
            },
            LeftOut {
                reason: Reason::Socket,
                pattern: None,
                path: "s.sock".into(),
                folder: false,
                files: 1,
                bytes: 0,
            },
        ];
        assert_eq!(insert(&conn, 1, &rows).unwrap(), 2);
        let stored: String = conn
            .query_row("SELECT path FROM snapshot_left_out WHERE seq = 0", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, "a\\tb\\\\c");
        assert_eq!(load(&conn, 1).unwrap(), rows);
        // A socket with a pattern, or a pattern row without one, is refused.
        assert!(conn
            .execute(
                "INSERT INTO snapshot_left_out VALUES (1, 9, 'socket', 'x', 'p', 'file', 1, 0)",
                []
            )
            .is_err());
    }
}
