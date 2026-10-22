//! Hash-pass and staging-read budgets per operation (issue #416 item 4).
//!
//! 1.0.3 hashed every staged byte twice before writing it, and nothing
//! noticed: the suites checked what a write produced, never how many times
//! it read and hashed the bytes on the way. Each budget below is the count
//! of passes the path's design requires, measured in bytes through
//! [`tapectl::pass_count`] (counted sha256, counted staging reads) and held
//! between a ceiling and 0.95 of it. The floor is what makes a missed counter
//! fail: a hasher built from `sha2::Sha256` instead of the counted one, or a
//! staged file read around [`tapectl::util::DropBehind`], drops a whole pass
//! out of the count.
//!
//! - **A write** (validate, plan, execute, seal, confirm) hashes every byte
//!   of the volume once, on the way to the medium, and reads each staged
//!   file once; validate re-reads only the small files the build generated.
//!   `--full-confirm` adds one hash of every byte, read back from
//!   the medium; `--prewrite-hash` adds one hash and one read of every staged
//!   byte, before the write. The positive control: a write with
//!   `--prewrite-hash` (1.0.3's default) is over the default write's budget.
//! - **`stage create`** hashes the source once (beside dar, issue #364), and
//!   dar's output once as plaintext and once as ciphertext. Nothing is read
//!   back from staging.
//!
//! The counts are test-only (ADR-0012, 2026-10-12 item 45): nothing records
//! them. They are process-wide, so this file is its own test binary and
//! every test holds a [`tapectl::pass_count::Probe`], which also makes them
//! take turns.
//!
//! A pin here moves only on purpose, with the reason in the commit.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use tapectl::config::{Config, StagingConfig, TapectlPaths};
use tapectl::crypto::keys::generate_keypair;
use tapectl::db;
use tapectl::pass_count::{self, Passes, Probe};
use tapectl::store::{MemStore, Tier};
use tapectl::volume::build::{self, BuildInputs, BuildSlice, BuildUnit, TenantInfo};
use tapectl::volume::layout_model::{KeyAvailability, SliceCheck};
use tapectl::volume::session::{ConfirmOutcome, ExecuteOutcome};

const BS: u64 = 65536;
/// Slices big enough that the volume's generated files (File 0, the
/// guides, the index, the envelopes, the seal) are well under 1% of its
/// bytes, so a whole write's ratio is about its slices.
const SLICES: usize = 4;
const SLICE_BYTES: usize = 8 * 1024 * 1024 + 4321;

fn sha_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Deterministic bytes no compressor shrinks.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// A catalog with one tenant's unit staged as [`SLICES`] slices, ready to be
/// built into a volume.
struct Staged {
    conn: Connection,
    units: Vec<BuildUnit>,
    volume_id: i64,
    tenant: String,
    operator: String,
    escrow: String,
    root: TempDir,
}

fn staged() -> Staged {
    let root = TempDir::new().unwrap();
    let conn = db::open(&root.path().join("tapectl.db")).unwrap();
    let operator = generate_keypair();
    let tenant = generate_keypair();
    let escrow = generate_keypair();
    conn.execute_batch(
        "INSERT INTO tenants (id, name, is_operator, status) VALUES (1, 'operator', 1, 'active');
         INSERT INTO tenants (id, name, is_operator, status) VALUES (2, 'alpha', 0, 'active');
         INSERT INTO units (id, uuid, name, tenant_id, current_path, status)
              VALUES (1, 'pass-unit', 'pass-unit', 2, '/src/pass', 'active');
         INSERT INTO snapshots (id, unit_id, version, status, source_path)
              VALUES (1, 1, 1, 'staged', '/src/pass');",
    )
    .unwrap();
    for (tenant_id, kp) in [(1, &operator), (2, &tenant)] {
        conn.execute(
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
    conn.execute(
        "INSERT INTO stage_sets (id, snapshot_id, status, slice_size) VALUES (1, 1, 'staged', ?1)",
        params![SLICE_BYTES as i64],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes,
                              status)
         VALUES ('PASS01', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
        [],
    )
    .unwrap();
    let volume_id = conn.last_insert_rowid();

    let slice_dir = root.path().join("slices");
    fs::create_dir_all(&slice_dir).unwrap();
    let mut slices = Vec::new();
    for n in 1..=SLICES {
        let bytes = noise(n as u64, SLICE_BYTES);
        let path = slice_dir.join(format!("slice_{n}.age"));
        fs::write(&path, &bytes).unwrap();
        let sha = sha_hex(&bytes);
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                       sha256_plain, sha256_encrypted, staging_path)
             VALUES (1, ?1, ?2, ?2, ?3, ?4, ?5)",
            params![
                n as i64,
                SLICE_BYTES as i64,
                sha_hex(b"plain"),
                sha,
                path.to_string_lossy()
            ],
        )
        .unwrap();
        slices.push(BuildSlice {
            slice_id: conn.last_insert_rowid(),
            slice_number: n as i64,
            size_bytes: SLICE_BYTES as i64,
            encrypted_bytes: SLICE_BYTES as i64,
            sha256_plain: sha_hex(b"plain"),
            sha256_encrypted: sha,
            staging_path: path,
        });
    }
    let units = vec![BuildUnit {
        stage_set_id: 1,
        snapshot_id: 1,
        unit_name: "pass-unit".to_string(),
        unit_uuid: "pass-unit".to_string(),
        tenant_id: 2,
        dar_version: Some("2.7.20".to_string()),
        dar_command: None,
        catalog_path: None,
        snapshot_version: 1,
        slices,
        span: None,
    }];
    Staged {
        conn,
        units,
        volume_id,
        tenant: tenant.public_key,
        operator: operator.public_key,
        escrow: escrow.public_key,
        root,
    }
}

/// What each step of one write cost, and the bytes the volume holds (every
/// file's true size).
#[derive(Debug)]
struct WriteCost {
    validate: Passes,
    execute: Passes,
    confirm: Passes,
    whole: Passes,
    volume_bytes: u64,
    staged_bytes: u64,
}

/// One write session over [`staged`]'s unit into a [`MemStore`], counted
/// step by step: `check` is `--prewrite-hash` as a mode, `tier` the confirm's
/// (`Navigable` by default, `Integrity` under `--full-confirm`).
fn write(probe: &Probe, check: SliceCheck, tier: Tier) -> WriteCost {
    let s = staged();
    let inputs = BuildInputs {
        label: "PASS01".to_string(),
        volume_uuid: "41641600-0000-4000-8000-000000000416".to_string(),
        media_type: "LTO-6".to_string(),
        tapectl_version: "pass-budget".to_string(),
        created_at: "2026-10-12T00:00:00Z".to_string(),
        block_size: BS,
        usable_bytes: 4 * 1024 * 1024 * 1024,
        enospc_buffer: 1024 * 1024,
        nominal_capacity: 2_500_000_000_000,
        mam_capacity: 2_500_000_000_000,
        mam_manufacturer: "TAPECTL-PASS".to_string(),
        mam_serial: "PASSSERIAL".to_string(),
        cartridge_identity_source: Some("mam".to_string()),
        mam_length: 0,
        mam_loads: 0,
        units: s.units.clone(),
        tenants: vec![TenantInfo {
            tenant_id: 2,
            tenant_name: "alpha".to_string(),
            public_keys: vec![s.tenant.clone()],
        }],
        operator_public_keys: vec![s.operator.clone()],
        escrow_public_key: Some(s.escrow.clone()),
        catalog_db_path: None,
        index: None,
    };
    let session_dir: PathBuf = s.root.path().join("session");
    fs::create_dir_all(&session_dir).unwrap();
    let built = build::build(&inputs, &session_dir).expect("build");
    let keys = KeyAvailability {
        tenant_ids: vec![2],
        tenants_with_active_key: [2].into_iter().collect(),
        operator_key_present: true,
        escrow_recipient_present: None,
        stage_sets_lacking_escrow: None,
    };
    let mut store = MemStore::new(BS as usize);

    let start = probe.read();
    let validated = built
        .into_validated(&keys, check, &mut store)
        .expect("validate");
    let validated_at = probe.read();
    let planned = validated
        .plan(&s.conn, s.volume_id, &s.units)
        .expect("plan");
    let planned_at = probe.read();
    let ready = match planned.execute(&s.conn, &mut store).expect("execute") {
        ExecuteOutcome::Ready(r) => r,
        _ => panic!("the write must reach Ready"),
    };
    let sealed = ready.seal(&mut store).expect("seal");
    let sealed_at = probe.read();
    assert!(matches!(
        sealed.confirm(&s.conn, &mut store, tier).expect("confirm"),
        ConfirmOutcome::Sealed(_)
    ));
    let end = probe.read();

    let unwritten: i64 = s
        .conn
        .query_row(
            "SELECT COUNT(*) FROM write_positions WHERE status != 'written'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unwritten, 0, "every file of the volume was written");
    let volume_bytes = true_bytes(&store, BS);
    WriteCost {
        validate: validated_at - start,
        execute: sealed_at - planned_at,
        confirm: end - sealed_at,
        whole: end - start,
        volume_bytes,
        staged_bytes: (SLICES * SLICE_BYTES) as u64,
    }
}

/// The volume's bytes without each file's block padding: what a pass over
/// every file reads. The padding is zeros after the last byte; a file whose
/// own bytes end in zeros is undercounted by them, which only loosens the
/// floor.
fn true_bytes(store: &MemStore, bs: u64) -> u64 {
    store
        .files
        .iter()
        .map(|f| {
            let end = f.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1) as u64;
            assert!(f.len() as u64 - end < bs.max(1) || f.is_empty());
            end
        })
        .sum()
}

fn ratio(n: u64, of: u64) -> f64 {
    n as f64 / of as f64
}

/// `measured`, as a multiple of `of`, is within `[0.95 × budget, budget]`.
fn within(what: &str, measured: u64, of: u64, budget: f64) {
    let r = ratio(measured, of);
    assert!(
        r <= budget && r >= 0.95 * budget,
        "{what}: {measured} bytes is {r:.4}x, budget {budget}x (floor {:.4}x)",
        0.95 * budget
    );
}

/// Every write's validate re-hashes the files the build generated (File 0,
/// the guides, the index, the envelopes) from the session directory, the
/// copies a resume reads back instead of regenerating; under a size check
/// that is all it reads.
fn generated_rehashed_at_validate(c: &WriteCost) {
    let generated = c.volume_bytes - c.staged_bytes;
    within("validate hashed", c.validate.hashed, generated, 1.0);
    within("validate read", c.validate.staging_read, generated, 1.0);
}

/// Execute hashes every byte on its way to the medium, read once from disk.
fn every_byte_once_at_execute(c: &WriteCost) {
    within("execute hashed", c.execute.hashed, c.volume_bytes, 1.0);
    within("execute read", c.execute.staging_read, c.volume_bytes, 1.0);
}

/// The default write: validate's re-hash of the generated files, one hash
/// and one read of every byte at execute, and a quick confirm that hashes
/// only the small files it navigates by. As a whole, a little over one pass.
#[test]
fn a_default_write_hashes_once_and_reads_staging_once() {
    let probe = pass_count::probe();
    let c = write(&probe, SliceCheck::Size, Tier::Navigable);
    eprintln!("default write: {c:?}");
    generated_rehashed_at_validate(&c);
    every_byte_once_at_execute(&c);
    assert!(
        c.confirm.hashed < c.volume_bytes / 100,
        "a quick confirm hashes only the files it navigates by: {c:?}"
    );
    assert_eq!(c.confirm.staging_read, 0, "a confirm reads the medium");
    within("write hashed", c.whole.hashed, c.volume_bytes, 1.02);
    within("write read", c.whole.staging_read, c.volume_bytes, 1.02);
}

/// `--full-confirm`: the readback hashes every byte a second time, and
/// reads nothing from disk.
#[test]
fn a_full_confirm_hashes_every_byte_a_second_time() {
    let probe = pass_count::probe();
    let c = write(&probe, SliceCheck::Size, Tier::Integrity);
    eprintln!("full-confirm write: {c:?}");
    generated_rehashed_at_validate(&c);
    every_byte_once_at_execute(&c);
    within("confirm hashed", c.confirm.hashed, c.volume_bytes, 1.0);
    assert_eq!(c.confirm.staging_read, 0, "a confirm reads the medium");
    within("write hashed", c.whole.hashed, c.volume_bytes, 2.02);
    within("write read", c.whole.staging_read, c.volume_bytes, 1.02);
}

/// `--prewrite-hash` adds a pass over every staged byte before the write:
/// 1.0.3's default. The positive control: such a write is over the default
/// write's budget, by the one pass this suite exists to catch.
#[test]
fn a_prewrite_hash_is_one_more_pass_and_over_the_default_budget() {
    let probe = pass_count::probe();
    let c = write(&probe, SliceCheck::FullHash, Tier::Navigable);
    eprintln!("prewrite-hash write: {c:?}");
    within("validate hashed", c.validate.hashed, c.volume_bytes, 1.0);
    within(
        "validate read",
        c.validate.staging_read,
        c.volume_bytes,
        1.0,
    );
    every_byte_once_at_execute(&c);
    for (what, n) in [("hashes", c.whole.hashed), ("reads", c.whole.staging_read)] {
        let r = ratio(n, c.volume_bytes);
        assert!(
            r > 1.9,
            "a write that hashes staging first {what} every byte twice, over the \
             default write's 1.02x: {r:.4}x"
        );
    }
}

/// `stage create`: the source hashed once, beside dar (#364), and dar's
/// output hashed once as plaintext and once as ciphertext; staging is
/// written, never read back. A source written in the last two seconds costs
/// one more pass of the files that recent (`validate::recheck`); this one
/// is older.
#[test]
fn stage_create_hashes_source_plaintext_and_ciphertext_once_each() {
    let probe = pass_count::probe();
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    let paths = TapectlPaths::new(home);
    paths.ensure_dirs().unwrap();
    let conn = db::open(&paths.db_file).unwrap();
    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('escrow-holder', 0, 'active')",
        [],
    )
    .unwrap();
    let escrow = generate_keypair();
    db::queries::insert_escrow_key(
        &conn,
        conn.last_insert_rowid(),
        "pass-escrow",
        &escrow.fingerprint,
        &escrow.public_key,
        None,
    )
    .unwrap();
    let mut config = Config {
        staging: StagingConfig {
            directory: root.path().join("staging").to_string_lossy().into_owned(),
            ..Default::default()
        },
        ..Default::default()
    };
    fs::create_dir_all(&config.staging.directory).unwrap();
    config.defaults.compression = "none".into();
    config.defaults.slice_size = "3M".into();
    tapectl::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();

    let src = root.path().join("src");
    fs::create_dir_all(src.join("sub")).unwrap();
    let files: [(&Path, usize); 3] = [
        (Path::new("a.bin"), 3 * 1024 * 1024 + 17),
        (Path::new("sub/b.bin"), 2 * 1024 * 1024 + 5),
        (Path::new("sub/c.txt"), 4096),
    ];
    for (i, (rel, len)) in files.iter().enumerate() {
        fs::write(src.join(rel), noise(100 + i as u64, *len)).unwrap();
    }
    tapectl::unit::init_unit_with_config(
        &conn,
        &config,
        &src.to_string_lossy(),
        "op",
        Some("pass-stage"),
        &[],
        None,
    )
    .unwrap();
    // A file changed within two seconds of its hash is hashed a second time
    // after dar has read it (git's racy case, `validate::recheck`); a source
    // that has sat still is not. Production's sources have sat still.
    std::thread::sleep(std::time::Duration::from_millis(2_200));
    let snapshot = tapectl::staging::snapshot_create(&conn, "pass-stage", &config).unwrap();

    let start = probe.read();
    tapectl::staging::stage_create(&conn, &paths, &config, snapshot, true).unwrap();
    let spent = probe.read() - start;

    let source: u64 = files.iter().map(|(_, len)| *len as u64).sum();
    let (plain, cipher): (i64, i64) = conn
        .query_row(
            "SELECT SUM(size_bytes), SUM(encrypted_bytes) FROM stage_slices",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(
        plain as u64 > source,
        "positive control: dar's output holds the source ({plain} vs {source})"
    );
    let layers = source + plain as u64 + cipher as u64;
    eprintln!("stage create: {spent:?}, source {source}, plain {plain}, cipher {cipher}");
    // The unit's own dotfile is hashed with the source; it is a few hundred
    // bytes, so the ceiling allows 0.1%.
    within("stage hashed", spent.hashed, layers, 1.001);
    assert!(
        spent.hashed >= layers,
        "every byte of each layer was hashed: {spent:?}, layers {layers}"
    );
    assert_eq!(
        spent.staging_read, 0,
        "stage create writes staging and never reads it back"
    );
}
