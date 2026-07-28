//! Pending/dirty detection (`docs/design/v2-open-questions.md` §11):
//! "units with no snapshot, or whose latest snapshot's walk fingerprint
//! (checksum_mode, default mtime_size) differs."
//!
//! The fingerprint is deliberately the same shape `staging::snapshot_create`
//! already records in the `files` table (path, size_bytes, modified_at) —
//! comparing a fresh walk against that recorded set needs no new schema and
//! stays consistent with what a real `snapshot create` would see. Media
//! immutability (§11: "pending ≈ new folders in practice") means the common
//! case is "no snapshot at all," which classifies without needing the
//! comparison; don't gold-plate this into content hashing at sync-scan time
//! — mtime_size is the documented default and is what's implemented here.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rusqlite::{params, Connection};
use walkdir::WalkDir;

use crate::config::LibraryConfig;
use crate::db::models::Unit;
use crate::error::Result;

/// Why a unit is pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingReason {
    /// No snapshot exists at all — never archived.
    New,
    /// A snapshot exists, but the current on-disk fingerprint no longer
    /// matches its recorded one (files added/removed/changed).
    Dirty,
}

/// A unit needing archival work, with an on-disk size estimate (fresh walk,
/// plaintext bytes — the same figure `snapshot_create` would record). This
/// is a planning estimate, not a commitment: the real capacity gate is
/// `Layout::validate` at actual write time.
#[derive(Debug, Clone)]
pub struct PendingUnit {
    pub unit: Unit,
    pub reason: PendingReason,
    pub estimated_bytes: u64,
}

/// One file as the fingerprint sees it — same shape as the `files` table
/// records at `snapshot_create` time, so a fresh walk and a recorded
/// snapshot compare like for like.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct FileStamp {
    path: String,
    size_bytes: i64,
    modified_at: String,
}

/// Fresh walk of `unit_path`, sorted by path. Mirrors
/// `staging::walk_directory`'s file enumeration (unfiltered — excludes are
/// a dar-time / archive-content concern applied later at `stage create`,
/// and the `files` table this compares against is unfiltered too) and its
/// exact mtime-to-RFC3339 conversion, so a byte-identical directory always
/// produces a byte-identical fingerprint against a byte-identical snapshot.
fn walk_fingerprint(unit_path: &Path) -> Vec<FileStamp> {
    let mut out = Vec::new();
    for entry in WalkDir::new(unit_path)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path == unit_path {
            continue; // skip root, same as walk_directory
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            continue;
        }
        let rel = path
            .strip_prefix(unit_path)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let modified_at = chrono::DateTime::from_timestamp(meta.mtime(), 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default();
        out.push(FileStamp {
            path: rel,
            size_bytes: meta.len() as i64,
            modified_at,
        });
    }
    out.sort();
    out
}

/// The same shape, read back from the latest snapshot's recorded `files`
/// rows for `unit_id`. `None` if the unit has no snapshot at all yet.
fn recorded_fingerprint(conn: &Connection, unit_id: i64) -> Result<Option<Vec<FileStamp>>> {
    let latest_snapshot_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM snapshots WHERE unit_id = ?1 ORDER BY version DESC LIMIT 1",
            params![unit_id],
            |row| row.get(0),
        )
        .ok();
    let Some(snapshot_id) = latest_snapshot_id else {
        return Ok(None);
    };

    let mut stmt = conn.prepare(
        "SELECT path, size_bytes, modified_at FROM files
         WHERE snapshot_id = ?1 AND is_directory = 0
         ORDER BY path",
    )?;
    let mut rows: Vec<FileStamp> = stmt
        .query_map(params![snapshot_id], |row| {
            Ok(FileStamp {
                path: row.get(0)?,
                size_bytes: row.get(1)?,
                modified_at: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    rows.sort();
    Ok(Some(rows))
}

/// Classify one unit: `None` if it has a snapshot whose recorded
/// fingerprint matches the current directory (not pending); `Some`
/// otherwise, naming why and estimating its current size from the same
/// walk (one filesystem pass either way).
pub fn classify(conn: &Connection, unit: &Unit) -> Result<Option<PendingUnit>> {
    let Some(path) = unit.current_path.as_deref() else {
        return Ok(None);
    };
    if !Path::new(path).is_dir() {
        // Vanished — sync's job to mark `missing`, not this scan's.
        return Ok(None);
    }

    let fresh = walk_fingerprint(Path::new(path));
    let estimated_bytes: u64 = fresh.iter().map(|f| f.size_bytes.max(0) as u64).sum();

    let reason = match recorded_fingerprint(conn, unit.id)? {
        None => PendingReason::New,
        Some(recorded) if recorded == fresh => return Ok(None),
        Some(_) => PendingReason::Dirty,
    };

    Ok(Some(PendingUnit {
        unit: unit.clone(),
        reason,
        estimated_bytes,
    }))
}

/// All pending units for one library: active units under its root, each
/// classified. Only `'active'` units are considered — `missing` units have
/// no directory to walk, and `tape_only`/`retired` are deliberate operator
/// states this module never second-guesses.
pub fn pending_units_for_library(
    conn: &Connection,
    lib: &LibraryConfig,
) -> Result<Vec<PendingUnit>> {
    let root = super::canonical_root(lib)?;
    let units = super::units_under_root(conn, &root)?;
    let mut out = Vec::new();
    for unit in units.into_iter().filter(|u| u.status == "active") {
        if let Some(p) = classify(conn, &unit)? {
            out.push(p);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use rusqlite::params;

    fn seed_unit(conn: &Connection, path: &Path) -> i64 {
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .ok(); // may already exist across calls in one test; ignore
        let tenant_id: i64 = conn
            .query_row("SELECT id FROM tenants WHERE name = 't'", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES (?1, ?1, ?2, ?3, 'active')",
            params![
                uuid::Uuid::new_v4().to_string(),
                tenant_id,
                path.to_string_lossy().to_string()
            ],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn a_unit_with_no_snapshot_is_new() {
        let conn = db::open_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), b"hello").unwrap();
        let unit_id = seed_unit(&conn, tmp.path());
        let unit = crate::db::queries::get_unit_by_uuid(
            &conn,
            &conn
                .query_row(
                    "SELECT uuid FROM units WHERE id = ?1",
                    params![unit_id],
                    |r| r.get::<_, String>(0),
                )
                .unwrap(),
        )
        .unwrap()
        .unwrap();

        let p = classify(&conn, &unit).unwrap().expect("must be pending");
        assert_eq!(p.reason, PendingReason::New);
        assert_eq!(p.estimated_bytes, 5);
    }

    #[test]
    fn a_unit_whose_snapshot_matches_current_disk_is_not_pending() {
        let conn = db::open_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), b"hello").unwrap();
        let unit_id = seed_unit(&conn, tmp.path());
        let unit_uuid: String = conn
            .query_row(
                "SELECT uuid FROM units WHERE id = ?1",
                params![unit_id],
                |r| r.get(0),
            )
            .unwrap();
        let unit = crate::db::queries::get_unit_by_uuid(&conn, &unit_uuid)
            .unwrap()
            .unwrap();

        // Simulate a real snapshot_create by using it directly.
        crate::staging::snapshot_create(&conn, &unit.name).unwrap();

        assert!(
            classify(&conn, &unit).unwrap().is_none(),
            "freshly snapshotted, unchanged unit must not be pending"
        );
    }

    #[test]
    fn a_unit_changed_since_its_snapshot_is_dirty() {
        let conn = db::open_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), b"hello").unwrap();
        let unit_id = seed_unit(&conn, tmp.path());
        let unit_uuid: String = conn
            .query_row(
                "SELECT uuid FROM units WHERE id = ?1",
                params![unit_id],
                |r| r.get(0),
            )
            .unwrap();
        let unit = crate::db::queries::get_unit_by_uuid(&conn, &unit_uuid)
            .unwrap()
            .unwrap();
        crate::staging::snapshot_create(&conn, &unit.name).unwrap();

        // Mutate after the snapshot: add a new file.
        std::fs::write(tmp.path().join("g.txt"), b"world!!").unwrap();

        let p = classify(&conn, &unit).unwrap().expect("must be dirty");
        assert_eq!(p.reason, PendingReason::Dirty);
        assert_eq!(p.estimated_bytes, 5 + 7);
    }

    #[test]
    fn a_unit_whose_directory_has_vanished_is_not_pending() {
        // classify() must not be sync's vanished-detector — that lives in
        // `library::sync` and marks the unit `missing` instead.
        let conn = db::open_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let gone = tmp.path().join("gone");
        std::fs::create_dir_all(&gone).unwrap();
        let unit_id = seed_unit(&conn, &gone);
        std::fs::remove_dir_all(&gone).unwrap();

        let unit_uuid: String = conn
            .query_row(
                "SELECT uuid FROM units WHERE id = ?1",
                params![unit_id],
                |r| r.get(0),
            )
            .unwrap();
        let unit = crate::db::queries::get_unit_by_uuid(&conn, &unit_uuid)
            .unwrap()
            .unwrap();

        assert!(classify(&conn, &unit).unwrap().is_none());
    }
}
