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
use tapectl::policy::coverage::IndexThisTape;
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
        }
    }
    /// One big unit of tenant A, compressed.
    pub fn one(compression: &'static str) -> Shape {
        Shape {
            units: vec![(UNIT_A.into(), TENANT_A, true)],
            compression,
            warning_in_slice: None,
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
    _home: tempfile::TempDir,
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
fn make_unit(h: &Home, name: &str, tenant: &'static str, big: bool, seed: u32) -> UnitTruth {
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
        files.push(("notes.txt".to_string(), text(40_000, seed + 31)));
    }
    for (path, bytes) in &files {
        fs::write(dir.join(path), bytes).unwrap();
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
        slices: slices as usize,
    }
}

impl Tape {
    /// Build a tape of `shape` from real stagings.
    pub fn build(shape: &Shape) -> Tape {
        let tmp = tempfile::tempdir().unwrap();
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
            units.push(make_unit(&h, name, tenant, *big, 1 + 7 * i as u32));
        }
        let mut keys = BTreeMap::new();
        keys.insert(key_name(Who::TenantA), secret_of(&h, TENANT_A));
        keys.insert(key_name(Who::TenantB), secret_of(&h, TENANT_B));
        keys.insert(key_name(Who::Operator), secret_of(&h, OPERATOR));
        keys.insert(key_name(Who::Escrow), escrow.keypair.secret_key.clone());

        let bu = write::staged_build_units(&h.conn).expect("the staged selection");
        let ids: Vec<i64> = bu.iter().map(|u| u.stage_set_id).collect();
        let work = tempfile::tempdir().unwrap();
        let idx = index::generate_files(
            &h.conn,
            &IndexThisTape {
                label: LABEL,
                uuid: VOLUME_UUID,
                cartridge_serial: SERIAL,
                generation: "LTO-6",
            },
            &ids,
            &work.path().join("index"),
        )
        .unwrap();
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
                params![LABEL, VOLUME_UUID],
            )
            .unwrap();
        let volume_id = h.conn.last_insert_rowid();
        let inputs = BuildInputs {
            label: LABEL.to_string(),
            volume_uuid: VOLUME_UUID.to_string(),
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
            escrow_public_key: Some(escrow.keypair.public_key.clone()),
            catalog_db_path: Some(catalog_db),
            index: Some(idx),
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
        if let Some(k) = shape.warning_in_slice {
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
        drop(h);
        Tape {
            files,
            units,
            keys,
            ending,
            entries,
            script,
            _home: tmp,
        }
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
count=$(ls "$D" | wc -l)
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
/// the three stand-ins) keeps a host's own `mt-st`, `age` or `dar` out of a
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

    /// A directory tape of `files` (a copy of the tape's, damaged by the
    /// test), with the tape's keys and script.
    pub fn with_files(tape: &Tape, files: Vec<Vec<u8>>) -> Heir {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        fs::create_dir_all(r.join("files")).unwrap();
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

/// Every regular file under `dir` (relative path, bytes).
pub fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            let name = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
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
