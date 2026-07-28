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
use tapectl::store::Tier;
use tapectl::volume::format::{self, ParsedIndexEntry};
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

/// Like `add_unit`, but also drops one distinctively-named file into the
/// unit's source directory before staging. The v2 leak-scan leg (T9 leg 2,
/// `volume-format-v2.md` sec 2's isolation invariant) needs a real source
/// FILENAME to travel through dar's catalog so it can assert that filename
/// never surfaces in plaintext — not just tenant/unit names. `.pdf` is
/// deliberate: it must not collide with the default staging excludes
/// (`*.nfo`, `*.tmp`, `Thumbs.db`, `.DS_Store`) or it would silently vanish
/// from the snapshot and the needle would never have a chance to appear
/// anywhere, making the assertion vacuous.
fn add_unit_with_sentinel_file(
    h: &mut Harness,
    tenant_name: &str,
    is_operator: bool,
    unit_name: &str,
    n_files: usize,
    sentinel_filename: &str,
) {
    tenant::add_tenant(&h.conn, &h.paths, tenant_name, None, is_operator).unwrap();
    let src = make_source(h.root.path(), unit_name, n_files);
    fs::write(
        src.join(sentinel_filename),
        b"sentinel file content for the v2 leak-scan leg; only its NAME is the needle",
    )
    .unwrap();
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

    // Restore leg (v2-implementation-plan.md T9): intent unchanged from
    // before the v2 flip — write, verify, restore, byte-identical diff. Only
    // the `tier` parameter (T8) changed mechanically; the v2 STRUCTURAL
    // positions (front index at File 3, envelopes from File 4, seal last) are
    // asserted separately in `mhvtl_v2_layout_positions_derived_from_front_index`
    // below, so a restore-path regression and a layout regression fail two
    // distinguishable tests rather than one entangled one.
    let verify = volume::write::volume_verify(
        &h.conn,
        &h.config,
        label,
        TAPE_DEV,
        BLOCK_SIZE,
        Tier::default(),
    )
    .unwrap();
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

/// Leg 1 (v2-implementation-plan.md T9): v2 positions, derived entirely from
/// the front index itself (File 3) — never hardcoded arithmetic. Front index
/// at File 3; envelopes from File 4; every envelope precedes every data
/// slice; seal marker last (`volume-format-v2.md` sec 1). Kept separate from
/// `mhvtl_full_round_trip` so a restore-path failure and a layout-derivation
/// failure are diagnosable independently.
#[test]
#[ignore]
fn mhvtl_v2_layout_positions_derived_from_front_index() {
    if !mhvtl_enabled() {
        eprintln!("skip: TAPECTL_MHVTL not set or {TAPE_DEV} missing");
        return;
    }
    let _g = tape_lock();
    let label = "MHVTLH";
    let h = write_volume(
        "v2-positions",
        label,
        &[("dana", "dana-u", 2), ("erin", "erin-u", 2)],
    );

    let parsed = read_front_index(TAPE_DEV, BLOCK_SIZE);
    let violations = format::validate_consistency(&parsed);
    assert!(
        violations.is_empty(),
        "front index self-consistency violations: {violations:?}"
    );

    assert_eq!(parsed[0].type_label, "id_thunk");
    assert_eq!(parsed[1].type_label, "system_guide");
    assert_eq!(parsed[2].type_label, "restore_sh");
    assert_eq!(parsed[3].type_label, "front_index");
    assert_eq!(parsed[3].position, 3, "front index must sit at File 3");

    let envelope_positions: Vec<i32> = parsed
        .iter()
        .filter(|e| {
            matches!(
                e.type_label.as_str(),
                "tenant_envelope" | "operator_envelope" | "operator_envelope_backup"
            )
        })
        .map(|e| e.position)
        .collect();
    assert!(
        !envelope_positions.is_empty(),
        "expected at least one envelope"
    );
    assert_eq!(
        *envelope_positions.iter().min().unwrap(),
        4,
        "the first envelope must sit at File 4 (volume-format-v2.md sec 1)"
    );

    let slice_positions: Vec<i32> = parsed
        .iter()
        .filter(|e| e.type_label == "data_slice")
        .map(|e| e.position)
        .collect();
    assert!(
        !slice_positions.is_empty(),
        "expected at least one data slice"
    );
    assert!(
        *envelope_positions.iter().max().unwrap() < *slice_positions.iter().min().unwrap(),
        "every envelope must precede every data slice (v2 order)"
    );

    let seal = parsed.last().unwrap();
    assert_eq!(seal.type_label, "seal_marker");
    assert_eq!(
        seal.position as usize,
        parsed.len() - 1,
        "seal marker must be the last file"
    );
    assert!(
        *slice_positions.iter().max().unwrap() < seal.position,
        "seal marker must come after every data slice"
    );

    drop(h); // keep temp dir alive until after tape reads
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

/// Read and parse the v2 front index (File 3) off the tape — the single
/// source of truth for on-tape positions from File 4 onward
/// (`docs/design/v2-implementation-plan.md` T9: "derive positions from the
/// front index... not hardcoded arithmetic"). Shared by every leg that needs
/// v2 structural positions.
fn read_front_index(device: &str, block_size: usize) -> Vec<ParsedIndexEntry> {
    let raw = read_tape_file_at(device, block_size, 3);
    let text = String::from_utf8_lossy(&raw);
    let trimmed = text.trim_end_matches('\0');
    format::parse_front_index(trimmed).expect("front index (File 3) must parse")
}

/// Every `sha256_plain` recorded for a unit's staged slices — the
/// plaintext-CONTENT hash that `volume-format-v2.md` sec 2 says must never
/// appear outside an encrypted envelope. Strengthens the leak-scan needle set
/// (T9 leg 2) beyond tenant/unit names. The emptiness assert guards against
/// this needle silently becoming a no-op if the join ever stops matching.
fn slice_plaintext_hashes(conn: &Connection, unit_name: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT sl.sha256_plain FROM stage_slices sl
             JOIN stage_sets ss ON ss.id = sl.stage_set_id
             JOIN snapshots s ON s.id = ss.snapshot_id
             JOIN units u ON u.id = s.unit_id
             WHERE u.name = ?1",
        )
        .unwrap();
    let hashes: Vec<String> = stmt
        .query_map(rusqlite::params![unit_name], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(
        !hashes.is_empty(),
        "expected at least one staged slice for unit {unit_name}"
    );
    hashes
}

/// Leg 2 (v2-implementation-plan.md T9, `volume-format-v2.md` sec 2's
/// isolation invariant, D4's real enforcement): plaintext positions are
/// `{0, 1, 2, 3, seal_marker}` — everything else must start with the age
/// magic. The needle set is strengthened beyond tenant/unit names: sentinel
/// source FILENAMES and the expected `sha256_plain` hex digests must also be
/// absent from every plaintext file. `sha256_encrypted` values and on-tape
/// sizes are the accepted disclosure (format sec 2) and are deliberately
/// NOT asserted absent.
#[test]
#[ignore]
fn mhvtl_no_plaintext_tenant_metadata() {
    if !mhvtl_enabled() {
        eprintln!("skip: TAPECTL_MHVTL not set or {TAPE_DEV} missing");
        return;
    }
    let _g = tape_lock();

    // Use names/filenames with no plausible substring collision against the
    // volume label, tapectl boilerplate, or dar/age headers. If any of these
    // strings (or an expected sha256_plain digest) show up in a plaintext
    // file on tape, isolation is broken.
    let label = "MHVTLI";
    let t_alpha = "tnt-alpha-xyzzy";
    let t_bravo = "tnt-bravo-plover";
    let u_alpha = "unit-alpha-xyzzy";
    let u_bravo = "unit-bravo-plover";
    let f_alpha = "secret-report-xyzzy-alpha.pdf";
    let f_bravo = "secret-report-plover-bravo.pdf";

    mhvtl_load();
    let mut h = setup_mhvtl("plaintext-leak");
    add_unit(&mut h, "op", true, "op-unit", 1);
    add_unit_with_sentinel_file(&mut h, t_alpha, false, u_alpha, 2, f_alpha);
    add_unit_with_sentinel_file(&mut h, t_bravo, false, u_bravo, 2, f_bravo);

    // Collected from the DB BEFORE the write — the plaintext-content hashes
    // that must never appear outside an encrypted envelope.
    let mut forbidden: Vec<String> = vec![
        t_alpha.to_string(),
        t_bravo.to_string(),
        u_alpha.to_string(),
        u_bravo.to_string(),
        f_alpha.to_string(),
        f_bravo.to_string(),
    ];
    forbidden.extend(slice_plaintext_hashes(&h.conn, u_alpha));
    forbidden.extend(slice_plaintext_hashes(&h.conn, u_bravo));

    volume::write::volume_init(&h.conn, &h.config, label, TAPE_DEV, BLOCK_SIZE).unwrap();
    volume::write::volume_write(&h.conn, &h.paths, &h.config, label, TAPE_DEV, BLOCK_SIZE).unwrap();

    // Pull layout info from the ID thunk (File 0) — it's plaintext TOML. v2's
    // [layout] table carries only front_index/seal_marker/total_files (the
    // v1 mini_index/first_envelope/operator_envelope fields are gone,
    // `volume-format-v2.md` sec 1/sec 8) — the envelope/slice RANGE now comes
    // from the front index itself (File 3), never from ID-thunk position
    // fields.
    let id_thunk = read_tape_file_at(TAPE_DEV, BLOCK_SIZE, 0);
    let id_text = std::str::from_utf8(&id_thunk).expect("id thunk utf8");
    assert!(id_text.contains(label));
    assert!(
        id_text.contains("tapectl-volume-v2"),
        "id thunk must carry the v2 magic"
    );
    let total_files = parse_i32_field(id_text, "total_files").expect("total_files");
    let front_index_pos = parse_i32_field(id_text, "front_index").expect("front_index");
    let seal_marker_pos = parse_i32_field(id_text, "seal_marker").expect("seal_marker");
    assert_eq!(
        front_index_pos, 3,
        "v2 front index must always sit at File 3 (volume-format-v2.md sec 1)"
    );

    // The front index (File 3) itself — the only source of the v2
    // envelope/slice range now that the v1 ID-thunk position fields are gone.
    let parsed = read_front_index(TAPE_DEV, BLOCK_SIZE);
    let violations = format::validate_consistency(&parsed);
    assert!(
        violations.is_empty(),
        "front index self-consistency violations: {violations:?}"
    );
    assert_eq!(
        seal_marker_pos as usize,
        parsed.len() - 1,
        "ID thunk's seal_marker pointer must agree with the front index's own last entry"
    );

    // Plaintext-by-design positions (volume-format-v2.md sec 2): 0 (ID
    // thunk), 1 (system guide), 2 (RESTORE.sh), 3 (front index), and the seal
    // marker (last file). Everything else must be age-encrypted.
    let age_magic = b"age-encryption.org/v1";
    let plaintext_positions = [0i32, 1, 2, front_index_pos, seal_marker_pos];

    for pos in 0..total_files {
        let data = read_tape_file_at(TAPE_DEV, BLOCK_SIZE, pos);
        assert!(!data.is_empty(), "file {pos} empty");

        if plaintext_positions.contains(&pos) {
            let s = String::from_utf8_lossy(&data);
            for needle in &forbidden {
                assert!(
                    !s.contains(needle.as_str()),
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
                    !s.contains(needle.as_str()),
                    "encrypted file {pos} contains plaintext {needle:?}"
                );
            }
        }
    }

    // Envelope-range sanity block, v2 order (volume-format-v2.md sec 1):
    // envelopes start at File 4 and every envelope precedes every data slice.
    // Derived entirely from the parsed front index, never hardcoded
    // arithmetic.
    let tenant_envelope_positions: Vec<i32> = parsed
        .iter()
        .filter(|e| e.type_label == "tenant_envelope")
        .map(|e| e.position)
        .collect();
    assert_eq!(
        tenant_envelope_positions.len(),
        2,
        "alpha and bravo must each get exactly one tenant envelope"
    );
    assert_eq!(
        *tenant_envelope_positions.iter().min().unwrap(),
        4,
        "the first envelope must sit at File 4 (volume-format-v2.md sec 1)"
    );
    let op_envelope_pos = parsed
        .iter()
        .find(|e| e.type_label == "operator_envelope")
        .map(|e| e.position)
        .expect("operator envelope present");
    let op_backup_pos = parsed
        .iter()
        .find(|e| e.type_label == "operator_envelope_backup")
        .map(|e| e.position)
        .expect("operator envelope backup present");
    assert!(
        op_envelope_pos > *tenant_envelope_positions.iter().max().unwrap(),
        "operator envelope must follow every tenant envelope"
    );
    assert_eq!(
        op_backup_pos,
        op_envelope_pos + 1,
        "operator envelope backup must immediately follow the operator envelope"
    );
    let slice_positions: Vec<i32> = parsed
        .iter()
        .filter(|e| e.type_label == "data_slice")
        .map(|e| e.position)
        .collect();
    assert!(
        !slice_positions.is_empty(),
        "expected at least one data slice"
    );
    assert!(
        op_backup_pos < *slice_positions.iter().min().unwrap(),
        "every envelope must precede every data slice (v2 order, format sec 1)"
    );

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
