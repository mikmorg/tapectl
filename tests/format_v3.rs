//! The keyless synthetic-heir harness: Layout 3's acceptance suite
//! (`docs/design/volume-format-v3.md`; the plan's T4.6).
//!
//! Every tape here is built from REAL stagings: tenants and keys made as
//! `tenant add` makes them, the escrow recipient as `key escrow` makes it,
//! units registered by `unit init` on made trees and staged by the real
//! `snapshot create` and `stage create` (GNU tar, the cut at member starts,
//! age). The tape is built from what `volume write` builds it from
//! ([`write::staged_build_units`], [`write::generate_tables`], the
//! cross-volume index and the on-tape `catalog.db`), and the full write
//! session (build -> validate -> plan -> execute -> seal -> confirm) runs
//! against a `MemStore`. No staged unit is made by hand (invariant 8).
//!
//! Then, using ONLY the bytes recorded in `MemStore.files`, never the
//! in-memory `Layout`, it verifies the tape as an heir with no database and
//! no tapectl would: parse File 3, walk the hash chain to the seal marker,
//! scan every plaintext file for what it must not say, and restore a
//! tenant's units by trial-decryption, the file table and the real `tar`.
//!
//! The ONE fact taken from the in-memory `Layout` is File 3's own true
//! (pre-padding) length: the tape omits it from File 3's own entry (§4), and
//! an heir recovers it by stripping trailing NUL padding instead.

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::OnceLock;

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

use tapectl::config::{Config, TapectlPaths};
use tapectl::db;
use tapectl::policy::coverage::IndexThisTape;
use tapectl::store::{MemStore, MismatchKind, Store, Tier};
use tapectl::volume::build::{self, BuildInputs, BuildUnit, BuiltLayout, TenantInfo};
use tapectl::volume::filetable;
use tapectl::volume::format;
use tapectl::volume::index;
use tapectl::volume::layout_model::{KeyAvailability, Layout, SliceCheck, ZoneKind};
use tapectl::volume::manifest::Manifest;
use tapectl::volume::session::{ConfirmOutcome, Ending, ExecuteOutcome};
use tapectl::volume::write;

/// The tape's block size: small, so a slice is many blocks and the drive's
/// early warning can come inside one (§5.2 W4).
const BS: u64 = 4096;
/// `[defaults] slice_size`: a few members to a slice, and a file larger
/// than it is a long member's run (§3.3, D22).
const SLICE: &str = "64K";
const LABEL: &str = "T46ACC";
const VOLUME_UUID: &str = "a1b2c3d4-e5f6-4789-a123-456789abcdef";
const SERIAL: &str = "T46SERIAL";

/// Names no tape could hold by chance: the isolation scan looks for each.
const OPERATOR: &str = "quandong-operator-acc";
const TENANT_A: &str = "loquat-tenant-alpha";
const TENANT_B: &str = "loquat-tenant-bravo";
const UNIT_A: &str = "loquat-unit-alpha";
const UNIT_B: &str = "loquat-unit-bravo";

/// The ids only one unit carries, in an ACL entry: no host names them
/// (§1.6, ADR-0012 item 277), so each is a row with no name in its unit's
/// map, and nowhere else.
const ID_ONLY_A: u32 = 4_242_917;
const ID_ONLY_B: u32 = 4_242_919;

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Deterministic incompressible bytes.
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

/// One unit as it was made: its files (relative path, bytes), and the id
/// its ACL entry names.
struct UnitTruth {
    name: &'static str,
    tenant: &'static str,
    stage_set_id: i64,
    files: Vec<(String, Vec<u8>)>,
    acl_id: u32,
}

/// A tapectl home with its operator, two tenants and the escrow recipient.
struct Home {
    conn: Connection,
    paths: TapectlPaths,
    config: Config,
    sources: PathBuf,
    escrow_public: String,
    _tmp: tempfile::TempDir,
}

fn home() -> Home {
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
    tapectl::tenant::add_tenant(&conn, &paths, OPERATOR, None, true).unwrap();
    for tenant in [TENANT_A, TENANT_B] {
        tapectl::tenant::add_tenant(&conn, &paths, tenant, None, false).unwrap();
    }
    let escrow = tapectl::cli::key::create_escrow_recipient(&conn, &paths, None).unwrap();
    Home {
        conn,
        paths,
        config,
        sources,
        escrow_public: escrow.keypair.public_key,
        _tmp: tmp,
    }
}

impl Home {
    /// `unit init` of a made tree, then `snapshot create` and `stage
    /// create`. The tree: a nested folder, a file larger than a slice (a
    /// long member's run), small files, a symbolic link, a hard link, and an
    /// ACL entry naming `acl_id`, an id no host names.
    fn unit(&self, tenant: &'static str, name: &'static str, acl_id: u32, seed: u32) -> UnitTruth {
        let dir = self.sources.join(name);
        fs::create_dir_all(dir.join("inner")).unwrap();
        let files: Vec<(String, Vec<u8>)> = vec![
            (format!("{name}-large.bin"), noise(150_000, seed)),
            (format!("{name}-small.txt"), noise(3_000, seed + 1)),
            (format!("inner/{name}-deep.dat"), noise(20_000, seed + 2)),
        ];
        for (path, bytes) in &files {
            fs::write(dir.join(path), bytes).unwrap();
        }
        std::os::unix::fs::symlink(format!("{name}-small.txt"), dir.join("a-symlink")).unwrap();
        fs::hard_link(
            dir.join(format!("{name}-small.txt")),
            dir.join(format!("{name}-hardlink.txt")),
        )
        .unwrap();
        let acl = std::process::Command::new("setfacl")
            .arg("-m")
            .arg(format!("u:{acl_id}:r"))
            .arg(dir.join(format!("{name}-small.txt")))
            .status()
            .is_ok_and(|s| s.success());
        assert!(acl, "setfacl must work on the test's file system");
        tapectl::unit::init_unit_with_config(
            &self.conn,
            &self.config,
            dir.to_str().unwrap(),
            tenant,
            Some(name),
            &[],
            None,
        )
        .unwrap();
        let snapshot = tapectl::staging::snapshot_create(&self.conn, name, &self.config).unwrap();
        let stage_set_id =
            tapectl::staging::stage_create(&self.conn, &self.paths, &self.config, snapshot, true)
                .unwrap_or_else(|e| panic!("stage create {name}: {e}"));
        UnitTruth {
            name,
            tenant,
            stage_set_id,
            files,
            acl_id,
        }
    }

    /// A tenant's active public keys, as the catalog records them.
    fn public_keys(&self, tenant: &str) -> Vec<String> {
        self.conn
            .prepare(
                "SELECT k.public_key FROM encryption_keys k JOIN tenants t ON t.id = k.tenant_id
                  WHERE t.name = ?1 AND k.is_active = 1 ORDER BY k.id",
            )
            .unwrap()
            .query_map(params![tenant], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn tenant_id(&self, tenant: &str) -> i64 {
        self.conn
            .query_row(
                "SELECT id FROM tenants WHERE name = ?1",
                params![tenant],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn identities(&self, tenant: &str) -> Vec<age::x25519::Identity> {
        let ids =
            tapectl::crypto::keys::load_tenant_identities(&self.conn, &self.paths.keys_dir, tenant)
                .unwrap();
        assert!(!ids.is_empty(), "{tenant} has a key");
        ids
    }

    /// Every key's `age1…` recipient and fingerprint, the escrow
    /// recipient's among them.
    fn key_strings(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .conn
            .prepare("SELECT public_key, fingerprint FROM encryption_keys")
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .flat_map(|r| {
                let (a, b) = r.unwrap();
                [a, b]
            })
            .collect();
        out.push(self.escrow_public.clone());
        out.sort();
        out.dedup();
        out
    }
}

/// What `volume write` builds a tape from, as it builds it: the staged
/// selection (refused as the write refuses it), each staging's file table
/// and id map, the cross-volume index and the on-tape `catalog.db`, all in
/// a work dir outside the session dir; then `build()`.
struct Built {
    built: BuiltLayout,
    units: Vec<BuildUnit>,
    keys: KeyAvailability,
    volume_id: i64,
    _dirs: Vec<tempfile::TempDir>,
}

fn build_tape(h: &Home) -> Built {
    let units = write::staged_build_units(&h.conn).expect("the staged selection");
    let ids: Vec<i64> = units.iter().map(|u| u.stage_set_id).collect();
    let work = tempfile::tempdir().unwrap();
    let index = index::generate_files(
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
    let tables = write::generate_tables(&h.conn, &units, &work.path().join("tables")).unwrap();
    let catalog_db = work.path().join("catalog.db");
    db::ontape_catalog::write(&h.conn, &ids, &catalog_db).unwrap();

    let mut tenant_ids: Vec<i64> = units.iter().map(|u| u.tenant_id).collect();
    tenant_ids.sort_unstable();
    tenant_ids.dedup();
    let tenants: Vec<TenantInfo> = [TENANT_A, TENANT_B]
        .into_iter()
        .map(|t| TenantInfo {
            tenant_id: h.tenant_id(t),
            tenant_name: t.to_string(),
            public_keys: h.public_keys(t),
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
        created_at: "2026-11-18T00:00:00Z".to_string(),
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
        units: units.clone(),
        tenants,
        operator_public_keys: h.public_keys(OPERATOR),
        escrow_public_key: Some(h.escrow_public.clone()),
        catalog_db_path: Some(catalog_db),
        index: Some(index),
        tables: Some(tables),
    };
    let session_dir = tempfile::tempdir().unwrap();
    let built = build::build(&inputs, session_dir.path()).expect("build succeeds");
    let keys = KeyAvailability {
        tenant_ids: tenant_ids.clone(),
        tenants_with_active_key: tenant_ids.iter().copied().collect(),
        operator_key_present: true,
        escrow_recipient_present: Some(true),
        stage_sets_lacking_escrow: None,
    };
    Built {
        built,
        units,
        keys,
        volume_id,
        _dirs: vec![work, session_dir],
    }
}

/// A sealed tape, its home and what each unit was made of. Shared across
/// test threads, so it keeps the home's paths and opens the catalog when
/// asked ([`Tape::home`]), never a `Connection` (which is not `Sync`).
struct Tape {
    paths: TapectlPaths,
    escrow_public: String,
    _tmp: tempfile::TempDir,
    layout: Layout,
    store: MemStore,
    ending: Ending,
    units: Vec<UnitTruth>,
}

/// The full write session over `b`, into `store`: validate, plan, execute,
/// seal, then confirm at the integrity tier, which must seal.
fn run_session(h: &Home, b: Built, mut store: MemStore) -> (Layout, MemStore, Ending) {
    let layout = b.built.layout.clone();
    let validated = b
        .built
        .into_validated(&b.keys, SliceCheck::FullHash, &mut store)
        .expect("validate passes for a tape of real stagings");
    let planned = validated
        .plan(&h.conn, b.volume_id, &b.units)
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
    drop(b._dirs);
    (layout, store, ending)
}

impl Tape {
    fn new(
        h: Home,
        layout: Layout,
        store: MemStore,
        ending: Ending,
        units: Vec<UnitTruth>,
    ) -> Tape {
        let Home {
            conn,
            paths,
            escrow_public,
            _tmp,
            ..
        } = h;
        drop(conn);
        Tape {
            paths,
            escrow_public,
            _tmp,
            layout,
            store,
            ending,
            units,
        }
    }

    /// The tape's home, its catalog opened again.
    fn home(&self) -> Home {
        Home {
            conn: db::open(&self.paths.db_file).unwrap(),
            paths: self.paths.clone(),
            config: Config::default(),
            sources: PathBuf::new(),
            escrow_public: self.escrow_public.clone(),
            _tmp: tempfile::tempdir().unwrap(),
        }
    }
}

fn two_unit_home() -> (Home, Vec<UnitTruth>) {
    let h = home();
    let a = h.unit(TENANT_A, UNIT_A, ID_ONLY_A, 1);
    let b = h.unit(TENANT_B, UNIT_B, ID_ONLY_B, 7);
    (h, vec![a, b])
}

/// The full tape, shared read-only by every test that needs one.
fn full_tape() -> &'static Tape {
    static TAPE: OnceLock<Tape> = OnceLock::new();
    TAPE.get_or_init(|| {
        let (h, units) = two_unit_home();
        let b = build_tape(&h);
        let (layout, store, ending) = run_session(&h, b, MemStore::new(BS as usize));
        assert_eq!(ending, Ending::Full);
        Tape::new(h, layout, store, ending, units)
    })
}

/// A tape the drive's early warning ended inside the first unit's second
/// slice: a short seal after it (§5.2 W4).
fn short_tape() -> &'static Tape {
    static TAPE: OnceLock<Tape> = OnceLock::new();
    TAPE.get_or_init(|| {
        let (h, units) = two_unit_home();
        let b = build_tape(&h);
        let layout = &b.built.layout;
        let slices: Vec<(i32, u64)> = layout
            .entries
            .iter()
            .filter(|e| matches!(e.kind, ZoneKind::Slice { .. }))
            .map(|e| (e.position, e.size_bytes.unwrap()))
            .collect();
        assert!(
            slices.len() >= 4,
            "the fixture plans several slices: {slices:?}"
        );
        let (k, k_size) = slices[1];
        assert!(
            k_size > BS,
            "slice {k} spans blocks, for the warning to come in it"
        );
        let before: u64 = layout
            .entries
            .iter()
            .filter(|e| e.position < k)
            .map(|e| e.size_bytes.unwrap().div_ceil(BS) * BS)
            .sum();
        let store = MemStore::new(BS as usize).with_early_warning_after(before + 1);
        let (layout, store, ending) = run_session(&h, b, store);
        assert_eq!(
            ending,
            Ending::Short {
                ended_after: k as u32
            }
        );
        Tape::new(h, layout, store, ending, units)
    })
}

// ── the keyless reader ──

fn front_index_true_len(layout: &Layout) -> usize {
    layout
        .entries
        .iter()
        .find(|e| matches!(e.kind, ZoneKind::FrontIndex))
        .and_then(|e| e.size_bytes)
        .expect("File 3 is sized") as usize
}

/// A file's true bytes as an heir reads them: the recorded bytes cut to the
/// front index's `size_bytes`.
fn true_bytes(files: &[Vec<u8>], e: &format::ParsedIndexEntry) -> Vec<u8> {
    let size = e.size_bytes.expect("sized") as usize;
    files[e.position as usize][..size].to_vec()
}

/// The seal marker: the last file recorded, as an heir finds it (§1.2 rule
/// 6: the last file before end of data).
fn seal_of(files: &[Vec<u8>]) -> format::ParsedSeal {
    format::parse_seal_marker(&String::from_utf8_lossy(files.last().unwrap())).unwrap()
}

fn front_index(t: &Tape) -> Vec<format::ParsedIndexEntry> {
    let fi = &t.store.files[3][..front_index_true_len(&t.layout)];
    format::parse_front_index(&String::from_utf8_lossy(fi)).unwrap()
}

/// The keyless chain walk (§5), from scratch and from recorded bytes only:
/// the seal binds File 3, File 3 binds every other file at or before the
/// seal's `ended_after`. `Err` names what broke; never a panic.
fn keyless_chain_walk(files: &[Vec<u8>], fi_true_len: usize) -> Result<u64, String> {
    let seal = format::parse_seal_marker(&String::from_utf8_lossy(
        files.last().ok_or("no files at all")?,
    ))
    .map_err(|e| format!("seal: {e}"))?;
    let fi = files.get(3).ok_or("no File 3")?;
    let fi = fi.get(..fi_true_len).ok_or("File 3 short")?;
    if sha256_hex(fi) != seal.front_index_sha256 {
        return Err("the seal does not bind File 3".to_string());
    }
    let entries = format::parse_front_index(&String::from_utf8_lossy(fi))
        .map_err(|e| format!("File 3: {e}"))?;
    let violations = format::validate_consistency(&entries);
    if !violations.is_empty() {
        return Err(format!("File 3 inconsistent: {violations:?}"));
    }
    let ended_after = seal.ended_after.ok_or("the seal states no ended_after")?;
    let mut checked = 0;
    for e in entries.iter().filter(|e| e.position as u64 <= ended_after) {
        if e.type_label == "front_index" {
            continue;
        }
        let (Some(size), Some(want)) = (e.size_bytes, e.sha256_encrypted.as_ref()) else {
            return Err(format!("position {} has no size or hash", e.position));
        };
        let padded = files
            .get(e.position as usize)
            .ok_or(format!("no file at {}", e.position))?;
        let got = sha256_hex(padded.get(..size as usize).ok_or("short file")?);
        if &got != want {
            return Err(format!("position {}: {got} is not {want}", e.position));
        }
        checked += 1;
    }
    Ok(checked)
}

/// Every tenant envelope (primaries and backups) a tenant's key opens:
/// `(position, members)`, members `(name, bytes)` in tar order.
/// One tar's members, `(name, bytes)` in order.
type Members = Vec<(String, Vec<u8>)>;

fn envelopes_opened_by(t: &Tape, tenant: &str) -> Vec<(i32, Members)> {
    let ids = t.home().identities(tenant);
    let mut out = Vec::new();
    for e in front_index(t).iter().filter(|e| {
        e.type_label.starts_with("tenant_envelope") || e.type_label.starts_with("operator_envelope")
    }) {
        let ct = true_bytes(&t.store.files, e);
        let Ok(reader) = age::Decryptor::new(&ct[..])
            .unwrap()
            .decrypt(ids.iter().map(|i| i as &dyn age::Identity))
        else {
            continue;
        };
        out.push((e.position, tar_members(reader)));
    }
    out
}

fn tar_members(reader: impl Read) -> Vec<(String, Vec<u8>)> {
    let mut archive = tar::Archive::new(reader);
    archive
        .entries()
        .unwrap()
        .map(|m| {
            let mut m = m.unwrap();
            let name = m.path().unwrap().to_string_lossy().into_owned();
            let mut bytes = Vec::new();
            m.read_to_end(&mut bytes).unwrap();
            (name, bytes)
        })
        .collect()
}

fn member<'a>(members: &'a [(String, Vec<u8>)], name: &str) -> &'a [u8] {
    &members
        .iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no {name}: {:?}", names(members)))
        .1
}

fn names(members: &[(String, Vec<u8>)]) -> Vec<&str> {
    members.iter().map(|(n, _)| n.as_str()).collect()
}

// ── the chain, the zones, File 0 ──

/// The keyless chain walk verifies every file of the full tape from its
/// recorded bytes alone, and `Store::confirm` agrees at the integrity tier.
#[test]
fn keyless_chain_walk_from_recorded_bytes_verifies_every_file() {
    let t = full_tape();
    let checked = keyless_chain_walk(&t.store.files, front_index_true_len(&t.layout)).unwrap();
    assert_eq!(
        checked as usize,
        t.store.files.len() - 2,
        "all but File 3 and the seal"
    );
    let mut store = MemStore::new(BS as usize);
    store.files = t.store.files.clone();
    let evidence = store.confirm(&t.layout, Tier::Integrity).unwrap();
    assert!(evidence.mismatches.is_empty(), "{evidence:?}");
}

/// §2.5's self-consistency on the parsed File 3, and the seal's
/// `file_count` and `ended_after` against it and against the recorded files.
#[test]
fn front_index_self_consistency_and_seal_file_count_match() {
    let t = full_tape();
    let entries = front_index(t);
    assert!(format::validate_consistency(&entries).is_empty());
    for (i, e) in entries.iter().enumerate() {
        assert_eq!(e.position, i as i32);
    }
    let seal = seal_of(&t.store.files);
    assert_eq!(seal.file_count as usize, entries.len());
    assert_eq!(seal.file_count as usize, t.store.files.len());
    assert_eq!(seal.ended_after, Some(entries.len() as u64 - 2));
    assert_eq!(seal.layout_version, 3);
    assert!(seal.requires.is_empty() && seal.requires_stated, "D1");
}

/// File 0 (§1.1, §1.2; D1): Layout 3's magic and layout version, `requires
/// = []` on the line after it, the planned seal and count, and `[media]`
/// last, saying how the serial was established, all read with no key from
/// the recorded bytes.
#[test]
fn file_0_states_layout_3_and_how_the_cartridge_serial_was_established() {
    let t = full_tape();
    let text = String::from_utf8_lossy(format::without_nul_padding(&t.store.files[0])).into_owned();
    assert!(text.contains("\nmagic = \"tapectl-volume-v3\"\n"), "{text}");
    assert!(
        text.contains("\nlayout_version = 3\nrequires = []\n"),
        "{text}"
    );
    format::check_id_thunk_readable(&text).unwrap();
    let pointers = format::parse_id_thunk_layout_pointers(&text).unwrap();
    assert_eq!(pointers.front_index, 3);
    assert_eq!(pointers.seal_marker as usize, t.store.files.len() - 1);
    let media = format::parse_id_thunk_media(&text).unwrap();
    assert_eq!(media.cartridge_serial, SERIAL);
    assert_eq!(media.cartridge_identity_source.as_deref(), Some("mam"));
    let at = text.find("[media]").unwrap();
    assert!(
        !text[at + "[media]".len()..].contains('['),
        "[media] is the last table"
    );
    let fi =
        String::from_utf8_lossy(&t.store.files[3][..front_index_true_len(&t.layout)]).into_owned();
    assert!(
        fi.contains("\nlayout_version = 3\nmay_end_early = true\n"),
        "{fi}"
    );
    assert!(!fi.contains("magic"), "File 3 carries no magic (item 294)");
}

/// §1 and §1.3: the four front files, the tenant envelopes, the operator
/// pair, then one backup per tenant in the primaries' order, then the slices
/// and the seal last; every backup is its primary's bytes (the same
/// `sha256_encrypted`), paired by hash, never by position.
#[test]
fn zone_order_and_the_backups_are_byte_copies_of_the_primaries() {
    let t = full_tape();
    let entries = front_index(t);
    let kinds: Vec<&str> = entries.iter().map(|e| e.type_label.as_str()).collect();
    assert_eq!(
        &kinds[..4],
        ["id_thunk", "system_guide", "restore_sh", "front_index"]
    );
    let tenants = 2;
    assert_eq!(&kinds[4..4 + tenants], ["tenant_envelope"; 2]);
    assert_eq!(
        &kinds[4 + tenants..6 + tenants],
        ["operator_envelope", "operator_envelope_backup"]
    );
    assert_eq!(
        &kinds[6 + tenants..6 + 2 * tenants],
        ["tenant_envelope_backup"; 2]
    );
    assert!(kinds[6 + 2 * tenants..kinds.len() - 1]
        .iter()
        .all(|k| *k == "data_slice"));
    assert_eq!(*kinds.last().unwrap(), "seal_marker");
    for (primary, backup) in [
        ("operator_envelope", "operator_envelope_backup"),
        ("tenant_envelope", "tenant_envelope_backup"),
    ] {
        let p: Vec<_> = entries.iter().filter(|e| e.type_label == primary).collect();
        let b: Vec<_> = entries.iter().filter(|e| e.type_label == backup).collect();
        assert_eq!(p.len(), b.len());
        for (p, b) in p.iter().zip(&b) {
            assert_eq!(
                p.sha256_encrypted, b.sha256_encrypted,
                "the primaries' order"
            );
            assert_eq!(true_bytes(&t.store.files, p), true_bytes(&t.store.files, b));
        }
    }
}

/// The fail-safe precedence: a tape with its seal gone reads as unsealed,
/// never a panic, and `Store::confirm` reports it as a mismatch.
#[test]
fn dropping_the_seal_marker_yields_unsealed_never_a_panic() {
    let t = full_tape();
    let mut files = t.store.files.clone();
    files.pop();
    assert!(keyless_chain_walk(&files, front_index_true_len(&t.layout)).is_err());
    let mut store = MemStore::new(BS as usize);
    store.files = files;
    let evidence = store.confirm(&t.layout, Tier::Integrity).unwrap();
    assert!(evidence
        .mismatches
        .iter()
        .any(|m| m.kind == MismatchKind::SealUnreadable));
}

// ── the envelopes ──

/// §1.6 and §1.9 (D4 to D7): each tenant's key opens its own envelope and
/// its backup and no other tenant's. Its MANIFEST block of its staging
/// states the payload, the stream's length, the tar line, the names of the
/// staging's two texts and every slice with its `continues`; the texts are
/// the catalog's file table and id map exactly. It holds none of the other
/// tenant's tables, ids or names. The operator's key opens every envelope;
/// the operator envelope carries no `tables/` member, and its `catalog.db`
/// carries both stagings' rows exactly.
#[test]
fn each_tenant_envelope_carries_its_own_tables_and_none_of_another_tenants() {
    let t = full_tape();
    for (unit, other) in [(&t.units[0], &t.units[1]), (&t.units[1], &t.units[0])] {
        let opened = envelopes_opened_by(t, unit.tenant);
        assert_eq!(
            opened.len(),
            2,
            "{}: its envelope and its backup",
            unit.tenant
        );
        for (position, members) in &opened {
            let manifest =
                Manifest::from_toml(std::str::from_utf8(member(members, "MANIFEST.toml")).unwrap())
                    .unwrap();
            assert_eq!(manifest.units.len(), 1, "{position}");
            let block = &manifest.units[0];
            assert_eq!(block.name, unit.name);
            assert_eq!(block.payload, "tar");
            assert_eq!(
                block.compression, None,
                "an uncompressed staging names none"
            );
            assert!(block
                .tar_version
                .as_deref()
                .is_some_and(|v| v.contains("GNU tar")));
            let (table_name, map_name) = tapectl::volume::manifest::table_names(
                &block.uuid,
                block.snapshot_version,
                block.stage_set_id,
            );
            assert_eq!(block.file_table.as_deref(), Some(table_name.as_str()));
            assert_eq!(block.id_map.as_deref(), Some(map_name.as_str()));
            let slices: Vec<(i64, bool)> = db_slices(&t.home().conn, unit.stage_set_id);
            assert_eq!(
                block
                    .slices
                    .iter()
                    .map(|s| (s.number, s.continues))
                    .collect::<Vec<_>>(),
                slices,
                "every slice, `continues` as staging recorded it"
            );
            assert!(slices.iter().any(|(_, c)| *c), "the large file is a run");

            let (stream_bytes, rows) =
                filetable::read_all(std::io::BufReader::new(member(members, &table_name))).unwrap();
            assert_eq!(stream_bytes as i64, block.stream_bytes);
            assert_eq!(
                rows,
                db::stage_files::load(&t.home().conn, unit.stage_set_id).unwrap()
            );
            let ids = tapectl::idmap::parse(member(members, &map_name)).unwrap();
            assert_eq!(
                ids,
                db::stage_ids::load(&t.home().conn, unit.stage_set_id).unwrap()
            );
            assert!(ids.iter().any(|r| r.id == unit.acl_id && r.name.is_none()));
            assert!(
                !ids.iter().any(|r| r.id == other.acl_id),
                "none of the other's ids"
            );
            for (name, bytes) in members {
                assert!(
                    !name.contains(other.name)
                        && !bytes
                            .windows(other.name.len())
                            .any(|w| w == other.name.as_bytes()),
                    "{}'s envelope says nothing of {} in {name}",
                    unit.tenant,
                    other.name
                );
            }
            assert_eq!(
                names(members)
                    .iter()
                    .filter(|n| n.starts_with("tables/"))
                    .count(),
                2
            );
        }
    }

    let operator = envelopes_opened_by(t, OPERATOR);
    assert_eq!(operator.len(), 6, "the operator opens every envelope");
    let op = front_index(t)
        .into_iter()
        .find(|e| e.type_label == "operator_envelope")
        .unwrap();
    let (_, members) = operator.iter().find(|(p, _)| *p == op.position).unwrap();
    assert!(!names(members).iter().any(|n| n.starts_with("tables/")));
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("catalog.db");
    fs::write(&db_path, member(members, "catalog.db")).unwrap();
    let (conn, cat) = db::ontape_catalog::read_all_but_files(&db_path).unwrap();
    for unit in &t.units {
        let rows: Vec<_> = db::ontape_catalog::file_table(&conn)
            .unwrap()
            .rows(unit.stage_set_id)
            .unwrap()
            .collect::<tapectl::error::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            db::stage_files::load(&t.home().conn, unit.stage_set_id).unwrap()
        );
        assert_eq!(
            db::ontape_catalog::id_map(&conn, unit.stage_set_id).unwrap(),
            db::stage_ids::load(&t.home().conn, unit.stage_set_id).unwrap()
        );
        let set = cat
            .stage_sets
            .iter()
            .find(|s| s.id == unit.stage_set_id)
            .unwrap();
        assert!(set.stream_bytes.is_some_and(|b| b > 0));
        assert!(cat
            .slices
            .iter()
            .any(|s| s.stage_set_id == unit.stage_set_id && s.continues));
    }
}

/// The slices staging recorded for a staging: `(number, continues)`.
fn db_slices(conn: &Connection, stage_set_id: i64) -> Vec<(i64, bool)> {
    conn.prepare(
        "SELECT slice_number, continues FROM stage_slices WHERE stage_set_id = ?1
          ORDER BY slice_number",
    )
    .unwrap()
    .query_map(params![stage_set_id], |r| Ok((r.get(0)?, r.get(1)?)))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// The heir path (§7.2) with a tenant's key and nothing else: find the
/// tenant's envelope by trial, read its MANIFEST and its file table, read
/// the staging's slices at their `tape_position`s, check each against File
/// 3, decrypt and join them: the stream is `stream_bytes` long, the real
/// `tar` restores it, and every file the table lists comes back with its
/// recorded sha256 (judged by the per-file check, invariant 6) and its
/// original bytes.
#[test]
fn keyed_heir_restore_from_the_slices_gives_every_file_back() {
    let t = full_tape();
    let entries = front_index(t);
    for unit in &t.units {
        let opened = envelopes_opened_by(t, unit.tenant);
        let (_, members) = &opened[0];
        let manifest =
            Manifest::from_toml(std::str::from_utf8(member(members, "MANIFEST.toml")).unwrap())
                .unwrap();
        let block = &manifest.units[0];
        let (stream_bytes, rows) = filetable::read_all(std::io::BufReader::new(member(
            members,
            block.file_table.as_deref().unwrap(),
        )))
        .unwrap();
        let ids = t.home().identities(unit.tenant);
        let mut stream = Vec::new();
        for slice in &block.slices {
            let pos = slice.tape_position.expect("a full tape holds every slice") as usize;
            let e = &entries[pos];
            assert_eq!(e.type_label, "data_slice");
            let ct = true_bytes(&t.store.files, e);
            assert_eq!(Some(&sha256_hex(&ct)), e.sha256_encrypted.as_ref(), "R9");
            assert_eq!(sha256_hex(&ct), slice.sha256_encrypted);
            let mut plain = Vec::new();
            age::Decryptor::new(&ct[..])
                .unwrap()
                .decrypt(ids.iter().map(|i| i as &dyn age::Identity))
                .unwrap()
                .read_to_end(&mut plain)
                .unwrap();
            assert_eq!(plain.len() as i64, slice.size_bytes);
            assert_eq!(sha256_hex(&plain), slice.sha256_plain);
            stream.extend_from_slice(&plain);
        }
        assert_eq!(stream.len() as u64, stream_bytes);

        let dest = tempfile::tempdir().unwrap();
        let mut tar = std::process::Command::new("tar")
            .args(["-x", "-f", "-", "--numeric-owner", "-C"])
            .arg(dest.path())
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        tar.stdin.take().unwrap().write_all(&stream).unwrap();
        assert!(tar.wait().unwrap().success());
        let mut checked = 0;
        for row in rows.iter().filter(|r| r.sha256.is_some()) {
            let bytes = fs::read(dest.path().join(&row.path)).unwrap();
            assert_eq!(
                sha256_hex(&bytes),
                db::files::sha256_to_hex(row.sha256.as_ref().unwrap()),
                "{}",
                row.path
            );
            checked += 1;
        }
        assert!(checked >= unit.files.len(), "{checked}");
        for (path, bytes) in &unit.files {
            assert_eq!(&fs::read(dest.path().join(path)).unwrap(), bytes, "{path}");
        }
    }
}

/// §1.3: a tenant envelope damaged on tape does not strand the tenant.
/// Keyless, the primary no longer matches File 3; the backup with the same
/// hash is the same envelope, and it opens with the tenant's own key, lists
/// the tenant's staging and carries its file table. Recorded bytes only.
#[test]
fn a_damaged_tenant_envelope_is_recovered_from_its_backup() {
    let t = full_tape();
    let entries = front_index(t);
    let unit = &t.units[0];
    let ids = t.home().identities(unit.tenant);
    let opens =
        |files: &[Vec<u8>], e: &format::ParsedIndexEntry| -> Option<Vec<(String, Vec<u8>)>> {
            let ct = true_bytes(files, e);
            if Some(sha256_hex(&ct)) != e.sha256_encrypted {
                return None;
            }
            let reader = age::Decryptor::new(&ct[..])
                .ok()?
                .decrypt(ids.iter().map(|i| i as &dyn age::Identity))
                .ok()?;
            Some(tar_members(reader))
        };
    let primary = entries
        .iter()
        .filter(|e| e.type_label == "tenant_envelope")
        .find(|e| opens(&t.store.files, e).is_some())
        .expect("positive control: the primary opens on an undamaged tape")
        .clone();
    let mut files = t.store.files.clone();
    let size = primary.size_bytes.unwrap() as usize;
    files[primary.position as usize][size / 2] ^= 0xff;
    assert!(
        opens(&files, &primary).is_none(),
        "the damaged primary does not open"
    );
    let backup = entries
        .iter()
        .find(|e| {
            e.type_label == "tenant_envelope_backup"
                && e.sha256_encrypted == primary.sha256_encrypted
        })
        .expect("every tenant envelope has a backup with its hash");
    let members = opens(&files, backup).expect("the backup opens with the tenant's key");
    let manifest =
        Manifest::from_toml(std::str::from_utf8(member(&members, "MANIFEST.toml")).unwrap())
            .unwrap();
    assert_eq!(manifest.units.len(), 1);
    assert_eq!(manifest.units[0].name, unit.name);
    member(&members, manifest.units[0].file_table.as_deref().unwrap());
    let err = keyless_chain_walk(&files, front_index_true_len(&t.layout)).unwrap_err();
    assert!(
        err.contains(&format!("position {}", primary.position)),
        "{err}"
    );
}

/// §1.2 rule 1: a seal that requires a feature this tapectl does not know
/// stops `Store::confirm` as inconclusive, naming it, never as evidence
/// against the medium. A key nobody requires is read past (rule 3).
#[test]
fn a_seal_requiring_an_unknown_feature_is_inconclusive_not_medium_bad() {
    let t = full_tape();
    let last = t.store.files.len() - 1;
    let seal = String::from_utf8_lossy(&t.store.files[last]).to_string();
    let stated = "requires = []";
    assert!(seal.contains(&format!("\n{stated}\n")), "D1: {seal}");
    let with = |s: String| {
        let mut store = MemStore::new(BS as usize);
        store.files = t.store.files.clone();
        let mut bytes = s.into_bytes();
        bytes.resize(t.store.files[last].len().max(bytes.len()), 0);
        store.files[last] = bytes;
        store.confirm(&t.layout, Tier::Integrity).unwrap()
    };
    let evidence = with(seal.replacen(stated, "requires = [\"unknown-feature\"]", 1));
    assert!(
        evidence
            .mismatches
            .iter()
            .any(|m| m.kind == MismatchKind::SealUnreadable
                && m.actual.contains("\"unknown-feature\"")),
        "{:?}",
        evidence.mismatches
    );
    assert!(!evidence.proves_medium_bad());
    let evidence = with(seal.replacen(stated, &format!("{stated}\nfuture_key = 7"), 1));
    assert!(evidence.mismatches.is_empty(), "{:?}", evidence.mismatches);
}

/// File 1 of a real build, for a volume whose `media_type` is LTO-6, says
/// which drives read it (#448's File 1 half). Recorded bytes only.
#[test]
fn file_1_of_a_built_lto6_tape_names_the_drives_that_read_it() {
    let t = full_tape();
    let guide =
        String::from_utf8_lossy(format::without_nul_padding(&t.store.files[1])).into_owned();
    let flat = guide.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains("This tape is LTO-6"), "{flat}");
    assert!(
        flat.contains("only if it is an LTO-6 or LTO-7 drive"),
        "{flat}"
    );
}

/// §1.5 (D8): each envelope carries its index last; a tenant's
/// `index/units.tsv` names this tape as File 0 does and only that tenant's
/// unit, its `index/paths.tsv` that unit's paths with their recorded
/// sha256 and none for the unit's own dotfile (#528), and nothing of the
/// other tenant's; the operator's `units.tsv` names both, with a `tenant`
/// column, and the operator envelope has no `paths.tsv`.
#[test]
fn the_index_names_this_tape_and_a_tenants_holds_none_of_the_others() {
    let t = full_tape();
    for (unit, other) in [(&t.units[0], &t.units[1]), (&t.units[1], &t.units[0])] {
        let opened = envelopes_opened_by(t, unit.tenant);
        let (_, members) = &opened[0];
        let n = names(members);
        assert_eq!(
            &n[n.len() - 2..],
            [index::UNITS_MEMBER, index::PATHS_MEMBER],
            "the index is last: {n:?}"
        );
        let units =
            index::parse_units(std::str::from_utf8(member(members, index::UNITS_MEMBER)).unwrap())
                .unwrap();
        assert_eq!(units.len(), 1, "{units:?}");
        assert_eq!(units[0].unit, unit.name);
        assert_eq!(units[0].volume, LABEL);
        assert_eq!(units[0].state, "this-tape");
        let paths =
            index::parse_paths(std::str::from_utf8(member(members, index::PATHS_MEMBER)).unwrap())
                .unwrap();
        assert!(paths.iter().all(|r| r.unit == unit.name));
        assert!(!paths.iter().any(|r| r.unit == other.name));
        for (path, bytes) in &unit.files {
            let row = paths.iter().find(|r| &r.path == path).unwrap();
            assert_eq!(row.sha256, sha256_hex(bytes), "{path}");
        }
        let dotfile = paths
            .iter()
            .find(|r| r.path == ".tapectl-unit.toml")
            .expect("the dotfile is listed");
        assert_eq!(dotfile.sha256, "", "D8: no hash for the unit's own dotfile");
    }
    let operator = envelopes_opened_by(t, OPERATOR);
    let op = front_index(t)
        .into_iter()
        .find(|e| e.type_label == "operator_envelope")
        .unwrap();
    let (_, members) = operator.iter().find(|(p, _)| *p == op.position).unwrap();
    assert!(!names(members).contains(&index::PATHS_MEMBER));
    let units =
        index::parse_units(std::str::from_utf8(member(members, index::UNITS_MEMBER)).unwrap())
            .unwrap();
    let mut listed: Vec<(&str, &str)> = units
        .iter()
        .map(|r| (r.tenant.as_str(), r.unit.as_str()))
        .collect();
    listed.sort_unstable();
    assert_eq!(listed, [(TENANT_A, UNIT_A), (TENANT_B, UNIT_B)]);
}

// ── the isolation invariant (§2) ──

/// Everything no plaintext file may reveal (§2; invariant 3), for `t`:
/// tenant, unit and file names, every file's sha256 and every slice's
/// `sha256_plain`, every key's recipient and fingerprint, and from the id
/// maps a user name, a group name and the ids one unit alone carries. Each
/// id needle comes with its positive control: it is in a decrypted id map.
fn needles(t: &Tape) -> Vec<String> {
    let home = t.home();
    let conn = &home.conn;
    let mut needles: Vec<String> = vec![
        OPERATOR.into(),
        TENANT_A.into(),
        TENANT_B.into(),
        UNIT_A.into(),
        UNIT_B.into(),
    ];
    for unit in &t.units {
        for (path, bytes) in &unit.files {
            needles.push(path.rsplit('/').next().unwrap().to_string());
            needles.push(sha256_hex(bytes));
        }
        for row in db::stage_files::load(conn, unit.stage_set_id).unwrap() {
            if let Some(h) = &row.sha256 {
                needles.push(db::files::sha256_to_hex(h));
            }
        }
        let plain: Vec<String> = conn
            .prepare("SELECT sha256_plain FROM stage_slices WHERE stage_set_id = ?1")
            .unwrap()
            .query_map(params![unit.stage_set_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        needles.extend(plain);
    }
    needles.extend(home.key_strings());
    needles.extend(
        id_needles(t)
            .into_iter()
            .flat_map(|(acl, names)| std::iter::once(acl).chain(names)),
    );
    needles.sort();
    needles.dedup();
    needles
}

/// The id map's needles, for each unit: its own ACL id, and the user and
/// group names the host gave the owner's ids. Each is in a decrypted
/// `.ids.tsv` (the positive control).
fn id_needles(t: &Tape) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for unit in &t.units {
        let opened = envelopes_opened_by(t, unit.tenant);
        let (_, members) = &opened[0];
        let map = members
            .iter()
            .find(|(n, _)| n.ends_with(".ids.tsv"))
            .map(|(_, b)| String::from_utf8(b.clone()).unwrap())
            .expect("an id map");
        let rows = tapectl::idmap::parse(map.as_bytes()).unwrap();
        let acl = unit.acl_id.to_string();
        assert!(
            map.contains(&format!("\t{acl}\t")),
            "positive control: {map}"
        );
        let mut host_names = Vec::new();
        for kind in [tapectl::idmap::IdKind::User, tapectl::idmap::IdKind::Group] {
            let named = rows
                .iter()
                .find(|r| r.kind == kind && r.name.is_some())
                .unwrap_or_else(|| panic!("the owner's {kind:?} id has a name: {rows:?}"));
            let name = named.name.clone().unwrap();
            assert!(
                map.contains(&format!("\t{name}\n")),
                "positive control: {map}"
            );
            host_names.push(name);
        }
        out.push((acl, host_names));
    }
    out
}

/// The plaintext files of a tape: Files 0 to 3 and the seal, `(position,
/// bytes)`.
fn plaintext_files(t: &Tape) -> Vec<(usize, Vec<u8>)> {
    let seal = t.store.files.len() - 1;
    [0, 1, 2, 3, seal]
        .into_iter()
        .map(|p| (p, format::without_nul_padding(&t.store.files[p]).to_vec()))
        .collect()
}

/// Whether `file` (at `position`) holds `needle`. Every needle counts
/// everywhere, with one exception for a host's user or group name
/// (`host_names`) in Files 1 and 2: such a name can be an English word
/// those files' generators write whatever the tape holds (`root` in "the
/// unit's root"). The exception is proven, not assumed: the same generator,
/// for another label and plan, writes the word too. Files 0 and 3 and the
/// seal are scanned for every needle without exception.
fn leaks(position: usize, file: &[u8], needle: &str, host_names: &[String]) -> bool {
    let found = file.windows(needle.len()).any(|w| w == needle.as_bytes());
    if !found || !(position == 1 || position == 2) || !host_names.iter().any(|n| n == needle) {
        return found;
    }
    let control = if position == 1 {
        tapectl::volume::layout::generate_system_guide_v2_for_media("CONTROL", 9, "LTO-6")
    } else {
        tapectl::volume::layout::generate_restore_script_v2("CONTROL", 9)
    };
    !control.contains(needle)
}

/// §2's machine check on real stagings: no plaintext file of the full tape
/// or of the short one (its seal at `ended_after + 1`) holds any needle. The
/// scan's positive control: a needle planted in a copy of File 3 is found.
#[test]
fn no_plaintext_file_reveals_a_name_a_hash_a_key_or_an_id() {
    for t in [full_tape(), short_tape()] {
        let needles = needles(t);
        let host_names: Vec<String> = id_needles(t).into_iter().flat_map(|(_, n)| n).collect();
        assert!(needles.len() > 20, "{needles:?}");
        for (position, bytes) in plaintext_files(t) {
            for needle in &needles {
                assert!(
                    !leaks(position, &bytes, needle, &host_names),
                    "plaintext file {position} holds {needle:?}"
                );
            }
        }
        let mut planted = plaintext_files(t)[3].1.clone();
        planted.extend_from_slice(needles[0].as_bytes());
        assert!(
            leaks(3, &planted, &needles[0], &host_names),
            "the scan finds a planted needle"
        );
    }
}

// ── a short tape (§5.1, §5.2) ──

/// The drive's early warning ended the tape inside the first unit's second
/// slice: a short seal at K+1 states `ended_after = K`, binds the same File
/// 3, and the keyless chain verifies every file at or before K. File 0, File
/// 3 and every envelope still describe the plan: the first unit's block and
/// file table list the slices after K (the seal withdraws them, §1.6), and
/// the second unit's slices are all after it.
#[test]
fn a_short_tape_seals_after_the_warning_and_its_envelopes_still_name_the_plan() {
    let t = short_tape();
    let Ending::Short { ended_after: k } = t.ending else {
        panic!("short")
    };
    let seal = seal_of(&t.store.files);
    assert_eq!(seal.ended_after, Some(u64::from(k)));
    assert_eq!(seal.file_count as u32, k + 2);
    assert_eq!(
        t.store.files.len() as u32,
        k + 2,
        "nothing follows the seal"
    );
    let checked = keyless_chain_walk(&t.store.files, front_index_true_len(&t.layout)).unwrap();
    assert_eq!(checked as u32, k, "files 0 to K but File 3");
    let entries = front_index(t);
    assert!(entries.len() as u32 > k + 2, "File 3 is the plan");
    let file0 =
        String::from_utf8_lossy(format::without_nul_padding(&t.store.files[0])).into_owned();
    let pointers = format::parse_id_thunk_layout_pointers(&file0).unwrap();
    assert_eq!(
        pointers.total_files as usize,
        entries.len(),
        "File 0 is the plan"
    );

    let opened = envelopes_opened_by(t, t.units[0].tenant);
    let (_, members) = &opened[0];
    let manifest =
        Manifest::from_toml(std::str::from_utf8(member(members, "MANIFEST.toml")).unwrap())
            .unwrap();
    let block = &manifest.units[0];
    let after: Vec<i64> = block
        .slices
        .iter()
        .filter(|s| s.tape_position.is_some_and(|p| p > i64::from(k)))
        .map(|s| s.number)
        .collect();
    assert!(
        !after.is_empty(),
        "the plan's slices after K are still listed"
    );
    let (_, rows) = filetable::read_all(std::io::BufReader::new(member(
        members,
        block.file_table.as_deref().unwrap(),
    )))
    .unwrap();
    assert!(
        rows.iter().any(|r| after.contains(&(r.slice as i64))),
        "the table lists members of slices after ended_after: the seal withdraws them"
    );
}

// ── a write without a table ──

/// Item 265 (#557): the selection `volume write` builds from refuses, by
/// name, a unit whose `stage_files` rows were deleted, saying to stage it
/// again; and `build` refuses a staging its tables do not cover.
#[test]
fn a_unit_whose_file_table_was_deleted_is_refused_by_name() {
    let (h, units) = two_unit_home();
    h.conn
        .execute(
            "DELETE FROM stage_files WHERE stage_set_id = ?1",
            params![units[1].stage_set_id],
        )
        .unwrap();
    let err = write::staged_build_units(&h.conn).unwrap_err().to_string();
    assert!(
        err.contains(&format!(
            "{UNIT_B} v1 (stage set {})",
            units[1].stage_set_id
        )),
        "{err}"
    );
    assert!(!err.contains(UNIT_A), "{err}");
    assert!(err.contains("Stage each version again"), "{err}");
    let dir = tempfile::tempdir().unwrap();
    assert!(write::generate_tables(&h.conn, &[], dir.path())
        .unwrap()
        .is_empty());
}

/// Each staging's tables are plaintext until they are in an envelope: none
/// of them is in the directory `build()` fills (staging's stand-in), under
/// any name, and none is left in the work dir's place under staging.
#[test]
fn the_tables_are_never_in_the_session_dir() {
    let (h, _units) = two_unit_home();
    let b = build_tape(&h);
    let mut stack = vec![b.built.session_dir.clone()];
    let mut scanned = 0;
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            scanned += 1;
            let bytes = fs::read(&p).unwrap();
            for needle in [filetable::HEADER, tapectl::idmap::HEADER, UNIT_A] {
                assert!(
                    !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                    "{needle} in {}",
                    p.display()
                );
            }
        }
    }
    assert!(scanned > 4, "the scan reached the session's files");
}
