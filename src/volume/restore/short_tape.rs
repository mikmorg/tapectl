//! Test support (#420): one tape that ended early, written by the real
//! session, for every reader of a short tape and of a range.
//!
//! Nothing is hand-built but the drive. A home is made as `tapectl init` and
//! `tenant add` make one (an operator and a tenant, each with a key on disk),
//! three units are staged by the real `stage create` (GNU tar's stream cut
//! at member starts into two slices each, every slice age-encrypted to the
//! tenant and the operator, and a file table kept), and the production
//! session (build, validate, plan, execute, seal, confirm at the integrity
//! tier) writes them to a `MemStore` whose
//! early warning comes one byte into `bravo`'s first slice. The session then
//! seals short at `bravo`'s first slice, K: `alpha` is whole on the tape
//! (`completed`), `bravo` is a `range` with its first slice on the tape and
//! its second not, and `charlie` was planned and not written, so it has no
//! row (`volume-format-v3.md` §5; ADR-0012 item 288; `layout-session.md`
//! "Confirm of a short ending").
//!
//! [`full_tape`] is the same write with no warning: the control.
//!
//! Its items live in a `#[cfg(test)] mod tests`, as `heir_tests` keeps its
//! own, so the source scans that read only a file's shipped part
//! (`tests/sg_boundary.rs`) see none of it; they are re-exported here.

#[cfg(test)]
pub(crate) use tests::*;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rusqlite::{params, Connection};
    use tempfile::TempDir;

    use crate::config::{Config, TapectlPaths};
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
    /// The units, in plan order: `alpha` whole, `bravo` ended by the warning in
    /// its first slice, `charlie` planned after it.
    pub(crate) const UNITS: [&str; 3] = ["alpha", "bravo", "charlie"];

    /// One unit version's files: `(path, bytes)`.
    pub(crate) type Files = Vec<(String, Vec<u8>)>;

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
        pub files: Vec<(String, Files)>,
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

    fn noise(len: usize, seed: u32) -> Vec<u8> {
        (0..len as u32)
            .map(|i| (i.wrapping_add(seed).wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect()
    }

    /// The tape that ended early: see the module doc.
    pub(crate) fn short_tape() -> ShortTape {
        written_tape(Some(1))
    }

    /// The same three units on a tape that sealed full: the control.
    pub(crate) fn full_tape() -> ShortTape {
        written_tape(None)
    }

    /// The same write with the early warning in `alpha`'s first slice: a sealed
    /// volume that holds no unit whole, its one write a `range` of `alpha`
    /// (the units after it have no row).
    pub(crate) fn range_only_tape() -> ShortTape {
        written_tape(Some(0))
    }

    /// `warning`: the index in [`UNITS`] of the unit in whose first slice the
    /// early warning comes, or `None` for a tape that seals full.
    fn written_tape(warning: Option<usize>) -> ShortTape {
        let home = TempDir::new().unwrap();
        let paths = TapectlPaths::new(home.path().join(".tapectl"));
        paths.ensure_dirs().unwrap();
        std::fs::create_dir_all(&paths.logs_dir).unwrap();
        let conn = crate::db::open(&paths.db_file).unwrap();
        // The units are staged by the real `stage create`: GNU tar's stream,
        // cut at member starts into slices of at most 150 KiB, which holds
        // a unit's dotfile and its first file and not the second.
        let work = TempDir::new().unwrap();
        let staging = work.path().join("staging");
        let sources = work.path().join("sources");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::create_dir_all(&sources).unwrap();
        let mut config = Config::default();
        config.staging.directory = staging.to_string_lossy().into_owned();
        config.defaults.slice_size = "150KiB".to_string();
        crate::tenant::add_tenant(&conn, &paths, OPERATOR, Some("System operator"), true).unwrap();
        let tenant_id = crate::tenant::add_tenant(&conn, &paths, TENANT, None, false).unwrap();
        // `stage create` refuses a home with no escrow recipient (ADR-0005).
        crate::cli::key::create_escrow_recipient(&conn, &paths, None).unwrap();
        let public_key = |name: &str| -> String {
            conn.query_row(
                "SELECT k.public_key FROM encryption_keys k JOIN tenants t ON t.id = k.tenant_id
                  WHERE t.name = ?1 AND k.is_active = 1 AND k.is_escrow = 0",
                params![name],
                |r| r.get(0),
            )
            .unwrap()
        };
        let operator_public = public_key(OPERATOR);
        let tenant_public = public_key(TENANT);
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
            let dir = sources.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            for (path, bytes) in &files {
                std::fs::write(dir.join(path), bytes).unwrap();
            }
            crate::unit::init_unit_with_config(
                &conn,
                &config,
                dir.to_str().unwrap(),
                TENANT,
                Some(name),
                &[],
                None,
            )
            .unwrap();
            let snapshot_id = crate::staging::snapshot_create(&conn, name, &config).unwrap();
            let stage_set_id =
                crate::staging::stage_create(&conn, &paths, &config, snapshot_id, true)
                    .unwrap_or_else(|e| panic!("stage create {name}: {e}"));
            stage_set_ids.push(stage_set_id);

            let (unit_uuid, tar_version, stream_bytes, compression, snapshot_version): (
                String,
                Option<String>,
                Option<i64>,
                Option<String>,
                i64,
            ) = conn
                .query_row(
                    "SELECT u.uuid, ss.tar_version, ss.stream_bytes, ss.compression, s.version
                       FROM stage_sets ss
                       JOIN snapshots s ON s.id = ss.snapshot_id
                       JOIN units u ON u.id = s.unit_id
                      WHERE ss.id = ?1",
                    params![stage_set_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .unwrap();
            let slices: Vec<BuildSlice> = conn
                .prepare(
                    "SELECT id, slice_number, size_bytes, encrypted_bytes, sha256_plain,
                            sha256_encrypted, staging_path, continues
                       FROM stage_slices
                      WHERE stage_set_id = ?1 AND staging_path IS NOT NULL
                      ORDER BY slice_number",
                )
                .unwrap()
                .query_map(params![stage_set_id], |r| {
                    Ok(BuildSlice {
                        slice_id: r.get(0)?,
                        slice_number: r.get(1)?,
                        size_bytes: r.get(2)?,
                        encrypted_bytes: r.get(3)?,
                        sha256_plain: r.get(4)?,
                        sha256_encrypted: r.get(5)?,
                        staging_path: PathBuf::from(r.get::<_, String>(6)?),
                        continues: r.get(7)?,
                    })
                })
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(slices.len(), 2, "each fixture unit is two slices");
            units.push(BuildUnit {
                stage_set_id,
                snapshot_id,
                unit_name: name.to_string(),
                unit_uuid,
                tenant_id,
                tar_version,
                stream_bytes: stream_bytes.unwrap_or(0),
                compression: compression.filter(|c| c != "none"),
                snapshot_version,
                slices,
                span: None,
            });
            files_by_unit.push((name.to_string(), files));
        }

        // The operator envelope's catalog.db, which names each unit's owner.
        let catalog_dir = TempDir::new().unwrap();
        let catalog_db = catalog_dir.path().join("catalog.db");
        crate::db::catalog_snapshot::build_catalog_snapshot(&conn, &stage_set_ids, &catalog_db)
            .unwrap();
        // The cross-volume index, as `volume write` makes it (a rebuild
        // refuses a tape without one).
        let index = crate::volume::index::generate_files(
            &conn,
            &crate::policy::coverage::IndexThisTape {
                label: LABEL,
                uuid: VOL_UUID,
                cartridge_serial: "",
                generation: "LTO-6",
            },
            &stage_set_ids,
            &catalog_dir.path().join("index"),
        )
        .unwrap();
        // Each staging's file table and id map, as `volume write` makes them.
        let tables = crate::volume::write::generate_tables(
            &conn,
            &units,
            &catalog_dir.path().join("tables"),
        )
        .unwrap();
        let inputs = BuildInputs {
            tables: Some(tables),
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
            index: Some(index),
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
        // The warning one byte into the unit's first slice: its first block
        // reaches it, its second is the first refused, and K is that slice.
        let warned_at = warning.map(|u| positions[u].1[0]);
        let mut store = match warned_at {
            Some(k) => {
                let bytes_before: u64 = built
                    .layout
                    .entries
                    .iter()
                    .filter(|e| (e.position as u32) < k)
                    .map(|e| e.on_tape_bytes(BS).unwrap())
                    .sum();
                MemStore::new(BS as usize).with_early_warning_after(bytes_before + 1)
            }
            None => MemStore::new(BS as usize),
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
        let expected = match warned_at {
            Some(ended_after) => Ending::Short { ended_after },
            None => Ending::Full,
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
            ended_after: warned_at.unwrap_or(planned_seal - 1),
            planned_seal,
            positions,
            files: files_by_unit,
            backup,
            operator,
            _dirs: vec![home, work, catalog_dir, session_dir, backup_dir],
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
        assert!(
            k + 1 < t.planned_seal,
            "a short seal, before the planned one"
        );
        assert_eq!(
            t.rows(&t.conn),
            vec![
                ("alpha".into(), "completed".into(), "current".into()),
                ("bravo".into(), "range".into(), "staged".into()),
            ],
            "charlie was planned and not written: no row (item 288)"
        );
        let leftover: i64 = t
            .conn
            .query_row(
                "SELECT COUNT(*) FROM write_positions wp
                   JOIN stage_slices sl ON sl.id = wp.stage_slice_id
                   JOIN stage_sets ss ON ss.id = sl.stage_set_id
                   JOIN snapshots s ON s.id = ss.snapshot_id
                   JOIN units u ON u.id = s.unit_id
                  WHERE u.name = 'charlie'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(leftover, 0, "its positions went with the row");
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
}
