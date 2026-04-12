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
    let scratch = PathBuf::from("/scratch/tapectl-perf");
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

    let mut config = Config::default();
    config.dar.binary = find_dar();
    config.staging = StagingConfig {
        directory: staging_dir.to_string_lossy().into_owned(),
    };
    config.defaults.slice_size = "200M".into();
    config.defaults.compression = "none".into();
    // audit's resolver reaches for an LTO backend name; populate a dummy
    // entry even though these tests never touch real tape.
    config.backends.lto.push(LtoBackendConfig {
        name: "dummy".into(),
        device_tape: "/dev/null".into(),
        device_sg: "/dev/null".into(),
        media_type: "LTO-6".into(),
        nominal_capacity: "2400G".into(),
        usable_capacity_factor: 0.92,
        manifest_reserve: "200M".into(),
        enospc_buffer: "50M".into(),
        block_size: "512K".into(),
        hardware_compression: false,
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
    let sid = staging::snapshot_create(&h.conn, "many-unit").unwrap();
    report("many_files", "init_unit + snapshot_create", t.elapsed());

    let t = Instant::now();
    staging::stage_create(&h.conn, &h.paths, &h.config, sid).unwrap();
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
    let sid = staging::snapshot_create(&h.conn, "big-unit").unwrap();
    report("large_file", "init_unit + snapshot_create", t.elapsed());

    let t = Instant::now();
    staging::stage_create(&h.conn, &h.paths, &h.config, sid).unwrap();
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
