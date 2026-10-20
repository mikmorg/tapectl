//! Test support (#420): one tape that ended early, written by the real
//! session, for every reader of a short tape and of a `cut` row.
//!
//! Nothing is hand-built but the drive. A home is made as `tapectl init` and
//! `tenant add` make one (an operator and a tenant, each with a key on disk),
//! three units are staged as `stage create` stages them (a real dar archive
//! cut into two slices each, every slice age-encrypted to the tenant and the
//! operator), and the production session (build, validate, plan, execute,
//! seal, confirm at the integrity tier) writes them to a `MemStore` whose
//! early warning comes one byte into `bravo`'s first slice. The session then
//! seals short at `bravo`'s first slice, K: `alpha` is whole on the tape
//! (`completed`), `bravo` is cut with its first slice on the tape and its
//! second not, and `charlie` is cut with nothing on the tape (`volume-format-
//! v2.md` §4.1-§4.3; `layout-session.md` "The short ending").
//!
//! [`full_tape`] is the same write with no warning: the control.

use std::path::PathBuf;

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::config::TapectlPaths;
use crate::crypto::keys;
use crate::store::{MemStore, Tier};
use crate::volume::build::{self, BuildInputs, BuildSlice, BuildUnit, TenantInfo};
use crate::volume::layout_model::{KeyAvailability, SliceCheck, ZoneKind};
use crate::volume::session::{ConfirmOutcome, Ending, ExecuteOutcome};

pub(crate) const BS: u64 = 65536;
pub(crate) const LABEL: &str = "SHORT01";
pub(crate) const VOL_UUID: &str = "42000000-0000-4000-8000-000000000420";
pub(crate) const OPERATOR: &str = "writer";
pub(crate) const TENANT: &str = "family";
/// The units, in plan order: `alpha` whole, `bravo` cut by the warning in
/// its first slice, `charlie` planned after it.
pub(crate) const UNITS: [&str; 3] = ["alpha", "bravo", "charlie"];

/// A written tape and the home that wrote it.
pub(crate) struct ShortTape {
    /// The home's catalog: the one that wrote the tape.
    pub conn: Connection,
    /// The home: the operator's and the tenant's keys are in its `keys/`.
    pub paths: TapectlPaths,
    /// The sealed tape.
    pub store: MemStore,
    pub volume_id: i64,
    /// K: the last file before the seal. On [`full_tape`], M-1.
    pub ended_after: u32,
    /// M: the seal's planned position.
    pub planned_seal: u32,
    /// Each unit and its slices' planned tape positions, in slice order.
    pub positions: Vec<(String, Vec<u32>)>,
    /// Each unit and the files its version holds.
    pub files: Vec<(String, Vec<(String, Vec<u8>)>)>,
    /// The catalog as a backup taken after `volume init` and before the
    /// write left it (#360's "lost write").
    pub backup: PathBuf,
    /// The operator's identity: opens the operator envelope and every slice.
    pub operator: age::x25519::Identity,
    pub _dirs: Vec<TempDir>,
}

impl ShortTape {
    /// The planned tape positions of `unit`'s slices.
    pub(crate) fn positions_of(&self, unit: &str) -> &[u32] {
        &self
            .positions
            .iter()
            .find(|(u, _)| u == unit)
            .unwrap_or_else(|| panic!("no unit {unit} in the fixture"))
            .1
    }

    /// `(unit, writes.status, snapshots.status)` of each unit, in plan order.
    pub(crate) fn rows(&self, conn: &Connection) -> Vec<(String, String, String)> {
        conn.prepare(
            "SELECT u.name, w.status, s.status FROM writes w
               JOIN snapshots s ON s.id = w.snapshot_id
               JOIN units u ON u.id = s.unit_id
               JOIN volumes v ON v.id = w.volume_id
              WHERE v.label = ?1 ORDER BY u.name",
        )
        .unwrap()
        .query_map(params![LABEL], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }
}

fn sha_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn noise(len: usize, seed: u32) -> Vec<u8> {
    (0..len as u32)
        .map(|i| (i.wrapping_add(seed).wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}

/// The tape that ended early: see the module doc.
pub(crate) fn short_tape() -> ShortTape {
    written_tape(true)
}

/// The same three units on a tape that sealed full: the control.
pub(crate) fn full_tape() -> ShortTape {
    written_tape(false)
}

fn written_tape(warning: bool) -> ShortTape {
    let home = TempDir::new().unwrap();
    let paths = TapectlPaths::new(home.path().join(".tapectl"));
    paths.ensure_dirs().unwrap();
    std::fs::create_dir_all(&paths.logs_dir).unwrap();
    let conn = crate::db::open(&paths.db_file).unwrap();
    crate::tenant::add_tenant(&conn, &paths, OPERATOR, Some("System operator"), true).unwrap();
    let tenant_id = crate::tenant::add_tenant(&conn, &paths, TENANT, None, false).unwrap();
    let public_key = |name: &str| -> String {
        conn.query_row(
            "SELECT k.public_key FROM encryption_keys k JOIN tenants t ON t.id = k.tenant_id
              WHERE t.name = ?1 AND k.is_active = 1",
            params![name],
            |r| r.get(0),
        )
        .unwrap()
    };
    let operator_public = public_key(OPERATOR);
    let tenant_public = public_key(TENANT);
    let recipients = vec![tenant_public.clone(), operator_public.clone()];
    let operator_file =
        keys::KeyFile::read(&keys::key_paths(&paths.keys_dir, OPERATOR, "primary").1).unwrap();
    let operator = keys::KeyFile::all_identities(std::slice::from_ref(&operator_file))
        .into_iter()
        .next()
        .expect("the operator's key is in the home");
    assert_eq!(operator.to_public().to_string(), operator_public);

    conn.execute(
        "INSERT INTO volumes (label, uuid, backend_type, backend_name, media_type,
                              capacity_bytes, status)
         VALUES (?1, ?2, 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
        params![LABEL, VOL_UUID],
    )
    .unwrap();
    let volume_id = conn.last_insert_rowid();

    let work = TempDir::new().unwrap();
    let slices_dir = TempDir::new().unwrap();
    let mut units = Vec::new();
    let mut files_by_unit = Vec::new();
    let mut stage_set_ids = Vec::new();
    for (u, name) in UNITS.iter().enumerate() {
        let files: Vec<(String, Vec<u8>)> = (0..2u32)
            .map(|i| {
                (
                    format!("{name}-{i}.bin"),
                    noise(100_000, 7919 * (u as u32 * 2 + i) + 3),
                )
            })
            .collect();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES (?1, ?2, ?3, ?4, 'active')",
            params![
                format!("4200000{u}-0000-4000-8000-00000000042{u}"),
                name,
                tenant_id,
                format!("/src/{name}")
            ],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'staged', ?2, 2, 200000)",
            params![unit_id, format!("/src/{name}")],
        )
        .unwrap();
        let snapshot_id = conn.last_insert_rowid();
        for (path, bytes) in &files {
            crate::db::files::fixture::insert(
                &conn,
                snapshot_id,
                path,
                bytes.len() as i64,
                "regular",
                Some(&sha_hex(bytes)),
            );
        }
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size, key_fingerprints)
             VALUES (?1, 'staged', 153600, ?2)",
            params![snapshot_id, serde_json::to_string(&recipients).unwrap()],
        )
        .unwrap();
        let stage_set_id = conn.last_insert_rowid();
        stage_set_ids.push(stage_set_id);

        // A real dar archive of the two files, cut at 150k: two slices.
        let tree = work.path().join(name);
        std::fs::create_dir_all(&tree).unwrap();
        for (path, bytes) in &files {
            std::fs::write(tree.join(path), bytes).unwrap();
        }
        let base = work.path().join(format!("{name}-arch"));
        let created = std::process::Command::new("dar")
            .arg("-c")
            .arg(&base)
            .arg("-R")
            .arg(&tree)
            .arg("-s")
            .arg("150k")
            .arg("-Q")
            .output()
            .unwrap();
        assert!(
            created.status.success(),
            "dar -c failed in test setup: {}",
            String::from_utf8_lossy(&created.stderr)
        );
        let mut slices = Vec::new();
        for n in 1.. {
            let part = work.path().join(format!("{name}-arch.{n}.dar"));
            if !part.exists() {
                break;
            }
            let plain = std::fs::read(&part).unwrap();
            let cipher = crate::staging::encrypt_data(&plain, &recipients).unwrap();
            conn.execute(
                "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes,
                                           encrypted_bytes, sha256_plain, sha256_encrypted)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    stage_set_id,
                    n,
                    plain.len() as i64,
                    cipher.len() as i64,
                    sha_hex(&plain),
                    sha_hex(&cipher)
                ],
            )
            .unwrap();
            let slice_id = conn.last_insert_rowid();
            let path = slices_dir.path().join(format!("slice_{slice_id}.age"));
            std::fs::write(&path, &cipher).unwrap();
            conn.execute(
                "UPDATE stage_slices SET staging_path = ?1 WHERE id = ?2",
                params![path.to_string_lossy(), slice_id],
            )
            .unwrap();
            slices.push(BuildSlice {
                slice_id,
                slice_number: n,
                size_bytes: plain.len() as i64,
                encrypted_bytes: cipher.len() as i64,
                sha256_plain: sha_hex(&plain),
                sha256_encrypted: sha_hex(&cipher),
                staging_path: path,
            });
        }
        assert_eq!(slices.len(), 2, "each fixture unit is two dar slices");
        conn.execute(
            "UPDATE stage_sets SET num_slices = 2 WHERE id = ?1",
            params![stage_set_id],
        )
        .unwrap();
        units.push(BuildUnit {
            stage_set_id,
            snapshot_id,
            unit_name: name.to_string(),
            unit_uuid: format!("4200000{u}-0000-4000-8000-00000000042{u}"),
            tenant_id,
            dar_version: Some("2.7.20".to_string()),
            dar_command: None,
            catalog_path: None,
            snapshot_version: 1,
            slices,
        });
        files_by_unit.push((name.to_string(), files));
    }

    // The operator envelope's catalog.db, which names each unit's owner.
    let catalog_dir = TempDir::new().unwrap();
    let catalog_db = catalog_dir.path().join("catalog.db");
    crate::db::catalog_snapshot::build_catalog_snapshot(&conn, &stage_set_ids, &catalog_db)
        .unwrap();
    let inputs = BuildInputs {
        label: LABEL.to_string(),
        volume_uuid: VOL_UUID.to_string(),
        media_type: "LTO-6".to_string(),
        tapectl_version: "0.1.0-test".to_string(),
        created_at: "2026-10-20T00:00:00Z".to_string(),
        block_size: BS,
        usable_bytes: 64 * 1024 * 1024,
        enospc_buffer: 1024 * 1024,
        nominal_capacity: 2_500_000_000_000,
        mam_capacity: 0,
        mam_manufacturer: String::new(),
        mam_serial: String::new(),
        cartridge_identity_source: None,
        mam_length: 0,
        mam_loads: 0,
        units: units.clone(),
        tenants: vec![TenantInfo {
            tenant_id,
            tenant_name: TENANT.to_string(),
            public_keys: vec![tenant_public],
        }],
        operator_public_keys: vec![operator_public],
        escrow_public_key: None,
        catalog_db_path: Some(catalog_db),
        index: None,
    };
    let session_dir = TempDir::new().unwrap();
    let built = build::build(&inputs, session_dir.path()).unwrap();

    let position_of = |stage_slice: i64| -> u32 {
        built
            .layout
            .entries
            .iter()
            .find(|e| matches!(e.kind, ZoneKind::Slice { stage_slice_id } if stage_slice_id == stage_slice))
            .expect("every staged slice is planned")
            .position as u32
    };
    let positions: Vec<(String, Vec<u32>)> = units
        .iter()
        .map(|u| {
            (
                u.unit_name.clone(),
                u.slices.iter().map(|s| position_of(s.slice_id)).collect(),
            )
        })
        .collect();
    let planned_seal = built
        .layout
        .entries
        .iter()
        .find(|e| matches!(e.kind, ZoneKind::SealMarker))
        .expect("a planned seal")
        .position as u32;
    // The warning one byte into bravo's first slice: its first block reaches
    // it, its second is the first refused, and K is that slice.
    let bravo_first = positions[1].1[0];
    let bytes_before: u64 = built
        .layout
        .entries
        .iter()
        .filter(|e| (e.position as u32) < bravo_first)
        .map(|e| e.on_tape_bytes(BS).unwrap())
        .sum();
    let mut store = if warning {
        MemStore::new(BS as usize).with_early_warning_after(bytes_before + 1)
    } else {
        MemStore::new(BS as usize)
    };

    // The backup, taken between `volume init` and `volume write`.
    let backup_dir = TempDir::new().unwrap();
    let backup = backup_dir.path().join("backup.db");
    conn.execute("VACUUM INTO ?1", params![backup.to_string_lossy()])
        .unwrap();

    let keys = KeyAvailability {
        tenant_ids: vec![tenant_id],
        tenants_with_active_key: [tenant_id].into_iter().collect(),
        operator_key_present: true,
        escrow_recipient_present: None,
        stage_sets_lacking_escrow: None,
    };
    let validated = built
        .into_validated(&keys, SliceCheck::Size, &mut store)
        .unwrap();
    let planned = validated
        .plan(&conn, volume_id, &units)
        .unwrap()
        .with_post_warning_bytes(Some(64 * 1024 * 1024));
    let ExecuteOutcome::Ready(ready) = planned.execute(&conn, &mut store).unwrap() else {
        panic!("the write must reach Ready");
    };
    let sealed = ready.seal(&mut store).unwrap();
    let ending = sealed.ending();
    let expected = if warning {
        Ending::Short {
            ended_after: bravo_first,
        }
    } else {
        Ending::Full
    };
    assert_eq!(ending, expected, "the fixture's ending");
    // What `write::finish_session` records once `seal()` returns.
    conn.execute(
        "UPDATE volumes SET sealed_at = datetime('now'), ended_after = ?2 WHERE id = ?1",
        params![volume_id, ending.recorded_ended_after()],
    )
    .unwrap();
    let ConfirmOutcome::Sealed(_) = sealed.confirm(&conn, &mut store, Tier::Integrity).unwrap()
    else {
        panic!("the write must seal and confirm");
    };

    ShortTape {
        conn,
        paths,
        store,
        volume_id,
        ended_after: if warning {
            bravo_first
        } else {
            planned_seal - 1
        },
        planned_seal,
        positions,
        files: files_by_unit,
        backup,
        operator,
        _dirs: vec![
            home,
            work,
            slices_dir,
            catalog_dir,
            session_dir,
            backup_dir,
        ],
    }
}

/// The fixture is what it says: the session split the rows, and the tape
/// holds nothing after its short seal.
#[test]
fn the_short_tape_fixture_is_short_and_split() {
    let t = short_tape();
    let k = t.ended_after;
    assert_eq!(t.positions_of("bravo")[0], k);
    assert!(t.positions_of("bravo")[1] > k);
    assert!(t.positions_of("charlie").iter().all(|p| *p > k));
    assert!(t.positions_of("alpha").iter().all(|p| *p < k));
    assert!(k + 1 < t.planned_seal, "a short seal, before the planned one");
    assert_eq!(
        t.rows(&t.conn),
        vec![
            ("alpha".into(), "completed".into(), "current".into()),
            ("bravo".into(), "cut".into(), "staged".into()),
            ("charlie".into(), "cut".into(), "staged".into()),
        ]
    );
    let recorded: (String, Option<i64>) = t
        .conn
        .query_row(
            "SELECT status, ended_after FROM volumes WHERE id = ?1",
            params![t.volume_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(recorded, ("sealed".to_string(), Some(i64::from(k))));
    let mut store = t.store;
    assert!(
        crate::store::read_small_bytes(&mut store, k + 2, "past the seal").is_err(),
        "nothing follows the short seal"
    );
    let seal = crate::store::read_small_bytes(&mut store, k + 1, "seal").unwrap();
    let parsed =
        crate::volume::format::parse_seal_marker(&String::from_utf8_lossy(&seal)).unwrap();
    assert_eq!(parsed.ended_after, Some(u64::from(k)));
}
