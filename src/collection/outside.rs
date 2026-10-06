//! What lies under a collection's root but belongs to no unit (issue #382).
//!
//! `collection sync` turns only real directories at exactly `unit_depth`
//! into units. Everything else down to that depth — a loose file, a symlink,
//! a symlinked directory standing where a unit folder would — is in no unit,
//! so it is never staged and never reaches a tape, and the catalog cannot
//! show it because it has no row. A clean `collection status` then read as
//! "the whole collection is archived" while it was not. This walk finds those
//! entries so `sync` and `status` can name them and exit non-zero.
//!
//! Inside a unit folder nothing is reported: the unit's own walk archives
//! what is there (symlinks included, as symlinks). An entry the collection's
//! `exclude` list matches is not reported either: the operator excluded it
//! on purpose.

use std::path::Path;

use walkdir::WalkDir;

use crate::config::CollectionConfig;
use crate::staging::exclude::Excludes;

/// What kind of entry lies outside every unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutsideKind {
    /// A regular file.
    File,
    /// A symlink to a file, or a dangling one.
    Symlink,
    /// A symlink to a directory — at `unit_depth` it stands where a unit
    /// folder would, and `sync` does not follow it.
    SymlinkedDirectory,
    /// A FIFO, socket or device node.
    Special,
}

impl OutsideKind {
    /// The word an operator reads, in text and JSON alike.
    pub fn label(self) -> &'static str {
        match self {
            OutsideKind::File => "file",
            OutsideKind::Symlink => "symlink",
            OutsideKind::SymlinkedDirectory => "symlinked directory",
            OutsideKind::Special => "special file",
        }
    }
}

/// One entry under a collection's root that belongs to no unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutsideEntry {
    /// Relative to the collection root.
    pub path: String,
    pub kind: OutsideKind,
}

/// Every entry from the root down to `unit_depth` that is not a real
/// directory, in path order — none of them is inside a unit, so none is
/// archived. Real directories above `unit_depth` are the collection's own
/// structure, and real directories at it are the units. Unreadable
/// directories are skipped, as `sync`'s own walk skips them.
pub fn entries_outside_units(root: &Path, lib: &CollectionConfig) -> Vec<OutsideEntry> {
    let depth = lib.unit_depth.max(1);
    let excludes = Excludes::new(root, &lib.exclude);

    let mut out: Vec<OutsideEntry> = WalkDir::new(root)
        .follow_links(false)
        .min_depth(1)
        .max_depth(depth)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let ft = e.file_type();
            if ft.is_dir() {
                return None;
            }
            let path = e.path();
            if excludes.excludes_file(path)
                || (e.depth() == depth && excludes.excludes_unit_dir(path))
            {
                return None;
            }
            let kind = if ft.is_symlink() {
                // `metadata` follows the link: a directory behind it is the
                // case `sync` most plausibly meant to register and did not.
                match std::fs::metadata(path) {
                    Ok(m) if m.is_dir() => OutsideKind::SymlinkedDirectory,
                    _ => OutsideKind::Symlink,
                }
            } else if ft.is_file() {
                OutsideKind::File
            } else {
                OutsideKind::Special
            };
            let rel = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned();
            Some(OutsideEntry { path: rel, kind })
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn lib(root: &Path, unit_depth: usize, exclude: &[&str]) -> CollectionConfig {
        CollectionConfig {
            name: "lib".into(),
            root: root.to_string_lossy().into_owned(),
            tenant: "t".into(),
            unit_depth,
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
            archive_set: None,
            dotfiles: true,
        }
    }

    fn found(root: &Path, lib: &CollectionConfig) -> Vec<(String, OutsideKind)> {
        entries_outside_units(root, lib)
            .into_iter()
            .map(|e| (e.path, e.kind))
            .collect()
    }

    /// The positive control first: a collection whose every entry is a unit
    /// folder or inside one reports nothing — including a loose file and a
    /// symlink INSIDE a unit, which the unit's own walk archives.
    #[test]
    fn nothing_is_reported_when_everything_is_inside_a_unit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("shows/a")).unwrap();
        std::fs::write(root.join("shows/a/ep1.mkv"), b"x").unwrap();
        symlink(root.join("shows/a/ep1.mkv"), root.join("shows/a/link")).unwrap();
        assert_eq!(found(root, &lib(root, 2, &[])), vec![]);
    }

    /// `unit_depth` 2 with a loose file at depth 1: it is above every unit.
    #[test]
    fn a_loose_file_above_unit_depth_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("shows/a")).unwrap();
        std::fs::write(root.join("shows/notes.txt"), b"x").unwrap();
        assert_eq!(
            found(root, &lib(root, 2, &[])),
            vec![("shows/notes.txt".to_string(), OutsideKind::File)]
        );
    }

    /// A symlinked directory at `unit_depth` stands where a unit folder
    /// would; `sync` does not follow it, so its content is in no unit.
    #[test]
    fn a_symlinked_directory_at_unit_depth_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("a")).unwrap();
        symlink(elsewhere.path(), root.join("b")).unwrap();
        assert_eq!(
            found(root, &lib(root, 1, &[])),
            vec![("b".to_string(), OutsideKind::SymlinkedDirectory)]
        );
    }

    /// A symlink at the root, with `unit_depth` 2: above every unit. And a
    /// regular file at the root with `unit_depth` 1 is AT unit depth but is
    /// no unit folder — in no unit either.
    #[test]
    fn a_symlink_and_a_file_at_the_root_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("shows/a")).unwrap();
        std::fs::write(root.join("README"), b"x").unwrap();
        symlink(root.join("README"), root.join("readme-link")).unwrap();
        symlink(root.join("nowhere"), root.join("dangling")).unwrap();
        assert_eq!(
            found(root, &lib(root, 2, &[])),
            vec![
                ("README".to_string(), OutsideKind::File),
                ("dangling".to_string(), OutsideKind::Symlink),
                ("readme-link".to_string(), OutsideKind::Symlink),
            ]
        );
        assert_eq!(
            found(root, &lib(root, 1, &[])),
            vec![
                ("README".to_string(), OutsideKind::File),
                ("dangling".to_string(), OutsideKind::Symlink),
                ("readme-link".to_string(), OutsideKind::Symlink),
            ]
        );
    }

    /// The collection's `exclude` list silences what it matches: the
    /// operator excluded it on purpose — a plain pattern by name, a
    /// directory pattern for everything under that directory.
    #[test]
    fn an_excluded_entry_is_not_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("shows/a")).unwrap();
        std::fs::create_dir_all(root.join("tmp")).unwrap();
        std::fs::write(root.join(".DS_Store"), b"x").unwrap();
        std::fs::write(root.join("tmp/scratch"), b"x").unwrap();
        std::fs::write(root.join("keep.txt"), b"x").unwrap();
        let l = lib(root, 2, &[".DS_Store", "tmp/"]);
        assert_eq!(
            found(root, &l),
            vec![("keep.txt".to_string(), OutsideKind::File)]
        );
    }
}
