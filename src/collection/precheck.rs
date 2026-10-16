//! The catalog-only refusals `volume write` makes before it touches a
//! backend, a MAM or the drive, which `collection run`'s destination check
//! must make too (issue #475): an unresolved write session, then a recorded
//! seal. `collection run` stages a whole batch before it calls `volume
//! write`, so [`super::plan::destination_budget`] asks them first (the late
//! refusal issue #224 removed).
//!
//! The predicate and its texts live in one place,
//! [`crate::volume::write::precontact_refusal`]; this module only names it
//! for the collection layer, and its tests drive both entry points over one
//! catalog so they cannot drift apart.

use rusqlite::Connection;

use crate::error::Result;

/// [`crate::volume::write::precontact_refusal`]: `None` when the catalog
/// lets the volume through to the tape's own checks.
pub fn refusal(conn: &Connection, label: &str, volume_id: i64) -> Result<Option<String>> {
    crate::volume::write::precontact_refusal(conn, label, volume_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, TapectlPaths};
    use crate::db;
    use rusqlite::params;

    /// `L1`, `initialized`, with one write session of `write_status` and its
    /// seal recorded or not.
    fn catalog(write_status: &str, sealed: bool) -> (Connection, i64) {
        let conn = db::open_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO tenants (id, name, is_operator, status) VALUES (1, 'media', 0, 'active');
             INSERT INTO units (id, uuid, name, tenant_id, checksum_mode, encrypt, status)
                 VALUES (1, 'u-old', 'old', 1, 'mtime_size', 1, 'active');
             INSERT INTO snapshots (id, unit_id, version, status, source_path, file_count, total_size)
                 VALUES (1, 1, 1, 'staged', '/tmp', 1, 10);
             INSERT INTO stage_sets (id, snapshot_id, status, slice_size) VALUES (1, 1, 'staged', 524288);
             INSERT INTO volumes (id, label, backend_type, backend_name, capacity_bytes, status)
                 VALUES (1, 'L1', 'lto', 'p', 10485760, 'initialized');",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (1, 1, 1, ?1)",
            params![write_status],
        )
        .unwrap();
        if sealed {
            conn.execute(
                "UPDATE volumes SET sealed_at = datetime('now') WHERE id = 1",
                [],
            )
            .unwrap();
        }
        (conn, 1)
    }

    /// What `volume write` itself says of the catalog, with no device behind
    /// it: refused before the device is named.
    fn volume_write_says(conn: &Connection) -> String {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().join("home"));
        crate::volume::write::volume_write(
            conn,
            &paths,
            &Config::default(),
            "L1",
            "/nonexistent/tapectl-precheck-test-nst",
            512 * 1024,
            false,
            false,
            false,
            false,
            true,
        )
        .unwrap_err()
        .to_string()
    }

    /// The point of the module (issue #475): `collection run` and `volume
    /// write` refuse a volume in the same words. Driven over the same
    /// catalog through both entry points, for every refusal this module
    /// makes, so a change to one text without the other fails here.
    #[test]
    fn the_run_refusals_are_volume_writes_own() {
        for (status, sealed) in [
            ("planned", false),
            ("in_progress", false),
            ("interrupted", false),
            ("interrupted", true),
            ("aborted", true),
        ] {
            let (conn, volume_id) = catalog(status, sealed);
            let ours = refusal(&conn, "L1", volume_id)
                .unwrap()
                .unwrap_or_else(|| panic!("{status}/sealed={sealed}: not refused"));
            assert_eq!(
                ours,
                volume_write_says(&conn),
                "{status}/sealed={sealed}: the two refusals drifted"
            );
        }
    }

    /// The positive control for the test above: a resolved, unsealed volume
    /// is no refusal of this module's, and `volume write` does not name
    /// either of them for it.
    #[test]
    fn a_resolved_unsealed_volume_passes_both_checks() {
        let (conn, volume_id) = catalog("aborted", false);
        assert_eq!(refusal(&conn, "L1", volume_id).unwrap(), None);
        let theirs = volume_write_says(&conn);
        assert!(
            !theirs.contains("unresolved write session") && !theirs.contains("RECORDS its seal"),
            "{theirs}"
        );
    }
}
