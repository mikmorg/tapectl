//! Issue #480 (ADR-0012 items 101 and 102): a damaged slice is read from
//! another copy, a bad age chunk costs only itself, and lost files are filled
//! from another stage set of the same version.
//!
//! Everything here is real except the drives: a real dar archive cut into
//! several slices, real age encryption (one file per slice, a fresh key
//! each), real `Supply`, real contacts and `restores` rows. The cartridges
//! are in-memory tapes behind a [`TestBay`], which plays the operator too: a
//! script of what is loaded when a cartridge is asked for, and a record of
//! what was asked.

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::io::Cursor;

    use tempfile::TempDir;

    use super::super::supply::{Answer, Arrival, Bay, Want};
    use super::super::*;
    use crate::store::{MemStore, TapeStore};
    use crate::tape::contact::Medium;
    use crate::tape::fake::FakeTape;
    use crate::volume::layout;

    /// Tape file number of slice 1, as in the other restore fixtures.
    const FIRST: usize = 4;

    /// What the operator does when a cartridge is asked for.
    #[derive(Debug, Clone, Copy)]
    enum Reply {
        /// Load this cartridge and press Enter.
        Load(&'static str),
        /// Press Enter without changing the cartridge.
        Enter,
        /// "skip".
        Skip,
        /// "stop".
        Stop,
    }

    /// A drive and an operator, scripted.
    struct TestBay {
        carts: BTreeMap<String, Box<dyn Store>>,
        /// What the st driver has open (`None` between a release and a load).
        open: Option<String>,
        /// What is physically in the drive.
        physical: Option<String>,
        attended: bool,
        script: VecDeque<Reply>,
        /// `(label asked for, the reason shown)`, in order.
        asked: Vec<(String, String)>,
        releases: usize,
    }

    impl TestBay {
        fn asked_labels(&self) -> Vec<&str> {
            self.asked.iter().map(|(l, _)| l.as_str()).collect()
        }
    }

    impl Bay for TestBay {
        fn store(&mut self) -> &mut dyn Store {
            let label = self.open.clone().expect("the drive is open");
            self.carts
                .get_mut(&label)
                .expect("a known cartridge")
                .as_mut()
        }

        fn release(&mut self) {
            self.open = None;
            self.releases += 1;
        }

        fn load<'c, 'a>(&mut self, _: &'c Connection, _: &'a Config) -> Result<Arrival<'c, 'a>> {
            match &self.physical {
                Some(label) => {
                    self.open = Some(label.clone());
                    Ok(Arrival::unread())
                }
                None => Err(TapectlError::Other("no cartridge is loaded".into())),
            }
        }

        fn attended(&self) -> bool {
            self.attended
        }

        fn ask(&mut self, want: &Want) -> Answer {
            self.asked
                .push((want.label.clone(), want.lines().join(" | ")));
            match self.script.pop_front().unwrap_or(Reply::Stop) {
                Reply::Load(label) => {
                    self.physical = Some(label.to_string());
                    Answer::Loaded
                }
                Reply::Enter => Answer::Loaded,
                Reply::Skip => Answer::Skip,
                Reply::Stop => Answer::Stop,
            }
        }
    }

    /// A site with no backend: the honest description of a test machine.
    fn site() -> ContactSite<'static> {
        static CFG: std::sync::OnceLock<Config> = std::sync::OnceLock::new();
        ContactSite::new(
            CFG.get_or_init(Config::default),
            Operation::RestoreUnit,
            "/nonexistent/tapectl-480-nst",
            Medium::NoBackend,
        )
    }

    fn sha_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(bytes))
    }

    /// Pseudo-random bytes (incompressible), seeded so each file differs.
    fn noise(len: usize, seed: u32) -> Vec<u8> {
        (0..len as u32)
            .map(|i| (i.wrapping_add(seed).wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect()
    }

    /// A tree of six 100 KB files: several dar slices at `-s 150k`.
    fn tree() -> Vec<(String, Vec<u8>)> {
        (0..6)
            .map(|i| (format!("f{i}.bin"), noise(100_000, i * 7919 + 1)))
            .collect()
    }

    /// One staging: a stage set, its slices (plaintext and ciphertext), and
    /// where its catalogue is.
    struct Staging {
        stage_set_id: i64,
        plains: Vec<Vec<u8>>,
        ciphers: Vec<Vec<u8>>,
    }

    impl Staging {
        fn slices(&self) -> usize {
            self.ciphers.len()
        }

        /// The ciphertext byte offset of a byte inside chunk `chunk` of slice
        /// `slice` (1-based), counted from the start of the file on tape.
        fn byte_in_chunk(&self, slice: usize, chunk: usize) -> usize {
            let plain = self.plains[slice - 1].len();
            let cipher = self.ciphers[slice - 1].len();
            let chunks = plain.div_ceil(crate::crypto::chunks::CHUNK).max(1);
            let header = cipher - plain - 16 * chunks;
            assert!(
                chunk < chunks,
                "slice {slice} has {chunks} chunk(s), not {chunk}"
            );
            header + chunk * crate::crypto::chunks::ENCRYPTED_CHUNK + 10
        }

        fn chunks_of(&self, slice: usize) -> usize {
            self.plains[slice - 1]
                .len()
                .div_ceil(crate::crypto::chunks::CHUNK)
                .max(1)
        }
    }

    struct Fixture {
        conn: Connection,
        paths: TapectlPaths,
        _home: TempDir,
        unit: String,
        snapshot_id: i64,
        public_key: String,
        files: Vec<(String, Vec<u8>)>,
        mems: BTreeMap<String, MemStore>,
        volume_ids: BTreeMap<String, i64>,
        /// Where each cartridge holds each stage set's slice 1.
        first_position: BTreeMap<(String, i64), usize>,
    }

    impl Fixture {
        /// Cartridges `labels`, each empty but for its File 0, and unit
        /// `unit-480` with `files` as version 1.
        fn new(labels: &[&str], files: Vec<(String, Vec<u8>)>) -> Fixture {
            let conn = crate::db::open_memory().unwrap();
            let home = TempDir::new().unwrap();
            let paths = TapectlPaths::new(home.path().join(".tapectl"));
            paths.ensure_dirs().unwrap();
            let kp = keys::generate_and_save(&paths.keys_dir, "t1", "primary").unwrap();
            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('t1', 0, 'active')",
                [],
            )
            .unwrap();
            let tenant_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                 VALUES ('unit-480-uuid', 'unit-480', ?1, 'mtime_size', 1, 'active')",
                params![tenant_id],
            )
            .unwrap();
            let unit_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
                 VALUES (?1, 1, 'staged', '/tmp', 1, 16)",
                params![unit_id],
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
            let mut mems = BTreeMap::new();
            let mut volume_ids = BTreeMap::new();
            for label in labels {
                conn.execute(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                          capacity_bytes, status)
                     VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
                    params![label],
                )
                .unwrap();
                volume_ids.insert(label.to_string(), conn.last_insert_rowid());
                let thunk = layout::generate_id_thunk_v2(&layout::IdThunkV2Params {
                    label,
                    uuid: &format!("aaaaaaaa-bbbb-cccc-dddd-{:012}", mems.len()),
                    media_type: "LTO-6",
                    tapectl_version: "0.0.0-test",
                    nominal_capacity: 2_500_000_000_000,
                    mam_capacity: 2_400_000_000_000,
                    total_files: 6,
                    mam_manufacturer: "TESTCO",
                    mam_serial: "",
                    mam_length: 846,
                    mam_loads: 1,
                    created_at: "2026-10-17T00:00:00Z",
                    cartridge_identity_source: None,
                })
                .into_bytes();
                let mut mem = MemStore::new(4096);
                mem.execute(&mut Cursor::new(thunk.clone()), thunk.len() as u64, false)
                    .unwrap();
                for filler in 1..FIRST {
                    mem.execute(&mut Cursor::new(vec![filler as u8; 100]), 100, false)
                        .unwrap();
                }
                mems.insert(label.to_string(), mem);
            }
            Fixture {
                conn,
                paths,
                _home: home,
                unit: "unit-480".to_string(),
                snapshot_id,
                public_key: kp.public_key,
                files,
                mems,
                volume_ids,
                first_position: BTreeMap::new(),
            }
        }

        /// One staging of `files` (a real dar run, slices of `slice_size`, each
        /// encrypted to the tenant), as `stage create` records it.
        fn stage(&self, files: &[(String, Vec<u8>)], slice_size: &str, catalogue: bool) -> Staging {
            self.stage_in(self.snapshot_id, files, slice_size, catalogue)
        }

        /// [`Self::stage`] for another snapshot (another version) of the unit.
        fn stage_in(
            &self,
            snapshot_id: i64,
            files: &[(String, Vec<u8>)],
            slice_size: &str,
            catalogue: bool,
        ) -> Staging {
            self.conn
                .execute(
                    "INSERT INTO stage_sets (snapshot_id, status, slice_size)
                     VALUES (?1, 'staged', 524288)",
                    params![snapshot_id],
                )
                .unwrap();
            let stage_set_id = self.conn.last_insert_rowid();
            let work = TempDir::new().unwrap();
            let src = work.path().join("src");
            fs::create_dir_all(&src).unwrap();
            for (path, bytes) in files {
                fs::write(src.join(path), bytes).unwrap();
            }
            let base = work.path().join("arch");
            let created = std::process::Command::new("dar")
                .arg("-c")
                .arg(&base)
                .arg("-R")
                .arg(&src)
                .arg("-s")
                .arg(slice_size)
                .arg("-Q")
                .output()
                .unwrap();
            assert!(
                created.status.success(),
                "dar -c failed in test setup: {}",
                String::from_utf8_lossy(&created.stderr)
            );
            if catalogue {
                let dir = self.paths.catalogs_dir.join(format!("ss{stage_set_id}"));
                fs::create_dir_all(&dir).unwrap();
                let catalogue = dir.join(format!("ss{stage_set_id}_v1"));
                crate::dar::create::extract_catalog("dar", &base, &catalogue).unwrap();
                self.conn
                    .execute(
                        "UPDATE stage_sets SET catalog_path = ?1 WHERE id = ?2",
                        params![catalogue.to_string_lossy(), stage_set_id],
                    )
                    .unwrap();
            }
            let plains: Vec<Vec<u8>> = (1..)
                .map(|n| work.path().join(format!("arch.{n}.dar")))
                .take_while(|p| p.exists())
                .map(|p| fs::read(p).unwrap())
                .collect();
            let ciphers: Vec<Vec<u8>> = plains
                .iter()
                .map(|p| {
                    crate::staging::encrypt_data(p, std::slice::from_ref(&self.public_key)).unwrap()
                })
                .collect();
            for (i, (plain, cipher)) in plains.iter().zip(&ciphers).enumerate() {
                self.conn
                    .execute(
                        "INSERT INTO stage_slices
                            (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                             sha256_plain, sha256_encrypted)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![
                            stage_set_id,
                            i as i64 + 1,
                            plain.len() as i64,
                            cipher.len() as i64,
                            sha_hex(plain),
                            sha_hex(cipher)
                        ],
                    )
                    .unwrap();
            }
            Staging {
                stage_set_id,
                plains,
                ciphers,
            }
        }

        /// Write the staging's slices to cartridge `label` and record the copy:
        /// a completed write with a position per slice.
        fn write(&mut self, st: &Staging, label: &str) {
            self.write_in(self.snapshot_id, st, label);
        }

        fn write_in(&mut self, snapshot_id: i64, st: &Staging, label: &str) {
            let volume_id = self.volume_ids[label];
            self.conn
                .execute(
                    "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                     VALUES (?1, ?2, ?3, 'completed')",
                    params![st.stage_set_id, snapshot_id, volume_id],
                )
                .unwrap();
            let write_id = self.conn.last_insert_rowid();
            let mem = self.mems.get_mut(label).unwrap();
            self.first_position
                .insert((label.to_string(), st.stage_set_id), mem.files.len());
            for (i, cipher) in st.ciphers.iter().enumerate() {
                let position = mem.files.len();
                mem.execute(&mut Cursor::new(cipher.clone()), cipher.len() as u64, false)
                    .unwrap();
                let slice_id: i64 = self
                    .conn
                    .query_row(
                        "SELECT id FROM stage_slices WHERE stage_set_id = ?1 AND slice_number = ?2",
                        params![st.stage_set_id, i as i64 + 1],
                        |r| r.get(0),
                    )
                    .unwrap();
                self.conn
                    .execute(
                        "INSERT INTO write_positions
                            (write_id, stage_slice_id, position, status, sha256_on_volume)
                         VALUES (?1, ?2, ?3, 'written', ?4)",
                        params![write_id, slice_id, position.to_string(), sha_hex(cipher)],
                    )
                    .unwrap();
            }
        }

        /// Bind volume `label` to a cartridge with this sticker and chip serial,
        /// as `volume init` does.
        fn bind(&self, label: &str, barcode: &str, serial: &str) {
            self.conn
                .execute(
                    "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number, status)
                     VALUES (?1, 'LTO-6', 2500000000000, ?2, 'in_use')",
                    params![barcode, serial],
                )
                .unwrap();
            let cartridge_id = self.conn.last_insert_rowid();
            self.conn
                .execute(
                    "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
                    params![cartridge_id, self.volume_ids[label]],
                )
                .unwrap();
        }

        /// Version `version` of the unit, with the same file list (checksums
        /// and all) — a different Version of the same unit.
        fn new_version(&self, version: i64) -> i64 {
            let unit_id: i64 = self
                .conn
                .query_row(
                    "SELECT id FROM units WHERE name = ?1",
                    params![self.unit],
                    |r| r.get(0),
                )
                .unwrap();
            self.conn
                .execute(
                    "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
                     VALUES (?1, ?2, 'staged', '/tmp', 1, 16)",
                    params![unit_id, version],
                )
                .unwrap();
            let snapshot_id = self.conn.last_insert_rowid();
            for (path, bytes) in &self.files {
                crate::db::files::fixture::insert(
                    &self.conn,
                    snapshot_id,
                    path,
                    bytes.len() as i64,
                    "regular",
                    Some(&sha_hex(bytes)),
                );
            }
            snapshot_id
        }

        /// Flip one byte inside chunk `chunk` of slice `slice` as cartridge
        /// `label` holds it.
        fn corrupt(&mut self, label: &str, st: &Staging, slice: usize, chunk: usize) {
            let position = self.first_position[&(label.to_string(), st.stage_set_id)] + slice - 1;
            let at = st.byte_in_chunk(slice, chunk);
            self.mems.get_mut(label).unwrap().files[position][at] ^= 0xFF;
        }

        /// Replace slice `slice` as cartridge `label` holds it with `plain`
        /// encrypted afresh to the tenant: a DIFFERENT age file (its own key,
        /// header and nonce) of exactly the length the catalog recorded, with
        /// the block padding the original had. Every chunk of it authenticates
        /// under the tenant's identity; its ciphertext hash is not the
        /// recorded one.
        fn replace_slice(&mut self, label: &str, st: &Staging, slice: usize, plain: &[u8]) {
            assert_eq!(
                plain.len(),
                st.plains[slice - 1].len(),
                "same plaintext length"
            );
            let position = self.first_position[&(label.to_string(), st.stage_set_id)] + slice - 1;
            let want = st.ciphers[slice - 1].len();
            let mut tries = 0;
            let mut cipher = loop {
                tries += 1;
                assert!(tries < 5000, "no age file of {want} bytes in {tries} tries");
                let c = crate::staging::encrypt_data(plain, std::slice::from_ref(&self.public_key))
                    .unwrap();
                if c.len() == want {
                    break c;
                }
            };
            let stored = self.mems[label].files[position].len();
            cipher.resize(stored, 0);
            self.mems.get_mut(label).unwrap().files[position] = cipher;
        }

        /// Forget the plaintext checksum recorded for slice `slice` of `st`,
        /// as a catalog that never had one.
        fn forget_plain_sha(&self, st: &Staging, slice: usize) {
            self.conn
                .execute(
                    "UPDATE stage_slices SET sha256_plain = '' WHERE stage_set_id = ?1 AND slice_number = ?2",
                    params![st.stage_set_id, slice as i64],
                )
                .unwrap();
        }

        /// The bay, with `start` in the drive: `fake` puts each cartridge on a
        /// [`FakeTape`] (for the motions), otherwise memory.
        fn bay(&self, start: &str, attended: bool, script: &[Reply]) -> TestBay {
            let carts = self
                .mems
                .iter()
                .map(|(label, mem)| (label.clone(), Box::new(clone_mem(mem)) as Box<dyn Store>))
                .collect();
            TestBay {
                carts,
                open: Some(start.to_string()),
                physical: Some(start.to_string()),
                attended,
                script: script.iter().copied().collect(),
                asked: Vec::new(),
                releases: 0,
            }
        }

        /// Like [`Self::bay`], each cartridge on its own [`FakeTape`], handed
        /// back by label for its operation record.
        fn fake_bay(&self, start: &str, script: &[Reply]) -> (TestBay, BTreeMap<String, FakeTape>) {
            let mut fakes = BTreeMap::new();
            let carts = self
                .mems
                .iter()
                .map(|(label, mem)| {
                    let fake = FakeTape::with_files(mem.files.clone(), 4096);
                    let store = TapeStore::from_ops(fake.boxed(), 0).unwrap();
                    fakes.insert(label.clone(), fake);
                    (label.clone(), Box::new(store) as Box<dyn Store>)
                })
                .collect();
            (
                TestBay {
                    carts,
                    open: Some(start.to_string()),
                    physical: Some(start.to_string()),
                    attended: true,
                    script: script.iter().copied().collect(),
                    asked: Vec::new(),
                    releases: 0,
                },
                fakes,
            )
        }

        /// `restore unit` from `label`, version 1, through `bay`.
        fn restore(
            &self,
            label: &str,
            bay: &mut TestBay,
            dest: &Path,
            options: &RestoreOptions,
        ) -> Result<RestoreReport> {
            let dest = dest.to_string_lossy().into_owned();
            restore_unit_from_bay(
                &self.conn,
                &self.paths,
                &Config::default(),
                &self.unit,
                label,
                1,
                RestoreTarget::Unit { dest_dir: &dest },
                options,
                bay,
                site(),
            )
        }

        fn assert_restored(&self, dest: &Path) {
            for (path, bytes) in &self.files {
                assert_eq!(
                    &fs::read(dest.join(path)).unwrap_or_default(),
                    bytes,
                    "{path} restored wrong"
                );
            }
        }

        /// Paths in `dest` whose bytes are not the original's.
        fn differing(&self, dest: &Path) -> Vec<String> {
            self.files
                .iter()
                .filter(|(path, bytes)| fs::read(dest.join(path)).ok().as_ref() != Some(bytes))
                .map(|(path, _)| path.clone())
                .collect()
        }

        /// `(volume label, outcome, detail)` of every contact, in order.
        fn contacts(&self) -> Vec<(String, String, Option<String>)> {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT COALESCE(v.label, '-'), COALESCE(c.outcome, '-'), c.detail
                       FROM cartridge_contacts c LEFT JOIN volumes v ON v.id = c.volume_id
                      ORDER BY c.id",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap()
        }
    }

    /// A copy of an in-memory tape, file for file.
    fn clone_mem(mem: &MemStore) -> MemStore {
        let mut copy = MemStore::new(4096);
        for file in &mem.files {
            copy.execute(&mut Cursor::new(file.clone()), file.len() as u64, false)
                .unwrap();
        }
        copy
    }

    const A: &str = "L6-0001";
    const B: &str = "L6-0002";
    const C: &str = "L6-0003";

    /// Two copies of one staging, a catalogue or not (stream or spool).
    fn two_copies(catalogue: bool) -> (Fixture, Staging) {
        let mut fx = Fixture::new(&[A, B], tree());
        let st = fx.stage(&tree(), "150k", catalogue);
        assert!(
            st.slices() >= 4,
            "want a many-slice archive: {}",
            st.slices()
        );
        assert!(st.chunks_of(2) >= 2, "want a slice of several chunks");
        fx.write(&st, A);
        fx.write(&st, B);
        (fx, st)
    }

    /// THE acceptance test of the slice half: two copies, a different slice bad
    /// on each, `restore unit` comes back whole with ONE cartridge swap — the
    /// operator is asked once, for the copy that holds the slice the first
    /// could not give, and that cartridge then serves the rest. Both ways dar
    /// can be fed.
    #[test]
    fn two_copies_with_different_bad_slices_restore_whole_with_one_swap() {
        for catalogue in [true, false] {
            let (mut fx, st) = two_copies(catalogue);
            // A is bad at slice 3; B at slice 2, which A gives whole, so B's
            // damage is never reached.
            fx.corrupt(A, &st, 3, 0);
            fx.corrupt(B, &st, 2, 0);
            let mut bay = fx.bay(A, true, &[Reply::Load(B)]);
            let dest = TempDir::new().unwrap();
            let report = fx
                .restore(A, &mut bay, dest.path(), &RestoreOptions::default())
                .unwrap_or_else(|e| panic!("catalogue={catalogue}: {e}"));
            fx.assert_restored(dest.path());

            assert_eq!(
                bay.asked_labels(),
                [B],
                "catalogue={catalogue}: one swap prompt"
            );
            assert!(
                bay.asked[0].1.contains("Slice 3") && bay.asked[0].1.contains("checksum mismatch"),
                "the prompt names the slice and why: {:?}",
                bay.asked
            );
            assert_eq!(report.substitutions.len(), 1);
            let sub = &report.substitutions[0];
            assert_eq!(
                (
                    sub.slice_number,
                    sub.supplied_by.as_str(),
                    sub.damaged_on.as_str()
                ),
                (3, B, A)
            );
            assert!(report.filled.is_empty());

            // Two cartridges were in the drive, so two contacts, each closed.
            let contacts = fx.contacts();
            assert_eq!(contacts.len(), 2, "{contacts:?}");
            assert_eq!((contacts[0].0.as_str(), contacts[0].1.as_str()), (A, "ok"));
            assert_eq!((contacts[1].0.as_str(), contacts[1].1.as_str()), (B, "ok"));
            assert!(
                contacts[0]
                    .2
                    .as_deref()
                    .unwrap_or("")
                    .contains("slice 3 damaged here"),
                "{contacts:?}"
            );
            assert!(
                contacts[1]
                    .2
                    .as_deref()
                    .unwrap_or("")
                    .contains("supplied slice 3"),
                "{contacts:?}"
            );
            // And the restore is one record, under the first contact.
            let rows = crate::volume::restore_record::rows(&fx.conn).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].outcome, "ok");
            assert_eq!(rows[0].volume_label.as_deref(), Some(A));
            assert_eq!(rows[0].slices_read, Some(st.slices() as i64));
        }
    }

    /// And the rule that keeps swaps few: A is bad at slice 2 and B at slice 4,
    /// so the cartridge asked for at slice 2 (B) is bad at 4 and A is asked for
    /// again — two swaps for two bad slices, never one per slice read.
    #[test]
    fn each_bad_slice_run_costs_one_swap_and_a_cartridge_is_kept_while_it_serves() {
        let (mut fx, st) = two_copies(true);
        fx.corrupt(A, &st, 2, 1);
        fx.corrupt(B, &st, 4, 0);
        let mut bay = fx.bay(A, true, &[Reply::Load(B), Reply::Load(A)]);
        let dest = TempDir::new().unwrap();
        let report = fx
            .restore(A, &mut bay, dest.path(), &RestoreOptions::default())
            .unwrap();
        fx.assert_restored(dest.path());
        assert_eq!(bay.asked_labels(), [B, A]);
        assert_eq!(
            report
                .substitutions
                .iter()
                .map(|s| (s.slice_number, s.supplied_by.as_str()))
                .collect::<Vec<_>>(),
            vec![(2, B), (4, A)]
        );
        // Slices 2 and 3 came from B though only 2 was bad on A: B stayed in
        // the drive while it served, and A's slice 3 was never asked for.
        assert_eq!(fx.contacts().len(), 3);

        // Consecutive bad slices on the first cartridge are ONE swap.
        let (mut fx, st) = two_copies(true);
        fx.corrupt(A, &st, 2, 0);
        fx.corrupt(A, &st, 3, 0);
        fx.corrupt(A, &st, 4, 0);
        let mut bay = fx.bay(A, true, &[Reply::Load(B)]);
        let dest = TempDir::new().unwrap();
        fx.restore(A, &mut bay, dest.path(), &RestoreOptions::default())
            .unwrap();
        fx.assert_restored(dest.path());
        assert_eq!(
            bay.asked_labels(),
            [B],
            "three bad slices in a row, one swap"
        );
    }

    /// A slice damaged in a DIFFERENT chunk on each copy is whole from the two
    /// together: A gives chunks up to its bad one, B carries on and gives
    /// everything A could not, then A is asked for again for what lies past B's
    /// bad chunk, because the bytes already delivered are the same bytes on every
    /// copy.
    #[test]
    fn a_slice_bad_in_different_chunks_on_each_copy_is_whole_from_both() {
        for catalogue in [true, false] {
            let (mut fx, st) = two_copies(catalogue);
            assert!(st.chunks_of(2) >= 3, "{}", st.chunks_of(2));
            fx.corrupt(A, &st, 2, 0);
            fx.corrupt(B, &st, 2, 2);
            let mut bay = fx.bay(A, true, &[Reply::Load(B), Reply::Load(A)]);
            let dest = TempDir::new().unwrap();
            fx.restore(A, &mut bay, dest.path(), &RestoreOptions::default())
                .unwrap_or_else(|e| panic!("catalogue={catalogue}: {e}"));
            fx.assert_restored(dest.path());
            assert_eq!(bay.asked_labels(), [B, A], "catalogue={catalogue}");
        }
    }

    /// No terminal, no swap: the read refuses by name, listing the volumes that
    /// hold the slice, and asks nothing. What was restored before stays.
    #[test]
    fn without_a_terminal_the_restore_refuses_by_name_and_lists_the_holders() {
        let (mut fx, st) = two_copies(false);
        fx.corrupt(A, &st, 3, 0);
        let mut bay = fx.bay(A, false, &[Reply::Load(B)]);
        let dest = TempDir::new().unwrap();
        let err = fx
            .restore(A, &mut bay, dest.path(), &RestoreOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("slice 3"), "{err}");
        assert!(err.contains("\"unit-480\""), "{err}");
        assert!(err.contains(B), "the holder is named: {err}");
        assert!(err.contains("swaps no cartridge"), "{err}");
        assert!(err.contains("--from L6-0002"), "{err}");
        assert!(
            err.contains("checksum mismatch"),
            "the cause is named: {err}"
        );
        assert!(bay.asked.is_empty(), "nobody was asked");
        assert_eq!(bay.releases, 0, "the drive was left alone");
        assert!(
            !dest.path().join(SCRATCH_NAME).exists(),
            "no decrypted slice is left behind"
        );
        let rows = crate::volume::restore_record::rows(&fx.conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, "failed");
        assert!(rows[0]
            .error
            .as_deref()
            .unwrap()
            .contains("swaps no cartridge"));
    }

    /// The operator declines the swap: the slice is read from the cartridge in
    /// the drive with a hole where its chunk fails to authenticate. The restore
    /// names exactly the files that came back damaged and fails; every other
    /// file is whole, and the hole is zeros, not the unauthenticated plaintext.
    #[test]
    fn a_declined_swap_leaves_a_hole_and_the_restore_names_the_damaged_files() {
        for catalogue in [true, false] {
            let (mut fx, st) = two_copies(catalogue);
            fx.corrupt(A, &st, 3, 0);
            let mut bay = fx.bay(A, true, &[Reply::Skip]);
            let dest = TempDir::new().unwrap();
            let err = fx
                .restore(A, &mut bay, dest.path(), &RestoreOptions::default())
                .expect_err(&format!("catalogue={catalogue}"))
                .to_string();
            assert!(err.contains("INCOMPLETE"), "{err}");
            assert!(err.contains("slice 3 on L6-0001"), "{err}");
            assert!(
                err.contains("unproven"),
                "a slice with holes is said to be unproven as well: {err}"
            );
            let differing = fx.differing(dest.path());
            assert!(
                !differing.is_empty() && differing.len() < fx.files.len(),
                "a chunk costs some files, not all: {differing:?}"
            );
            for path in &differing {
                assert!(err.contains(path.as_str()), "{path} is named: {err}");
            }
            for (path, _) in &fx.files {
                if !differing.contains(path) {
                    assert!(
                        !err.contains(&format!("    {path} (")),
                        "{path} came back whole yet is named lost: {err}"
                    );
                }
            }
            // The one swap was offered, and the cartridge never left.
            assert_eq!(bay.asked_labels(), [B]);
            let contacts = fx.contacts();
            assert_eq!(
                contacts.last().unwrap().0,
                A,
                "the first cartridge came back: {contacts:?}"
            );
        }
    }

    /// A slice bad in the SAME chunk on every copy cannot be had whole: after
    /// every copy has been tried, the chunk is a hole, the files it fell in are
    /// named, and the restore fails.
    #[test]
    fn a_chunk_bad_on_every_copy_is_a_hole_after_every_copy_was_tried() {
        let (mut fx, st) = two_copies(true);
        fx.corrupt(A, &st, 3, 1);
        fx.corrupt(B, &st, 3, 1);
        let mut bay = fx.bay(A, true, &[Reply::Load(B)]);
        let dest = TempDir::new().unwrap();
        let err = fx
            .restore(A, &mut bay, dest.path(), &RestoreOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("INCOMPLETE"), "{err}");
        assert!(err.contains("slice 3"), "{err}");
        assert_eq!(
            bay.asked_labels(),
            [B],
            "the only other copy was tried once"
        );
        let differing = fx.differing(dest.path());
        assert!(!differing.is_empty());
        for path in &differing {
            assert!(err.contains(path.as_str()), "{path}: {err}");
        }
    }

    /// The acceptance test of the chunk half, with real dar and real age and no
    /// other copy: one corrupted byte inside a slice costs only the files in its
    /// 64 KiB chunk. The restore names them, exits non-zero, and nothing
    /// unauthenticated is written — the files that are not named are byte for
    /// byte whole, and the damaged ones carry zeros where the chunk was.
    #[test]
    fn a_single_corrupted_byte_costs_only_the_files_in_its_chunk() {
        for catalogue in [true, false] {
            let mut fx = Fixture::new(&[A], tree());
            let st = fx.stage(&tree(), "150k", catalogue);
            fx.write(&st, A);
            fx.corrupt(A, &st, 2, 1);
            let mem = clone_mem(&fx.mems[A]);
            let mut mem = mem;
            let dest = TempDir::new().unwrap();
            let dest_str = dest.path().to_string_lossy().into_owned();
            let err = restore_unit_from_store(
                &fx.conn,
                &fx.paths,
                &Config::default(),
                &fx.unit,
                A,
                1,
                RestoreTarget::Unit {
                    dest_dir: &dest_str,
                },
                &RestoreOptions::default(),
                &mut mem,
                site(),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("INCOMPLETE"), "{err}");
            let differing = fx.differing(dest.path());
            assert!(
                !differing.is_empty() && differing.len() <= 2,
                "one 64 KiB chunk of 100 KB files costs one or two files: {differing:?}"
            );
            for path in &differing {
                assert!(err.contains(path.as_str()), "{path} is named: {err}");
                // A hole is zeros: some 4 KiB run of the damaged file is all zero.
                let bytes = fs::read(dest.path().join(path)).unwrap();
                assert!(
                    bytes.windows(4096).any(|w| w.iter().all(|&b| b == 0)),
                    "{path} carries no hole of zeros"
                );
            }
            for (path, bytes) in &fx.files {
                if !differing.contains(path) {
                    assert_eq!(&fs::read(dest.path().join(path)).unwrap(), bytes, "{path}");
                }
            }
            let rows = crate::volume::restore_record::rows(&fx.conn).unwrap();
            assert_eq!(rows[0].outcome, "failed");
            assert!(rows[0].error.as_deref().unwrap().contains("INCOMPLETE"));
            assert!(
                !dest.path().join(SCRATCH_NAME).exists(),
                "the scratch directory is gone"
            );
        }
    }

    // ── what proves a slice whole (the second review's finding) ──────────
    //
    // An age chunk's tag proves it was encrypted under some key the tenant's
    // identity unwraps; any other age file to that tenant authenticates in
    // full. These tests put such a file where the catalog expects a slice.

    /// Bytes unlike anything else in the fixtures (`noise` is one stream
    /// read from different offsets, so its seeds overlap): a hash chain.
    fn unlike(len: usize, tag: u64) -> Vec<u8> {
        use sha2::Digest;
        let mut out = Vec::with_capacity(len + 32);
        let mut n = 0u64;
        while out.len() < len {
            let mut h = sha2::Sha256::new();
            h.update(tag.to_le_bytes());
            h.update(n.to_le_bytes());
            out.extend_from_slice(&h.finalize());
            n += 1;
        }
        out.truncate(len);
        out
    }

    /// A wrong plaintext of slice 3's length: not a dar slice of this archive
    /// (dar may give up on it as soon as it reads it).
    fn wrong_plain(st: &Staging) -> Vec<u8> {
        unlike(st.plains[2].len(), 4242)
    }

    /// Slice 3's own plaintext with one byte flipped well inside it: a
    /// different file, but one dar reads to the end (it finds the damage by a
    /// file's CRC, not by giving up).
    fn nearly_plain(st: &Staging) -> Vec<u8> {
        let mut plain = st.plains[2].clone();
        let at = plain.len() / 2;
        plain[at] ^= 0x55;
        plain
    }

    /// THE finding, spooled: a different age file at the right position, of
    /// the right length, authenticates in full and does not hash. It is not
    /// the slice: it is dropped, the slice is read from the other copy, and
    /// the restore is whole with the substitution said.
    #[test]
    fn a_wrong_age_file_at_the_right_position_is_taken_from_another_copy_when_spooled() {
        let (mut fx, st) = two_copies(false);
        let wrong = wrong_plain(&st);
        fx.replace_slice(A, &st, 3, &wrong);
        let mut bay = fx.bay(A, true, &[Reply::Load(B)]);
        let dest = TempDir::new().unwrap();
        let report = fx
            .restore(A, &mut bay, dest.path(), &RestoreOptions::default())
            .unwrap_or_else(|e| panic!("{e}"));
        fx.assert_restored(dest.path());
        assert_eq!(bay.asked_labels(), [B]);
        assert!(
            bay.asked[0].1.contains("Slice 3") && bay.asked[0].1.contains("checksum mismatch"),
            "{:?}",
            bay.asked
        );
        assert_eq!(report.substitutions.len(), 1, "{report:?}");
        let sub = &report.substitutions[0];
        assert_eq!(
            (
                sub.slice_number,
                sub.supplied_by.as_str(),
                sub.damaged_on.as_str()
            ),
            (3, B, A)
        );
        assert!(report.filled.is_empty());
    }

    /// The same, streamed: the wrong file's bytes went down the pipe to dar
    /// before anything could say so, and a stream cannot take bytes back. The
    /// good copy is NOT mixed in over them (it is another age file), and the
    /// restore fails by name rather than hand back the wrong slice as if it
    /// were the right one.
    #[test]
    fn a_wrong_age_file_streamed_to_dar_is_refused_by_name_never_taken_as_the_slice() {
        let (mut fx, st) = two_copies(true);
        fx.replace_slice(A, &st, 3, &nearly_plain(&st));
        let mut bay = fx.bay(A, true, &[Reply::Load(B)]);
        let dest = TempDir::new().unwrap();
        let err = fx
            .restore(A, &mut bay, dest.path(), &RestoreOptions::default())
            .expect_err("a wrong file must not come back as the slice")
            .to_string();
        assert!(err.contains("slice 3"), "{err}");
        assert!(err.contains("different age file"), "{err}");
        assert!(
            err.contains("matches the catalog"),
            "the good copy is said to be the slice: {err}"
        );
        assert!(err.contains("--spool"), "the way out is named: {err}");
        assert_eq!(bay.asked_labels(), [B]);
        let rows = crate::volume::restore_record::rows(&fx.conn).unwrap();
        assert_eq!(rows[0].outcome, "failed");
    }

    /// A wrong file dar gives up on as soon as it reads it (noise where a dar
    /// slice should be) is refused all the same: either dar swallows the lot
    /// and the next copy is refused as another age file, or dar closes the
    /// pipe and the slice goes on the books as unproven. Never a restore that
    /// reports success.
    #[test]
    fn a_wrong_age_file_dar_gives_up_on_is_never_a_successful_restore() {
        let (mut fx, st) = two_copies(true);
        fx.replace_slice(A, &st, 3, &wrong_plain(&st));
        let mut bay = fx.bay(A, true, &[Reply::Load(B)]);
        let dest = TempDir::new().unwrap();
        let err = fx
            .restore(A, &mut bay, dest.path(), &RestoreOptions::default())
            .expect_err("a wrong file must not come back as the slice")
            .to_string();
        assert!(err.contains("slice 3"), "{err}");
        assert!(
            err.contains("different age file") || err.contains("unproven"),
            "{err}"
        );
    }

    /// The spooled counterpart of the refusal above, where the wrong file
    /// cannot be told by its plaintext (no checksum recorded): it is
    /// unproven, a copy that hashes as the catalog recorded is the slice, and
    /// the part spooled from the first is dropped for it.
    #[test]
    fn an_unproven_spooled_slice_gives_way_to_a_copy_that_hashes_as_recorded() {
        let (mut fx, st) = two_copies(false);
        fx.forget_plain_sha(&st, 3);
        fx.replace_slice(A, &st, 3, &nearly_plain(&st));
        let mut bay = fx.bay(A, true, &[Reply::Load(B)]);
        let dest = TempDir::new().unwrap();
        let report = fx
            .restore(A, &mut bay, dest.path(), &RestoreOptions::default())
            .unwrap_or_else(|e| panic!("{e}"));
        fx.assert_restored(dest.path());
        assert_eq!(bay.asked_labels(), [B]);
        assert_eq!(
            report
                .substitutions
                .iter()
                .map(|s| (s.slice_number, s.supplied_by.as_str()))
                .collect::<Vec<_>>(),
            vec![(3, B)]
        );
    }

    /// The mirror of the acceptance test for slices bad in different chunks
    /// (A bad late, B bad early): spooled, the two together are whole and
    /// proven by the slice's plaintext checksum, and the operator is asked for
    /// B once. Streamed, the bytes A gave went to dar before anything was
    /// hashing them, so the slice cannot be proven — it is restored (every file
    /// in it checks out), said to be unproven, and the operator is not sent
    /// back to a cartridge that could not prove it anyway.
    #[test]
    fn a_slice_bad_late_on_the_first_copy_and_early_on_the_second_is_proven_only_when_spooled() {
        for catalogue in [false, true] {
            let (mut fx, st) = two_copies(catalogue);
            assert!(st.chunks_of(2) >= 3, "{}", st.chunks_of(2));
            fx.corrupt(A, &st, 2, 2);
            fx.corrupt(B, &st, 2, 0);
            let mut bay = fx.bay(A, true, &[Reply::Load(B), Reply::Load(A)]);
            let dest = TempDir::new().unwrap();
            fx.restore(A, &mut bay, dest.path(), &RestoreOptions::default())
                .unwrap_or_else(|e| panic!("catalogue={catalogue}: {e}"));
            fx.assert_restored(dest.path());
            assert_eq!(bay.asked_labels(), [B], "catalogue={catalogue}");
            let said_unproven = fx
                .contacts()
                .iter()
                .any(|c| c.2.as_deref().unwrap_or("").contains("slice 2 unproven"));
            assert_eq!(said_unproven, catalogue, "catalogue={catalogue}");
        }
    }

    /// Spooled, with no other copy: the wrong file is no use to dar, so the
    /// slice is a hole from end to end, the restore says why in the words of
    /// the finding, and the files that were not in that slice are whole.
    #[test]
    fn a_wrong_age_file_on_the_only_copy_is_a_whole_slice_of_hole_when_spooled() {
        let mut fx = Fixture::new(&[A], tree());
        let st = fx.stage(&tree(), "150k", false);
        fx.write(&st, A);
        let wrong = wrong_plain(&st);
        fx.replace_slice(A, &st, 3, &wrong);
        let mut mem = clone_mem(&fx.mems[A]);
        let dest = TempDir::new().unwrap();
        let dest_str = dest.path().to_string_lossy().into_owned();
        let err = restore_unit_from_store(
            &fx.conn,
            &fx.paths,
            &Config::default(),
            &fx.unit,
            A,
            1,
            RestoreTarget::Unit {
                dest_dir: &dest_str,
            },
            &RestoreOptions::default(),
            &mut mem,
            site(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("INCOMPLETE"), "{err}");
        assert!(err.contains("slice 3 on L6-0001"), "{err}");
        assert!(
            err.contains("does not match the checksum recorded for the slice"),
            "{err}"
        );
        assert!(err.contains("unproven"), "{err}");
        let differing = fx.differing(dest.path());
        assert!(
            !differing.is_empty(),
            "the wrong slice restored nothing true"
        );
        for path in &differing {
            assert!(err.contains(path.as_str()), "{path} is named: {err}");
        }
        // Not a byte of the wrong file is anywhere in what came back: sample
        // 64-byte runs of it and look for each.
        let samples: Vec<&[u8]> = (0..8)
            .map(|k| &wrong[k * (wrong.len() / 9)..][..64])
            .collect();
        for (path, _) in &fx.files {
            let got = fs::read(dest.path().join(path)).unwrap_or_default();
            for sample in &samples {
                assert!(
                    !got.windows(64).any(|w| w == *sample),
                    "{path} carries bytes of the wrong file"
                );
            }
        }
    }

    /// Streamed, with no other copy: nothing was hashing the plaintext as it
    /// went to dar, so the slice is unproven — not "whole" — and the restore
    /// says so and judges the files in it one by one.
    #[test]
    fn a_wrong_age_file_streamed_from_the_only_copy_is_unproven_and_the_files_decide() {
        let mut fx = Fixture::new(&[A], tree());
        let st = fx.stage(&tree(), "150k", true);
        fx.write(&st, A);
        let wrong = wrong_plain(&st);
        fx.replace_slice(A, &st, 3, &wrong);
        let mut mem = clone_mem(&fx.mems[A]);
        let dest = TempDir::new().unwrap();
        let dest_str = dest.path().to_string_lossy().into_owned();
        let err = restore_unit_from_store(
            &fx.conn,
            &fx.paths,
            &Config::default(),
            &fx.unit,
            A,
            1,
            RestoreTarget::Unit {
                dest_dir: &dest_str,
            },
            &RestoreOptions::default(),
            &mut mem,
            site(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("slice 3 on L6-0001"), "{err}");
        assert!(err.contains("unproven"), "{err}");
        let differing = fx.differing(dest.path());
        assert!(!differing.is_empty());
        for path in &differing {
            assert!(
                err.contains(path.as_str()) || err.contains("dar stopped reading"),
                "{path} is named, or dar's giving up is: {err}"
            );
        }
    }

    /// The other side of the same check, spooled: a different age file whose
    /// PLAINTEXT is the slice (re-encrypted: other key, header, nonce, so the
    /// ciphertext hash is not the recorded one) IS the slice, because the
    /// recorded plaintext checksum says so. Nothing is unproven, nobody is
    /// asked for another cartridge, and the restore is whole. The same slice
    /// with no plaintext checksum recorded (a catalog that never had one) is
    /// unproven — still restored, since every file in it checks out — and the
    /// cartridge's record says so.
    #[test]
    fn a_reencrypted_slice_is_whole_by_its_plaintext_checksum_and_unproven_without_one() {
        for recorded in [true, false] {
            let mut fx = Fixture::new(&[A], tree());
            let st = fx.stage(&tree(), "150k", false);
            fx.write(&st, A);
            fx.replace_slice(A, &st, 3, &st.plains[2].clone());
            if !recorded {
                fx.forget_plain_sha(&st, 3);
            }
            let mut mem = clone_mem(&fx.mems[A]);
            let dest = TempDir::new().unwrap();
            let dest_str = dest.path().to_string_lossy().into_owned();
            restore_unit_from_store(
                &fx.conn,
                &fx.paths,
                &Config::default(),
                &fx.unit,
                A,
                1,
                RestoreTarget::Unit {
                    dest_dir: &dest_str,
                },
                &RestoreOptions::default(),
                &mut mem,
                site(),
            )
            .unwrap_or_else(|e| panic!("recorded={recorded}: {e}"));
            fx.assert_restored(dest.path());
            let detail = fx.contacts()[0].2.clone().unwrap_or_default();
            assert_eq!(
                detail.contains("slice 3 unproven"),
                !recorded,
                "recorded={recorded}: {detail}"
            );
        }
    }

    /// One slice is one age file. Copy A is good for slice 2's first two
    /// chunks and bad after; copy B is a clean, complete age file of the same
    /// plaintext — but another file, with its own header and nonce. Its
    /// chunks authenticate under its own key and prove nothing about the part
    /// A gave, so they are never mixed into the same slice: the restore
    /// refuses, by name, both ways dar can be fed.
    #[test]
    fn chunks_of_two_age_files_are_never_mixed_into_one_slice() {
        for catalogue in [true, false] {
            let (mut fx, st) = two_copies(catalogue);
            assert!(st.chunks_of(2) >= 3, "{}", st.chunks_of(2));
            fx.corrupt(A, &st, 2, 2);
            fx.replace_slice(B, &st, 2, &st.plains[1].clone());
            let mut bay = fx.bay(A, true, &[Reply::Load(B)]);
            let dest = TempDir::new().unwrap();
            let err = fx
                .restore(A, &mut bay, dest.path(), &RestoreOptions::default())
                .expect_err(&format!(
                    "catalogue={catalogue}: mixed chunks must be refused"
                ))
                .to_string();
            assert!(err.contains("slice 2"), "catalogue={catalogue}: {err}");
            assert!(
                err.contains("different age file") && err.contains("never mixed"),
                "catalogue={catalogue}: {err}"
            );
            assert_eq!(bay.asked_labels(), [B], "catalogue={catalogue}");
            assert!(
                !dest.path().join(SCRATCH_NAME).exists(),
                "catalogue={catalogue}: no decrypted slice is left behind"
            );
        }
    }

    /// A wrong cartridge in the drive when the right one is asked for is
    /// refused by the identity check — File 0 names another volume — and the
    /// operator is asked again, naming the cartridge; and a substitute whose File
    /// 0 requires a feature this tapectl does not know is refused by the same
    /// text `restore raw` gives (#476).
    #[test]
    fn a_substitute_must_pass_the_identity_check_too() {
        let (mut fx, st) = two_copies(false);
        let mut other = fx.mems.remove(B).unwrap();
        // B requires a feature nobody has.
        let old = other.files[0].clone();
        other.files[0] = crate::volume::format::rewrite_file0(
            &old,
            "\nrequires = [\"short-seal\"]\n",
            "\nrequires = [\"unknown-feature\"]\n",
        );
        fx.mems.insert(B.to_string(), other);
        fx.corrupt(A, &st, 3, 0);
        // Enter without changing anything leaves A in the drive: wrong tape;
        // then B is loaded: refused for its requires; then give up.
        let mut bay = fx.bay(A, true, &[Reply::Enter, Reply::Load(B), Reply::Stop]);
        let dest = TempDir::new().unwrap();
        let err = fx
            .restore(A, &mut bay, dest.path(), &RestoreOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("stopped at the operator's request"), "{err}");
        assert_eq!(
            bay.asked_labels(),
            [B, B, B],
            "asked again after each refusal"
        );
        // Every refused arrival is a contact, closed failed.
        let contacts = fx.contacts();
        let failed: Vec<_> = contacts.iter().filter(|c| c.1 == "failed").collect();
        assert_eq!(failed.len(), 2, "{contacts:?}");
        assert!(
            failed[0].2.as_deref().unwrap_or("").contains("wrong tape"),
            "{contacts:?}"
        );
        assert!(
            failed[1]
                .2
                .as_deref()
                .unwrap_or("")
                .contains("unknown-feature"),
            "{contacts:?}"
        );
    }

    // ── item 102: a lost file is filled from another copy of the same version ──

    /// The tree with one byte changed in every file: the same names, other bytes.
    fn tampered(files: &[(String, Vec<u8>)]) -> Vec<(String, Vec<u8>)> {
        files
            .iter()
            .map(|(p, b)| {
                let mut b = b.clone();
                b[777] ^= 0x55;
                (p.clone(), b)
            })
            .collect()
    }

    /// Two stagings of one version (two stage sets, so slices of different sizes
    /// and different hashes: slice substitution matches none of them), one on A
    /// and one on B. A's is damaged in the middle of a slice.
    fn two_stagings(catalogue: bool) -> (Fixture, Staging, Staging) {
        let mut fx = Fixture::new(&[A, B], tree());
        let one = fx.stage(&tree(), "150k", catalogue);
        let two = fx.stage(&tree(), "90k", catalogue);
        assert_ne!(one.slices(), two.slices(), "different slicing");
        fx.write(&one, A);
        fx.write(&two, B);
        (fx, one, two)
    }

    /// THE acceptance test of item 102: the only copy of a stage set is damaged
    /// where no slice of it can be had, and the lost files come whole from the
    /// other staging of the same version — asked for by label and serial,
    /// verified against the recorded checksums, and the restore is whole.
    #[test]
    fn files_lost_to_a_damaged_slice_are_filled_from_another_staging_of_the_version() {
        for catalogue in [true, false] {
            let (mut fx, one, _two) = two_stagings(catalogue);
            fx.corrupt(A, &one, 3, 1);
            let mut bay = fx.bay(A, true, &[Reply::Load(B)]);
            let dest = TempDir::new().unwrap();
            let report = fx
                .restore(A, &mut bay, dest.path(), &RestoreOptions::default())
                .unwrap_or_else(|e| panic!("catalogue={catalogue}: {e}"));
            fx.assert_restored(dest.path());
            assert_eq!(bay.asked_labels(), [B], "catalogue={catalogue}");
            assert!(
                bay.asked[0].1.contains("damaged or missing")
                    && bay.asked[0].1.contains("the same files"),
                "{:?}",
                bay.asked
            );
            assert!(!report.filled.is_empty(), "{report:?}");
            for (path, from) in &report.filled {
                assert!(
                    from.starts_with(B) && from.contains("sha256 matches"),
                    "{path}: {from}"
                );
            }
            // A's slices had no other copy, so none was substituted.
            assert!(report.substitutions.is_empty());
            let contacts = fx.contacts();
            assert_eq!(contacts.len(), 2, "{contacts:?}");
            assert!(
                contacts[1].2.as_deref().unwrap_or("").contains("supplied"),
                "the copy's contact says what it supplied: {contacts:?}"
            );
            // And nothing of the fill is left behind.
            assert!(!dest.path().join(SCRATCH_NAME).exists());
        }
    }

    /// With the other staging on the cartridge already in the drive there is no
    /// swap at all; and with a catalogue only the slices that hold the lost files
    /// (and the last) are read from it.
    #[test]
    fn a_fill_from_the_cartridge_in_the_drive_asks_nobody_and_reads_only_what_holds_the_files() {
        let mut fx = Fixture::new(&[A], tree());
        // The later stage set of a version is the one a restore reads; the
        // earlier is the other copy.
        let two = fx.stage(&tree(), "50k", true);
        let one = fx.stage(&tree(), "150k", true);
        fx.write(&two, A);
        fx.write(&one, A);
        fx.corrupt(A, &one, 3, 1);
        let (mut bay, fakes) = fx.fake_bay(A, &[]);
        let dest = TempDir::new().unwrap();
        let report = fx
            .restore(A, &mut bay, dest.path(), &RestoreOptions::default())
            .unwrap();
        fx.assert_restored(dest.path());
        assert!(bay.asked.is_empty());
        assert!(!report.filled.is_empty(), "{report:?}");
        let two_first = fx.first_position[&(A.to_string(), two.stage_set_id)];
        let reads: Vec<usize> = fakes[A]
            .ops()
            .iter()
            .filter_map(|op| match op {
                crate::tape::fake::Op::Read(n)
                    if (*n as usize) >= two_first && (*n as usize) < two_first + two.slices() =>
                {
                    Some(*n as usize - two_first + 1)
                }
                _ => None,
            })
            .collect();
        assert!(
            !reads.is_empty() && reads.len() < two.slices(),
            "only some of the other staging's {} slices are read: {reads:?}",
            two.slices()
        );
        assert!(
            reads.contains(&two.slices()),
            "dar's catalogue slice is read: {reads:?}"
        );
    }

    /// A file whose only other copy has different bytes than the version
    /// recorded is reported, not written: the checksum is the arbiter.
    #[test]
    fn a_copy_that_does_not_match_the_recorded_checksum_is_refused_and_said() {
        let mut fx = Fixture::new(&[A, B], tree());
        let one = fx.stage(&tree(), "150k", true);
        let two = fx.stage(&tampered(&tree()), "90k", true);
        fx.write(&one, A);
        fx.write(&two, B);
        fx.corrupt(A, &one, 3, 1);
        let mut bay = fx.bay(A, true, &[Reply::Load(B)]);
        let dest = TempDir::new().unwrap();
        let err = fx
            .restore(A, &mut bay, dest.path(), &RestoreOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("INCOMPLETE"), "{err}");
        assert!(
            err.contains("does not match the version's recorded checksum"),
            "{err}"
        );
        assert!(err.contains("not kept"), "{err}");
        let lost = fx.differing(dest.path());
        assert!(!lost.is_empty());
        for path in &lost {
            assert!(err.contains(path.as_str()), "{path} is named: {err}");
            // Not the tampered copy's bytes: the bad file is as dar left it.
            let bytes = fs::read(dest.path().join(path)).unwrap();
            let wrong = &tampered(&tree())
                .into_iter()
                .find(|(p, _)| p == path)
                .unwrap()
                .1;
            assert_ne!(&bytes, wrong, "{path}: the refused copy was written");
        }
    }

    /// Without a person at the terminal the fill refuses by name too, listing the
    /// holders of the other staging.
    #[test]
    fn an_unattended_fill_refuses_by_name_listing_the_holders() {
        let (mut fx, one, _two) = two_stagings(true);
        fx.corrupt(A, &one, 3, 1);
        let mut bay = fx.bay(A, false, &[Reply::Load(B)]);
        let dest = TempDir::new().unwrap();
        let err = fx
            .restore(A, &mut bay, dest.path(), &RestoreOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("INCOMPLETE"), "{err}");
        assert!(err.contains(B), "the holder is named: {err}");
        assert!(err.contains("swaps no cartridge without a person"), "{err}");
        assert!(bay.asked.is_empty());
    }

    /// Another VERSION is never a source, even where the file is unchanged.
    #[test]
    fn another_version_is_never_a_source() {
        let mut fx = Fixture::new(&[A, B], tree());
        let one = fx.stage(&tree(), "150k", true);
        fx.write(&one, A);
        let v2 = fx.new_version(2);
        let other = fx.stage_in(v2, &tree(), "90k", true);
        fx.write_in(v2, &other, B);
        fx.corrupt(A, &one, 3, 1);
        let mut bay = fx.bay(A, true, &[Reply::Load(B)]);
        let dest = TempDir::new().unwrap();
        let err = fx
            .restore(A, &mut bay, dest.path(), &RestoreOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("INCOMPLETE"), "{err}");
        assert!(
            bay.asked.is_empty(),
            "nothing of another version is asked for"
        );
    }

    /// `restore file` fills its one file from the other staging too, and places
    /// it only once it is whole.
    #[test]
    fn restore_file_is_filled_from_another_staging_and_placed_whole() {
        let (mut fx, one, _two) = two_stagings(true);
        fx.corrupt(A, &one, 3, 1);
        // A file the damaged chunk falls in: found by restoring the unit once.
        let probe = TempDir::new().unwrap();
        let mut bay = fx.bay(A, true, &[]);
        fx.restore(A, &mut bay, probe.path(), &RestoreOptions::default())
            .unwrap_err();
        let damaged = fx.differing(probe.path());
        let file = damaged.first().expect("a file lost to the chunk").clone();

        let mut bay = fx.bay(A, true, &[Reply::Load(B)]);
        let dest = TempDir::new().unwrap();
        let dest_str = dest.path().to_string_lossy().into_owned();
        let report = restore_unit_from_bay(
            &fx.conn,
            &fx.paths,
            &Config::default(),
            &fx.unit,
            A,
            1,
            RestoreTarget::File {
                file_path: &file,
                dest_dir: &dest_str,
            },
            &RestoreOptions::default(),
            &mut bay,
            site(),
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let want = &fx.files.iter().find(|(p, _)| *p == file).unwrap().1;
        assert_eq!(&fs::read(dest.path().join(&file)).unwrap(), want);
        assert_eq!(report.filled.len(), 1);
        assert_eq!(bay.asked_labels(), [B]);
    }

    // ── cost: what a swap costs the tapes (issue #416's discipline, #480) ──

    /// The pin for a one-swap restore, next to the clean restore's (the
    /// `FIRST`/`Space`/`Read` vector in `contact`'s tests, unchanged): the first
    /// cartridge is read forward to the slice that fails and that slice to its
    /// end — one rewind, none after — then released; the second is opened at BOT,
    /// its File 0 looked at once for the identity check, and read forward from
    /// the failed slice to the end. No cartridge is rewound twice, no file is
    /// read twice, and the first is not asked for again.
    #[test]
    fn a_one_swap_restore_moves_each_tape_once_and_forward_only() {
        let (mut fx, st) = two_copies(false);
        fx.corrupt(A, &st, 3, 0);
        let slices = st.slices();
        let (mut bay, fakes) = fx.fake_bay(A, &[Reply::Load(B)]);
        let dest = TempDir::new().unwrap();
        fx.restore(A, &mut bay, dest.path(), &RestoreOptions::default())
            .unwrap();
        fx.assert_restored(dest.path());
        assert_eq!(bay.asked_labels(), [B]);
        assert_eq!(bay.releases, 1, "the first cartridge was released once");

        use crate::tape::fake::Op;
        let first = FIRST as u32;
        // A: File 0 (the contact check), then slices 1..=3 — the third bad, read
        // through to the filemark for its hash.
        let mut want_a = vec![Op::Rewind, Op::ReadHead(0), Op::Space(first - 1)];
        want_a.extend((0..3).map(|i| Op::Read(first + i)));
        assert_eq!(fakes[A].ops(), want_a, "the first cartridge");
        // B: opened at BOT, File 0 once, then slice 3 on to the last.
        let ops_b = fakes[B].ops();
        assert_eq!(ops_b[..2], [Op::Rewind, Op::ReadHead(0)], "{ops_b:?}");
        let reads_b: Vec<u32> = ops_b
            .iter()
            .filter_map(|op| match op {
                Op::Read(n) => Some(*n),
                _ => None,
            })
            .collect();
        assert_eq!(
            reads_b,
            (2..slices as u32).map(|i| first + i).collect::<Vec<_>>(),
            "B is read from the failed slice to the last, each once: {ops_b:?}"
        );
        assert_eq!(fakes[B].rewinds(), 1, "{ops_b:?}");
        assert_eq!(fakes[A].rewinds(), 1);
    }

    /// `restore volume` reads through the same supply: a bad slice of one of its
    /// units is read from the other copy, and the cartridge that served stays in
    /// the drive for the units after it.
    #[test]
    fn restore_volume_substitutes_a_slice_too() {
        let (mut fx, st) = two_copies(false);
        fx.corrupt(A, &st, 3, 0);
        let dest = TempDir::new().unwrap();
        let root = dest.path().to_string_lossy().into_owned();
        let requests = unit_requests(&[fx.unit.clone()], &root).unwrap();
        let config = Config::default();
        let options = RestoreOptions::default();
        let planned = plan_units(&fx.conn, &config, A, &requests, &options).unwrap();
        let mut bay = fx.bay(A, true, &[Reply::Load(B)]);
        let report = restore_units_from_bay(
            &fx.conn,
            &fx.paths,
            &config,
            A,
            &planned,
            &options,
            false,
            &mut bay,
            site(),
        )
        .unwrap();
        assert_eq!(report.failed(), 0, "{report:?}");
        fx.assert_restored(&dest.path().join(&fx.unit));
        assert_eq!(bay.asked_labels(), [B]);
        assert_eq!(fx.contacts().len(), 2);
        let rows = crate::volume::restore_record::rows(&fx.conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, "ok");
    }

    /// A restore the operator stops at the prompt leaves what it restored and
    /// says so, and the contact of the cartridge that was in the drive is closed.
    #[test]
    fn stopping_at_the_prompt_ends_the_restore_by_name() {
        let (mut fx, st) = two_copies(true);
        fx.corrupt(A, &st, 3, 0);
        let mut bay = fx.bay(A, true, &[Reply::Stop]);
        let dest = TempDir::new().unwrap();
        let err = fx
            .restore(A, &mut bay, dest.path(), &RestoreOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("stopped at the operator's request"), "{err}");
        assert!(err.contains(B), "{err}");
        let contacts = fx.contacts();
        assert_eq!(contacts.len(), 1, "{contacts:?}");
        assert_eq!(
            contacts[0].1, "ok",
            "A left the drive normally: {contacts:?}"
        );
        let rows = crate::volume::restore_record::rows(&fx.conn).unwrap();
        assert_eq!(rows[0].outcome, "failed");
        assert!(!dest.path().join(SCRATCH_NAME).exists());
    }

    /// A quarantined volume (a verify proved its medium bad) is the last resort:
    /// an in-service copy is asked for first, and when the quarantined one is
    /// all there is it is asked for, said so.
    #[test]
    fn a_quarantined_copy_is_asked_for_last_and_said_to_be_the_last_resort() {
        let mut fx = Fixture::new(&[A, B, C], tree());
        let st = fx.stage(&tree(), "150k", false);
        for label in [A, B, C] {
            fx.write(&st, label);
        }
        fx.corrupt(A, &st, 3, 0);
        fx.conn
            .execute(
                "UPDATE volumes SET observed_condition = 'quarantined' WHERE label = ?1",
                params![B],
            )
            .unwrap();
        // C (in service) is asked for, though B has the lower id.
        let mut bay = fx.bay(A, true, &[Reply::Load(C)]);
        let dest = TempDir::new().unwrap();
        fx.restore(A, &mut bay, dest.path(), &RestoreOptions::default())
            .unwrap();
        fx.assert_restored(dest.path());
        assert_eq!(bay.asked_labels(), [C]);
        assert!(!bay.asked[0].1.contains("last resort"), "{:?}", bay.asked);

        // C damaged too: the quarantined B is all that is left, and the prompt says so.
        fx.corrupt(C, &st, 3, 0);
        let mut bay = fx.bay(A, true, &[Reply::Load(C), Reply::Load(B)]);
        let dest = TempDir::new().unwrap();
        fx.restore(A, &mut bay, dest.path(), &RestoreOptions::default())
            .unwrap();
        fx.assert_restored(dest.path());
        assert_eq!(bay.asked_labels(), [C, B]);
        assert!(
            bay.asked[1].1.contains("quarantined") && bay.asked[1].1.contains("last resort"),
            "{:?}",
            bay.asked
        );
    }

    /// What the operator reads: the prompt, the lines `restore unit` prints after
    /// a repair, and the error of a restore that could not be made whole. Printed
    /// so the texts can be reviewed as the operator sees them (`--nocapture`), and
    /// pinned in the parts an operator acts on.
    #[test]
    fn what_the_operator_reads() {
        use crate::cli::restore::repair_lines;

        // A substitution.
        let (mut fx, st) = two_copies(false);
        fx.bind(B, "LTO6-0002", "HUM1234567");
        fx.corrupt(A, &st, 3, 0);
        let mut bay = fx.bay(A, true, &[Reply::Load(B)]);
        let dest = TempDir::new().unwrap();
        let report = fx
            .restore(A, &mut bay, dest.path(), &RestoreOptions::default())
            .unwrap();
        assert!(
            bay.asked[0]
                .1
                .contains("cartridge LTO6-0002, chip serial HUM1234567"),
            "the prompt names the sticker and the chip: {:?}",
            bay.asked
        );
        let mut out = String::new();
        for line in bay.asked[0].1.split(" | ") {
            out.push_str(&format!("{line}\n"));
        }
        out.push_str(&format!(
            "restored \"{}\" v{} from {} ({} slices) to {}\n",
            report.unit_name,
            report.version,
            report.volume_label,
            report.slices,
            report.destination
        ));
        for line in repair_lines(&report.substitutions, &report.filled) {
            out.push_str(&format!("{line}\n"));
        }
        eprintln!("=== SUBSTITUTION ===\n{out}");
        assert!(
            out.contains("was read from L6-0002, another copy of it"),
            "{out}"
        );

        // A salvage: one corrupted byte, the only copy.
        let mut fx = Fixture::new(&[A], tree());
        let st = fx.stage(&tree(), "150k", false);
        fx.write(&st, A);
        fx.corrupt(A, &st, 2, 1);
        let mut mem = clone_mem(&fx.mems[A]);
        let dest = TempDir::new().unwrap();
        let dest_str = dest.path().to_string_lossy().into_owned();
        let err = restore_unit_from_store(
            &fx.conn,
            &fx.paths,
            &Config::default(),
            &fx.unit,
            A,
            1,
            RestoreTarget::Unit {
                dest_dir: &dest_str,
            },
            &RestoreOptions::default(),
            &mut mem,
            site(),
        )
        .unwrap_err()
        .to_string();
        eprintln!("=== SALVAGE ===\n{err}\n");

        // A fill.
        let (mut fx, one, _two) = two_stagings(true);
        fx.corrupt(A, &one, 3, 1);
        let mut bay = fx.bay(A, true, &[Reply::Load(B)]);
        let dest = TempDir::new().unwrap();
        let report = fx
            .restore(A, &mut bay, dest.path(), &RestoreOptions::default())
            .unwrap();
        let mut out = String::new();
        for line in bay.asked[0].1.split(" | ") {
            out.push_str(&format!("{line}\n"));
        }
        out.push_str(&format!(
            "restored \"{}\" v{} from {} ({} slices) to {}\n",
            report.unit_name,
            report.version,
            report.volume_label,
            report.slices,
            report.destination
        ));
        for line in repair_lines(&report.substitutions, &report.filled) {
            out.push_str(&format!("{line}\n"));
        }
        eprintln!("=== FILL ===\n{out}");
        assert!(
            out.contains("were filled from another copy of this version"),
            "{out}"
        );
    }

    // ── ADR-0012 2026-10-19 item 107: the repairs with `--key` ──────────────
    //
    // An heir holds the escrow secret and nothing in the home opens the tape.
    // Every repair runs on the same identities the first read does, so each of
    // them must work with the identity given on the command line alone.

    /// The heir's position: the key the slices are encrypted to is no longer in
    /// the home — a decoy takes its file name, so the home still has a key for
    /// the tenant and it opens nothing on this tape — and the secret is in a
    /// file of its own, as `--key` takes it. The directory keeps the file.
    fn heir_keys(fx: &Fixture) -> (TempDir, Vec<keys::KeyFile>) {
        let home_key = fx.paths.keys_dir.join("t1-primary.age.key");
        let secret = fs::read_to_string(&home_key).unwrap();
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("escrow.key");
        fs::write(&file, format!("# the escrow secret\n{secret}")).unwrap();
        keys::save_secret_key(&home_key, &keys::generate_keypair().secret_key).unwrap();
        let given = vec![keys::KeyFile::read(&file).unwrap()];
        (dir, given)
    }

    fn with_keys(given: &[keys::KeyFile]) -> RestoreOptions {
        RestoreOptions {
            keys: given.to_vec(),
            ..RestoreOptions::default()
        }
    }

    /// The wrong-key refusal as it stands today, with no `--key` — the control
    /// every test below compares to: age's own words, and no key file named.
    fn assert_wrong_key_refusal_without_keys(fx: &Fixture, label: &str, bay: &mut TestBay) {
        let dest = TempDir::new().unwrap();
        let err = fx
            .restore(label, bay, dest.path(), &RestoreOptions::default())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&age::DecryptError::NoMatchingKeys.to_string()),
            "{err}"
        );
        assert!(!err.contains("--key"), "no --key was given: {err}");
    }

    /// SLICE SUBSTITUTION with only the given identity: the acceptance test of
    /// the slice half, whole, through a swap to the second copy.
    #[test]
    fn a_slice_is_substituted_from_another_copy_with_only_the_key_given() {
        for catalogue in [true, false] {
            let (mut fx, st) = two_copies(catalogue);
            fx.corrupt(A, &st, 3, 0);
            fx.corrupt(B, &st, 2, 0);
            let (_dir, given) = heir_keys(&fx);

            // Control: the same damage, the home's key opens nothing.
            let mut bay = fx.bay(A, true, &[Reply::Load(B)]);
            assert_wrong_key_refusal_without_keys(&fx, A, &mut bay);

            let mut bay = fx.bay(A, true, &[Reply::Load(B)]);
            let dest = TempDir::new().unwrap();
            let report = fx
                .restore(A, &mut bay, dest.path(), &with_keys(&given))
                .unwrap_or_else(|e| panic!("catalogue={catalogue}: {e}"));
            fx.assert_restored(dest.path());
            assert_eq!(bay.asked_labels(), [B], "catalogue={catalogue}");
            assert_eq!(report.substitutions.len(), 1);
            let sub = &report.substitutions[0];
            assert_eq!(
                (
                    sub.slice_number,
                    sub.supplied_by.as_str(),
                    sub.damaged_on.as_str()
                ),
                (3, B, A)
            );
        }
    }

    /// CHUNK SALVAGE with only the given identity: one corrupted byte costs the
    /// files in its chunk, the rest are byte for byte whole, the restore says
    /// INCOMPLETE and names the damaged files — as without `--key`.
    #[test]
    fn a_bad_chunk_is_salvaged_around_with_only_the_key_given() {
        for catalogue in [true, false] {
            let mut fx = Fixture::new(&[A], tree());
            let st = fx.stage(&tree(), "150k", catalogue);
            fx.write(&st, A);
            fx.corrupt(A, &st, 2, 1);
            let (_dir, given) = heir_keys(&fx);
            let restore = |options: &RestoreOptions, dest: &Path| {
                let mut mem = clone_mem(&fx.mems[A]);
                let dest_str = dest.to_string_lossy().into_owned();
                restore_unit_from_store(
                    &fx.conn,
                    &fx.paths,
                    &Config::default(),
                    &fx.unit,
                    A,
                    1,
                    RestoreTarget::Unit {
                        dest_dir: &dest_str,
                    },
                    options,
                    &mut mem,
                    site(),
                )
                .unwrap_err()
                .to_string()
            };

            // Control: the home's key opens nothing, and the refusal says so.
            let dest = TempDir::new().unwrap();
            let err = restore(&RestoreOptions::default(), dest.path());
            assert!(
                err.contains(&age::DecryptError::NoMatchingKeys.to_string()),
                "{err}"
            );

            let dest = TempDir::new().unwrap();
            let err = restore(&with_keys(&given), dest.path());
            assert!(err.contains("INCOMPLETE"), "catalogue={catalogue}: {err}");
            let differing = fx.differing(dest.path());
            assert!(
                !differing.is_empty() && differing.len() <= 2,
                "one 64 KiB chunk of 100 KB files costs one or two files: {differing:?}"
            );
            for path in &differing {
                assert!(err.contains(path.as_str()), "{path} is named: {err}");
            }
            for (path, bytes) in &fx.files {
                if !differing.contains(path) {
                    assert_eq!(&fs::read(dest.path().join(path)).unwrap(), bytes, "{path}");
                }
            }
        }
    }

    /// FILE FILL with only the given identity: the only copy of a stage set is
    /// damaged past reading and the lost files come whole from the other
    /// staging of the version, decrypted with the identity given.
    #[test]
    fn lost_files_are_filled_from_another_staging_with_only_the_key_given() {
        let (mut fx, one, _two) = two_stagings(true);
        fx.corrupt(A, &one, 3, 1);
        let (_dir, given) = heir_keys(&fx);
        let mut bay = fx.bay(A, true, &[Reply::Load(B)]);
        let dest = TempDir::new().unwrap();
        let report = fx
            .restore(A, &mut bay, dest.path(), &with_keys(&given))
            .unwrap();
        fx.assert_restored(dest.path());
        assert_eq!(bay.asked_labels(), [B]);
        assert!(!report.filled.is_empty(), "{report:?}");
    }

    /// A key given that opens nothing gets the refusal as it stands, with the
    /// files named after it; one wrong file beside the right one is no
    /// obstacle, in either order.
    #[test]
    fn a_wrong_key_is_refused_naming_the_files_and_a_right_one_beside_it_opens() {
        let mut fx = Fixture::new(&[A], tree());
        let st = fx.stage(&tree(), "150k", true);
        fx.write(&st, A);
        let (dir, right) = heir_keys(&fx);
        let wrong_a = dir.path().join("wrong-a.key");
        let wrong_b = dir.path().join("wrong-b.key");
        for f in [&wrong_a, &wrong_b] {
            fs::write(f, format!("{}\n", keys::generate_keypair().secret_key)).unwrap();
        }
        let wrong = keys::KeyFile::read_all(&[wrong_a.clone(), wrong_b.clone()]).unwrap();

        let mut bay = fx.bay(A, true, &[]);
        let dest = TempDir::new().unwrap();
        let err = fx
            .restore(A, &mut bay, dest.path(), &with_keys(&wrong))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&age::DecryptError::NoMatchingKeys.to_string()),
            "the refusal as it stands: {err}"
        );
        for f in [&wrong_a, &wrong_b] {
            assert!(err.contains(&f.display().to_string()), "{f:?} named: {err}");
        }

        for order in [[0, 1, 2], [2, 1, 0]] {
            let all: Vec<keys::KeyFile> = [wrong[0].clone(), wrong[1].clone(), right[0].clone()]
                .into_iter()
                .collect();
            let given: Vec<keys::KeyFile> = order.iter().map(|&i| all[i].clone()).collect();
            let mut bay = fx.bay(A, true, &[]);
            let dest = TempDir::new().unwrap();
            fx.restore(A, &mut bay, dest.path(), &with_keys(&given))
                .unwrap_or_else(|e| panic!("order {order:?}: {e}"));
            fx.assert_restored(dest.path());
        }
    }
}
