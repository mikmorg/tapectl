//! Performance/scaling tests for the disk-side of the pipeline.
//!
//! Gated behind `TAPECTL_PERF_TESTS=1` — they do real work (thousands of
//! files, hundreds of DB rows, dar + age encryption) and are too slow to
//! run on every `cargo test` invocation. No tape hardware is required.
//!
//! Scenarios are intentionally scaled to a dev VM (≈100 GiB /scratch, no
//! real drive). Production-scale numbers (2+ TB units, 100K+ files) need
//! real LTO hardware and are covered by `docs/lto6-validation-checklist.md`
//! when that lands.
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
