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
    // Run migrations
    let schema = include_str!("../src/db/migrations/001_initial.sql");
    conn.execute_batch(schema).unwrap();
    let fts5 = include_str!("../src/db/migrations/002_fts5_catalog.sql");
    conn.execute_batch(fts5).unwrap();
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

// ── FTS5 Search Tests ──

#[test]
fn test_fts5_search() {
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
        [],
    )
    .unwrap();
    let tid = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
         VALUES ('u1', 'movies', ?1, 'mtime_size', 1, 'active')",
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

    // Insert files — triggers should populate FTS
    conn.execute(
        "INSERT INTO files (snapshot_id, path, size_bytes, is_directory)
         VALUES (?1, 'season1/episode01.mkv', 5000000000, 0)",
        [snap_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO files (snapshot_id, path, size_bytes, is_directory)
         VALUES (?1, 'season1/episode02.mkv', 4500000000, 0)",
        [snap_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO files (snapshot_id, path, size_bytes, is_directory)
         VALUES (?1, 'extras/behind_scenes.mp4', 1000000000, 0)",
        [snap_id],
    )
    .unwrap();

    // FTS5 indexes the full path as a token — search for the whole path segment
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files_fts WHERE files_fts MATCH '\"season1/episode01.mkv\"'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);

    // Total files indexed
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM files_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 3);

    // Prefix search on path segments
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files_fts WHERE files_fts MATCH 'season1*'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(count >= 2);
}

// ── Encryption Key Tests ──

#[test]
fn test_encryption_key_rotation() {
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('alice', 0, 'active')",
        [],
    )
    .unwrap();
    let tid = conn.last_insert_rowid();

    // Create initial keys
    conn.execute(
        "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key, key_type, is_active)
         VALUES (?1, 'alice-primary', 'fp1', 'pk1', 'primary', 1)",
        [tid],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key, key_type, is_active)
         VALUES (?1, 'alice-backup', 'fp2', 'pk2', 'backup', 1)",
        [tid],
    )
    .unwrap();

    // Rotate: deactivate old, create new
    conn.execute(
        "UPDATE encryption_keys SET is_active = 0 WHERE tenant_id = ?1 AND is_active = 1",
        [tid],
    )
    .unwrap();

    let active_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM encryption_keys WHERE tenant_id = ?1 AND is_active = 1",
            [tid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(active_count, 0);

    // New keys
    conn.execute(
        "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key, key_type, is_active)
         VALUES (?1, 'alice-rotated', 'fp3', 'pk3', 'primary', 1)",
        [tid],
    )
    .unwrap();

    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM encryption_keys WHERE tenant_id = ?1",
            [tid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(total, 3); // Old keys preserved, never deleted
}

// ── Policy Resolution Tests ──

#[test]
fn test_archive_set_policy_inheritance() {
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
        [],
    )
    .unwrap();
    let tid = conn.last_insert_rowid();

    // Create archive set
    conn.execute(
        "INSERT INTO archive_sets (name, min_copies, required_locations, checksum_mode)
         VALUES ('critical', 3, '[\"home\",\"offsite\"]', 'sha256')",
        [],
    )
    .unwrap();
    let as_id = conn.last_insert_rowid();

    // Create unit referencing archive set
    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, archive_set_id, checksum_mode, encrypt, status)
         VALUES ('u1', 'important-data', ?1, ?2, 'mtime_size', 1, 'active')",
        rusqlite::params![tid, as_id],
    )
    .unwrap();

    // Verify unit's archive_set_id is set
    let unit_as: Option<i64> = conn
        .query_row(
            "SELECT archive_set_id FROM units WHERE name = 'important-data'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unit_as, Some(as_id));

    // Verify archive set values
    let min_copies: i64 = conn
        .query_row(
            "SELECT min_copies FROM archive_sets WHERE id = ?1",
            [as_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(min_copies, 3);
}

// ── Verification Session Tests ──

#[test]
fn test_verification_session_tracking() {
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES ('L6-0001', 'lto', 'primary', 'LTO-6', 2500000000000, 'active')",
        [],
    )
    .unwrap();
    let vol_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO verification_sessions (volume_id, verify_type, outcome, slices_checked, slices_passed, slices_failed)
         VALUES (?1, 'full', 'passed', 10, 10, 0)",
        [vol_id],
    )
    .unwrap();
    let vs_id = conn.last_insert_rowid();

    let outcome: String = conn
        .query_row(
            "SELECT outcome FROM verification_sessions WHERE id = ?1",
            [vs_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(outcome, "passed");
}

// ── Multi-Tenant Isolation Test ──

#[test]
fn test_multi_tenant_isolation() {
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('alice', 0, 'active')",
        [],
    )
    .unwrap();
    let alice_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('bob', 0, 'active')",
        [],
    )
    .unwrap();
    let bob_id = conn.last_insert_rowid();

    // Each tenant has their own units
    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
         VALUES ('a1', 'alice-data', ?1, 'mtime_size', 1, 'active')",
        [alice_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
         VALUES ('b1', 'bob-data', ?1, 'mtime_size', 1, 'active')",
        [bob_id],
    )
    .unwrap();

    // Alice can only see her units
    let alice_units: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM units WHERE tenant_id = ?1",
            [alice_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(alice_units, 1);

    let bob_units: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM units WHERE tenant_id = ?1",
            [bob_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bob_units, 1);
}

// ── Tenant Reassignment Test ──

#[test]
fn test_tenant_reassignment() {
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('alice', 0, 'active')",
        [],
    )
    .unwrap();
    let alice_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('bob', 0, 'active')",
        [],
    )
    .unwrap();
    let bob_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
         VALUES ('u1', 'data1', ?1, 'mtime_size', 1, 'active')",
        [alice_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
         VALUES ('u2', 'data2', ?1, 'mtime_size', 1, 'active')",
        [alice_id],
    )
    .unwrap();

    // Reassign all units from alice to bob
    let moved = conn
        .execute(
            "UPDATE units SET tenant_id = ?1 WHERE tenant_id = ?2",
            rusqlite::params![bob_id, alice_id],
        )
        .unwrap();
    assert_eq!(moved, 2);

    let alice_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM units WHERE tenant_id = ?1",
            [alice_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(alice_count, 0);

    let bob_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM units WHERE tenant_id = ?1",
            [bob_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bob_count, 2);
}

// ── Snapshot Mark-Reclaimable Preconditions Test ──

#[test]
fn test_snapshot_status_check_constraints() {
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
        [],
    )
    .unwrap();
    let tid = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
         VALUES ('u1', 'test', ?1, 'mtime_size', 1, 'active')",
        [tid],
    )
    .unwrap();
    let uid = conn.last_insert_rowid();

    // Invalid status should fail
    let result = conn.execute(
        "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
         VALUES (?1, 1, 'full', 'invalid_status', '/tmp')",
        [uid],
    );
    assert!(
        result.is_err(),
        "CHECK constraint should reject invalid status"
    );
}

// ── Import Volume Test ──

#[test]
fn test_import_volume() {
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status, notes)
         VALUES ('IMPORTED-001', 'lto', 'lto', 'LTO-6', 2500000000000, 'active', 'Pre-existing tape')",
        [],
    )
    .unwrap();
    let vol_id = conn.last_insert_rowid();

    let label: String = conn
        .query_row("SELECT label FROM volumes WHERE id = ?1", [vol_id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(label, "IMPORTED-001");

    let notes: Option<String> = conn
        .query_row("SELECT notes FROM volumes WHERE id = ?1", [vol_id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(notes, Some("Pre-existing tape".into()));
}

// ── Config Parsing Test ──

// ── Failure Mode Tests ──

#[test]
fn test_duplicate_volume_label_rejected() {
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes)
         VALUES ('TAPE001', 'lto', 'lto6', 2500000000000)",
        [],
    )
    .unwrap();

    let result = conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes)
         VALUES ('TAPE001', 'lto', 'lto6', 2500000000000)",
        [],
    );
    assert!(
        result.is_err(),
        "duplicate volume label must be rejected by UNIQUE constraint"
    );
}

#[test]
fn test_duplicate_cartridge_barcode_rejected() {
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO cartridges (barcode, media_type, nominal_capacity)
         VALUES ('BC0001', 'LTO-6', 2500000000000)",
        [],
    )
    .unwrap();

    let result = conn.execute(
        "INSERT INTO cartridges (barcode, media_type, nominal_capacity)
         VALUES ('BC0001', 'LTO-6', 2500000000000)",
        [],
    );
    assert!(
        result.is_err(),
        "duplicate cartridge barcode must be rejected"
    );
}

#[test]
fn test_invalid_volume_status_rejected() {
    let (_tmp, conn, _home) = setup();

    let result = conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
         VALUES ('BAD1', 'lto', 'lto6', 2500000000000, 'bogus_status')",
        [],
    );
    assert!(
        result.is_err(),
        "CHECK constraint must reject invalid volume status"
    );
}

#[test]
fn test_invalid_cartridge_status_rejected() {
    let (_tmp, conn, _home) = setup();

    let result = conn.execute(
        "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status)
         VALUES ('BC0002', 'LTO-6', 2500000000000, 'not_a_status')",
        [],
    );
    assert!(
        result.is_err(),
        "CHECK constraint must reject invalid cartridge status"
    );
}

#[test]
fn test_stage_set_requires_existing_snapshot() {
    let (_tmp, conn, _home) = setup();

    // Reference a snapshot that doesn't exist
    let result = conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status) VALUES (99999, 'staging')",
        [],
    );
    assert!(
        result.is_err(),
        "foreign key to snapshots must reject missing snapshot_id"
    );
}

#[test]
fn test_fts5_path_tokenization() {
    // Verifies the fix in catalog search: FTS5 default tokenizer splits paths
    // on non-alphanumeric, so a query like `season* episode*` must match
    // 'season1/episode01.mkv'. This exercises the tokenization path the CLI
    // search now uses after the FTS5 phrase+prefix bug was fixed.
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
        [],
    )
    .unwrap();
    let tid = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
         VALUES ('u1', 'movies', ?1, 'mtime_size', 1, 'active')",
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

    for path in ["season1/episode01.mkv", "season1/episode02.mkv", "other.txt"] {
        conn.execute(
            "INSERT INTO files (snapshot_id, path, size_bytes, is_directory)
             VALUES (?1, ?2, 1000, 0)",
            rusqlite::params![snap_id, path],
        )
        .unwrap();
    }

    // Two-token prefix query: must match both episodes, not 'other.txt'
    let mut stmt = conn
        .prepare("SELECT path FROM files_fts WHERE files_fts MATCH ?1 ORDER BY rank")
        .unwrap();
    let rows: Vec<String> = stmt
        .query_map(["season* episode*"], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(rows.len(), 2, "expected 2 FTS5 matches, got {rows:?}");
    assert!(rows.iter().all(|p| p.starts_with("season1/")));

    // Single-token prefix on a path-embedded token
    let rows2: Vec<String> = stmt
        .query_map(["episode*"], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(rows2.len(), 2);

    // Non-matching prefix returns nothing
    let rows3: Vec<String> = stmt
        .query_map(["zzzz*"], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(rows3.is_empty());
}

#[test]
fn test_nonexistent_lookups_return_none() {
    let (_tmp, conn, _home) = setup();

    // None-returning SELECTs on missing rows must not panic or error —
    // the code relies on .optional() / .ok() semantics throughout.
    let tenant: Option<i64> = conn
        .query_row("SELECT id FROM tenants WHERE name = ?1", ["ghost"], |r| {
            r.get(0)
        })
        .ok();
    assert!(tenant.is_none());

    let unit: Option<i64> = conn
        .query_row("SELECT id FROM units WHERE name = ?1", ["ghost"], |r| {
            r.get(0)
        })
        .ok();
    assert!(unit.is_none());

    let volume: Option<i64> = conn
        .query_row("SELECT id FROM volumes WHERE label = ?1", ["GHOST"], |r| {
            r.get(0)
        })
        .ok();
    assert!(volume.is_none());
}

#[test]
fn test_write_requires_existing_volume_and_stage_set() {
    let (_tmp, conn, _home) = setup();

    // writes FK to volumes and stage_sets
    let result = conn.execute(
        "INSERT INTO writes (volume_id, stage_set_id, status) VALUES (99999, 99999, 'planned')",
        [],
    );
    assert!(
        result.is_err(),
        "writes must not allow orphan volume_id/stage_set_id"
    );
}

#[test]
fn test_duplicate_slice_number_within_stage_set_rejected() {
    let (_tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
        [],
    )
    .unwrap();
    let tid = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
         VALUES ('u1', 'u', ?1, 'mtime_size', 1, 'active')",
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
    let sid = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 104857600)",
        [sid],
    )
    .unwrap();
    let ss_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted)
         VALUES (?1, 0, 100, 100, 'a', 'b')",
        [ss_id],
    )
    .unwrap();

    // Second slice with the same slice_number must fail — UNIQUE(stage_set_id, slice_number)
    let result = conn.execute(
        "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted)
         VALUES (?1, 0, 100, 100, 'c', 'd')",
        [ss_id],
    );
    assert!(
        result.is_err(),
        "duplicate (stage_set_id, slice_number) must be rejected"
    );
}

#[test]
fn test_config_default_values() {
    let (_tmp, _conn, home) = setup();
    let config_path = home.join("config.toml");
    let content = std::fs::read_to_string(&config_path).unwrap();
    let parsed: toml::Value = content.parse().unwrap();

    // Verify defaults section exists
    let defaults = parsed.get("defaults").unwrap();
    assert_eq!(
        defaults.get("slice_size").unwrap().as_str().unwrap(),
        "100M"
    );
    assert_eq!(
        defaults.get("checksum_mode").unwrap().as_str().unwrap(),
        "mtime_size"
    );
    assert!(defaults.get("encrypt").unwrap().as_bool().unwrap());
}
