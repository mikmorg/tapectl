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
    /// Relative to the unit, in the escaped form of §1.5 (ADR-0012 item
    /// 297), which the table stores as it is.
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

    /// The one shape `--json` gives a left-out row (#542): `snapshot
    /// create`'s, and `stage create`'s and `collection status`'s with it.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "reason": self.reason.as_str(),
            "pattern": self.pattern,
            "path": self.path,
            "kind": self.kind(),
            "files": self.files,
            "bytes": self.bytes,
        })
    }
}

/// What the newest snapshot of one unit left out (#542's third criterion:
/// `collection status` shows it for a unit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitLeftOut {
    pub unit_id: i64,
    /// The newest snapshot's version.
    pub version: i64,
    /// Its rows, in `seq` order; never empty.
    pub rows: Vec<LeftOut>,
}

impl UnitLeftOut {
    /// Files left out: a file row counts 1, a folder row what was under it.
    pub fn files(&self) -> i64 {
        self.rows.iter().map(|r| r.files).sum()
    }

    /// Bytes of content left out.
    pub fn bytes(&self) -> i64 {
        self.rows.iter().map(|r| r.bytes).sum()
    }
}

/// For each of `unit_ids`, what its newest snapshot (the highest version,
/// as the pending scan reads it: `content_match::latest_snapshot`) left
/// out, in `unit_ids`' order. A unit with no snapshot, or whose newest left
/// nothing out, has no entry. One statement for every unit, whatever their
/// number: each unit's newest version by `UNIQUE(unit_id, version)`, its
/// rows by the primary key.
pub fn newest_for_units(conn: &Connection, unit_ids: &[i64]) -> Result<Vec<UnitLeftOut>> {
    if unit_ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids = serde_json::to_string(unit_ids).expect("ids serialize");
    let mut stmt = conn.prepare(
        "SELECT s.unit_id, s.version, s.id,
                l.reason, l.pattern, l.path, l.kind, l.files, l.bytes
         FROM json_each(?1) j
         JOIN snapshots s ON s.unit_id = j.value
              AND s.version = (SELECT MAX(version) FROM snapshots WHERE unit_id = j.value)
         JOIN snapshot_left_out l ON l.snapshot_id = s.id
         ORDER BY j.key, l.seq",
    )?;
    let raw: Vec<(i64, i64, i64, Raw)> = stmt
        .query_map(params![ids], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                (
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ),
            ))
        })?
        .collect::<std::result::Result<_, _>>()?;
    let mut out: Vec<UnitLeftOut> = Vec::new();
    for (unit_id, version, snapshot_id, row) in raw {
        let row = from_raw(snapshot_id, row)?;
        match out.last_mut() {
            Some(last) if last.unit_id == unit_id => last.rows.push(row),
            _ => out.push(UnitLeftOut {
                unit_id,
                version,
                rows: vec![row],
            }),
        }
    }
    Ok(out)
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
            row.path,
            row.kind(),
            row.files,
            row.bytes,
        ])?;
    }
    Ok(rows.len())
}

/// `snapshot_id`'s left-out list in `seq` order, paths as stored (escaped).
pub fn load(conn: &Connection, snapshot_id: i64) -> Result<Vec<LeftOut>> {
    let mut stmt = conn.prepare(
        "SELECT reason, pattern, path, kind, files, bytes FROM snapshot_left_out
         WHERE snapshot_id = ?1 ORDER BY seq",
    )?;
    let raw: Vec<Raw> = stmt
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
        .map(|row| from_raw(snapshot_id, row))
        .collect()
}

/// One stored row: reason, pattern, path, kind, files, bytes.
type Raw = (String, Option<String>, String, String, i64, i64);

/// A stored row as a [`LeftOut`], its path checked to be escaped as the
/// index escapes a field.
fn from_raw(snapshot_id: i64, (reason, pattern, path, kind, files, bytes): Raw) -> Result<LeftOut> {
    crate::volume::index::unescape(&path).map_err(|e| {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_round_trip_with_an_escaped_path_and_in_order() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
            [],
        )
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
                path: "a\\tb\\\\c\\xe9".into(),
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
            .query_row(
                "SELECT path FROM snapshot_left_out WHERE seq = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, "a\\tb\\\\c\\xe9", "stored as it is held: escaped");
        assert_eq!(load(&conn, 1).unwrap(), rows);
        // A socket with a pattern, or a pattern row without one, is refused.
        assert!(conn
            .execute(
                "INSERT INTO snapshot_left_out VALUES (1, 9, 'socket', 'x', 'p', 'file', 1, 0)",
                []
            )
            .is_err());
    }

    fn row(path: &str, files: i64, bytes: i64) -> LeftOut {
        LeftOut {
            reason: Reason::Pattern,
            pattern: Some("*.tmp".into()),
            path: path.into(),
            folder: files > 1,
            files,
            bytes,
        }
    }

    /// #542: the one `--json` shape of a left-out row, every key and value,
    /// so `stage create` and `collection status` carry `snapshot create`'s.
    #[test]
    fn a_rows_json_is_snapshot_creates_shape() {
        assert_eq!(
            row("a.tmp", 1, 5).to_json(),
            serde_json::json!({
                "reason": "pattern", "pattern": "*.tmp", "path": "a.tmp",
                "kind": "file", "files": 1, "bytes": 5,
            })
        );
        let socket = LeftOut {
            reason: Reason::Socket,
            pattern: None,
            path: "s.sock".into(),
            folder: false,
            files: 1,
            bytes: 0,
        };
        assert_eq!(
            socket.to_json(),
            serde_json::json!({
                "reason": "socket", "pattern": null, "path": "s.sock",
                "kind": "file", "files": 1, "bytes": 0,
            })
        );
    }

    /// #542's third criterion: each unit's NEWEST snapshot's rows, in the
    /// order the units are given; a unit whose newest left nothing out, or
    /// with no snapshot, has no entry.
    #[test]
    fn newest_for_units_reads_each_units_newest_version_only() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
            [],
        )
        .unwrap();
        for u in ["a", "b", "c", "d"] {
            conn.execute(
                "INSERT INTO units (uuid, name, tenant_id, current_path, checksum_mode, encrypt, status)
                 VALUES (?1, ?1, 1, '/x', 'mtime_size', 1, 'active')",
                params![u],
            )
            .unwrap();
        }
        // a: v1 left out old.tmp, v2 new.tmp and a folder. b: v1 left out
        // b.tmp, v2 nothing. c: v1 left out c.tmp. d: no snapshot.
        let snap = |unit: i64, version: i64| -> i64 {
            conn.execute(
                "INSERT INTO snapshots (unit_id, version, source_path, total_size, file_count)
                 VALUES (?1, ?2, '/x', 0, 0)",
                params![unit, version],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        let a1 = snap(1, 1);
        let a2 = snap(1, 2);
        let b1 = snap(2, 1);
        snap(2, 2);
        let c1 = snap(3, 1);
        insert(&conn, a1, &[row("old.tmp", 1, 1)]).unwrap();
        insert(&conn, a2, &[row("new.tmp", 1, 7), row("cache", 3, 30)]).unwrap();
        insert(&conn, b1, &[row("b.tmp", 1, 2)]).unwrap();
        insert(&conn, c1, &[row("c.tmp", 1, 4)]).unwrap();

        let got = newest_for_units(&conn, &[3, 4, 2, 1]).unwrap();
        assert_eq!(
            got,
            vec![
                UnitLeftOut {
                    unit_id: 3,
                    version: 1,
                    rows: vec![row("c.tmp", 1, 4)],
                },
                UnitLeftOut {
                    unit_id: 1,
                    version: 2,
                    rows: vec![row("new.tmp", 1, 7), row("cache", 3, 30)],
                },
            ]
        );
        assert_eq!((got[1].files(), got[1].bytes()), (4, 37));
        assert!(newest_for_units(&conn, &[]).unwrap().is_empty());
    }
}
