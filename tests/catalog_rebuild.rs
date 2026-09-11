//! `catalog rebuild --from-volume` (#136) against a volume written by the
//! real write session.
//!
//! The tape here is not a hand-rolled fixture: `build_sealed_volume` runs the
//! production build -> validate -> plan -> execute -> seal -> confirm chain
//! into a `MemStore`, and the rebuild then reads back only what those bytes
//! say. A harness that assembled the envelopes itself would be a fixture
//! simpler than the artifact — it would keep passing after a change to the
//! real packing broke every real tape.
//!
//! The load-bearing assertion is `restore_resolution_query`: the *verbatim*
//! join `volume::restore::restore_unit` uses. A rebuild that inserts rows
//! which merely look plausible but do not satisfy that query has rebuilt
//! nothing an operator can restore from.

use std::path::Path;

use sha2::{Digest, Sha256};

use tapectl::crypto::keys::generate_keypair;
use tapectl::db;
use tapectl::staging;
use tapectl::store::{MemStore, Tier};
use tapectl::volume::build::{self, BuildInputs, BuildSlice, BuildUnit, TenantInfo};
use tapectl::volume::layout_model::KeyAvailability;
use tapectl::volume::rebuild;
use tapectl::volume::session::{ConfirmOutcome, ExecuteOutcome};

const BS: u64 = 65536;
const LABEL: &str = "REBUILD01";
const VOL_UUID: &str = "11111111-2222-3333-4444-555555555555";

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

struct TenantFixture {
    tenant_id: i64,
    name: String,
    public_key: String,
    secret_key: String,
}

/// The units this fixture writes, as `(unit name, tenant name, content)`.
const UNITS: &[(&str, &str, &[u8])] = &[
    ("photos/2019", "alpha", b"alpha photos payload"),
    ("photos/2020", "alpha", b"alpha more photos payload"),
    ("ledgers/fy24", "bravo", b"bravo ledger payload"),
];

struct SealedVolume {
    store: MemStore,
    operator_secret: String,
    escrow_secret: String,
    tenant_secret: String,
    /// Unit name -> the tape positions its slices landed on, in slice order.
    expected_positions: Vec<(String, Vec<i64>)>,
    /// Unit name -> its slice's `sha256_plain`, as staged.
    expected_plain_hashes: Vec<(String, String)>,
}

fn insert_tenant(conn: &rusqlite::Connection, name: &str, is_operator: bool) -> TenantFixture {
    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES (?1, ?2, 'active')",
        rusqlite::params![name, is_operator as i64],
    )
    .unwrap();
    let tenant_id = conn.last_insert_rowid();
    let kp = generate_keypair();
    conn.execute(
        "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key, key_type, is_active)
         VALUES (?1, ?2, ?3, ?4, 'primary', 1)",
        rusqlite::params![tenant_id, format!("{name}-primary"), kp.fingerprint, kp.public_key],
    )
    .unwrap();
    TenantFixture {
        tenant_id,
        name: name.to_string(),
        public_key: kp.public_key,
        secret_key: kp.secret_key,
    }
}

/// Run the full production write session into a `MemStore` and return the
/// sealed result. `with_catalog_db` selects whether the operator envelope
/// carries #83's `catalog.db` — false stands in for a tape written before it.
fn build_sealed_volume(with_catalog_db: bool) -> SealedVolume {
    let db_dir = tempfile::tempdir().unwrap();
    let conn = db::open(&db_dir.path().join("src.db")).unwrap();

    let operator = insert_tenant(&conn, "operator", true);
    let alpha = insert_tenant(&conn, "alpha", false);
    let bravo = insert_tenant(&conn, "bravo", false);

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000, 'active')",
        rusqlite::params![LABEL],
    )
    .unwrap();
    let volume_id = conn.last_insert_rowid();

    let slices_dir = tempfile::tempdir().unwrap();
    let mut build_units = Vec::new();
    let mut stage_set_ids = Vec::new();
    let mut expected_plain_hashes = Vec::new();

    for (i, (unit_name, tenant_name, content)) in UNITS.iter().enumerate() {
        let tenant = if *tenant_name == "alpha" {
            &alpha
        } else {
            &bravo
        };
        let unit_uuid = format!("unit-uuid-{i:04}");
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES (?1, ?2, ?3, ?4, 'active')",
            rusqlite::params![unit_uuid, unit_name, tenant.tenant_id, "/src/data"],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 7, 'staged', ?2, 2, ?3)",
            rusqlite::params![unit_id, format!("/src/{unit_name}"), content.len() as i64],
        )
        .unwrap();
        let snapshot_id = conn.last_insert_rowid();

        // Two file rows per snapshot, so the `files` assertion has something
        // to count that the manifest alone could never supply.
        for n in 0..2 {
            conn.execute(
                "INSERT INTO files (snapshot_id, path, size_bytes, sha256, is_directory)
                 VALUES (?1, ?2, ?3, ?4, 0)",
                rusqlite::params![
                    snapshot_id,
                    format!("{unit_name}/file{n}.bin"),
                    100 + n,
                    sha256_hex(format!("{unit_name}-{n}").as_bytes()),
                ],
            )
            .unwrap();
        }

        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 10485760)",
            rusqlite::params![snapshot_id],
        )
        .unwrap();
        let stage_set_id = conn.last_insert_rowid();
        stage_set_ids.push(stage_set_id);

        // Two slices per unit, so "slice number is not tape position" is a
        // claim the fixture can actually falsify.
        let mut slices = Vec::new();
        for slice_number in 1..=2i64 {
            let mut plaintext = content.to_vec();
            plaintext.extend_from_slice(format!("-slice{slice_number}").as_bytes());
            let recipients = vec![tenant.public_key.clone(), operator.public_key.clone()];
            let encrypted = staging::encrypt_data(&plaintext, &recipients).unwrap();
            let sha_plain = sha256_hex(&plaintext);
            let sha_enc = sha256_hex(&encrypted);

            conn.execute(
                "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                           sha256_plain, sha256_encrypted)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    stage_set_id,
                    slice_number,
                    plaintext.len() as i64,
                    encrypted.len() as i64,
                    sha_plain,
                    sha_enc
                ],
            )
            .unwrap();
            let slice_id = conn.last_insert_rowid();
            let slice_path = slices_dir.path().join(format!("slice_{slice_id}.age"));
            std::fs::write(&slice_path, &encrypted).unwrap();
            conn.execute(
                "UPDATE stage_slices SET staging_path = ?1 WHERE id = ?2",
                rusqlite::params![slice_path.to_string_lossy(), slice_id],
            )
            .unwrap();

            if slice_number == 1 {
                expected_plain_hashes.push((unit_name.to_string(), sha_plain.clone()));
            }
            slices.push(BuildSlice {
                slice_id,
                slice_number,
                size_bytes: plaintext.len() as i64,
                encrypted_bytes: encrypted.len() as i64,
                sha256_plain: sha_plain,
                sha256_encrypted: sha_enc,
                staging_path: slice_path,
            });
        }

        build_units.push(BuildUnit {
            stage_set_id,
            snapshot_id,
            unit_name: unit_name.to_string(),
            unit_uuid,
            tenant_id: tenant.tenant_id,
            dar_version: Some("2.7.20".to_string()),
            dar_command: None,
            catalog_path: None,
            snapshot_version: 7,
            slices,
        });
    }

    let catalog_dir = tempfile::tempdir().unwrap();
    let catalog_db_path = if with_catalog_db {
        let p = catalog_dir.path().join("catalog.db");
        db::catalog_snapshot::build_catalog_snapshot(&conn, &stage_set_ids, &p).unwrap();
        Some(p)
    } else {
        None
    };

    // ADR-0005's permanent escrow recipient. `with_escrow` adds it to EVERY
    // envelope's recipient list, so the escrow key is the other half of the
    // ratified "operator or escrow key" rule — untested until this fixture
    // carried one.
    let escrow = generate_keypair();

    let tenants = [alpha, bravo];
    let inputs = BuildInputs {
        label: LABEL.to_string(),
        volume_uuid: VOL_UUID.to_string(),
        media_type: "LTO-6".to_string(),
        tapectl_version: "0.1.0-test".to_string(),
        created_at: "2026-09-11T00:00:00Z".to_string(),
        block_size: BS,
        usable_bytes: 64 * 1024 * 1024,
        enospc_buffer: 1024 * 1024,
        nominal_capacity: 2_400_000_000,
        mam_capacity: 2_400_000_000,
        mam_manufacturer: "TAPECTL-TEST".to_string(),
        mam_serial: "REBUILDSERIAL".to_string(),
        mam_length: 0,
        mam_loads: 0,
        units: build_units.clone(),
        tenants: tenants
            .iter()
            .map(|t| TenantInfo {
                tenant_id: t.tenant_id,
                tenant_name: t.name.clone(),
                public_keys: vec![t.public_key.clone()],
            })
            .collect(),
        operator_public_keys: vec![operator.public_key.clone()],
        escrow_public_key: Some(escrow.public_key.clone()),
        catalog_db_path,
    };

    let session_dir = tempfile::tempdir().unwrap();
    let built = build::build(&inputs, session_dir.path()).expect("build succeeds");

    let keys = KeyAvailability {
        tenant_ids: tenants.iter().map(|t| t.tenant_id).collect(),
        tenants_with_active_key: tenants.iter().map(|t| t.tenant_id).collect(),
        operator_key_present: true,
        escrow_recipient_present: None,
        stage_sets_lacking_escrow: None,
    };

    let mut store = MemStore::new(BS as usize);
    let validated = built.into_validated(&keys, &mut store).expect("validate");
    let planned = validated
        .plan(&conn, volume_id, &build_units)
        .expect("plan");
    let ready = match planned.execute(&conn, &mut store).expect("execute") {
        ExecuteOutcome::Ready(r) => r,
        ExecuteOutcome::Interrupted(_) => panic!("harness must reach Ready, got Interrupted"),
        ExecuteOutcome::Aborted(a) => panic!("harness must reach Ready, got Aborted: {}", a.reason),
    };
    let sealed = ready.seal(&mut store).expect("seal");
    match sealed
        .confirm(&conn, &mut store, Tier::Integrity)
        .expect("confirm")
    {
        ConfirmOutcome::Sealed(_) => {}
        ConfirmOutcome::Quarantined(q) => panic!("harness must seal, got {:?}", q.reason),
    }

    // The positions the REAL plan chose, read back from the source catalog —
    // the rebuild must arrive at these same numbers from the tape alone.
    let mut expected_positions = Vec::new();
    for (unit_name, _, _) in UNITS {
        let mut stmt = conn
            .prepare(
                "SELECT CAST(wp.position AS INTEGER)
                 FROM write_positions wp
                 JOIN writes w ON w.id = wp.write_id
                 JOIN stage_slices sl ON sl.id = wp.stage_slice_id
                 JOIN stage_sets ss ON ss.id = sl.stage_set_id
                 JOIN snapshots s ON s.id = ss.snapshot_id
                 JOIN units u ON u.id = s.unit_id
                 WHERE u.name = ?1 ORDER BY sl.slice_number",
            )
            .unwrap();
        let positions: Vec<i64> = stmt
            .query_map(rusqlite::params![unit_name], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        expected_positions.push((unit_name.to_string(), positions));
    }

    SealedVolume {
        store,
        operator_secret: operator.secret_key,
        escrow_secret: escrow.secret_key,
        tenant_secret: tenants[0].secret_key.clone(),
        expected_positions,
        expected_plain_hashes,
    }
}

/// Write a secret key to a file in the shape `crypto::keys::read_secret_key`
/// expects, and return its path.
fn key_file(dir: &Path, name: &str, secret: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("# tapectl test key\n{secret}\n")).unwrap();
    p
}

fn fresh_db(dir: &Path) -> rusqlite::Connection {
    db::open(&dir.join("rebuilt.db")).unwrap()
}

/// The VERBATIM resolution join from `volume::restore::restore_unit`. The
/// point of copying it rather than paraphrasing: a rebuild is only useful if
/// this exact query finds the slices.
fn restore_resolution_query(
    conn: &rusqlite::Connection,
    unit_name: &str,
) -> Vec<(i64, String, String)> {
    let unit_id: i64 = conn
        .query_row(
            "SELECT id FROM units WHERE name = ?1",
            rusqlite::params![unit_name],
            |r| r.get(0),
        )
        .unwrap_or_else(|e| panic!("rebuilt catalog has no unit {unit_name}: {e}"));
    let mut stmt = conn
        .prepare(
            "SELECT sl.slice_number, wp.position, sl.sha256_plain
             FROM write_positions wp
             JOIN writes w ON w.id = wp.write_id
             JOIN stage_slices sl ON sl.id = wp.stage_slice_id
             JOIN stage_sets ss ON ss.id = sl.stage_set_id
             JOIN snapshots s ON s.id = ss.snapshot_id
             JOIN volumes v ON v.id = w.volume_id
             WHERE s.unit_id = ?1 AND v.label = ?2 AND w.status = 'completed'
               AND wp.status = 'written'
             ORDER BY sl.slice_number",
        )
        .unwrap();
    stmt.query_map(rusqlite::params![unit_id, LABEL], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })
    .unwrap()
    .map(|r| r.unwrap())
    .collect()
}

fn rebuild(
    conn: &rusqlite::Connection,
    vol: &mut SealedVolume,
    secret: &str,
    scratch: &Path,
) -> tapectl::error::Result<rebuild::RebuildReport> {
    let key_dir = tempfile::tempdir().unwrap();
    let key = key_file(key_dir.path(), "k.age.key", secret);
    let secret_str = tapectl::crypto::keys::read_secret_key(&key)?;
    let identity: age::x25519::Identity = secret_str.parse().unwrap();
    rebuild::rebuild_from_store(
        conn,
        &mut vol.store,
        &[identity],
        Some(LABEL),
        "recovered",
        Some("lto0"),
        scratch,
        "memstore",
    )
}

#[test]
fn a_rebuilt_catalog_satisfies_the_query_restore_actually_uses() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let report =
        rebuild(&conn, &mut vol, &secret, scratch.path()).expect("rebuild from a sealed volume");

    assert_eq!(report.label, LABEL);
    assert!(report.volume_inserted);
    assert!(report.had_catalog_db);
    assert_eq!(report.units, UNITS.len());
    assert!(
        report.units_without_tenant_envelope.is_empty(),
        "every unit is in a tenant envelope on this tape: {:?}",
        report.units_without_tenant_envelope
    );

    for (unit_name, expected) in &vol.expected_positions {
        let rows = restore_resolution_query(&conn, unit_name);
        assert_eq!(
            rows.len(),
            expected.len(),
            "unit {unit_name}: restore's own query found {} slices, expected {}",
            rows.len(),
            expected.len()
        );
        let got: Vec<i64> = rows
            .iter()
            .map(|(_, pos, _)| pos.parse().unwrap())
            .collect();
        assert_eq!(
            &got, expected,
            "unit {unit_name}: rebuilt tape positions disagree with what the write plan chose"
        );
    }
}

/// The trap the #72 retrieval guide names: slice number is not tape position,
/// and slices do not start at 0. A rebuild that stored the slice number in
/// `write_positions.position` would restore the wrong files, and every other
/// assertion here would still pass.
#[test]
fn write_positions_record_the_tape_position_not_the_slice_number() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    let rows = restore_resolution_query(&conn, UNITS[0].0);
    assert_eq!(rows.len(), 2);
    for (slice_number, position, _) in &rows {
        let pos: i64 = position.parse().unwrap();
        assert!(
            pos > *slice_number,
            "slice {slice_number} landed at position {pos}: data slices sit after the \
             envelopes, so a position equal to the slice number means the two were confused"
        );
    }
}

/// `sha256_plain` is on NEITHER the front index (sacred invariant) nor the
/// on-tape `catalog.db` — only the envelope manifest has it. If a rebuild
/// ever sourced slices from the front index, this is what would go missing.
#[test]
fn slice_plaintext_hashes_survive_the_rebuild() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    for (unit_name, expected) in &vol.expected_plain_hashes {
        let rows = restore_resolution_query(&conn, unit_name);
        let (_, _, sha_plain) = &rows[0];
        assert_eq!(
            sha_plain, expected,
            "unit {unit_name}: slice 1's sha256_plain did not survive the rebuild"
        );
    }
}

/// The operator manifest files every unit under the placeholder tenant
/// `"operator"`. Real ownership is only in the tenant envelopes, so a rebuild
/// that read the operator envelope alone would file the whole tape under one
/// invented tenant.
#[test]
fn units_are_filed_under_their_real_tenants_not_the_operator_placeholder() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    for (unit_name, tenant_name, _) in UNITS {
        let got: String = conn
            .query_row(
                "SELECT t.name FROM units u JOIN tenants t ON t.id = u.tenant_id
                 WHERE u.name = ?1",
                rusqlite::params![unit_name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(&got, tenant_name, "unit {unit_name} was filed under {got}");
    }
    let operator_tenant: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tenants WHERE name = 'operator'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        operator_tenant, 0,
        "\"operator\" is the manifest's placeholder for the all-units envelope, \
         not a tenant a rebuild should create"
    );
}

/// The per-file index exists only in the operator envelope's `catalog.db`.
#[test]
fn the_file_index_comes_back_from_the_operator_catalog_db() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    assert_eq!(report.files, UNITS.len() * 2);
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files f JOIN snapshots s ON s.id = f.snapshot_id
             JOIN units u ON u.id = s.unit_id WHERE u.name = ?1",
            rusqlite::params![UNITS[0].0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);

    // `source_path` is NOT NULL and lives only in catalog.db — a rebuild
    // without it must say so rather than invent one.
    let source: String = conn
        .query_row(
            "SELECT s.source_path FROM snapshots s JOIN units u ON u.id = s.unit_id
             WHERE u.name = ?1",
            rusqlite::params![UNITS[0].0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(source, format!("/src/{}", UNITS[0].0));
}

/// A tape written before #83 carries no `catalog.db`. The restore path must
/// still come back whole — what degrades is the searchable index.
#[test]
fn a_tape_without_a_catalog_db_still_rebuilds_the_restore_path() {
    let mut vol = build_sealed_volume(false);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    assert!(!report.had_catalog_db);
    assert_eq!(report.files, 0);
    assert_eq!(report.units, UNITS.len());

    for (unit_name, expected) in &vol.expected_positions {
        let rows = restore_resolution_query(&conn, unit_name);
        let got: Vec<i64> = rows
            .iter()
            .map(|(_, pos, _)| pos.parse().unwrap())
            .collect();
        assert_eq!(&got, expected, "unit {unit_name}");
    }

    let source: String = conn
        .query_row(
            "SELECT s.source_path FROM snapshots s JOIN units u ON u.id = s.unit_id
             WHERE u.name = ?1",
            rusqlite::params![UNITS[0].0],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        source.contains("unknown"),
        "an unknown source path must say so, not be invented: got {source:?}"
    );
}

/// Idempotence is the property that makes a rebuild safe to run across a
/// shelf of cartridges, twice, in any order.
#[test]
fn rebuilding_the_same_volume_twice_changes_nothing_the_second_time() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let first = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();
    assert!(!first.is_noop());

    let counts_after_first = row_counts(&conn);
    let second = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    assert!(
        second.is_noop(),
        "a second rebuild inserted rows: {second:?}"
    );
    assert_eq!(
        counts_after_first,
        row_counts(&conn),
        "a second rebuild changed the catalog"
    );
}

fn row_counts(conn: &rusqlite::Connection) -> Vec<(String, i64)> {
    [
        "tenants",
        "units",
        "snapshots",
        "stage_sets",
        "stage_slices",
        "volumes",
        "writes",
        "write_positions",
        "files",
    ]
    .iter()
    .map(|t| {
        let n: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
            .unwrap();
        (t.to_string(), n)
    })
    .collect()
}

/// The access-control boundary, and the reason it needs no key introspection:
/// the operator envelope's recipients are operator + escrow only, so a tenant
/// key simply cannot open it.
#[test]
fn a_tenant_key_is_refused_and_pointed_at_the_heir_path() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.tenant_secret.clone();

    let err = rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect_err("a tenant key must not rebuild the operator's catalog");
    let msg = err.to_string();
    assert!(
        msg.contains("neither an operator key nor the escrow key"),
        "the refusal must name the reason: {msg}"
    );
    assert!(
        msg.contains("RESTORE.sh"),
        "a refused tenant must be pointed at the path that does serve them: {msg}"
    );

    assert_eq!(
        row_counts(&conn).iter().map(|(_, n)| *n).sum::<i64>(),
        0,
        "a refused rebuild must leave the catalog untouched"
    );
}

/// A rebuild is a claim about where rows came from, and ADR-0001 has the
/// catalog as a ledger of claims — so provenance is an `events` row, not a
/// column on every table.
#[test]
fn the_rebuild_records_its_provenance_as_an_event() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    let details: String = conn
        .query_row(
            "SELECT details FROM events WHERE action = 'catalog_rebuild'",
            [],
            |r| r.get(0),
        )
        .expect("a catalog_rebuild event");
    assert!(details.contains(LABEL), "{details}");
    assert!(details.contains(VOL_UUID), "{details}");
    assert!(details.contains("catalog.db present"), "{details}");
}

/// The defect this test exists for: `audit` scopes every per-unit check to
/// `status = 'active'` (`cli::audit`'s `list_units(conn, None, Some("active"))`).
/// Rebuilt units were first written as `tape_only`, which made a catalog
/// rebuilt after a disaster report ZERO violations where the catalog it
/// replaced reported three `copy_count` violations for the same units on the
/// same single tape. Under-reporting risk to someone who has just lost their
/// database is the worst direction for this to fail in.
///
/// Pinned through the exact call `audit` makes, not through a status string,
/// so it keeps testing the real scoping if that scoping ever moves.
#[test]
fn rebuilt_units_are_visible_to_the_scope_audit_checks() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    let audited = tapectl::db::queries::list_units(&conn, None, Some("active")).unwrap();
    let mut names: Vec<&str> = audited.iter().map(|u| u.name.as_str()).collect();
    names.sort_unstable();
    let mut expected: Vec<&str> = UNITS.iter().map(|(n, _, _)| *n).collect();
    expected.sort_unstable();
    assert_eq!(
        names, expected,
        "every rebuilt unit must fall inside the scope audit checks, or a \
         rebuilt catalog silently reports fewer violations than the truth"
    );

    // `tape_only` is a policy state `unit mark-tape-only` sets deliberately
    // after checking enforced preconditions. A rebuild has read a tape, not
    // looked at anyone's disk, and must not claim it.
    let tape_only: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM units WHERE status = 'tape_only'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tape_only, 0);
}

/// The `volumes` row must name a backend the config actually declares — an
/// invented name would put a backend in the catalog that nothing can resolve.
#[test]
fn the_rebuilt_volume_names_the_configured_backend() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    let (btype, bname): (String, String) = conn
        .query_row(
            "SELECT backend_type, backend_name FROM volumes WHERE label = ?1",
            rusqlite::params![LABEL],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(btype, "lto");
    assert_eq!(bname, "lto0", "the caller's configured backend name");
}

/// The other half of the ratified access rule. `with_escrow` puts the escrow
/// recipient on every envelope (ADR-0005), so the escrow key must rebuild
/// exactly as the operator key does — this is the key an heir actually holds,
/// printed on the kit `key escrow-kit` generates.
#[test]
fn the_escrow_key_rebuilds_as_well_as_the_operator_key() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.escrow_secret.clone();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect("the escrow key must open the operator envelope");
    assert_eq!(report.units, UNITS.len());
    assert!(report.had_catalog_db);

    for (unit_name, expected) in &vol.expected_positions {
        let rows = restore_resolution_query(&conn, unit_name);
        let got: Vec<i64> = rows
            .iter()
            .map(|(_, pos, _)| pos.parse().unwrap())
            .collect();
        assert_eq!(&got, expected, "unit {unit_name}");
    }
}

/// #137: rebuilt stage sets carry `origin = 'rebuilt'`, which is what lets
/// the escrow predicate say "unknown — attest it" instead of "no recorded
/// recipient list" — the same verdict, a different explanation.
#[test]
fn rebuilt_stage_sets_are_marked_as_such() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    let (rebuilt, total): (i64, i64) = conn
        .query_row(
            "SELECT SUM(origin = 'rebuilt'), COUNT(*) FROM stage_sets",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(total, UNITS.len() as i64);
    assert_eq!(rebuilt, total, "every rebuilt stage set must say so");

    // And through the predicate: unknown, not a gap.
    let fp: Option<String> = conn
        .query_row("SELECT key_fingerprints FROM stage_sets LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        tapectl::policy::escrow::marker(
            fp.as_deref(),
            tapectl::policy::escrow::Origin::Rebuilt,
            Some("age1anything"),
        ),
        "?"
    );
}
