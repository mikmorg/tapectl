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

/// `TAPECTL_PERF_SCRATCH`, or the harness default: where every scenario's
/// temporary tree, and the staging scenario's reusable fixture, live.
fn perf_scratch() -> PathBuf {
    PathBuf::from(
        std::env::var("TAPECTL_PERF_SCRATCH").unwrap_or_else(|_| "/scratch/tapectl-perf".into()),
    )
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
    let scratch = perf_scratch();
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

    // The way `catalog rebuild` reads it back: everything but the per-file
    // rows, then each version's rows streamed as one range of keys.
    let start = Instant::now();
    let (ontape, cat) = db::ontape_catalog::read_all_but_files(&out).unwrap();
    let spans = db::ontape_catalog::FileSpans::read(&ontape, cat.generation).unwrap();
    let rows = spans
        .stream(&ontape, 1, |entries| {
            let mut n = 0;
            for e in entries {
                e?;
                n += 1;
            }
            Ok(n)
        })
        .unwrap();
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
         VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
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
        index: None,
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
             VALUES ('PERF-2G', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
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
        index: None,
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

// ─────────────────────────────────────────────────────────────────────────────
// Issue #364: what staging costs, pass by pass.

/// xorshift64*: a generator written out here, not taken from `rand`, so the
/// fixture's bytes are the same on every machine and every release of any
/// crate. Fast (about a byte per cycle) and, for this purpose, incompressible.
struct Xorshift(u64);

impl Xorshift {
    fn new(seed: u64) -> Self {
        Xorshift(seed | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn fill(&mut self, buf: &mut [u8]) {
        for word in buf.chunks_exact_mut(8) {
            word.copy_from_slice(&self.next().to_le_bytes());
        }
    }
}

/// Bump when the generator or the layout below changes, so a fixture an
/// older generator left is rebuilt and not reused.
const STAGE_FIXTURE_FORMAT: u32 = 1;
/// Disk reads that are not the source: the test binary's own pages, the
/// catalog, dar's and the helpers' libraries. About 45 MiB measured; this
/// much is allowed on top of the ratio, so a small fixture is not mistaken
/// for a second read.
const STAGE_READ_SLACK: u64 = 128 << 20;
/// "A few large files": three, together three quarters of the source.
const STAGE_FIXTURE_LARGE_FILES: u64 = 3;
/// Small files per directory, so a few thousand of them are not one listing.
const STAGE_FIXTURE_PER_DIR: u64 = 100;

/// One file of the staging fixture.
struct FixtureFile {
    rel: PathBuf,
    size: u64,
    seed: u64,
}

fn fixture_seed(n: u64) -> u64 {
    0x9E37_79B9_7F4A_7C15u64.wrapping_mul(n + 1)
}

/// The files of a `total_mib` source of which `small_files` are small: the
/// large ones share three quarters of it, the small ones the other quarter,
/// each between a quarter and seven quarters of the small files' mean (so
/// 64 to 448 KiB at the defaults). Pure arithmetic on the knobs, so a
/// fixture on disk can be checked against it.
fn stage_fixture_plan(total_mib: u64, small_files: u64) -> Vec<FixtureFile> {
    let total = total_mib << 20;
    let small_total = if small_files == 0 { 0 } else { total / 4 };
    let large_each = (total - small_total) / STAGE_FIXTURE_LARGE_FILES;
    let mut plan: Vec<FixtureFile> = (0..STAGE_FIXTURE_LARGE_FILES)
        .map(|i| FixtureFile {
            rel: PathBuf::from(format!("large-{i}.bin")),
            size: large_each,
            seed: fixture_seed(i),
        })
        .collect();
    if small_files > 0 {
        let mean = small_total / small_files;
        let mut sizes = Xorshift::new(fixture_seed(1 << 32));
        for i in 0..small_files {
            plan.push(FixtureFile {
                rel: PathBuf::from(format!("d{:03}/s{i:06}.bin", i / STAGE_FIXTURE_PER_DIR)),
                size: mean / 4 + sizes.next() % (mean * 3 / 2 + 1),
                seed: fixture_seed((1 << 33) + i),
            });
        }
    }
    plan
}

/// Whether `data` holds exactly the files of `plan`, at their sizes (the
/// unit dotfile a previous run's `init_unit` left is not one of them).
fn stage_fixture_matches(data: &Path, plan: &[FixtureFile]) -> bool {
    let found = walkdir::WalkDir::new(data)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && e.file_name() != ".tapectl-unit.toml")
        .count();
    found == plan.len()
        && plan.iter().all(|f| {
            fs::metadata(data.join(&f.rel))
                .map(|m| m.len() == f.size)
                .unwrap_or(false)
        })
}

/// The staging scenario's source tree: deterministic, incompressible, and
/// kept between runs. It lives at a fixed path under `TAPECTL_PERF_SCRATCH`
/// (the harness's own tempdir is deleted at the end of every scenario), and a
/// `READY` marker, written last, names its parameters; it is reused only when
/// the marker, the file count and every file's size agree with the plan, and
/// rebuilt otherwise. Returns the tree and its size in bytes.
fn stage_fixture(total_mib: u64, small_files: u64) -> (PathBuf, u64) {
    use std::io::Write as _;
    let root = perf_scratch().join(format!("stage-fixture-{total_mib}m-{small_files}f"));
    let data = root.join("data");
    let plan = stage_fixture_plan(total_mib, small_files);
    let bytes: u64 = plan.iter().map(|f| f.size).sum();
    let marker = format!(
        "format={STAGE_FIXTURE_FORMAT} mib={total_mib} small={small_files} files={} bytes={bytes}\n",
        plan.len()
    );
    if fs::read_to_string(root.join("READY")).is_ok_and(|m| m == marker)
        && stage_fixture_matches(&data, &plan)
    {
        eprintln!("[perf] stage_fixture reusing {}", data.display());
        return (data, bytes);
    }
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&data).unwrap();
    let started = Instant::now();
    let mut buf = vec![0u8; 1 << 20];
    for f in &plan {
        let path = data.join(&f.rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut out = fs::File::create(&path).unwrap();
        let mut rng = Xorshift::new(f.seed);
        let mut left = f.size;
        while left > 0 {
            rng.fill(&mut buf);
            let n = left.min(buf.len() as u64) as usize;
            out.write_all(&buf[..n]).unwrap();
            left -= n as u64;
        }
    }
    fs::write(root.join("READY"), &marker).unwrap();
    report(
        "stage_fixture",
        &format!("generate {} files, {} MiB", plan.len(), bytes >> 20),
        started.elapsed(),
    );
    (data, bytes)
}

/// Drop every page of every file under `dir` from the page cache, so the
/// next read comes off the disk. `sync` first, because only clean pages go;
/// then `posix_fadvise(POSIX_FADV_DONTNEED)` on each file. This stands in for
/// dropping the system cache, which needs root and would evict everyone
/// else's pages too.
fn evict_from_page_cache(dir: &Path) {
    use std::os::unix::io::AsRawFd;
    nix::unistd::sync();
    for e in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if e.file_type().is_file() {
            if let Ok(f) = fs::File::open(e.path()) {
                let _ = nix::fcntl::posix_fadvise(
                    f.as_raw_fd(),
                    0,
                    0,
                    nix::fcntl::PosixFadviseAdvice::POSIX_FADV_DONTNEED,
                );
            }
        }
    }
}

/// Bytes the kernel fetched from, and sent to, the block layer for this
/// process (`/proc/self/io`: every thread of it) and, separately, for the
/// children it has reaped (`RUSAGE_CHILDREN`'s `ru_inblock`/`ru_oublock`, in
/// 512-byte blocks) — dar, here. Both are page-cache misses and writeback,
/// not `read(2)` calls: a read the cache answers does not count. Read twice
/// and subtract; the suite runs with `--test-threads=1`, so no other
/// scenario's children are reaped in between.
#[derive(Clone, Copy)]
struct IoBytes {
    own_read: u64,
    own_written: u64,
    child_read: u64,
    child_written: u64,
}

impl IoBytes {
    fn now() -> IoBytes {
        let text = fs::read_to_string("/proc/self/io").expect("/proc/self/io");
        let field = |name: &str| -> u64 {
            text.lines()
                .find_map(|l| l.strip_prefix(name))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or_else(|| panic!("{name} in /proc/self/io"))
        };
        let children =
            nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_CHILDREN).unwrap();
        IoBytes {
            own_read: field("read_bytes:"),
            own_written: field("write_bytes:"),
            child_read: children.block_reads() as u64 * 512,
            child_written: children.block_writes() as u64 * 512,
        }
    }

    fn since(self, earlier: IoBytes) -> IoBytes {
        IoBytes {
            own_read: self.own_read - earlier.own_read,
            own_written: self.own_written - earlier.own_written,
            child_read: self.child_read - earlier.child_read,
            child_written: self.child_written - earlier.child_written,
        }
    }

    fn read(self) -> u64 {
        self.own_read + self.child_read
    }

    fn written(self) -> u64 {
        self.own_written + self.child_written
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1u64 << 20) as f64
}

fn mib_per_s(bytes: u64, over: Duration) -> f64 {
    mib(bytes) / over.as_secs_f64().max(1e-9)
}

/// Issue #364 (the staging-throughput epic's benchmark): one unit of
/// incompressible data, a few large files and a few thousand small ones,
/// through the library's real `snapshot create` and `stage create`, with the
/// fixture evicted from the page cache first so every read is a disk read.
///
/// Reports, in MiB/s over the source's size (1 MiB = 1,048,576 bytes): the
/// whole run and each step; every phase `stage create` records in
/// `phase_timings` (the rows `volume info` and the stage report are made
/// from, taken from the catalog and not timed here); how many bytes the
/// whole run, dar included, read from disk, as a multiple of the source; and
/// how many it wrote. Only `archive` counts bytes, so only it has a rate
/// (dar, the source hash and the age encryption run inside it, together).
///
/// Asserts only what holds on any machine: the stage succeeds, the
/// `archive` phase was recorded, and the disk reads are at least half the
/// source and at most one and a half times it plus [`STAGE_READ_SLACK`]. The
/// upper bound is the one-read-per-byte promise of #370 and #441 with room
/// for what is not the source (this test binary's own page-ins, the catalog;
/// before #441 dar's `fadvise` made the second read come off the disk: 1.96x
/// on a 2 GiB fixture); the lower bound is the positive control for the
/// eviction: under half means the source came from the cache, or the scratch
/// is a filesystem that reads no blocks, and every figure here would measure
/// the page cache. Never a timing.
///
/// Knobs: `TAPECTL_PERF_STAGE_MIB` (source size, 4096),
/// `TAPECTL_PERF_STAGE_SMALL` (small files, 4000, in a quarter of it),
/// `TAPECTL_PERF_STAGE_SLICE` (dar `-s`, 1G, the 1.1 default) and
/// `TAPECTL_PERF_HASH_THREADS` (`[staging] hash_threads`; the shipped
/// default is 1, which leaves #366's pool idle).
#[test]
#[ignore = "perf suite: set TAPECTL_PERF_TESTS=1 and pass --ignored"]
fn perf_staging_throughput_per_pass() {
    use tapectl::progress::{start_session, Display};

    if !perf_enabled() {
        eprintln!("skip: TAPECTL_PERF_TESTS not set");
        return;
    }
    let env_u64 = |name: &str, default: u64| -> u64 {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let total_mib = env_u64("TAPECTL_PERF_STAGE_MIB", 4096);
    let small_files = env_u64("TAPECTL_PERF_STAGE_SMALL", 4000);
    let (source, source_bytes) = stage_fixture(total_mib, small_files);

    let mut h = setup("stage-throughput");
    h.config.defaults.slice_size =
        std::env::var("TAPECTL_PERF_STAGE_SLICE").unwrap_or_else(|_| "1G".into());
    if let Some(threads) = std::env::var("TAPECTL_PERF_HASH_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        h.config.staging.hash_threads = threads;
    }
    tenant::add_tenant(&h.conn, &h.paths, "op", None, true).unwrap();

    // `init_unit` writes the unit's dotfile into the source; take a previous
    // run's away first and this run's away after, so the fixture stays as
    // generated.
    let dotfile = source.join(".tapectl-unit.toml");
    let _ = fs::remove_file(&dotfile);
    unit::init_unit(
        &h.conn,
        &h.paths,
        &source.to_string_lossy(),
        "op",
        Some("stage-unit"),
        &[],
        None,
    )
    .unwrap();
    eprintln!(
        "[perf] stage_throughput source {:.1} MiB in {} files, slice {}, hash_threads {}",
        mib(source_bytes),
        stage_fixture_plan(total_mib, small_files).len(),
        h.config.defaults.slice_size,
        h.config.staging.hash_threads,
    );

    evict_from_page_cache(&source);
    let io_start = IoBytes::now();
    let run = Instant::now();

    let t = Instant::now();
    let sid = staging::snapshot_create(&h.conn, "stage-unit", &h.config).unwrap();
    let snapshot_elapsed = t.elapsed();
    let io_snapshot = IoBytes::now().since(io_start);

    // A session of its own, so the phases are recorded: `phase_timings::record`
    // does nothing on a thread without one (`main` opens one for the command).
    let session = start_session(Some(&h.paths.logs_dir), "stage-create", Display::Off, false);
    let t = Instant::now();
    let staged = staging::stage_create(&h.conn, &h.paths, &h.config, sid, true);
    let stage_elapsed = t.elapsed();
    let total_elapsed = run.elapsed();
    let io = IoBytes::now().since(io_start);
    drop(session);
    let _ = fs::remove_file(&dotfile);
    let stage_set_id = staged.expect("stage_create");

    let phases = tapectl::db::phase_timings::latest_for_stage_set(&h.conn, stage_set_id).unwrap();
    let (slice_bytes, plain_bytes): (i64, i64) = h
        .conn
        .query_row(
            "SELECT total_encrypted_size, total_dar_size FROM stage_sets WHERE id = ?1",
            params![stage_set_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();

    report(
        "stage_throughput",
        &format!(
            "snapshot_create, {:.1} MiB read from disk",
            mib(io_snapshot.read())
        ),
        snapshot_elapsed,
    );
    report(
        "stage_throughput",
        &format!(
            "stage_create, {:.1} MiB/s over the source",
            mib_per_s(source_bytes, stage_elapsed)
        ),
        stage_elapsed,
    );
    report(
        "stage_throughput",
        &format!(
            "total, {:.1} MiB/s over the source",
            mib_per_s(source_bytes, total_elapsed)
        ),
        total_elapsed,
    );
    for p in &phases {
        let d = Duration::from_millis(p.duration_ms.max(0) as u64);
        let rate = match p.bytes {
            Some(b) => format!(
                "{:.1} MiB/s over {:.1} MiB",
                mib_per_s(b as u64, d),
                mib(b as u64)
            ),
            None => "no byte count".into(),
        };
        report(
            "stage_throughput",
            &format!("phase {:<9} {rate}", p.phase),
            d,
        );
    }
    let ratio = io.read() as f64 / source_bytes as f64;
    eprintln!(
        "[perf] stage_throughput read from disk {:.1} MiB = {ratio:.2}x the source \
         (own {:.1} MiB, dar and other children {:.1} MiB)",
        mib(io.read()),
        mib(io.own_read),
        mib(io.child_read),
    );
    eprintln!(
        "[perf] stage_throughput written {:.1} MiB (own {:.1}, children {:.1}); \
         staged slices {:.1} MiB from a {:.1} MiB dar stream",
        mib(io.written()),
        mib(io.own_written),
        mib(io.child_written),
        mib(slice_bytes as u64),
        mib(plain_bytes as u64),
    );

    assert!(
        phases
            .iter()
            .any(|p| p.phase == "archive" && p.outcome == "ok"),
        "stage create recorded no ok `archive` phase: {phases:?}"
    );
    assert!(
        ratio >= 0.5,
        "only {ratio:.2}x the source was read from disk: the fixture was still in the page \
         cache (or {} reads no blocks), so every rate above is a cache rate",
        source.display()
    );
    assert!(
        io.read() as f64 <= 1.5 * source_bytes as f64 + STAGE_READ_SLACK as f64,
        "staging read {ratio:.2}x the source from disk ({:.1} MiB for {:.1} MiB): it is \
         reading the source more than once (issues #370, #441)",
        mib(io.read()),
        mib(source_bytes)
    );
}
