//! A staging's file table in the catalog: `stage_files` (ADR-0012 item 265;
//! `volume-format-v3.md` §1.6 and Appendix A.3; #557).
//!
//! The rows are `volume::filetable::Row`s, the same type the table's text
//! is written from and parsed into. A path is interned in `paths` under the
//! staging's unit and named by its id, never stored as text twice. The rows
//! outlive the release of the staging's slices and go only with the
//! staging's own row (`ON DELETE CASCADE`), so that `db backup`, the Heir
//! Kit's catalogue and every tape's operator `catalog.db` carry them.

use rusqlite::{params, Connection, Statement};

use crate::error::{Result, TapectlError};
use crate::volume::filetable::{Chain, Kind, Row};

/// The unit a staging belongs to: its paths are interned under it.
fn unit_of(conn: &Connection, stage_set_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT s.unit_id FROM stage_sets ss JOIN snapshots s ON s.id = ss.snapshot_id
          WHERE ss.id = ?1",
        params![stage_set_id],
        |r| r.get(0),
    )?)
}

fn as_i64(value: u64, what: &str) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| TapectlError::Other(format!("file table: {what} {value} is too large")))
}

/// Write `rows` as `stage_set_id`'s file table, in stream order, each held
/// to the table's rules (`filetable::Chain`). Run it inside the caller's
/// transaction; a staging that already has a table is refused by the
/// primary key (ask [`exists`] first). Returns the rows written.
///
/// Shaped as `db::files::insert_version` is (issue #413): the rows are
/// gathered by one prepared statement into a TEMP table, then the paths not
/// yet interned (the unit's root `.`, a hard link's target) go into `paths`
/// in one `INSERT … SELECT`, which flushes the full-text index once, and the
/// table goes into `stage_files` in one more. A million members is a
/// million executions of one statement and three statements besides.
pub fn insert(
    conn: &Connection,
    stage_set_id: i64,
    rows: impl IntoIterator<Item = Result<Row>>,
) -> Result<usize> {
    let unit_id = unit_of(conn, stage_set_id)?;
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS stage_files_in (
             seq    INTEGER PRIMARY KEY,
             kind   TEXT NOT NULL,
             slice  INTEGER NOT NULL,
             size   INTEGER NOT NULL,
             sha256 BLOB,
             start  INTEGER NOT NULL,
             \"end\" INTEGER NOT NULL,
             path   TEXT NOT NULL,
             link   TEXT
         );
         DELETE FROM temp.stage_files_in;",
    )?;
    let written = (|| -> Result<usize> {
        {
            let mut gather = conn.prepare(
                "INSERT INTO temp.stage_files_in
                     (seq, kind, slice, size, sha256, start, \"end\", path, link)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            let mut chain = Chain::default();
            for row in rows {
                let row = row?;
                let seq = chain.rows();
                chain.accept(&row).map_err(|e| {
                    TapectlError::Other(format!(
                        "file table of stage set {stage_set_id}: member {seq}: {e}"
                    ))
                })?;
                gather.execute(params![
                    as_i64(seq, "seq")?,
                    row.kind.letter(),
                    as_i64(row.slice, "slice")?,
                    as_i64(row.size, "size")?,
                    row.sha256.as_ref().map(|h| h.as_slice()),
                    as_i64(row.start, "start")?,
                    as_i64(row.end, "end")?,
                    row.path,
                    row.link,
                ])?;
            }
        }
        conn.execute(
            "INSERT INTO paths (unit_id, path)
             SELECT ?1, t.path FROM (
                 SELECT path, seq FROM temp.stage_files_in
                 UNION ALL
                 SELECT link, seq FROM temp.stage_files_in WHERE link IS NOT NULL
             ) t
              WHERE NOT EXISTS (SELECT 1 FROM paths p WHERE p.unit_id = ?1 AND p.path = t.path)
              GROUP BY t.path
              ORDER BY MIN(t.seq)",
            params![unit_id],
        )?;
        Ok(conn.execute(
            "INSERT INTO stage_files (stage_set_id, seq, kind, slice, path_id, size, sha256,
                                      start, \"end\", link_path_id)
             SELECT ?1, t.seq, t.kind, t.slice, p.id, t.size, t.sha256, t.start, t.\"end\", lp.id
               FROM temp.stage_files_in t
               JOIN paths p ON p.unit_id = ?2 AND p.path = t.path
               LEFT JOIN paths lp ON lp.unit_id = ?2 AND lp.path = t.link
              ORDER BY t.seq",
            params![stage_set_id, unit_id],
        )?)
    })();
    // Emptied on every outcome, as `insert_version`'s: the TEMP table lives
    // as long as the connection. The batch's own error is the one reported.
    let cleared = conn.execute("DELETE FROM temp.stage_files_in", []);
    let written = written?;
    cleared?;
    Ok(written)
}

/// Whether `stage_set_id` has a file table.
pub fn exists(conn: &Connection, stage_set_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM stage_files WHERE stage_set_id = ?1)",
        params![stage_set_id],
        |r| r.get(0),
    )?)
}

/// The statement that reads a table back, prepared once:
/// `Query::new(conn)?.rows(id)?` iterates one staging's rows in stream
/// order without holding them all.
pub struct Query<'c> {
    stmt: Statement<'c>,
}

/// A refusal that names the row (`volume-format-v3.md` §1.4: "A reader
/// refuses, naming the row, a value outside a column's allowed set"; #595
/// 5-C2): a `catalog.db` built without the CHECKs reaches this.
fn bad_row(stage_set_id: i64, seq: i64, what: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        format!("stage_files row (stage_set_id {stage_set_id}, seq {seq}): {what}").into(),
    )
}

fn kind_of(letter: &str) -> Option<Kind> {
    Kind::from_letter(letter)
}

fn sha_of(blob: Option<Vec<u8>>) -> Option<Option<[u8; 32]>> {
    match blob {
        None => Some(None),
        Some(b) => <[u8; 32]>::try_from(b.as_slice()).ok().map(Some),
    }
}

/// A column that must not be negative, as the unsigned the row holds.
fn unsigned(v: i64, column: &str, set: i64, seq: i64) -> rusqlite::Result<u64> {
    u64::try_from(v).map_err(|_| bad_row(set, seq, format!("{column} is {v}, below zero")))
}

impl<'c> Query<'c> {
    pub fn new(conn: &'c Connection) -> Result<Self> {
        Ok(Query {
            stmt: conn.prepare(
                "SELECT f.kind, f.slice, f.start, f.\"end\", f.size, f.sha256, p.path, lp.path,
                        f.stage_set_id, f.seq
                   FROM stage_files f
                   JOIN paths p ON p.id = f.path_id
                   LEFT JOIN paths lp ON lp.id = f.link_path_id
                  WHERE f.stage_set_id = ?1
                  ORDER BY f.seq",
            )?,
        })
    }

    /// `stage_set_id`'s rows, in stream order.
    pub fn rows(&mut self, stage_set_id: i64) -> Result<impl Iterator<Item = Result<Row>> + '_> {
        let rows = self.stmt.query_map(params![stage_set_id], |r| {
            let (set, seq): (i64, i64) = (r.get(8)?, r.get(9)?);
            let letter: String = r.get(0)?;
            let kind = kind_of(&letter)
                .ok_or_else(|| bad_row(set, seq, format!("kind {letter:?} is not a file kind")))?;
            let sha256 = sha_of(r.get(5)?)
                .ok_or_else(|| bad_row(set, seq, "sha256 is not 32 bytes".into()))?;
            Ok(Row {
                kind,
                slice: unsigned(r.get(1)?, "slice", set, seq)?,
                start: unsigned(r.get(2)?, "start", set, seq)?,
                end: unsigned(r.get(3)?, "end", set, seq)?,
                size: unsigned(r.get(4)?, "size", set, seq)?,
                sha256,
                path: r.get(6)?,
                link: r.get(7)?,
            })
        })?;
        Ok(rows.map(|r| r.map_err(TapectlError::from)))
    }
}

/// `stage_set_id`'s whole table in memory, in stream order: for the
/// lookups of `volume::filetable`, which take a slice of rows.
pub fn load(conn: &Connection, stage_set_id: i64) -> Result<Vec<Row>> {
    Query::new(conn)?.rows(stage_set_id)?.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::filetable;

    /// #595 5-C2: a `catalog.db` made without the CHECKs refuses naming the
    /// staging and `seq` of the bad row.
    #[test]
    fn a_row_outside_the_allowed_set_is_refused_naming_it() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE paths (id INTEGER PRIMARY KEY, unit_id INTEGER, path TEXT);
             CREATE TABLE stage_files (stage_set_id INTEGER, seq INTEGER, kind TEXT,
               slice INTEGER, path_id INTEGER, size INTEGER, sha256 BLOB, start INTEGER,
               \"end\" INTEGER, link_path_id INTEGER);
             INSERT INTO paths VALUES (1, 1, 'a');
             INSERT INTO stage_files VALUES (7, 0, 'd', 1, 1, 0, NULL, 0, 1, NULL);
             INSERT INTO stage_files VALUES (7, 3, 'x', 1, 1, 0, NULL, 0, 1, NULL);",
        )
        .unwrap();
        let err = load(&conn, 7).unwrap_err().to_string();
        assert!(err.contains("stage_set_id 7, seq 3"), "{err}");
        assert!(err.contains("\"x\""), "{err}");
        conn.execute("DELETE FROM stage_files WHERE seq = 3", [])
            .unwrap();
        conn.execute("UPDATE stage_files SET size = -5", [])
            .unwrap();
        let err = load(&conn, 7).unwrap_err().to_string();
        assert!(
            err.contains("stage_set_id 7, seq 0") && err.contains("size"),
            "{err}"
        );
    }

    /// A catalog with one unit, one version of it and one stage set, the
    /// version's file list holding `a` (so one path is interned already).
    fn catalog() -> (Connection, i64) {
        let conn = crate::db::open_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO tenants (id, name) VALUES (1, 't');
             INSERT INTO units (id, uuid, name, tenant_id, current_path)
                  VALUES (1, '11111111-2222-3333-4444-555555555555', 'u', 1, '/src/u');
             INSERT INTO snapshots (id, unit_id, version, source_path) VALUES (1, 1, 1, '/src/u');
             INSERT INTO stage_sets (id, snapshot_id, slice_size) VALUES (7, 1, 4096);",
        )
        .unwrap();
        crate::db::files::fixture::insert(&conn, 1, "a\\tb", 5, "regular", None);
        (conn, 7)
    }

    fn rows() -> Vec<Row> {
        let hash = |b: u8| Some([b; 32]);
        vec![
            Row {
                kind: Kind::Folder,
                slice: 1,
                start: 0,
                end: 512,
                size: 0,
                sha256: None,
                path: ".".into(),
                link: None,
            },
            Row {
                kind: Kind::File,
                slice: 1,
                start: 512,
                end: 2048,
                size: 1000,
                sha256: hash(1),
                path: "a\\tb".into(),
                link: None,
            },
            Row {
                kind: Kind::File,
                slice: 2,
                start: 2048,
                end: 2048 + 9000,
                size: 8000,
                sha256: hash(2),
                path: "new\\nline\\\\".into(),
                link: None,
            },
            Row {
                kind: Kind::HardLink,
                slice: 5,
                start: 11048,
                end: 11560,
                size: 1000,
                sha256: hash(1),
                path: "again".into(),
                link: Some("a\\tb".into()),
            },
        ]
    }

    #[test]
    fn a_table_round_trips_through_the_catalog_and_names_each_path_once() {
        let (conn, ss) = catalog();
        assert!(!exists(&conn, ss).unwrap());
        let paths_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM paths", [], |r| r.get(0))
            .unwrap();
        assert_eq!(insert(&conn, ss, rows().into_iter().map(Ok)).unwrap(), 4);
        assert!(exists(&conn, ss).unwrap());
        assert_eq!(load(&conn, ss).unwrap(), rows());

        // `a\tb` was interned already; `.`, the long file and `again` are
        // new, and the link's target is the same path row as the file.
        let paths_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM paths", [], |r| r.get(0))
            .unwrap();
        assert_eq!(paths_after - paths_before, 3);
        let (path_id, link_id): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT path_id FROM stage_files WHERE seq = 1),
                        (SELECT link_path_id FROM stage_files WHERE seq = 3)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(path_id, link_id);

        // What the catalog gives back writes the same text as the rows.
        let mut from_db = Vec::new();
        filetable::write(
            &mut from_db,
            20000,
            Query::new(&conn).unwrap().rows(ss).unwrap(),
        )
        .unwrap();
        let mut from_rows = Vec::new();
        filetable::write(&mut from_rows, 20000, rows().into_iter().map(Ok)).unwrap();
        assert_eq!(from_db, from_rows);
    }

    #[test]
    fn a_row_that_breaks_the_chain_writes_nothing() {
        let (conn, ss) = catalog();
        let mut bad = rows();
        bad[2].start += 512;
        let e = insert(&conn, ss, bad.into_iter().map(Ok)).unwrap_err();
        assert!(e.to_string().contains("member 2"), "{e}");
        assert!(!exists(&conn, ss).unwrap());
        // The TEMP table is empty for the next batch.
        assert_eq!(insert(&conn, ss, rows().into_iter().map(Ok)).unwrap(), 4);
    }

    #[test]
    fn the_table_outlives_its_slices_release_and_goes_with_its_stage_set() {
        let (conn, ss) = catalog();
        insert(&conn, ss, rows().into_iter().map(Ok)).unwrap();
        conn.execute(
            "UPDATE stage_sets SET status = 'cleaned', cleaned_at = datetime('now') WHERE id = ?1",
            [ss],
        )
        .unwrap();
        assert_eq!(load(&conn, ss).unwrap().len(), 4);
        conn.execute("DELETE FROM stage_sets WHERE id = ?1", [ss])
            .unwrap();
        assert!(!exists(&conn, ss).unwrap());
    }

    #[test]
    fn a_second_table_for_one_staging_is_refused() {
        let (conn, ss) = catalog();
        insert(&conn, ss, rows().into_iter().map(Ok)).unwrap();
        assert!(insert(&conn, ss, rows().into_iter().map(Ok)).is_err());
        assert_eq!(load(&conn, ss).unwrap(), rows());
    }
}
