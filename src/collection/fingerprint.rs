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
//! comparison; `mtime_size` is the documented default and stays exactly
//! that fast, no-hashing comparison unconditionally.
//!
//! Issue #36/H10 wires up the other two `checksum_mode` values on top of
//! that same comparison, never replacing it: `sha256` additionally compares
//! content hash for regular files (`hash_diff`) once the mtime_size
//! fingerprint already matches — the one edit mtime_size can never catch
//! (same path, size, mtime, different bytes). `sha256_on_archive` means
//! "hash at archive time," so dirty detection for it is deliberately
//! identical to `mtime_size` (coordinator decision, issue #36). This makes
//! `classify` the single scanner every caller (`unit status --dirty`,
//! `mark-tape-only`'s guard, `report dirty`, and the pre-existing
//! `collection sync/status/plan`) shares — a second, independently-written
//! dirty check is exactly the class of bug issue #33 spent a cycle fixing
//! (a walk and a validator disagreeing about the same fact).

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rusqlite::{params, Connection};
use walkdir::WalkDir;

use crate::config::CollectionConfig;
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

/// Specific on-disk changes behind a `Dirty` verdict (issue #36/H10): an
/// operator deciding whether it's safe to `mark-tape-only` — or just
/// running `unit status --dirty` — needs to know WHICH files changed, not
/// only that something did. Always empty for `PendingReason::New` (there is
/// no recorded baseline yet to diff against).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FingerprintDiff {
    /// Paths present on disk but not in the recorded fingerprint.
    pub added: Vec<String>,
    /// Paths in the recorded fingerprint but no longer on disk.
    pub removed: Vec<String>,
    /// Paths present in both but changed (mtime_size mismatch, or —
    /// `sha256` mode only — a content hash mismatch at an unchanged
    /// mtime_size).
    pub modified: Vec<String>,
}

impl FingerprintDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.modified.is_empty()
    }

    /// Compact one-line summary for CLI error messages (`mark-tape-only`'s
    /// guard) and plain-text reports — capped so a unit with thousands of
    /// changed files can't flood a single line. Full, uncapped lists remain
    /// on the struct itself for callers that want every path (`unit status
    /// --dirty`'s plain output, and every JSON caller).
    pub fn describe(&self) -> String {
        const MAX_NAMES: usize = 8;

        let mut counts = Vec::new();
        if !self.added.is_empty() {
            counts.push(format!("{} added", self.added.len()));
        }
        if !self.removed.is_empty() {
            counts.push(format!("{} removed", self.removed.len()));
        }
        if !self.modified.is_empty() {
            counts.push(format!("{} modified", self.modified.len()));
        }
        if counts.is_empty() {
            return "no changes".to_string();
        }

        let mut names: Vec<&str> = self
            .added
            .iter()
            .chain(self.removed.iter())
            .chain(self.modified.iter())
            .map(|s| s.as_str())
            .collect();
        let total = names.len();
        names.truncate(MAX_NAMES);
        let more = if total > MAX_NAMES {
            format!(", … and {} more", total - MAX_NAMES)
        } else {
            String::new()
        };
        format!("{} ({}{more})", counts.join(", "), names.join(", "))
    }
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
    /// Specific added/removed/modified paths — see `FingerprintDiff`.
    pub changes: FingerprintDiff,
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

/// Merge-diff two path-sorted fingerprints into added/removed/modified path
/// lists. `walk_fingerprint` and `recorded_fingerprint` both already return
/// `Vec<FileStamp>` sorted by path (their derived `Ord` compares `path`
/// first), so this is one linear pass over data already in memory — not a
/// second filesystem or database scan.
fn diff_stamps(recorded: &[FileStamp], fresh: &[FileStamp]) -> FingerprintDiff {
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut modified = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < recorded.len() && j < fresh.len() {
        match recorded[i].path.cmp(&fresh[j].path) {
            std::cmp::Ordering::Equal => {
                if recorded[i].size_bytes != fresh[j].size_bytes
                    || recorded[i].modified_at != fresh[j].modified_at
                {
                    modified.push(fresh[j].path.clone());
                }
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => {
                removed.push(recorded[i].path.clone());
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                added.push(fresh[j].path.clone());
                j += 1;
            }
        }
    }
    removed.extend(recorded[i..].iter().map(|f| f.path.clone()));
    added.extend(fresh[j..].iter().map(|f| f.path.clone()));
    FingerprintDiff {
        added,
        removed,
        modified,
    }
}

/// `(file_type, sha256)`, keyed by path.
type HashBaseline = HashMap<String, (Option<String>, Option<String>)>;

/// Recorded `(file_type, sha256)` per path in the latest snapshot — the
/// extra baseline `hash_diff` needs beyond what `FileStamp` carries. Its own
/// "latest snapshot" lookup rather than a shared helper with
/// `recorded_fingerprint`, so the `mtime_size` comparison above is
/// provably untouched by this addition (issue #36): `recorded_fingerprint`
/// is not modified by one byte.  `None` if the unit has no snapshot yet —
/// `classify` never actually calls this in that case, since `New` is
/// decided first, but it mirrors `recorded_fingerprint`'s own contract.
fn recorded_hash_baseline(conn: &Connection, unit_id: i64) -> Result<HashBaseline> {
    let latest_snapshot_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM snapshots WHERE unit_id = ?1 ORDER BY version DESC LIMIT 1",
            params![unit_id],
            |row| row.get(0),
        )
        .ok();
    let Some(snapshot_id) = latest_snapshot_id else {
        return Ok(HashMap::new());
    };

    let mut stmt = conn.prepare(
        "SELECT path, file_type, sha256 FROM files
         WHERE snapshot_id = ?1 AND is_directory = 0",
    )?;
    let map = stmt
        .query_map(params![snapshot_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ),
            ))
        })?
        .collect::<std::result::Result<HashMap<_, _>, _>>()?;
    Ok(map)
}

/// `sha256` checksum_mode only, and only reached once the mtime_size
/// comparison already found the recorded and fresh fingerprints identical
/// (same paths, sizes, mtimes) — the one case `mtime_size` can never catch:
/// content changed while size and mtime did not. Compares each regular
/// file's fresh content hash against the recorded `files.sha256` baseline,
/// via the exact same `hash_source_file` that established that baseline in
/// the first place (`staging::validate::validate_source` ->
/// `backfill_checksums`), so the two can never disagree about the
/// algorithm.
///
/// Symlinks and special files (issue #33/H7: `file_type` != `"regular"`)
/// have no content of their own to hash — their recorded `size_bytes` is
/// lstat's target-string length, not payload — so they are excluded from
/// hash comparison entirely and judged by mtime_size alone, which this
/// function is only reached after already confirming unchanged. A `NULL`
/// baseline (the file was recorded by `snapshot_create` but never staged,
/// so no hash was ever computed) has nothing to compare against either;
/// both cases fall back to the mtime_size verdict — "no change" — rather
/// than reporting a spurious dirty flag.
fn hash_diff(
    conn: &Connection,
    unit_id: i64,
    unit_path: &Path,
    fresh: &[FileStamp],
) -> Result<FingerprintDiff> {
    let baseline = recorded_hash_baseline(conn, unit_id)?;
    let mut modified = Vec::new();
    for stamp in fresh {
        let Some((file_type, sha256)) = baseline.get(&stamp.path) else {
            continue; // Not reached in practice: mtime_size already agreed
                      // on the path set by the time this function is called.
        };
        if file_type.as_deref() != Some("regular") {
            continue; // symlink/special/dir — no content to hash.
        }
        let Some(expected) = sha256 else {
            continue; // never staged — no baseline to compare against.
        };
        let full_path = unit_path.join(&stamp.path);
        // A single unreadable file must NOT abort the scan. `report dirty`
        // sweeps every unit, so propagating this error would turn one bad
        // file into "the whole report failed" — a diagnostic command that
        // refuses to diagnose. The realistic trigger is concrete: migration
        // 005 backfills every pre-existing row as 'regular' because the
        // symlink/special distinction was never recorded before #33, so a
        // legacy row that is really a symlink reaches this line and
        // `hash_source_file`'s own non-regular guard (#33) rejects it.
        // Treat an unreadable file as "cannot prove clean" — report it as
        // modified so it surfaces to the operator, and keep scanning.
        let actual = match crate::staging::validate::hash_source_file(&full_path, &stamp.path) {
            Ok((hex, _)) => hex,
            Err(e) => {
                tracing::warn!(
                    path = %stamp.path,
                    error = %e,
                    "cannot hash file during dirty scan — reporting as changed"
                );
                modified.push(stamp.path.clone());
                continue;
            }
        };
        if &actual != expected {
            modified.push(stamp.path.clone());
        }
    }
    Ok(FingerprintDiff {
        added: Vec::new(),
        removed: Vec::new(),
        modified,
    })
}

/// Classify one unit: `None` if it has a snapshot whose recorded
/// fingerprint matches the current directory (not pending); `Some`
/// otherwise, naming why, estimating its current size from the same walk
/// (one filesystem pass either way), and — when dirty — the specific
/// added/removed/modified paths (issue #36/H10).
///
/// `unit.checksum_mode` decides how thorough the comparison is:
///   - `mtime_size` (the default) and `sha256_on_archive` (coordinator
///     decision, issue #36: "hash at archive time" says nothing about how
///     to detect dirty, so it stays the fast path) — path+size+mtime only,
///     byte-for-byte the same comparison this function has always made.
///     No file is ever opened.
///   - `sha256` — additionally hashes regular files once their mtime_size
///     fingerprint already matches (`hash_diff`), to catch the one edit
///     mtime_size can never see.
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

    let Some(recorded) = recorded_fingerprint(conn, unit.id)? else {
        return Ok(Some(PendingUnit {
            unit: unit.clone(),
            reason: PendingReason::New,
            estimated_bytes,
            changes: FingerprintDiff::default(),
        }));
    };

    if recorded != fresh {
        // mtime_size already disagrees — dirty regardless of
        // checksum_mode, exactly as before this field existed; naming the
        // specific changes is the only thing new here.
        return Ok(Some(PendingUnit {
            unit: unit.clone(),
            reason: PendingReason::Dirty,
            estimated_bytes,
            changes: diff_stamps(&recorded, &fresh),
        }));
    }

    // mtime_size fingerprints match exactly. `mtime_size` and
    // `sha256_on_archive` stop here — the same early return this function
    // has always made for a matching fingerprint (previously spelled as a
    // match-guard `Some(recorded) if recorded == fresh => return Ok(None)`;
    // case-equivalent, no file is opened).
    if unit.checksum_mode != "sha256" {
        return Ok(None);
    }

    let changes = hash_diff(conn, unit.id, Path::new(path), &fresh)?;
    if changes.is_empty() {
        return Ok(None);
    }
    Ok(Some(PendingUnit {
        unit: unit.clone(),
        reason: PendingReason::Dirty,
        estimated_bytes,
        changes,
    }))
}

/// All pending units for one collection: active units under its root, each
/// classified. Only `'active'` units are considered — `missing` units have
/// no directory to walk, and `tape_only`/`retired` are deliberate operator
/// states this module never second-guesses.
pub fn pending_units_for_collection(
    conn: &Connection,
    lib: &CollectionConfig,
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
        // issue #36: the specific change must be named, not just "dirty".
        assert_eq!(p.changes.added, vec!["g.txt".to_string()]);
        assert!(p.changes.removed.is_empty());
        assert!(p.changes.modified.is_empty());
    }

    #[test]
    fn a_removed_file_is_named_in_the_dirty_changes() {
        let conn = db::open_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("f.txt");
        std::fs::write(&file_path, b"hello").unwrap();
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

        std::fs::remove_file(&file_path).unwrap();

        let p = classify(&conn, &unit).unwrap().expect("must be dirty");
        assert_eq!(p.reason, PendingReason::Dirty);
        assert_eq!(p.changes.removed, vec!["f.txt".to_string()]);
        assert!(p.changes.added.is_empty());
        assert!(p.changes.modified.is_empty());
    }

    #[test]
    fn a_modified_file_is_named_in_the_dirty_changes() {
        let conn = db::open_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("f.txt");
        std::fs::write(&file_path, b"hello").unwrap();
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

        // A different size is enough for mtime_size to see this without
        // needing to fuss with mtime precision.
        std::fs::write(&file_path, b"hello, world! this content is now longer").unwrap();

        let p = classify(&conn, &unit).unwrap().expect("must be dirty");
        assert_eq!(p.reason, PendingReason::Dirty);
        assert_eq!(p.changes.modified, vec!["f.txt".to_string()]);
        assert!(p.changes.added.is_empty());
        assert!(p.changes.removed.is_empty());
    }

    /// Sets a file's mtime back to `mtime` after its content has already
    /// been rewritten — used by the `sha256` checksum_mode tests to
    /// construct the one case `mtime_size` cannot see: identical size,
    /// identical mtime, different bytes. `std::fs::File::set_modified` is
    /// stable stdlib (1.75+); no new crate needed for this.
    fn restore_mtime(path: &Path, mtime: std::time::SystemTime) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    #[test]
    fn mtime_size_mode_does_not_catch_a_same_size_same_mtime_content_change() {
        // Contrast for the sha256-mode test below: the default
        // checksum_mode is deliberately blind to this exact edit (issue
        // #36) — that blindness is mtime_size's whole reason for being
        // fast, not a bug the sha256 test is fixing.
        let conn = db::open_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("f.txt");
        std::fs::write(&file_path, b"original content!").unwrap();
        let unit_id = seed_unit(&conn, tmp.path()); // checksum_mode stays default 'mtime_size'
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
        assert_eq!(unit.checksum_mode, "mtime_size");
        crate::staging::snapshot_create(&conn, &unit.name).unwrap();

        let mtime_before = std::fs::metadata(&file_path).unwrap().modified().unwrap();
        std::fs::write(&file_path, b"REPLACED content!").unwrap(); // same length (17 bytes)
        restore_mtime(&file_path, mtime_before);

        assert!(
            classify(&conn, &unit).unwrap().is_none(),
            "mtime_size must stay blind to a same-size-same-mtime content \
             change — that tradeoff is documented, not a bug"
        );
    }

    #[test]
    fn sha256_mode_catches_a_same_size_same_mtime_content_change() {
        // The one edit mtime_size cannot catch (issue #36): same path, same
        // size, same mtime, different bytes. Only checksum_mode = 'sha256'
        // compares content — see the mtime_size contrast test above.
        let conn = db::open_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("f.txt");
        std::fs::write(&file_path, b"original content!").unwrap();

        let unit_id = seed_unit(&conn, tmp.path());
        conn.execute(
            "UPDATE units SET checksum_mode = 'sha256' WHERE id = ?1",
            params![unit_id],
        )
        .unwrap();
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
        assert_eq!(unit.checksum_mode, "sha256");

        crate::staging::snapshot_create(&conn, &unit.name).unwrap();

        // Establish the sha256 baseline for the original content — what a
        // real `stage_create` would have backfilled via the exact same
        // `hash_source_file` this scan reuses.
        let (original_hash, _) =
            crate::staging::validate::hash_source_file(&file_path, "f.txt").unwrap();
        conn.execute(
            "UPDATE files SET sha256 = ?1 WHERE path = 'f.txt'",
            params![original_hash],
        )
        .unwrap();

        // Replace the content with different bytes of the SAME length,
        // then restore the original mtime — mtime_size alone sees no
        // change at all.
        let mtime_before = std::fs::metadata(&file_path).unwrap().modified().unwrap();
        std::fs::write(&file_path, b"REPLACED content!").unwrap();
        restore_mtime(&file_path, mtime_before);

        let p = classify(&conn, &unit)
            .unwrap()
            .expect("sha256 mode must catch a content change mtime_size cannot see");
        assert_eq!(p.reason, PendingReason::Dirty);
        assert_eq!(p.changes.modified, vec!["f.txt".to_string()]);
        assert!(p.changes.added.is_empty());
        assert!(p.changes.removed.is_empty());
    }

    #[test]
    fn sha256_mode_falls_back_to_mtime_size_when_no_baseline_hash_was_ever_recorded() {
        // A unit that has only ever been `snapshot create`d, never staged,
        // has no `files.sha256` baseline at all (issue #36's documented
        // fallback): sha256 mode must not spuriously flag it dirty just
        // because there is nothing to hash-compare against — mtime_size's
        // "unchanged" verdict is authoritative for such a file.
        let conn = db::open_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), b"hello").unwrap();

        let unit_id = seed_unit(&conn, tmp.path());
        conn.execute(
            "UPDATE units SET checksum_mode = 'sha256' WHERE id = ?1",
            params![unit_id],
        )
        .unwrap();
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
        // Never staged — files.sha256 stays NULL for f.txt.

        assert!(
            classify(&conn, &unit).unwrap().is_none(),
            "sha256 mode must fall back to the mtime_size verdict when no \
             baseline hash exists, not report a spurious change"
        );
    }

    #[test]
    fn sha256_mode_excludes_a_symlink_from_hash_comparison() {
        // Traps (issue #36): a symlink's recorded size_bytes is lstat's
        // target-string length, not content (issue #33/H7) — it has no
        // content sha256 to compare. If hash comparison were applied to
        // it anyway, every symlink-containing unit would report dirty
        // forever. Must be judged by mtime/size alone, exactly like the
        // NULL-baseline fallback above.
        let conn = db::open_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("target.txt"), b"hi").unwrap();
        std::os::unix::fs::symlink("target.txt", tmp.path().join("link")).unwrap();

        let unit_id = seed_unit(&conn, tmp.path());
        conn.execute(
            "UPDATE units SET checksum_mode = 'sha256' WHERE id = ?1",
            params![unit_id],
        )
        .unwrap();
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

        // snapshot_create's real walk records the symlink's file_type
        // ('symlink') and leaves its sha256 NULL — nothing to backfill.
        crate::staging::snapshot_create(&conn, &unit.name).unwrap();

        // Run the scan twice: the trap is specifically about a symlink
        // being flagged dirty *forever*, not just once.
        assert!(
            classify(&conn, &unit).unwrap().is_none(),
            "sha256 mode must not flag a symlink dirty for lacking a content hash"
        );
        assert!(
            classify(&conn, &unit).unwrap().is_none(),
            "must stay clean on a second scan too — not perpetually dirty"
        );
    }

    #[test]
    fn sha256_mode_reports_an_unreadable_file_instead_of_aborting_the_scan() {
        // `report dirty` sweeps every unit, so a single unreadable file
        // must not propagate an error and kill the whole report — a
        // diagnostic command that refuses to diagnose. It is reported as
        // changed ("cannot prove clean") and the scan continues.
        use std::os::unix::fs::PermissionsExt;

        let conn = db::open_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let unreadable = tmp.path().join("locked.txt");
        std::fs::write(&unreadable, b"secret").unwrap();

        let unit_id = seed_unit(&conn, tmp.path());
        conn.execute(
            "UPDATE units SET checksum_mode = 'sha256' WHERE id = ?1",
            params![unit_id],
        )
        .unwrap();
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
        // snapshot_create leaves sha256 NULL (only staging populates it),
        // and a NULL baseline is skipped — so give it one, which is what a
        // previously-staged unit would have.
        conn.execute(
            "UPDATE files SET sha256 = 'aa00bb11cc22dd33ee44ff55aa66bb77cc88dd99ee00ff11aa22bb33cc44dd55'
             WHERE path = 'locked.txt'",
            [],
        )
        .unwrap();

        // Size and mtime are untouched, so the mtime_size fingerprint still
        // matches and the scan reaches the hash comparison.
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&unreadable).is_ok() {
            // Running as root, where mode 0o000 is not enforced — the
            // precondition cannot be established, so this proves nothing.
            std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o644)).unwrap();
            return;
        }

        let pending = classify(&conn, &unit)
            .expect("an unreadable file must not abort the scan")
            .expect("the unit must be reported dirty, not silently clean");
        assert_eq!(pending.reason, PendingReason::Dirty);
        assert!(
            pending.changes.modified.contains(&"locked.txt".to_string()),
            "the unreadable file must be surfaced as changed, got {:?}",
            pending.changes
        );

        // Restore so TempDir cleanup can remove it.
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn a_unit_whose_directory_has_vanished_is_not_pending() {
        // classify() must not be sync's vanished-detector — that lives in
        // `collection::sync` and marks the unit `missing` instead.
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
