//! Compression, proven for each algorithm carried (T5.7; ADR-0012 items 280
//! and 284 to 287, #565; `volume-format-v3.md` §3.5): `gzip`, `xz` and `zstd`,
//! each slice compressed alone after the cut and before it is encrypted.
//!
//! A child of [`super`] (`span_proofs`), so it builds on the same real pieces:
//! each unit is staged by the real `stage create` (GNU tar, the cut, the
//! in-process compressor, age) and each tape written by the real `volume
//! write` over a [`MemStore`]. No catalog row is made by hand but the
//! `archive_sets` rows that stand for an operator's `archive-set create`.
//!
//! Every proof is a function of the algorithm, and a macro makes one test of
//! it for each of the three, so a failure names the algorithm in the test's
//! path. The units hold text that packs and noise that does not, so that the
//! slices differ in size on tape; each proof's positive control says that a
//! slice did shrink (the proof cannot pass on a unit that was never packed).
//!
//! What these extend, which they do not edit:
//! - proof 4 (a slice replaced from a second copy) extends
//!   `restore::repair_tests::two_copies_with_different_bad_slices_restore_whole_with_one_swap`,
//!   which runs `None` and `zstd`, to the three algorithms;
//! - proof 1 (the layers) extends
//!   `staging::tar_stage_tests::compression_level_resolves_dotfile_over_archive_set_over_defaults`
//!   (zstd, the level) to the algorithm and to all three;
//! - a compressed unit restoring whole and by path for each algorithm is
//!   `restore::reader_tests::a_compressed_unit_restores_whole_and_by_path_for_each_algorithm`,
//!   and the heir's is `tests/heir_restore.rs`: not repeated here.

use std::cell::RefCell;

use super::*;
use crate::volume::erase::{EraseOutcome, EraseReport, MediumKind, Recorded};
use crate::volume::rewrite::{self, ExpectedVolume, RewriteOptions, RewriteSteps};
use crate::volume::write::{CapacityNumbers, CompactReadReport, Initialized};

/// The algorithms carried (item 285).
const ALGS: [&str; 3] = ["gzip", "xz", "zstd"];

/// One test of `$proof` for each algorithm.
macro_rules! each_algorithm {
    ($module:ident, $proof:ident) => {
        mod $module {
            #[test]
            fn gzip() {
                super::$proof("gzip")
            }
            #[test]
            fn xz() {
                super::$proof("xz")
            }
            #[test]
            fn zstd() {
                super::$proof("zstd")
            }
        }
    };
}

// ── fixtures ──

/// Deterministic text, which every algorithm packs well.
fn text(len: usize, seed: u32) -> Vec<u8> {
    const WORDS: [&str; 24] = [
        "ledger",
        "orchard",
        "harvest",
        "quince",
        "tapestry",
        "lantern",
        "meadow",
        "copper",
        "willow",
        "granite",
        "voyage",
        "almanac",
        "saffron",
        "bobbin",
        "cinder",
        "driftwood",
        "ember",
        "fathom",
        "gable",
        "heron",
        "indigo",
        "juniper",
        "kettle",
        "linen",
    ];
    let mut x = seed.wrapping_mul(2_654_435_761) ^ 0x85EB_CA6B;
    let mut out = Vec::with_capacity(len + 16);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        out.extend_from_slice(WORDS[(x >> 8) as usize % WORDS.len()].as_bytes());
        out.push(b' ');
    }
    out.truncate(len);
    out
}

/// A unit's files, `(length, packs)` each: text where it packs, noise where
/// it does not, so that slices of one unit differ in size on tape.
fn mixed_files(tag: &str, seed: u32) -> Files {
    let spec = [
        (90_000, true),
        (30_000, false),
        (90_000, true),
        (30_000, false),
        (90_000, true),
    ];
    spec.iter()
        .enumerate()
        .map(|(i, &(len, packs))| {
            let s = seed + i as u32 * 7919;
            (
                format!("{tag}-part-{:04}.bin", i + 1),
                if packs { text(len, s) } else { noise(len, s) },
            )
        })
        .collect()
}

/// A unit small enough to stage many of.
fn small_files(tag: &str, seed: u32) -> Files {
    vec![(format!("{tag}-part-0001.txt"), text(20_000, seed))]
}

/// `[defaults] compression` for the stagings that follow.
fn compress_with(h: &mut Home, algorithm: &str, level: Option<i64>) {
    h.config.defaults.compression = algorithm.to_string();
    h.config.defaults.compression_level = level;
}

/// What the catalog recorded for the unit's staging: `(compression, level)`.
fn staging_facts(conn: &Connection, unit: &str) -> (String, Option<i64>) {
    conn.query_row(
        "SELECT compression, compression_level FROM stage_sets WHERE id = ?1",
        params![stage_set_of(conn, unit)],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

/// Each slice's `(size_bytes, encrypted_bytes)`: the cut, and the tape file.
fn cuts(conn: &Connection, unit: &str) -> Vec<(i64, i64)> {
    conn.prepare(
        "SELECT size_bytes, encrypted_bytes FROM stage_slices
          WHERE stage_set_id = ?1 ORDER BY slice_number",
    )
    .unwrap()
    .query_map(params![stage_set_of(conn, unit)], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// The positive control of every proof: `unit` was packed by `algorithm`, and
/// its slices are not the cut's size on tape (some shrank, and they differ).
fn assert_packed(h: &Home, unit: &str, algorithm: &str) {
    assert_eq!(
        staging_facts(&h.conn, unit).0,
        algorithm,
        "{unit} is staged with {algorithm}"
    );
    let cuts = cuts(&h.conn, unit);
    assert!(cuts.len() >= 4, "{unit}: a many-slice unit: {cuts:?}");
    assert!(
        cuts.iter().any(|&(cut, tape)| tape < cut),
        "{algorithm}: some slice of {unit} is smaller on tape than its cut: {cuts:?}"
    );
    let (min, max) = cuts
        .iter()
        .fold((i64::MAX, 0), |(lo, hi), &(_, t)| (lo.min(t), hi.max(t)));
    assert!(
        max - min > 4 * BS as i64,
        "{algorithm}: the slices of {unit} differ in size on tape: {cuts:?}"
    );
}

/// The slices of `unit` as `label` holds them: `(slice, tape file bytes)`.
fn slice_bytes(h: &Home, label: &str, unit: &str) -> Vec<(i64, Vec<u8>)> {
    let sizes = cuts(&h.conn, unit);
    position_rows(&h.conn)
        .into_iter()
        .filter(|(l, u, _, _, status)| l == label && u == unit && status == "written")
        .map(|(_, _, slice, position, _)| {
            let bytes = &h.files(label)[position as usize];
            (
                slice,
                bytes[..sizes[slice as usize - 1].1 as usize].to_vec(),
            )
        })
        .collect()
}

/// `restore unit` of `unit` from `start`, the operator loading `then` when
/// asked, and every file back byte for byte.
fn assert_restores(h: &Home, unit: &str, start: &str, then: &[&'static str]) -> RestoreReport {
    let script: Vec<Reply> = then.iter().map(|l| Reply::Load(l)).collect();
    let mut b = bay(h, start, &script);
    let (r, dest) = restore(
        h,
        unit,
        What::Unit,
        Some(start),
        &RestoreOptions::default(),
        &mut b,
    );
    let report = r.unwrap_or_else(|e| panic!("restore {unit} from {start}: {e}"));
    assert_restored(h, unit, dest.path());
    report
}

// ── 1. the three layers ──

/// Proof 1: `compression` and `compression_level` resolve through `[defaults]`,
/// an archive set and a unit's dotfile, the most specific winning, `none` when
/// none is set; the level is judged against the algorithm it resolves beside;
/// a level out of range is refused, at each end of the range.
fn prove_layers(algorithm: &str) {
    let other = ALGS[(ALGS.iter().position(|a| *a == algorithm).unwrap() + 1) % 3];
    let mut h = Home::new(None, 1, "64K");
    let in_set = |h: &mut Home, name: &str, set: Option<&str>| {
        h.unit_in(TENANT_ONE, name, small_files(name, 3), set);
    };

    // Nothing set: none.
    assert_eq!(h.config.defaults.compression, "none");
    in_set(&mut h, "lay-none", None);
    h.stage("lay-none");
    assert_eq!(staging_facts(&h.conn, "lay-none"), ("none".into(), None));

    // `[defaults]`.
    compress_with(&mut h, algorithm, Some(1));
    in_set(&mut h, "lay-defaults", None);
    h.stage("lay-defaults");
    assert_eq!(
        staging_facts(&h.conn, "lay-defaults"),
        (algorithm.into(), Some(1)),
        "{algorithm}: [defaults]"
    );

    // An archive set over `[defaults]`: its algorithm and its level.
    h.conn
        .execute(
            "INSERT INTO archive_sets (name, compression, compression_level)
             VALUES ('layer-set', ?1, 2)",
            params![other],
        )
        .unwrap();
    in_set(&mut h, "lay-set", Some("layer-set"));
    h.stage("lay-set");
    assert_eq!(
        staging_facts(&h.conn, "lay-set"),
        (other.into(), Some(2)),
        "{algorithm}: an archive set over [defaults]"
    );

    // A dotfile over its archive set.
    in_set(&mut h, "lay-dotfile", Some("layer-set"));
    let dotfile = h.sources.join("lay-dotfile").join(".tapectl-unit.toml");
    let mut df = crate::unit::dotfile::read_dotfile(&dotfile).unwrap();
    df.compression = Some(algorithm.to_string());
    df.compression_level = Some(3);
    crate::unit::dotfile::write_dotfile(&dotfile, &df).unwrap();
    h.stage("lay-dotfile");
    assert_eq!(
        staging_facts(&h.conn, "lay-dotfile"),
        (algorithm.into(), Some(3)),
        "{algorithm}: a dotfile over its archive set"
    );

    // A dotfile's `none` hides every layer below it.
    in_set(&mut h, "lay-dotfile-none", Some("layer-set"));
    let dotfile = h
        .sources
        .join("lay-dotfile-none")
        .join(".tapectl-unit.toml");
    let mut df = crate::unit::dotfile::read_dotfile(&dotfile).unwrap();
    df.compression = Some("none".to_string());
    crate::unit::dotfile::write_dotfile(&dotfile, &df).unwrap();
    h.stage("lay-dotfile-none");
    assert_eq!(
        staging_facts(&h.conn, "lay-dotfile-none").0,
        "none",
        "{algorithm}: a dotfile's none over the set and [defaults]"
    );

    // The level is judged against the algorithm it resolves beside: a level
    // set at `[defaults]` and an algorithm set by a more specific layer.
    let (low, high) = match algorithm {
        "gzip" => (1, 9),
        "xz" => (0, 9),
        _ => (1, 19),
    };
    let mut ok = 0;
    for (alg, lo, hi) in [("gzip", 1, 9), ("xz", 0, 9), ("zstd", 1, 19)] {
        // The inherited level `high` of `algorithm` is valid beside `alg`
        // exactly when it is in `alg`'s range.
        let fits = high >= lo && high <= hi;
        let problem = crate::config::level_problem(alg, Some(high));
        assert_eq!(problem.is_none(), fits, "{algorithm}'s {high} beside {alg}");
        ok += usize::from(fits);
    }
    assert!(ok >= 1, "the level fits at least its own algorithm");

    // Out of range, each end: refused at the config's load, naming the key
    // and the range, and by `stage create` with nothing staged.
    for bad in [low - 1, high + 1] {
        assert!(
            crate::config::level_problem(algorithm, Some(bad)).is_some(),
            "{algorithm} level {bad} is refused by the config check"
        );
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join("config.toml");
        fs::write(
            &cfg,
            format!("[defaults]\ncompression = \"{algorithm}\"\ncompression_level = {bad}\n"),
        )
        .unwrap();
        let e = Config::load(&cfg)
            .expect_err("a level out of range is refused at load")
            .to_string();
        assert!(
            e.contains("compression_level") && e.contains(&bad.to_string()),
            "{algorithm}: {e}"
        );

        compress_with(&mut h, algorithm, Some(bad));
        let name = format!("lay-bad-{bad}");
        in_set(&mut h, &name, None);
        let snapshot = crate::staging::snapshot_create(&h.conn, &name, &h.config).unwrap();
        let e = crate::staging::stage_create(&h.conn, &h.paths, &h.config, snapshot, true)
            .expect_err("a level out of range is refused by stage create")
            .to_string();
        assert!(
            e.contains("compression_level") && e.contains(&format!("{low} to {high}")),
            "{algorithm} level {bad}: {e}"
        );
        let staged: i64 = h
            .conn
            .query_row(
                "SELECT COUNT(*) FROM stage_sets ss JOIN snapshots s ON s.id = ss.snapshot_id
                  JOIN units u ON u.id = s.unit_id WHERE u.name = ?1",
                params![name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(staged, 0, "{algorithm} level {bad}: nothing was staged");
    }
    // Each end of the range is accepted.
    for good in [low, high] {
        assert!(
            crate::config::level_problem(algorithm, Some(good)).is_none(),
            "{algorithm} level {good}"
        );
    }
    compress_with(&mut h, algorithm, Some(low));
    in_set(&mut h, "lay-low", None);
    h.stage("lay-low");
    assert_eq!(
        staging_facts(&h.conn, "lay-low"),
        (algorithm.into(), Some(low)),
        "{algorithm}: the lowest level stages"
    );
}
each_algorithm!(
    layers_resolve_and_a_level_out_of_range_is_refused,
    prove_layers
);

// ── 2. one tape, both ──

/// Proof 2: a compressed and an uncompressed unit are on one tape, each
/// restores, and the MANIFEST names the algorithm of the one that states it.
fn prove_one_tape(algorithm: &str) {
    let (packed, plain, tape) = ("one-packed", "one-plain", "CMP2-T1");
    let mut h = Home::new(None, 1, "64K");
    compress_with(&mut h, algorithm, None);
    h.unit(TENANT_ONE, packed, mixed_files("packed", 11));
    h.stage(packed);
    compress_with(&mut h, "none", None);
    h.unit(TENANT_TWO, plain, mixed_files("plain", 12));
    h.stage(plain);
    h.volume(tape);
    h.write(tape, Warn::Never);

    assert_packed(&h, packed, algorithm);
    assert_eq!(staging_facts(&h.conn, plain).0, "none");
    assert!(
        cuts(&h.conn, plain).iter().all(|&(cut, tape)| tape >= cut),
        "the uncompressed unit is the cut's size or more on tape"
    );
    assert_eq!(
        writes_of(&h.conn, tape),
        rows(&[(packed, "completed"), (plain, "completed")]),
        "{algorithm}: both units are on the one tape"
    );

    assert_restores(&h, packed, tape, &[]);
    assert_restores(&h, plain, tape, &[]);

    // The envelope's MANIFEST names the algorithm of the unit that has one.
    let manifest = member(&operator_envelope(&h, tape), "MANIFEST.toml");
    let line = format!("compression = \"{algorithm}\"");
    assert!(
        manifest_block(&manifest, packed).contains(&line),
        "{algorithm}: {}",
        manifest_block(&manifest, packed)
    );
    assert!(
        !manifest_block(&manifest, plain).contains("compression"),
        "an uncompressed unit states none: {}",
        manifest_block(&manifest, plain)
    );
}
each_algorithm!(
    a_compressed_and_an_uncompressed_unit_share_one_tape,
    prove_one_tape
);

// ── 3. a second cartridge ──

const SPAN: &str = "span-packed";
const T3: [&str; 2] = ["CMP3-T1", "CMP3-T2"];

/// A compressed unit that continues over two cartridges at the early warning
/// (one byte into its slice 2, so tape 1 holds slices 1 and 2), copy target 1.
fn spanning_home(algorithm: &str) -> Home {
    let mut h = Home::new(None, 1, "64K");
    compress_with(&mut h, algorithm, None);
    h.unit(TENANT_ONE, SPAN, mixed_files("span", 21));
    h.stage(SPAN);
    for label in T3 {
        h.volume(label);
    }
    h.write(T3[0], Warn::InSlice(SPAN, 2));
    h.write(T3[1], Warn::Never);
    h
}

/// Proof 3: the unit continues on the second cartridge at the early warning
/// and restores from both, starting on either.
fn prove_continues(algorithm: &str) {
    let h = spanning_home(algorithm);
    assert_packed(&h, SPAN, algorithm);
    let n = num_slices(&h.conn, SPAN);
    assert_eq!(
        write::recorded_ranges(&h.conn, T3[0]).unwrap(),
        [range(SPAN, 1, 2)],
        "{algorithm}: tape 1 ends after the slice the warning came in"
    );
    assert_eq!(
        write::recorded_ranges(&h.conn, T3[1]).unwrap(),
        [range(SPAN, 3, n)],
        "{algorithm}: tape 2 takes the owed slices"
    );
    assert_eq!(snapshot_status(&h.conn, SPAN), "current");
    assert_eq!(slice_copies(&h.conn, SPAN), vec![1; n as usize]);

    let report = assert_restores(&h, SPAN, T3[0], &[T3[1]]);
    // Slice 1 holds only the unit's own entries (its dotfile), which a
    // restore does not take: every other slice is read.
    let rows = crate::db::stage_files::load(&h.conn, stage_set_of(&h.conn, SPAN)).unwrap();
    let first: Vec<&str> = rows
        .iter()
        .filter(|r| r.slice == 1 && r.kind == crate::volume::filetable::Kind::File)
        .map(|r| r.path.as_str())
        .collect();
    assert_eq!(first, [".tapectl-unit.toml"], "{algorithm}");
    assert_eq!(report.slices as i64, n - 1, "{algorithm}");
    assert_restores(&h, SPAN, T3[1], &[T3[0], T3[1]]);
}
each_algorithm!(
    a_compressed_unit_continues_on_a_second_cartridge,
    prove_continues
);

// ── 4. a second copy ──

/// Proof 4: a second copy of a compressed staging is the same slices (the same
/// bytes, the same hashes), so a slice damaged on the first is replaced from
/// the second by its sha256, with one swap, and the unit restores whole.
fn prove_second_copy(algorithm: &str) {
    let (unit, labels) = ("copy-packed", ["CMP4-A", "CMP4-B"]);
    let mut h = Home::new(None, 2, "64K");
    compress_with(&mut h, algorithm, None);
    h.unit(TENANT_ONE, unit, mixed_files("copy", 31));
    h.stage(unit);
    for label in labels {
        h.volume(label);
    }
    for label in labels {
        h.write(label, Warn::Never);
    }
    assert_packed(&h, unit, algorithm);

    let recorded: Vec<SliceRow> = slice_rows(&h.conn);
    assert!(!recorded.is_empty());
    let on_a = slice_bytes(&h, labels[0], unit);
    let on_b = slice_bytes(&h, labels[1], unit);
    assert_eq!(on_a.len(), recorded.len());
    assert_eq!(
        on_a, on_b,
        "{algorithm}: the two copies hold the same bytes"
    );
    for ((_, bytes), row) in on_a.iter().zip(&recorded) {
        assert_eq!(
            sha_hex(bytes),
            row.5,
            "{algorithm}: slice {} is the staging's, by sha256",
            row.1
        );
    }

    // Slice 2 of copy A is damaged: a byte inside it flipped. It is replaced
    // from copy B, by its checksum.
    let at = position_of(&h.conn, labels[0], unit, 2) as usize;
    let middle = cuts(&h.conn, unit)[1].1 as usize / 2;
    h.tapes.get_mut(labels[0]).unwrap().files[at][middle] ^= 0x55;
    let mut b = bay(&h, labels[0], &[Reply::Load(labels[1])]);
    let (r, dest) = restore(
        &h,
        unit,
        What::Unit,
        Some(labels[0]),
        &RestoreOptions::default(),
        &mut b,
    );
    let report = r.unwrap_or_else(|e| panic!("{algorithm}: {e}"));
    assert_restored(&h, unit, dest.path());
    assert_eq!(b.asked_labels(), [labels[1]], "{algorithm}: one swap");
    assert_eq!(report.substitutions.len(), 1, "{algorithm}");
    let sub = &report.substitutions[0];
    assert_eq!(
        (
            sub.slice_number,
            sub.supplied_by.as_str(),
            sub.damaged_on.as_str()
        ),
        (2, labels[1], labels[0]),
        "{algorithm}"
    );
}
each_algorithm!(
    a_second_copy_of_a_compressed_staging_replaces_a_slice_by_sha256,
    prove_second_copy
);

// ── 5. rewrite and compaction ──

const CHIP: &str = "CHIP-COMPRESSION-PROOFS";

/// The real steps of a rewrite on tapes in memory (the same pieces
/// `range_carry_proofs` runs, which are private to it).
struct MemSteps<'a> {
    h: &'a Home,
    tapes: RefCell<BTreeMap<String, MemStore>>,
    source: String,
}

impl RewriteSteps for MemSteps<'_> {
    fn host_preflight(&self, _what: &str) -> Result<()> {
        Ok(())
    }

    fn write_preflight(
        &self,
        expected: &ExpectedVolume,
        only_stage_sets: Option<&[i64]>,
    ) -> Result<Option<CapacityNumbers>> {
        write::rehearse_write_capacity(
            &self.h.conn,
            &self.h.paths,
            &self.h.config,
            DEVICE,
            BS,
            &write::RehearsedVolume {
                label: &expected.label,
                media_type: &expected.media_type,
                capacity_bytes: expected.capacity_bytes,
                cartridge_serial: &expected.cartridge_serial,
                manufacturer: &expected.manufacturer,
            },
            only_stage_sets,
            false,
        )
        .map(Some)
    }

    fn read(&self, source: &str) -> Result<CompactReadReport> {
        let mut store = copy_of(&self.tapes.borrow()[source]);
        write::compact_read(
            &self.h.conn,
            &self.h.config,
            source,
            &mut store,
            ContactSite::new(
                &self.h.config,
                Operation::VolumeRewrite,
                DEVICE,
                Medium::NoBackend,
            ),
        )
    }

    fn erase(&self, _confirm_label: &str, _cartridge_serial: Option<&str>) -> Result<EraseReport> {
        let c = &self.h.conn;
        let barcode = format!("{}-STK", self.source);
        let cartridge: i64 = c.query_row(
            "SELECT id FROM cartridges WHERE barcode = ?1",
            params![barcode],
            |r| r.get(0),
        )?;
        let tx = c.unchecked_transaction()?;
        crate::cli::operations::record_cartridge_erased(
            &tx,
            cartridge,
            &barcode,
            "pending_erase",
            &[volume_id(c, &self.source)],
            "available",
            None,
        )?;
        tx.commit()?;
        self.tapes
            .borrow_mut()
            .insert(self.source.clone(), MemStore::new(BS));
        Ok(EraseReport {
            device: DEVICE.into(),
            mode: "short",
            chip_serial: Some(CHIP.into()),
            medium: MediumKind::Tapectl,
            file0: None,
            sealed: None,
            detail: None,
            catalog_volumes: Vec::new(),
            catalog_cartridge: None,
            notes: Vec::new(),
            confirm_label: None,
            outcome: EraseOutcome::Erased,
            medium_after: Some(MediumKind::Empty),
            chip_serial_after: Some(CHIP.into()),
            recorded: Some(Recorded::default()),
            warnings: Vec::new(),
        })
    }

    fn init(&self, label: &str) -> Result<Initialized> {
        let mut tapes = self.tapes.borrow_mut();
        let mut medium = tapes.remove(&self.source).expect("the cartridge's medium");
        let mut slot = ContactSlot::empty();
        let r = write::volume_init_contacted(
            &self.h.conn,
            &self.h.config,
            label,
            DEVICE,
            BS,
            false,
            None,
            Some(&format!("{}-STK", self.source)),
            &mut slot,
            ContactStore::Injected(&mut medium),
        );
        let r = slot.finish_result(r);
        tapes.insert(label.to_string(), medium);
        r
    }

    fn write(&self, label: &str, only_stage_sets: Option<&[i64]>) -> Result<()> {
        let mut tapes = self.tapes.borrow_mut();
        let medium = tapes.remove(label).expect("the destination was made");
        let mut drive = Drive::new(&self.h.conn, label, medium, Warn::Never);
        let mut slot = ContactSlot::empty();
        let r = write::volume_write_contacted(
            &self.h.conn,
            &self.h.paths,
            &self.h.config,
            label,
            DEVICE,
            BS,
            false,
            false,
            false,
            true,
            true,
            &mut slot,
            ContactStore::Injected(&mut drive),
            only_stage_sets,
        );
        let r = slot.finish_result(r);
        tapes.insert(label.to_string(), drive.medium);
        r
    }

    fn resume_write(&self, _label: &str) -> Result<()> {
        panic!("a proof of the carry resumes nothing");
    }

    fn chip_serial(&self) -> Option<String> {
        Some(CHIP.into())
    }
}

fn volume_id(conn: &Connection, label: &str) -> i64 {
    conn.query_row(
        "SELECT id FROM volumes WHERE label = ?1",
        params![label],
        |r| r.get(0),
    )
    .unwrap()
}

/// [`Home::write`] with the full readback, the fact that makes a write a copy
/// a rewrite may lean on.
fn write_full(h: &mut Home, label: &str, warn: Warn) {
    let medium = h.tapes.remove(label).expect("the volume was made");
    let mut drive = Drive::new(&h.conn, label, medium, warn);
    let mut slot = ContactSlot::empty();
    let r = write::volume_write_contacted(
        &h.conn,
        &h.paths,
        &h.config,
        label,
        DEVICE,
        BS,
        false,
        false,
        false,
        true,
        true,
        &mut slot,
        ContactStore::Injected(&mut drive),
        None,
    );
    let r = slot.finish_result(r);
    h.tapes.insert(label.to_string(), drive.medium);
    r.unwrap_or_else(|e| panic!("volume write {label}: {e}"));
    crate::cli::volume::release_staging_after_write(&h.conn, &h.config, label, false);
}

/// A compressed unit over two fully read back cartridges, the first holding
/// slices 1 and 2 and the second the rest.
fn carried_home(algorithm: &str) -> Home {
    let mut h = Home::new(None, 1, "64K");
    compress_with(&mut h, algorithm, None);
    h.unit(TENANT_ONE, SPAN, mixed_files("span", 41));
    h.stage(SPAN);
    for label in T3 {
        h.volume(label);
    }
    write_full(&mut h, T3[0], Warn::InSlice(SPAN, 2));
    write_full(&mut h, T3[1], Warn::Never);
    assert_packed(&h, SPAN, algorithm);
    h
}

/// Proof 5, rewrite: `volume rewrite` of the cartridge holding the first run
/// carries the compressed slices unchanged: the same hashes in the catalog,
/// the same bytes on the new volume, and the unit restores.
fn prove_rewrite(algorithm: &str) {
    let mut h = carried_home(algorithm);
    let rows_before = slice_rows(&h.conn);
    let bytes_before = slice_bytes(&h, T3[0], SPAN);
    assert_eq!(bytes_before.len(), 2);

    let steps = MemSteps {
        tapes: RefCell::new(std::mem::take(&mut h.tapes)),
        h: &h,
        source: T3[0].to_string(),
    };
    let result = rewrite::rewrite(
        &steps.h.conn,
        &steps.h.config,
        &steps,
        &RewriteOptions {
            source: T3[0],
            destination: "CMP5-R",
            device: DEVICE,
            only_copy_on_staging: Some(T3[0]),
            assume_yes: true,
            with_staged: false,
            keep_staging: false,
        },
    );
    let tapes = steps.tapes.into_inner();
    h.tapes = tapes;
    let report = result.unwrap_or_else(|e| panic!("{algorithm}: rewrite: {e}"));
    assert_eq!(report.slices, 2, "{algorithm}: the two slices it held");

    assert_eq!(
        slice_rows(&h.conn),
        rows_before,
        "{algorithm}: the slices' sizes and hashes are unchanged"
    );
    assert_eq!(
        slice_bytes(&h, "CMP5-R", SPAN),
        bytes_before,
        "{algorithm}: the new volume holds the very bytes"
    );
    assert_eq!(
        write::recorded_ranges(&h.conn, "CMP5-R").unwrap(),
        [range(SPAN, 1, 2)]
    );
    assert_restores(&h, SPAN, "CMP5-R", &[T3[1]]);
}
each_algorithm!(
    volume_rewrite_carries_a_compressed_unit_unchanged,
    prove_rewrite
);

/// Proof 5, compaction: `compact-read`, the write and `compact-finish` of the
/// cartridge holding the first run carry the compressed slices unchanged.
fn prove_compaction(algorithm: &str) {
    let mut h = carried_home(algorithm);
    let rows_before = slice_rows(&h.conn);
    let bytes_before = slice_bytes(&h, T3[0], SPAN);
    h.volume("CMP5-C");

    let mut store = copy_of(&h.tapes[T3[0]]);
    let read = write::compact_read(
        &h.conn,
        &h.config,
        T3[0],
        &mut store,
        ContactSite::new(
            &h.config,
            Operation::VolumeCompactRead,
            DEVICE,
            Medium::NoBackend,
        ),
    )
    .unwrap_or_else(|e| panic!("{algorithm}: compact-read: {e}"));
    assert_eq!(read.slices_read, 2, "{algorithm}");
    write_full(&mut h, "CMP5-C", Warn::Never);
    write::compact_finish(&h.conn, &h.config, T3[0], true)
        .unwrap_or_else(|e| panic!("{algorithm}: compact-finish: {e}"));

    assert_eq!(
        slice_rows(&h.conn),
        rows_before,
        "{algorithm}: the slices' sizes and hashes are unchanged"
    );
    assert_eq!(
        slice_bytes(&h, "CMP5-C", SPAN),
        bytes_before,
        "{algorithm}: the new volume holds the very bytes"
    );
    assert_eq!(
        write::recorded_ranges(&h.conn, "CMP5-C").unwrap(),
        [range(SPAN, 1, 2)]
    );
    assert_restores(&h, SPAN, "CMP5-C", &[T3[1]]);
}
each_algorithm!(
    compaction_carries_a_compressed_unit_unchanged,
    prove_compaction
);

// ── 6. a rebuild ──

type StagingRow = (String, String, Option<i64>, Option<i64>, Option<String>);

/// Every staging's recorded facts: `(unit, compression, level, stream bytes,
/// tar version)`.
fn staging_rows(conn: &Connection) -> Vec<StagingRow> {
    conn.prepare(
        "SELECT u.name, ss.compression, ss.compression_level, ss.stream_bytes, ss.tar_version
           FROM stage_sets ss JOIN snapshots s ON s.id = ss.snapshot_id
           JOIN units u ON u.id = s.unit_id ORDER BY u.name",
    )
    .unwrap()
    .query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// Proof 6: each tape of a compressed unit's span, and a tape with a
/// compressed and an uncompressed unit, rebuilt into an empty catalog give
/// the staging's algorithm and level (and the rest of the writing catalog's
/// rows).
fn prove_rebuild(algorithm: &str) {
    let plain = "reb-plain";
    let mut h = Home::new(None, 1, "64K");
    compress_with(&mut h, algorithm, Some(2));
    h.unit(TENANT_ONE, SPAN, mixed_files("span", 51));
    h.stage(SPAN);
    compress_with(&mut h, "none", None);
    h.unit(TENANT_TWO, plain, small_files("plain", 52));
    h.stage(plain);
    for label in T3 {
        h.volume(label);
    }
    h.write(T3[0], Warn::InSlice(SPAN, 2));
    h.write(T3[1], Warn::Never);
    assert_packed(&h, SPAN, algorithm);

    let written = staging_rows(&h.conn);
    assert!(
        written
            .iter()
            .any(|r| r.0 == SPAN && r.1 == algorithm && r.2 == Some(2)),
        "{algorithm}: the writing catalog has its level: {written:?}"
    );
    for order in [[0, 1], [1, 0]] {
        let dir = TempDir::new().unwrap();
        let fresh = crate::db::open(&dir.path().join("rebuilt.db")).unwrap();
        for n in order {
            rebuild(&h, &fresh, T3[n]);
        }
        assert_eq!(
            staging_rows(&fresh),
            written,
            "{algorithm}: rebuilt in order {order:?}"
        );
        assert_eq!(slice_rows(&fresh), slice_rows(&h.conn), "{algorithm}");
        assert_eq!(snapshot_status(&fresh, SPAN), "current", "{algorithm}");
    }
}
each_algorithm!(
    a_rebuild_from_tape_restores_the_algorithm_and_level,
    prove_rebuild
);

// ── 7. the plan, and the early warning, by size on tape ──

/// `(first slice, tape bytes before the first slice)` of a tape, and the
/// padded on-tape size of each slice, from a probe write.
struct Probe {
    fixed: u64,
    padded: Vec<u64>,
    total: u64,
}

fn probe(algorithm: &str, level: &str) -> Probe {
    let mut h = Home::new(None, 1, "64K");
    compress_with(&mut h, algorithm, None);
    h.unit(TENANT_ONE, SPAN, mixed_files("plan", 61));
    h.stage(SPAN);
    h.volume(level);
    h.write(level, Warn::Never);
    let (_, _, first) = written_run(&h.conn, level, SPAN);
    Probe {
        fixed: h.files(level)[..first as usize]
            .iter()
            .map(|f| f.len() as u64)
            .sum(),
        padded: cuts(&h.conn, SPAN)
            .iter()
            .map(|&(_, tape)| (tape as u64).div_ceil(BS as u64) * BS as u64)
            .collect(),
        total: tape_bytes(&h, level),
    }
}

/// Proof 7: the plan counts each slice by its size on tape, and the early
/// warning's rule (the slice in flight finishes; the tape ends after it; the
/// rest is owed to the next) holds with slices that differ in size.
fn prove_plan(algorithm: &str) {
    let p = probe(algorithm, "CMP7-P");

    // (a) The cuts are equal and the tape files are not.
    let mut h = Home::new(None, 1, "64K");
    compress_with(&mut h, algorithm, None);
    h.unit(TENANT_ONE, SPAN, mixed_files("plan", 61));
    h.stage(SPAN);
    assert_packed(&h, SPAN, algorithm);
    let sizes = cuts(&h.conn, SPAN);
    let n = sizes.len();
    assert_eq!(
        padded_bytes(&h.conn, SPAN),
        p.padded.iter().sum::<u64>(),
        "{algorithm}: the plan's figure is the block-padded size on tape"
    );
    let raw: u64 = sizes
        .iter()
        .map(|&(cut, _)| (cut as u64).div_ceil(BS as u64) * BS as u64)
        .sum();
    assert!(
        padded_bytes(&h.conn, SPAN) + 64 * 1024 < raw,
        "{algorithm}: and it is not the cuts' sum ({raw})"
    );

    // (b) A cartridge whose nominal capacity holds the unit packed and not
    // its cuts: packed, it is written whole; the same files uncompressed are
    // a range ended by the early warning at the same capacity.
    let capacity = ((p.total as f64 / crate::config::DEFAULT_FILL_CEILING) as u64)
        .div_ceil(BS as u64)
        * BS as u64
        + 4 * BS as u64;
    let mut packed = Home::new(Some(&capacity.to_string()), 1, "64K");
    compress_with(&mut packed, algorithm, None);
    packed.unit(TENANT_ONE, SPAN, mixed_files("plan", 61));
    packed.stage(SPAN);
    packed.volume("CMP7-A");
    packed.write("CMP7-A", Warn::AtBytes(capacity));
    assert_eq!(
        writes_of(&packed.conn, "CMP7-A"),
        rows(&[(SPAN, "completed")]),
        "{algorithm}: packed, the unit fits the cartridge ({capacity} bytes)"
    );
    let mut raw_home = Home::new(Some(&capacity.to_string()), 1, "64K");
    raw_home.unit(TENANT_ONE, SPAN, mixed_files("plan", 61));
    raw_home.stage(SPAN);
    raw_home.volume("CMP7-B");
    raw_home.write("CMP7-B", Warn::AtBytes(capacity));
    assert_eq!(
        writes_of(&raw_home.conn, "CMP7-B"),
        rows(&[(SPAN, "range")]),
        "positive control: uncompressed, the same files do not fit the same cartridge"
    );

    // (c) The early warning comes inside slice 4, the one that does not pack,
    // by bytes on the medium: the slice in flight finishes, the tape ends
    // after it, the rest is owed. A plan that counted each slice as its cut
    // would have the warning fall in another slice.
    const K: usize = 4;
    assert!(n > K + 1, "{algorithm}: {n} slices");
    let before: u64 = p.padded[..K - 1].iter().sum();
    let warn_at = p.fixed + before + p.padded[K - 1] / 2;
    let mut by_cut = p.fixed;
    let mut cut_model = None;
    for (i, &(cut, _)) in sizes.iter().enumerate() {
        by_cut += (cut as u64).div_ceil(BS as u64) * BS as u64;
        if by_cut > warn_at {
            cut_model = Some(i + 1);
            break;
        }
    }
    assert_ne!(
        cut_model,
        Some(K),
        "{algorithm}: positive control: counting every slice as its cut, the warning would fall \
         in slice {cut_model:?}"
    );
    h.volume(T3[0]);
    h.volume(T3[1]);
    h.write(T3[0], Warn::AtBytes(warn_at));
    assert_eq!(
        write::recorded_ranges(&h.conn, T3[0]).unwrap(),
        [range(SPAN, 1, K as i64)],
        "{algorithm}: tape 1 ends after the slice the warning came in"
    );
    assert!(sealing(&h.conn, T3[0]).1.is_some(), "{algorithm}");
    h.write(T3[1], Warn::Never);
    assert_eq!(
        write::recorded_ranges(&h.conn, T3[1]).unwrap(),
        [range(SPAN, K as i64 + 1, n as i64)],
        "{algorithm}: the owed slices follow on tape 2"
    );
    assert_eq!(snapshot_status(&h.conn, SPAN), "current");
    assert_restores(&h, SPAN, T3[0], &[T3[1]]);
}
each_algorithm!(
    the_plan_and_the_early_warning_follow_each_slices_size_on_tape,
    prove_plan
);
