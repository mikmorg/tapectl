//! Cost budgets for the tape read and write paths, at production scale
//! (issue #416, item 3).
//!
//! Every slowdown production found — #389's rewind per file, #396's
//! RESTORE.sh rewinds, #397's long locate to the seal — passed every gate,
//! because the suites asserted what a path READ and never what it COST, at
//! a tenth of production's scale, on a virtual tape that rewinds in no
//! time. This module drives each read path through the real `TapeStore`
//! over [`FakeTape`]'s distance model, on a tape laid out by the real write
//! session at production shape (a 150-slice unit and a 47-slice unit, each
//! slice modeled as 10 GiB of tape), and holds each to a budget of head
//! travel in multiples of the tape's recorded length.
//!
//! Each budget is the travel the path's design REQUIRES, plus a margin
//! for the small files around it, never the travel it happens to cost
//! today: a verify of a tape nothing has just seen reads the seal marker
//! first (`v2-open-questions.md` §2.5), so it pays one pass out, one rewind
//! and one pass back; a confirm straight after a write reads the seal last
//! (#397), so it pays the rewind and the pass. The positive control runs
//! the same path with the cursor distrusted ([`State::position_fails`]):
//! a rewind before every read, which is what every release before 1.0.5
//! did (#389), and every multi-file budget below fails under it.
//!
//! [`State::position_fails`]: crate::tape::fake::State::position_fails

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::config::Config;
use crate::crypto::keys::generate_keypair;
use crate::db;
use crate::store::{Checkpoint, Checkpoints, ConfirmPlan, ReadOrder, Store, TapeStore, Tier};
use crate::tape::contact::{ContactSite, Medium, Operation};
use crate::tape::fake::{FakeTape, Op};
use crate::volume::build::{self, BuildInputs, BuildSlice, BuildUnit, TenantInfo};
use crate::volume::format::ParsedIndexEntry;
use crate::volume::layout_model::{KeyAvailability, Layout, SliceCheck, ZoneKind};
use crate::volume::session::{ConfirmOutcome, ExecuteOutcome};

const BS: u64 = 65536;
const LABEL: &str = "COST01";
const VOL_UUID: &str = "41600000-0000-4000-8000-000000000416";
/// One slice's modeled length: production's 10 GiB `slice_size`.
const SLICE_TAPE: u64 = 10 * 1024 * 1024 * 1024;
/// The wide unit (#416 item 2's "~150 slices") and the narrow one.
const WIDE: usize = 150;
const NARROW: usize = 47;

fn sha_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The tape as the real session wrote and confirmed it, the catalog that
/// wrote it, and what the write itself cost.
struct Written {
    conn: Connection,
    fake: FakeTape,
    layout: Layout,
    volume_id: i64,
    operator_secret: String,
    config: Config,
    /// `(travel, ops)` of the write, and of the confirm that followed it in
    /// the same contact.
    write: (u64, Vec<Op>),
    confirm: (u64, Vec<Op>),
    _dirs: Vec<TempDir>,
}

/// One unit's catalog rows and staged slices.
fn stage_unit(
    conn: &Connection,
    dir: &std::path::Path,
    tenant_id: i64,
    name: &str,
    slices: usize,
) -> BuildUnit {
    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, current_path, status)
         VALUES (?1, ?2, ?3, ?4, 'active')",
        params![
            format!("uuid-{name}"),
            name,
            tenant_id,
            format!("/src/{name}")
        ],
    )
    .unwrap();
    let unit_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
         VALUES (?1, 1, 'staged', ?2, 1, 64)",
        params![unit_id, format!("/src/{name}")],
    )
    .unwrap();
    let snapshot_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', ?2)",
        params![snapshot_id, SLICE_TAPE as i64],
    )
    .unwrap();
    let stage_set_id = conn.last_insert_rowid();
    let mut built = Vec::with_capacity(slices);
    for n in 1..=slices as i64 {
        let content = format!("{name}: staged ciphertext of slice {n}").into_bytes();
        let sha = sha_hex(&content);
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                       sha256_plain, sha256_encrypted)
             VALUES (?1, ?2, ?3, ?3, ?4, ?5)",
            params![
                stage_set_id,
                n,
                content.len() as i64,
                sha_hex(b"plain"),
                sha
            ],
        )
        .unwrap();
        let slice_id = conn.last_insert_rowid();
        let path = dir.join(format!("slice_{slice_id}.age"));
        std::fs::write(&path, &content).unwrap();
        conn.execute(
            "UPDATE stage_slices SET staging_path = ?1 WHERE id = ?2",
            params![path.to_string_lossy(), slice_id],
        )
        .unwrap();
        built.push(BuildSlice {
            slice_id,
            slice_number: n,
            size_bytes: content.len() as i64,
            encrypted_bytes: content.len() as i64,
            sha256_plain: sha_hex(b"plain"),
            sha256_encrypted: sha,
            staging_path: path,
        });
    }
    BuildUnit {
        stage_set_id,
        snapshot_id,
        unit_name: name.to_string(),
        unit_uuid: format!("uuid-{name}"),
        tenant_id,
        dar_version: Some("2.7.20".to_string()),
        dar_command: None,
        catalog_path: None,
        snapshot_version: 1,
        slices: built,
    }
}

/// Write and confirm a production-shaped volume through the real session
/// and the real `TapeStore`, every slice modeled as [`SLICE_TAPE`].
fn written() -> Written {
    let db_dir = tempfile::tempdir().unwrap();
    let conn = db::open(&db_dir.path().join("catalog.db")).unwrap();
    let operator = generate_keypair();
    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('operator', 1, 'active')",
        [],
    )
    .unwrap();
    let alpha = generate_keypair();
    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('alpha', 0, 'active')",
        [],
    )
    .unwrap();
    let tenant_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO volumes (label, uuid, backend_type, backend_name, media_type,
                              capacity_bytes, status)
         VALUES (?1, ?2, 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
        params![LABEL, VOL_UUID],
    )
    .unwrap();
    let volume_id = conn.last_insert_rowid();

    let slices_dir = tempfile::tempdir().unwrap();
    let units = vec![
        stage_unit(&conn, slices_dir.path(), tenant_id, "wide", WIDE),
        stage_unit(&conn, slices_dir.path(), tenant_id, "narrow", NARROW),
    ];
    let inputs = BuildInputs {
        label: LABEL.to_string(),
        volume_uuid: VOL_UUID.to_string(),
        media_type: "LTO-6".to_string(),
        tapectl_version: "0.1.0-test".to_string(),
        created_at: "2026-10-07T00:00:00Z".to_string(),
        block_size: BS,
        usable_bytes: 1024 * 1024 * 1024,
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
            tenant_name: "alpha".to_string(),
            public_keys: vec![alpha.public_key.clone()],
        }],
        operator_public_keys: vec![operator.public_key.clone()],
        escrow_public_key: None,
        catalog_db_path: None,
    };
    let session_dir = tempfile::tempdir().unwrap();
    let built = build::build(&inputs, session_dir.path()).unwrap();
    let layout = built.layout.clone();

    let fake = FakeTape::with_files(Vec::new(), BS as usize);
    for e in &layout.entries {
        if matches!(e.kind, ZoneKind::Slice { .. }) {
            fake.model_len(e.position as u32, SLICE_TAPE);
        }
    }
    let keys = KeyAvailability {
        tenant_ids: vec![tenant_id],
        tenants_with_active_key: [tenant_id].into_iter().collect(),
        operator_key_present: true,
        escrow_recipient_present: None,
        stage_sets_lacking_escrow: None,
    };
    let mut store = TapeStore::from_ops(fake.boxed(), u64::MAX).unwrap();
    let validated = built
        .into_validated(&keys, SliceCheck::Size, &mut store)
        .unwrap();
    let planned = validated.plan(&conn, volume_id, &units).unwrap();
    fake.load();
    let ExecuteOutcome::Ready(ready) = planned.execute(&conn, &mut store).unwrap() else {
        panic!("the write must reach Ready");
    };
    let sealed = ready.seal(&mut store).unwrap();
    let write = (fake.travel(), fake.ops());
    let before = fake.travel();
    fake.state().ops.clear();
    let ConfirmOutcome::Sealed(_) = sealed.confirm(&conn, &mut store, Tier::Integrity).unwrap()
    else {
        panic!("the write must seal and confirm");
    };
    let confirm = (fake.travel() - before, fake.ops());

    let staging = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.staging.directory = staging.path().to_string_lossy().into_owned();
    Written {
        conn,
        fake,
        layout,
        volume_id,
        operator_secret: operator.secret_key,
        config,
        write,
        confirm,
        _dirs: vec![db_dir, slices_dir, session_dir, staging],
    }
}

/// One read path, as a new contact runs it: the cartridge just loaded, a
/// store opened over it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Path {
    VerifyFull,
    VerifyQuick,
    Rebuild,
    Raw,
    Envelope,
    ReadSlices,
    CompactRead,
    Resume,
    Binding,
}

const PATHS: [Path; 9] = [
    Path::VerifyFull,
    Path::VerifyQuick,
    Path::Rebuild,
    Path::Raw,
    Path::Envelope,
    Path::ReadSlices,
    Path::CompactRead,
    Path::Resume,
    Path::Binding,
];

/// What one run of a path cost.
#[derive(Debug)]
struct Cost {
    /// Head travel in multiples of the recorded tape's length.
    lengths: f64,
    rewinds: usize,
}

impl Written {
    fn site(&self, operation: Operation) -> ContactSite<'_> {
        ContactSite::new(&self.config, operation, "fake", Medium::NoBackend)
    }

    fn entry(&self, kind: fn(&ZoneKind) -> bool) -> ParsedIndexEntry {
        let e = self
            .layout
            .entries
            .iter()
            .find(|e| kind(&e.kind))
            .expect("the layout has the entry");
        ParsedIndexEntry {
            position: e.position,
            type_label: e.kind.type_label().to_string(),
            size_bytes: e.size_bytes,
            sha256_encrypted: e.sha256.clone(),
        }
    }

    /// The checkpoints a full readback interrupted halfway through the
    /// wide unit's slices left (#410), taken by a real checkpointing run.
    fn halfway_checkpoints(&self) -> Checkpoints {
        let seen = std::cell::RefCell::new(Vec::<(u32, String, String)>::new());
        let record = |c: Checkpoint<'_>| {
            seen.borrow_mut().push((
                c.position,
                c.sha256.to_string(),
                c.front_index_sha256.to_string(),
            ))
        };
        self.fake.load();
        let mut store = TapeStore::from_ops(self.fake.boxed(), 0).unwrap();
        store
            .confirm_with(
                &self.layout,
                ConfirmPlan::new(Tier::Integrity)
                    .with_order(ReadOrder::SealLast)
                    .checkpointing(&record),
            )
            .unwrap();
        let halfway = self
            .layout
            .entries
            .iter()
            .filter(|e| matches!(e.kind, ZoneKind::Slice { .. }))
            .nth(WIDE / 2)
            .unwrap()
            .position as u32;
        let seen = seen.into_inner();
        Checkpoints {
            front_index_sha256: seen[0].2.clone(),
            passed: seen
                .into_iter()
                .filter(|(p, _, _)| *p < halfway)
                .map(|(p, sha, _)| (p, sha))
                .collect(),
        }
    }

    /// Run `path` as a new contact and return what it cost. With
    /// `rewind_per_read`, `MTIOCGET` fails throughout, so the store trusts
    /// no cursor and repositions from BOT before every read (#389's
    /// pre-1.0.5 access pattern).
    fn run(&self, path: Path, rewind_per_read: bool) -> Cost {
        self.fake.state().position_fails = false;
        let checkpoints = (path == Path::Resume).then(|| self.halfway_checkpoints());
        let scratch = tempfile::tempdir().unwrap();
        self.fake.load();
        self.fake.state().position_fails = rewind_per_read;
        let mut store = TapeStore::from_ops(self.fake.boxed(), 0).unwrap();
        match path {
            Path::VerifyFull | Path::VerifyQuick => {
                let tier = if path == Path::VerifyFull {
                    Tier::Integrity
                } else {
                    Tier::Navigable
                };
                let r = crate::volume::write::volume_verify_with_store(
                    &self.conn,
                    &mut store,
                    LABEL,
                    self.volume_id,
                    BS as usize,
                    tier,
                    self.site(Operation::VolumeVerify),
                )
                .unwrap();
                assert!(r.mismatches.is_empty(), "{:?}", r.mismatches);
            }
            Path::Rebuild => {
                let fresh = db::open_memory().unwrap();
                let identity: age::x25519::Identity = self.operator_secret.parse().unwrap();
                crate::volume::rebuild::rebuild_from_store(
                    &fresh,
                    &mut store,
                    &[identity],
                    Some(LABEL),
                    "recovered",
                    Some("lto0"),
                    scratch.path(),
                    "fake",
                    self.site(Operation::CatalogRebuild),
                )
                .unwrap();
            }
            Path::Raw => {
                crate::volume::raw::restore_raw(&mut store, scratch.path(), Some(LABEL)).unwrap();
            }
            Path::Envelope => {
                let identity: age::x25519::Identity = self.operator_secret.parse().unwrap();
                let entry = self.entry(|k| matches!(k, ZoneKind::OperatorEnvelope));
                crate::volume::envelope::open_envelope(
                    &mut store,
                    &entry,
                    &[identity],
                    scratch.path(),
                )
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            }
            Path::ReadSlices => {
                crate::volume::write::read_slices(
                    &self.conn,
                    &self.config,
                    LABEL,
                    "narrow",
                    &mut store,
                    self.site(Operation::VolumeReadSlices),
                )
                .unwrap();
            }
            Path::CompactRead => {
                crate::volume::write::compact_read(
                    &self.conn,
                    &self.config,
                    LABEL,
                    &mut store,
                    self.site(Operation::VolumeCompactRead),
                )
                .unwrap();
            }
            Path::Resume => {
                let evidence = store
                    .confirm_with(
                        &self.layout,
                        ConfirmPlan::new(Tier::Integrity)
                            .with_order(ReadOrder::SealLast)
                            .resuming(checkpoints.as_ref()),
                    )
                    .unwrap();
                assert!(evidence.mismatches.is_empty(), "{:?}", evidence.mismatches);
            }
            Path::Binding => {
                let facts = crate::volume::binding::read_file0_facts(&mut store);
                assert_eq!(facts.uuid.as_deref(), Some(VOL_UUID), "{facts:?}");
            }
        }
        let cost = Cost {
            lengths: self.fake.travel() as f64 / self.fake.tape_length() as f64,
            rewinds: self.fake.moving_rewinds(),
        };
        self.fake.state().position_fails = false;
        cost
    }

    /// Every path's cost, in [`PATHS`] order.
    fn costs(&self, rewind_per_read: bool) -> Vec<(Path, Cost)> {
        PATHS
            .iter()
            .map(|p| (*p, self.run(*p, rewind_per_read)))
            .collect()
    }
}

/// The written tape, built once for the whole module: a 197-slice session
/// is the slowest part of every test here.
fn tape() -> std::sync::MutexGuard<'static, Written> {
    static TAPE: std::sync::OnceLock<std::sync::Mutex<Written>> = std::sync::OnceLock::new();
    TAPE.get_or_init(|| std::sync::Mutex::new(written()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Each path's budget: `(most head travel in tape lengths, most rewinds that
/// move tape)`. A rewind at BOT — every open's — is free and not counted.
/// The comment on each is the motion its design calls for; each was
/// measured on 2026-10-07 at exactly that.
fn budget(path: Path) -> (f64, usize) {
    match path {
        // File 0 and File 3 for the contact, then seal first (§2.5: a tape
        // nothing has just seen): out to the seal, one rewind, one pass.
        Path::VerifyFull => (3.05, 1),
        // File 3 first, then a space out to the seal (#397). The rewind is
        // the contact's head read of File 3 being read again whole — a few
        // files from BOT, so nearly free in travel, but counted.
        Path::VerifyQuick => (1.05, 1),
        // File 0, File 3 and the operator envelope, all before the slices
        // (format §1). Rebuild never reads the seal.
        Path::Rebuild => (0.01, 0),
        // File 0 and File 3 heads, a rewind a few files from BOT, then one
        // forward pass over every file.
        Path::Raw => (1.05, 1),
        // One envelope, before the slices.
        Path::Envelope => (0.01, 0),
        // File 0, then a space over the wide unit to the narrow unit's
        // slices, which end just before the seal: one forward pass.
        Path::ReadSlices => (1.05, 0),
        // File 0, then every live slice in position order: one pass.
        Path::CompactRead => (1.05, 0),
        // File 3, a space past what the interrupted readback passed (#410),
        // the rest and the seal: one pass.
        Path::Resume => (1.05, 0),
        // File 0 alone.
        Path::Binding => (0.01, 0),
    }
}

fn over_budget(path: Path, cost: &Cost) -> bool {
    let (lengths, rewinds) = budget(path);
    cost.lengths > lengths || cost.rewinds > rewinds
}

/// A write lays the tape down once, and the confirm straight after it is
/// one rewind and one forward pass that reads the seal last (#397) — never
/// a locate out to the seal and back.
#[test]
fn a_write_and_its_confirm_stay_within_their_budgets() {
    let w = tape();
    let length = w.fake.tape_length() as f64;
    assert!(
        length >= (WIDE + NARROW) as f64 * SLICE_TAPE as f64,
        "positive control: the tape is modeled at production scale"
    );
    let (write, ops) = &w.write;
    assert!(
        (*write as f64 / length) <= 1.0001,
        "the write lays the tape down once: {}",
        *write as f64 / length
    );
    assert!(
        !ops.iter()
            .any(|op| matches!(op, Op::Read(_) | Op::ReadHead(_))),
        "the write reads nothing back: {ops:?}"
    );
    let (confirm, ops) = &w.confirm;
    let rewinds = ops.iter().filter(|op| **op == Op::Rewind).count();
    assert!(
        (*confirm as f64 / length) <= 2.05 && rewinds == 1,
        "confirm after a write: one rewind, one pass ({} lengths, {rewinds} rewinds)",
        *confirm as f64 / length
    );
}

/// The confirm budget's positive control (#397's class): from where a
/// write leaves the head — end of data — the seal-last pass the session
/// uses keeps to one rewind and one pass, and the seal-first order every
/// release before #397 used (rewind, out to the seal, rewind again, pass)
/// breaks the same budget.
#[test]
fn a_seal_first_confirm_after_a_write_breaks_the_confirm_budget() {
    let w = tape();
    let run = |order: ReadOrder| {
        w.fake.load();
        let end = w.fake.state().files.len();
        w.fake.state().head = (end, 0);
        let mut store = TapeStore::from_ops(w.fake.boxed(), 0).unwrap();
        let evidence = store
            .confirm_with(
                &w.layout,
                ConfirmPlan::new(Tier::Integrity).with_order(order),
            )
            .unwrap();
        assert!(evidence.mismatches.is_empty(), "{:?}", evidence.mismatches);
        let lengths = w.fake.travel() as f64 / w.fake.tape_length() as f64;
        (lengths, w.fake.moving_rewinds())
    };
    let (lengths, rewinds) = run(ReadOrder::SealLast);
    assert!(lengths <= 2.05 && rewinds == 1, "{lengths} {rewinds}");
    let (lengths, rewinds) = run(ReadOrder::SealFirst);
    assert!(
        lengths > 2.05 && rewinds > 1,
        "positive control: seal first after a write is out and back again: {lengths} {rewinds}"
    );
}

/// Every read path within its budget at production scale.
#[test]
fn every_read_path_stays_within_its_budget() {
    let w = tape();
    let costs = w.costs(false);
    let over: Vec<_> = costs.iter().filter(|(p, c)| over_budget(*p, c)).collect();
    assert!(over.is_empty(), "over budget: {over:?}\nall: {costs:#?}");
}

/// The positive control (#416's acceptance): with the cursor distrusted,
/// `TapeStore` repositions from BOT before every read — every release
/// before 1.0.5 (#389) — and every path that reads more than one file
/// breaks its budget. A budget this cannot break would be measuring
/// nothing.
#[test]
fn the_pre_1_0_5_rewind_per_read_breaks_every_multi_file_budget() {
    let w = tape();
    let costs = w.costs(true);
    let broken: Vec<Path> = costs
        .iter()
        .filter(|(p, c)| over_budget(*p, c))
        .map(|(p, _)| *p)
        .collect();
    assert_eq!(
        broken,
        PATHS
            .iter()
            .copied()
            .filter(|p| !matches!(p, Path::Envelope | Path::Binding))
            .collect::<Vec<_>>(),
        "every path that reads more than one file breaks its budget; a one-file path \
         cannot: {costs:#?}"
    );
    for (path, cost) in &costs {
        if matches!(
            path,
            Path::VerifyFull | Path::Raw | Path::ReadSlices | Path::CompactRead | Path::Resume
        ) {
            assert!(
                cost.lengths > 10.0,
                "{path:?}: a rewind per slice read costs many tape lengths: {cost:?}"
            );
        }
    }
}
