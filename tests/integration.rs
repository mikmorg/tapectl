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
    // Use the REAL migration path (db::open runs configure + every registered
    // migration + the orphan sweep). A hand-maintained list of include_str!'d
    // migrations silently rots: it stopped at 003 and broke the moment 004
    // added volumes.uuid. This helper now cannot drift from production.
    tapectl::db::open(path).unwrap()
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

/// Rotate the SAME tenant twice through the real `key rotate` code path (not a
/// SQL simulation — replaces the old masking test per #31/T3). Before the H13
/// fix the second rotation errored on the hardcoded `rotated-primary` filename
/// collision *after* committing the deactivation, stranding the tenant with
/// zero active keys.
#[test]
fn test_key_rotate_twice_keeps_tenant_active() {
    use tapectl::cli::key::KeyCommands;
    let (_tmp, conn, home) = setup();
    let paths = tapectl::config::TapectlPaths::new(home);
    paths.ensure_dirs().unwrap();

    // Real setup: operator + tenant, each with generated primary + backup keys.
    tapectl::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();
    tapectl::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();

    // `key rotate` refuses without a registered escrow recipient (ADR-0005 /
    // T2) — register one first, same as a real operator would have to.
    let gen_escrow = KeyCommands::Generate {
        tenant: None,
        alias: None,
        key_type: "primary".to_string(),
        description: None,
        escrow: true,
    };
    tapectl::cli::key::run(&conn, &paths, &gen_escrow, false).unwrap();

    let active = |name: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM encryption_keys k JOIN tenants t ON t.id = k.tenant_id
             WHERE t.name = ?1 AND k.is_active = 1",
            [name],
            |r| r.get(0),
        )
        .unwrap()
    };
    let total = |name: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM encryption_keys k JOIN tenants t ON t.id = k.tenant_id
             WHERE t.name = ?1",
            [name],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(active("alice"), 2, "initial primary + backup active");

    let rotate = KeyCommands::Rotate {
        tenant: "alice".to_string(),
    };
    tapectl::cli::key::run(&conn, &paths, &rotate, false).unwrap();
    assert_eq!(
        active("alice"),
        2,
        "exactly one active pair after 1st rotation"
    );

    // The H13 reproduction: a second rotation must not strand the tenant.
    tapectl::cli::key::run(&conn, &paths, &rotate, false).unwrap();
    assert_eq!(
        active("alice"),
        2,
        "tenant must keep an active key pair after a second rotation (H13)"
    );
    // Old keys are deactivated, never deleted (decrypt pre-rotation data).
    assert_eq!(total("alice"), 6, "2 initial + 2 + 2 rotated, all retained");
}

// ── Escrow Recipient Tests (ADR-0005 / T2) ──

/// The whole point of the wiring: a fresh ciphertext produced through the
/// real recipient-list helper (`recipient_list_with_escrow`) must be
/// decryptable by the escrow identity alone, not just the tenant it was
/// nominally encrypted for. The escrow identity is generated in-test with
/// the age crate directly (same style as `tests/tenant_isolation.rs`) and
/// never touches tapectl's key store — exactly how the real secret only
/// ever lives on paper.
#[test]
fn escrow_identity_can_decrypt_a_staging_ciphertext() {
    use age::x25519::Identity;
    use std::io::Read;
    use tapectl::db::queries;
    use tapectl::staging::encrypt_data;

    let (_tmp, conn, home) = setup();
    let paths = tapectl::config::TapectlPaths::new(home);
    paths.ensure_dirs().unwrap();

    tapectl::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();
    let op_id: i64 = conn
        .query_row("SELECT id FROM tenants WHERE name = 'op'", [], |r| r.get(0))
        .unwrap();

    // Stand-in for a tenant's own recipient in a slice's base recipient list.
    let alice_pub = Identity::generate().to_public().to_string();

    let escrow_id = Identity::generate();
    let escrow_pub = escrow_id.to_public().to_string();
    queries::insert_escrow_key(&conn, op_id, "op-escrow", &escrow_pub, &escrow_pub, None).unwrap();

    let recipients = queries::recipient_list_with_escrow(&conn, vec![alice_pub.clone()]).unwrap();
    assert!(recipients.contains(&escrow_pub), "escrow key not appended");
    assert!(recipients.contains(&alice_pub), "original recipient lost");

    let plaintext = b"unit contents that only alice+escrow should read";
    let ciphertext = encrypt_data(plaintext, &recipients).unwrap();

    let decryptor = age::Decryptor::new(&ciphertext[..]).unwrap();
    let mut reader = decryptor
        .decrypt(std::iter::once(&escrow_id as &dyn age::Identity))
        .unwrap();
    let mut out = Vec::new();
    reader.read_to_end(&mut out).unwrap();
    assert_eq!(out, plaintext, "escrow identity could not decrypt");
}

#[test]
fn key_rotate_refuses_without_escrow() {
    use tapectl::cli::key::KeyCommands;
    let (_tmp, conn, home) = setup();
    let paths = tapectl::config::TapectlPaths::new(home);
    paths.ensure_dirs().unwrap();

    tapectl::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();

    let active_count = || -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM encryption_keys k JOIN tenants t ON t.id = k.tenant_id
             WHERE t.name = 'alice' AND k.is_active = 1",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(active_count(), 2, "initial primary + backup active");

    let rotate = KeyCommands::Rotate {
        tenant: "alice".to_string(),
    };
    let err = tapectl::cli::key::run(&conn, &paths, &rotate, false).unwrap_err();
    assert!(
        format!("{err}").contains("escrow"),
        "expected an escrow-related refusal, got: {err}"
    );

    // The refusal must be a true no-op.
    assert_eq!(
        active_count(),
        2,
        "rotate must not deactivate anything when it refuses"
    );
}

/// The regression this design protects against: the escrow row lives under
/// the operator tenant, so rotating the OPERATOR's own keys is exactly the
/// case that would deactivate the escrow row too without the dedicated
/// `is_escrow = 0` exclusions in `get_active_keys_for_tenant` and the
/// rotate UPDATE.
#[test]
fn key_rotate_with_escrow_present_leaves_escrow_row_untouched() {
    use tapectl::cli::key::KeyCommands;
    let (_tmp, conn, home) = setup();
    let paths = tapectl::config::TapectlPaths::new(home);
    paths.ensure_dirs().unwrap();

    tapectl::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();

    let gen_escrow = KeyCommands::Generate {
        tenant: None,
        alias: None,
        key_type: "primary".to_string(),
        description: None,
        escrow: true,
    };
    tapectl::cli::key::run(&conn, &paths, &gen_escrow, false).unwrap();

    let (escrow_id, escrow_pubkey_before): (i64, String) = conn
        .query_row(
            "SELECT id, public_key FROM encryption_keys WHERE is_escrow = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let op_id: i64 = conn
        .query_row("SELECT id FROM tenants WHERE name = 'op'", [], |r| r.get(0))
        .unwrap();

    let rotate = KeyCommands::Rotate {
        tenant: "op".to_string(),
    };
    tapectl::cli::key::run(&conn, &paths, &rotate, false).unwrap();

    let (escrow_active, escrow_pubkey_after): (bool, String) = conn
        .query_row(
            "SELECT is_active, public_key FROM encryption_keys WHERE id = ?1",
            [escrow_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(escrow_active, "rotate must never deactivate the escrow row");
    assert_eq!(
        escrow_pubkey_after, escrow_pubkey_before,
        "escrow row must be byte-for-byte untouched by rotation"
    );

    // Meanwhile the operator's own (non-escrow) primary+backup pair WAS
    // rotated: the original 2 deactivated, a fresh 2 active.
    let deactivated_normal: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM encryption_keys
             WHERE tenant_id = ?1 AND is_escrow = 0 AND is_active = 0",
            [op_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        deactivated_normal, 2,
        "operator's original primary+backup should be deactivated by rotation"
    );
    let active_normal: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM encryption_keys
             WHERE tenant_id = ?1 AND is_escrow = 0 AND is_active = 1",
            [op_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        active_normal, 2,
        "operator should have a fresh active primary+backup pair"
    );
}

#[test]
fn second_escrow_registration_refuses() {
    use tapectl::cli::key::KeyCommands;
    let (_tmp, conn, home) = setup();
    let paths = tapectl::config::TapectlPaths::new(home);
    paths.ensure_dirs().unwrap();

    tapectl::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();

    let gen_escrow = KeyCommands::Generate {
        tenant: None,
        alias: None,
        key_type: "primary".to_string(),
        description: None,
        escrow: true,
    };
    tapectl::cli::key::run(&conn, &paths, &gen_escrow, false).unwrap();

    let err = tapectl::cli::key::run(&conn, &paths, &gen_escrow, false).unwrap_err();
    assert!(
        format!("{err}").contains("already registered"),
        "expected an already-registered refusal, got: {err}"
    );

    let escrow_count = || -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM encryption_keys WHERE is_escrow = 1",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(escrow_count(), 1, "must still be exactly one escrow row");

    // `key import --escrow` must refuse under the same rule, even with a
    // syntactically valid, freshly-generated public key.
    let fresh_pub = age::x25519::Identity::generate().to_public().to_string();
    let import = KeyCommands::Import {
        tenant: None,
        alias: None,
        path: fresh_pub,
        key_type: "primary".to_string(),
        escrow: true,
    };
    let err2 = tapectl::cli::key::run(&conn, &paths, &import, false).unwrap_err();
    assert!(
        format!("{err2}").contains("already registered"),
        "expected an already-registered refusal, got: {err2}"
    );
    assert_eq!(
        escrow_count(),
        1,
        "still exactly one escrow row after the import refusal too"
    );
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

    for path in [
        "season1/episode01.mkv",
        "season1/episode02.mkv",
        "other.txt",
    ] {
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

// ── Audit Trail Tests (Phase 4) ──

fn seed_unit(conn: &Connection) -> i64 {
    conn.execute(
        "INSERT INTO tenants (name, description, is_operator, status)
         VALUES ('t1', '', 0, 'active')",
        [],
    )
    .unwrap();
    let tid = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO units (tenant_id, uuid, name, current_path, status)
         VALUES (?1, 'uuid-u1', 'u1', '/tmp/u1', 'active')",
        [tid],
    )
    .unwrap();
    conn.last_insert_rowid()
}

#[test]
fn test_audit_unit_rename_logs_field_change() {
    let (_tmp, conn, _home) = setup();
    let unit_id = seed_unit(&conn);

    tapectl::db::queries::update_unit_name(&conn, unit_id, "u1-renamed").unwrap();

    let (action, field, old, new): (String, String, Option<String>, String) = conn
        .query_row(
            "SELECT action, field, old_value, new_value FROM events
             WHERE entity_type='unit' AND entity_id=?1 AND action='rename'",
            [unit_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(action, "rename");
    assert_eq!(field, "name");
    assert_eq!(old.as_deref(), Some("u1"));
    assert_eq!(new, "u1-renamed");
}

#[test]
fn test_audit_unit_path_change_logs_field_change() {
    let (_tmp, conn, _home) = setup();
    let unit_id = seed_unit(&conn);

    tapectl::db::queries::update_unit_path(&conn, unit_id, "/tmp/u1-new").unwrap();

    let (field, old, new): (String, Option<String>, String) = conn
        .query_row(
            "SELECT field, old_value, new_value FROM events
             WHERE entity_type='unit' AND entity_id=?1 AND action='path_change'",
            [unit_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(field, "current_path");
    assert_eq!(old.as_deref(), Some("/tmp/u1"));
    assert_eq!(new, "/tmp/u1-new");

    // path history table should also have the new row
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM unit_path_history WHERE unit_id = ?1",
            [unit_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn test_audit_crash_recovery_logs_system_event() {
    // Use tapectl::db::open to run migrations, then seed orphaned rows, then
    // re-open to trigger the recovery sweep.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("tapectl.db");
    let conn = tapectl::db::open(&db_path).unwrap();

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES ('V1', 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
        [],
    ).unwrap();
    let vid = conn.last_insert_rowid();
    let unit_id = seed_unit(&conn);
    conn.execute(
        "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
         VALUES (?1, 1, 'created', '/tmp/u1', 0, 0)",
        [unit_id],
    )
    .unwrap();
    let sid = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status, slice_size)
         VALUES (?1, 'staging', 524288)",
        [sid],
    )
    .unwrap();
    let ssid = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO writes (volume_id, stage_set_id, snapshot_id, status)
         VALUES (?1, ?2, ?3, 'in_progress')",
        [vid, ssid, sid],
    )
    .unwrap();
    drop(conn);

    // Re-open via the lib entry point so recover_orphaned_sessions runs.
    let conn = tapectl::db::open(&db_path).unwrap();

    let write_events: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events
             WHERE entity_type='system' AND action='crash_recovery'
               AND field='writes.status'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(write_events, 1);

    let stage_events: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events
             WHERE entity_type='system' AND action='crash_recovery'
               AND field='stage_sets.status'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stage_events, 1);
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

// ── Export (H11 regression, #37) ──

/// With two stage sets staged for the same unit, `export_unit` must select
/// exactly ONE (the latest) — never interleave slices from both, which would
/// produce duplicate slice numbers and an ambiguous, unrestorable directory.
#[test]
fn test_export_selects_single_stage_set() {
    use sha2::{Digest, Sha256};
    let (tmp, conn, _home) = setup();

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

    // Two staged stage sets: v1 (older) and v2 (newer), each with two slices,
    // with real files on disk so the copy succeeds.
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let mk_stage_set = |version: i64, tag: &str| -> i64 {
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
             VALUES (?1, ?2, 'full', 'current', '/tmp')",
            rusqlite::params![uid, version],
        )
        .unwrap();
        let snap = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 104857600)",
            [snap],
        )
        .unwrap();
        let ss = conn.last_insert_rowid();
        for num in 1..=2i64 {
            let fname = format!("{tag}_v{version}.{num}.dar.age");
            let path = staging.join(&fname);
            let bytes = format!("{tag}-slice-{num}").into_bytes();
            std::fs::write(&path, &bytes).unwrap();
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            let sha = format!("{:x}", hasher.finalize());
            conn.execute(
                "INSERT INTO stage_slices
                    (stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted, staging_path)
                 VALUES (?1, ?2, 100, ?3, 'p', ?4, ?5)",
                rusqlite::params![ss, num, bytes.len() as i64, sha, path.to_string_lossy()],
            )
            .unwrap();
        }
        ss
    };
    mk_stage_set(1, "old");
    let ss2 = mk_stage_set(2, "new");

    let dest = tmp.path().join("export-out");
    tapectl::cli::operations::export_unit(&conn, "unit1", dest.to_str().unwrap(), false).unwrap();

    let manifest = std::fs::read_to_string(dest.join("MANIFEST.toml")).unwrap();
    let parsed: toml::Value = manifest.parse().expect("MANIFEST.toml must be valid TOML");
    let export = parsed.get("export").unwrap();

    // Exactly the latest stage set (v2), exactly its two slices — not four.
    assert_eq!(
        export.get("snapshot_version").unwrap().as_integer(),
        Some(2)
    );
    assert_eq!(export.get("stage_set_id").unwrap().as_integer(), Some(ss2));
    assert_eq!(export.get("total_slices").unwrap().as_integer(), Some(2));

    let slices = parsed.get("slices").unwrap().as_array().unwrap();
    assert_eq!(
        slices.len(),
        2,
        "must export one stage set's slices, not both"
    );
    let nums: Vec<i64> = slices
        .iter()
        .map(|s| s.get("number").unwrap().as_integer().unwrap())
        .collect();
    assert_eq!(nums, vec![1, 2], "no duplicate slice numbers");

    // Only the v2 files were copied; no old_* files leaked in.
    let copied: Vec<String> = std::fs::read_dir(&dest)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".dar.age"))
        .collect();
    assert_eq!(copied.len(), 2);
    assert!(
        copied.iter().all(|n| n.starts_with("new_")),
        "leaked old stage set: {copied:?}"
    );

    // The RECOVERY.md verification recipe (#29) must actually work: run the
    // exact `sha256sum -c SHA256SUMS` step an heir would, in the export dir.
    let sums = dest.join("SHA256SUMS");
    assert!(sums.exists(), "SHA256SUMS must be written");
    let status = std::process::Command::new("sha256sum")
        .arg("-c")
        .arg("SHA256SUMS")
        .current_dir(&dest)
        .status()
        .expect("run sha256sum -c");
    assert!(
        status.success(),
        "sha256sum -c SHA256SUMS must pass on the export"
    );

    let recovery = std::fs::read_to_string(dest.join("RECOVERY.md")).unwrap();
    assert!(recovery.contains("sha256sum -c SHA256SUMS"));
    assert!(
        !recovery.contains("ARCHIVE_BASE"),
        "placeholder leaked into RECOVERY.md"
    );
}

// ── Capacity gate (#28, #8 silent-bad-copy fix) ──

/// `volume_write` must refuse before writing when the staged data won't fit the
/// tape — the #8 dry-run showed the old path wrote past end-of-tape, reported
/// success, and left dead slices with the snapshot marked current.
#[test]
fn test_volume_write_refuses_over_capacity() {
    use tapectl::config::{Config, LtoBackendConfig, TapectlPaths};
    let (tmp, conn, _home) = setup();

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
        [],
    )
    .unwrap();
    let tid = conn.last_insert_rowid();
    // T8: volume_write now resolves tenant/operator keys (to assemble
    // BuildInputs) BEFORE the pre-flight validate that carries the capacity
    // check, so a keyless tenant/operator would refuse earlier than capacity
    // does — this fixture's tenant doubles as the operator, so one real key
    // covers both lookups and lets the flow reach the capacity gate this
    // test actually probes.
    let key = tapectl::crypto::keys::generate_keypair();
    conn.execute(
        "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key, key_type, is_active)
         VALUES (?1, 'op-primary', ?2, ?3, 'primary', 1)",
        rusqlite::params![tid, key.fingerprint, key.public_key],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
         VALUES ('u1', 'big', ?1, 'mtime_size', 1, 'active')",
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
    let snap = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 104857600)",
        [snap],
    )
    .unwrap();
    let ss = conn.last_insert_rowid();
    // One 5 MB encrypted slice — far larger than the 1 MB volume below.
    conn.execute(
        "INSERT INTO stage_slices
            (stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted, staging_path)
         VALUES (?1, 1, 5000000, 5242880, 'p', 'e', '/nonexistent/slice.dar.age')",
        [ss],
    )
    .unwrap();

    // A 1 MB volume — the staged 5 MB cannot fit.
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES ('L6-CAP', 'lto', 'p', 'LTO-6', 1048576, 'initialized')",
        [],
    )
    .unwrap();

    let mut config = Config::default();
    config.backends.lto.push(LtoBackendConfig {
        name: "p".into(),
        device_tape: "/dev/null".into(),
        device_sg: "/dev/null".into(),
        media_type: "LTO-6".into(),
        nominal_capacity: "1M".into(),
        usable_capacity_factor: 1.0,
        enospc_buffer: "0".into(),
        block_size: "512K".into(),
        hardware_compression: false,
    });
    // build() materializes the Layout's generated zones under
    // config.staging.directory before validate ever runs — the default
    // ("/mnt/staging") isn't writable in a test sandbox, so point it at this
    // test's own tmp dir.
    config.staging.directory = tmp.path().join("staging").to_string_lossy().to_string();
    let paths = TapectlPaths::new(tmp.path().to_path_buf());

    // The pre-flight validate (which carries the capacity check) runs before
    // the store is opened, so the bogus device is never touched.
    let err = tapectl::volume::write::volume_write(
        &conn,
        &paths,
        &config,
        "L6-CAP",
        "/dev/null",
        512 * 1024,
        false,
    )
    .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("capacity"),
        "expected a capacity refusal, got: {msg}"
    );

    // Nothing was written: no write rows, volume still 'initialized'.
    let writes: i64 = conn
        .query_row("SELECT COUNT(*) FROM writes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(writes, 0, "capacity refusal must not create write records");
}

/// T8: `volume_write` refuses fast (before touching the device or calling
/// `ValidatedLayout::plan`) when this volume already has a non-terminal
/// `writes` row — this is what turns what would otherwise be a raw
/// `UNIQUE(stage_set_id, volume_id)` constraint violation into a clear,
/// actionable error (automatic cross-process resume is not wired; see the
/// T8 report).
#[test]
fn test_volume_write_refuses_when_an_unresolved_write_session_already_exists() {
    use tapectl::config::{Config, LtoBackendConfig, TapectlPaths};
    let (tmp, conn, _home) = setup();

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
         VALUES (?1, 1, 'full', 'staged', '/tmp')",
        [uid],
    )
    .unwrap();
    let snap = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 104857600)",
        [snap],
    )
    .unwrap();
    let ss = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES ('L6-BUSY', 'lto', 'p', 'LTO-6', 2_500_000_000_000, 'active')",
        [],
    )
    .unwrap();
    let vol_id = conn.last_insert_rowid();

    // Simulate a prior attempt left interrupted (e.g. SIGINT, or a crash
    // swept by `recover_orphaned_sessions`).
    conn.execute(
        "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
         VALUES (?1, ?2, ?3, 'interrupted')",
        rusqlite::params![ss, snap, vol_id],
    )
    .unwrap();

    let mut config = Config::default();
    config.backends.lto.push(LtoBackendConfig {
        name: "p".into(),
        device_tape: "/dev/null".into(),
        device_sg: "/dev/null".into(),
        media_type: "LTO-6".into(),
        nominal_capacity: "2500G".into(),
        usable_capacity_factor: 1.0,
        enospc_buffer: "0".into(),
        block_size: "512K".into(),
        hardware_compression: false,
    });
    config.staging.directory = tmp.path().join("staging").to_string_lossy().to_string();
    let paths = TapectlPaths::new(tmp.path().to_path_buf());

    let err = tapectl::volume::write::volume_write(
        &conn,
        &paths,
        &config,
        "L6-BUSY",
        "/dev/null",
        512 * 1024,
        false,
    )
    .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("unresolved write session"),
        "expected a clear refusal naming the unresolved session, got: {msg}"
    );

    // Refused before ever calling plan(): still exactly the one pre-existing
    // writes row, not a second one.
    let writes: i64 = conn
        .query_row("SELECT COUNT(*) FROM writes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(writes, 1, "the guard must not insert a second writes row");
}

/// T8: a tenant with staged data but no active encryption key must refuse
/// clearly (naming the tenant) before `build()` ever attempts to encrypt an
/// envelope to an empty recipient list.
#[test]
fn test_volume_write_refuses_when_a_tenant_has_no_active_key() {
    use tapectl::config::{Config, LtoBackendConfig, TapectlPaths};
    let (tmp, conn, _home) = setup();

    // Operator, WITH a key.
    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('operator', 1, 'active')",
        [],
    )
    .unwrap();
    let op_id = conn.last_insert_rowid();
    let op_key = tapectl::crypto::keys::generate_keypair();
    conn.execute(
        "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key, key_type, is_active)
         VALUES (?1, 'op-primary', ?2, ?3, 'primary', 1)",
        rusqlite::params![op_id, op_key.fingerprint, op_key.public_key],
    )
    .unwrap();

    // Content tenant, deliberately WITHOUT any key.
    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('keyless', 0, 'active')",
        [],
    )
    .unwrap();
    let tenant_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
         VALUES ('u1', 'unit1', ?1, 'mtime_size', 1, 'active')",
        [tenant_id],
    )
    .unwrap();
    let uid = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
         VALUES (?1, 1, 'full', 'staged', '/tmp')",
        [uid],
    )
    .unwrap();
    let snap = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 104857600)",
        [snap],
    )
    .unwrap();
    let ss = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO stage_slices
            (stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted, staging_path)
         VALUES (?1, 1, 100, 110, 'p', 'e', '/nonexistent/slice.dar.age')",
        [ss],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES ('L6-NOKEY', 'lto', 'p', 'LTO-6', 2_500_000_000_000, 'active')",
        [],
    )
    .unwrap();

    let mut config = Config::default();
    config.backends.lto.push(LtoBackendConfig {
        name: "p".into(),
        device_tape: "/dev/null".into(),
        device_sg: "/dev/null".into(),
        media_type: "LTO-6".into(),
        nominal_capacity: "2500G".into(),
        usable_capacity_factor: 1.0,
        enospc_buffer: "0".into(),
        block_size: "512K".into(),
        hardware_compression: false,
    });
    config.staging.directory = tmp.path().join("staging").to_string_lossy().to_string();
    let paths = TapectlPaths::new(tmp.path().to_path_buf());

    let err = tapectl::volume::write::volume_write(
        &conn,
        &paths,
        &config,
        "L6-NOKEY",
        "/dev/null",
        512 * 1024,
        false,
    )
    .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("keyless") && msg.contains("no active key"),
        "expected a refusal naming the keyless tenant, got: {msg}"
    );

    let writes: i64 = conn
        .query_row("SELECT COUNT(*) FROM writes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        writes, 0,
        "a keyless-tenant refusal must not create write records"
    );
}

// ── Tracing subscriber tests (issue #45/H10) ──
//
// Every other test in this file calls library functions directly
// (`tapectl::cli::...::run`, `tapectl::db::open`, ...) and never goes
// through `fn main()` in src/main.rs — so none of them exercise the
// global tracing subscriber `main()` installs. Proving "warnings land on
// stderr, not stdout" requires observing a real OS-level stream split,
// which only exists once a real process has been spawned. This is
// therefore the one test in the suite that spawns the compiled `tapectl`
// binary itself, via Cargo's CARGO_BIN_EXE_<name> mechanism.

/// Proves the subscriber installed in `main()` (src/main.rs `init_tracing`)
/// writes to stderr and not stdout.
///
/// Trigger: `unit discover` against a `discovery.watch_roots` entry that
/// does not exist on disk hits the pre-existing warn! in
/// `src/unit/discovery.rs` ("watch root does not exist, skipping") with no
/// dar/tape/staging setup required. Run with `--json`: the JSON branch of
/// `unit discover` (src/cli/unit.rs) deliberately omits `skipped_roots`, so
/// stdout is clean of the warning text *by construction* — this test would
/// catch a subscriber regression (e.g. dropping `.with_writer(stderr)`, or
/// its removal) that leaks the line onto stdout, corrupting every --json
/// consumer (the real-world case: `scripts/mhvtl-verify-gate.sh` pipes
/// `volume verify --json` through `tee` then parses the file).
///
/// What this does NOT prove: it does not exercise `volume verify`/`db
/// fsck` themselves (neither currently logs via `tracing`), and it runs at
/// default verbosity only (no `--verbose` case). It proves the specific,
/// load-bearing property this ticket is about: the installed subscriber's
/// writer is stderr, not stdout, for at least one real warn! call site,
/// observed as a real process's real stdout/stderr file descriptors.
#[test]
fn test_tracing_warning_goes_to_stderr_not_stdout() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();
    let db_path = home.join("tapectl.db");
    let config_path = home.join("config.toml");
    std::fs::create_dir_all(home.join("keys")).unwrap();

    let bogus_root = home.join("does-not-exist-watch-root");
    let bogus_root_str = bogus_root.to_string_lossy();
    assert!(
        !bogus_root.exists(),
        "test fixture bug: bogus watch root must not exist"
    );

    std::fs::write(
        &config_path,
        format!(
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

[discovery]
watch_roots = ["{bogus_root_str}"]
"#
        ),
    )
    .unwrap();

    // Create the schema the same way `tapectl init` would (real migration
    // path via tapectl::db::open, same as `setup()` above), then drop the
    // connection before the subprocess opens the same file — no WAL lock
    // contention between the two processes.
    {
        let _conn = tapectl_test_db(&db_path);
    }

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--json",
            "unit",
            "discover",
        ])
        .output()
        .expect("failed to run the tapectl binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "unit discover should exit 0 on a merely-missing watch root \
         (advisory, not fatal); stdout: {stdout:?}, stderr: {stderr:?}"
    );

    // stdout: valid, uncorrupted JSON. This is the exact property a
    // --json consumer depends on.
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout was not valid JSON ({e}): {stdout:?}"));
    assert!(
        parsed.get("created").is_some(),
        "unexpected JSON shape on stdout: {parsed}"
    );
    assert!(
        !stdout.contains("watch root"),
        "warning text leaked onto stdout, corrupting --json output: {stdout:?}"
    );

    // stderr: the tracing::warn! from src/unit/discovery.rs, emitted at
    // default (non-verbose) verbosity — proves both that WARN-level
    // surfaces without --verbose, and that it landed on stderr.
    assert!(
        stderr.contains("watch root does not exist"),
        "expected the discovery warning on stderr, got: {stderr:?}"
    );
}

/// Issue #40: `db backup` without `--include-keys` must not just silently
/// skip the keys — it must say so, somewhere an operator running with
/// `--json` can actually observe deterministically. A tracing log line is
/// easy to miss (and awkward to assert on: prose, a separate stream,
/// subject to log-level filtering); the `--json` stdout payload is the
/// contract a `--json` consumer actually depends on, so that's what this
/// test pins down — real subprocess, real CLI parsing, both flag states.
#[test]
fn test_db_backup_json_reports_whether_keys_were_included() {
    let (_tmp, conn, home) = setup();
    let config_path = home.join("config.toml");
    // The subprocess opens the same sqlite file — drop this connection
    // first so there's no WAL lock contention between the two processes
    // (same reasoning as test_tracing_warning_goes_to_stderr_not_stdout).
    drop(conn);

    let run_backup = |dest: &std::path::Path, include_keys: bool| -> serde_json::Value {
        let mut args = vec![
            "--config".to_string(),
            config_path.to_str().unwrap().to_string(),
            "--json".to_string(),
            "db".to_string(),
            "backup".to_string(),
            "--to".to_string(),
            dest.to_str().unwrap().to_string(),
        ];
        if include_keys {
            args.push("--include-keys".to_string());
        }

        let output = std::process::Command::new(env!("CARGO_BIN_EXE_tapectl"))
            .args(&args)
            .output()
            .expect("failed to run the tapectl binary");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "db backup should exit 0; stdout: {stdout:?}, stderr: {stderr:?}"
        );
        serde_json::from_str(stdout.trim())
            .unwrap_or_else(|e| panic!("stdout was not valid JSON ({e}): {stdout:?}"))
    };

    let without = run_backup(&home.join("backup-no-keys.db"), false);
    assert_eq!(
        without.get("keys_included"),
        Some(&serde_json::json!(false)),
        "without --include-keys, the JSON output must say keys were not \
         included, not just silently omit them: {without}"
    );

    let with = run_backup(&home.join("backup-with-keys.db"), true);
    assert_eq!(
        with.get("keys_included"),
        Some(&serde_json::json!(true)),
        "with --include-keys, the JSON output must confirm keys were \
         included: {with}"
    );
}

/// Issue #91 / ADR-0004: `volume retire`'s impact analysis must DISPLAY
/// evidence age wherever a destructive operation consumes copy coverage —
/// never gate, never require a flag.
///
/// Scenario: two units both have a copy on the volume being retired
/// (`L6-RETIRE`).
/// - `zero_copy_unit` has NO other copy anywhere -- this is the existing
///   Tier-2 at-risk case and must still fire the consent gate.
/// - `covered_unit` ALSO has a copy on a second, already-sealed volume
///   (`L6-OTHER`) whose only passing verification is far in the past.
///
/// This mixes the zero-copy (Tier 2) and evidence-bearing (Tier 1) cases in
/// one retirement, which is deliberate: the evidence line must appear only
/// for the unit that still has coverage, and the naive "forgot to exclude
/// the retiring volume" bug would cite `L6-RETIRE` itself as the covered
/// unit's remaining evidence -- so the retiring volume's label must be
/// ABSENT from that unit's evidence.
#[test]
fn test_volume_retire_shows_coverage_evidence_age() {
    let (_tmp, conn, home) = setup();

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
        [],
    )
    .unwrap();
    let tid = conn.last_insert_rowid();

    // Two units.
    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
         VALUES ('u1', 'zero_copy_unit', ?1, 'mtime_size', 1, 'active')",
        [tid],
    )
    .unwrap();
    let zero_unit_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
         VALUES ('u2', 'covered_unit', ?1, 'mtime_size', 1, 'active')",
        [tid],
    )
    .unwrap();
    let covered_unit_id = conn.last_insert_rowid();

    // Snapshots + stage sets, one per unit.
    conn.execute(
        "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
         VALUES (?1, 1, 'full', 'current', '/tmp/u1')",
        [zero_unit_id],
    )
    .unwrap();
    let zero_snap_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 104857600)",
        [zero_snap_id],
    )
    .unwrap();
    let zero_ss_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
         VALUES (?1, 1, 'full', 'current', '/tmp/u2')",
        [covered_unit_id],
    )
    .unwrap();
    let covered_snap_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 104857600)",
        [covered_snap_id],
    )
    .unwrap();
    let covered_ss_id = conn.last_insert_rowid();

    // A second stage_set for covered_unit's copy on the OTHER volume.
    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 104857600)",
        [covered_snap_id],
    )
    .unwrap();
    let covered_ss2_id = conn.last_insert_rowid();

    // Volumes: the one being retired, sealed, plus a second sealed volume
    // that still provides coverage.
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES ('L6-RETIRE', 'lto', 'primary', 'LTO-6', 2500000000000, 'sealed')",
        [],
    )
    .unwrap();
    let retire_vol_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES ('L6-OTHER', 'lto', 'primary', 'LTO-6', 2500000000000, 'sealed')",
        [],
    )
    .unwrap();
    let other_vol_id = conn.last_insert_rowid();

    // A third, eligible-but-never-verified volume covering zero_copy_unit
    // would defeat the "zero copies" scenario, so it is NOT added there --
    // instead prove the never-verified rendering via a THIRD unit that has
    // its only other copy on a volume with no passed verification session
    // at all.
    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
         VALUES ('u3', 'never_verified_unit', ?1, 'mtime_size', 1, 'active')",
        [tid],
    )
    .unwrap();
    let never_unit_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
         VALUES (?1, 1, 'full', 'current', '/tmp/u3')",
        [never_unit_id],
    )
    .unwrap();
    let never_snap_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 104857600)",
        [never_snap_id],
    )
    .unwrap();
    let never_ss_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 104857600)",
        [never_snap_id],
    )
    .unwrap();
    let never_ss2_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES ('L6-NEVER', 'lto', 'primary', 'LTO-6', 2500000000000, 'sealed')",
        [],
    )
    .unwrap();
    let never_vol_id = conn.last_insert_rowid();

    // Writes: zero_copy_unit only on the retiring volume.
    conn.execute(
        "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
         VALUES (?1, ?2, ?3, 'completed')",
        rusqlite::params![zero_ss_id, zero_snap_id, retire_vol_id],
    )
    .unwrap();

    // covered_unit: one write on the retiring volume, one on L6-OTHER.
    conn.execute(
        "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
         VALUES (?1, ?2, ?3, 'completed')",
        rusqlite::params![covered_ss_id, covered_snap_id, retire_vol_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
         VALUES (?1, ?2, ?3, 'completed')",
        rusqlite::params![covered_ss2_id, covered_snap_id, other_vol_id],
    )
    .unwrap();

    // never_verified_unit: one write on the retiring volume, one on
    // L6-NEVER (no verification session at all).
    conn.execute(
        "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
         VALUES (?1, ?2, ?3, 'completed')",
        rusqlite::params![never_ss_id, never_snap_id, retire_vol_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
         VALUES (?1, ?2, ?3, 'completed')",
        rusqlite::params![never_ss2_id, never_snap_id, never_vol_id],
    )
    .unwrap();

    // A passing verification session on L6-OTHER, far in the past.
    conn.execute(
        "INSERT INTO verification_sessions (volume_id, completed_at, outcome)
         VALUES (?1, '2011-08-01 00:00:00', 'passed')",
        [other_vol_id],
    )
    .unwrap();
    // A FAILED verification session on L6-NEVER -- must not count as
    // evidence (a failed session is not evidence).
    conn.execute(
        "INSERT INTO verification_sessions (volume_id, completed_at, outcome)
         VALUES (?1, '2026-01-01 00:00:00', 'failed')",
        [never_vol_id],
    )
    .unwrap();

    drop(conn); // release the connection before the subprocess opens the same file

    let config_path = home.join("config.toml");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--json",
            "--yes",
            "volume",
            "retire",
            "L6-RETIRE",
        ])
        .output()
        .expect("failed to run the tapectl binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "volume retire --yes should exit 0 even with an at-risk unit present \
         (Tier 2, --yes waives it); stdout: {stdout:?}, stderr: {stderr:?}"
    );

    // Parse the WHOLE stdout as JSON -- a stray println! outside the
    // json_output branch is a real defect class here (issue #56).
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout was not valid JSON ({e}): {stdout:?}"));

    let affected = parsed["affected_units"]
        .as_array()
        .expect("affected_units must be an array");

    let covered = affected
        .iter()
        .find(|u| u["unit"] == "covered_unit")
        .expect("covered_unit must appear in affected_units");
    let covered_evidence = covered["evidence"]
        .as_array()
        .expect("covered_unit must carry an evidence array");
    assert_eq!(covered_evidence.len(), 1, "evidence: {covered_evidence:?}");
    assert_eq!(covered_evidence[0]["volume"], "L6-OTHER");
    assert!(
        !covered_evidence.iter().any(|e| e["volume"] == "L6-RETIRE"),
        "the retiring volume must never appear as a unit's own remaining \
         coverage evidence: {covered_evidence:?}"
    );
    let summary = covered["evidence_summary"]
        .as_str()
        .expect("covered_unit must carry a non-null evidence_summary");
    assert!(summary.contains("L6-OTHER"), "summary: {summary}");
    assert!(summary.contains("days ago"), "summary: {summary}");

    let never = affected
        .iter()
        .find(|u| u["unit"] == "never_verified_unit")
        .expect("never_verified_unit must appear in affected_units");
    let never_evidence = never["evidence"]
        .as_array()
        .expect("never_verified_unit must carry an evidence array");
    assert_eq!(never_evidence.len(), 1, "evidence: {never_evidence:?}");
    assert_eq!(never_evidence[0]["volume"], "L6-NEVER");
    assert!(
        never_evidence[0]["last_verified"].is_null(),
        "an eligible-but-never-passed volume must render as never-verified, \
         not be omitted: {never_evidence:?}"
    );
    let never_summary = never["evidence_summary"]
        .as_str()
        .expect("never_verified_unit must carry a non-null evidence_summary");
    assert!(
        never_summary.contains("never verified"),
        "summary: {never_summary}"
    );

    // The zero-copy unit alone must NOT trigger the "unparseable" or
    // evidence machinery -- it has zero remaining copies, so no evidence at
    // all, and it must still be flagged at_risk (Tier 2 unchanged).
    let zero = affected
        .iter()
        .find(|u| u["unit"] == "zero_copy_unit")
        .expect("zero_copy_unit must appear in affected_units");
    assert_eq!(zero["remaining_copies"], 0);
    let zero_evidence = zero["evidence"]
        .as_array()
        .expect("zero_copy_unit must carry an evidence array (empty)");
    assert!(
        zero_evidence.is_empty(),
        "a zero-copy unit has no remaining-coverage evidence: {zero_evidence:?}"
    );
    assert!(
        zero["evidence_summary"].is_null(),
        "a zero-copy unit's evidence_summary must be null, not a fabricated line: {zero:?}"
    );

    // The success-path JSON (this ran with --yes, so it succeeded, not
    // refused) doesn't carry a top-level at_risk_units array -- that's
    // only on the refusal object. Confirm via remaining_copies instead
    // that only zero_copy_unit is at zero and the other two are not.
    assert_eq!(covered["remaining_copies"], 1);
    assert_eq!(never["remaining_copies"], 1);
}

/// Issue #99: `unit mark-tape-only` surfaces ADR-0004 evidence for the
/// coverage the unit is now relying on, in both text and `--json`, without
/// changing the existing Tier-2/Tier-3 guard behavior or exit code.
#[test]
fn test_unit_mark_tape_only_shows_coverage_evidence() {
    let (_tmp, conn, home) = setup();

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
    let unit_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
         VALUES (?1, 1, 'full', 'current', '/tmp/u1')",
        [unit_id],
    )
    .unwrap();
    let snap_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 104857600)",
        [snap_id],
    )
    .unwrap();
    let ss1_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 104857600)",
        [snap_id],
    )
    .unwrap();
    let ss2_id = conn.last_insert_rowid();

    conn.execute("INSERT INTO locations (name) VALUES ('site-a')", [])
        .unwrap();
    let loc_a = conn.last_insert_rowid();
    conn.execute("INSERT INTO locations (name) VALUES ('site-b')", [])
        .unwrap();
    let loc_b = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status, location_id)
         VALUES ('L6-A', 'lto', 'primary', 'LTO-6', 2500000000000, 'sealed', ?1)",
        [loc_a],
    )
    .unwrap();
    let vol_a = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status, location_id)
         VALUES ('L6-B', 'lto', 'primary', 'LTO-6', 2500000000000, 'sealed', ?1)",
        [loc_b],
    )
    .unwrap();
    let vol_b = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
         VALUES (?1, ?2, ?3, 'completed')",
        rusqlite::params![ss1_id, snap_id, vol_a],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
         VALUES (?1, ?2, ?3, 'completed')",
        rusqlite::params![ss2_id, snap_id, vol_b],
    )
    .unwrap();

    // Only L6-B has a passed verification session -- L6-A must render
    // "never verified" and, being older/weaker (never beats any age), be
    // the volume `describe()` names.
    conn.execute(
        "INSERT INTO verification_sessions (volume_id, completed_at, outcome)
         VALUES (?1, '2020-01-01 00:00:00', 'passed')",
        [vol_b],
    )
    .unwrap();

    drop(conn);

    let config_path = home.join("config.toml");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--json",
            "unit",
            "mark-tape-only",
            "unit1",
        ])
        .output()
        .expect("failed to run the tapectl binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "unit mark-tape-only should succeed with 2 copies/2 locations; \
         stdout: {stdout:?}, stderr: {stderr:?}"
    );

    let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout was not valid JSON ({e}): {stdout:?}"));

    assert_eq!(parsed["status"], "tape_only");
    assert_eq!(parsed["copies"], 2);
    assert_eq!(parsed["locations"], 2);

    let evidence = parsed["evidence"]
        .as_array()
        .expect("evidence must be an array");
    let labels: Vec<&str> = evidence
        .iter()
        .map(|e| e["volume"].as_str().unwrap())
        .collect();
    assert!(labels.contains(&"L6-A"), "evidence: {labels:?}");
    assert!(labels.contains(&"L6-B"), "evidence: {labels:?}");

    let summary = parsed["evidence_summary"]
        .as_str()
        .expect("mark-tape-only must carry a non-null evidence_summary");
    assert!(summary.contains("unit1"), "summary: {summary}");
    assert!(summary.contains("never verified"), "summary: {summary}");
    assert!(summary.contains("2 copies"), "summary: {summary}");

    // Text mode: re-running mark-tape-only on unit1 (idempotent -- no
    // status precondition beyond the copy/location/dirty guards) must show
    // the same evidence line alongside the existing summary line.
    let text_output = std::process::Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "unit",
            "mark-tape-only",
            "unit1",
        ])
        .output()
        .expect("failed to run the tapectl binary");
    assert!(text_output.status.success());
    let text_stdout = String::from_utf8_lossy(&text_output.stdout);
    assert!(
        text_stdout.contains("marked tape-only"),
        "existing summary line must be unchanged: {text_stdout}"
    );
    assert!(
        text_stdout.contains("unit1") && text_stdout.contains("never verified"),
        "text mode must show the evidence line too: {text_stdout}"
    );

    // Guard/exit-code behavior is unaffected: a below-threshold unit still
    // refuses with a non-zero exit and unchanged wording.
    let db_path = home.join("tapectl.db");
    let conn2 = tapectl_test_db(&db_path);
    conn2
        .execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('op2', 1, 'active')",
            [],
        )
        .unwrap();
    let tid2 = conn2.last_insert_rowid();
    conn2
        .execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('u2', 'unit2', ?1, 'mtime_size', 1, 'active')",
            [tid2],
        )
        .unwrap();
    // Deliberately no snapshot/writes -- below the copy threshold.
    drop(conn2);

    let guard_output = std::process::Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "unit",
            "mark-tape-only",
            "unit2",
        ])
        .output()
        .expect("failed to run the tapectl binary");
    assert!(
        !guard_output.status.success(),
        "a below-threshold unit must still refuse without --force (guard unchanged)"
    );
    let guard_stderr = String::from_utf8_lossy(&guard_output.stderr);
    assert!(
        guard_stderr.contains("insufficient copies"),
        "guard wording must be unchanged: {guard_stderr}"
    );
}

/// Issue #99: `volume compact-finish` surfaces ADR-0004 evidence for a unit
/// that retains coverage on another volume after the source volume retires,
/// without changing the existing no-copy-elsewhere refusal guard or exit
/// code.
#[test]
fn test_compact_finish_shows_coverage_evidence() {
    let (_tmp, conn, home) = setup();

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
    let unit_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
         VALUES (?1, 1, 'full', 'current', '/tmp/u1')",
        [unit_id],
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
         VALUES (?1, 1, 100, 100, 'plain', 'enc')",
        [ss_id],
    )
    .unwrap();
    let slice_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES ('L6-SRC', 'lto', 'primary', 'LTO-6', 2500000000000, 'sealed')",
        [],
    )
    .unwrap();
    let vol_src = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES ('L6-DST', 'lto', 'primary', 'LTO-6', 2500000000000, 'sealed')",
        [],
    )
    .unwrap();
    let vol_dst = conn.last_insert_rowid();

    // Same stage_slice, written (as two copies) to both volumes -- so
    // compact_finish's own "live slice has no copy elsewhere" guard sees
    // this slice as protected and lets retirement proceed.
    conn.execute(
        "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
         VALUES (?1, ?2, ?3, 'completed')",
        rusqlite::params![ss_id, snap_id, vol_src],
    )
    .unwrap();
    let write_src = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
         VALUES (?1, ?2, ?3, 'completed')",
        rusqlite::params![ss_id, snap_id, vol_dst],
    )
    .unwrap();
    let write_dst = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO write_positions (write_id, stage_slice_id, position, status)
         VALUES (?1, ?2, '0', 'written')",
        rusqlite::params![write_src, slice_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO write_positions (write_id, stage_slice_id, position, status)
         VALUES (?1, ?2, '0', 'written')",
        rusqlite::params![write_dst, slice_id],
    )
    .unwrap();

    // L6-DST has a passed verification session; the evidence line should
    // name it.
    conn.execute(
        "INSERT INTO verification_sessions (volume_id, completed_at, outcome)
         VALUES (?1, '2020-01-01 00:00:00', 'passed')",
        [vol_dst],
    )
    .unwrap();

    drop(conn);

    let config_path = home.join("config.toml");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--json",
            "volume",
            "compact-finish",
            "L6-SRC",
        ])
        .output()
        .expect("failed to run the tapectl binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "compact-finish must succeed: the live slice has a copy on L6-DST; \
         stdout: {stdout:?}, stderr: {stderr:?}"
    );

    let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout was not valid JSON ({e}): {stdout:?}"));

    assert_eq!(parsed["label"], "L6-SRC");
    assert_eq!(parsed["status"], "retired");

    let affected = parsed["affected_units"]
        .as_array()
        .expect("affected_units must be an array");
    let unit1 = affected
        .iter()
        .find(|u| u["unit"] == "unit1")
        .expect("unit1 must appear in affected_units");
    let evidence = unit1["evidence"]
        .as_array()
        .expect("unit1 must carry an evidence array");
    assert_eq!(evidence.len(), 1, "evidence: {evidence:?}");
    assert_eq!(evidence[0]["volume"], "L6-DST");
    assert!(
        !evidence.iter().any(|e| e["volume"] == "L6-SRC"),
        "the retired source volume must never appear as remaining coverage: {evidence:?}"
    );
    let summary = unit1["evidence_summary"]
        .as_str()
        .expect("unit1 must carry a non-null evidence_summary");
    assert!(summary.contains("L6-DST"), "summary: {summary}");
}
