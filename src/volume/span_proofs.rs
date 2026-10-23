//! The span proofs (#478, tapectl 1.7.0): ADR-0012 items 93, 94, 127-131 and
//! 144, and item 136's proofs as far as a medium in memory can show them,
//! across the pieces different builds made (the planner, confirm, the
//! per-slice release, restore and rebuild). The mhvtl gate's span leg proves
//! the rest with a tape and the real `age` (item 136; RESTORE.sh's modes are
//! the gate's, item 70).
//!
//! Everything is real but the drive. The tenants, their keys and the escrow
//! recipient are made as `tapectl init` and `tenant add` make them; each unit
//! is a folder of files registered by `unit init`, its policy edited in its
//! dotfile as an operator edits it; `snapshot create` and `stage create` run
//! the real dar and age. Each cartridge is registered and each volume made by
//! the real `volume init` over a [`MemStore`], and each tape is written by the
//! real `volume write` ([`write::volume_write_contacted`] with the store
//! injected), followed by the release the CLI runs after it. The drive's
//! early warning is MemStore's own ([`MemStore::with_early_warning_after`]),
//! armed where a test puts it ([`Warn`]). No catalog row is made by hand.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use tempfile::TempDir;

use crate::cli::cartridge::CartridgeCommands;
use crate::cli::volume::StagingRelease;
use crate::config::{Config, LtoBackendConfig, TapectlPaths};
use crate::error::Result;
use crate::store::{CapacityReport, EarlyWarning, MemStore, Store};
use crate::tape::contact::{ContactSite, ContactSlot, Medium, Operation};
use crate::volume::format::ParsedIndexEntry;
use crate::volume::restore::repair_tests::tests::{Reply, TestBay};
use crate::volume::restore::{RestoreOptions, RestoreReport, RestoreTarget};
use crate::volume::write::{self, ContactStore, RecordedRange};

/// The tape's block size: small, so a fixture's slices are a few blocks.
const BS: usize = 4096;
/// The drive: a path no device is at, so every fact check that reads the
/// hardware finds nothing and the injected store is the medium.
const DEVICE: &str = "/nonexistent/tapectl-span-proofs-nst";
const DEVICE_SG: &str = "/nonexistent/tapectl-span-proofs-sg";
const BACKEND: &str = "lto6span";

/// The tenants, named so that no name is a word a tape could hold by chance
/// (the isolation scan looks for each).
const OPERATOR: &str = "quince-operator-main";
const TENANT_ONE: &str = "persimmon-tenant-one";
const TENANT_TWO: &str = "persimmon-tenant-two";

/// Deterministic incompressible bytes, so dar's output is the input's size.
fn noise(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761) ^ 0x9E37_79B9;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 11) as u8
        })
        .collect()
}

/// One unit's files: `(name, bytes)`, each name prefixed by the unit's tag.
type Files = Vec<(String, Vec<u8>)>;

fn files(tag: &str, sizes: &[usize], seed: u32) -> Files {
    sizes
        .iter()
        .enumerate()
        .map(|(i, &len)| {
            (
                format!("{tag}-part-{:04}.bin", i + 1),
                noise(len, seed + i as u32 * 7919),
            )
        })
        .collect()
}

/// When the drive's early warning comes on a write.
#[derive(Debug, Clone, Copy)]
enum Warn {
    /// Never: the tape seals full.
    Never,
    /// One byte into tape file of `unit`'s slice `n`, wherever the plan
    /// puts it (looked up in the plan's `write_positions` when the session
    /// starts writing): its first block reaches the warning.
    InSlice(&'static str, i64),
    /// After this many bytes on the medium: where the cartridge's nominal
    /// capacity ends, the estimate item 94 plans by.
    AtBytes(u64),
}

/// `m`'s files on a fresh medium whose early warning is after `at` bytes.
fn rearmed(m: MemStore, at: u64) -> MemStore {
    let mut armed = MemStore::new(BS).with_early_warning_after(at);
    armed.files = m.files;
    armed.syncs = m.syncs;
    armed
}

/// The drive a write sees: the volume's medium, its early warning armed as
/// [`Warn`] says.
struct Drive<'c> {
    medium: MemStore,
    conn: &'c Connection,
    label: String,
    warn: Warn,
}

impl<'c> Drive<'c> {
    fn new(conn: &'c Connection, label: &str, medium: MemStore, warn: Warn) -> Self {
        let medium = match warn {
            Warn::AtBytes(at) => rearmed(medium, at),
            _ => medium,
        };
        Drive {
            medium,
            conn,
            label: label.to_string(),
            warn,
        }
    }
}

impl Store for Drive<'_> {
    fn capacity(&mut self) -> Result<CapacityReport> {
        self.medium.capacity()
    }
    fn execute(&mut self, src: &mut dyn Read, len: u64, sync: bool) -> Result<u64> {
        if let Warn::InSlice(unit, n) = self.warn {
            let at = planned_position(self.conn, &self.label, unit, n);
            if at == Some(self.medium.files.len() as i64) {
                let before = self
                    .medium
                    .files
                    .iter()
                    .map(|f| f.len() as u64)
                    .sum::<u64>();
                let medium = std::mem::replace(&mut self.medium, MemStore::new(BS));
                self.medium = rearmed(medium, before + 1);
                self.warn = Warn::Never;
            }
        }
        self.medium.execute(src, len, sync)
    }
    fn early_warning(&self) -> Option<EarlyWarning> {
        self.medium.early_warning()
    }
    fn read_file(&mut self, position: u32, sink: &mut dyn Write) -> Result<u64> {
        self.medium.read_file(position, sink)
    }
    fn reposition_for_resume(&mut self, file_index: u32) -> Result<()> {
        self.medium.reposition_for_resume(file_index)
    }
    fn reposition_at_most(&mut self, file_index: u32) -> Result<u32> {
        self.medium.reposition_at_most(file_index)
    }
    fn blank_at_bot(&mut self) -> Result<bool> {
        self.medium.blank_at_bot()
    }
    fn eod_at_or_before(&mut self, position: u32) -> Result<bool> {
        self.medium.eod_at_or_before(position)
    }
    fn files_on_medium(&mut self) -> Result<Option<u32>> {
        self.medium.files_on_medium()
    }
}

/// A medium's recorded files on a fresh store (for a restore or a rebuild,
/// which read and never write).
fn copy_of(m: &MemStore) -> MemStore {
    let mut copy = MemStore::new(BS);
    copy.files = m.files.clone();
    copy.syncs = m.syncs.clone();
    copy
}

/// A tapectl home with its operator, two tenants and the escrow recipient,
/// a drive that declares `post_warning_bytes` (a span needs it, item 123),
/// and the volumes made in it, each medium as its last contact left it.
struct Home {
    conn: Connection,
    paths: TapectlPaths,
    config: Config,
    sources: PathBuf,
    tapes: BTreeMap<String, MemStore>,
    escrow_public: String,
    /// The files each unit was made of, by unit name.
    units: BTreeMap<String, Files>,
    _tmp: TempDir,
}

impl Home {
    /// `capacity`: the drive's `capacity_override` (a virtual drive's small
    /// cartridges), or `None` for LTO-6's own. `slice_size` is `[defaults]`.
    fn new(capacity: Option<&str>, min_copies: i32, slice_size: &str) -> Home {
        let tmp = TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().join("home"));
        paths.ensure_dirs().unwrap();
        let conn = crate::db::open(&paths.db_file).unwrap();
        let staging = tmp.path().join("staging");
        let sources = tmp.path().join("sources");
        fs::create_dir_all(&staging).unwrap();
        fs::create_dir_all(&sources).unwrap();

        let mut config = Config::default();
        config.dar.binary = "dar".to_string();
        config.staging.directory = staging.to_string_lossy().into_owned();
        config.defaults.slice_size = slice_size.to_string();
        config.defaults.min_copies = min_copies;
        config.backends.lto.push(LtoBackendConfig {
            name: BACKEND.into(),
            device_tape: DEVICE.into(),
            device_sg: DEVICE_SG.into(),
            generation: "LTO-6".into(),
            capacity_override: capacity.map(str::to_string),
            fill_ceiling: crate::config::DEFAULT_FILL_CEILING,
            enospc_buffer: "0".into(),
            post_warning_bytes: Some("64M".into()),
        });

        crate::tenant::add_tenant(&conn, &paths, OPERATOR, None, true).unwrap();
        for tenant in [TENANT_ONE, TENANT_TWO] {
            crate::tenant::add_tenant(&conn, &paths, tenant, None, false).unwrap();
        }
        let escrow = crate::cli::key::create_escrow_recipient(&conn, &paths, None).unwrap();
        Home {
            conn,
            paths,
            config,
            sources,
            tapes: BTreeMap::new(),
            escrow_public: escrow.keypair.public_key,
            units: BTreeMap::new(),
            _tmp: tmp,
        }
    }

    /// `unit init` of a folder of `files` for `tenant`; `allow_spanning`
    /// set in its dotfile's `[policy]` when given, as an operator edits it.
    fn unit(&mut self, tenant: &str, name: &str, files: Files, allow_spanning: Option<bool>) {
        self.unit_in(tenant, name, files, allow_spanning, None);
    }

    /// [`Self::unit`], in archive set `archive_set` when given.
    fn unit_in(
        &mut self,
        tenant: &str,
        name: &str,
        files: Files,
        allow_spanning: Option<bool>,
        archive_set: Option<&str>,
    ) {
        let dir = self.sources.join(name);
        fs::create_dir_all(&dir).unwrap();
        for (file, bytes) in &files {
            fs::write(dir.join(file), bytes).unwrap();
        }
        crate::unit::init_unit_with_config(
            &self.conn,
            &self.config,
            dir.to_str().unwrap(),
            tenant,
            Some(name),
            &[],
            archive_set,
        )
        .unwrap();
        if let Some(value) = allow_spanning {
            self.set_allow_spanning(name, value);
        }
        self.units.insert(name.to_string(), files);
    }

    /// The operator's edit of `unit`'s dotfile: `allow_spanning = value` in
    /// its `[policy]`.
    fn set_allow_spanning(&self, unit: &str, value: bool) {
        let dotfile = self.sources.join(unit).join(".tapectl-unit.toml");
        let mut d = crate::unit::dotfile::read_dotfile(&dotfile).unwrap();
        d.allow_spanning = Some(value);
        crate::unit::dotfile::write_dotfile(&dotfile, &d).unwrap();
    }

    /// `snapshot create` then `stage create` of `unit`: its stage set.
    fn stage(&self, unit: &str) -> i64 {
        let snapshot = crate::staging::snapshot_create(&self.conn, unit, &self.config).unwrap();
        crate::staging::stage_create(&self.conn, &self.paths, &self.config, snapshot, true)
            .unwrap_or_else(|e| panic!("stage create {unit}: {e}"))
    }

    /// `cartridge register` of a blank LTO-6 cartridge, then `volume init`
    /// of `label` on it (`--cartridge`: no chip serial can be read here).
    fn volume(&mut self, label: &str) {
        let barcode = format!("{label}-STK");
        crate::cli::cartridge::run(
            &self.conn,
            &self.config,
            &CartridgeCommands::Register {
                barcode: barcode.clone(),
                generation: "LTO-6".into(),
                capacity: None,
                serial: None,
                notes: None,
            },
            false,
            true,
            false,
        )
        .unwrap();
        let mut medium = MemStore::new(BS);
        let mut slot = ContactSlot::empty();
        let r = write::volume_init_contacted(
            &self.conn,
            &self.config,
            label,
            DEVICE,
            BS,
            false,
            None,
            Some(&barcode),
            &mut slot,
            ContactStore::Injected(&mut medium),
        );
        slot.finish_result(r)
            .unwrap_or_else(|e| panic!("volume init {label}: {e}"));
        self.tapes.insert(label.to_string(), medium);
    }

    /// `volume write` of `label` with the drive's early warning as `warn`,
    /// then the staging release `volume write` runs after a write that
    /// stood.
    fn write(&mut self, label: &str, warn: Warn) -> StagingRelease {
        self.try_write(label, warn)
            .unwrap_or_else(|e| panic!("volume write {label}: {e}"))
    }

    fn try_write(&mut self, label: &str, warn: Warn) -> Result<StagingRelease> {
        let medium = self.tapes.remove(label).expect("the volume was made");
        let mut drive = Drive::new(&self.conn, label, medium, warn);
        let mut slot = ContactSlot::empty();
        let r = write::volume_write_contacted(
            &self.conn,
            &self.paths,
            &self.config,
            label,
            DEVICE,
            BS,
            false,
            false,
            false,
            false,
            true,
            &mut slot,
            ContactStore::Injected(&mut drive),
            None,
        );
        let r = slot.finish_result(r);
        self.tapes.insert(label.to_string(), drive.medium);
        r?;
        Ok(crate::cli::volume::release_staging_after_write(
            &self.conn,
            &self.config,
            label,
            false,
        ))
    }

    fn files(&self, label: &str) -> &[Vec<u8>] {
        &self.tapes[label].files
    }

    /// The operator's identities, as `catalog rebuild --key` reads them.
    fn operator_identities(&self) -> Vec<age::x25519::Identity> {
        let ids =
            crate::crypto::keys::load_tenant_identities(&self.conn, &self.paths.keys_dir, OPERATOR)
                .unwrap();
        assert!(!ids.is_empty(), "the operator has a key");
        ids
    }
}

// ── catalog reads ──

/// Where the plan put `unit`'s slice `n` on `label`, once it is planned.
fn planned_position(conn: &Connection, label: &str, unit: &str, n: i64) -> Option<i64> {
    conn.query_row(
        "SELECT CAST(wp.position AS INTEGER) FROM write_positions wp
           JOIN writes w ON w.id = wp.write_id
           JOIN volumes v ON v.id = w.volume_id
           JOIN stage_slices sl ON sl.id = wp.stage_slice_id
           JOIN snapshots s ON s.id = w.snapshot_id
           JOIN units u ON u.id = s.unit_id
          WHERE v.label = ?1 AND u.name = ?2 AND sl.slice_number = ?3",
        params![label, unit, n],
        |r| r.get(0),
    )
    .ok()
}

fn position_of(conn: &Connection, label: &str, unit: &str, n: i64) -> i64 {
    planned_position(conn, label, unit, n)
        .unwrap_or_else(|| panic!("{unit} slice {n} is not planned on {label}"))
}

/// `(unit, writes.status, writes.may_span)` of the volume, by unit name.
fn writes_of(conn: &Connection, label: &str) -> Vec<(String, String, bool)> {
    conn.prepare(
        "SELECT u.name, w.status, w.may_span FROM writes w
           JOIN volumes v ON v.id = w.volume_id
           JOIN snapshots s ON s.id = w.snapshot_id
           JOIN units u ON u.id = s.unit_id
          WHERE v.label = ?1 ORDER BY u.name",
    )
    .unwrap()
    .query_map(params![label], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

fn rows(want: &[(&str, &str, bool)]) -> Vec<(String, String, bool)> {
    let mut v: Vec<(String, String, bool)> = want
        .iter()
        .map(|(u, s, m)| (u.to_string(), s.to_string(), *m))
        .collect();
    v.sort();
    v
}

fn stage_set_of(conn: &Connection, unit: &str) -> i64 {
    conn.query_row(
        "SELECT ss.id FROM stage_sets ss JOIN snapshots s ON s.id = ss.snapshot_id
           JOIN units u ON u.id = s.unit_id WHERE u.name = ?1",
        params![unit],
        |r| r.get(0),
    )
    .unwrap()
}

fn stage_set_status(conn: &Connection, unit: &str) -> String {
    conn.query_row(
        "SELECT status FROM stage_sets WHERE id = ?1",
        params![stage_set_of(conn, unit)],
        |r| r.get(0),
    )
    .unwrap()
}

fn snapshot_status(conn: &Connection, unit: &str) -> String {
    conn.query_row(
        "SELECT s.status FROM snapshots s JOIN units u ON u.id = s.unit_id WHERE u.name = ?1",
        params![unit],
        |r| r.get(0),
    )
    .unwrap()
}

fn num_slices(conn: &Connection, unit: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM stage_slices WHERE stage_set_id = ?1",
        params![stage_set_of(conn, unit)],
        |r| r.get(0),
    )
    .unwrap()
}

/// Each slice's copies, in slice order (item 127).
fn slice_copies(conn: &Connection, unit: &str) -> Vec<i64> {
    crate::policy::coverage::slice_copies(conn, stage_set_of(conn, unit))
        .unwrap()
        .into_iter()
        .map(|s| s.copies)
        .collect()
}

/// The unit's copies as `audit` counts a unit's current versions.
fn unit_copies(conn: &Connection, unit: &str) -> i64 {
    let id: i64 = conn
        .query_row("SELECT id FROM units WHERE name = ?1", [unit], |r| r.get(0))
        .unwrap();
    conn.query_row(
        &format!(
            "SELECT {}",
            crate::policy::coverage::copy_count_expr(
                &crate::policy::coverage::CoverageQuery::current_unit("?1")
            )
        ),
        [id],
        |r| r.get(0),
    )
    .unwrap()
}

/// The slices of `unit` whose staged file was released (item 129): the
/// release recorded, the path cleared and the file gone, all three together.
fn released_slices(conn: &Connection, unit: &str) -> Vec<i64> {
    let rows: Vec<(i64, Option<String>, Option<String>)> = conn
        .prepare(
            "SELECT slice_number, released_at, staging_path
               FROM stage_slices WHERE stage_set_id = ?1 ORDER BY slice_number",
        )
        .unwrap()
        .query_map(params![stage_set_of(conn, unit)], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let mut released = Vec::new();
    for (n, released_at, path) in rows {
        match (released_at, path) {
            (Some(_), None) => released.push(n),
            (None, Some(p)) => assert!(
                Path::new(&p).exists(),
                "{unit} slice {n} is kept, so its staged file is there: {p}"
            ),
            (released_at, path) => {
                panic!("{unit} slice {n}: released_at {released_at:?} with staging_path {path:?}")
            }
        }
    }
    released
}

/// File 0's `requires` line, as the tape holds it.
fn requires_line(files: &[Vec<u8>]) -> String {
    String::from_utf8_lossy(&files[0])
        .lines()
        .find(|l| l.starts_with("requires = "))
        .expect("File 0 states requires")
        .to_string()
}

/// A tape's front index (File 3), from its recorded bytes.
fn front_index(files: &[Vec<u8>]) -> Vec<ParsedIndexEntry> {
    let raw = crate::volume::format::without_nul_padding(&files[3]);
    crate::volume::format::parse_front_index(&String::from_utf8_lossy(raw)).unwrap()
}

fn range(unit: &str, first_slice: i64, last_slice: i64) -> RecordedRange {
    RecordedRange {
        unit: unit.into(),
        version: 1,
        first_slice,
        last_slice,
    }
}

/// The volume's `(sealed, ended_after)`.
fn sealing(conn: &Connection, label: &str) -> (bool, Option<i64>) {
    conn.query_row(
        "SELECT sealed_at IS NOT NULL, ended_after FROM volumes WHERE label = ?1",
        params![label],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

// ── item 93: two copies over three tapes ──

const A93: &str = "kumquat-whole-alpha";
const B93: &str = "kumquat-span-bravo";
const C93: &str = "kumquat-whole-charlie";
const T93: [&str; 3] = ["SPN93-T1", "SPN93-T2", "SPN93-T3"];

/// Item 93's catalog: `a` and `c` whole, `b` spannable in three slices, at a
/// copy target of 2, three blank volumes; nothing written yet.
fn home_93() -> Home {
    let mut h = Home::new(None, 2, "64K");
    h.unit(TENANT_ONE, A93, files("alpha-ledger", &[20_000], 1), None);
    h.unit(
        TENANT_ONE,
        B93,
        files("bravo-reel", &[50_000, 50_000, 50_000], 2),
        Some(true),
    );
    h.unit(TENANT_TWO, C93, files("charlie-notes", &[20_000], 3), None);
    for unit in [A93, B93, C93] {
        h.stage(unit);
    }
    assert_eq!(num_slices(&h.conn, A93), 1);
    assert_eq!(num_slices(&h.conn, B93), 3, "b is three slices");
    assert_eq!(num_slices(&h.conn, C93), 1);
    for label in T93 {
        h.volume(label);
    }
    h
}

/// THE item 93 proof, from the real write: two copies of a spannable `b`
/// over three tapes. Tape 1 holds the whole units first and `b` last, and
/// the early warning inside `b`'s slice 2 ends its range at 1-2. Tape 2
/// begins with `b`'s owed run, the whole set, and holds it whole. Tape 3
/// begins with the owed run 3-3 and seals full. The rows are `range`,
/// `completed`, `range`; every slice is on two volumes; the version is
/// `current` once one sealed volume set holds every slice (item 128); and
/// staging is released slice by slice as each slice meets the floor (item
/// 129), the whole units' sets as each meets it.
#[test]
fn item_93_two_copies_over_three_tapes_are_range_completed_range() {
    let mut h = home_93();

    // Tape 1: a and c whole first, b last; the warning one byte into b's
    // slice 2 ends the tape after it.
    let release = h.write(T93[0], Warn::InSlice(B93, 2));
    let c = &h.conn;
    let a1 = position_of(c, T93[0], A93, 1);
    let c1 = position_of(c, T93[0], C93, 1);
    let b1 = position_of(c, T93[0], B93, 1);
    assert_eq!(
        [c1, b1],
        [a1 + 1, a1 + 2],
        "whole units first, the spannable set last (item 83)"
    );
    assert_eq!(
        sealing(c, T93[0]),
        (true, Some(b1 + 1)),
        "ended after slice 2"
    );
    assert_eq!(
        writes_of(c, T93[0]),
        rows(&[
            (A93, "completed", false),
            (B93, "range", true),
            (C93, "completed", false)
        ])
    );
    assert_eq!(
        requires_line(h.files(T93[0])),
        "requires = [\"short-seal\", \"spanning\"]"
    );
    assert_eq!(
        write::recorded_ranges(c, T93[0]).unwrap(),
        [range(B93, 1, 2)]
    );
    assert_eq!(
        snapshot_status(c, B93),
        "staged",
        "a range alone (item 128)"
    );
    assert_eq!(slice_copies(c, B93), [1, 1, 0]);
    assert_eq!(unit_copies(c, B93), 0, "a range is not a copy on its own");
    assert!(
        matches!(release, StagingRelease::Retained { .. }),
        "{release:?}"
    );
    assert!(
        released_slices(c, B93).is_empty(),
        "no slice has two copies"
    );
    for unit in [A93, B93, C93] {
        assert_eq!(stage_set_status(c, unit), "staged", "{unit}");
    }

    // Tape 2: b's owed run is the whole set (1-2 owe one copy, 3 two), first.
    let release = h.write(T93[1], Warn::Never);
    let c = &h.conn;
    let first = position_of(c, T93[1], B93, 1);
    assert!(
        first < position_of(c, T93[1], A93, 1) && first < position_of(c, T93[1], C93, 1),
        "the owed run first (item 122)"
    );
    assert_eq!(sealing(c, T93[1]), (true, None), "a full seal");
    assert_eq!(
        writes_of(c, T93[1]),
        rows(&[
            (A93, "completed", false),
            (B93, "completed", true),
            (C93, "completed", false)
        ])
    );
    assert!(write::recorded_ranges(c, T93[1]).unwrap().is_empty());
    assert_eq!(
        snapshot_status(c, B93),
        "current",
        "tape 2 holds every slice on a sealed volume (item 128)"
    );
    assert_eq!(slice_copies(c, B93), [2, 2, 1]);
    assert_eq!(unit_copies(c, B93), 1, "tape 2's whole copy");
    // The release: a and c have their two copies; b's slices 1 and 2 have
    // theirs and are released, slice 3 is kept (item 129).
    assert!(
        matches!(release, StagingRelease::Released(_)),
        "{release:?}"
    );
    assert_eq!(stage_set_status(c, A93), "cleaned");
    assert_eq!(stage_set_status(c, C93), "cleaned");
    assert_eq!(stage_set_status(c, B93), "staged");
    assert_eq!(released_slices(c, B93), [1, 2]);

    // Tape 3: the owed run 3-3, first and alone; a full seal, still a range.
    h.write(T93[2], Warn::Never);
    let c = &h.conn;
    assert_eq!(sealing(c, T93[2]), (true, None));
    assert_eq!(writes_of(c, T93[2]), rows(&[(B93, "range", true)]));
    assert_eq!(
        write::recorded_ranges(c, T93[2]).unwrap(),
        [range(B93, 3, 3)]
    );
    assert_eq!(
        slice_copies(c, B93),
        [2, 2, 2],
        "every slice on two volumes"
    );
    assert_eq!(unit_copies(c, B93), 2, "item 127: the least-covered slice");
    assert_eq!(snapshot_status(c, B93), "current");
    assert_eq!(released_slices(c, B93), [1, 2, 3]);
    let keep = write::keep_together(c, T93[2]).unwrap();
    let tapes: Vec<&str> = keep[0].tapes.iter().map(|t| t.label.as_str()).collect();
    assert_eq!(
        (keep.len(), tapes),
        (1, vec![T93[0], T93[2]]),
        "tapes 1 and 3 are one copy (item 134)"
    );
    for (label, holds) in [(T93[0], [1, 2]), (T93[2], [3, 3])] {
        assert_eq!(
            requires_line(h.files(label)),
            "requires = [\"short-seal\", \"spanning\"]",
            "{label} holds {holds:?}"
        );
    }
}

// ── item 94: a spannable set larger than one tape, over three tapes ──

const W94: &str = "medlar-whole-waiting";
const S94: &str = "medlar-span-sequoia";
const T94: [&str; 3] = ["SPN94-T1", "SPN94-T2", "SPN94-T3"];
/// The cartridges' nominal capacity, the drive's `capacity_override` (a
/// virtual drive's small cartridges): about 1.75 MB past a tape's own files.
const CAP94: u64 = 2_500_000;
/// Where each tape's early warning comes: a little past the nominal
/// capacity, as a real cartridge's may. The plan's estimate takes it at the
/// nominal capacity (X11), and the room past it is what the waiting unit
/// fills after the span's tail on its last tape.
const WARN94: u64 = CAP94 + 352_000;

/// Item 94's catalog at a copy target of 1: `W` whole (under half a tape)
/// staged first, so 1.6.0's order would put it first, then `S`, which may
/// span and is about 2.75 tapes; three blank volumes.
fn home_94() -> Home {
    let mut h = Home::new(Some(&CAP94.to_string()), 1, "128K");
    h.unit(
        TENANT_TWO,
        W94,
        files("waiting-ledger", &[380_000, 380_000], 21),
        None,
    );
    h.unit(
        TENANT_ONE,
        S94,
        files("sequoia-reel", &[775_000; 6], 22),
        Some(true),
    );
    h.stage(W94);
    h.stage(S94);
    for label in T94 {
        h.volume(label);
    }
    h
}

/// The block-padded bytes of `unit`'s slices: its size to the plan.
fn padded_bytes(conn: &Connection, unit: &str) -> u64 {
    let sizes: Vec<i64> = conn
        .prepare("SELECT encrypted_bytes FROM stage_slices WHERE stage_set_id = ?1")
        .unwrap()
        .query_map(params![stage_set_of(conn, unit)], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    sizes
        .iter()
        .map(|&b| (b as u64).div_ceil(BS as u64) * BS as u64)
        .sum()
}

/// `(first slice, last slice, first position)` of `unit`'s written slices
/// on `label`.
fn written_run(conn: &Connection, label: &str, unit: &str) -> (i64, i64, i64) {
    conn.query_row(
        "SELECT MIN(sl.slice_number), MAX(sl.slice_number), MIN(CAST(wp.position AS INTEGER))
           FROM write_positions wp
           JOIN writes w ON w.id = wp.write_id
           JOIN volumes v ON v.id = w.volume_id
           JOIN stage_slices sl ON sl.id = wp.stage_slice_id
           JOIN snapshots s ON s.id = w.snapshot_id
           JOIN units u ON u.id = s.unit_id
          WHERE v.label = ?1 AND u.name = ?2 AND wp.status = 'written'",
        params![label, unit],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

/// THE item 94 proof, from the real write. `S` is larger than one tape and
/// allows spanning, so `stage create` admits it. On tape 1 it starts right
/// after the tape's own files, ahead of the whole unit `W` that 1.6.0's
/// order puts first: by the plan's estimate (the early warning at the
/// nominal capacity), starting there it touches three tapes, and behind `W`
/// it would touch four. `W` waits, staged and named. Tape 2 is the owed run,
/// to its warning; `W` still waits. Tape 3 is the rest of `S`, then `W`
/// after its tail, and seals full. The three ranges join end to end, every
/// slice has its one copy, the version is `current` only once tape 3 seals
/// (item 128), and each tape's slices are released after it (item 129).
#[test]
fn item_94_a_set_larger_than_one_tape_starts_early_and_touches_the_fewest_tapes() {
    let mut h = home_94();
    let n = num_slices(&h.conn, S94);
    let s_bytes = padded_bytes(&h.conn, S94);
    let w_bytes = padded_bytes(&h.conn, W94);
    assert!(s_bytes > CAP94, "S is larger than one tape: {s_bytes}");

    // Tape 1: S first, W waits.
    h.write(T94[0], Warn::AtBytes(WARN94));
    let c = &h.conn;
    assert_eq!(writes_of(c, T94[0]), rows(&[(S94, "range", true)]));
    let (first, k1, s1) = written_run(c, T94[0], S94);
    assert_eq!(first, 1);
    let fixed: u64 = h.files(T94[0])[..s1 as usize]
        .iter()
        .map(|f| f.len() as u64)
        .sum();
    assert!(
        front_index(h.files(T94[0]))[s1 as usize - 1].type_label != "data_slice",
        "S's slice 1 is the tape's first data file"
    );
    assert_eq!(write::waiting_units(c, T94[0]).unwrap(), [W94]);
    assert_eq!(
        write::recorded_ending(c, T94[0]).unwrap().deferred_units,
        [W94]
    );
    assert_eq!(stage_set_status(c, W94), "staged");
    // The estimate (X11): each tape holds the bound less the tape's own
    // files. From where S started it touches three tapes; behind W, four.
    let room = CAP94 - fixed;
    let tapes_from = |start: u64| 1 + (s_bytes - (CAP94 - start)).div_ceil(room);
    assert_eq!(tapes_from(fixed), 3, "the fewest tapes");
    assert_eq!(
        tapes_from(fixed + w_bytes),
        4,
        "behind W it would touch four"
    );
    assert_eq!(snapshot_status(c, S94), "staged");
    assert_eq!(released_slices(c, S94), (1..=k1).collect::<Vec<_>>());

    // Tape 2: the owed run, from K1+1, to the warning; W still waits.
    h.write(T94[1], Warn::AtBytes(WARN94));
    let c = &h.conn;
    assert_eq!(writes_of(c, T94[1]), rows(&[(S94, "range", true)]));
    let (first, k2, _) = written_run(c, T94[1], S94);
    assert_eq!(first, k1 + 1, "the owed run continues the range");
    assert!(k2 < n, "tape 2 ends inside S");
    assert_eq!(write::waiting_units(c, T94[1]).unwrap(), [W94]);
    assert_eq!(snapshot_status(c, S94), "staged", "item 128: not yet");
    assert_eq!(released_slices(c, S94), (1..=k2).collect::<Vec<_>>());

    // Tape 3: the rest of S, then W after its tail; a full seal.
    h.write(T94[2], Warn::AtBytes(WARN94));
    let c = &h.conn;
    assert_eq!(sealing(c, T94[2]), (true, None));
    assert_eq!(
        writes_of(c, T94[2]),
        rows(&[(S94, "range", true), (W94, "completed", false)])
    );
    let (first, last, _) = written_run(c, T94[2], S94);
    assert_eq!((first, last), (k2 + 1, n));
    assert!(
        position_of(c, T94[2], W94, 1) > position_of(c, T94[2], S94, n),
        "W after S's tail"
    );
    assert!(write::waiting_units(c, T94[2]).unwrap().is_empty());
    assert_eq!(
        write::recorded_ranges(c, T94[2]).unwrap(),
        [range(S94, k2 + 1, n)]
    );
    assert_eq!(slice_copies(c, S94), vec![1; n as usize]);
    assert_eq!(
        snapshot_status(c, S94),
        "current",
        "item 128: every slice sealed"
    );
    assert_eq!(unit_copies(c, S94), 1);
    assert_eq!(released_slices(c, S94), (1..=n).collect::<Vec<_>>());
    assert_eq!(stage_set_status(c, W94), "cleaned");
    let keep = write::keep_together(c, T94[2]).unwrap();
    let tapes: Vec<&str> = keep[0].tapes.iter().map(|t| t.label.as_str()).collect();
    assert_eq!((keep.len(), tapes), (1, T94.to_vec()));
}

// ── item 131: tapectl's restore across the cartridges ──

/// [`home_93`] with its three tapes written by item 93's layout.
fn written_93() -> Home {
    let mut h = home_93();
    h.write(T93[0], Warn::InSlice(B93, 2));
    h.write(T93[1], Warn::Never);
    h.write(T93[2], Warn::Never);
    h
}

/// [`home_94`] with its three tapes written, each tape's early warning at
/// [`WARN94`].
fn written_94() -> Home {
    let mut h = home_94();
    for label in T94 {
        h.write(label, Warn::AtBytes(WARN94));
    }
    h
}

/// H1's scripted bay ([`TestBay`]) over copies of every medium of `h`, with
/// `start` in the drive and an operator who answers the prompts by `script`.
fn bay(h: &Home, start: &str, script: &[Reply]) -> TestBay {
    TestBay {
        carts: h
            .tapes
            .iter()
            .map(|(label, m)| (label.clone(), Box::new(copy_of(m)) as Box<dyn Store>))
            .collect(),
        open: Some(start.to_string()),
        physical: Some(start.to_string()),
        attended: true,
        script: script.iter().copied().collect(),
        asked: Vec::new(),
        releases: 0,
    }
}

fn restore_site(h: &Home) -> ContactSite<'_> {
    ContactSite::new(&h.config, Operation::RestoreUnit, DEVICE, Medium::NoBackend)
}

/// What a restore is of: the whole unit, or one file of it.
#[derive(Clone, Copy)]
enum What<'a> {
    Unit,
    File(&'a str),
}

/// `restore unit` / `restore file` of `unit` v1 into a fresh directory
/// through `bay`: `--from label`, or the loaded tape when `from` is `None`.
fn restore(
    h: &Home,
    unit: &str,
    what: What<'_>,
    from: Option<&str>,
    options: &RestoreOptions,
    bay: &mut TestBay,
) -> (Result<RestoreReport>, TempDir) {
    let dest = TempDir::new().unwrap();
    let dest_dir = dest.path().to_string_lossy().into_owned();
    let target = match what {
        What::Unit => RestoreTarget::Unit {
            dest_dir: &dest_dir,
        },
        What::File(file_path) => RestoreTarget::File {
            file_path,
            dest_dir: &dest_dir,
        },
    };
    let r = match from {
        Some(label) => crate::volume::restore::restore_unit_from_bay(
            &h.conn,
            &h.paths,
            &h.config,
            unit,
            label,
            1,
            target,
            options,
            bay,
            restore_site(h),
        ),
        None => crate::volume::restore::restore_unit_from_loaded_bay(
            &h.conn,
            &h.paths,
            &h.config,
            unit,
            target,
            options,
            None,
            bay,
            restore_site(h),
        ),
    };
    (r, dest)
}

/// Every file of `unit` is in `dest`, byte for byte.
fn assert_restored(h: &Home, unit: &str, dest: &Path) {
    for (file, bytes) in &h.units[unit] {
        assert!(
            fs::read(dest.join(file)).ok().as_ref() == Some(bytes),
            "{unit}: {file} is not restored byte for byte"
        );
    }
}

/// Item 131 on item 93's tapes, written by the real write: `restore unit`
/// from tape 1 reads its range and asks for the live whole copy on tape 2
/// for the rest (a whole copy first, not tape 3's range), by label; from
/// tape 2 alone it needs no other cartridge; `--from` tape 3, which does not
/// hold slice 1, is refused before anything is read, naming the holders of
/// slice 1. Every restored file is byte-identical.
#[test]
fn item_131_restore_unit_crosses_the_written_cartridges() {
    let h = written_93();

    let mut b = bay(&h, T93[0], &[Reply::Load(T93[1])]);
    let (r, dest) = restore(
        &h,
        B93,
        What::Unit,
        Some(T93[0]),
        &RestoreOptions::default(),
        &mut b,
    );
    let report = r.unwrap_or_else(|e| panic!("from {}: {e}", T93[0]));
    assert_restored(&h, B93, dest.path());
    assert_eq!(report.slices, 3);
    assert_eq!(b.asked_labels(), [T93[1]], "the whole copy, by label");
    let prompt = &b.asked[0].1;
    assert!(
        prompt.contains("slice 3 is next") && prompt.contains(&format!("cartridge {}-STK", T93[1])),
        "{prompt}"
    );

    let mut b = bay(&h, T93[1], &[]);
    let (r, dest) = restore(
        &h,
        B93,
        What::Unit,
        Some(T93[1]),
        &RestoreOptions::default(),
        &mut b,
    );
    r.unwrap_or_else(|e| panic!("from {}: {e}", T93[1]));
    assert_restored(&h, B93, dest.path());
    assert!(b.asked.is_empty(), "tape 2 holds the version whole");

    // Named, the refusal comes before the drive (the dry run is that path).
    let dest = TempDir::new().unwrap();
    let err = crate::volume::restore::restore_unit(
        &h.conn,
        &h.paths,
        &h.config,
        B93,
        T93[2],
        &dest.path().to_string_lossy(),
        DEVICE,
        BS,
        None,
        true,
        &RestoreOptions::default(),
    )
    .unwrap_err()
    .to_string();
    for needle in [
        format!("volume \"{}\" holds only slice 3", T93[2]),
        format!("Slice 1 is on \"{}\"", T93[0]),
        format!("--from {}", T93[0]),
    ] {
        assert!(err.contains(&needle), "{needle}: {err}");
    }

    // A whole unit of the same tapes restores as in 1.6.0.
    let mut b = bay(&h, T93[0], &[]);
    let (r, dest) = restore(
        &h,
        C93,
        What::Unit,
        Some(T93[0]),
        &RestoreOptions::default(),
        &mut b,
    );
    r.unwrap();
    assert_restored(&h, C93, dest.path());
    assert!(b.asked.is_empty());
}

/// Item 131 on item 94's tapes, which hold the version only as three
/// ranges: `restore unit --from` tape 1 asks for tapes 2 and 3 in slice
/// order; `restore unit` of the loaded tape 1 does the same, and so does
/// `--spool`; `restore file` of a file whose bytes are on the later tapes
/// reads across the cartridges. Every file comes back byte for byte.
#[test]
fn item_131_restore_crosses_three_written_ranges() {
    let h = written_94();
    let later = [Reply::Load(T94[1]), Reply::Load(T94[2])];

    let mut b = bay(&h, T94[0], &later);
    let (r, dest) = restore(
        &h,
        S94,
        What::Unit,
        Some(T94[0]),
        &RestoreOptions::default(),
        &mut b,
    );
    let report = r.unwrap_or_else(|e| panic!("--from {}: {e}", T94[0]));
    assert_restored(&h, S94, dest.path());
    assert_eq!(report.slices as i64, num_slices(&h.conn, S94));
    assert_eq!(b.asked_labels(), [T94[1], T94[2]], "in slice order");
    for (prompt, (label, first)) in b.asked.iter().zip([
        (T94[1], written_run(&h.conn, T94[1], S94).0),
        (T94[2], written_run(&h.conn, T94[2], S94).0),
    ]) {
        assert!(
            prompt.1.contains(&format!("slice {first} is next"))
                && prompt.1.contains(&format!("cartridge {label}-STK")),
            "{}",
            prompt.1
        );
    }

    let mut b = bay(&h, T94[0], &later);
    let (r, dest) = restore(
        &h,
        S94,
        What::Unit,
        None,
        &RestoreOptions::default(),
        &mut b,
    );
    r.unwrap_or_else(|e| panic!("loaded {}: {e}", T94[0]));
    assert_restored(&h, S94, dest.path());
    assert_eq!(b.asked_labels(), [T94[1], T94[2]]);

    let spool = RestoreOptions {
        spool: true,
        ..RestoreOptions::default()
    };
    let mut b = bay(&h, T94[0], &later);
    let (r, dest) = restore(&h, S94, What::Unit, Some(T94[0]), &spool, &mut b);
    r.unwrap_or_else(|e| panic!("--spool: {e}"));
    assert_restored(&h, S94, dest.path());

    let (file, bytes) = h.units[S94].last().unwrap().clone();
    let mut b = bay(&h, T94[0], &later);
    let (r, dest) = restore(
        &h,
        S94,
        What::File(&file),
        Some(T94[0]),
        &RestoreOptions::default(),
        &mut b,
    );
    r.unwrap_or_else(|e| panic!("restore file {file}: {e}"));
    assert_eq!(fs::read(dest.path().join(&file)).unwrap(), bytes);
    assert_eq!(
        b.asked_labels(),
        [T94[1], T94[2]],
        "the file's bytes, and the archive's end, are on the later tapes"
    );

    // The whole unit written after the span's tail restores alone.
    let mut b = bay(&h, T94[2], &[]);
    let (r, dest) = restore(
        &h,
        W94,
        What::Unit,
        Some(T94[2]),
        &RestoreOptions::default(),
        &mut b,
    );
    r.unwrap();
    assert_restored(&h, W94, dest.path());
    assert!(b.asked.is_empty());
}

// ── catalog rebuild of each tape (items 130 and 144) ──

/// Every write: `(label, unit, status, may_span, snapshot status)`.
type WriteRow = (String, String, String, bool, String);

fn write_rows(conn: &Connection) -> Vec<WriteRow> {
    conn.prepare(
        "SELECT v.label, u.name, w.status, w.may_span, s.status FROM writes w
           JOIN volumes v ON v.id = w.volume_id
           JOIN snapshots s ON s.id = w.snapshot_id
           JOIN units u ON u.id = s.unit_id
          ORDER BY v.label, u.name",
    )
    .unwrap()
    .query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// Every position: `(label, unit, slice, position, status)`.
type PositionRow = (String, String, i64, i64, String);

fn position_rows(conn: &Connection) -> Vec<PositionRow> {
    conn.prepare(
        "SELECT v.label, u.name, sl.slice_number, CAST(wp.position AS INTEGER), wp.status
           FROM write_positions wp
           JOIN writes w ON w.id = wp.write_id
           JOIN volumes v ON v.id = w.volume_id
           JOIN stage_slices sl ON sl.id = wp.stage_slice_id
           JOIN snapshots s ON s.id = w.snapshot_id
           JOIN units u ON u.id = s.unit_id
          ORDER BY v.label, u.name, sl.slice_number",
    )
    .unwrap()
    .query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// Every slice row: `(unit, slice, size, encrypted size, sha256_plain,
/// sha256_encrypted)`.
type SliceRow = (String, i64, i64, i64, String, String);

fn slice_rows(conn: &Connection) -> Vec<SliceRow> {
    conn.prepare(
        "SELECT u.name, sl.slice_number, sl.size_bytes, sl.encrypted_bytes, sl.sha256_plain,
                sl.sha256_encrypted
           FROM stage_slices sl
           JOIN stage_sets ss ON ss.id = sl.stage_set_id
           JOIN snapshots s ON s.id = ss.snapshot_id
           JOIN units u ON u.id = s.unit_id
          ORDER BY u.name, sl.slice_number",
    )
    .unwrap()
    .query_map([], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
        ))
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// Every volume: `(label, sealed, ended_after)`.
fn volume_rows(conn: &Connection) -> Vec<(String, bool, Option<i64>)> {
    conn.prepare("SELECT label, sealed_at IS NOT NULL, ended_after FROM volumes ORDER BY label")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// `catalog rebuild --from-volume` of `label` into `fresh`, with the
/// operator's key, from a copy of the recorded medium.
fn rebuild(h: &Home, fresh: &Connection, label: &str) -> crate::volume::rebuild::RebuildReport {
    let scratch = TempDir::new().unwrap();
    let mut store = copy_of(&h.tapes[label]);
    crate::volume::rebuild::rebuild_from_store(
        fresh,
        &mut store,
        &h.operator_identities(),
        Some(label),
        "recovered",
        Some(BACKEND),
        scratch.path(),
        "memstore",
        ContactSite::new(
            &h.config,
            Operation::CatalogRebuild,
            "memstore",
            Medium::NoBackend,
        ),
    )
    .unwrap_or_else(|e| panic!("rebuild of {label}: {e}"))
}

/// Items 130 and 144 on item 93's tapes, written by the real write: each
/// tape rebuilt into an empty catalog, in every order, gives the writing
/// catalog's rows: the writes (`range`, `completed`, `range` and the whole
/// units'), every position and its status, every slice row with the hashes
/// staged, and each volume's ending. The spanning version is `staged`
/// exactly while no rebuilt set of sealed volumes holds every slice, and
/// the rebuild that completes it promotes it (item 144); at the end it has
/// its two copies.
#[test]
fn items_130_and_144_each_tape_rebuilt_in_any_order_gives_the_writing_catalogs_rows() {
    let h = written_93();
    let (writes, positions, slices, volumes) = (
        write_rows(&h.conn),
        position_rows(&h.conn),
        slice_rows(&h.conn),
        volume_rows(&h.conn),
    );
    let orders: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    for order in orders {
        let dir = TempDir::new().unwrap();
        let fresh = crate::db::open(&dir.path().join("rebuilt.db")).unwrap();
        for (i, &n) in order.iter().enumerate() {
            let report = rebuild(&h, &fresh, T93[n]);
            let rebuilt = &order[..=i];
            let whole = rebuilt.contains(&1) || (rebuilt.contains(&0) && rebuilt.contains(&2));
            assert_eq!(
                snapshot_status(&fresh, B93),
                if whole { "current" } else { "staged" },
                "{order:?} after {}",
                T93[n]
            );
            let before = &order[..i];
            let whole_before = before.contains(&1) || (before.contains(&0) && before.contains(&2));
            // Promoted by the rebuild that completes it, unless tape 2 came
            // first: that rebuild inserts the version current.
            let promoted_now = whole && !whole_before && i > 0;
            assert_eq!(
                report.versions_promoted,
                if promoted_now {
                    vec![format!("{B93} v1")]
                } else {
                    Vec::new()
                },
                "{order:?} after {}",
                T93[n]
            );
        }
        assert_eq!(write_rows(&fresh), writes, "{order:?}");
        assert_eq!(position_rows(&fresh), positions, "{order:?}");
        assert_eq!(slice_rows(&fresh), slices, "{order:?}");
        assert_eq!(volume_rows(&fresh), volumes, "{order:?}");
        assert_eq!(slice_copies(&fresh, B93), [2, 2, 2], "{order:?}");
        assert_eq!(unit_copies(&fresh, B93), 2, "{order:?}");
    }
}

/// Items 130 and 144 on item 94's tapes: rebuilt one by one into an empty
/// catalog, last tape first, the version is `staged` until the third, and
/// the three give the writing catalog's rows.
#[test]
fn items_130_and_144_item_94s_tapes_rebuilt_give_the_writing_catalogs_rows() {
    let h = written_94();
    let dir = TempDir::new().unwrap();
    let fresh = crate::db::open(&dir.path().join("rebuilt.db")).unwrap();
    for (i, label) in [T94[2], T94[0], T94[1]].into_iter().enumerate() {
        let report = rebuild(&h, &fresh, label);
        assert_eq!(
            snapshot_status(&fresh, S94),
            if i < 2 { "staged" } else { "current" },
            "after {label}"
        );
        assert_eq!(
            report.versions_promoted,
            if i < 2 {
                Vec::new()
            } else {
                vec![format!("{S94} v1")]
            }
        );
    }
    assert_eq!(write_rows(&fresh), write_rows(&h.conn));
    assert_eq!(position_rows(&fresh), position_rows(&h.conn));
    assert_eq!(slice_rows(&fresh), slice_rows(&h.conn));
    assert_eq!(volume_rows(&fresh), volume_rows(&h.conn));
    assert_eq!(unit_copies(&fresh, S94), 1);
}
