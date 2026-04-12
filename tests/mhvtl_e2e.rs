//! mhvtl-gated end-to-end tests for the full write→verify→restore pipeline.
//!
//! All tests are `#[ignore]` and additionally skip at runtime unless both:
//!   - `TAPECTL_MHVTL=1` is set
//!   - `/dev/nst0` exists
//!
//! Run locally with:
//!   TAPECTL_MHVTL=1 cargo test --test mhvtl_e2e -- --ignored --nocapture

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::Connection;
use tempfile::TempDir;

use tapectl::config::{Config, LtoBackendConfig, StagingConfig, TapectlPaths};
use tapectl::{db, staging, tenant, unit, volume};

const TAPE_DEV: &str = "/dev/nst0";
const SG_DEV: &str = "/dev/sg1";
const BLOCK_SIZE: usize = 512 * 1024;

fn mhvtl_enabled() -> bool {
    std::env::var("TAPECTL_MHVTL").is_ok() && Path::new(TAPE_DEV).exists()
}

// Config::default() points at /opt/dar/bin/dar; on dev VMs dar lives in /usr/bin.
fn find_dar() -> String {
    for p in ["/opt/dar/bin/dar", "/usr/local/bin/dar", "/usr/bin/dar"] {
        if Path::new(p).exists() {
            return p.into();
        }
    }
    "dar".into()
}

// mhvtl library element numbers are canned; "load 1" is a best-effort no-op
// on VMs where the drive is already loaded, and volume_init rewinds anyway.
fn mhvtl_load() {
    let _ = Command::new("mtx")
        .args(["-f", "/dev/sg0", "load", "1"])
        .status();
}

struct Harness {
    root: TempDir,
    paths: TapectlPaths,
    conn: Connection,
    config: Config,
    source_dirs: Vec<PathBuf>,
}

impl Harness {
    fn source(&self, idx: usize) -> &Path {
        &self.source_dirs[idx]
    }
}

fn setup_mhvtl(name: &str) -> Harness {
    let scratch = PathBuf::from("/scratch/tapectl-mhvtl-test");
    fs::create_dir_all(&scratch).unwrap();
    let root = tempfile::Builder::new()
        .prefix(&format!("{name}-"))
        .tempdir_in(&scratch)
        .unwrap();

    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let paths = TapectlPaths::new(home);
    paths.ensure_dirs().unwrap();

    let staging_dir = root.path().join("staging");
    fs::create_dir_all(&staging_dir).unwrap();

    let conn = db::open(&paths.db_file).unwrap();

    let mut config = Config::default();
    config.dar.binary = find_dar();
    config.staging = StagingConfig {
        directory: staging_dir.to_string_lossy().into_owned(),
    };
    config.defaults.slice_size = "50M".into();
    config.defaults.compression = "none".into();
    config.backends.lto.push(LtoBackendConfig {
        name: "mhvtl".into(),
        device_tape: TAPE_DEV.into(),
        device_sg: SG_DEV.into(),
        media_type: "LTO-6".into(),
        nominal_capacity: "2400G".into(),
        usable_capacity_factor: 0.92,
        manifest_reserve: "200M".into(),
        enospc_buffer: "50M".into(),
        block_size: "512K".into(),
        hardware_compression: false,
    });

    Harness {
        root,
        paths,
        conn,
        config,
        source_dirs: Vec::new(),
    }
}

fn make_source(root: &Path, name: &str, n_files: usize) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    for i in 0..n_files {
        let p = dir.join(format!("file_{i:03}.bin"));
        let content: Vec<u8> = (0..1024).map(|j| ((i * 31 + j) & 0xff) as u8).collect();
        fs::write(p, content).unwrap();
    }
    dir
}

fn add_unit(
    h: &mut Harness,
    tenant_name: &str,
    is_operator: bool,
    unit_name: &str,
    n_files: usize,
) {
    tenant::add_tenant(&h.conn, &h.paths, tenant_name, None, is_operator).unwrap();
    let src = make_source(h.root.path(), unit_name, n_files);
    unit::init_unit(
        &h.conn,
        &h.paths,
        &src.to_string_lossy(),
        tenant_name,
        Some(unit_name),
        &[],
        None,
    )
    .unwrap();
    h.source_dirs.push(src);
    let sid = staging::snapshot_create(&h.conn, unit_name).unwrap();
    staging::stage_create(&h.conn, &h.paths, &h.config, sid).unwrap();
}

/// Build a harness and write a freshly-initialized volume with the given units.
/// The first unit is always under the operator tenant "op" (required for the
/// planning-header / operator-envelope encryption path).
fn write_volume(name: &str, label: &str, units: &[(&str, &str, usize)]) -> Harness {
    mhvtl_load();
    let mut h = setup_mhvtl(name);
    add_unit(&mut h, "op", true, "op-unit", 1);
    for (tenant, unit, n) in units {
        add_unit(&mut h, tenant, false, unit, *n);
    }
    volume::write::volume_init(&h.conn, &h.config, label, TAPE_DEV, BLOCK_SIZE).unwrap();
    volume::write::volume_write(&h.conn, &h.paths, &h.config, label, TAPE_DEV, BLOCK_SIZE).unwrap();
    h
}

fn restore_to(h: &Harness, unit_name: &str, label: &str, dest: &Path) {
    fs::create_dir_all(dest).unwrap();
    volume::restore::restore_unit(
        &h.conn,
        &h.paths,
        &h.config,
        unit_name,
        label,
        &dest.to_string_lossy(),
        TAPE_DEV,
        BLOCK_SIZE,
        false,
    )
    .unwrap();
}

fn diff_recursive(a: &Path, b: &Path) -> bool {
    Command::new("diff")
        .arg("-r")
        .arg(a)
        .arg(b)
        .status()
        .expect("diff failed to spawn")
        .success()
}

// ─────────────────────────────────────────────────────────────────────────────

#[test]
#[ignore]
fn mhvtl_full_round_trip() {
    if !mhvtl_enabled() {
        eprintln!("skip: TAPECTL_MHVTL not set or {TAPE_DEV} missing");
        return;
    }
    let label = "MHVTLA";
    let h = write_volume("round-trip", label, &[("alice", "alice-unit", 5)]);

    let verify =
        volume::write::volume_verify(&h.conn, &h.config, label, TAPE_DEV, BLOCK_SIZE).unwrap();
    assert_eq!(verify.failed, 0, "verify had failures: {verify:?}");
    assert!(verify.passed > 0, "verify found no slices");

    let dest = h.root.path().join("restored-alice");
    restore_to(&h, "alice-unit", label, &dest);
    // dar is invoked with -R <unit_path>, so extracted files land directly in dest.
    assert!(
        diff_recursive(h.source(1), &dest),
        "restored content differs from source"
    );
}

#[test]
#[ignore]
fn mhvtl_tenant_isolation() {
    if !mhvtl_enabled() {
        return;
    }
    let label = "MHVTLB";
    let h = write_volume(
        "tenant-iso",
        label,
        &[("alice", "alice-u", 3), ("bob", "bob-u", 3)],
    );

    // Remove bob's secret key — restoring bob-u must fail cleanly while
    // alice-u still succeeds. Exercises tenant-envelope trial-decrypt isolation.
    fs::remove_file(h.paths.keys_dir.join("bob-primary.age.key")).unwrap();

    restore_to(&h, "alice-u", label, &h.root.path().join("restored-alice"));

    let bob_dest = h.root.path().join("restored-bob");
    fs::create_dir_all(&bob_dest).unwrap();
    let res = volume::restore::restore_unit(
        &h.conn,
        &h.paths,
        &h.config,
        "bob-u",
        label,
        &bob_dest.to_string_lossy(),
        TAPE_DEV,
        BLOCK_SIZE,
        false,
    );
    assert!(res.is_err(), "bob restore must fail with missing key");
}

#[test]
#[ignore]
fn mhvtl_volume_identify() {
    if !mhvtl_enabled() {
        return;
    }
    let label = "MHVTLC";
    let _h = write_volume("identify", label, &[("alice", "alice-u", 1)]);

    let id = volume::write::volume_identify(TAPE_DEV, BLOCK_SIZE).unwrap();
    assert!(id.contains(label), "id thunk missing label: {id}");
    assert!(id.contains("TAPECTL"), "id thunk missing header: {id}");
}

#[test]
#[ignore]
fn mhvtl_health_logs_populated() {
    if !mhvtl_enabled() {
        return;
    }
    let label = "MHVTLD";
    let h = write_volume("health", label, &[("alice", "alice-u", 2)]);

    let (count, raw_len): (i64, i64) = h
        .conn
        .query_row(
            "SELECT COUNT(*), COALESCE(MAX(LENGTH(raw_log)), 0)
             FROM health_logs WHERE operation = 'write'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(count >= 1, "no health_logs row after write");
    assert!(raw_len > 0, "raw_log empty");
}
