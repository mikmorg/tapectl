//! mhvtl-gated end-to-end tests for the full write→verify→restore pipeline.
//!
//! All tests are `#[ignore]` and additionally skip at runtime unless both:
//!   - `TAPECTL_MHVTL=1` is set
//!   - `/dev/nst0` exists
//!
//! Run locally with:
//!   TAPECTL_MHVTL=1 cargo test --test mhvtl_e2e -- --ignored --nocapture
//!
//! Only one test at a time may hold the tape device; `cargo test` parallelizes
//! tests by default, so every test in this file acquires `TAPE_LOCK` before
//! touching the drive. This serializes them automatically without requiring
//! `--test-threads=1` at the command line.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};

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

/// Global lock so parallel tests don't race on the single tape device. Poison
/// is fine — a prior test panic shouldn't wedge the whole suite, so we clear
/// it rather than propagating.
fn tape_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
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
    let _g = tape_lock();
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
    let _g = tape_lock();
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
    let _g = tape_lock();
    let label = "MHVTLC";
    let _h = write_volume("identify", label, &[("alice", "alice-u", 1)]);

    let id = volume::write::volume_identify(TAPE_DEV, BLOCK_SIZE).unwrap();
    assert!(id.contains(label), "id thunk missing label: {id}");
    assert!(id.contains("TAPECTL"), "id thunk missing header: {id}");
}

/// Read one tape file at an absolute position by rewind + forward_space_file.
fn read_tape_file_at(device: &str, block_size: usize, pos: i32) -> Vec<u8> {
    let mut tape = tapectl::tape::ioctl::TapeDevice::open_read(device, block_size).unwrap();
    tape.rewind().unwrap();
    if pos > 0 {
        tape.forward_space_file(pos).unwrap();
    }
    tape.read_file().unwrap()
}

/// Parse `key = N` or `key = <N>` from an ID thunk TOML region.
fn parse_i32_field(text: &str, key: &str) -> Option<i32> {
    let needle = format!("{key} = ");
    let idx = text.find(&needle)?;
    let rest = &text[idx + needle.len()..];
    let end = rest.find('\n').unwrap_or(rest.len());
    rest[..end].trim().parse().ok()
}

#[test]
#[ignore]
fn mhvtl_no_plaintext_tenant_metadata() {
    if !mhvtl_enabled() {
        eprintln!("skip: TAPECTL_MHVTL not set or {TAPE_DEV} missing");
        return;
    }
    let _g = tape_lock();

    // Use names with no plausible substring collision against the volume
    // label, tapectl boilerplate, or dar/age headers. If any of these strings
    // show up in a plaintext file on tape, isolation is broken.
    let label = "MHVTLI";
    let t_alpha = "tnt-alpha-xyzzy";
    let t_bravo = "tnt-bravo-plover";
    let u_alpha = "unit-alpha-xyzzy";
    let u_bravo = "unit-bravo-plover";
    let forbidden = [t_alpha, t_bravo, u_alpha, u_bravo];

    let h = write_volume(
        "plaintext-leak",
        label,
        &[(t_alpha, u_alpha, 2), (t_bravo, u_bravo, 2)],
    );

    // Pull layout info from the ID thunk (File 0) — it's plaintext TOML.
    let id_thunk = read_tape_file_at(TAPE_DEV, BLOCK_SIZE, 0);
    let id_text = std::str::from_utf8(&id_thunk).expect("id thunk utf8");
    assert!(id_text.contains(label));
    let total_files = parse_i32_field(id_text, "total_files").expect("total_files");
    let mini_index = parse_i32_field(id_text, "mini_index").expect("mini_index");
    let first_envelope = parse_i32_field(id_text, "first_envelope").expect("first_envelope");
    let op_envelope = parse_i32_field(id_text, "operator_envelope").expect("operator_envelope");

    // Classify every file on the tape and scan plaintext files.
    // Plaintext-by-design positions: 0 (ID thunk), 1 (system guide),
    //                                2 (RESTORE.sh), mini_index.
    // Everything else must be age-encrypted.
    let age_magic = b"age-encryption.org/v1";
    let plaintext_positions = [0i32, 1, 2, mini_index];

    for pos in 0..total_files {
        let data = read_tape_file_at(TAPE_DEV, BLOCK_SIZE, pos);
        assert!(!data.is_empty(), "file {pos} empty");

        if plaintext_positions.contains(&pos) {
            let s = String::from_utf8_lossy(&data);
            for needle in &forbidden {
                assert!(
                    !s.contains(needle),
                    "plaintext leak at file {pos}: contains {needle:?}"
                );
            }
        } else {
            // Encrypted file — must start with age header magic.
            assert!(
                data.starts_with(age_magic),
                "file {pos} is not age-encrypted (first 32 bytes: {:?})",
                &data[..data.len().min(32)]
            );
            // And, just to be safe, raw ciphertext must not contain the
            // forbidden substrings either (age ciphertext is effectively
            // random; this guards against pathological mis-wiring where a
            // file ends up encrypted but still carries a plaintext prefix).
            let s = String::from_utf8_lossy(&data);
            for needle in &forbidden {
                assert!(
                    !s.contains(needle),
                    "encrypted file {pos} contains plaintext {needle:?}"
                );
            }
        }
    }

    // Sanity-check the envelope range: between mini_index and op_envelope we
    // should see at least one tenant envelope per tenant, and op_envelope
    // plus its backup at the end.
    assert!(first_envelope == mini_index + 1);
    assert!(op_envelope >= first_envelope + 3); // op, alpha, bravo
    assert!(op_envelope + 1 < total_files);

    drop(h); // keep temp dir alive until after tape reads
}

#[test]
#[ignore]
fn mhvtl_both_tenants_self_restore() {
    if !mhvtl_enabled() {
        return;
    }
    let _g = tape_lock();
    // Complements mhvtl_tenant_isolation (which proves a missing key fails).
    // Here both keys are present and each tenant's unit restores bit-identical.
    let label = "MHVTLJ";
    let h = write_volume(
        "both-restore",
        label,
        &[("alice", "alice-u", 3), ("bob", "bob-u", 3)],
    );

    let alice_dest = h.root.path().join("restored-alice");
    let bob_dest = h.root.path().join("restored-bob");
    restore_to(&h, "alice-u", label, &alice_dest);
    restore_to(&h, "bob-u", label, &bob_dest);

    // source_dirs ordering inside write_volume: [op-unit, alice-u, bob-u]
    assert!(diff_recursive(h.source(1), &alice_dest), "alice diff");
    assert!(diff_recursive(h.source(2), &bob_dest), "bob diff");
}

#[test]
#[ignore]
fn mhvtl_health_logs_populated() {
    if !mhvtl_enabled() {
        return;
    }
    let _g = tape_lock();
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
