//! A staging's id map in the catalog: `stage_ids` (ADR-0012 items 272, 277;
//! `volume-format-v3.md` §1.6 and Appendix A.3; #564).
//!
//! The rows are `idmap::IdRow`s, the type the map's text is written from
//! and parsed into. A name the host did not know is NULL. The rows outlive
//! the release of the staging's slices and go only with the staging's own
//! row (`ON DELETE CASCADE`).

use rusqlite::{params, Connection};

use crate::error::{Result, TapectlError};
use crate::idmap::{IdKind, IdRow};

/// Write `rows` as `stage_set_id`'s id map, with one prepared statement.
/// Run it inside the caller's transaction; a staging that already has a map
/// (or one id twice) is refused by the primary key. Returns the rows
/// written.
pub fn insert<'a>(
    conn: &Connection,
    stage_set_id: i64,
    rows: impl IntoIterator<Item = &'a IdRow>,
) -> Result<usize> {
    let mut stmt = conn
        .prepare("INSERT INTO stage_ids (stage_set_id, kind, id, name) VALUES (?1, ?2, ?3, ?4)")?;
    let mut written = 0;
    for row in rows {
        written += stmt.execute(params![
            stage_set_id,
            row.kind.letter(),
            row.id,
            row.name.as_deref().filter(|n| !n.is_empty()),
        ])?;
    }
    Ok(written)
}

/// Whether `stage_set_id` has an id map. A staging whose stream carried no
/// id has none; a stream always carries its members' owners, so a staging
/// with a file table and no map has lost it.
pub fn exists(conn: &Connection, stage_set_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM stage_ids WHERE stage_set_id = ?1)",
        params![stage_set_id],
        |r| r.get(0),
    )?)
}

/// `stage_set_id`'s map in the text's order: users before groups, each by
/// number. (The primary key's own order puts `g` first.)
pub fn load(conn: &Connection, stage_set_id: i64) -> Result<Vec<IdRow>> {
    let mut stmt = conn.prepare(
        "SELECT kind, id, name FROM stage_ids WHERE stage_set_id = ?1
          ORDER BY CASE kind WHEN 'u' THEN 0 ELSE 1 END, id",
    )?;
    let rows = stmt.query_map(params![stage_set_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?, r.get(2)?))
    })?;
    rows.map(|row| {
        let (kind, id, name) = row?;
        let kind = IdKind::from_letter(&kind)
            .ok_or_else(|| TapectlError::Other(format!("stage_ids: not an id kind: {kind:?}")))?;
        Ok(IdRow { kind, id, name })
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::idmap;

    fn catalog() -> (Connection, i64) {
        let conn = crate::db::open_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO tenants (id, name) VALUES (1, 't');
             INSERT INTO units (id, uuid, name, tenant_id) VALUES (1, 'uuid-1', 'u', 1);
             INSERT INTO snapshots (id, unit_id, version, source_path) VALUES (1, 1, 1, '/src/u');
             INSERT INTO stage_sets (id, snapshot_id, slice_size) VALUES (7, 1, 4096);",
        )
        .unwrap();
        (conn, 7)
    }

    fn rows() -> Vec<IdRow> {
        let row = |kind, id, name: Option<&str>| IdRow {
            kind,
            id,
            name: name.map(str::to_string),
        };
        vec![
            row(IdKind::User, 0, Some("root")),
            row(IdKind::User, 4_294_967_294, None),
            row(IdKind::Group, 0, Some("ro\\tot")),
            row(IdKind::Group, 1000, None),
        ]
    }

    #[test]
    fn a_map_round_trips_through_the_catalog_in_the_texts_order() {
        let (conn, ss) = catalog();
        assert!(!exists(&conn, ss).unwrap());
        // Inserted groups first: the order read back is the text's anyway.
        let mut shuffled = rows();
        shuffled.reverse();
        assert_eq!(insert(&conn, ss, &shuffled).unwrap(), 4);
        assert!(exists(&conn, ss).unwrap());
        assert_eq!(load(&conn, ss).unwrap(), rows());
        let unnamed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM stage_ids WHERE name IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unnamed, 2, "a name the host did not know is NULL");

        let mut from_db = Vec::new();
        idmap::write(&mut from_db, &load(&conn, ss).unwrap()).unwrap();
        let mut from_rows = Vec::new();
        idmap::write(&mut from_rows, &rows()).unwrap();
        assert_eq!(from_db, from_rows);
    }

    #[test]
    fn the_map_outlives_its_slices_release_and_goes_with_its_stage_set() {
        let (conn, ss) = catalog();
        insert(&conn, ss, &rows()).unwrap();
        conn.execute(
            "UPDATE stage_sets SET status = 'cleaned' WHERE id = ?1",
            [ss],
        )
        .unwrap();
        assert_eq!(load(&conn, ss).unwrap().len(), 4);
        conn.execute("DELETE FROM stage_sets WHERE id = ?1", [ss])
            .unwrap();
        assert!(!exists(&conn, ss).unwrap());
    }

    #[test]
    fn one_id_twice_is_refused() {
        let (conn, ss) = catalog();
        let twice = [rows()[0].clone(), rows()[0].clone()];
        assert!(insert(&conn, ss, &twice).is_err());
    }
}
