//! Performance/scaling tests for the disk-side of the pipeline.
//!
//! Gated behind `TAPECTL_PERF_TESTS=1` — they do real work (thousands of
//! files, hundreds of DB rows, dar + age encryption) and are too slow to
//! run on every `cargo test` invocation. No tape hardware is required.
//!
//! Scenarios are scaled to a dev VM (≈100 GiB /scratch, no real drive): a
//! 100k-file unit and a 2 GiB write through a `MemStore` (issue #416) are as
//! large as they go; terabyte units need real LTO hardware
//! (`docs/lto6-validation-checklist.md`).
//!
//! Run locally with:
//!   TAPECTL_PERF_TESTS=1 cargo test --test performance --release -- --nocapture
//!
//! Each scenario prints its elapsed time to stderr and asserts a generous
//! wall-clock ceiling. The ceilings catch order-of-magnitude regressions
//! without being flaky on a loaded VM; the eprintln output is how you
//! compare against `docs/perf-baselines.md`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection};
use tempfile::TempDir;

use tapectl::config::{Config, LtoBackendConfig, StagingConfig, TapectlPaths};
use tapectl::{db, staging, tenant, unit};

fn perf_enabled() -> bool {
    std::env::var("TAPECTL_PERF_TESTS").is_ok()
}

fn find_dar() -> String {
    for p in ["/opt/dar/bin/dar", "/usr/local/bin/dar", "/usr/bin/dar"] {
        if Path::new(p).exists() {
            return p.into();
        }
    }
    "dar".into()
}

struct PerfHarness {
    _root: TempDir,
    paths: TapectlPaths,
    conn: Connection,
    config: Config,
    source_root: PathBuf,
}

fn setup(name: &str) -> PerfHarness {
    // Env-overridable with the previous literal as the default, mirroring
    // the gate script's TAPECTL_GATE_SCRATCH (issue #111). Nothing changes
    // for anyone who does not set it; a machine without /scratch can now run
    // the suite without editing the harness.
    let scratch = PathBuf::from(
        std::env::var("TAPECTL_PERF_SCRATCH").unwrap_or_else(|_| "/scratch/tapectl-perf".into()),
    );
    fs::create_dir_all(&scratch).unwrap();
    let root = tempfile::Builder::new()
        .prefix(&format!("{name}-"))
        .tempdir_in(&scratch)
        .unwrap();

    let home = root.path().join("home");
    let staging_dir = root.path().join("staging");
    let source_root = root.path().join("src");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&staging_dir).unwrap();
    fs::create_dir_all(&source_root).unwrap();

    let paths = TapectlPaths::new(home);
    paths.ensure_dirs().unwrap();
    let conn = db::open(&paths.db_file).unwrap();

    // Issue #115 / ADR-0005: `stage_create` refuses without a registered
    // escrow recipient. Registered here, in `setup`, so it is in place
    // before ANY scenario's first `stage_create` by construction — every
    // scenario starts by calling this function, and none of them stages
    // before it returns. Public key only, exactly as production does; the
    // holder is its own tenant, independent of the "op" operator tenant the
    // scenarios add afterwards. Mirrors `tests/mhvtl_e2e.rs`'s harness.
    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('escrow-holder', 0, 'active')",
        [],
    )
    .unwrap();
    let escrow_holder_id = conn.last_insert_rowid();
    let escrow_kp = tapectl::crypto::keys::generate_keypair();
    tapectl::db::queries::insert_escrow_key(
        &conn,
        escrow_holder_id,
        "perf-escrow",
        &escrow_kp.fingerprint,
        &escrow_kp.public_key,
        Some("test escrow recipient (ADR-0005)"),
    )
    .unwrap();

    let mut config = Config::default();
    config.dar.binary = find_dar();
    config.staging = StagingConfig {
        directory: staging_dir.to_string_lossy().into_owned(),
        ..Default::default()
    };
    config.defaults.slice_size = "200M".into();
    config.defaults.compression = "none".into();
    // audit's resolver reaches for an LTO backend name; populate a dummy
    // entry even though these tests never touch real tape.
    config.backends.lto.push(LtoBackendConfig {
        name: "dummy".into(),
        device_tape: "/dev/null".into(),
        device_sg: "/dev/null".into(),
        generation: "LTO-8".into(),
        capacity_override: Some("2400G".into()),
        fill_ceiling: 0.92,
        enospc_buffer: "50M".into(),
    });

    PerfHarness {
        _root: root,
        paths,
        conn,
        config,
        source_root,
    }
}

fn report(scenario: &str, detail: &str, elapsed: Duration) {
    eprintln!(
        "[perf] {scenario:<28} {detail:<40} elapsed={:>8.2}s",
        elapsed.as_secs_f64()
    );
}

// ─────────────────────────────────────────────────────────────────────────────

/// Many small files in one unit — stresses filesystem walk, per-file sha256,
/// dar's file-count scaling, and the manifest-insert transaction path.
#[test]
#[ignore = "perf suite: set TAPECTL_PERF_TESTS=1 and pass --ignored"]
fn perf_many_files_single_unit() {
    if !perf_enabled() {
        eprintln!("skip: TAPECTL_PERF_TESTS not set");
        return;
    }
    let n_files: usize = std::env::var("TAPECTL_PERF_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5_000);

    let h = setup("many-files");
    tenant::add_tenant(&h.conn, &h.paths, "op", None, true).unwrap();

    let dir = h.source_root.join("many");
    fs::create_dir_all(&dir).unwrap();
    let t = Instant::now();
    for i in 0..n_files {
        fs::write(dir.join(format!("f_{i:06}.bin")), format!("file {i}\n")).unwrap();
    }
    report(
        "many_files",
        &format!("create {n_files} files"),
        t.elapsed(),
    );

    let t = Instant::now();
    unit::init_unit(
        &h.conn,
        &h.paths,
        &dir.to_string_lossy(),
        "op",
        Some("many-unit"),
        &[],
        None,
    )
    .unwrap();
    let sid = staging::snapshot_create(&h.conn, "many-unit", &Config::default()).unwrap();
    report("many_files", "init_unit + snapshot_create", t.elapsed());

    let t = Instant::now();
    staging::stage_create(&h.conn, &h.paths, &h.config, sid, true).unwrap();
    let stage_elapsed = t.elapsed();
    report("many_files", "stage_create (dar + age)", stage_elapsed);

    // Loose ceiling: if this ever takes more than 10 min on a dev VM,
    // something has broken catastrophically.
    assert!(
        stage_elapsed < Duration::from_secs(600),
        "stage_create for {n_files} files took {:?}, >10min ceiling",
        stage_elapsed
    );
}

/// Many units in the database — stresses catalog/audit queries whose cost
/// scales with the unit count. No staging, no tape. Pure DB access path.
#[test]
#[ignore = "perf suite: set TAPECTL_PERF_TESTS=1 and pass --ignored"]
fn perf_many_units_audit() {
    if !perf_enabled() {
        eprintln!("skip: TAPECTL_PERF_TESTS not set");
        return;
    }
    let n_units: usize = std::env::var("TAPECTL_PERF_UNITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);

    let h = setup("many-units");

    // Bulk-insert units directly: init_unit requires a real directory and
    // dotfile per unit, which would dominate the measurement. For a DB-only
    // scaling signal we want the raw query cost, not filesystem overhead.
    h.conn
        .execute(
            "INSERT INTO tenants (name, is_operator, status)
             VALUES ('op', 1, 'active')",
            [],
        )
        .unwrap();
    let tenant_id = h.conn.last_insert_rowid();

    let t = Instant::now();
    let tx = h.conn.unchecked_transaction().unwrap();
    for i in 0..n_units {
        tx.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status, current_path)
             VALUES (?1, ?2, ?3, 'sha256', 1, 'active', ?4)",
            params![
                format!("uuid-{i:06}"),
                format!("unit-{i:06}"),
                tenant_id,
                format!("/nonexistent/unit-{i:06}"),
            ],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    report(
        "many_units",
        &format!("insert {n_units} units"),
        t.elapsed(),
    );

    // Replicate the O(units) query pattern `tapectl audit` walks per unit.
    // We call the underlying queries directly so the test produces no
    // stdout noise and measures the dominant scaling cost — one policy
    // resolve plus one copy-count query per unit — rather than also
    // including findings serialization.
    let t = Instant::now();
    let units = tapectl::db::queries::list_units(&h.conn, None, Some("active")).unwrap();
    for unit in &units {
        let _policy = tapectl::policy::resolve(&h.conn, &h.config, unit);
        let _copy_count: i64 = h
            .conn
            .query_row(
                "SELECT COUNT(DISTINCT w.volume_id)
                 FROM writes w
                 JOIN stage_sets ss ON ss.id = w.stage_set_id
                 JOIN snapshots s ON s.id = ss.snapshot_id
                 WHERE s.unit_id = ?1 AND s.status = 'current' AND w.status = 'completed'",
                params![unit.id],
                |row| row.get(0),
            )
            .unwrap();
    }
    let audit_elapsed = t.elapsed();
    report(
        "many_units",
        &format!("audit core loop ({} units)", units.len()),
        audit_elapsed,
    );

    assert!(
        audit_elapsed < Duration::from_secs(300),
        "audit for {n_units} units took {:?}, >5min ceiling",
        audit_elapsed
    );
}

/// One large file in a unit — stresses streaming sha256, dar single-slice
/// path, and age encryption throughput.
#[test]
#[ignore = "perf suite: set TAPECTL_PERF_TESTS=1 and pass --ignored"]
fn perf_large_single_file() {
    if !perf_enabled() {
        eprintln!("skip: TAPECTL_PERF_TESTS not set");
        return;
    }
    // 500 MB default; override via env for larger runs on roomier disks.
    // The design doc's 2+ TB target needs real hardware — documented in
    // docs/perf-baselines.md.
    let size_mb: usize = std::env::var("TAPECTL_PERF_LARGE_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);

    let h = setup("large-file");
    tenant::add_tenant(&h.conn, &h.paths, "op", None, true).unwrap();

    let dir = h.source_root.join("big");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("big.bin");

    let t = Instant::now();
    // Write in 4 MB chunks with a rotating pattern so dar can't trivially
    // compress it away even if compression is accidentally enabled.
    let chunk_size = 4 * 1024 * 1024;
    let total_bytes = size_mb * 1024 * 1024;
    let chunks = total_bytes / chunk_size;
    let mut buf = vec![0u8; chunk_size];
    {
        use std::io::Write as _;
        let mut f = fs::File::create(&path).unwrap();
        for c in 0..chunks {
            for (i, b) in buf.iter_mut().enumerate() {
                *b = ((c.wrapping_mul(31) + i) & 0xff) as u8;
            }
            f.write_all(&buf).unwrap();
        }
    }
    report("large_file", &format!("write {size_mb} MB"), t.elapsed());

    let t = Instant::now();
    unit::init_unit(
        &h.conn,
        &h.paths,
        &dir.to_string_lossy(),
        "op",
        Some("big-unit"),
        &[],
        None,
    )
    .unwrap();
    let sid = staging::snapshot_create(&h.conn, "big-unit", &Config::default()).unwrap();
    report("large_file", "init_unit + snapshot_create", t.elapsed());

    let t = Instant::now();
    staging::stage_create(&h.conn, &h.paths, &h.config, sid, true).unwrap();
    let stage_elapsed = t.elapsed();
    let mib_per_sec = (size_mb as f64) / stage_elapsed.as_secs_f64();
    report(
        "large_file",
        &format!("stage_create ({mib_per_sec:.1} MiB/s)"),
        stage_elapsed,
    );

    assert!(
        stage_elapsed < Duration::from_secs(600),
        "stage_create for {size_mb} MB took {:?}, >10min ceiling",
        stage_elapsed
    );
}

/// Issue #413: a fresh catalog takes a 184k-file version — L6-0001's size —
/// in seconds. This is the write `catalog rebuild` makes after a DR `init`,
/// and the first big `snapshot create` on a new install, through the one
/// function both use. Row by row it went superlinear (306 s measured):
/// each statement flushed the search index. Then the version goes on tape
/// in the `catalog.db` shape and is streamed back the way a rebuild reads
/// it, both timed.
#[test]
#[ignore = "perf suite: set TAPECTL_PERF_TESTS=1 and pass --ignored"]
fn perf_fresh_catalog_takes_a_184k_file_version() {
    if !perf_enabled() {
        eprintln!("skip: TAPECTL_PERF_TESTS not set");
        return;
    }
    // L6-0001's file count by default; the 10 s ceiling is for that size.
    let files: usize = std::env::var("TAPECTL_PERF_CATALOG_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(184_552);
    let root = TempDir::new().unwrap();
    // `db::open`, not an in-memory catalog: the migrations and the
    // `PRAGMA optimize` statistics a DR `init` leaves are part of the case.
    let conn = db::open(&root.path().join("tapectl.db")).unwrap();
    conn.execute_batch(
        "INSERT INTO tenants (id, name, is_operator, status) VALUES (1, 't', 1, 'active');
         INSERT INTO units (id, uuid, name, tenant_id) VALUES (1, 'u', 'u', 1);
         INSERT INTO snapshots (id, unit_id, version, status, source_path)
              VALUES (1, 1, 1, 'staged', '/s');
         INSERT INTO stage_sets (id, snapshot_id, status, slice_size) VALUES (1, 1, 'staged', 1);",
    )
    .unwrap();
    let entries = (0..files).map(|i| {
        Ok(db::files::FileEntry {
            path: format!("{}/{}/IMG_{i:06}.jpg", i % 97, i % 13),
            kind: db::files::FileKind::Regular,
            size_bytes: i as i64,
            mtime_ns: Some(1_700_000_000_000_000_000 + i as i64 * 1_000_000_000),
            sha256: Some([(i % 251) as u8; 32]),
            link_target: None,
        })
    });

    let start = Instant::now();
    let tx = db::busy::immediate_tx(&conn).unwrap();
    let written = db::files::insert_version(&tx, 1, entries).unwrap();
    tx.commit().unwrap();
    let insert = start.elapsed();
    assert_eq!(written, files);
    report("fresh catalog insert", &format!("{files} rows"), insert);

    let out = root.path().join("catalog.db");
    let start = Instant::now();
    db::ontape_catalog::write(&conn, &[1], &out).unwrap();
    report(
        "catalog.db build",
        &format!("{files} rows"),
        start.elapsed(),
    );

    // The way `catalog rebuild` reads it back: everything but `files`, then
    // each version's rows streamed as one range of ids.
    let start = Instant::now();
    let (ontape, _) = db::ontape_catalog::read_all_but_files(&out).unwrap();
    let spans = db::ontape_catalog::file_id_spans(&ontape).unwrap();
    let (first, last) = spans[&1];
    let mut stmt = ontape
        .prepare(db::ontape_catalog::FILES_OF_SNAPSHOT)
        .unwrap();
    let rows = stmt
        .query_map(params![1, first, last], db::ontape_catalog::file_row)
        .unwrap()
        .count();
    assert_eq!(rows, files);
    report(
        "catalog.db stream",
        &format!("{files} rows"),
        start.elapsed(),
    );

    assert!(
        files > 184_552 || insert < Duration::from_secs(10),
        "a {files}-file version into a fresh catalog took {insert:?}: issue #413's \
         superlinear insert is back"
    );
}

/// ADR-0012 amendment 2026-10-07, item 10 (#413): the whole of `catalog
/// rebuild` over a tape whose `catalog.db` carries L6-0001's 184,552 file
/// rows -- the DR path after `init`, end to end. The tape is the real write
/// session's (build -> validate -> plan -> execute -> seal -> confirm) into a
/// `MemStore`, so the rebuild decrypts a real operator envelope, opens the
/// `catalog.db` inside it and streams every row into the shared insert path
/// (`db::files::insert_version`) the test above times alone. One ceiling,
/// on `rebuild_from_store` as a whole; the write that makes the tape is
/// reported, not bounded.
///
/// Measured 2026-10-07 on vm-desk1 (see `docs/perf-baselines.md`):
/// the rebuild took 17.4 s in a debug build and 5.3 s in release, the build
/// this suite runs in. The ceiling is 20 s: about four times the release
/// figure, clear of a shared VM's noise, and far under the row-by-row
/// insert #413 removed (157.7 s for the insert alone).
#[test]
#[ignore = "perf suite: set TAPECTL_PERF_TESTS=1 and pass --ignored"]
fn perf_rebuild_from_a_tape_carrying_a_184k_file_version() {
    use tapectl::crypto::keys::generate_keypair;
    use tapectl::store::{MemStore, Tier};
    use tapectl::tape::contact::{ContactSite, Medium, Operation};
    use tapectl::volume::build::{self, BuildInputs, BuildSlice, BuildUnit, TenantInfo};
    use tapectl::volume::layout_model::{KeyAvailability, SliceCheck};
    use tapectl::volume::rebuild;
    use tapectl::volume::session::{ConfirmOutcome, ExecuteOutcome};

    if !perf_enabled() {
        eprintln!("skip: TAPECTL_PERF_TESTS not set");
        return;
    }
    let files: usize = std::env::var("TAPECTL_PERF_CATALOG_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(184_552);
    const BS: u64 = 65536;
    const LABEL: &str = "PERF-REBUILD";
    const CEILING_SECS: u64 = 20;

    let root = TempDir::new().unwrap();
    let src = db::open(&root.path().join("src.db")).unwrap();
    let operator = generate_keypair();
    let tenant = generate_keypair();
    let escrow = generate_keypair();
    src.execute_batch(
        "INSERT INTO tenants (id, name, is_operator, status) VALUES (1, 'operator', 1, 'active');
         INSERT INTO tenants (id, name, is_operator, status) VALUES (2, 'alpha', 0, 'active');",
    )
    .unwrap();
    for (tenant_id, kp) in [(1, &operator), (2, &tenant)] {
        src.execute(
            "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key, key_type,
                                          is_active)
             VALUES (?1, ?2, ?3, ?4, 'primary', 1)",
            params![
                tenant_id,
                format!("k{tenant_id}"),
                kp.fingerprint,
                kp.public_key
            ],
        )
        .unwrap();
    }
    let recipients = vec![
        tenant.public_key.clone(),
        operator.public_key.clone(),
        escrow.public_key.clone(),
    ];
    src.execute(
        "INSERT INTO units (id, uuid, name, tenant_id, current_path, status)
         VALUES (1, 'perf-unit-uuid', 'photos/l6-0001', 2, '/src/photos', 'active')",
        [],
    )
    .unwrap();
    src.execute(
        "INSERT INTO snapshots (id, unit_id, version, status, source_path, file_count, total_size)
         VALUES (1, 1, 1, 'staged', '/src/photos', ?1, ?2)",
        params![files as i64, files as i64],
    )
    .unwrap();
    src.execute(
        "INSERT INTO stage_sets (id, snapshot_id, status, slice_size, key_fingerprints)
         VALUES (1, 1, 'staged', 10485760, ?1)",
        params![serde_json::to_string(&recipients).unwrap()],
    )
    .unwrap();
    src.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes,
                              status)
         VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
        params![LABEL],
    )
    .unwrap();
    let volume_id = src.last_insert_rowid();

    let start = Instant::now();
    let entries = (0..files).map(|i| {
        Ok(db::files::FileEntry {
            path: format!("{}/{}/IMG_{i:06}.jpg", i % 97, i % 13),
            kind: db::files::FileKind::Regular,
            size_bytes: i as i64,
            mtime_ns: Some(1_700_000_000_000_000_000 + i as i64 * 1_000_000_000),
            sha256: Some([(i % 251) as u8; 32]),
            link_target: None,
        })
    });
    let tx = db::busy::immediate_tx(&src).unwrap();
    assert_eq!(db::files::insert_version(&tx, 1, entries).unwrap(), files);
    tx.commit().unwrap();

    let slice_dir = root.path().join("slices");
    fs::create_dir_all(&slice_dir).unwrap();
    let plaintext = b"perf rebuild payload".to_vec();
    let encrypted = staging::encrypt_data(&plaintext, &recipients).unwrap();
    let sha = |b: &[u8]| {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(b))
    };
    let slice_path = slice_dir.join("slice_1.age");
    fs::write(&slice_path, &encrypted).unwrap();
    src.execute(
        "INSERT INTO stage_slices (id, stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                   sha256_plain, sha256_encrypted, staging_path)
         VALUES (1, 1, 1, ?1, ?2, ?3, ?4, ?5)",
        params![
            plaintext.len() as i64,
            encrypted.len() as i64,
            sha(&plaintext),
            sha(&encrypted),
            slice_path.to_string_lossy()
        ],
    )
    .unwrap();
    let units = vec![BuildUnit {
        stage_set_id: 1,
        snapshot_id: 1,
        unit_name: "photos/l6-0001".to_string(),
        unit_uuid: "perf-unit-uuid".to_string(),
        tenant_id: 2,
        dar_version: Some("2.7.20".to_string()),
        dar_command: None,
        catalog_path: None,
        snapshot_version: 1,
        slices: vec![BuildSlice {
            slice_id: 1,
            slice_number: 1,
            size_bytes: plaintext.len() as i64,
            encrypted_bytes: encrypted.len() as i64,
            sha256_plain: sha(&plaintext),
            sha256_encrypted: sha(&encrypted),
            staging_path: slice_path,
        }],
    }];

    let catalog_db = root.path().join("catalog.db");
    db::catalog_snapshot::build_catalog_snapshot(&src, &[1], &catalog_db).unwrap();
    let catalog_db_bytes = fs::metadata(&catalog_db).unwrap().len();

    let inputs = BuildInputs {
        label: LABEL.to_string(),
        volume_uuid: "77777777-8888-9999-aaaa-bbbbbbbbbbbb".to_string(),
        media_type: "LTO-6".to_string(),
        tapectl_version: "perf".to_string(),
        created_at: "2026-10-07T00:00:00Z".to_string(),
        block_size: BS,
        usable_bytes: 4 * 1024 * 1024 * 1024,
        enospc_buffer: 1024 * 1024,
        nominal_capacity: 2_500_000_000_000,
        mam_capacity: 2_500_000_000_000,
        mam_manufacturer: "TAPECTL-PERF".to_string(),
        mam_serial: "PERFSERIAL".to_string(),
        cartridge_identity_source: Some("mam".to_string()),
        mam_length: 0,
        mam_loads: 0,
        units: units.clone(),
        tenants: vec![TenantInfo {
            tenant_id: 2,
            tenant_name: "alpha".to_string(),
            public_keys: vec![tenant.public_key.clone()],
        }],
        operator_public_keys: vec![operator.public_key.clone()],
        escrow_public_key: Some(escrow.public_key.clone()),
        catalog_db_path: Some(catalog_db),
    };
    let session_dir = root.path().join("session");
    fs::create_dir_all(&session_dir).unwrap();
    let built = build::build(&inputs, &session_dir).expect("build");
    let keys = KeyAvailability {
        tenant_ids: [2].into_iter().collect(),
        tenants_with_active_key: [2].into_iter().collect(),
        operator_key_present: true,
        escrow_recipient_present: None,
        stage_sets_lacking_escrow: None,
    };
    let mut store = MemStore::new(BS as usize);
    let validated = built
        .into_validated(&keys, SliceCheck::Size, &mut store)
        .expect("validate");
    let planned = validated.plan(&src, volume_id, &units).expect("plan");
    let ready = match planned.execute(&src, &mut store).expect("execute") {
        ExecuteOutcome::Ready(r) => r,
        _ => panic!("the write must reach Ready"),
    };
    let sealed = ready.seal(&mut store).expect("seal");
    assert!(matches!(
        sealed
            .confirm(&src, &mut store, Tier::Integrity)
            .expect("confirm"),
        ConfirmOutcome::Sealed(_)
    ));
    report(
        "rebuild fixture: insert + catalog.db + write",
        &format!("{files} rows, catalog.db {} MiB", catalog_db_bytes >> 20),
        start.elapsed(),
    );

    // The disaster: a fresh catalog, the operator key, and the tape.
    let dst = db::open(&root.path().join("rebuilt.db")).unwrap();
    let identity: age::x25519::Identity = operator.secret_key.parse().unwrap();
    let cfg = Config::default();
    let site = ContactSite::new(
        &cfg,
        Operation::CatalogRebuild,
        "memstore",
        Medium::NoBackend,
    );
    let scratch = root.path().join("scratch");
    fs::create_dir_all(&scratch).unwrap();

    let start = Instant::now();
    let rebuilt = rebuild::rebuild_from_store(
        &dst,
        &mut store,
        &[identity],
        Some(LABEL),
        "recovered",
        Some("lto0"),
        &scratch,
        "memstore",
        site,
    )
    .expect("rebuild");
    let elapsed = start.elapsed();
    report("catalog rebuild", &format!("{files} rows"), elapsed);

    assert_eq!(rebuilt.units, 1, "{rebuilt:?}");
    let rows: i64 = dst
        .query_row("SELECT COUNT(*) FROM file_versions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows as usize, files, "every file row came back");
    assert!(
        files > 184_552 || elapsed < Duration::from_secs(CEILING_SECS),
        "catalog rebuild of a {files}-file version took {elapsed:?}, over the \
         {CEILING_SECS} s ceiling (ADR-0012 amendment 2026-10-07 item 10)"
    );
}

// ── issue #416 items 9 and 11: costs as ratios, at production's shape ──

/// Statements SQLite ran while tracing was on (`Connection::trace_v2`), as
/// `tests/catalog_budget.rs` counts them: a trigger's sub-program is part of
/// the statement that fired it.
static STATEMENTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn count_statement(event: rusqlite::trace::TraceEvent<'_>) {
    if let rusqlite::trace::TraceEvent::Stmt(_, sql) = event {
        if !sql.trim_start().starts_with("--") {
            STATEMENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Run `f` with every statement `conn` runs counted; return its result, the
/// count and the time it took.
fn counted<T>(conn: &Connection, f: impl FnOnce() -> T) -> (T, u64, Duration) {
    use rusqlite::trace::TraceEventCodes;
    use std::sync::atomic::Ordering;
    STATEMENTS.store(0, Ordering::Relaxed);
    conn.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(count_statement));
    let start = Instant::now();
    let out = f();
    let elapsed = start.elapsed();
    conn.trace_v2(TraceEventCodes::empty(), None);
    (out, STATEMENTS.load(Ordering::Relaxed), elapsed)
}

/// This process's peak resident set (`VmHWM`), in KiB.
fn peak_rss_kib() -> u64 {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let line = status
        .lines()
        .find(|l| l.starts_with("VmHWM:"))
        .expect("VmHWM in /proc/self/status");
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

/// Start `VmHWM` over from the current resident set (writing `5` to
/// `clear_refs`, Linux 4.0+), so a scenario's peak is its own and not the
/// one an earlier scenario in this process left.
fn reset_peak_rss() {
    fs::write("/proc/self/clear_refs", "5").expect("reset VmHWM through /proc/self/clear_refs");
}

/// What one `n`-file unit cost to snapshot and stage: statements and time
/// for each step, and the peak resident set across both.
struct UnitCost {
    /// Everything the walk records: the files, the directories they sit
    /// in, and the unit's own dotfile.
    entries: u64,
    snapshot: (u64, Duration),
    stage: (u64, Duration),
    peak_kib: u64,
}

/// Files and directories under `dir`, not counting `dir`.
fn entries_under(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            1 + if e.file_type().unwrap().is_dir() {
                entries_under(&e.path())
            } else {
                0
            }
        })
        .sum()
}

fn unit_of_files(n: usize) -> UnitCost {
    let h = setup(&format!("files-{n}"));
    tenant::add_tenant(&h.conn, &h.paths, "op", None, true).unwrap();
    let dir = h.source_root.join("unit");
    for i in 0..n {
        let sub = dir.join(format!("d{:03}", i / 1000));
        if i % 1000 == 0 {
            fs::create_dir_all(&sub).unwrap();
        }
        fs::write(sub.join(format!("f_{i:06}.txt")), format!("file {i}\n")).unwrap();
    }
    unit::init_unit_with_config(
        &h.conn,
        &h.config,
        &dir.to_string_lossy(),
        "op",
        Some("files-unit"),
        &[],
        None,
    )
    .unwrap();

    reset_peak_rss();
    let (sid, snap_stmts, snap_time) = counted(&h.conn, || {
        staging::snapshot_create(&h.conn, "files-unit", &h.config).unwrap()
    });
    let ((), stage_stmts, stage_time) = counted(&h.conn, || {
        staging::stage_create(&h.conn, &h.paths, &h.config, sid, true).unwrap();
    });
    let cost = UnitCost {
        entries: entries_under(&dir),
        snapshot: (snap_stmts, snap_time),
        stage: (stage_stmts, stage_time),
        peak_kib: peak_rss_kib(),
    };
    report(
        "files_unit",
        &format!("snapshot_create {n} files, {snap_stmts} stmts"),
        snap_time,
    );
    report(
        "files_unit",
        &format!("stage_create {n} files, {stage_stmts} stmts"),
        stage_time,
    );
    eprintln!(
        "[perf] files_unit {n} files: VmHWM {} MiB",
        cost.peak_kib / 1024
    );
    cost
}

/// Issue #416 item 11: a 100,000-file unit, snapshotted and staged, against
/// the same unit at a tenth of the size. Each step runs a bounded number of
/// statements beyond one per entry the walk records, files and directories
/// alike (an N+1 shows as 2N), scales linearly in
/// time (a superlinear step, such as #413's, shows as ~100x for 10x the
/// files), and the process stays under 512 MiB resident: the walk and the
/// manifest stream, they do not hold the unit in memory. dar runs in its
/// own process, so its memory is not in the figure.
#[test]
#[ignore = "perf suite: set TAPECTL_PERF_TESTS=1 and pass --ignored"]
fn perf_a_100k_file_unit_scales_linearly() {
    if !perf_enabled() {
        eprintln!("skip: TAPECTL_PERF_TESTS not set");
        return;
    }
    let n: usize = std::env::var("TAPECTL_PERF_UNIT_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let small = unit_of_files(n / 10);
    let large = unit_of_files(n);

    for (step, (stmts, _)) in [
        ("snapshot_create", large.snapshot),
        ("stage_create", large.stage),
    ] {
        assert!(
            stmts <= large.entries + 50,
            "{step} of a {n}-file unit ran {stmts} statements, over N + 50 \
             (N = {} entries)",
            large.entries
        );
    }
    // Ten times the files may cost up to twenty times the time: a linear
    // step with its fixed costs is under ten; a quadratic one is ~100.
    for (step, (_, t_small), (_, t_large)) in [
        ("snapshot_create", small.snapshot, large.snapshot),
        ("stage_create", small.stage, large.stage),
    ] {
        let growth = t_large.as_secs_f64() / t_small.as_secs_f64().max(1e-3);
        eprintln!("[perf] files_unit {step}: {growth:.1}x the time for 10x the files");
        assert!(
            growth <= 20.0,
            "{step}: {n} files took {growth:.1}x the time of {}: not linear",
            n / 10
        );
    }
    assert!(
        large.peak_kib <= 512 * 1024,
        "a {n}-file unit peaked at {} MiB resident, over 512 MiB",
        large.peak_kib / 1024
    );
}

/// Issue #416 item 9: a 2 GiB write and full confirm through the real write
/// session into a `MemStore`, timed against this machine's own sha256 speed
/// rather than a wall-clock ceiling. The write hashes every byte once on the
/// way to the store and the confirm once more on the way back
/// (`tests/pass_budget.rs` pins those counts): two hash passes, which the
/// calibration times on one 256 MiB buffer. The ceiling allows three times
/// that, for the reads, the copies and the queues between them: it bounds
/// throughput against the machine's own sha256, on a fast machine or a
/// loaded one, and catches an order-of-magnitude slowdown. One added pass or
/// a pipeline that stopped overlapping stays under it; the exact pass
/// counts are `tests/pass_budget.rs`'s job, and the ratio printed here is
/// the figure to compare with `docs/perf-baselines.md`. Measured 2026-10-12 on vm-desk1, release:
/// 0.82x (the write's hash overlaps its reads, and one calibration run on a
/// shared VM is noisy, so under 1.0 is normal).
#[test]
#[ignore = "perf suite: set TAPECTL_PERF_TESTS=1 and pass --ignored"]
fn perf_a_2gib_write_and_full_confirm_through_a_store() {
    use sha2::{Digest, Sha256};
    use tapectl::crypto::keys::generate_keypair;
    use tapectl::store::{MemStore, Tier};
    use tapectl::volume::build::{self, BuildInputs, BuildSlice, BuildUnit, TenantInfo};
    use tapectl::volume::layout_model::{KeyAvailability, SliceCheck};
    use tapectl::volume::session::{ConfirmOutcome, ExecuteOutcome};

    if !perf_enabled() {
        eprintln!("skip: TAPECTL_PERF_TESTS not set");
        return;
    }
    let mib: usize = std::env::var("TAPECTL_PERF_WRITE_MIB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2048);
    const SLICE_MIB: usize = 256;
    const BS: u64 = 512 * 1024;
    const CEILING: f64 = 3.0;
    let slices = mib.div_ceil(SLICE_MIB);

    let h = setup("write-2g");
    let operator = generate_keypair();
    let tenant = generate_keypair();
    let escrow = generate_keypair();
    h.conn
        .execute_batch(
            "INSERT INTO tenants (id, name, is_operator, status) VALUES (11, 'w-op', 1, 'active');
             INSERT INTO tenants (id, name, is_operator, status)
                  VALUES (12, 'w-alpha', 0, 'active');
             INSERT INTO units (id, uuid, name, tenant_id, current_path, status)
                  VALUES (11, 'write-2g', 'write-2g', 12, '/src/w', 'active');
             INSERT INTO snapshots (id, unit_id, version, status, source_path)
                  VALUES (11, 11, 1, 'staged', '/src/w');",
        )
        .unwrap();
    for (tenant_id, kp) in [(11, &operator), (12, &tenant)] {
        h.conn
            .execute(
                "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key,
                                              key_type, is_active)
                 VALUES (?1, ?2, ?3, ?4, 'primary', 1)",
                params![
                    tenant_id,
                    format!("w{tenant_id}"),
                    kp.fingerprint,
                    kp.public_key
                ],
            )
            .unwrap();
    }
    h.conn
        .execute(
            "INSERT INTO stage_sets (id, snapshot_id, status, slice_size)
             VALUES (11, 11, 'staged', ?1)",
            params![(SLICE_MIB << 20) as i64],
        )
        .unwrap();
    h.conn
        .execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES ('PERF-2G', 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
            [],
        )
        .unwrap();
    let volume_id = h.conn.last_insert_rowid();

    // The slices, each its own pseudo-random bytes. The first is hashed in
    // memory as it is made: the calibration, sha256 on this machine now.
    let slice_dir = Path::new(&h.config.staging.directory).join("write-2g");
    fs::create_dir_all(&slice_dir).unwrap();
    let mut built_slices = Vec::new();
    let mut rate = 0.0f64; // bytes per second
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let mut buf = vec![0u8; SLICE_MIB << 20];
    for n in 1..=slices {
        for word in buf.chunks_exact_mut(8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            word.copy_from_slice(&x.to_le_bytes());
        }
        let started = Instant::now();
        let sha = format!("{:x}", Sha256::digest(&buf));
        if n == 1 {
            rate = buf.len() as f64 / started.elapsed().as_secs_f64();
        }
        let path = slice_dir.join(format!("slice_{n}.age"));
        fs::write(&path, &buf).unwrap();
        h.conn
            .execute(
                "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes,
                                           encrypted_bytes, sha256_plain, sha256_encrypted,
                                           staging_path)
                 VALUES (11, ?1, ?2, ?2, ?3, ?3, ?4)",
                params![n as i64, buf.len() as i64, sha, path.to_string_lossy()],
            )
            .unwrap();
        built_slices.push(BuildSlice {
            slice_id: h.conn.last_insert_rowid(),
            slice_number: n as i64,
            size_bytes: buf.len() as i64,
            encrypted_bytes: buf.len() as i64,
            sha256_plain: sha.clone(),
            sha256_encrypted: sha,
            staging_path: path,
        });
    }
    drop(buf);
    let bytes = (slices * (SLICE_MIB << 20)) as f64;
    eprintln!(
        "[perf] write_2g calibration: sha256 at {:.0} MiB/s",
        rate / (1u64 << 20) as f64
    );

    let units = vec![BuildUnit {
        stage_set_id: 11,
        snapshot_id: 11,
        unit_name: "write-2g".to_string(),
        unit_uuid: "write-2g".to_string(),
        tenant_id: 12,
        dar_version: Some("2.7.20".to_string()),
        dar_command: None,
        catalog_path: None,
        snapshot_version: 1,
        slices: built_slices,
    }];
    let inputs = BuildInputs {
        label: "PERF-2G".to_string(),
        volume_uuid: "41641600-0000-4000-8000-000000000009".to_string(),
        media_type: "LTO-6".to_string(),
        tapectl_version: "perf".to_string(),
        created_at: "2026-10-12T00:00:00Z".to_string(),
        block_size: BS,
        usable_bytes: 64 * 1024 * 1024 * 1024,
        enospc_buffer: 1024 * 1024,
        nominal_capacity: 2_500_000_000_000,
        mam_capacity: 2_500_000_000_000,
        mam_manufacturer: "TAPECTL-PERF".to_string(),
        mam_serial: "PERF2GSERIAL".to_string(),
        cartridge_identity_source: Some("mam".to_string()),
        mam_length: 0,
        mam_loads: 0,
        units: units.clone(),
        tenants: vec![TenantInfo {
            tenant_id: 12,
            tenant_name: "w-alpha".to_string(),
            public_keys: vec![tenant.public_key.clone()],
        }],
        operator_public_keys: vec![operator.public_key.clone()],
        escrow_public_key: Some(escrow.public_key.clone()),
        catalog_db_path: None,
    };
    let session_dir = slice_dir.join("session");
    fs::create_dir_all(&session_dir).unwrap();
    let built = build::build(&inputs, &session_dir).expect("build");
    let keys = KeyAvailability {
        tenant_ids: vec![12],
        tenants_with_active_key: [12].into_iter().collect(),
        operator_key_present: true,
        escrow_recipient_present: None,
        stage_sets_lacking_escrow: None,
    };
    let mut store = MemStore::new(BS as usize);

    let start = Instant::now();
    let validated = built
        .into_validated(&keys, SliceCheck::Size, &mut store)
        .expect("validate");
    let planned = validated.plan(&h.conn, volume_id, &units).expect("plan");
    let ready = match planned.execute(&h.conn, &mut store).expect("execute") {
        ExecuteOutcome::Ready(r) => r,
        _ => panic!("the write must reach Ready"),
    };
    let sealed = ready.seal(&mut store).expect("seal");
    let written = start.elapsed();
    assert!(matches!(
        sealed
            .confirm(&h.conn, &mut store, Tier::Integrity)
            .expect("confirm"),
        ConfirmOutcome::Sealed(_)
    ));
    let elapsed = start.elapsed();

    let one_pass = bytes / rate;
    let ratio = elapsed.as_secs_f64() / (2.0 * one_pass);
    report(
        "write_2g",
        &format!(
            "write {:.2}x one hash pass",
            written.as_secs_f64() / one_pass
        ),
        written,
    );
    report(
        "write_2g",
        &format!("write + full confirm {ratio:.2}x two passes"),
        elapsed,
    );
    assert!(
        ratio <= CEILING,
        "a {mib} MiB write and full confirm took {elapsed:?}, {ratio:.2}x the two sha256 \
         passes it needs ({one_pass:.1} s each at this machine's rate), over {CEILING}x"
    );
}
