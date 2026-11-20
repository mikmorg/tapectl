//! Issue #480 (ADR-0012 items 101 and 102): a damaged slice is read from
//! another copy, a bad age chunk costs only itself, and lost files are filled
//! from another stage set of the same version.
//!
//! Everything here is real except the drives: units staged by `stage create`
//! (GNU tar, the cut at member starts, age), tapes written by `volume write`
//! ([`super::fixture::Home`]), real `Supply`, real contacts and `restores`
//! rows. The cartridges are in-memory tapes behind a [`TestBay`], which plays
//! the operator too: a script of what is loaded when a cartridge is asked
//! for, and a record of what was asked.

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::{BTreeMap, BTreeSet, VecDeque};
    use std::io::Read;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::super::fixture::{assert_restored, files, listing, Home, Item, What, BS, TENANT};
    use super::super::supply::{Answer, Arrival, Bay, Want};
    use super::super::*;
    use crate::store::TapeStore;
    use crate::tape::fake::{FakeTape, Op};
    /// What the operator does when a cartridge is asked for.
    #[derive(Debug, Clone, Copy)]
    pub(crate) enum Reply {
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
    pub(crate) struct TestBay {
        pub(crate) carts: BTreeMap<String, Box<dyn Store>>,
        /// What the st driver has open (`None` between a release and a load).
        pub(crate) open: Option<String>,
        /// What is physically in the drive.
        pub(crate) physical: Option<String>,
        pub(crate) attended: bool,
        pub(crate) script: VecDeque<Reply>,
        /// `(label asked for, the reason shown)`, in order.
        pub(crate) asked: Vec<(String, String)>,
        pub(crate) releases: usize,
    }

    impl TestBay {
        pub(crate) fn asked_labels(&self) -> Vec<&str> {
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

    // ── the fixture's own helpers ───────────────────────────────────────
    //
    // Every unit is staged by the real `stage create` and every tape written
    // by the real `volume write` ([`Home`]); what a test adds is damage to
    // the bytes a tape holds, and a script for the operator.

    const A: &str = "RPR-A";
    const B: &str = "RPR-B";
    const C: &str = "RPR-C";
    const UNIT: &str = "alpha";
    /// An age chunk's plaintext.
    const CHUNK: u64 = 65_536;
    /// `[defaults] slice_size = "256K"`, in bytes.
    const SLICE_256K: u64 = 256 * 1024;

    /// Forty files of 30,000 bytes and one more in a folder: at 256K about
    /// eight members to a slice, so six slices of nearly four chunks each.
    fn tree() -> Vec<Item> {
        let mut items = files("alpha", 40, 30_000, 1);
        items.push(Item::file("sub/deep.txt", b"deep in a folder\n".to_vec()));
        items
    }

    /// One unit staged once and written to each of `labels` (`min_copies`
    /// is their number, so the staging stays until the last is written):
    /// the same ciphertext on every cartridge.
    fn copies(slice_size: &str, compression: Option<&str>, labels: &[&str]) -> Home {
        let mut h = Home::new(slice_size, compression, labels.len() as i32);
        h.unit(TENANT, UNIT, &tree());
        h.stage(UNIT);
        for label in labels {
            h.volume(label);
        }
        for label in labels {
            h.write(label);
        }
        assert!(
            h.num_slices(UNIT) >= 4,
            "a many-slice unit: {}",
            h.num_slices(UNIT)
        );
        h
    }

    /// Flip a byte inside chunk `chunk` of slice `slice` of the stage set
    /// `set` as cartridge `label` holds it. The age header is a few hundred
    /// bytes, so 2,000 bytes in is chunk 0's data, and each later chunk is
    /// 65,552 ciphertext bytes on.
    fn damage_in(h: &mut Home, label: &str, set: i64, slice: i64, chunk: u64) {
        let position = h.position_in(label, set, slice);
        h.flip(label, position, (2_000 + chunk * (CHUNK + 16)) as usize);
    }

    /// [`damage_in`] on the latest stage set of the unit.
    fn damage(h: &mut Home, label: &str, slice: i64, chunk: u64) {
        let set = h.stage_set(UNIT, 1);
        damage_in(h, label, set, slice, chunk);
    }

    /// The members of `set` that touch the 64 KiB chunk `chunk` of slice
    /// `slice`'s cut: what a hole in that chunk costs, and nothing else.
    fn lost_in_chunk(
        h: &Home,
        set: i64,
        slice_size: u64,
        slice: u64,
        chunk: u64,
    ) -> BTreeSet<String> {
        let rows = stage_files::load(&h.conn, set).unwrap();
        let begins = filetable::slice_start(&rows, slice_size, slice).expect("a slice of members");
        let (lo, hi) = (begins + chunk * CHUNK, begins + (chunk + 1) * CHUNK);
        rows.iter()
            .filter(|r| r.start < hi && r.end > lo)
            .map(|r| r.path.clone())
            .collect()
    }

    /// The members of `set` that begin in slice `slice`.
    fn members_of_slice(h: &Home, set: i64, slice: u64) -> BTreeSet<String> {
        stage_files::load(&h.conn, set)
            .unwrap()
            .into_iter()
            .filter(|r| r.slice == slice)
            .map(|r| r.path)
            .collect()
    }

    /// The paths an INCOMPLETE restore lists as damaged or missing.
    fn lost_paths(err: &str) -> BTreeSet<String> {
        err.lines()
            .skip_while(|l| !l.contains("file(s) damaged or missing:"))
            .skip(1)
            .take_while(|l| l.starts_with("    ") && !l.starts_with("     "))
            .filter_map(|l| l.trim().rsplit_once(" (").map(|(p, _)| p.to_string()))
            .collect()
    }

    /// Every file of the unit but the `lost` ones is in `dest` byte for
    /// byte, and no lost one is there at all: a member that could not be
    /// given is left out, never written short or as zeros.
    fn assert_all_but(h: &Home, dest: &Path, lost: &BTreeSet<String>) {
        let want = listing(&h.source(UNIT));
        let got = listing(dest);
        for (path, what) in &want {
            if lost.contains(path) {
                assert!(
                    !dest.join(path).exists(),
                    "{path} was listed lost, so it is not written"
                );
            } else {
                assert_eq!(got.get(path), Some(what), "{path} must be restored whole");
            }
        }
        assert!(
            got.len() > lost.len(),
            "positive control: the other files are there ({} of {})",
            got.len(),
            want.len()
        );
    }

    /// `(volume label, outcome, detail)` of every cartridge contact a restore
    /// made (not the fixture's own `volume init` and `volume write`), in order.
    fn contacts(h: &Home) -> Vec<(String, String, Option<String>)> {
        let mut stmt = h
            .conn
            .prepare(
                "SELECT COALESCE(v.label, '-'), COALESCE(c.outcome, '-'), c.detail
                   FROM cartridge_contacts c LEFT JOIN volumes v ON v.id = c.volume_id
                  WHERE c.operation LIKE 'restore%'
                  ORDER BY c.id",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    }

    fn detail(contact: &(String, String, Option<String>)) -> &str {
        contact.2.as_deref().unwrap_or("")
    }

    /// `restore unit` of the unit, version 1, into a fresh directory.
    fn restore_unit(
        h: &Home,
        from: &str,
        options: &RestoreOptions,
        bay: &mut TestBay,
    ) -> (Result<RestoreReport>, TempDir) {
        h.restore(UNIT, What::Unit, from, options, bay)
    }

    /// The bay with every cartridge on its own [`FakeTape`], handed back by
    /// label for its record of operations.
    fn fake_bay(h: &Home, start: &str, script: &[Reply]) -> (TestBay, BTreeMap<String, FakeTape>) {
        let mut fakes = BTreeMap::new();
        let carts = h
            .tapes
            .iter()
            .map(|(label, medium)| {
                let fake = FakeTape::with_files(medium.files.clone(), BS);
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

    // ── another age file where a slice is ──────────────────────────────

    /// Bytes unlike anything else in the fixtures (`noise` is one stream read
    /// from different offsets, so its seeds overlap): a hash chain.
    fn unlike(len: usize, tag: u64) -> Vec<u8> {
        use sha2::Digest;
        let mut out = Vec::with_capacity(len + 32);
        let mut n = 0u64;
        while out.len() < len {
            let mut hash = sha2::Sha256::new();
            hash.update(tag.to_le_bytes());
            hash.update(n.to_le_bytes());
            out.extend_from_slice(&hash.finalize());
            n += 1;
        }
        out.truncate(len);
        out
    }

    /// What the catalog records for slice `n` of `set`: the cut's length and
    /// the age file's.
    fn slice_sizes(h: &Home, set: i64, n: i64) -> (usize, usize) {
        h.conn
            .query_row(
                "SELECT size_bytes, encrypted_bytes FROM stage_slices
                  WHERE stage_set_id = ?1 AND slice_number = ?2",
                params![set, n],
                |r| Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize)),
            )
            .unwrap()
    }

    /// The plaintext of the slice the tape holds at tape file `position`,
    /// opened with the tenant's key.
    fn plain_at(h: &Home, label: &str, set: i64, n: i64) -> Vec<u8> {
        let position = h.position_in(label, set, n);
        let (_, encrypted) = slice_sizes(h, set, n);
        let identities = keys::load_tenant_identities(&h.conn, &h.paths.keys_dir, TENANT).unwrap();
        let cipher = &h.tapes[label].files[position][..encrypted];
        let decryptor = age::Decryptor::new(cipher).unwrap();
        let mut reader = decryptor
            .decrypt(identities.iter().map(|i| i as &dyn age::Identity))
            .unwrap();
        let mut plain = Vec::new();
        reader.read_to_end(&mut plain).unwrap();
        plain
    }

    /// Replace slice `n` of `set` as cartridge `label` holds it with `plain`
    /// encrypted afresh to the recipients the staging used: a DIFFERENT age
    /// file (its own file key, nonce and header) of exactly the length the
    /// catalog recorded, with the block padding the original had. Every chunk
    /// of it authenticates under the tenant's identity; its ciphertext hash
    /// is not the recorded one.
    fn replace_slice(h: &mut Home, label: &str, set: i64, n: i64, plain: &[u8]) {
        let (size, encrypted) = slice_sizes(h, set, n);
        assert_eq!(plain.len(), size, "the same plaintext length");
        let recipients: Vec<String> = serde_json::from_str(
            &h.conn
                .query_row(
                    "SELECT key_fingerprints FROM stage_sets WHERE id = ?1",
                    params![set],
                    |r| r.get::<_, String>(0),
                )
                .unwrap(),
        )
        .unwrap();
        // header + the 16-byte nonce + each chunk and its 16-byte tag.
        let chunks = plain.len().div_ceil(CHUNK as usize).max(1);
        let header_len = encrypted - 16 - plain.len() - 16 * chunks;
        let mut cipher =
            crate::staging::encrypt_data_with_header_len(plain, &recipients, header_len);
        assert_eq!(cipher.len(), encrypted, "the recorded length");
        let position = h.position_in(label, set, n);
        let stored = h.tapes[label].files[position].len();
        cipher.resize(stored, 0);
        h.tapes.get_mut(label).unwrap().files[position] = cipher;
    }

    /// `plain` with the byte at `at` changed.
    fn one_byte_off(plain: &[u8], at: usize) -> Vec<u8> {
        let mut out = plain.to_vec();
        out[at] ^= 0x55;
        out
    }

    /// Forget the plaintext checksum recorded for slice `n` of `set`, as a
    /// catalog that never had one.
    fn forget_plain_sha(h: &Home, set: i64, n: i64) {
        h.conn
            .execute(
                "UPDATE stage_slices SET sha256_plain = '' WHERE stage_set_id = ?1 AND slice_number = ?2",
                params![set, n],
            )
            .unwrap();
    }

    // ── two stagings of one version ────────────────────────────────────

    /// A second stage set of the version, as `stage clean --unit alpha
    /// --version 1 --force` then `stage create alpha --version 1` make one:
    /// different slice sizes, so different slices and hashes. Slice
    /// substitution matches none of them; only the file table can.
    fn restage(h: &mut Home, slice_size: &str) -> i64 {
        let unit_id: i64 = h
            .conn
            .query_row("SELECT id FROM units WHERE name = ?1", params![UNIT], |r| {
                r.get(0)
            })
            .unwrap();
        crate::staging::clean::clean_staging(
            &h.conn,
            &h.config,
            true,
            crate::staging::clean::CleanScope::UnitVersion {
                unit_id,
                version: 1,
            },
        )
        .unwrap();
        h.config.defaults.slice_size = slice_size.to_string();
        let snapshot_id: i64 = h
            .conn
            .query_row(
                "SELECT id FROM snapshots WHERE unit_id = ?1 AND version = 1",
                params![unit_id],
                |r| r.get(0),
            )
            .unwrap();
        crate::staging::stage_create_admitting(
            &h.conn,
            &h.paths,
            &h.config,
            snapshot_id,
            true,
            crate::staging::Admission::Restage,
            None,
            &mut std::io::sink(),
        )
        .unwrap_or_else(|e| panic!("stage create alpha --version 1: {e}"))
    }

    /// Version 1 staged at 256K and written to A, then staged again at 192K
    /// and written to B. Returns the home and the two stage sets.
    fn two_stagings() -> (Home, i64, i64) {
        let mut h = Home::new("256K", None, 2);
        h.unit(TENANT, UNIT, &tree());
        h.stage(UNIT);
        h.volume(A);
        h.volume(B);
        h.write(A);
        let one = h.stage_set(UNIT, 1);
        let two = restage(&mut h, "192K");
        assert_ne!(one, two, "a second stage set");
        h.write(B);
        assert_ne!(
            h.num_slices(UNIT),
            slices_of(&h, one),
            "different slicing: {} and {}",
            h.num_slices(UNIT),
            slices_of(&h, one)
        );
        (h, one, two)
    }

    fn slices_of(h: &Home, set: i64) -> i64 {
        h.conn
            .query_row(
                "SELECT COUNT(*) FROM stage_slices WHERE stage_set_id = ?1",
                params![set],
                |r| r.get(0),
            )
            .unwrap()
    }

    // ── a damaged slice is read from another copy ──────────────────────

    /// THE acceptance test of the slice half: two copies, a different slice
    /// bad on each, `restore unit` comes back whole with ONE cartridge swap:
    /// the operator is asked once, for the copy that holds the slice the
    /// first could not give, and that cartridge then serves the rest. Plain
    /// and compressed (each slice compressed alone) alike.
    #[test]
    fn two_copies_with_different_bad_slices_restore_whole_with_one_swap() {
        for compression in [None, Some("zstd")] {
            let mut h = copies("256K", compression, &[A, B]);
            let slices = h.num_slices(UNIT);
            // A is bad at slice 3; B at slice 2, which A gives whole, so B's
            // damage is never reached.
            damage(&mut h, A, 3, 0);
            damage(&mut h, B, 2, 0);
            let mut bay = h.bay(A, &[Reply::Load(B)]);
            let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
            let report = r.unwrap_or_else(|e| panic!("compression={compression:?}: {e}"));
            assert_restored(&h, UNIT, dest.path(), &[]);

            assert_eq!(
                bay.asked_labels(),
                [B],
                "compression={compression:?}: one swap prompt"
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
            let contacts = contacts(&h);
            assert_eq!(contacts.len(), 2, "{contacts:?}");
            assert_eq!((contacts[0].0.as_str(), contacts[0].1.as_str()), (A, "ok"));
            assert_eq!((contacts[1].0.as_str(), contacts[1].1.as_str()), (B, "ok"));
            assert!(
                detail(&contacts[0]).contains("slice 3 damaged here"),
                "{contacts:?}"
            );
            assert!(
                detail(&contacts[1]).contains("supplied slice 3"),
                "{contacts:?}"
            );
            // And the restore is one record, under the first contact.
            let rows = crate::volume::restore_record::rows(&h.conn).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].outcome, "ok");
            assert_eq!(rows[0].volume_label.as_deref(), Some(A));
            assert_eq!(rows[0].slices_read, Some(slices));
        }
    }

    /// And the rule that keeps swaps few: A is bad at slice 2 and B at slice 4,
    /// so the cartridge asked for at slice 2 (B) is bad at 4 and A is asked for
    /// again: two swaps for two bad slices, never one per slice read.
    #[test]
    fn each_bad_slice_run_costs_one_swap_and_a_cartridge_is_kept_while_it_serves() {
        let mut h = copies("256K", None, &[A, B]);
        damage(&mut h, A, 2, 1);
        damage(&mut h, B, 4, 0);
        let mut bay = h.bay(A, &[Reply::Load(B), Reply::Load(A)]);
        let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        let report = r.unwrap();
        assert_restored(&h, UNIT, dest.path(), &[]);
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
        assert_eq!(contacts(&h).len(), 3);

        // Consecutive bad slices on the first cartridge are ONE swap.
        let mut h = copies("256K", None, &[A, B]);
        damage(&mut h, A, 2, 0);
        damage(&mut h, A, 3, 0);
        damage(&mut h, A, 4, 0);
        let mut bay = h.bay(A, &[Reply::Load(B)]);
        let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        r.unwrap();
        assert_restored(&h, UNIT, dest.path(), &[]);
        assert_eq!(
            bay.asked_labels(),
            [B],
            "three bad slices in a row, one swap"
        );
    }

    /// A slice damaged in a DIFFERENT chunk on each copy is whole from the two
    /// together: A gives chunks up to its bad one, B carries on and gives
    /// everything A could not, then A is asked for again for what lies past B's
    /// bad chunk, because the bytes already delivered are the same bytes on
    /// every copy.
    #[test]
    fn a_slice_bad_in_different_chunks_on_each_copy_is_whole_from_both() {
        let mut h = copies("256K", None, &[A, B]);
        // A slice of at least three chunks, the first and the third bad on
        // the two copies.
        let (size, _) = slice_sizes(&h, h.stage_set(UNIT, 1), 2);
        assert!(size as u64 > 2 * CHUNK, "slice 2 holds {size} bytes");
        damage(&mut h, A, 2, 0);
        damage(&mut h, B, 2, 2);
        let mut bay = h.bay(A, &[Reply::Load(B), Reply::Load(A)]);
        let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        r.unwrap_or_else(|e| panic!("{e}"));
        assert_restored(&h, UNIT, dest.path(), &[]);
        assert_eq!(bay.asked_labels(), [B, A]);
    }

    /// No terminal, no swap: the read refuses by name, listing the volumes that
    /// hold the slice, and asks nothing. What was restored before stays.
    #[test]
    fn without_a_terminal_the_restore_refuses_by_name_and_lists_the_holders() {
        let mut h = copies("256K", None, &[A, B]);
        damage(&mut h, A, 3, 0);
        let mut bay = h.bay(A, &[Reply::Load(B)]);
        bay.attended = false;
        let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        let err = r.unwrap_err().to_string();
        assert!(err.contains("slice 3"), "{err}");
        assert!(err.contains(&format!("\"{UNIT}\"")), "{err}");
        assert!(err.contains(B), "the holder is named: {err}");
        assert!(err.contains("swaps no cartridge"), "{err}");
        assert!(err.contains(&format!("--from {B}")), "{err}");
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
        let rows = crate::volume::restore_record::rows(&h.conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, "failed");
        assert!(rows[0]
            .error
            .as_deref()
            .unwrap()
            .contains("swaps no cartridge"));
    }

    /// The operator declines the swap: the slice is read from the cartridge in
    /// the drive with a hole where its chunk fails to authenticate. The
    /// restore names exactly the files the hole touches and fails; every
    /// other file is whole, and the files it names are not written at all.
    #[test]
    fn a_declined_swap_leaves_a_hole_and_the_restore_names_the_damaged_files() {
        let mut h = copies("256K", None, &[A, B]);
        damage(&mut h, A, 3, 0);
        let mut bay = h.bay(A, &[Reply::Skip]);
        let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        let err = r.expect_err("a hole nobody fills").to_string();
        assert!(err.contains("INCOMPLETE"), "{err}");
        assert!(err.contains(&format!("slice 3 on {A}")), "{err}");
        assert!(
            err.contains("unproven"),
            "a slice with holes is said to be unproven as well: {err}"
        );
        let lost = lost_paths(&err);
        let set = h.stage_set(UNIT, 1);
        assert_eq!(
            lost,
            lost_in_chunk(&h, set, SLICE_256K, 3, 0),
            "exactly the members the hole touches: {err}"
        );
        assert!(
            lost.len() < h.rows(UNIT).len() / 2,
            "a chunk costs some files, not all: {lost:?}"
        );
        assert_all_but(&h, dest.path(), &lost);
        // The one swap was offered, and the cartridge never left.
        assert_eq!(bay.asked_labels(), [B]);
        let contacts = contacts(&h);
        assert_eq!(
            contacts.last().unwrap().0,
            A,
            "the first cartridge came back: {contacts:?}"
        );
    }

    /// A slice bad in the SAME chunk on every copy cannot be had whole: after
    /// every copy has been tried, the chunk is a hole, the files it fell in are
    /// named, and the restore fails.
    #[test]
    fn a_chunk_bad_on_every_copy_is_a_hole_after_every_copy_was_tried() {
        let mut h = copies("256K", None, &[A, B]);
        damage(&mut h, A, 3, 1);
        damage(&mut h, B, 3, 1);
        let mut bay = h.bay(A, &[Reply::Load(B)]);
        let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        let err = r.unwrap_err().to_string();
        assert!(err.contains("INCOMPLETE"), "{err}");
        assert!(err.contains("slice 3"), "{err}");
        assert_eq!(
            bay.asked_labels(),
            [B],
            "the only other copy was tried once"
        );
        let lost = lost_paths(&err);
        assert_eq!(
            lost,
            lost_in_chunk(&h, h.stage_set(UNIT, 1), SLICE_256K, 3, 1),
            "{err}"
        );
        assert_all_but(&h, dest.path(), &lost);
    }

    /// The acceptance test of the chunk half, with real tar and real age and
    /// no other copy: one corrupted byte inside a slice costs only the files
    /// that touch its 64 KiB chunk. The restore names them, fails, and
    /// nothing unauthenticated is written: the files that are not named are
    /// byte for byte whole, and the damaged ones are not in the destination.
    #[test]
    fn a_single_corrupted_byte_costs_only_the_files_in_its_chunk() {
        for compression in [None, Some("zstd")] {
            let mut h = copies("256K", compression, &[A]);
            damage(&mut h, A, 2, 1);
            let mut bay = h.bay(A, &[]);
            let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
            let err = r.unwrap_err().to_string();
            assert!(err.contains("INCOMPLETE"), "{err}");
            let lost = lost_paths(&err);
            assert!(!lost.is_empty(), "{err}");
            let slice_two = members_of_slice(&h, h.stage_set(UNIT, 1), 2);
            assert!(lost.is_subset(&slice_two), "only slice 2's: {lost:?}");
            if compression.is_none() {
                assert!(
                    lost.len() <= 3,
                    "one 64 KiB chunk of 30 KB files costs a few files: {lost:?}"
                );
                assert_eq!(
                    lost,
                    lost_in_chunk(&h, h.stage_set(UNIT, 1), SLICE_256K, 2, 1),
                    "{err}"
                );
            } else {
                // Nothing after a bad chunk of a compressed slice decodes: the
                // files from the chunk to the slice's end are left out.
                let from = lost_in_chunk(&h, h.stage_set(UNIT, 1), SLICE_256K, 2, 1);
                assert!(from.is_subset(&lost), "{lost:?} holds {from:?}");
                // A suffix of the slice's members in stream order: the frame
                // decodes in blocks, so what lies before the bad block is
                // whole and nothing after it decodes.
                let order: Vec<String> = h
                    .rows(UNIT)
                    .into_iter()
                    .filter(|r| r.slice == 2)
                    .map(|r| r.path)
                    .collect();
                let first = order.iter().position(|p| lost.contains(p)).unwrap();
                assert!(
                    order[first..].iter().all(|p| lost.contains(p)),
                    "{lost:?} is not a suffix of {order:?}"
                );
            }
            assert_all_but(&h, dest.path(), &lost);
            let rows = crate::volume::restore_record::rows(&h.conn).unwrap();
            assert_eq!(rows[0].outcome, "failed");
            assert!(rows[0].error.as_deref().unwrap().contains("INCOMPLETE"));
            assert!(
                !dest.path().join(SCRATCH_NAME).exists(),
                "no scratch directory is left"
            );
        }
    }

    // ── what proves a slice whole (the second review's finding) ────────
    //
    // An age chunk's tag proves it was encrypted under some key the tenant's
    // identity unwraps; any other age file to that tenant authenticates in
    // full. These tests put such a file where the catalog expects a slice.

    /// THE finding: a different age file at the right position, of the right
    /// length, authenticates in full and does not hash. It is not the slice:
    /// it is dropped, the slice is read from the other copy, and the restore
    /// is whole with the substitution said.
    #[test]
    fn a_wrong_age_file_at_the_right_position_is_replaced_from_another_copy() {
        let mut h = copies("256K", None, &[A, B]);
        let set = h.stage_set(UNIT, 1);
        let (size, _) = slice_sizes(&h, set, 3);
        replace_slice(&mut h, A, set, 3, &unlike(size, 4242));
        let mut bay = h.bay(A, &[Reply::Load(B)]);
        let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        let report = r.unwrap_or_else(|e| panic!("{e}"));
        assert_restored(&h, UNIT, dest.path(), &[]);
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

    /// The same finding with a file that is nearly the slice: its plaintext
    /// is the slice's with one byte of a file's data changed. The wrong byte
    /// is never fed to tar (the slice is dropped for the copy that proves
    /// out), so the restored file is the archived one.
    #[test]
    fn a_plaintext_one_byte_off_is_replaced_from_another_copy_and_never_reaches_tar() {
        let mut h = copies("256K", None, &[A, B]);
        let set = h.stage_set(UNIT, 1);
        let plain = plain_at(&h, A, set, 3);
        let (path, at) = byte_in_a_member(&h, set, 3);
        replace_slice(&mut h, A, set, 3, &one_byte_off(&plain, at));
        let mut bay = h.bay(A, &[Reply::Load(B)]);
        let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        let report = r.unwrap_or_else(|e| panic!("{e}"));
        assert_restored(&h, UNIT, dest.path(), &[]);
        assert_eq!(
            fs::read(dest.path().join(&path)).unwrap(),
            fs::read(h.source(UNIT).join(&path)).unwrap(),
            "{path} is the archived file, not the one a byte off"
        );
        assert_eq!(bay.asked_labels(), [B]);
        assert_eq!(report.substitutions.len(), 1, "{report:?}");
        assert_eq!(report.substitutions[0].slice_number, 3);
    }

    /// A byte inside the data of a 30,000-byte member that begins in slice
    /// `n` of `set`: its path and its offset in the slice's cut. The last
    /// block of the member's data is 304 bytes of it and then padding, so ten
    /// bytes into it is the file's own.
    fn byte_in_a_member(h: &Home, set: i64, n: u64) -> (String, usize) {
        let rows = stage_files::load(&h.conn, set).unwrap();
        let begins = filetable::slice_start(&rows, SLICE_256K, n).unwrap();
        let row = rows
            .iter()
            .filter(|r| r.slice == n && r.size == 30_000)
            .nth(1)
            .expect("a member of 30,000 bytes in the slice");
        (row.path.clone(), (row.end - 512 + 10 - begins) as usize)
    }

    /// Where the recorded plaintext checksum cannot say, a copy that hashes
    /// as the catalog recorded is the slice: the first copy's age file
    /// authenticates in full and is a byte off, nothing proves it wrong, and
    /// the other copy's ciphertext does prove itself.
    #[test]
    fn an_unproven_slice_gives_way_to_a_copy_that_hashes_as_recorded() {
        let mut h = copies("256K", None, &[A, B]);
        let set = h.stage_set(UNIT, 1);
        forget_plain_sha(&h, set, 3);
        let plain = plain_at(&h, A, set, 3);
        let (_, at) = byte_in_a_member(&h, set, 3);
        replace_slice(&mut h, A, set, 3, &one_byte_off(&plain, at));
        let mut bay = h.bay(A, &[Reply::Load(B)]);
        let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        let report = r.unwrap_or_else(|e| panic!("{e}"));
        assert_restored(&h, UNIT, dest.path(), &[]);
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
    /// (A bad late, B bad early): the two together are whole and proven by
    /// the slice's plaintext checksum, and the operator is asked for B once.
    /// A slice is always held in memory, so nothing is ever left "unproven"
    /// for want of a copy to hash it, and nobody is sent back to A.
    #[test]
    fn a_slice_bad_late_on_the_first_copy_and_early_on_the_second_is_proven_by_its_plaintext() {
        let mut h = copies("256K", None, &[A, B]);
        let (size, _) = slice_sizes(&h, h.stage_set(UNIT, 1), 2);
        assert!(size as u64 > 2 * CHUNK, "slice 2 holds {size} bytes");
        damage(&mut h, A, 2, 2);
        damage(&mut h, B, 2, 0);
        let mut bay = h.bay(A, &[Reply::Load(B), Reply::Load(A)]);
        let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        r.unwrap_or_else(|e| panic!("{e}"));
        assert_restored(&h, UNIT, dest.path(), &[]);
        assert_eq!(bay.asked_labels(), [B]);
        assert!(
            !contacts(&h)
                .iter()
                .any(|c| detail(c).contains("slice 2 unproven")),
            "{:?}",
            contacts(&h)
        );
    }

    /// With no other copy: the wrong file is no use, so the slice is a hole
    /// from end to end, the restore says why in the words of the finding,
    /// and the files that were not in that slice are whole. Not a byte of the
    /// wrong file is anywhere in what came back, and the restore is never a
    /// success.
    #[test]
    fn a_wrong_age_file_on_the_only_copy_is_a_whole_slice_of_hole() {
        let mut h = copies("256K", None, &[A]);
        let set = h.stage_set(UNIT, 1);
        let (size, _) = slice_sizes(&h, set, 3);
        let wrong = unlike(size, 4242);
        replace_slice(&mut h, A, set, 3, &wrong);
        let mut bay = h.bay(A, &[]);
        let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        let err = r.expect_err("a wrong file is no slice").to_string();
        assert!(err.contains("INCOMPLETE"), "{err}");
        assert!(err.contains(&format!("slice 3 on {A}")), "{err}");
        assert!(
            err.contains("does not match the checksum recorded for the slice"),
            "{err}"
        );
        assert!(err.contains("unproven"), "{err}");
        let lost = lost_paths(&err);
        assert_eq!(
            lost,
            members_of_slice(&h, set, 3),
            "every member of slice 3: {err}"
        );
        assert_all_but(&h, dest.path(), &lost);
        // Not a byte of the wrong file is anywhere in what came back: sample
        // 64-byte runs of it and look for each.
        let samples: Vec<&[u8]> = (0..8)
            .map(|k| &wrong[k * (wrong.len() / 9)..][..64])
            .collect();
        let mut looked = 0;
        for (path, _) in listing(dest.path()) {
            let Ok(got) = fs::read(dest.path().join(&path)) else {
                continue;
            };
            looked += got.len();
            for sample in &samples {
                assert!(
                    !got.windows(64).any(|w| w == *sample),
                    "{path} carries bytes of the wrong file"
                );
            }
        }
        assert!(looked > 0, "positive control: files were searched");
    }

    /// The only copy of a slice that is a byte off: with the plaintext
    /// checksum recorded it is not the slice, and the slice is a hole from
    /// end to end (every member left out, though only one is wrong). With no
    /// checksum recorded it is unproven, delivered in full, and the files in
    /// it decide, each by its own recorded sha256: only the one a byte off is
    /// left out.
    #[test]
    fn a_slice_one_byte_off_on_the_only_copy_is_judged_by_its_checksum_or_by_its_files() {
        for recorded in [true, false] {
            let mut h = copies("256K", None, &[A]);
            let set = h.stage_set(UNIT, 1);
            let plain = plain_at(&h, A, set, 3);
            let (path, at) = byte_in_a_member(&h, set, 3);
            replace_slice(&mut h, A, set, 3, &one_byte_off(&plain, at));
            if !recorded {
                forget_plain_sha(&h, set, 3);
            }
            let mut bay = h.bay(A, &[]);
            let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
            let err = r
                .expect_err(&format!("recorded={recorded}: not the slice"))
                .to_string();
            assert!(
                err.contains(&format!("slice 3 on {A}")),
                "recorded={recorded}: {err}"
            );
            assert!(err.contains("unproven"), "recorded={recorded}: {err}");
            let lost = lost_paths(&err);
            if recorded {
                assert!(
                    err.contains("does not match the checksum recorded for the slice"),
                    "{err}"
                );
                assert_eq!(lost, members_of_slice(&h, set, 3), "{err}");
            } else {
                assert_eq!(
                    lost,
                    BTreeSet::from([path.clone()]),
                    "only the member a byte off: {err}"
                );
            }
            assert_all_but(&h, dest.path(), &lost);
        }
    }

    /// The other side of the same check: a different age file whose
    /// PLAINTEXT is the slice (re-encrypted: other key, header, nonce, so the
    /// ciphertext hash is not the recorded one) IS the slice, because the
    /// recorded plaintext checksum says so. Nothing is unproven, nobody is
    /// asked for another cartridge, and the restore is whole. The same slice
    /// with no plaintext checksum recorded (a catalog that never had one) is
    /// unproven (still restored, since every file in it checks out) and the
    /// cartridge's record says so.
    #[test]
    fn a_reencrypted_slice_is_whole_by_its_plaintext_checksum_and_unproven_without_one() {
        for recorded in [true, false] {
            let mut h = copies("256K", None, &[A]);
            let set = h.stage_set(UNIT, 1);
            let plain = plain_at(&h, A, set, 3);
            replace_slice(&mut h, A, set, 3, &plain);
            if !recorded {
                forget_plain_sha(&h, set, 3);
            }
            let mut bay = h.bay(A, &[]);
            let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
            let report = r.unwrap_or_else(|e| panic!("recorded={recorded}: {e}"));
            assert_restored(&h, UNIT, dest.path(), &[]);
            assert!(report.substitutions.is_empty(), "{report:?}");
            let said = detail(&contacts(&h)[0]).to_string();
            assert_eq!(
                said.contains("slice 3 unproven"),
                !recorded,
                "recorded={recorded}: {said}"
            );
        }
    }

    /// One slice is one age file. Copy A is good for slice 2's first two
    /// chunks and bad after; copy B is a clean, complete age file of the same
    /// plaintext, but another file, with its own header and nonce. Its
    /// chunks authenticate under its own key and prove nothing about the part
    /// A gave, so they are never mixed into the same slice: the restore
    /// refuses, by name.
    #[test]
    fn chunks_of_two_age_files_are_never_mixed_into_one_slice() {
        let mut h = copies("256K", None, &[A, B]);
        let set = h.stage_set(UNIT, 1);
        let (size, _) = slice_sizes(&h, set, 2);
        assert!(size as u64 > 2 * CHUNK, "slice 2 holds {size} bytes");
        damage(&mut h, A, 2, 2);
        let plain = plain_at(&h, B, set, 2);
        replace_slice(&mut h, B, set, 2, &plain);
        let mut bay = h.bay(A, &[Reply::Load(B)]);
        let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        let err = r.expect_err("mixed chunks must be refused").to_string();
        assert!(err.contains("slice 2"), "{err}");
        assert!(
            err.contains("different age file") && err.contains("never mixed"),
            "{err}"
        );
        assert_eq!(bay.asked_labels(), [B]);
        assert!(
            !dest.path().join(SCRATCH_NAME).exists(),
            "no decrypted slice is left behind"
        );
    }

    /// A wrong cartridge in the drive when the right one is asked for is
    /// refused by the identity check (File 0 names another volume) and the
    /// operator is asked again, naming the cartridge; and a substitute whose
    /// File 0 requires a feature this tapectl does not know is refused by the
    /// same text `restore raw` gives (#476).
    #[test]
    fn a_substitute_must_pass_the_identity_check_too() {
        let mut h = copies("256K", None, &[A, B]);
        // B requires a feature nobody has.
        let old = h.tapes[B].files[0].clone();
        h.tapes.get_mut(B).unwrap().files[0] = crate::volume::format::rewrite_file0(
            &old,
            "\nrequires = []\n",
            "\nrequires = [\"unknown-feature\"]\n",
        );
        damage(&mut h, A, 3, 0);
        // Enter without changing anything leaves A in the drive: wrong tape;
        // then B is loaded: refused for its requires; then give up.
        let mut bay = h.bay(A, &[Reply::Enter, Reply::Load(B), Reply::Stop]);
        let (r, _dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        let err = r.unwrap_err().to_string();
        assert!(err.contains("stopped at the operator's request"), "{err}");
        assert_eq!(
            bay.asked_labels(),
            [B, B, B],
            "asked again after each refusal"
        );
        // Every refused arrival is a contact, closed failed.
        let contacts = contacts(&h);
        let failed: Vec<_> = contacts.iter().filter(|c| c.1 == "failed").collect();
        assert_eq!(failed.len(), 2, "{contacts:?}");
        assert!(detail(failed[0]).contains("wrong tape"), "{contacts:?}");
        assert!(
            detail(failed[1]).contains("unknown-feature"),
            "{contacts:?}"
        );
    }

    // ── item 102: a lost file is filled from another copy of the same version ──

    /// THE acceptance test of item 102: the only copy of a stage set is
    /// damaged where no slice of it can be had, and the lost files come whole
    /// from the other staging of the same version, asked for by label and
    /// serial, verified against the recorded checksums; and the restore is
    /// whole.
    #[test]
    fn files_lost_to_a_damaged_slice_are_filled_from_another_staging_of_the_version() {
        let (mut h, one, _two) = two_stagings();
        damage_in(&mut h, A, one, 3, 1);
        let mut bay = h.bay(A, &[Reply::Load(B)]);
        let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        let report = r.unwrap_or_else(|e| panic!("{e}"));
        assert_restored(&h, UNIT, dest.path(), &[]);
        assert_eq!(bay.asked_labels(), [B]);
        assert!(
            bay.asked[0].1.contains("damaged or missing")
                && bay.asked[0].1.contains("the same files"),
            "{:?}",
            bay.asked
        );
        let hole = lost_in_chunk(&h, one, SLICE_256K, 3, 1);
        assert_eq!(
            report
                .filled
                .iter()
                .map(|(p, _)| p.clone())
                .collect::<BTreeSet<_>>(),
            hole,
            "exactly what the hole cost is filled"
        );
        for (path, from) in &report.filled {
            assert!(
                from.starts_with(B) && from.contains("sha256 matches"),
                "{path}: {from}"
            );
        }
        // A's slices had no other copy, so none was substituted.
        assert!(report.substitutions.is_empty());
        let contacts = contacts(&h);
        assert_eq!(contacts.len(), 2, "{contacts:?}");
        assert!(
            detail(&contacts[1]).contains("supplied"),
            "the copy's contact says what it supplied: {contacts:?}"
        );
        // And nothing of the fill is left behind.
        assert!(!dest.path().join(SCRATCH_NAME).exists());
    }

    /// Only the slices that hold the lost files are read from the other
    /// staging: its own file table says which, and there is no catalogue
    /// slice to read. (The old "asks nobody" half, a fill from the cartridge
    /// already in the drive, cannot be made through the real commands: a
    /// sealed volume takes no second write, and `stage create --version`
    /// refuses a re-stage while the first stage set has live slices, so two
    /// stagings of one version never share a cartridge.)
    #[test]
    fn a_fill_reads_only_the_slices_of_the_other_staging_that_hold_the_files() {
        let (mut h, one, two) = two_stagings();
        damage_in(&mut h, A, one, 3, 1);
        let (mut bay, fakes) = fake_bay(&h, A, &[Reply::Load(B)]);
        let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        let report = r.unwrap_or_else(|e| panic!("{e}"));
        assert_restored(&h, UNIT, dest.path(), &[]);
        assert_eq!(bay.asked_labels(), [B]);
        assert!(!report.filled.is_empty(), "{report:?}");
        let total = slices_of(&h, two);
        let first_slice = h.position_in(B, two, 1);
        let reads: BTreeSet<u64> = fakes[B]
            .ops()
            .iter()
            .filter_map(|op| match op {
                Op::Read(n) if (*n as usize) >= first_slice => {
                    Some((*n as usize - first_slice) as u64 + 1)
                }
                _ => None,
            })
            .filter(|n| *n as i64 <= total)
            .collect();
        // The slices of the second stage set that hold the lost files, by
        // ITS table and ITS slice size, and no others.
        let rows = stage_files::load(&h.conn, two).unwrap();
        let lost: Vec<&str> = report.filled.iter().map(|(p, _)| p.as_str()).collect();
        let wanted: BTreeSet<u64> =
            filetable::slices_of(filetable::select(&rows, &lost), 192 * 1024);
        assert_eq!(reads, wanted, "the other staging's slices read");
        assert!(
            (reads.len() as i64) < total,
            "positive control: only some of the other staging's {total} slices are read: {reads:?}"
        );
    }

    /// A file whose only other copy has different bytes than the version
    /// recorded is reported, not written: the checksum is the arbiter. The
    /// two stagings of one version cannot disagree through `stage create`
    /// (it refuses a re-stage whose hashes differ from the snapshot's), so
    /// the disagreement is made in the catalog: the restoring stage set's
    /// table records another sha256 for two files than the other staging's
    /// bytes have.
    #[test]
    fn a_copy_that_does_not_match_the_recorded_checksum_is_refused_and_said() {
        let (h, one, _two) = two_stagings();
        let unit_id: i64 = h
            .conn
            .query_row("SELECT id FROM units WHERE name = ?1", params![UNIT], |r| {
                r.get(0)
            })
            .unwrap();
        let tampered = ["alpha-03.bin", "alpha-07.bin"];
        for path in tampered {
            let changed = h
                .conn
                .execute(
                    "UPDATE stage_files SET sha256 = zeroblob(32)
                      WHERE stage_set_id = ?1
                        AND path_id = (SELECT id FROM paths WHERE unit_id = ?2 AND path = ?3)",
                    params![one, unit_id, path],
                )
                .unwrap();
            assert_eq!(changed, 1, "{path} is in the table");
        }
        let mut bay = h.bay(A, &[Reply::Load(B)]);
        let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        let err = r.unwrap_err().to_string();
        assert!(err.contains("INCOMPLETE"), "{err}");
        assert!(
            err.contains("does not match the version's recorded checksum"),
            "{err}"
        );
        assert!(err.contains("not kept"), "{err}");
        let lost = lost_paths(&err);
        assert_eq!(
            lost,
            tampered
                .iter()
                .map(|p| p.to_string())
                .collect::<BTreeSet<_>>(),
            "{err}"
        );
        // The other copy was asked for and read: its files are not written.
        assert_eq!(bay.asked_labels(), [B]);
        assert_all_but(&h, dest.path(), &lost);
        assert!(
            h.source(UNIT).join("alpha-03.bin").exists()
                && !dest.path().join("alpha-03.bin").exists(),
            "the refused copy was not written"
        );
    }

    /// Without a person at the terminal the fill refuses by name too, listing
    /// the holders of the other staging.
    #[test]
    fn an_unattended_fill_refuses_by_name_listing_the_holders() {
        let (mut h, one, _two) = two_stagings();
        damage_in(&mut h, A, one, 3, 1);
        let mut bay = h.bay(A, &[Reply::Load(B)]);
        bay.attended = false;
        let (r, _dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        let err = r.unwrap_err().to_string();
        assert!(err.contains("INCOMPLETE"), "{err}");
        assert!(err.contains(B), "the holder is named: {err}");
        assert!(err.contains("swaps no cartridge without a person"), "{err}");
        assert!(bay.asked.is_empty());
    }

    /// Another VERSION is never a source, even where the file is unchanged.
    #[test]
    fn another_version_is_never_a_source() {
        let mut h = Home::new("256K", None, 1);
        h.unit(TENANT, UNIT, &tree());
        h.stage(UNIT);
        h.volume(A);
        h.volume(B);
        h.write(A);
        // Version 2: one file changed, every other the same, written to B.
        fs::write(
            h.source(UNIT).join("sub/deep.txt"),
            b"deeper still, in version two\n",
        )
        .unwrap();
        h.stage(UNIT);
        h.write(B);
        let versions: i64 = h
            .conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(versions, 2);
        let one = h.stage_set(UNIT, 1);
        damage_in(&mut h, A, one, 3, 1);
        let mut bay = h.bay(A, &[Reply::Load(B)]);
        let (r, _dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        let err = r.unwrap_err().to_string();
        assert!(err.contains("INCOMPLETE"), "{err}");
        assert!(
            bay.asked.is_empty(),
            "nothing of another version is asked for"
        );
    }

    /// `restore file` fills its one file from the other staging too, and
    /// places it only once it is whole.
    #[test]
    fn restore_file_is_filled_from_another_staging_and_placed_whole() {
        let (mut h, one, _two) = two_stagings();
        damage_in(&mut h, A, one, 3, 1);
        // A file the damaged chunk falls in.
        let file = lost_in_chunk(&h, one, SLICE_256K, 3, 1)
            .into_iter()
            .find(|p| p.starts_with("alpha-"))
            .expect("a file lost to the chunk");

        // Positive control: with nobody to ask, the one file is lost.
        let mut bay = h.bay(A, &[Reply::Skip]);
        let (r, dest) = h.restore(
            UNIT,
            What::File(&file),
            A,
            &RestoreOptions::default(),
            &mut bay,
        );
        r.expect_err("the one file is lost without the other copy");
        assert!(!dest.path().join(&file).exists(), "not placed short");

        let mut bay = h.bay(A, &[Reply::Load(B)]);
        let (r, dest) = h.restore(
            UNIT,
            What::File(&file),
            A,
            &RestoreOptions::default(),
            &mut bay,
        );
        let report = r.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            fs::read(dest.path().join(&file)).unwrap(),
            fs::read(h.source(UNIT).join(&file)).unwrap()
        );
        assert_eq!(report.filled.len(), 1);
        assert_eq!(bay.asked_labels(), [B]);
        assert!(!dest.path().join(SCRATCH_NAME).exists());
    }

    // ── cost: what a swap costs the tapes (issue #416's discipline, #480) ──

    /// The pin for a one-swap restore: the first cartridge is read forward
    /// to the slice that fails and that slice to its end (one rewind, none
    /// after) then released; the second is opened at BOT, its File 0 looked at
    /// once for the identity check, and read forward from the failed slice
    /// to the end. No cartridge is rewound twice, no file is read twice, and
    /// the first is not asked for again.
    #[test]
    fn a_one_swap_restore_moves_each_tape_once_and_forward_only() {
        let mut h = copies("256K", None, &[A, B]);
        damage(&mut h, A, 3, 0);
        let slices = h.num_slices(UNIT);
        let (mut bay, fakes) = fake_bay(&h, A, &[Reply::Load(B)]);
        let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        r.unwrap();
        assert_restored(&h, UNIT, dest.path(), &[]);
        assert_eq!(bay.asked_labels(), [B]);
        assert_eq!(bay.releases, 1, "the first cartridge was released once");

        let slice = |label: &str, n: i64| h.position(label, UNIT, n) as u32;
        let read_files = |label: &str| -> Vec<u32> {
            fakes[label]
                .ops()
                .iter()
                .filter_map(|op| match op {
                    Op::Read(n) => Some(*n),
                    _ => None,
                })
                .collect()
        };
        // A: File 0 (the contact check), then slices 1..=3, the third bad,
        // read through to the filemark for its hash.
        let ops_a = fakes[A].ops();
        assert_eq!(ops_a[..2], [Op::Rewind, Op::ReadHead(0)], "{ops_a:?}");
        assert_eq!(
            read_files(A),
            (1..=3).map(|n| slice(A, n)).collect::<Vec<_>>(),
            "the first cartridge, forward to the bad slice: {ops_a:?}"
        );
        // B: opened at BOT, File 0 once, then slice 3 on to the last.
        let ops_b = fakes[B].ops();
        assert_eq!(ops_b[..2], [Op::Rewind, Op::ReadHead(0)], "{ops_b:?}");
        assert_eq!(
            read_files(B),
            (3..=slices).map(|n| slice(B, n)).collect::<Vec<_>>(),
            "B is read from the failed slice to the last, each once: {ops_b:?}"
        );
        assert_eq!(fakes[B].rewinds(), 1, "{ops_b:?}");
        assert_eq!(fakes[A].rewinds(), 1, "{ops_a:?}");
    }

    /// `restore volume` reads through the same supply: a bad slice of one of
    /// its units is read from the other copy, and the cartridge that served
    /// stays in the drive for the units after it.
    #[test]
    fn restore_volume_substitutes_a_slice_too() {
        let mut h = copies("256K", None, &[A, B]);
        damage(&mut h, A, 3, 0);
        let dest = TempDir::new().unwrap();
        let root = dest.path().to_string_lossy().into_owned();
        let requests = unit_requests(&[UNIT.to_string()], &root).unwrap();
        let options = RestoreOptions::default();
        let planned = plan_units(&h.conn, &h.config, A, &requests, &options).unwrap();
        let mut bay = h.bay(A, &[Reply::Load(B)]);
        let report = restore_units_from_bay(
            &h.conn,
            &h.paths,
            &h.config,
            A,
            &planned,
            &options,
            false,
            &mut bay,
            h.site(),
        )
        .unwrap();
        assert_eq!(report.failed(), 0, "{report:?}");
        assert_restored(&h, UNIT, &dest.path().join(UNIT), &[]);
        assert_eq!(bay.asked_labels(), [B]);
        assert_eq!(contacts(&h).len(), 2);
        let rows = crate::volume::restore_record::rows(&h.conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, "ok");
    }

    /// A restore the operator stops at the prompt leaves what it restored and
    /// says so, and the contact of the cartridge that was in the drive is
    /// closed.
    #[test]
    fn stopping_at_the_prompt_ends_the_restore_by_name() {
        let mut h = copies("256K", None, &[A, B]);
        damage(&mut h, A, 3, 0);
        let mut bay = h.bay(A, &[Reply::Stop]);
        let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        let err = r.unwrap_err().to_string();
        assert!(err.contains("stopped at the operator's request"), "{err}");
        assert!(err.contains(B), "{err}");
        let contacts = contacts(&h);
        assert_eq!(contacts.len(), 1, "{contacts:?}");
        assert_eq!(
            contacts[0].1, "ok",
            "A left the drive normally: {contacts:?}"
        );
        let rows = crate::volume::restore_record::rows(&h.conn).unwrap();
        assert_eq!(rows[0].outcome, "failed");
        assert!(!dest.path().join(SCRATCH_NAME).exists());
    }

    /// A quarantined volume (a verify proved its medium bad) is the last
    /// resort: an in-service copy is asked for first, and when the
    /// quarantined one is all there is it is asked for, said so.
    #[test]
    fn a_quarantined_copy_is_asked_for_last_and_said_to_be_the_last_resort() {
        let mut h = copies("256K", None, &[A, B, C]);
        damage(&mut h, A, 3, 0);
        h.conn
            .execute(
                "UPDATE volumes SET observed_condition = 'quarantined' WHERE label = ?1",
                params![B],
            )
            .unwrap();
        // C (in service) is asked for, though B has the lower id.
        let mut bay = h.bay(A, &[Reply::Load(C)]);
        let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        r.unwrap();
        assert_restored(&h, UNIT, dest.path(), &[]);
        assert_eq!(bay.asked_labels(), [C]);
        assert!(!bay.asked[0].1.contains("last resort"), "{:?}", bay.asked);

        // C damaged too: the quarantined B is all that is left, and the
        // prompt says so.
        damage(&mut h, C, 3, 0);
        let mut bay = h.bay(A, &[Reply::Load(C), Reply::Load(B)]);
        let (r, dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        r.unwrap();
        assert_restored(&h, UNIT, dest.path(), &[]);
        assert_eq!(bay.asked_labels(), [C, B]);
        assert!(
            bay.asked[1].1.contains("quarantined") && bay.asked[1].1.contains("last resort"),
            "{:?}",
            bay.asked
        );
    }

    /// What the operator reads: the prompt, the lines `restore unit` prints
    /// after a repair, and the error of a restore that could not be made
    /// whole. Printed so the texts can be reviewed as the operator sees them
    /// (`--nocapture`), and pinned in the parts an operator acts on.
    #[test]
    fn what_the_operator_reads() {
        // A substitution.
        let mut h = copies("256K", None, &[A, B]);
        h.conn
            .execute(
                "UPDATE cartridges SET serial_number = 'HUM1234567' WHERE barcode = ?1",
                params![format!("{B}-STK")],
            )
            .unwrap();
        damage(&mut h, A, 3, 0);
        let mut bay = h.bay(A, &[Reply::Load(B)]);
        let (r, _dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        let report = r.unwrap();
        assert!(
            bay.asked[0]
                .1
                .contains(&format!("cartridge {B}-STK, chip serial HUM1234567")),
            "the prompt names the sticker and the chip: {:?}",
            bay.asked
        );
        let out = what_it_printed(&bay, &report);
        eprintln!("=== SUBSTITUTION ===\n{out}");
        assert!(
            out.contains(&format!("was read from {B}, another copy of it")),
            "{out}"
        );

        // A salvage: one corrupted byte, the only copy.
        let mut h = copies("256K", None, &[A]);
        damage(&mut h, A, 2, 1);
        let mut bay = h.bay(A, &[]);
        let (r, _dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        let err = r.unwrap_err().to_string();
        eprintln!("=== SALVAGE ===\n{err}\n");
        assert!(err.contains("INCOMPLETE") && err.contains("file(s) damaged or missing:"));

        // A fill.
        let (mut h, one, _two) = two_stagings();
        damage_in(&mut h, A, one, 3, 1);
        let mut bay = h.bay(A, &[Reply::Load(B)]);
        let (r, _dest) = restore_unit(&h, A, &RestoreOptions::default(), &mut bay);
        let report = r.unwrap();
        let out = what_it_printed(&bay, &report);
        eprintln!("=== FILL ===\n{out}");
        assert!(
            out.contains("were filled from another copy of this version"),
            "{out}"
        );
    }

    /// The prompt, the summary line and the repair lines, as `restore unit`
    /// prints them.
    fn what_it_printed(bay: &TestBay, report: &RestoreReport) -> String {
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
        for line in crate::cli::restore::repair_lines(&report.substitutions, &report.filled) {
            out.push_str(&format!("{line}\n"));
        }
        out
    }

    // ── ADR-0012 2026-10-19 item 107: the repairs with `--key` ─────────────
    //
    // An heir holds the escrow secret and nothing in the home opens the tape.
    // Every repair runs on the same identities the first read does, so each
    // of them must work with the identity given on the command line alone.

    /// The heir's position: the key the slices are encrypted to is no longer
    /// in the home (a decoy takes the file name of every key the home has,
    /// the tenant's and the operator's, so the home still has keys and they
    /// open nothing on this tape) and the secret is in a file of its own, as
    /// `--key` takes it. The directory keeps the file.
    fn heir_keys(h: &Home) -> (TempDir, Vec<keys::KeyFile>) {
        let mut key_files: Vec<PathBuf> = fs::read_dir(&h.paths.keys_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(".age.key"))
            .collect();
        key_files.sort();
        let tenants_own = key_files
            .iter()
            .find(|p| p.file_name().unwrap().to_string_lossy().starts_with(TENANT))
            .expect("the tenant's key file")
            .clone();
        let secret = fs::read_to_string(&tenants_own).unwrap();
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("escrow.key");
        fs::write(&file, format!("# the escrow secret\n{secret}")).unwrap();
        for key_file in &key_files {
            keys::save_secret_key(key_file, &keys::generate_keypair().secret_key).unwrap();
        }
        let given = vec![keys::KeyFile::read(&file).unwrap()];
        (dir, given)
    }

    fn with_keys(given: &[keys::KeyFile]) -> RestoreOptions {
        RestoreOptions {
            keys: given.to_vec(),
            ..RestoreOptions::default()
        }
    }

    /// The wrong-key refusal as it stands today, with no `--key`: the control
    /// every test below compares to: age's own words, and no key file named.
    fn assert_wrong_key_refusal_without_keys(h: &Home, from: &str, bay: &mut TestBay) {
        let (r, _dest) = restore_unit(h, from, &RestoreOptions::default(), bay);
        let err = r.unwrap_err().to_string();
        assert!(
            err.contains(&age::DecryptError::NoMatchingKeys.to_string()),
            "{err}"
        );
        assert!(!err.contains("--key"), "no --key was given: {err}");
    }

    /// SLICE SUBSTITUTION with only the given identity: the acceptance test
    /// of the slice half, whole, through a swap to the second copy.
    #[test]
    fn a_slice_is_substituted_from_another_copy_with_only_the_key_given() {
        let mut h = copies("256K", None, &[A, B]);
        damage(&mut h, A, 3, 0);
        damage(&mut h, B, 2, 0);
        let (_dir, given) = heir_keys(&h);

        // Control: the same damage, the home's keys open nothing.
        let mut bay = h.bay(A, &[Reply::Load(B)]);
        assert_wrong_key_refusal_without_keys(&h, A, &mut bay);

        let mut bay = h.bay(A, &[Reply::Load(B)]);
        let (r, dest) = restore_unit(&h, A, &with_keys(&given), &mut bay);
        let report = r.unwrap_or_else(|e| panic!("{e}"));
        assert_restored(&h, UNIT, dest.path(), &[]);
        assert_eq!(bay.asked_labels(), [B]);
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

    /// CHUNK SALVAGE with only the given identity: one corrupted byte costs
    /// the files in its chunk, the rest are byte for byte whole, the restore
    /// says INCOMPLETE and names the damaged files, as without `--key`.
    #[test]
    fn a_bad_chunk_is_salvaged_around_with_only_the_key_given() {
        let mut h = copies("256K", None, &[A]);
        damage(&mut h, A, 2, 1);
        let (_dir, given) = heir_keys(&h);

        // Control: the home's keys open nothing, and the refusal says so.
        let mut bay = h.bay(A, &[]);
        assert_wrong_key_refusal_without_keys(&h, A, &mut bay);

        let mut bay = h.bay(A, &[]);
        let (r, dest) = restore_unit(&h, A, &with_keys(&given), &mut bay);
        let err = r.unwrap_err().to_string();
        assert!(err.contains("INCOMPLETE"), "{err}");
        let lost = lost_paths(&err);
        assert_eq!(
            lost,
            lost_in_chunk(&h, h.stage_set(UNIT, 1), SLICE_256K, 2, 1),
            "{err}"
        );
        assert_all_but(&h, dest.path(), &lost);
    }

    /// FILE FILL with only the given identity: the only copy of a stage set
    /// is damaged past reading and the lost files come whole from the other
    /// staging of the version, decrypted with the identity given.
    #[test]
    fn lost_files_are_filled_from_another_staging_with_only_the_key_given() {
        let (mut h, one, _two) = two_stagings();
        damage_in(&mut h, A, one, 3, 1);
        let (_dir, given) = heir_keys(&h);
        let mut bay = h.bay(A, &[Reply::Load(B)]);
        let (r, dest) = restore_unit(&h, A, &with_keys(&given), &mut bay);
        let report = r.unwrap_or_else(|e| panic!("{e}"));
        assert_restored(&h, UNIT, dest.path(), &[]);
        assert_eq!(bay.asked_labels(), [B]);
        assert!(!report.filled.is_empty(), "{report:?}");
    }

    /// A key given that opens nothing gets the refusal as it stands, with
    /// the files named after it; one wrong file beside the right one is no
    /// obstacle, in either order.
    #[test]
    fn a_wrong_key_is_refused_naming_the_files_and_a_right_one_beside_it_opens() {
        let h = copies("256K", None, &[A]);
        let (dir, right) = heir_keys(&h);
        let wrong_a = dir.path().join("wrong-a.key");
        let wrong_b = dir.path().join("wrong-b.key");
        for f in [&wrong_a, &wrong_b] {
            fs::write(f, format!("{}\n", keys::generate_keypair().secret_key)).unwrap();
        }
        let wrong = keys::KeyFile::read_all(&[wrong_a.clone(), wrong_b.clone()]).unwrap();

        let mut bay = h.bay(A, &[]);
        let (r, _dest) = restore_unit(&h, A, &with_keys(&wrong), &mut bay);
        let err = r.unwrap_err().to_string();
        assert!(
            err.contains(&age::DecryptError::NoMatchingKeys.to_string()),
            "the refusal as it stands: {err}"
        );
        for f in [&wrong_a, &wrong_b] {
            assert!(err.contains(&f.display().to_string()), "{f:?} named: {err}");
        }

        for order in [[0, 1, 2], [2, 1, 0]] {
            let all: Vec<keys::KeyFile> =
                vec![wrong[0].clone(), wrong[1].clone(), right[0].clone()];
            let given: Vec<keys::KeyFile> = order.iter().map(|&i| all[i].clone()).collect();
            let mut bay = h.bay(A, &[]);
            let (r, dest) = restore_unit(&h, A, &with_keys(&given), &mut bay);
            r.unwrap_or_else(|e| panic!("order {order:?}: {e}"));
            assert_restored(&h, UNIT, dest.path(), &[]);
        }
    }
}
