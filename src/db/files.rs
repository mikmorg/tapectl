//! Per-file data: `paths` and `file_versions` (migration 030; issue #380
//! option A, issue #381; ADR-0012 amendment 2026-10-06, item 7).
//!
//! A path is stored once per unit (`paths`, searched through `paths_fts`),
//! and each version holds a narrow membership row per path
//! (`file_versions`, keyed `(snapshot_id, path_id)`): its kind, size, mtime,
//! sha256 as 32 raw bytes and, for a symlink, its target. The migration's
//! header explains the shape; this module is the one place that writes it and
//! that converts between it and the text the rest of tapectl speaks (hex
//! hashes, RFC 3339 mtimes, the walk's `file_type` names).
//!
//! dar's catalogue is the authority for permissions, owners, xattrs, ACLs,
//! device numbers and hard links; none of them enter this database.

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{params, Connection};

use crate::error::{Result, TapectlError};

/// The join every per-file query reads through: a version's membership rows
/// with their paths. `fv` and `p` are the aliases the callers use.
pub const VERSION_FILES: &str = "file_versions fv JOIN paths p ON p.id = fv.path_id";

/// What a walked entry is (`file_versions.kind`). The numbers are the
/// column's CHECK; the names are the walk's and the old `files.file_type`'s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    Dir,
    Regular,
    Symlink,
    /// FIFO, socket, block or character device.
    Special,
}

impl FileKind {
    /// The stored value.
    pub fn code(self) -> i64 {
        match self {
            FileKind::Dir => 0,
            FileKind::Regular => 1,
            FileKind::Symlink => 2,
            FileKind::Special => 3,
        }
    }

    pub fn from_code(code: i64) -> Option<Self> {
        match code {
            0 => Some(FileKind::Dir),
            1 => Some(FileKind::Regular),
            2 => Some(FileKind::Symlink),
            3 => Some(FileKind::Special),
            _ => None,
        }
    }

    /// The walk's name for it: `dir`, `regular`, `symlink`, `special`.
    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::Dir => "dir",
            FileKind::Regular => "regular",
            FileKind::Symlink => "symlink",
            FileKind::Special => "special",
        }
    }

    /// The inverse of [`FileKind::as_str`].
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "dir" => Some(FileKind::Dir),
            "regular" => Some(FileKind::Regular),
            "symlink" => Some(FileKind::Symlink),
            "special" => Some(FileKind::Special),
            _ => None,
        }
    }

    pub fn is_dir(self) -> bool {
        self == FileKind::Dir
    }

    /// What the on-tape `catalog.db` records (`is_directory` only, in both of
    /// its generations) can say: a directory or not. Not-a-directory is read
    /// as `Regular` (#381): the on-tape shape carries no other type until its
    /// 1.2.0 change (ADR-0012 item 7).
    pub fn from_is_directory(is_directory: bool) -> Self {
        if is_directory {
            FileKind::Dir
        } else {
            FileKind::Regular
        }
    }
}

impl ToSql for FileKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.code()))
    }
}

impl FromSql for FileKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let code = value.as_i64()?;
        FileKind::from_code(code).ok_or(FromSqlError::OutOfRange(code))
    }
}

/// One entry of a version, as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub kind: FileKind,
    pub size_bytes: i64,
    /// Unix time in nanoseconds, UTC.
    pub mtime_ns: Option<i64>,
    pub sha256: Option<[u8; 32]>,
    pub link_target: Option<String>,
}

/// 64 lowercase hex characters -> 32 bytes. Anything else is refused: a hash
/// that could not come back as the same text is not stored.
pub fn sha256_from_hex(hex: &str) -> Result<[u8; 32]> {
    let bad = || {
        TapectlError::Other(format!(
            "not a sha256 (64 lowercase hex characters): {hex:?}"
        ))
    };
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return Err(bad());
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (i, pair) in bytes.chunks_exact(2).enumerate() {
        out[i] = (nibble(pair[0]).ok_or_else(bad)? << 4) | nibble(pair[1]).ok_or_else(bad)?;
    }
    Ok(out)
}

/// 32 bytes -> 64 lowercase hex characters, the spelling every hash in
/// tapectl's output and on tape uses.
pub fn sha256_to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// A `file_versions.sha256` column read back as hex. The column's CHECK
/// admits only NULL or 32 bytes.
pub fn sha256_column(value: Option<Vec<u8>>) -> Option<String> {
    value.map(|b| sha256_to_hex(&b))
}

/// Whole seconds since the epoch -> `mtime_ns`. The walks record whole
/// seconds, as they always have, so a version recorded before migration 030
/// and a fresh walk compare equal.
pub fn mtime_ns_from_secs(secs: i64) -> Option<i64> {
    secs.checked_mul(1_000_000_000)
}

/// `mtime_ns` -> the RFC 3339 text tapectl has always shown and the on-tape
/// `catalog.db` carries (`2026-09-01T12:00:00+00:00` for a whole second).
pub fn mtime_ns_to_rfc3339(ns: i64) -> Option<String> {
    let secs = ns.div_euclid(1_000_000_000);
    let nanos = ns.rem_euclid(1_000_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, nanos).map(|dt| dt.to_rfc3339())
}

/// The inverse of [`mtime_ns_to_rfc3339`], for text read off a tape. Refused
/// unless it converts back to exactly the same text: the on-tape value is
/// what the walk wrote, and a value that would come back different is not
/// one tapectl wrote.
pub fn mtime_ns_from_rfc3339(text: &str) -> Result<i64> {
    let bad = || {
        TapectlError::Other(format!(
            "not a modified_at tapectl writes (YYYY-MM-DDTHH:MM:SS+00:00): {text:?}"
        ))
    };
    let dt = chrono::DateTime::parse_from_rfc3339(text).map_err(|_| bad())?;
    let ns = dt.timestamp_nanos_opt().ok_or_else(bad)?;
    if mtime_ns_to_rfc3339(ns).as_deref() != Some(text) {
        return Err(bad());
    }
    Ok(ns)
}

/// The unit a version belongs to.
fn unit_of(conn: &Connection, snapshot_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT unit_id FROM snapshots WHERE id = ?1",
        params![snapshot_id],
        |r| r.get(0),
    )?)
}

/// Write `entries` as `snapshot_id`'s file list, interning each path under
/// the version's unit. A (version, path) already present is left as it is
/// and not counted — `catalog rebuild` may run twice over one tape. Returns
/// the rows written. Run it inside the caller's transaction.
///
/// Issue #413: the rows are gathered in a TEMP table first, then written by
/// one `INSERT … SELECT` into `paths` and one into `file_versions`. Row by
/// row, every statement opened a savepoint and FTS5 flushed its pending
/// index at each one, so every path became its own index segment plus
/// merges: 306 s for 184k rows on a fresh catalog. One statement flushes the
/// search index once. The same path twice in `entries` is refused by
/// `paths`' UNIQUE constraint, as it always was by `files`'.
pub fn insert_version(
    conn: &Connection,
    snapshot_id: i64,
    entries: impl IntoIterator<Item = Result<FileEntry>>,
) -> Result<usize> {
    let unit_id = unit_of(conn, snapshot_id)?;
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS version_files_in (
             seq         INTEGER PRIMARY KEY,
             path        TEXT NOT NULL,
             kind        INTEGER NOT NULL,
             size_bytes  INTEGER NOT NULL,
             mtime_ns    INTEGER,
             sha256      BLOB,
             link_target TEXT
         );
         DELETE FROM temp.version_files_in;",
    )?;
    let written = (|| -> Result<usize> {
        {
            let mut gather = conn.prepare(
                "INSERT INTO temp.version_files_in
                     (path, kind, size_bytes, mtime_ns, sha256, link_target)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for entry in entries {
                let e = entry?;
                gather.execute(params![
                    e.path,
                    e.kind,
                    e.size_bytes,
                    e.mtime_ns,
                    e.sha256.as_ref().map(|h| h.as_slice()),
                    e.link_target,
                ])?;
            }
        }
        conn.execute(
            "INSERT INTO paths (unit_id, path)
             SELECT ?1, t.path FROM temp.version_files_in t
              WHERE NOT EXISTS (SELECT 1 FROM paths p WHERE p.unit_id = ?1 AND p.path = t.path)
              ORDER BY t.seq",
            params![unit_id],
        )?;
        Ok(conn.execute(
            "INSERT INTO file_versions (snapshot_id, path_id, kind, size_bytes, mtime_ns,
                                        sha256, link_target)
             SELECT ?1, p.id, t.kind, t.size_bytes, t.mtime_ns, t.sha256, t.link_target
               FROM temp.version_files_in t
               JOIN paths p ON p.unit_id = ?2 AND p.path = t.path
              WHERE NOT EXISTS (SELECT 1 FROM file_versions fv
                                 WHERE fv.snapshot_id = ?1 AND fv.path_id = p.id)
              ORDER BY t.seq",
            params![snapshot_id, unit_id],
        )?)
    })();
    // Emptied on every outcome: the TEMP table lives as long as the
    // connection, and a failed batch must not leak into the next one.
    // The batch's own error, if any, is the one reported.
    let cleared = conn.execute("DELETE FROM temp.version_files_in", []);
    let written = written?;
    cleared?;
    Ok(written)
}

/// Record `hex` as `path`'s sha256 in `snapshot_id`, unless it already has
/// one (the first staging establishes the baseline; issue #32/H6). Keyed by
/// the path's id under the version's unit, so it is one lookup in `paths`'
/// UNIQUE index and one in `file_versions`' primary key.
pub const BACKFILL_SQL: &str = "UPDATE file_versions SET sha256 = ?1
     WHERE snapshot_id = ?2
       AND path_id = (SELECT id FROM paths WHERE unit_id = ?3 AND path = ?4)
       AND sha256 IS NULL";

/// [`BACKFILL_SQL`] for every `(path, hex)` of one version.
pub fn backfill_sha256(
    conn: &Connection,
    snapshot_id: i64,
    checksums: &[(String, String)],
) -> Result<()> {
    let unit_id = unit_of(conn, snapshot_id)?;
    let mut update = conn.prepare(BACKFILL_SQL)?;
    for (path, hex) in checksums {
        let hash = sha256_from_hex(hex)?;
        update.execute(params![hash.as_slice(), snapshot_id, unit_id, path])?;
    }
    Ok(())
}

/// Remove `snapshot_id`'s file list. Its paths stay interned (see migration
/// 030's header). Returns the rows removed.
pub fn delete_version(conn: &Connection, snapshot_id: i64) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM file_versions WHERE snapshot_id = ?1",
        params![snapshot_id],
    )?)
}

/// Test fixtures and assertions over a version's file list, in the old
/// `files` vocabulary (a `file_type` name, a hex sha256), keyed by
/// `(snapshot_id, path)`. Public so the integration tests can use them too;
/// production code goes through the functions above. Every helper panics on
/// a database error: these are for tests only.
#[doc(hidden)]
pub mod fixture {
    use super::*;

    /// Add one row to `snapshot_id`'s file list.
    pub fn insert(
        conn: &Connection,
        snapshot_id: i64,
        path: &str,
        size_bytes: i64,
        file_type: &str,
        sha256: Option<&str>,
    ) {
        insert_entry(
            conn,
            snapshot_id,
            FileEntry {
                path: path.to_string(),
                kind: FileKind::from_name(file_type)
                    .unwrap_or_else(|| panic!("unknown file_type {file_type:?}")),
                size_bytes,
                mtime_ns: None,
                sha256: sha256.map(|h| sha256_from_hex(h).expect("a 64-hex fixture sha256")),
                link_target: None,
            },
        );
    }

    pub fn insert_entry(conn: &Connection, snapshot_id: i64, entry: FileEntry) {
        insert_version(conn, snapshot_id, [Ok(entry)]).expect("fixture file row");
    }

    /// The rows `snapshot_id` holds, or only the one for `path`.
    pub fn count(conn: &Connection, snapshot_id: i64, path: Option<&str>) -> i64 {
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM {VERSION_FILES}
                 WHERE fv.snapshot_id = ?1 AND (?2 IS NULL OR p.path = ?2)"
            ),
            params![snapshot_id, path],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// `path`'s sha256 in `snapshot_id`, as hex. Panics if there is no row.
    pub fn sha256(conn: &Connection, snapshot_id: i64, path: &str) -> Option<String> {
        conn.query_row(
            &format!(
                "SELECT fv.sha256 FROM {VERSION_FILES}
                 WHERE fv.snapshot_id = ?1 AND p.path = ?2"
            ),
            params![snapshot_id, path],
            |r| r.get::<_, Option<Vec<u8>>>(0),
        )
        .map(sha256_column)
        .unwrap()
    }

    /// `path`'s recorded size in `snapshot_id`. Panics if there is no row.
    pub fn size(conn: &Connection, snapshot_id: i64, path: &str) -> i64 {
        conn.query_row(
            &format!(
                "SELECT fv.size_bytes FROM {VERSION_FILES}
                 WHERE fv.snapshot_id = ?1 AND p.path = ?2"
            ),
            params![snapshot_id, path],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// Overwrite (or clear) `path`'s sha256 in `snapshot_id`, whatever it was.
    pub fn set_sha256(conn: &Connection, snapshot_id: i64, path: &str, hex: Option<&str>) {
        let hash = hex.map(|h| sha256_from_hex(h).expect("a 64-hex fixture sha256"));
        let n = conn
            .execute(
                "UPDATE file_versions SET sha256 = ?1
                 WHERE snapshot_id = ?2
                   AND path_id = (SELECT id FROM paths
                                   WHERE unit_id = (SELECT unit_id FROM snapshots WHERE id = ?2)
                                     AND path = ?3)",
                params![hash.as_ref().map(|h| h.as_slice()), snapshot_id, path],
            )
            .unwrap();
        assert_eq!(n, 1, "no row for {path:?} in snapshot {snapshot_id}");
    }

    /// Remove `path` from `snapshot_id`'s file list.
    pub fn delete(conn: &Connection, snapshot_id: i64, path: &str) {
        let n = conn
            .execute(
                "DELETE FROM file_versions
                 WHERE snapshot_id = ?1
                   AND path_id = (SELECT id FROM paths
                                   WHERE unit_id = (SELECT unit_id FROM snapshots WHERE id = ?1)
                                     AND path = ?2)",
                params![snapshot_id, path],
            )
            .unwrap();
        assert_eq!(n, 1, "no row for {path:?} in snapshot {snapshot_id}");
    }

    /// `snapshot_id`'s non-directory `(path, size)` rows, by path.
    pub fn non_dirs(conn: &Connection, snapshot_id: i64) -> Vec<(String, i64)> {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT p.path, fv.size_bytes FROM {VERSION_FILES}
                 WHERE fv.snapshot_id = ?1 AND fv.kind <> 0 ORDER BY p.path"
            ))
            .unwrap();
        stmt.query_map(params![snapshot_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_codes_are_the_columns_check() {
        let conn = crate::db::open_memory().unwrap();
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'file_versions'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(sql.contains("CHECK(kind IN (0, 1, 2, 3))"), "{sql}");
        for kind in [
            FileKind::Dir,
            FileKind::Regular,
            FileKind::Symlink,
            FileKind::Special,
        ] {
            assert_eq!(FileKind::from_code(kind.code()), Some(kind));
        }
        assert_eq!(FileKind::from_code(4), None);
    }

    #[test]
    fn sha256_hex_round_trips_and_refuses_what_would_not() {
        let hex = "00ff10a0".repeat(8);
        let bytes = sha256_from_hex(&hex).unwrap();
        assert_eq!(bytes[1], 0xff);
        assert_eq!(sha256_to_hex(&bytes), hex);
        for bad in [
            "",
            "deadbeef",
            &"AB".repeat(32),
            &"g0".repeat(32),
            &"a".repeat(65),
        ] {
            assert!(sha256_from_hex(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn mtime_text_round_trips_only_in_the_walks_spelling() {
        let ns = mtime_ns_from_rfc3339("2026-09-01T12:00:00+00:00").unwrap();
        assert_eq!(ns, 1_788_264_000 * 1_000_000_000);
        assert_eq!(
            mtime_ns_to_rfc3339(ns).as_deref(),
            Some("2026-09-01T12:00:00+00:00")
        );
        assert_eq!(mtime_ns_from_secs(1_788_264_000), Some(ns));
        // What the walk itself produces, compared as the stored value.
        let walked = chrono::DateTime::from_timestamp(1_788_264_000, 0)
            .unwrap()
            .to_rfc3339();
        assert_eq!(mtime_ns_from_rfc3339(&walked).unwrap(), ns);
        for bad in [
            "2026-09-01T12:00:00Z",
            "2026-09-01T14:00:00+02:00",
            "2026-09-01",
            "",
        ] {
            assert!(
                mtime_ns_from_rfc3339(bad).is_err(),
                "{bad:?} must be refused"
            );
        }
        assert_eq!(
            mtime_ns_to_rfc3339(-1_000_000_000).as_deref(),
            Some("1969-12-31T23:59:59+00:00")
        );
    }

    /// Issue #413: a version's file list reaches the search index in ONE
    /// flush. Written row by row, every statement opened a savepoint and
    /// FTS5 flushed its pending index on each, so every path became its own
    /// segment plus merges: superlinear, 306 s for 184k rows on a fresh
    /// catalog. Counted, not timed: the writes to `paths_fts_data` (the
    /// index's segments), on a catalog `db::open` created, which is the
    /// shape (and the `PRAGMA optimize` statistics) a DR rebuild meets.
    #[test]
    fn a_version_reaches_the_search_index_in_one_flush() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        const N: usize = 3_000;
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("tapectl.db")).unwrap();
        conn.execute_batch(
            "INSERT INTO tenants (id, name, is_operator, status) VALUES (1, 't', 1, 'active');
             INSERT INTO units (id, uuid, name, tenant_id) VALUES (1, 'u', 'u', 1);
             INSERT INTO snapshots (id, unit_id, version, source_path) VALUES (1, 1, 1, '/s');",
        )
        .unwrap();

        let writes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&writes);
        conn.update_hook(Some(
            move |_: rusqlite::hooks::Action, _: &str, table: &str, _: i64| {
                if table == "paths_fts_data" {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            },
        ))
        .unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        let written = insert_version(
            &tx,
            1,
            (0..N).map(|i| {
                Ok(FileEntry {
                    path: format!("album{}/photo_{i:05}.jpg", i % 7),
                    kind: FileKind::Regular,
                    size_bytes: i as i64,
                    mtime_ns: Some(i as i64),
                    sha256: None,
                    link_target: None,
                })
            }),
        )
        .unwrap();
        tx.commit().unwrap();
        conn.update_hook(None::<fn(rusqlite::hooks::Action, &str, &str, i64)>)
            .unwrap();

        assert_eq!(written, N, "positive control: every row written");
        let indexed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM paths_fts WHERE paths_fts MATCH 'photo*'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            indexed, N as i64,
            "positive control: every path is searchable"
        );
        let n = writes.load(Ordering::SeqCst);
        assert!(n > 0, "positive control: the hook sees the index's writes");
        assert!(
            n < N / 20,
            "{n} writes to the search index for {N} paths: it must be flushed once, \
             not once per row"
        );
    }
}
