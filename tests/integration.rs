//! Integration tests for tapectl.
//! These test the full command flow using in-memory or temp databases.

use std::path::PathBuf;

use rusqlite::Connection;
use tempfile::TempDir;

/// Set up a temp directory with initialized tapectl database.
fn setup() -> (TempDir, Connection, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();
    let db_path = home.join("tapectl.db");
    let keys_dir = home.join("keys");
    let config_path = home.join("config.toml");

    std::fs::create_dir_all(&keys_dir).unwrap();

    // Write minimal config
    std::fs::write(
        &config_path,
        r#"
[dar]
binary = "/usr/bin/dar"

[staging]
directory = "/tmp/tapectl-test-staging"

[defaults]
slice_size = "100M"
compression = "none"
hash = "sha256"
checksum_mode = "mtime_size"
encrypt = true
preserve_xattrs = true
preserve_acls = true
preserve_fsa = true
min_copies_for_tape_only = 2
min_locations_for_tape_only = 2
"#,
    )
    .unwrap();

    let conn = tapectl_test_db(&db_path);
    (tmp, conn, home)
}

fn tapectl_test_db(path: &std::path::Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
        .unwrap();
    // Run migration
    let schema = include_str!("../src/db/migrations/001_initial.sql");
    conn.execute_batch(schema).unwrap();
    conn
}

// ── Tenant Tests ──

#[test]
fn test_tenant_crud() {
    let (_tmp, conn, _home) = setup();

    // Insert
    conn.execute(
        "INSERT INTO tenants (name, description, is_operator, status) VALUES ('alice', 'Test', 0, 'active')",
        [],
    ).unwrap();
    let id: i64 = conn.last_insert_rowid();
    assert!(id > 0);

    // Read
    let name: String = conn
        .query_row("SELECT name FROM tenants WHERE id = ?1", [id], |r| r.get(0))
        .unwrap();
    assert_eq!(name, "alice");

    // List
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tenants WHERE status = 'active'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);

    // Delete (soft)
    conn.execute("UPDATE tenants SET status = 'deleted' WHERE id = ?1", [id])
        .unwrap();
    let status: String = conn
        .query_row("SELECT status FROM tenants WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(status, "deleted");
}

// ── Unit Tests ──

#[test]
fn test_unit_crud() {
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
        [],
    )
    .unwrap();
    let tenant_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
         VALUES ('test-uuid', 'tv/show/s01', ?1, 'mtime_size', 1, 'active')",
        [tenant_id],
    )
    .unwrap();
    let unit_id = conn.last_insert_rowid();

    let name: String = conn
        .query_row("SELECT name FROM units WHERE id = ?1", [unit_id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(name, "tv/show/s01");

    // Tags
    conn.execute("INSERT OR IGNORE INTO tags (name) VALUES ('drama')", [])
        .unwrap();
    let tag_id: i64 = conn
        .query_row("SELECT id FROM tags WHERE name = 'drama'", [], |r| r.get(0))
        .unwrap();
    conn.execute(
        "INSERT INTO unit_tags (unit_id, tag_id) VALUES (?1, ?2)",
        [unit_id, tag_id],
    )
    .unwrap();

    let tag_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM unit_tags WHERE unit_id = ?1",
            [unit_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tag_count, 1);
}

// ── Snapshot Tests ──

#[test]
fn test_snapshot_lifecycle() {
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
        [],
    )
    .unwrap();
    let tid = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
         VALUES ('u1', 'test-unit', ?1, 'mtime_size', 1, 'active')",
        [tid],
    )
    .unwrap();
    let uid = conn.last_insert_rowid();

    // Create snapshot
    conn.execute(
        "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path, total_size, file_count)
         VALUES (?1, 1, 'full', 'created', '/tmp/test', 1000, 10)",
        [uid],
    )
    .unwrap();
    let snap_id = conn.last_insert_rowid();

    // Lifecycle: created -> staged -> current -> superseded -> reclaimable -> purged
    for status in &["staged", "current", "superseded", "reclaimable", "purged"] {
        conn.execute(
            "UPDATE snapshots SET status = ?1 WHERE id = ?2",
            rusqlite::params![status, snap_id],
        )
        .unwrap();
        let actual: String = conn
            .query_row(
                "SELECT status FROM snapshots WHERE id = ?1",
                [snap_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(&actual, status);
    }
}

// ── Archive Set Tests ──

#[test]
fn test_archive_set_crud() {
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO archive_sets (name, min_copies, required_locations, encrypt, checksum_mode)
         VALUES ('critical', 3, '[\"home\",\"offsite\"]', 1, 'sha256')",
        [],
    )
    .unwrap();
    let as_id = conn.last_insert_rowid();

    let min_copies: i64 = conn
        .query_row(
            "SELECT min_copies FROM archive_sets WHERE id = ?1",
            [as_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(min_copies, 3);

    // Edit
    conn.execute(
        "UPDATE archive_sets SET min_copies = 5 WHERE id = ?1",
        [as_id],
    )
    .unwrap();
    let updated: i64 = conn
        .query_row(
            "SELECT min_copies FROM archive_sets WHERE id = ?1",
            [as_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(updated, 5);
}

// ── Volume & Write Tests ──

#[test]
fn test_volume_write_positions() {
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
        [],
    )
    .unwrap();
    let tid = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
         VALUES ('u1', 'unit1', ?1, 'mtime_size', 1, 'active')",
        [tid],
    )
    .unwrap();
    let uid = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
         VALUES (?1, 1, 'full', 'current', '/tmp')",
        [uid],
    )
    .unwrap();
    let snap_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 104857600)",
        [snap_id],
    )
    .unwrap();
    let ss_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted)
         VALUES (?1, 1, 1000, 1100, 'abc123', 'def456')",
        [ss_id],
    ).unwrap();
    let slice_id = conn.last_insert_rowid();

    // Volume
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES ('L6-0001', 'lto', 'primary', 'LTO-6', 2500000000000, 'active')",
        [],
    )
    .unwrap();
    let vol_id = conn.last_insert_rowid();

    // Write
    conn.execute(
        "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
         VALUES (?1, ?2, ?3, 'completed')",
        rusqlite::params![ss_id, snap_id, vol_id],
    )
    .unwrap();
    let write_id = conn.last_insert_rowid();

    // Write position
    conn.execute(
        "INSERT INTO write_positions (write_id, stage_slice_id, position, status, sha256_on_volume)
         VALUES (?1, ?2, '4', 'written', 'def456')",
        rusqlite::params![write_id, slice_id],
    )
    .unwrap();

    // Query: find copies of unit
    let copy_count: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT w.volume_id)
             FROM writes w
             JOIN stage_sets ss ON ss.id = w.stage_set_id
             JOIN snapshots s ON s.id = ss.snapshot_id
             WHERE s.unit_id = ?1 AND s.status = 'current' AND w.status = 'completed'",
            [uid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(copy_count, 1);
}

// ── Location & Movement Tests ──

#[test]
fn test_location_and_volume_movement() {
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO locations (name, description) VALUES ('home', 'Home rack')",
        [],
    )
    .unwrap();
    let loc1 = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO locations (name, description) VALUES ('offsite', 'Parents house')",
        [],
    )
    .unwrap();
    let loc2 = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status, location_id)
         VALUES ('L6-0001', 'lto', 'primary', 'LTO-6', 2500000000000, 'active', ?1)",
        [loc1],
    )
    .unwrap();
    let vol_id = conn.last_insert_rowid();

    // Move
    conn.execute(
        "INSERT INTO volume_movements (volume_id, from_location, to_location) VALUES (?1, ?2, ?3)",
        rusqlite::params![vol_id, loc1, loc2],
    )
    .unwrap();
    conn.execute(
        "UPDATE volumes SET location_id = ?1 WHERE id = ?2",
        rusqlite::params![loc2, vol_id],
    )
    .unwrap();

    let new_loc: i64 = conn
        .query_row(
            "SELECT location_id FROM volumes WHERE id = ?1",
            [vol_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(new_loc, loc2);
}

// ── Cartridge Tests ──

#[test]
fn test_cartridge_lifecycle() {
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status)
         VALUES ('L6-0001', 'LTO-6', 2500000000000, 'available')",
        [],
    )
    .unwrap();
    let cart_id = conn.last_insert_rowid();

    // Use it
    conn.execute(
        "UPDATE cartridges SET status = 'in_use' WHERE id = ?1",
        [cart_id],
    )
    .unwrap();

    // Mark for erase
    conn.execute(
        "UPDATE cartridges SET status = 'pending_erase' WHERE id = ?1",
        [cart_id],
    )
    .unwrap();

    // Erase and mark available
    conn.execute(
        "UPDATE cartridges SET status = 'available' WHERE id = ?1",
        [cart_id],
    )
    .unwrap();

    let status: String = conn
        .query_row(
            "SELECT status FROM cartridges WHERE id = ?1",
            [cart_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "available");
}

// ── Event Audit Trail Tests ──

#[test]
fn test_event_logging() {
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO events (entity_type, entity_id, entity_label, action)
         VALUES ('unit', 1, 'test-unit', 'created')",
        [],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO events (entity_type, entity_id, entity_label, action, field, old_value, new_value)
         VALUES ('unit', 1, 'test-unit', 'renamed', 'name', 'old-name', 'new-name')",
        [],
    )
    .unwrap();

    let event_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE entity_type = 'unit'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(event_count, 2);
}

// ── Compaction Candidate Query Test ──

#[test]
fn test_compaction_candidate_query() {
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
        [],
    )
    .unwrap();
    let tid = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
         VALUES ('u1', 'unit1', ?1, 'mtime_size', 1, 'active')",
        [tid],
    )
    .unwrap();
    let uid = conn.last_insert_rowid();

    // Create a current snapshot and a reclaimable one
    conn.execute(
        "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
         VALUES (?1, 1, 'full', 'reclaimable', '/tmp')",
        [uid],
    )
    .unwrap();
    let snap1 = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
         VALUES (?1, 2, 'full', 'current', '/tmp')",
        [uid],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 104857600)",
        [snap1],
    )
    .unwrap();
    let ss1 = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted)
         VALUES (?1, 1, 1000, 1100, 'abc', 'def')",
        [ss1],
    ).unwrap();

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, bytes_written, status)
         VALUES ('L6-0001', 'lto', 'primary', 'LTO-6', 2500000000000, 10000, 'active')",
        [],
    )
    .unwrap();
    let vol_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
         VALUES (?1, ?2, ?3, 'completed')",
        rusqlite::params![ss1, snap1, vol_id],
    )
    .unwrap();

    // Compaction candidate query (from design)
    let candidates: Vec<(String, f64)> = {
        let mut stmt = conn.prepare(
            "SELECT v.label,
                    CAST(SUM(CASE WHEN s.status NOT IN ('reclaimable','purged') THEN ss.encrypted_bytes ELSE 0 END) AS REAL) / v.bytes_written as utilization
             FROM volumes v
             JOIN writes w ON w.volume_id = v.id AND w.status = 'completed'
             JOIN stage_sets sts ON sts.id = w.stage_set_id
             JOIN snapshots s ON s.id = sts.snapshot_id
             JOIN stage_slices ss ON ss.stage_set_id = sts.id
             WHERE v.status IN ('active','full')
             GROUP BY v.id
             HAVING utilization < 0.50",
        ).unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    };

    // The reclaimable snapshot means 0 live bytes → utilization = 0.0 < 0.50
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].0, "L6-0001");
}

// ── Schema Integrity Tests ──

#[test]
fn test_schema_has_all_tables() {
    let (_tmp, conn, _home) = setup();

    let expected_tables = vec![
        "meta",
        "tenants",
        "encryption_keys",
        "archive_sets",
        "units",
        "tags",
        "unit_tags",
        "unit_path_history",
        "snapshots",
        "manifests",
        "manifest_entries",
        "files",
        "stage_sets",
        "stage_slices",
        "locations",
        "cartridges",
        "cartridge_volumes",
        "volumes",
        "volume_movements",
        "writes",
        "write_positions",
        "verification_sessions",
        "verification_results",
        "health_logs",
        "events",
    ];

    for table in &expected_tables {
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='{table}'"
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "table '{}' not found", table);
    }
}

#[test]
fn test_foreign_keys_enforced() {
    let (_tmp, conn, _home) = setup();

    // Trying to insert a unit with nonexistent tenant_id should fail
    let result = conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
         VALUES ('bad-uuid', 'bad-unit', 99999, 'mtime_size', 1, 'active')",
        [],
    );
    assert!(result.is_err(), "foreign key constraint should have failed");
}

#[test]
fn test_unique_constraints() {
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('alice', 0, 'active')",
        [],
    )
    .unwrap();

    // Duplicate name should fail
    let result = conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('alice', 0, 'active')",
        [],
    );
    assert!(result.is_err(), "unique constraint should have failed");
}
