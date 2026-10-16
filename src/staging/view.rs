//! Where a run reads a unit's bytes from, when that is not where the unit
//! lives (ADR-0012 2026-10-16 item 75, issue #455).
//!
//! A unit is registered at a real path (`units.current_path`), and every path
//! the catalog records stays that one: `snapshots.source_path`, the `files`
//! rows' relative paths, `stage_sets.dar_command`. A [`SourceView`] only
//! redirects the READS: `collection run`, whose `snapshot_command` took a
//! read-only snapshot of the collection's root, hands each unit's snapshot and
//! stage a view whose [`read_path`](SourceView::read_path) is the same folder
//! inside that snapshot. `snapshot create` and `stage create` on their own
//! pass no view and read the unit where it lives.
//!
//! The view is an explicit argument, not ambient state: a stage runs on a
//! worker thread when `[staging] jobs` is above 1, and a value held anywhere
//! but in the call would not follow it.

use std::path::{Path, PathBuf};

use crate::error::{Result, TapectlError};

/// The live root a set of units is registered under, and the read-only copy
/// of it that stands in for it during one run. Both are absolute and
/// canonical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceView {
    live_root: PathBuf,
    read_root: PathBuf,
}

impl SourceView {
    pub fn new(live_root: impl Into<PathBuf>, read_root: impl Into<PathBuf>) -> Self {
        Self {
            live_root: live_root.into(),
            read_root: read_root.into(),
        }
    }

    /// The path to READ for the unit recorded at `recorded`: its folder under
    /// the live root, taken to the same place under the snapshot.
    ///
    /// A unit that is not under the live root is refused, never read from its
    /// live path instead: a run that was promised a snapshot must not
    /// quietly stage the living source.
    pub fn read_path(&self, recorded: &str) -> Result<PathBuf> {
        let relative = Path::new(recorded)
            .strip_prefix(&self.live_root)
            .map_err(|_| {
                TapectlError::Other(format!(
                    "unit path {recorded} is not under the collection root {}, so it has no \
                     counterpart in the snapshot at {}",
                    self.live_root.display(),
                    self.read_root.display()
                ))
            })?;
        Ok(self.read_root.join(relative))
    }

    /// The live root the view stands in for.
    pub fn live_root(&self) -> &Path {
        &self.live_root
    }

    /// The snapshot of it.
    pub fn read_root(&self) -> &Path {
        &self.read_root
    }
}

/// The path to read for `recorded`: itself when there is no view, else its
/// counterpart in the snapshot ([`SourceView::read_path`]). A path that is
/// not text cannot be recorded in the catalog, so it cannot be read here.
pub(crate) fn read_path_of(view: Option<&SourceView>, recorded: &str) -> Result<String> {
    let Some(view) = view else {
        return Ok(recorded.to_string());
    };
    let read = view.read_path(recorded)?;
    read.into_os_string().into_string().map_err(|p| {
        TapectlError::Other(format!(
            "the snapshot path {} is not valid UTF-8",
            p.to_string_lossy()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_unit_maps_to_the_same_folder_under_the_snapshot() {
        let view = SourceView::new("/srv/media", "/mnt/snap/media");
        assert_eq!(
            view.read_path("/srv/media/movies/Brazil.1985").unwrap(),
            PathBuf::from("/mnt/snap/media/movies/Brazil.1985")
        );
        assert_eq!(
            view.read_path("/srv/media").unwrap(),
            PathBuf::from("/mnt/snap/media")
        );
    }

    /// The string-prefix trap: `/srv/media2` shares a prefix with
    /// `/srv/media` and is not under it. It is refused, not read live.
    #[test]
    fn a_path_outside_the_root_is_refused_not_read_from_where_it_lives() {
        let view = SourceView::new("/srv/media", "/mnt/snap/media");
        for outside in ["/srv/media2/x", "/srv/other", "/srv"] {
            let err = view.read_path(outside).unwrap_err().to_string();
            assert!(err.contains("not under the collection root"), "{err}");
        }
    }

    #[test]
    fn no_view_reads_the_recorded_path() {
        assert_eq!(read_path_of(None, "/srv/media/a").unwrap(), "/srv/media/a");
        let view = SourceView::new("/srv/media", "/mnt/snap");
        assert_eq!(
            read_path_of(Some(&view), "/srv/media/a").unwrap(),
            "/mnt/snap/a"
        );
    }
}
