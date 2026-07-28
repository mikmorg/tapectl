//! `library status` (`docs/design/v2-open-questions.md` §11): pending /
//! dirty / missing / under-copied counts.

use rusqlite::{params, Connection};

use crate::config::{Config, LibraryConfig};
use crate::error::Result;

use super::fingerprint::PendingReason;

/// One library's readiness snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LibraryStatus {
    /// Units with no snapshot at all yet.
    pub pending: usize,
    /// Units with a snapshot, but whose on-disk fingerprint has since
    /// changed.
    pub dirty: usize,
    /// Units whose directory has vanished (`library sync` sets this; never
    /// auto-deleted or retired).
    pub missing: usize,
    /// Active units with fewer completed tape copies than their resolved
    /// policy requires.
    pub under_copied: usize,
}

/// Compute one library's status.
pub fn status_for_library(
    conn: &Connection,
    config: &Config,
    lib: &LibraryConfig,
) -> Result<LibraryStatus> {
    let mut status = LibraryStatus::default();

    for p in super::fingerprint::pending_units_for_library(conn, lib)? {
        match p.reason {
            PendingReason::New => status.pending += 1,
            PendingReason::Dirty => status.dirty += 1,
        }
    }

    let root = super::canonical_root(lib)?;
    let tracked = super::units_under_root(conn, &root)?;
    status.missing = tracked.iter().filter(|u| u.status == "missing").count();

    // Under-copied: the same copy-count derivation `cli::audit` uses for
    // its "copy_count" violation check (§11: "from the audit derivations")
    // — reused, not reinvented.
    for unit in tracked.iter().filter(|u| u.status == "active") {
        let resolved = crate::policy::resolve(conn, config, unit);
        let copy_count: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT w.volume_id)
             FROM writes w
             JOIN stage_sets ss ON ss.id = w.stage_set_id
             JOIN snapshots s ON s.id = ss.snapshot_id
             WHERE s.unit_id = ?1 AND s.status = 'current' AND w.status = 'completed'",
            params![unit.id],
            |row| row.get(0),
        )?;
        if copy_count < resolved.min_copies {
            status.under_copied += 1;
        }
    }

    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TapectlPaths;
    use crate::db;

    #[test]
    fn status_counts_a_fresh_sync_as_pending_and_under_copied_not_dirty() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("alpha")).unwrap();

        let lib = LibraryConfig {
            name: "testlib".into(),
            root: root.path().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };
        let paths = TapectlPaths::new(home.path().to_path_buf());
        super::super::sync::sync_library(&conn, &paths, &lib, false).unwrap();

        let config = Config::default();
        let status = status_for_library(&conn, &config, &lib).unwrap();
        assert_eq!(status.pending, 1, "freshly synced unit has no snapshot yet");
        assert_eq!(status.dirty, 0);
        assert_eq!(status.missing, 0);
        assert_eq!(
            status.under_copied, 1,
            "zero completed writes must be under the default min_copies"
        );
    }

    #[test]
    fn status_counts_a_vanished_unit_as_missing() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("alpha")).unwrap();

        let lib = LibraryConfig {
            name: "testlib".into(),
            root: root.path().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };
        let paths = TapectlPaths::new(home.path().to_path_buf());
        super::super::sync::sync_library(&conn, &paths, &lib, false).unwrap();
        std::fs::remove_dir_all(root.path().join("alpha")).unwrap();
        super::super::sync::sync_library(&conn, &paths, &lib, false).unwrap();

        let config = Config::default();
        let status = status_for_library(&conn, &config, &lib).unwrap();
        assert_eq!(status.missing, 1);
        assert_eq!(
            status.pending, 0,
            "a missing unit's vanished directory must not be walked for pending-detection"
        );
    }
}
