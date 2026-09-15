//! The one predicate behind *Dirty* (`docs/design/v2-open-questions.md`
//! §11) and behind ADR-0012's ruling that a version is minted only when
//! content changed (issue #159): does an already-computed directory walk
//! match a specific recorded snapshot, by the unit's own `checksum_mode`?
//!
//! Deliberately takes the walk as input rather than performing one itself.
//! The walk is a full filesystem pass, and each of this module's two real
//! callers has already paid that cost for its own reason
//! (`collection::fingerprint::classify`'s dirty scan;
//! `staging::snapshot_create`'s manifest build). Calling a `WalkDir` here
//! too would silently double every snapshot's wall-clock time — exactly
//! the regression issue #159 calls out by name.
//!
//! **Home is `src/unit/`, not `src/collection/` or `src/staging/`,**
//! because of the dependency direction: `collection` already legitimately
//! depends on `staging` (dar/encryption plumbing) and on `unit`; `staging`
//! already depends on `unit` (nesting checks). Putting the shared
//! comparison in `collection` would force `staging::snapshot_create` to
//! reach upward into it — a cycle. `unit` depends on neither, so both can
//! depend on it without one.
//!
//! The one piece that doesn't fit that rule is content hashing
//! (`checksum_mode = "sha256"`): the only hashing primitive in the tree is
//! `staging::validate::hash_source_file`. Rather than pull `staging` in as
//! a dependency of `unit`, `matches_snapshot` takes it as an injected
//! closure — both real callers pass the exact same function, so the
//! algorithm can never drift between them, and this module still depends
//! on nothing but `crate::db`/`crate::error`.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;

/// One file as recorded / freshly walked — the same shape the `files`
/// table stores (path, size_bytes, modified_at as RFC3339), so a
/// snapshot's recorded rows and a fresh walk compare like for like.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FileStamp {
    pub(crate) path: String,
    pub(crate) size_bytes: i64,
    pub(crate) modified_at: String,
}

/// Specific added/removed/modified paths behind a content mismatch. Empty
/// means "matches" — a brand new unit (nothing recorded yet) is never
/// represented by this type at all; that's the caller's `None`/`New` case.
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

/// Bidirectional, sorted-merge compare of two path-sorted stamp lists —
/// one linear pass over data already in memory, not a second filesystem or
/// database scan. Both inputs must already be sorted by path (`FileStamp`'s
/// derived `Ord` compares `path` first). Removal counts as change: a
/// `recorded` entry with no `fresh` counterpart is reported in `removed`,
/// not silently dropped (ADR-0012 — "two versions of a unit never hold
/// identical content" requires the comparison to be bidirectional).
pub(crate) fn diff_stamps(recorded: &[FileStamp], fresh: &[FileStamp]) -> FingerprintDiff {
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

/// The unit's most recent snapshot, if any: `(id, version, status)`. This
/// single query is what keeps `collection::fingerprint::classify` (the
/// *Dirty* scan) and `staging::snapshot_create` (the minting
/// short-circuit, issue #159) from ever comparing against two different
/// rows — both call this, neither re-derives "latest" on its own.
pub(crate) fn latest_snapshot(
    conn: &Connection,
    unit_id: i64,
) -> Result<Option<(i64, i64, String)>> {
    Ok(conn
        .query_row(
            "SELECT id, version, status FROM snapshots WHERE unit_id = ?1
             ORDER BY version DESC LIMIT 1",
            params![unit_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?)
}

/// `snapshot_id`'s recorded `(path, size_bytes, modified_at)` for every
/// non-directory row, sorted by path.
fn recorded_stamps(conn: &Connection, snapshot_id: i64) -> Result<Vec<FileStamp>> {
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
    Ok(rows)
}

/// `(file_type, sha256)`, keyed by path.
type HashBaseline = HashMap<String, (Option<String>, Option<String>)>;

/// `snapshot_id`'s recorded `(file_type, sha256)` per path — the extra
/// baseline the `sha256` checksum_mode needs beyond what `FileStamp`
/// carries.
fn recorded_hash_baseline(conn: &Connection, snapshot_id: i64) -> Result<HashBaseline> {
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

/// Does `fresh` (an already-computed walk, sorted by path) match
/// `snapshot_id`'s recorded content, by `checksum_mode`? An empty
/// `FingerprintDiff` means "matches" — the caller decides what that means
/// (not dirty; or, for `snapshot_create`, nothing to mint).
///
/// - `mtime_size` (the default) and `sha256_on_archive` ("hash at archive
///   time" says nothing about how to detect a match, issue #36): path +
///   size + mtime only. No file is ever opened.
/// - `sha256`: additionally hashes regular files via `hash_source_file`,
///   once the mtime_size comparison already agrees — the one edit
///   mtime_size can never catch (same path, size, mtime, different bytes).
///   Symlinks/special files and files with no recorded baseline (never
///   staged) fall back to the mtime_size verdict rather than reporting a
///   spurious change. A single unreadable file is reported as `modified`
///   ("cannot prove clean") rather than aborting the whole comparison.
pub(crate) fn matches_snapshot(
    conn: &Connection,
    snapshot_id: i64,
    checksum_mode: &str,
    unit_path: &Path,
    fresh: &[FileStamp],
    hash_source_file: impl Fn(&Path, &str) -> Result<(String, i64)>,
) -> Result<FingerprintDiff> {
    let recorded = recorded_stamps(conn, snapshot_id)?;
    let diff = diff_stamps(&recorded, fresh);
    if !diff.is_empty() || checksum_mode != "sha256" {
        // mtime_size already disagrees (dirty regardless of checksum_mode),
        // or mtime_size agrees and this mode never looks further.
        return Ok(diff);
    }

    let baseline = recorded_hash_baseline(conn, snapshot_id)?;
    let mut modified = Vec::new();
    for stamp in fresh {
        let Some((file_type, sha256)) = baseline.get(&stamp.path) else {
            continue; // Not reached in practice: mtime_size already agreed
                      // on the path set by the time this loop runs.
        };
        if file_type.as_deref() != Some("regular") {
            continue; // symlink/special/dir — no content to hash.
        }
        let Some(expected) = sha256 else {
            continue; // never staged — no baseline to compare against.
        };
        let full_path = unit_path.join(&stamp.path);
        let actual = match hash_source_file(&full_path, &stamp.path) {
            Ok((hex, _)) => hex,
            Err(e) => {
                tracing::warn!(
                    path = %stamp.path,
                    error = %e,
                    "cannot hash file during content-match scan — reporting as changed"
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

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(path: &str, size: i64, mtime: &str) -> FileStamp {
        FileStamp {
            path: path.to_string(),
            size_bytes: size,
            modified_at: mtime.to_string(),
        }
    }

    #[test]
    fn diff_stamps_reports_no_changes_for_identical_sorted_lists() {
        let recorded = vec![stamp("a.txt", 5, "t1"), stamp("b.txt", 7, "t1")];
        let fresh = recorded.clone();
        assert!(diff_stamps(&recorded, &fresh).is_empty());
    }

    #[test]
    fn diff_stamps_reports_a_new_path_as_added() {
        let recorded = vec![stamp("a.txt", 5, "t1")];
        let fresh = vec![stamp("a.txt", 5, "t1"), stamp("b.txt", 7, "t1")];
        let diff = diff_stamps(&recorded, &fresh);
        assert_eq!(diff.added, vec!["b.txt".to_string()]);
        assert!(diff.removed.is_empty());
        assert!(diff.modified.is_empty());
    }

    #[test]
    fn diff_stamps_reports_a_missing_path_as_removed() {
        // ADR-0012: removal is change, not silently ignored.
        let recorded = vec![stamp("a.txt", 5, "t1"), stamp("b.txt", 7, "t1")];
        let fresh = vec![stamp("a.txt", 5, "t1")];
        let diff = diff_stamps(&recorded, &fresh);
        assert!(diff.added.is_empty());
        assert_eq!(diff.removed, vec!["b.txt".to_string()]);
        assert!(diff.modified.is_empty());
    }

    #[test]
    fn diff_stamps_reports_a_size_or_mtime_change_as_modified() {
        let recorded = vec![stamp("a.txt", 5, "t1")];
        let fresh = vec![stamp("a.txt", 6, "t1")];
        let diff = diff_stamps(&recorded, &fresh);
        assert_eq!(diff.modified, vec!["a.txt".to_string()]);
    }
}
