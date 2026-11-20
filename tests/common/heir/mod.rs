//! The heir's script's test harness (the plan's T4.10; `tests/heir_restore.rs`).
//!
//! Every tape here is built from REAL stagings, as `tests/format_v3.rs` builds
//! them: tenants and keys made as `tenant add` makes them, the escrow
//! recipient as `key escrow` makes it, units registered by `unit init` on made
//! trees and staged by the real `snapshot create` and `stage create` (GNU tar,
//! the cut at member starts, age, and the unit's compression). The volume is
//! built from what `volume write` builds it from and the full write session
//! runs against a `MemStore`. No staged unit is made by hand (invariant 8).
//!
//! The recorded files are then laid out as a DIRECTORY TAPE: `files/<n>` for
//! each tape file, block-padded to 512 KiB as on a cartridge. RESTORE.sh is
//! taken from the tape's own File 2 and run with a PATH of exactly the tools
//! a test chooses. `mt` and `dd` are stand-ins that drive the directory tape
//! and log every call to `state/ops` (what the navigation tests count);
//! `age` is `examples/heir_age.rs`, built from the `age` crate; everything
//! else (awk, sha256sum, tar, gzip, xz, zstd) is the host's own.
//!
//! A tape is built once per shape and kept in memory ([`Tape`]); each test
//! writes its own directory tape from it ([`Heir::of`]), so tests damage
//! their copy and no other.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rusqlite::{params, Connection};

use tapectl::config::{Config, TapectlPaths};
use tapectl::db;
use tapectl::policy::coverage::{IndexRun, IndexThisTape};
use tapectl::store::{MemStore, Tier};
use tapectl::volume::build::{self, BuildInputs, TenantInfo};
use tapectl::volume::format;
use tapectl::volume::index;
use tapectl::volume::layout_model::{KeyAvailability, SliceCheck, ZoneKind};
use tapectl::volume::session::{ConfirmOutcome, Ending, ExecuteOutcome};
use tapectl::volume::write;

/// The tape's block size as RESTORE.sh reads it.
pub const BLOCK: usize = 524_288;
/// The block size the in-memory store pads to (small, so a slice is many
/// blocks and the early warning can come inside one).
const BS: u64 = 4096;
pub const LABEL: &str = "HEIR01";
const VOLUME_UUID: &str = "a1b2c3d4-e5f6-4789-a123-456789abcdef";
const SERIAL: &str = "HEIRSERIAL";
/// `[defaults] slice_size`: a few members to a slice, and a file larger than
/// it is a long member's run (§3.3, D22).
const SLICE: &str = "64K";

pub const OPERATOR: &str = "quandong-operator";
pub const TENANT_A: &str = "loquat-alpha";
pub const TENANT_B: &str = "loquat-bravo";
pub const UNIT_A: &str = "alpha-unit";
pub const UNIT_B: &str = "bravo-unit";

/// Deterministic incompressible bytes.
pub fn noise(len: usize, seed: u32) -> Vec<u8> {
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

/// Deterministic compressible text.
pub fn text(len: usize, seed: u32) -> Vec<u8> {
    let words = [
        "tape",
        "slice",
        "member",
        "archive",
        "heir",
        "cartridge",
        "filemark",
        "seal",
    ];
    let mut out = Vec::with_capacity(len + 16);
    let mut x = seed.wrapping_mul(2_654_435_761) | 1;
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        out.extend_from_slice(words[(x as usize) % words.len()].as_bytes());
        out.push(b' ');
    }
    out.truncate(len);
    out
}

/// One unit as it was made: its regular files (relative path, bytes).
#[derive(Clone)]
pub struct UnitTruth {
    pub name: String,
    pub tenant: &'static str,
    pub files: Vec<(String, Vec<u8>)>,
    /// What a `raw_names` shape adds (ADR-0012 item 297, #569): regular files
    /// named by bytes that are not UTF-8 (relative path bytes, content),
    /// beside a hard link and a symbolic link to them that are in `links`.
    pub raw_files: Vec<(Vec<u8>, Vec<u8>)>,
    /// How many slices its staging has.
    pub slices: usize,
}

/// Who a key file belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Who {
    TenantA,
    TenantB,
    Operator,
    Escrow,
}

/// What a tape is made of.
#[derive(Clone)]
pub struct Shape {
    /// The units, in staging order: `(name, tenant, big)`. A big unit has a
    /// file larger than a slice (a long member's run), a name with a
    /// backslash, and several slices.
    pub units: Vec<(String, &'static str, bool)>,
    /// `compression` for every unit: `"none"`, `"gzip"`, `"xz"` or `"zstd"`.
    pub compression: &'static str,
    /// End the tape early: the drive's warning comes inside the slice with
    /// this index counted over every slice of the plan (0-based).
    pub warning_in_slice: Option<usize>,
    /// How many tapes are written from the same stagings (copies of one
    /// version: `HEIR01`, `HEIR02` ...). With a `restage` the series is the
    /// first tape and the tape of the second staging, and this is not used.
    pub copies: usize,
    /// Which tape of a series the early warning ends (`warning_in_slice`
    /// counts over that tape's own slices).
    pub warn_tape: usize,
    /// Stage the first unit a second time after the first tape is written,
    /// and write a second tape from that staging alone.
    pub restage: Restage,
    /// Edit the generated index files before the build (`units.tsv` and
    /// `paths.tsv` of each envelope), to give a tape a map that real
    /// catalogs of one write do not make.
    pub index_edit: Option<fn(&index::IndexFiles)>,
    /// Build the tape with no cross-volume index at all.
    pub no_index: bool,
    /// Write the units across cartridges as a real write plans them
    /// (`staged_build_units_spanning`).
    pub span: Option<Span>,
    /// Add to each BIG unit files whose names are not UTF-8, a folder so
    /// named, a hard link and a symbolic link whose target is such a name
    /// (`UnitTruth::raw_files`; [`RAW_NAMES`]).
    pub raw_names: bool,
}

/// The names a `raw_names` shape adds, as the bytes they are on disk: a
/// Latin-1 file, a folder so named with a file in it, a hard link to the
/// first file, and a symbolic link whose target is the first file's name.
pub const RAW_NAMES: [&[u8]; 6] = [
    b"caf\xe9.txt",
    b"dir\xe9/in\xff.bin",
    b"hard\xe9-link",
    b"sym\xe9-link",
    b"to-\xfe\xfd-target",
    // UTF-8 text that LOOKS like a byte escape: a backslash, `x`, `e`, `9`.
    b"lit\\xe9.txt",
];

/// A unit written across cartridges (T4.12): one tape for each entry of
/// `warns`, each planned from what the earlier tapes' confirms recorded.
#[derive(Clone)]
pub struct Span {
    /// `[defaults] min_copies`: 1 gives each slice one copy over the series,
    /// 2 gives item 93's two copies.
    pub min_copies: i32,
    /// For each tape in order, the slice of its OWN plan (0-based) the
    /// drive's early warning comes in, or `None` when the tape seals full.
    pub warns: Vec<Option<usize>>,
}

/// What a second staging of the first unit changes (#528).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Restage {
    None,
    /// `unit tag` rewrites the unit's dotfile; the same Version stages again.
    Dotfile,
    /// A file's bytes change: a new Version (2) is minted and staged.
    Content,
}

impl Shape {
    /// Two tenants, a small unit and a big one.
    pub fn two() -> Shape {
        Shape {
            units: vec![
                (UNIT_A.into(), TENANT_A, false),
                (UNIT_B.into(), TENANT_B, true),
            ],
            compression: "none",
            warning_in_slice: None,
            copies: 1,
            warn_tape: 0,
            restage: Restage::None,
            index_edit: None,
            no_index: false,
            span: None,
            raw_names: false,
        }
    }
    /// One big unit of tenant A, compressed.
    pub fn one(compression: &'static str) -> Shape {
        Shape {
            units: vec![(UNIT_A.into(), TENANT_A, true)],
            compression,
            warning_in_slice: None,
            copies: 1,
            warn_tape: 0,
            restage: Restage::None,
            index_edit: None,
            no_index: false,
            span: None,
            raw_names: false,
        }
    }
}

/// A sealed tape: every file as recorded, and what each unit was made of.
pub struct Tape {
    /// True bytes of each tape file, as the in-memory store recorded them.
    pub files: Vec<Vec<u8>>,
    pub units: Vec<UnitTruth>,
    /// Secret keys (`AGE-SECRET-KEY-1...`), by owner.
    pub keys: BTreeMap<&'static str, String>,
    pub ending: Ending,
    /// The front index's entries.
    pub entries: Vec<format::ParsedIndexEntry>,
    /// File 2, the script, as the tape holds it.
    pub script: Vec<u8>,
    /// The volume's label and uuid (File 0's).
    pub label: String,
    pub volume_uuid: String,
    /// The unit's uuid of each unit, by name.
    pub unit_uuids: BTreeMap<String, String>,
    /// For each unit the tape's plan holds a slice of: the first and last
    /// slice number of the plan's run, and of the run the seal covers (what
    /// the tape holds), `(unit, planned first, planned last, held first,
    /// held last)`; held is `(0, 0)` when no planned slice is on the tape.
    pub runs: Vec<(String, i64, i64, i64, i64)>,
    _home: std::sync::Arc<tempfile::TempDir>,
}

fn key_name(w: Who) -> &'static str {
    match w {
        Who::TenantA => "tenant-a",
        Who::TenantB => "tenant-b",
        Who::Operator => "operator",
        Who::Escrow => "escrow",
    }
}

struct Home {
    conn: Connection,
    paths: TapectlPaths,
    config: Config,
    sources: PathBuf,
}

/// What writing a tape needs besides the catalog.
struct Ctx {
    escrow_public: String,
    keys: BTreeMap<&'static str, String>,
    tmp: std::sync::Arc<tempfile::TempDir>,
}

fn label_of(n: usize) -> String {
    if n == 0 {
        LABEL.to_string()
    } else {
        format!("HEIR{:02}", n + 1)
    }
}

fn volume_uuid_of(n: usize) -> String {
    if n == 0 {
        VOLUME_UUID.to_string()
    } else {
        format!("a1b2c3d4-e5f6-4789-a123-4567890000{n:02x}")
    }
}

fn public_keys(conn: &Connection, tenant: &str) -> Vec<String> {
    conn.prepare(
        "SELECT k.public_key FROM encryption_keys k JOIN tenants t ON t.id = k.tenant_id
          WHERE t.name = ?1 AND k.is_active = 1 ORDER BY k.id",
    )
    .unwrap()
    .query_map(params![tenant], |r| r.get(0))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

fn tenant_id(conn: &Connection, tenant: &str) -> i64 {
    conn.query_row(
        "SELECT id FROM tenants WHERE name = ?1",
        params![tenant],
        |r| r.get(0),
    )
    .unwrap()
}

fn secret_of(h: &Home, tenant: &str) -> String {
    use age::secrecy::ExposeSecret;
    let ids =
        tapectl::crypto::keys::load_tenant_identities(&h.conn, &h.paths.keys_dir, tenant).unwrap();
    ids[0].to_string().expose_secret().to_string()
}

/// `unit init` of a made tree, then `snapshot create` and `stage create`.
fn make_unit(
    h: &Home,
    name: &str,
    tenant: &'static str,
    big: bool,
    raw: bool,
    seed: u32,
) -> UnitTruth {
    let dir = h.sources.join(name);
    fs::create_dir_all(dir.join("inner")).unwrap();
    let mut files: Vec<(String, Vec<u8>)> = vec![
        (format!("{name}-small.txt"), text(3_000, seed)),
        (format!("inner/{name}-deep.dat"), noise(20_000, seed + 2)),
        ("empty".to_string(), Vec::new()),
    ];
    if big {
        files.push((format!("{name}-large.bin"), noise(150_000, seed + 1)));
        for i in 0..8 {
            files.push((format!("inner/mid-{i}.dat"), noise(12_000, seed + 10 + i)));
        }
        files.push(("odd\\back name.txt".to_string(), text(2_000, seed + 30)));
        // A TAB and a newline in a name: the index's escaping carries both.
        files.push(("tab\tand\nnewline.txt".to_string(), text(1_500, seed + 32)));
        // A folder whose name holds a backslash.
        files.push(("dir\\one/leaf.txt".to_string(), text(700, seed + 33)));
        files.push(("notes.txt".to_string(), text(40_000, seed + 31)));
    }
    for (path, bytes) in &files {
        fs::create_dir_all(dir.join(path).parent().unwrap()).unwrap();
        fs::write(dir.join(path), bytes).unwrap();
    }
    let mut raw_files: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    if big && raw {
        use std::os::unix::ffi::OsStrExt;
        let os = std::ffi::OsStr::from_bytes;
        let first = text(2_500, seed + 40);
        let inner = noise(9_000, seed + 41);
        fs::write(dir.join(os(RAW_NAMES[0])), &first).unwrap();
        fs::create_dir_all(dir.join(os(RAW_NAMES[1])).parent().unwrap()).unwrap();
        fs::write(dir.join(os(RAW_NAMES[1])), &inner).unwrap();
        fs::hard_link(dir.join(os(RAW_NAMES[0])), dir.join(os(RAW_NAMES[2]))).unwrap();
        std::os::unix::fs::symlink(os(RAW_NAMES[0]), dir.join(os(RAW_NAMES[3]))).unwrap();
        std::os::unix::fs::symlink(os(b"\xff\xfegone"), dir.join(os(RAW_NAMES[4]))).unwrap();
        let literal = text(1_200, seed + 42);
        fs::write(dir.join(os(RAW_NAMES[5])), &literal).unwrap();
        raw_files.push((RAW_NAMES[5].to_vec(), literal));
        raw_files.push((RAW_NAMES[0].to_vec(), first.clone()));
        raw_files.push((RAW_NAMES[1].to_vec(), inner));
        raw_files.push((RAW_NAMES[2].to_vec(), first));
    }
    std::os::unix::fs::symlink(format!("{name}-small.txt"), dir.join("a-symlink")).unwrap();
    fs::hard_link(
        dir.join(format!("{name}-small.txt")),
        dir.join(format!("{name}-hardlink.txt")),
    )
    .unwrap();
    files.push((
        format!("{name}-hardlink.txt"),
        files[0].1.clone(), // same bytes as the first name
    ));
    tapectl::unit::init_unit_with_config(
        &h.conn,
        &h.config,
        dir.to_str().unwrap(),
        tenant,
        Some(name),
        &[],
        None,
    )
    .unwrap();
    let snapshot = tapectl::staging::snapshot_create(&h.conn, name, &h.config).unwrap();
    let stage_set_id = tapectl::staging::stage_create(&h.conn, &h.paths, &h.config, snapshot, true)
        .unwrap_or_else(|e| panic!("stage create {name}: {e}"));
    let slices: i64 = h
        .conn
        .query_row(
            "SELECT COUNT(*) FROM stage_slices WHERE stage_set_id = ?1",
            params![stage_set_id],
            |r| r.get(0),
        )
        .unwrap();
    UnitTruth {
        name: name.to_string(),
        tenant,
        files,
        raw_files,
        slices: slices as usize,
    }
}

/// Write one tape of `units` (the selection `bu`) to a memory store through
/// the whole write session, and keep what it recorded. `n` numbers the tapes
/// of one catalog: it names the label and the volume uuid.
fn write_tape(
    h: &Home,
    ctx: &Ctx,
    shape: &Shape,
    units: &[UnitTruth],
    bu: Vec<build::BuildUnit>,
    n: usize,
    warn: Option<usize>,
) -> Tape {
    let label = label_of(n);
    let volume_uuid = volume_uuid_of(n);
    let ids: Vec<i64> = bu.iter().map(|u| u.stage_set_id).collect();
    let work = tempfile::tempdir().unwrap();
    let idx = if shape.no_index {
        None
    } else {
        // As `index_runs` does for a write: a unit that may span states the
        // run its plan holds, any other the whole set.
        let runs: Vec<IndexRun> = bu
            .iter()
            .map(|u| match (&u.span, u.slices.first(), u.slices.last()) {
                (Some(_), Some(first), Some(last)) => IndexRun {
                    stage_set_id: u.stage_set_id,
                    slices: Some((first.slice_number, last.slice_number)),
                },
                _ => IndexRun::whole(u.stage_set_id),
            })
            .collect();
        let idx = index::generate_files_for_runs(
            &h.conn,
            &IndexThisTape {
                label: &label,
                uuid: &volume_uuid,
                cartridge_serial: SERIAL,
                generation: "LTO-6",
            },
            &runs,
            &work.path().join("index"),
        )
        .unwrap();
        if let Some(edit) = shape.index_edit {
            edit(&idx);
        }
        Some(idx)
    };
    let tables = write::generate_tables(&h.conn, &bu, &work.path().join("tables")).unwrap();
    let catalog_db = work.path().join("catalog.db");
    db::ontape_catalog::write(&h.conn, &ids, &catalog_db).unwrap();
    let mut tenant_ids: Vec<i64> = bu.iter().map(|u| u.tenant_id).collect();
    tenant_ids.sort_unstable();
    tenant_ids.dedup();
    let tenants: Vec<TenantInfo> = [TENANT_A, TENANT_B]
        .into_iter()
        .map(|t| TenantInfo {
            tenant_id: tenant_id(&h.conn, t),
            tenant_name: t.to_string(),
            public_keys: public_keys(&h.conn, t),
        })
        .filter(|t| tenant_ids.contains(&t.tenant_id))
        .collect();
    h.conn
        .execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes,
                                  status, uuid)
             VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized', ?2)",
            params![label, volume_uuid],
        )
        .unwrap();
    let volume_id = h.conn.last_insert_rowid();
    let inputs = BuildInputs {
        label: label.clone(),
        volume_uuid: volume_uuid.clone(),
        media_type: "LTO-6".to_string(),
        tapectl_version: env!("CARGO_PKG_VERSION").to_string(),
        created_at: "2026-11-19T00:00:00Z".to_string(),
        block_size: BS,
        usable_bytes: 512 * 1024 * 1024,
        enospc_buffer: 0,
        nominal_capacity: 2_500_000_000_000,
        mam_capacity: 2_500_000_000_000,
        mam_manufacturer: "TAPECTL-TEST".to_string(),
        mam_serial: SERIAL.to_string(),
        cartridge_identity_source: Some("mam".to_string()),
        mam_length: 0,
        mam_loads: 0,
        units: bu.clone(),
        tenants,
        operator_public_keys: public_keys(&h.conn, OPERATOR),
        escrow_public_key: Some(ctx.escrow_public.clone()),
        catalog_db_path: Some(catalog_db),
        index: idx,
        tables: Some(tables),
    };
    let session_dir = tempfile::tempdir().unwrap();
    let built = build::build(&inputs, session_dir.path()).expect("build succeeds");
    let kav = KeyAvailability {
        tenant_ids: tenant_ids.clone(),
        tenants_with_active_key: tenant_ids.iter().copied().collect(),
        operator_key_present: true,
        escrow_recipient_present: Some(true),
        stage_sets_lacking_escrow: None,
    };
    let mut store = MemStore::new(BS as usize);
    if let Some(k) = warn {
        let slices: Vec<(i32, u64)> = built
            .layout
            .entries
            .iter()
            .filter(|e| matches!(e.kind, ZoneKind::Slice { .. }))
            .map(|e| (e.position, e.size_bytes.unwrap()))
            .collect();
        let (pos, size) = slices[k];
        assert!(
            size > BS,
            "the slice spans blocks, for the warning to come in it"
        );
        let before: u64 = built
            .layout
            .entries
            .iter()
            .filter(|e| e.position < pos)
            .map(|e| e.size_bytes.unwrap().div_ceil(BS) * BS)
            .sum();
        store = store.with_early_warning_after(before + 1);
    }
    let validated = built
        .into_validated(&kav, SliceCheck::FullHash, &mut store)
        .expect("validate passes for a tape of real stagings");
    let planned = validated
        .plan(&h.conn, volume_id, &bu)
        .expect("plan inserts its rows");
    let ready = match planned.execute(&h.conn, &mut store).expect("execute") {
        ExecuteOutcome::Ready(r) => r,
        ExecuteOutcome::Interrupted(_) => panic!("unexpected interruption"),
        ExecuteOutcome::Aborted(a) => panic!("unexpected abort: {}", a.reason),
    };
    let ending = ready.ending();
    let pending = ready.seal(&mut store).expect("seal");
    match pending
        .confirm(&h.conn, &mut store, Tier::Integrity)
        .expect("confirm")
    {
        ConfirmOutcome::Sealed(_) => {}
        ConfirmOutcome::Quarantined(q) => panic!("quarantined: {:?}", q.reason),
        ConfirmOutcome::Inconclusive(i) => panic!("inconclusive: {:?}", i.evidence.mismatches),
    }
    let fi_len = store.files[3]
        .iter()
        .rposition(|b| *b != 0)
        .map(|i| i + 1)
        .unwrap();
    let entries =
        format::parse_front_index(&String::from_utf8_lossy(&store.files[3][..fi_len])).unwrap();
    let script_entry = &entries[2];
    assert_eq!(script_entry.type_label, "restore_sh");
    let script = store.files[2][..script_entry.size_bytes.unwrap() as usize].to_vec();
    drop(work);
    drop(session_dir);
    let files = store.files.clone();
    let end = files.len() - 2;
    let on_tape: Vec<usize> = entries
        .iter()
        .filter(|e| e.type_label == "data_slice" && (e.position as usize) <= end)
        .map(|e| e.position as usize)
        .collect();
    let mut runs = Vec::new();
    let mut at = 0usize;
    let all_slices: Vec<usize> = entries
        .iter()
        .filter(|e| e.type_label == "data_slice")
        .map(|e| e.position as usize)
        .collect();
    for u in &bu {
        let nums: Vec<i64> = u.slices.iter().map(|s| s.slice_number).collect();
        let mut held = Vec::new();
        for (i, num) in nums.iter().enumerate() {
            if on_tape.contains(&all_slices[at + i]) {
                held.push(*num);
            }
        }
        at += nums.len();
        runs.push((
            u.unit_name.clone(),
            *nums.first().unwrap(),
            *nums.last().unwrap(),
            held.first().copied().unwrap_or(0),
            held.last().copied().unwrap_or(0),
        ));
    }
    let mut unit_uuids = BTreeMap::new();
    for u in units {
        let uuid: String = h
            .conn
            .query_row(
                "SELECT uuid FROM units WHERE name = ?1",
                params![u.name],
                |r| r.get(0),
            )
            .unwrap();
        unit_uuids.insert(u.name.clone(), uuid);
    }
    Tape {
        files,
        units: units.to_vec(),
        keys: ctx.keys.clone(),
        ending,
        entries,
        script,
        label,
        volume_uuid,
        unit_uuids,
        runs,
        _home: ctx.tmp.clone(),
    }
}

impl Tape {
    /// Build a tape of `shape` from real stagings.
    pub fn build(shape: &Shape) -> Tape {
        Tape::build_series(shape).remove(0)
    }

    /// The tapes of `shape`: `shape.copies` tapes written from the same
    /// stagings, or, with a `restage`, the first tape and a second tape
    /// written from the second staging of the first unit alone. They share
    /// one catalog, so one set of keys and one uuid for each unit.
    pub fn build_series(shape: &Shape) -> Vec<Tape> {
        let tmp = std::sync::Arc::new(tempfile::tempdir().unwrap());
        let paths = TapectlPaths::new(tmp.path().join("home"));
        paths.ensure_dirs().unwrap();
        let conn = db::open(&paths.db_file).unwrap();
        let staging = tmp.path().join("staging");
        let sources = tmp.path().join("sources");
        fs::create_dir_all(&staging).unwrap();
        fs::create_dir_all(&sources).unwrap();
        let mut config = Config::default();
        config.staging.directory = staging.to_string_lossy().into_owned();
        config.defaults.slice_size = SLICE.to_string();
        config.defaults.compression = shape.compression.to_string();
        if let Some(sp) = &shape.span {
            config.defaults.min_copies = sp.min_copies;
        }
        tapectl::tenant::add_tenant(&conn, &paths, OPERATOR, None, true).unwrap();
        for tenant in [TENANT_A, TENANT_B] {
            tapectl::tenant::add_tenant(&conn, &paths, tenant, None, false).unwrap();
        }
        let escrow = tapectl::cli::key::create_escrow_recipient(&conn, &paths, None).unwrap();
        let h = Home {
            conn,
            paths,
            config,
            sources,
        };
        let mut units = Vec::new();
        for (i, (name, tenant, big)) in shape.units.iter().enumerate() {
            units.push(make_unit(
                &h,
                name,
                tenant,
                *big,
                shape.raw_names,
                1 + 7 * i as u32,
            ));
        }
        let mut keys = BTreeMap::new();
        keys.insert(key_name(Who::TenantA), secret_of(&h, TENANT_A));
        keys.insert(key_name(Who::TenantB), secret_of(&h, TENANT_B));
        keys.insert(key_name(Who::Operator), secret_of(&h, OPERATOR));
        keys.insert(key_name(Who::Escrow), escrow.keypair.secret_key.clone());
        let ctx = Ctx {
            escrow_public: escrow.keypair.public_key.clone(),
            keys,
            tmp: tmp.clone(),
        };
        let all = write::staged_build_units(&h.conn).expect("the staged selection");
        let mut tapes = Vec::new();
        match shape.restage {
            Restage::None => {
                if let Some(sp) = &shape.span {
                    for (n, warn) in sp.warns.iter().enumerate() {
                        let bu = write::staged_build_units_spanning(&h.conn, &h.config)
                            .expect("the spanning selection");
                        tapes.push(write_tape(&h, &ctx, shape, &units, bu, n, *warn));
                    }
                } else {
                    for n in 0..shape.copies.max(1) {
                        let warn = shape.warning_in_slice.filter(|_| n == shape.warn_tape);
                        tapes.push(write_tape(&h, &ctx, shape, &units, all.clone(), n, warn));
                    }
                }
            }
            r => {
                let warn0 = shape.warning_in_slice.filter(|_| shape.warn_tape == 0);
                tapes.push(write_tape(&h, &ctx, shape, &units, all.clone(), 0, warn0));
                // The first staging is released, as a write releases it; the
                // second is made after a change, as `stage create` makes it.
                let first = &shape.units[0];
                let old_set = all
                    .iter()
                    .find(|u| u.unit_name == first.0)
                    .unwrap()
                    .stage_set_id;
                h.conn
                    .execute(
                        "UPDATE stage_sets SET status = 'cleaned' WHERE id = ?1",
                        params![old_set],
                    )
                    .unwrap();
                let mut truth = units.clone();
                let dir = h.sources.join(&first.0);
                // Whole-second mtimes: let the change land in a later second.
                std::thread::sleep(std::time::Duration::from_millis(1100));
                match r {
                    Restage::Dotfile => {
                        let unit = tapectl::db::queries::get_unit_by_name(&h.conn, &first.0)
                            .unwrap()
                            .unwrap();
                        tapectl::unit::tag_unit(&h.conn, &unit, &["re-staged".to_string()], &[])
                            .unwrap();
                    }
                    _ => {
                        let path = format!("{}-small.txt", first.0);
                        let bytes = text(3_500, 99);
                        fs::write(dir.join(&path), &bytes).unwrap();
                        let t = &mut truth[0];
                        for (p, b) in t.files.iter_mut() {
                            if *p == path || p.ends_with("-hardlink.txt") {
                                *b = bytes.clone();
                            }
                        }
                    }
                }
                let snapshot =
                    tapectl::staging::snapshot_create(&h.conn, &first.0, &h.config).unwrap();
                let set =
                    tapectl::staging::stage_create(&h.conn, &h.paths, &h.config, snapshot, true)
                        .unwrap_or_else(|e| panic!("the second staging of {}: {e}", first.0));
                let slices: i64 = h
                    .conn
                    .query_row(
                        "SELECT COUNT(*) FROM stage_slices WHERE stage_set_id = ?1",
                        params![set],
                        |r| r.get(0),
                    )
                    .unwrap();
                truth[0].slices = slices as usize;
                let mut again = write::staged_build_units(&h.conn).expect("the staged selection");
                again.retain(|u| u.stage_set_id == set);
                assert_eq!(again.len(), 1, "the second staging alone");
                let warn1 = shape.warning_in_slice.filter(|_| shape.warn_tape == 1);
                tapes.push(write_tape(&h, &ctx, shape, &truth, again, 1, warn1));
            }
        }
        drop(h);
        tapes
    }

    /// The position of the last file before the seal.
    pub fn ended_after(&self) -> usize {
        self.files.len() - 2
    }

    /// The tape positions of every data slice.
    pub fn slice_positions(&self) -> Vec<usize> {
        self.entries
            .iter()
            .filter(|e| e.type_label == "data_slice")
            .map(|e| e.position as usize)
            .collect()
    }

    /// The data slices that are on this tape: the plan's, up to the seal.
    pub fn slices_on_tape(&self) -> Vec<usize> {
        let end = self.ended_after();
        self.slice_positions()
            .into_iter()
            .filter(|p| *p <= end)
            .collect()
    }

    /// The tape positions of the envelopes (primaries) and their backups.
    pub fn positions_of(&self, type_label: &str) -> Vec<usize> {
        self.entries
            .iter()
            .filter(|e| e.type_label == type_label)
            .map(|e| e.position as usize)
            .collect()
    }

    /// The size File 3 gives a file: the length an heir trims it to.
    pub fn size_of(&self, pos: usize) -> usize {
        self.entries[pos].size_bytes.unwrap() as usize
    }
}

// ---- the stand-in tools ----

/// `mt`: rewind, fsf N, bsfm N, eod, status, setblk, --version. The directory
/// tape is `$FAKE_FILES/<n>`; the head's place is `$FAKE_TAPE/pos`: the file
/// it is at the start of (`N`), inside of (`N+`), or `?` when it is lost.
/// Every call appends a line to `$FAKE_TAPE/ops`. `FAKE_MT_GNU=1` makes it
/// GNU cpio's mt (no setblk, its own status wording); `FAKE_MT_STATUS_LIE=1`
/// makes `status` report one file further on than the head really is.
const FAKE_MT: &str = r#"#!/bin/sh
T="$FAKE_TAPE"
D="$FAKE_FILES"
op=""; n=""
while [ $# -gt 0 ]; do
  case "$1" in
    -f) shift 2 ;;
    --version)
      if [ -n "${FAKE_MT_GNU:-}" ]; then echo "mt (GNU cpio) 2.15"; else echo "mt-st v. 1.7"; fi
      exit 0 ;;
    *) if [ -z "$op" ]; then op=$1; else n=$1; fi; shift ;;
  esac
done
echo "mt $op${n:+ $n}" >>"$T/ops"
pos=$(cat "$T/pos")
count=$(ls "$D/" | wc -l)
case "$op" in
  rewind) echo 0 >"$T/pos" ;;
  eod | eom) [ -z "${FAKE_MT_NO_EOD:-}" ] || exit 2; echo "$count" >"$T/pos" ;;
  bsfm)
    case "$pos" in
      *+) p=${pos%+} ;;
      *[!0-9]*) echo "?" >"$T/pos"; exit 1 ;;
      *) p=$pos ;;
    esac
    to=$(( p - n + 1 ))
    if [ "$to" -lt 0 ]; then echo 0 >"$T/pos"; exit 1; fi
    echo "$to" >"$T/pos" ;;
  fsf)
    case "$pos" in
      *+) to=$(( ${pos%+} + n )) ;;
      *[!0-9]*) echo "?" >"$T/pos"; exit 0 ;;
      *) to=$(( pos + n )) ;;
    esac
    if [ "$to" -gt "$count" ]; then
      echo "?" >"$T/pos"
      echo "/dev/nst0: Input/output error" >&2
      exit 1
    fi
    echo "$to" >"$T/pos" ;;
  status)
    case "$pos" in
      *+) f=${pos%+}; b=1 ;;
      *[!0-9]*) f=-1; b=-1 ;;
      *) f=$pos; b=0 ;;
    esac
    [ -z "${FAKE_MT_STATUS_LIE:-}" ] || f=$((f + 1))
    if [ -n "${FAKE_MT_GNU:-}" ]; then
      printf 'drive type = 114\ndrive status = 1124073472\nsense key error = 0\nresidue count = 0\nfile number = %s\nblock number = %s\n' "$f" "$b"
    else
      echo "SCSI 2 tape drive:"
      echo "File number=$f, block number=$b, partition=0."
      echo "Tape block size 524288 bytes. Density code 0x5a (LTO-6)."
    fi ;;
  setblk)
    if [ -n "${FAKE_MT_GNU:-}" ]; then
      echo "mt: invalid argument 'setblk' for 'operation'" >&2
      exit 2
    fi
    if [ -n "${FAKE_MT_SETBLK_FAILS:-}" ]; then
      echo "mt: /dev/nst0: setblk failed: No such device" >&2
      exit 2
    fi ;;
esac
exit 0
"#;

/// `dd if=<tape> [of=FILE] bs=...`: one tape file per call. `FAKE_DD_FAIL=N`
/// makes the read of file N fail with an I/O error and lose the position.
const FAKE_DD: &str = r#"#!/bin/sh
T="$FAKE_TAPE"
D="$FAKE_FILES"
of=""
for a in "$@"; do
  case "$a" in
    of=*) of=${a#of=} ;;
  esac
done
pos=$(cat "$T/pos")
echo "dd $pos" >>"$T/ops"
case "$pos" in
  *[!0-9]*)
    echo "fake dd: the head is not at the start of a file ($pos)" >&2
    echo "?" >"$T/pos"
    if [ -n "$of" ]; then printf 'NOT-A-FILE-START' >"$of"; else printf 'NOT-A-FILE-START'; fi
    exit 0 ;;
esac
if [ -n "${FAKE_DD_SLOW:-}" ] && [ "${FAKE_DD_SLOW%%:*}" = "$pos" ]; then
  : >"$T/dd.slow"
  sleep "${FAKE_DD_SLOW#*:}"
fi
if [ -e "$D/.fail.$pos" ]; then
  echo "dd: error reading '/dev/nst0': Input/output error" >&2
  echo "?" >"$T/pos"
  exit 1
fi
if [ "${FAKE_DD_FAIL:-none}" = "$pos" ]; then
  echo "dd: error reading '/dev/nst0': Input/output error" >&2
  echo "?" >"$T/pos"
  exit 1
fi
f="$D/$pos"
if [ ! -f "$f" ]; then echo "?" >"$T/pos"; exit 0; fi
if [ -n "$of" ]; then
  cat "$f" >"$of" || { echo "?" >"$T/pos"; exit 1; }
elif ! cat "$f"; then
  echo "$pos+" >"$T/pos"
  exit 1
fi
echo $((pos + 1)) >"$T/pos"
"#;

/// Tools the script runs, taken from the host: a PATH of exactly these (and
/// the three stand-ins) keeps a host's own `mt-st` or `age` out of a
/// run, and lets a test leave one out.
const HOST_TOOLS: &[&str] = &[
    "env",
    "bash",
    "sh",
    "awk",
    "sed",
    "grep",
    "tr",
    "head",
    "tail",
    "cat",
    "wc",
    "ls",
    "df",
    "mktemp",
    "mkdir",
    "rm",
    "rmdir",
    "chmod",
    "sort",
    "cut",
    "dirname",
    "basename",
    "sha256sum",
    "tar",
    "gzip",
    "xz",
    "zstd",
    "mv",
    "cp",
    "date",
    "touch",
    "ln",
    "sleep",
    "uniq",
    "true",
    "false",
    "expr",
    "stat",
    "id",
    "cmp",
    "find",
    "xargs",
    "timeout",
];

pub fn host_tool(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// The `age` stand-in: `examples/heir_age`, next to the test executable's
/// own directory. Fails loudly with the path it looked at.
pub fn heir_age_binary() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let deps = exe.parent().unwrap();
    let examples = deps.parent().unwrap().join("examples").join("heir_age");
    assert!(
        examples.is_file(),
        "the age stand-in is not built: expected {} (cargo test builds the examples; \
         with `--test heir_restore` alone run `cargo build --example heir_age` first)",
        examples.display()
    );
    examples
}

fn write_exec(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// The line RESTORE.sh prints when it waits for the cartridge that holds the
/// next slice it needs.
pub const PROMPT: &str = "Load the cartridge that holds slice ";

/// A run in progress (`Heir::spawn`).
pub struct Running {
    pub child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    out_path: PathBuf,
    asked: usize,
}

impl Running {
    /// Everything it has said so far, both streams.
    pub fn text(&self) -> String {
        fs::read_to_string(&self.out_path).unwrap_or_default()
    }
    pub fn type_line(&mut self, line: &str) {
        use std::io::Write;
        if let Some(w) = self.stdin.as_mut() {
            let _ = writeln!(w, "{line}");
            let _ = w.flush();
        }
    }
    pub fn close_input(&mut self) {
        self.stdin = None;
    }
    pub fn try_wait(&mut self) -> Option<i32> {
        self.child
            .try_wait()
            .unwrap()
            .map(|s| s.code().unwrap_or(-1))
    }
    /// Wait until the output holds `needle` the `n`-th time (at most `secs`).
    pub fn wait_for(&mut self, needle: &str, n: usize, secs: u64) -> String {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            let t = self.text();
            if t.matches(needle).count() >= n {
                return t;
            }
            if self.try_wait().is_some() || std::time::Instant::now() > until {
                panic!("waiting for {needle:?} x{n}:\n{t}");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    /// Send `kill -SIG` to the script alone.
    pub fn signal(&self, sig: &str) {
        let st = Command::new("kill")
            .args([format!("-{sig}"), self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(st.success(), "kill -{sig}");
    }
    /// Wait for the end (at most 60 s) and return what it said.
    pub fn finish(&mut self) -> Out {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if let Some(code) = self.try_wait() {
                return Out {
                    code,
                    stdout: self.text(),
                    stderr: String::new(),
                };
            }
            if std::time::Instant::now() > until {
                let _ = self.child.kill();
                panic!("still running:\n{}", self.text());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

/// What a run printed and how it ended.
pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    /// Everything it said, both streams.
    pub fn all(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }
}

/// A directory tape and the means to run RESTORE.sh against it.
pub struct Heir {
    pub root: tempfile::TempDir,
    pub keys: BTreeMap<&'static str, PathBuf>,
    script: PathBuf,
    pub entries: Vec<format::ParsedIndexEntry>,
    pub file_count: usize,
}

impl Heir {
    /// A directory tape of `tape`'s files as recorded.
    pub fn of(tape: &Tape) -> Heir {
        Heir::with_files(tape, tape.files.clone())
    }

    /// A drive with `tapes[0]` loaded and every tape of the series on the
    /// shelf (`load` puts one in the drive).
    pub fn of_series(tapes: &[&Tape]) -> Heir {
        let h = Heir::of(tapes[0]);
        for t in &tapes[1..] {
            h.shelve(&t.label, &t.files);
        }
        h
    }

    /// Put a cartridge of `files` on the shelf under `name`, block-padded.
    pub fn shelve(&self, name: &str, files: &[Vec<u8>]) -> PathBuf {
        let dir = self.root.path().join("cart").join(name);
        fs::create_dir_all(&dir).unwrap();
        for (n, bytes) in files.iter().enumerate() {
            let p = dir.join(n.to_string());
            fs::write(&p, bytes).unwrap();
            let blocks = bytes.len().div_ceil(BLOCK).max(1);
            fs::OpenOptions::new()
                .write(true)
                .open(&p)
                .unwrap()
                .set_len((blocks * BLOCK) as u64)
                .unwrap();
        }
        dir
    }

    /// A shelved cartridge with no file on it.
    pub fn shelve_blank(&self, name: &str) -> PathBuf {
        self.shelve(name, &[])
    }

    /// Copy the shelved cartridge `from` as `to`.
    pub fn copy_cart(&self, from: &str, to: &str) -> PathBuf {
        let src = self.root.path().join("cart").join(from);
        let dst = self.root.path().join("cart").join(to);
        fs::create_dir_all(&dst).unwrap();
        for e in fs::read_dir(&src).unwrap() {
            let e = e.unwrap();
            fs::copy(e.path(), dst.join(e.file_name())).unwrap();
        }
        dst
    }

    /// Make the read of file `n` of the shelved cartridge `name` fail with an
    /// I/O error, as a cartridge the drive cannot read does.
    pub fn fail_file(&self, name: &str, n: usize) {
        fs::write(
            self.root
                .path()
                .join("cart")
                .join(name)
                .join(format!(".fail.{n}")),
            "",
        )
        .unwrap();
    }

    /// The path of file `n` of the shelved cartridge `name`.
    pub fn cart_file(&self, name: &str, n: usize) -> PathBuf {
        self.root.path().join("cart").join(name).join(n.to_string())
    }

    /// The cartridge in the drive changes to `name`: the head is at the
    /// start of its first file, as after a load.
    pub fn load(&self, name: &str) {
        let r = self.root.path();
        let files = r.join("files");
        let _ = fs::remove_file(&files);
        std::os::unix::fs::symlink(r.join("cart").join(name), &files).unwrap();
        fs::write(r.join("state/pos"), "0").unwrap();
        use std::io::Write;
        let mut ops = fs::OpenOptions::new()
            .append(true)
            .open(r.join("state/ops"))
            .unwrap();
        writeln!(ops, "swap").unwrap();
    }

    /// The bay is emptied (the cartridge was ejected and none loaded): every
    /// `mt` and `dd` finds no cartridge, the head is nowhere.
    pub fn empty_bay(&self) {
        let r = self.root.path();
        let _ = fs::remove_file(r.join("files"));
        fs::write(r.join("state/pos"), "0").unwrap();
        use std::io::Write;
        let mut ops = fs::OpenOptions::new()
            .append(true)
            .open(r.join("state/ops"))
            .unwrap();
        writeln!(ops, "swap").unwrap();
    }

    /// The name of the cartridge in the drive.
    pub fn loaded(&self) -> String {
        fs::read_link(self.root.path().join("files"))
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    /// The tape files `dd` read, one list for each cartridge of the run (a
    /// `swap` in the log starts the next).
    pub fn reads_by_cartridge(&self) -> Vec<Vec<usize>> {
        let mut out = vec![Vec::new()];
        for l in self.ops() {
            if l == "swap" {
                out.push(Vec::new());
            } else if let Some(n) = l.strip_prefix("dd ").and_then(|n| n.parse().ok()) {
                out.last_mut().unwrap().push(n);
            }
        }
        out
    }

    /// Start the script with its standard input a pipe the test writes to;
    /// both its streams go to one file, so the order is the script's own.
    pub fn spawn(&self, args: &[&str], envs: &[(&str, &str)]) -> Running {
        let out_path = self.root.path().join("state/run.out");
        let out = fs::File::create(&out_path).unwrap();
        let mut c = Command::new("bash");
        c.arg(&self.script)
            .args(args)
            .env_clear()
            .env("PATH", self.bin())
            .env("HOME", self.root.path())
            .env("TMPDIR", self.root.path().join("tmp"))
            .env("FAKE_TAPE", self.root.path().join("state"))
            .env("FAKE_FILES", self.root.path().join("files"))
            .env("TAPE_DEVICE", "/dev/nst0")
            .stdin(Stdio::piped())
            .stdout(Stdio::from(out.try_clone().unwrap()))
            .stderr(Stdio::from(out));
        for (k, v) in envs {
            c.env(k, v);
        }
        fs::create_dir_all(self.root.path().join("tmp")).unwrap();
        let mut child = c.spawn().expect("spawn RESTORE.sh");
        let stdin = child.stdin.take();
        Running {
            child,
            stdin,
            out_path,
            asked: 0,
        }
    }

    /// Run with a person at the prompt: `on_prompt(k, text so far)` is
    /// called for the k-th time the script asks for a cartridge, and what it
    /// returns is typed in (`None` ends the input).
    pub fn run_prompted(
        &self,
        args: &[&str],
        mut on_prompt: impl FnMut(usize, &str) -> Option<String>,
    ) -> Out {
        let mut r = self.spawn(args, &[]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(280);
        loop {
            let text = r.text();
            let asked = text.matches(PROMPT).count();
            while r.asked < asked {
                r.asked += 1;
                match on_prompt(r.asked, &text) {
                    Some(line) => r.type_line(&line),
                    None => r.close_input(),
                }
            }
            if let Some(code) = r.try_wait() {
                return Out {
                    code,
                    stdout: r.text(),
                    stderr: String::new(),
                };
            }
            if std::time::Instant::now() > deadline {
                let _ = r.child.kill();
                panic!("RESTORE.sh {args:?} did not end:\n{text}");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// A directory tape of `files` (a copy of the tape's, damaged by the
    /// test), with the tape's keys and script.
    pub fn with_files(tape: &Tape, files: Vec<Vec<u8>>) -> Heir {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        // The cartridge in the drive is `files`, a link to `cart/<label>`.
        let cart = r.join("cart").join(&tape.label);
        fs::create_dir_all(&cart).unwrap();
        std::os::unix::fs::symlink(&cart, r.join("files")).unwrap();
        fs::create_dir_all(r.join("state")).unwrap();
        fs::create_dir_all(r.join("bin")).unwrap();
        fs::create_dir_all(r.join("keys")).unwrap();
        for (n, bytes) in files.iter().enumerate() {
            let p = r.join("files").join(n.to_string());
            fs::write(&p, bytes).unwrap();
            let blocks = bytes.len().div_ceil(BLOCK).max(1);
            fs::OpenOptions::new()
                .write(true)
                .open(&p)
                .unwrap()
                .set_len((blocks * BLOCK) as u64)
                .unwrap();
        }
        fs::write(r.join("state/pos"), "0").unwrap();
        fs::write(r.join("state/ops"), "").unwrap();
        let script = r.join("RESTORE.sh");
        fs::write(&script, &tape.script).unwrap();
        let mut keys = BTreeMap::new();
        for (name, secret) in &tape.keys {
            let p = r.join("keys").join(format!("{name}.key"));
            fs::write(&p, format!("# created: test\n{secret}\n")).unwrap();
            keys.insert(*name, p);
        }
        let bin = r.join("bin");
        write_exec(&bin.join("mt"), FAKE_MT);
        write_exec(&bin.join("dd"), FAKE_DD);
        std::os::unix::fs::symlink(heir_age_binary(), bin.join("age")).unwrap();
        for tool in HOST_TOOLS {
            if *tool == "dd" {
                continue;
            }
            if let Some(p) = host_tool(tool) {
                let _ = std::os::unix::fs::symlink(p, bin.join(tool));
            }
        }
        Heir {
            root,
            keys,
            script,
            entries: tape.entries.clone(),
            file_count: files.len(),
        }
    }

    pub fn bin(&self) -> PathBuf {
        self.root.path().join("bin")
    }

    pub fn key(&self, who: Who) -> &Path {
        &self.keys[key_name(who)]
    }

    /// The tape file `n`'s path in the directory tape.
    pub fn file(&self, n: usize) -> PathBuf {
        self.root.path().join("files").join(n.to_string())
    }

    /// Put a stand-in (or the host's own tool, by `path`) on the PATH as
    /// `name`, replacing what was there.
    pub fn put_tool(&self, name: &str, body: &str) {
        let p = self.bin().join(name);
        let _ = fs::remove_file(&p);
        write_exec(&p, body);
    }

    /// Link the host's `real` as `name`.
    pub fn link_tool(&self, name: &str, real: &Path) {
        let p = self.bin().join(name);
        let _ = fs::remove_file(&p);
        std::os::unix::fs::symlink(real, p).unwrap();
    }

    /// Leave a tool off the PATH.
    pub fn remove_tool(&self, name: &str) {
        let _ = fs::remove_file(self.bin().join(name));
    }

    /// The command: the script, run by bash, under `timeout`, with the
    /// stand-ins first and nothing else on the PATH.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new("timeout");
        c.arg("240")
            .arg("bash")
            .arg(&self.script)
            .args(args)
            .env_clear()
            .env("PATH", self.bin())
            .env("HOME", self.root.path())
            .env("TMPDIR", self.root.path().join("tmp"))
            .env("FAKE_TAPE", self.root.path().join("state"))
            .env("FAKE_FILES", self.root.path().join("files"))
            .env("TAPE_DEVICE", "/dev/nst0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        fs::create_dir_all(self.root.path().join("tmp")).unwrap();
        c
    }

    pub fn run(&self, args: &[&str]) -> Out {
        self.run_with(args, &[])
    }

    pub fn run_with(&self, args: &[&str], envs: &[(&str, &str)]) -> Out {
        let mut c = self.command(args);
        for (k, v) in envs {
            c.env(k, v);
        }
        let o = c.output().unwrap();
        Out {
            code: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    }

    /// The `mt` and `dd` calls so far, one per line.
    pub fn ops(&self) -> Vec<String> {
        fs::read_to_string(self.root.path().join("state/ops"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The tape files `dd` read, in order.
    pub fn dd_reads(&self) -> Vec<usize> {
        self.ops()
            .iter()
            .filter_map(|l| l.strip_prefix("dd ").and_then(|n| n.parse().ok()))
            .collect()
    }

    pub fn count_ops(&self, op: &str) -> usize {
        self.ops().iter().filter(|l| l.as_str() == op).count()
    }

    /// Forget the calls so far (a second run on the same tape).
    pub fn reset_ops(&self) {
        fs::write(self.root.path().join("state/ops"), "").unwrap();
        fs::write(self.root.path().join("state/pos"), "0").unwrap();
    }

    /// Flip one byte of tape file `n`, `offset` bytes in.
    pub fn flip(&self, n: usize, offset: usize) {
        let p = self.file(n);
        let mut b = fs::read(&p).unwrap();
        b[offset] ^= 0xFF;
        fs::write(&p, b).unwrap();
    }

    /// Replace tape file `n` with `bytes`, block-padded.
    pub fn replace(&self, n: usize, bytes: &[u8]) {
        let p = self.file(n);
        fs::write(&p, bytes).unwrap();
        let blocks = bytes.len().div_ceil(BLOCK).max(1);
        fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_len((blocks * BLOCK) as u64)
            .unwrap();
    }

    /// Remove tape file `n` and every one after it (a tape that ends there).
    pub fn cut_after(&self, n: usize) {
        for i in n..self.file_count {
            let _ = fs::remove_file(self.file(i));
        }
    }

    /// A fresh empty destination directory.
    pub fn dest(&self, name: &str) -> PathBuf {
        let p = self.root.path().join(name);
        fs::create_dir_all(&p).unwrap();
        p
    }
}

/// Every regular file under `dir` (relative path, bytes). A name that is
/// UTF-8 is its own key; one that is not is keyed by its escaped form
/// (`volume::index::escape`), never a lossy conversion. [`tree_bytes`] has
/// the exact names.
pub fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    tree_bytes(dir)
        .into_iter()
        .map(|(name, bytes)| {
            let key = match String::from_utf8(name) {
                Ok(text) => text,
                Err(e) => index::escape_bytes(e.as_bytes()).into_owned(),
            };
            (key, bytes)
        })
        .collect()
}

/// Every regular file under `dir`: its path's bytes relative to `dir`, and
/// its content.
pub fn tree_bytes(dir: &Path) -> BTreeMap<Vec<u8>, Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let mut out = BTreeMap::new();
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<Vec<u8>, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            let name = p
                .strip_prefix(base)
                .unwrap()
                .as_os_str()
                .as_bytes()
                .to_vec();
            let ft = e.file_type().unwrap();
            if ft.is_dir() {
                walk(base, &p, out);
            } else if ft.is_file() {
                out.insert(name, fs::read(&p).unwrap());
            }
        }
    }
    walk(dir, dir, &mut out);
    out
}

/// A tar stand-in that drops one file from the restored tree and exits 0, as
/// BusyBox tar does with files missing (invariant 6): the real tar runs, then
/// the file named by `victim` (relative to `-C`) is removed.
pub fn dropping_tar(real: &Path, victim: &str) -> String {
    format!(
        r#"#!/bin/bash
dest=""
args=("$@")
for ((i = 0; i < ${{#args[@]}}; i++)); do
  if [ "${{args[$i]}}" = -C ]; then dest=${{args[$((i + 1))]}}; fi
done
"{real}" "$@"
rc=$?
if [ -n "$dest" ] && [ -d "$dest" ]; then rm -f -- "$dest/{victim}"; fi
exit $rc
"#,
        real = real.display(),
        victim = victim
    )
}

// ---- editing the generated index (what a real catalog of one write does not make) ----

/// Rewrite a TSV file: `f` gets the header's column names and the rows (each a
/// vector of fields, as written, still escaped).
pub fn edit_tsv(path: &Path, f: impl FnOnce(&mut Vec<String>, &mut Vec<Vec<String>>)) {
    let text = fs::read_to_string(path).unwrap();
    let mut lines = text.lines();
    let mut header: Vec<String> = lines
        .next()
        .unwrap()
        .split('\t')
        .map(str::to_string)
        .collect();
    let mut rows: Vec<Vec<String>> = lines
        .map(|l| l.split('\t').map(str::to_string).collect())
        .collect();
    f(&mut header, &mut rows);
    let mut out = header.join("\t");
    out.push('\n');
    for r in rows {
        out.push_str(&r.join("\t"));
        out.push('\n');
    }
    fs::write(path, out).unwrap();
}

fn col(header: &[String], name: &str) -> usize {
    header.iter().position(|c| c == name).unwrap()
}

/// Every `units.tsv` of an index: the operator's, then each tenant's.
fn units_files(idx: &index::IndexFiles) -> Vec<PathBuf> {
    let mut v = vec![idx.operator_units.clone()];
    v.extend(idx.tenants.values().map(|t| t.units.clone()));
    v
}

/// Two earlier tapes that hold every version this tape holds: `OLD01`,
/// sealed, and `OLD02`, sealed and then proven bad.
pub fn index_holders(idx: &index::IndexFiles) {
    for p in units_files(idx) {
        edit_tsv(&p, |h, rows| {
            let mut extra = Vec::new();
            for r in rows.iter() {
                for (vol, state) in [("OLD01", "sealed"), ("OLD02", "quarantined")] {
                    let mut c = r.clone();
                    c[col(h, "volume")] = vol.to_string();
                    c[col(h, "volume_uuid")] =
                        format!("00000000-0000-4000-8000-0000000000{}", &vol[3..]);
                    c[col(h, "cartridge_serial")] = format!("SERIAL{vol}");
                    c[col(h, "state")] = state.to_string();
                    c[col(h, "sealed_at")] = "2026-10-01T00:00:00Z".to_string();
                    extra.push(c);
                }
            }
            // A third tape holds only the first slice of a unit of several.
            for r in rows.iter() {
                if r[col(h, "num_slices")].parse::<u32>().unwrap_or(1) > 1 {
                    let mut c = r.clone();
                    c[col(h, "volume")] = "OLD03".to_string();
                    c[col(h, "volume_uuid")] = "00000000-0000-4000-8000-000000000003".to_string();
                    c[col(h, "state")] = "sealed".to_string();
                    c[col(h, "sealed_at")] = "2026-10-02T00:00:00Z".to_string();
                    c[col(h, "first_slice")] = "1".to_string();
                    c[col(h, "last_slice")] = "1".to_string();
                    extra.push(c);
                }
            }
            rows.extend(extra);
        });
    }
}

/// The columns in the opposite order and one column this script does not
/// know, in every file of the index.
pub fn index_permuted(idx: &index::IndexFiles) {
    let mut files = units_files(idx);
    files.extend(idx.tenants.values().map(|t| t.paths.clone()));
    for p in files {
        edit_tsv(&p, |h, rows| {
            h.reverse();
            h.push("zzz-unknown".to_string());
            for r in rows.iter_mut() {
                r.reverse();
                r.push("something new".to_string());
            }
        });
    }
}

/// Sixty more units in every index file: each tenant's `units.tsv` and the
/// operator's, and sixty more paths in each tenant's `paths.tsv`, all
/// matching `bulk/`.
pub fn index_bulk(idx: &index::IndexFiles) {
    for (i, p) in units_files(idx).into_iter().enumerate() {
        edit_tsv(&p, |h, rows| {
            let tenant = col_opt(h, "tenant");
            for n in 0..60 {
                let mut r = vec![String::new(); h.len()];
                r[col(h, "unit")] = format!("bulk-{i}-{n:02}");
                r[col(h, "unit_uuid")] = format!("bbbbbbbb-0000-4000-8000-{i:04}{n:08}");
                r[col(h, "version")] = if n % 7 == 0 { "10".into() } else { "2".into() };
                r[col(h, "num_slices")] = "3".into();
                r[col(h, "first_slice")] = "1".into();
                r[col(h, "last_slice")] = "3".into();
                r[col(h, "version_bytes")] = "3000000".into();
                r[col(h, "volume")] = "OLD01".into();
                r[col(h, "volume_uuid")] = "00000000-0000-4000-8000-000000000001".into();
                r[col(h, "state")] = "sealed".into();
                r[col(h, "sealed_at")] = "2026-10-01T00:00:00Z".into();
                if let Some(t) = tenant {
                    r[t] = TENANT_A.into();
                }
                rows.push(r);
            }
        });
    }
    for t in idx.tenants.values() {
        edit_tsv(&t.paths, |h, rows| {
            for n in 0..60 {
                let mut r = vec![String::new(); h.len()];
                r[col(h, "unit")] = "bulk".into();
                r[col(h, "version")] = "2".into();
                r[col(h, "kind")] = "f".into();
                r[col(h, "size_bytes")] = "10".into();
                r[col(h, "path")] = format!("bulk/file-{n:02}.txt");
                rows.push(r);
            }
        });
    }
}

fn col_opt(header: &[String], name: &str) -> Option<usize> {
    header.iter().position(|c| c == name)
}
