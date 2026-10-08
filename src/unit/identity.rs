//! A unit's identity is the uuid in its dotfile (design v4 §2.2), and a
//! directory found carrying a uuid is taken to be that unit. ADR-0012,
//! 2026-10-07 item 24 (#378): that inference holds for a move, never for a
//! copy. `unit discover` and `collection sync` refuse, per unit,
//!
//! - two directories carrying one uuid in a walk ([`refused_groups`],
//!   [`duplicate_uuid_refusal`]) when neither is the unit's recorded
//!   directory, and
//! - a known uuid found in a new directory while the unit's recorded one
//!   still exists ([`copy_refusal`]): a restore into a scanned root, say.
//!
//! Before this, both commands repointed the unit's `current_path` to
//! whichever directory they met last, so a restored copy silently became
//! the live unit.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::db::models::Unit;

/// Whether `a` and `b` name the same directory, however each is spelled.
/// Two spellings that cannot both be resolved compare as written.
fn same_directory(a: &str, b: &str) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// Why `dir`, which carries `existing`'s uuid, must not be taken for the
/// unit: `existing`'s recorded directory is another one and still exists.
/// `None` when `dir` is the recorded directory, or when the recorded one is
/// gone (a move, which the caller follows as before).
pub fn copy_refusal(existing: &Unit, dir: &str) -> Option<String> {
    let recorded = existing.current_path.as_deref()?;
    if same_directory(recorded, dir) || !Path::new(recorded).is_dir() {
        return None;
    }
    Some(format!(
        "refused: its .tapectl-unit.toml carries the uuid {} of unit \"{}\", whose \
         directory {recorded} still exists, so this is a copy (a restore, say), not a \
         move. Remove the copy or move it out of the scanned roots; if the unit really \
         moved here, move {recorded} away first",
        existing.uuid, existing.name
    ))
}

/// The uuids carried by more than one of `found`'s directories, each with
/// every directory carrying it, in the order found.
fn duplicated_uuids(found: &[(String, PathBuf)]) -> BTreeMap<String, Vec<PathBuf>> {
    let mut by_uuid: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for (uuid, dir) in found {
        by_uuid.entry(uuid.clone()).or_default().push(dir.clone());
    }
    by_uuid.retain(|_, dirs| dirs.len() > 1);
    by_uuid
}

/// The groups of `found` refused as a whole: every uuid carried by more
/// than one directory, unless one of them is the recorded directory of the
/// unit the catalog knows by that uuid. That one is the unit, and the
/// others are copies [`copy_refusal`] names one by one, so the unit itself
/// is not refused for a copy made of it.
pub fn refused_groups(
    conn: &rusqlite::Connection,
    found: &[(String, PathBuf)],
) -> crate::error::Result<BTreeMap<String, Vec<PathBuf>>> {
    let mut groups = duplicated_uuids(found);
    let mut held = Vec::new();
    for (uuid, dirs) in &groups {
        let Some(unit) = crate::db::queries::get_unit_by_uuid(conn, uuid)? else {
            continue;
        };
        let Some(recorded) = unit.current_path.as_deref() else {
            continue;
        };
        if dirs
            .iter()
            .any(|d| same_directory(recorded, &d.to_string_lossy()))
        {
            held.push(uuid.clone());
        }
    }
    for uuid in held {
        groups.remove(&uuid);
    }
    Ok(groups)
}

/// The refusal for one of the directories that share `uuid`.
pub fn duplicate_uuid_refusal(uuid: &str, dirs: &[PathBuf]) -> String {
    let names: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
    format!(
        "refused: the uuid {uuid} is carried by {} directories ({}); one unit has one \
         directory, so at most one of them is the unit and the others are copies. \
         Remove the copies or move them out of the scanned roots",
        dirs.len(),
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_uuids_carried_twice_are_duplicated() {
        let found = vec![
            ("a".to_string(), PathBuf::from("/r/one")),
            ("b".to_string(), PathBuf::from("/r/two")),
            ("a".to_string(), PathBuf::from("/r/three")),
        ];
        let dup = duplicated_uuids(&found);
        assert_eq!(dup.len(), 1);
        assert_eq!(
            dup["a"],
            vec![PathBuf::from("/r/one"), PathBuf::from("/r/three")]
        );
        let msg = duplicate_uuid_refusal("a", &dup["a"]);
        assert!(msg.contains("/r/one") && msg.contains("/r/three"), "{msg}");
    }
}
