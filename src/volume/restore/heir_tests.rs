//! ADR-0012 2026-10-19 item 107: the heir's path, end to end, with only the
//! escrow secret.
//!
//! A tape is written by the real session — real dar slices, each age file
//! encrypted to the tenant, the operator and the escrow recipient — into a
//! `MemStore`. The heir's home is then made the way `tapectl init
//! --escrow-public-key` makes one (a fresh operator whose keys did not write
//! the tape, and the escrow PUBLIC key registered; `init` itself lives in the
//! binary, so the library calls it makes stand in), and the heir runs, in
//! library form, exactly the commands the ADR names:
//!
//! ```text
//! tapectl catalog rebuild --from-volume --key escrow.key
//! tapectl restore unit --unit keepsake --key escrow.key --to DIR
//! ```
//!
//! Nothing here is faked but the drive: the rebuild is `rebuild_from_store`
//! and the restore `restore_unit_from_store`, the functions the commands call
//! over a `TapeStore`.

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    use super::super::*;
    use crate::crypto::keys::{generate_keypair, KeyFile};
    use crate::progress::{self, Display};
    use crate::store::MemStore;
    use crate::tape::contact::Medium;
    use crate::volume::build::{self, BuildInputs, BuildSlice, BuildUnit, TenantInfo};
    use crate::volume::layout_model::{KeyAvailability, SliceCheck};
    use crate::volume::rebuild;
    use crate::volume::session::{ConfirmOutcome, ExecuteOutcome};

    const BS: u64 = 65536;
    const LABEL: &str = "HEIR01";
    const VOL_UUID: &str = "49000000-0000-4000-8000-000000000107";
    const UNIT: &str = "keepsake";

    fn sha_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn noise(len: usize, seed: u32) -> Vec<u8> {
        (0..len as u32)
            .map(|i| (i.wrapping_add(seed).wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect()
    }

    /// Four 100 KB files: several dar slices at `-s 150k`.
    fn tree() -> Vec<(String, Vec<u8>)> {
        (0..4)
            .map(|i| (format!("f{i}.bin"), noise(100_000, i * 7919 + 3)))
            .collect()
    }

    fn cfg() -> &'static Config {
        static CFG: std::sync::OnceLock<Config> = std::sync::OnceLock::new();
        CFG.get_or_init(Config::default)
    }

    /// What the writer's machine leaves behind: the sealed tape, the files
    /// it holds, and the escrow keypair — whose SECRET is the only thing the
    /// heir has.
    struct Tape {
        store: MemStore,
        files: Vec<(String, Vec<u8>)>,
        escrow_public: String,
        escrow_secret: String,
        operator_public: String,
        _dirs: Vec<TempDir>,
    }

    fn written_tape() -> Tape {
        let db_dir = tempfile::tempdir().unwrap();
        let src = crate::db::open(&db_dir.path().join("src.db")).unwrap();
        let operator = generate_keypair();
        let family = generate_keypair();
        let escrow = generate_keypair();

        let tenant = |name: &str, is_operator: bool, kp: &crate::crypto::keys::GeneratedKeypair| {
            src.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES (?1, ?2, 'active')",
                params![name, is_operator as i64],
            )
            .unwrap();
            let id = src.last_insert_rowid();
            src.execute(
                "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key,
                                              key_type, is_active)
                 VALUES (?1, ?2, ?3, ?4, 'primary', 1)",
                params![id, format!("{name}-primary"), kp.fingerprint, kp.public_key],
            )
            .unwrap();
            id
        };
        tenant("writer", true, &operator);
        let tenant_id = tenant("family", false, &family);

        src.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES ('unit-uuid-107', ?1, ?2, '/src/keepsake', 'active')",
            params![UNIT, tenant_id],
        )
        .unwrap();
        let unit_id = src.last_insert_rowid();
        src.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'staged', '/src/keepsake', 4, 400000)",
            params![unit_id],
        )
        .unwrap();
        let snapshot_id = src.last_insert_rowid();
        let files = tree();
        for (path, bytes) in &files {
            crate::db::files::fixture::insert(
                &src,
                snapshot_id,
                path,
                bytes.len() as i64,
                "regular",
                Some(&sha_hex(bytes)),
            );
        }
        let recipients = vec![
            family.public_key.clone(),
            operator.public_key.clone(),
            escrow.public_key.clone(),
        ];
        src.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size, key_fingerprints)
             VALUES (?1, 'staged', 524288, ?2)",
            params![snapshot_id, serde_json::to_string(&recipients).unwrap()],
        )
        .unwrap();
        let stage_set_id = src.last_insert_rowid();
        src.execute(
            "INSERT INTO volumes (label, uuid, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES (?1, ?2, 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
            params![LABEL, VOL_UUID],
        )
        .unwrap();
        let volume_id = src.last_insert_rowid();

        // A real dar archive of the tree, cut into slices.
        let work = tempfile::tempdir().unwrap();
        let tree_dir = work.path().join("src");
        std::fs::create_dir_all(&tree_dir).unwrap();
        for (path, bytes) in &files {
            std::fs::write(tree_dir.join(path), bytes).unwrap();
        }
        let base = work.path().join("arch");
        let created = std::process::Command::new("dar")
            .arg("-c")
            .arg(&base)
            .arg("-R")
            .arg(&tree_dir)
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
        let slices_dir = tempfile::tempdir().unwrap();
        let mut slices = Vec::new();
        for n in 1.. {
            let part = work.path().join(format!("arch.{n}.dar"));
            if !part.exists() {
                break;
            }
            let plain = std::fs::read(&part).unwrap();
            let cipher = crate::staging::encrypt_data(&plain, &recipients).unwrap();
            src.execute(
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
            let slice_id = src.last_insert_rowid();
            let path = slices_dir.path().join(format!("slice_{slice_id}.age"));
            std::fs::write(&path, &cipher).unwrap();
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
        assert!(slices.len() >= 3, "want a many-slice archive");

        let unit = BuildUnit {
            stage_set_id,
            snapshot_id,
            unit_name: UNIT.to_string(),
            unit_uuid: "unit-uuid-107".to_string(),
            tenant_id,
            dar_version: Some("2.7.20".to_string()),
            dar_command: None,
            catalog_path: None,
            snapshot_version: 1,
            slices,
        };
        // The operator envelope's catalog.db, which names the tenant that
        // owns the unit: without it every unit files under a placeholder.
        let catalog_dir = tempfile::tempdir().unwrap();
        let catalog_db = catalog_dir.path().join("catalog.db");
        crate::db::catalog_snapshot::build_catalog_snapshot(&src, &[stage_set_id], &catalog_db)
            .unwrap();
        let inputs = BuildInputs {
            label: LABEL.to_string(),
            volume_uuid: VOL_UUID.to_string(),
            media_type: "LTO-6".to_string(),
            tapectl_version: "0.1.0-test".to_string(),
            created_at: "2026-10-19T00:00:00Z".to_string(),
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
            units: vec![unit.clone()],
            tenants: vec![TenantInfo {
                tenant_id,
                tenant_name: "family".to_string(),
                public_keys: vec![family.public_key.clone()],
            }],
            operator_public_keys: vec![operator.public_key.clone()],
            escrow_public_key: Some(escrow.public_key.clone()),
            catalog_db_path: Some(catalog_db),
            index: None,
        };
        let session_dir = tempfile::tempdir().unwrap();
        let built = build::build(&inputs, session_dir.path()).unwrap();
        let keys = KeyAvailability {
            tenant_ids: vec![tenant_id],
            tenants_with_active_key: [tenant_id].into_iter().collect(),
            operator_key_present: true,
            escrow_recipient_present: None,
            stage_sets_lacking_escrow: None,
        };
        let mut store = MemStore::new(BS as usize);
        let validated = built
            .into_validated(&keys, SliceCheck::Size, &mut store)
            .unwrap();
        let planned = validated.plan(&src, volume_id, &[unit]).unwrap();
        let ExecuteOutcome::Ready(ready) = planned.execute(&src, &mut store).unwrap() else {
            panic!("the write must reach Ready");
        };
        let sealed = ready.seal(&mut store).unwrap();
        let ConfirmOutcome::Sealed(_) = sealed
            .confirm(&src, &mut store, crate::store::Tier::Integrity)
            .unwrap()
        else {
            panic!("the write must seal and confirm");
        };

        Tape {
            store,
            files,
            escrow_public: escrow.public_key,
            escrow_secret: escrow.secret_key,
            operator_public: operator.public_key,
            _dirs: vec![db_dir, work, slices_dir, catalog_dir, session_dir],
        }
    }

    /// The heir's machine: `tapectl init --operator heir --escrow-public-key
    /// <the cover sheet's>` — a fresh operator whose keys did not write the
    /// tape, and the escrow PUBLIC key registered. The secret is in no file
    /// of this home.
    struct Home {
        paths: TapectlPaths,
        conn: Connection,
        _dir: TempDir,
    }

    fn heir_home(escrow_public: &str) -> Home {
        let dir = TempDir::new().unwrap();
        let paths = TapectlPaths::new(dir.path().join(".tapectl"));
        paths.ensure_dirs().unwrap();
        std::fs::create_dir_all(&paths.logs_dir).unwrap();
        let conn = crate::db::open(&paths.db_file).unwrap();
        crate::tenant::add_tenant(&conn, &paths, "heir", Some("System operator"), true).unwrap();
        crate::cli::key::adopt_escrow_recipient(&conn, &paths, escrow_public).unwrap();
        Home {
            paths,
            conn,
            _dir: dir,
        }
    }

    /// Every file under `root`, relative, with its sha256.
    fn tree_listing(root: &Path) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push((
                        p.strip_prefix(root).unwrap().display().to_string(),
                        sha_hex(&std::fs::read(&p).unwrap()),
                    ));
                }
            }
        }
        out.sort();
        out
    }

    /// Files under `root` whose bytes hold `needle`.
    fn files_holding(root: &Path, needle: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if std::fs::read(&p)
                    .unwrap()
                    .windows(needle.len())
                    .any(|w| w == needle.as_bytes())
                {
                    out.push(p.strip_prefix(root).unwrap().display().to_string());
                }
            }
        }
        out.sort();
        out
    }

    fn rebuild_with(home: &Home, tape: &mut Tape, given: &[KeyFile]) -> Result<rebuild::RebuildReport> {
        let scratch = TempDir::new().unwrap();
        rebuild::rebuild_from_store(
            &home.conn,
            &mut tape.store,
            &KeyFile::all_identities(given),
            Some(LABEL),
            "recovered",
            Some("lto0"),
            scratch.path(),
            "memstore",
            ContactSite::new(
                cfg(),
                Operation::CatalogRebuild,
                "memstore",
                Medium::NoBackend,
            ),
        )
    }

    fn restore_with(
        home: &Home,
        tape: &mut Tape,
        dest: &Path,
        options: &RestoreOptions,
    ) -> Result<RestoreReport> {
        let dest = dest.to_string_lossy().into_owned();
        restore_unit_from_store(
            &home.conn,
            &home.paths,
            cfg(),
            UNIT,
            LABEL,
            1,
            RestoreTarget::Unit { dest_dir: &dest },
            options,
            &mut tape.store,
            ContactSite::new(
                cfg(),
                Operation::RestoreUnit,
                "memstore",
                Medium::NoBackend,
            ),
        )
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// THE acceptance test of item 107: an heir with only the escrow secret
    /// rebuilds the catalog and restores through tapectl, and the secret
    /// goes nowhere.
    ///
    /// ```text
    /// tapectl init --operator heir --escrow-public-key age1...
    /// tapectl catalog rebuild --from-volume --key escrow.key
    /// tapectl restore unit --unit keepsake --key escrow.key --to DIR
    /// ```
    #[test]
    fn an_heir_with_only_the_escrow_secret_rebuilds_and_restores_through_tapectl() {
        let mut tape = written_tape();
        let home = heir_home(&tape.escrow_public);
        let keydir = TempDir::new().unwrap();
        let escrow_key = keydir.path().join("escrow.key");
        std::fs::write(
            &escrow_key,
            format!(
                "# created: 2026-10-19T00:00:00Z\n# public key: {}\n{}\n",
                tape.escrow_public, tape.escrow_secret
            ),
        )
        .unwrap();
        let given = KeyFile::read_all(&[escrow_key.clone()]).unwrap();

        // The heir's home owns no key that wrote this tape.
        let homes_publics: Vec<String> = home
            .conn
            .prepare("SELECT public_key FROM encryption_keys")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert!(
            !homes_publics.contains(&tape.operator_public),
            "the home's operator did not write the tape"
        );
        let key_rows_before = count(&home.conn, "SELECT COUNT(*) FROM encryption_keys");
        let keys_before = tree_listing(&home.paths.keys_dir);

        // Positive controls for the scans below: the secret IS in the key
        // file the heir passes, and the scan finds a secret that is planted.
        assert_eq!(
            files_holding(keydir.path(), &tape.escrow_secret),
            ["escrow.key"]
        );
        assert!(
            files_holding(&home.paths.home, &tape.escrow_secret).is_empty(),
            "the escrow secret is in no file of the heir's home before the commands"
        );

        let session = progress::start_session(
            Some(&home.paths.logs_dir),
            "catalog rebuild",
            Display::Off,
            false,
        );

        // `catalog rebuild --from-volume --key escrow.key`
        let report = rebuild_with(&home, &mut tape, &given).expect("the escrow key rebuilds");
        assert!(report.volume_inserted, "{report:?}");
        assert!(report.key_is_escrow, "the key given is the registered escrow");
        assert_eq!(report.units, 1);
        assert_eq!(
            count(&home.conn, "SELECT COUNT(*) FROM units WHERE name = 'keepsake'"),
            1
        );
        assert_eq!(
            home.conn
                .query_row(
                    "SELECT t.name FROM units u JOIN tenants t ON t.id = u.tenant_id
                      WHERE u.name = 'keepsake'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "family",
            "the unit is filed under the tenant that owns it"
        );

        // Without `--key` the same restore fails with the wrong-key refusal
        // as it stands: the home holds keys, and none of them opens the tape.
        let none = TempDir::new().unwrap();
        let err = restore_with(&home, &mut tape, none.path(), &RestoreOptions::default())
            .expect_err("the home's keys open nothing on this tape")
            .to_string();
        assert!(
            err.contains(&age::DecryptError::NoMatchingKeys.to_string()),
            "{err}"
        );
        assert!(!err.contains("--key"), "no --key was given: {err}");

        // `restore unit --unit keepsake --key escrow.key --to DIR`
        let dest = TempDir::new().unwrap();
        let options = RestoreOptions {
            keys: given.clone(),
            ..RestoreOptions::default()
        };
        let restored = restore_with(&home, &mut tape, dest.path(), &options)
            .expect("the escrow key restores through tapectl");
        assert_eq!(restored.unit_name, UNIT);
        assert!(restored.slices >= 3);
        for (path, bytes) in &tape.files {
            assert_eq!(
                &std::fs::read(dest.path().join(path)).unwrap_or_default(),
                bytes,
                "{path} restored wrong"
            );
        }
        drop(session);

        // The key was used and kept nowhere: not in the home's keys/, not
        // registered, not in the catalog, not in the session log.
        assert_eq!(
            tree_listing(&home.paths.keys_dir),
            keys_before,
            "keys/ is untouched: nothing copied in, nothing rewritten"
        );
        assert_eq!(
            count(&home.conn, "SELECT COUNT(*) FROM encryption_keys"),
            key_rows_before,
            "no key was registered by either command"
        );
        assert_eq!(
            count(
                &home.conn,
                "SELECT COUNT(*) FROM encryption_keys WHERE is_escrow = 1"
            ),
            1
        );
        assert_eq!(
            home.conn
                .query_row(
                    "SELECT public_key FROM encryption_keys WHERE is_escrow = 1",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            tape.escrow_public,
            "the registered escrow identity is the one init adopted, unreplaced (ADR-0005)"
        );
        home.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let logs = tree_listing(&home.paths.logs_dir);
        assert!(
            !logs.is_empty(),
            "positive control: the session wrote a log to scan"
        );
        assert_eq!(
            files_holding(&home.paths.home, &tape.escrow_secret),
            Vec::<String>::new(),
            "the escrow secret is in no file of the home — catalog, logs, keys, staging"
        );
    }

    /// `--key` repeated: the identities of every file are tried, in any
    /// order, and a file that opens nothing beside the right one is no
    /// obstacle — for the rebuild as for the restore.
    #[test]
    fn repeated_key_flags_all_count_for_the_rebuild_and_the_restore() {
        let mut tape = written_tape();
        let home = heir_home(&tape.escrow_public);
        let dir = TempDir::new().unwrap();
        let write = |name: &str, secret: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, format!("{secret}\n")).unwrap();
            p
        };
        let wrong = write("wrong.key", &generate_keypair().secret_key);
        let right = write("escrow.key", &tape.escrow_secret);
        let given = KeyFile::read_all(&[wrong, right]).unwrap();

        let report = rebuild_with(&home, &mut tape, &given).expect("wrong first, right second");
        assert!(report.key_is_escrow);
        let dest = TempDir::new().unwrap();
        let options = RestoreOptions {
            keys: given,
            ..RestoreOptions::default()
        };
        restore_with(&home, &mut tape, dest.path(), &options).expect("restore");
        for (path, bytes) in &tape.files {
            assert_eq!(&std::fs::read(dest.path().join(path)).unwrap(), bytes);
        }
    }
}
