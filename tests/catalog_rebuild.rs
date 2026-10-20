//! `catalog rebuild --from-volume` (#136) against a volume written by the
//! real write session.
//!
//! The tape here is not a hand-rolled fixture: `build_sealed_volume` runs the
//! production build -> validate -> plan -> execute -> seal -> confirm chain
//! into a `MemStore`, and the rebuild then reads back only what those bytes
//! say. A harness that assembled the envelopes itself would be a fixture
//! simpler than the artifact — it would keep passing after a change to the
//! real packing broke every real tape.
//!
//! The load-bearing assertion is `restore_resolution_query`: the very
//! resolution `volume::restore::restore_unit` performs. A rebuild that inserts rows
//! which merely look plausible but do not satisfy that query has rebuilt
//! nothing an operator can restore from.

use std::path::Path;

use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use tapectl::crypto::keys::{generate_keypair, GeneratedKeypair};
use tapectl::db;
use tapectl::staging;
use tapectl::store::{MemStore, Store, Tier};
use tapectl::tape::contact::{ContactSite, Medium, Operation};
use tapectl::tape::mam::MamInfo;

/// A `MemStore` rebuild has no configured drive behind it; these are the
/// long-lived stand-ins the `ContactSite` borrows.
fn cfg() -> &'static tapectl::config::Config {
    static CFG: std::sync::OnceLock<tapectl::config::Config> = std::sync::OnceLock::new();
    CFG.get_or_init(tapectl::config::Config::default)
}

fn backend() -> &'static tapectl::config::LtoBackendConfig {
    static BK: std::sync::OnceLock<tapectl::config::LtoBackendConfig> = std::sync::OnceLock::new();
    BK.get_or_init(|| tapectl::config::LtoBackendConfig {
        name: "lto0".to_string(),
        device_tape: "/nonexistent/tapectl-rebuild-test-nst".to_string(),
        device_sg: "/nonexistent/tapectl-rebuild-test-sg".to_string(),
        generation: "LTO-6".to_string(),
        capacity_override: None,
        fill_ceiling: 0.95,
        enospc_buffer: "1GiB".to_string(),
        post_warning_bytes: None,
    })
}
use tapectl::volume::build::{self, BuildInputs, BuildSlice, BuildUnit, TenantInfo};
use tapectl::volume::layout_model::{KeyAvailability, SliceCheck};
use tapectl::volume::rebuild;
use tapectl::volume::session::{ConfirmOutcome, ExecuteOutcome};

const BS: u64 = 65536;
const LABEL: &str = "REBUILD01";
const VOL_UUID: &str = "11111111-2222-3333-4444-555555555555";

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

struct TenantFixture {
    tenant_id: i64,
    name: String,
    public_key: String,
    secret_key: String,
}

/// The units this fixture writes, as `(unit name, tenant name, content)`.
const UNITS: &[(&str, &str, &[u8])] = &[
    ("photos/2019", "alpha", b"alpha photos payload"),
    ("photos/2020", "alpha", b"alpha more photos payload"),
    ("ledgers/fy24", "bravo", b"bravo ledger payload"),
];

/// Which `catalog.db` the operator envelope carries.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CatalogDb {
    /// None at all — a tape written before #83.
    None,
    /// Generation 1's shape, the pre-2026-09-11 one: `files` with no file
    /// kinds, and no `tenants`, no `key_fingerprints`, no `sha256_plain`.
    /// Made by `ontape_catalog::fixture::downgrade` from this tape's own
    /// generation-3 file, so it names this tape's slices; its `files.id`
    /// numbering is that of master's never-shipped post-030 writer (from 1),
    /// not the shipped writers' (the catalog's own ids, verbatim). Files the
    /// shipped writers made are pinned in `db::ontape_catalog`'s tests
    /// (`tests/fixtures/ontape_catalog/`).
    Old,
    /// Generation 2's shape, 2026-09-11 until 1.1.0: generation 1 plus
    /// ownership and receipts, still `files` with no file kinds. Made the
    /// same way, with the same caveat.
    Gen2,
    /// Generation 3, what `build_catalog_snapshot` writes today (1.1.0 on).
    New,
}

struct SealedVolume {
    /// File 0's label — [`LABEL`] unless the test wrote a second tape.
    label: String,
    store: MemStore,
    /// The catalog that WROTE this tape, kept open (issue #379: a rebuild
    /// into a live catalog that still holds the stage set). Declared before
    /// its directory so it closes first.
    source_conn: rusqlite::Connection,
    _source_dir: tempfile::TempDir,
    operator_secret: String,
    escrow_secret: String,
    escrow_public: String,
    tenant_secret: String,
    /// Unit name -> the tape positions its slices landed on, in slice order.
    expected_positions: Vec<(String, Vec<i64>)>,
    /// Unit name -> its slice's `sha256_plain`, as staged.
    expected_plain_hashes: Vec<(String, String)>,
}

fn insert_tenant_keyed(
    conn: &rusqlite::Connection,
    name: &str,
    is_operator: bool,
    kp: GeneratedKeypair,
) -> TenantFixture {
    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES (?1, ?2, 'active')",
        rusqlite::params![name, is_operator as i64],
    )
    .unwrap();
    let tenant_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key, key_type, is_active)
         VALUES (?1, ?2, ?3, ?4, 'primary', 1)",
        rusqlite::params![tenant_id, format!("{name}-primary"), kp.fingerprint, kp.public_key],
    )
    .unwrap();
    TenantFixture {
        tenant_id,
        name: name.to_string(),
        public_key: kp.public_key,
        secret_key: kp.secret_key,
    }
}

/// Run the full production write session into a `MemStore` and return the
/// sealed result. `with_catalog_db` selects whether the operator envelope
/// carries #83's `catalog.db` — false stands in for a tape written before it.
fn build_sealed_volume(with_catalog_db: bool) -> SealedVolume {
    build_sealed_volume_with(if with_catalog_db {
        CatalogDb::New
    } else {
        CatalogDb::None
    })
}

thread_local! {
    /// When set, every unit's stage set also has the dar catalogue
    /// `stage create` leaves it (`<uuid8>_v7.1.dar`, bytes
    /// [`catalogue_bytes`]) and the units carry uuids whose first eight
    /// characters differ, as real uuids' do (issue #473).
    static WITH_CATALOGUES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// What the fixture's dar catalogue of `unit_name` holds.
fn catalogue_bytes(unit_name: &str) -> Vec<u8> {
    format!("dar catalogue of {unit_name}").into_bytes()
}

/// How the next fixture tape's cross-volume index (§1.5, #418 R4) is made.
#[derive(Clone, Copy, PartialEq, Eq)]
enum IndexKind {
    /// None: a tape written before 1.3.0.
    None,
    /// The catalog also holds two OTHER sealed tapes, `HIST-A` (a later
    /// version of one alpha unit and one bravo unit) and `HIST-Q`
    /// (quarantined, a still later version of the alpha unit); the index
    /// names both beside this tape.
    History,
    /// `History`, but the operator's `units.tsv` is overwritten with a file
    /// that has no `volume` column.
    Unreadable,
}

thread_local! {
    static INDEX_KIND: std::cell::Cell<IndexKind> = const { std::cell::Cell::new(IndexKind::None) };
}

/// [`build_sealed_volume_with`] with `kind`'s cross-volume index.
fn build_sealed_volume_with_index(kind: IndexKind) -> SealedVolume {
    INDEX_KIND.with(|k| k.set(kind));
    let vol = build_sealed_volume_with(CatalogDb::New);
    INDEX_KIND.with(|k| k.set(IndexKind::None));
    vol
}

thread_local! {
    /// When set, the first unit's version also holds a symlink and a special
    /// file ([`link_and_special`]), as the walk records them.
    static WITH_LINK_AND_SPECIAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A symlink and a FIFO in `unit_name`, as the walk records them: the kind,
/// lstat's size, no hash, and the symlink's target.
fn link_and_special(unit_name: &str) -> Vec<db::files::FileEntry> {
    use db::files::{FileEntry, FileKind};
    vec![
        FileEntry {
            path: format!("{unit_name}/latest"),
            kind: FileKind::Symlink,
            size_bytes: 9,
            mtime_ns: Some(1_788_264_000_000_000_000),
            sha256: None,
            link_target: Some("file0.bin".into()),
        },
        FileEntry {
            path: format!("{unit_name}/pipe"),
            kind: FileKind::Special,
            size_bytes: 0,
            mtime_ns: Some(1_788_264_000_000_000_000),
            sha256: None,
            link_target: None,
        },
    ]
}

thread_local! {
    /// When set, the fixture's first tenant (`alpha` in [`UNITS`]) is named
    /// this, and the operator's own tenant row is named `opadmin`: a catalog
    /// whose operator is not called `operator`, with a REAL tenant that is
    /// (issue #459 item 2).
    static ALPHA_TENANT_NAME: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
}

/// [`build_sealed_volume_with`], with `alpha` named `operator` — the name
/// the operator envelope's manifest carries as its placeholder tenant.
fn build_sealed_volume_with_a_tenant_named_operator(catalog_db: CatalogDb) -> SealedVolume {
    ALPHA_TENANT_NAME.with(|n| n.set(Some("operator")));
    let vol = build_sealed_volume_with(catalog_db);
    ALPHA_TENANT_NAME.with(|n| n.set(None));
    vol
}

thread_local! {
    /// SQL [`build_sealed_volume_editing_catalog_db`] runs against the
    /// `catalog.db` before it goes into the operator envelope: a stand-in for
    /// a value an older tapectl wrote that today's catalog cannot hold.
    static CATALOG_DB_EDIT: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// [`build_sealed_volume_with`], with `sql` run against its `catalog.db`
/// before the envelope is sealed.
fn build_sealed_volume_editing_catalog_db(catalog_db: CatalogDb, sql: &str) -> SealedVolume {
    assert!(
        catalog_db != CatalogDb::None,
        "there is no catalog.db to edit"
    );
    CATALOG_DB_EDIT.with(|e| *e.borrow_mut() = Some(sql.to_string()));
    build_sealed_volume_with(catalog_db)
}

/// Every existing test in this file wants a `mam`-sourced serial and does
/// not care what it is — `"REBUILDSERIAL"` was always meant to represent one
/// (issue #165's cartridge-binding tests are the first to need the other
/// shapes File 0 can carry).
fn build_sealed_volume_with(catalog_db: CatalogDb) -> SealedVolume {
    build_sealed_volume_full(catalog_db, "REBUILDSERIAL", Some("mam"))
}

/// [`build_sealed_volume_with`], parameterized on the cartridge identity File
/// 0 will carry (issue #165) — the same three shapes `rebuild`'s own
/// resolution distinguishes:
///
/// - `(serial, Some("mam"))` — a chip-reported serial.
/// - `(barcode, Some("operator"))` — an operator-typed barcode.
/// - `(serial_or_empty, None)` — UNKNOWN: either the ADR-0010-to-#192 window
///   (a non-empty serial with no identity source recorded) or a fully legacy
///   write (`""`, standing in for a File 0 with no `[media]` table at all —
///   `classify_media` treats both the same way, so this fixture does not
///   need to fabricate a thunk with the whole table missing to exercise it).
fn build_sealed_volume_full(
    catalog_db: CatalogDb,
    mam_serial: &str,
    cartridge_identity_source: Option<&str>,
) -> SealedVolume {
    build_tape(TapeSpec {
        catalog_db,
        mam_serial,
        cartridge_identity_source,
        label: LABEL,
        volume_uuid: VOL_UUID,
        keys: &FixtureKeys::fresh(),
        slices_per_unit: 2,
    })
}

/// The four keypairs one fixture tape is written with. Two tapes of the same
/// catalog (issue #379) share one set, so one operator key opens both.
struct FixtureKeys {
    operator: GeneratedKeypair,
    alpha: GeneratedKeypair,
    bravo: GeneratedKeypair,
    escrow: GeneratedKeypair,
}

fn copy_keypair(kp: &GeneratedKeypair) -> GeneratedKeypair {
    GeneratedKeypair {
        public_key: kp.public_key.clone(),
        secret_key: kp.secret_key.clone(),
        fingerprint: kp.fingerprint.clone(),
    }
}

impl FixtureKeys {
    fn fresh() -> Self {
        FixtureKeys {
            operator: generate_keypair(),
            alpha: generate_keypair(),
            bravo: generate_keypair(),
            escrow: generate_keypair(),
        }
    }
}

/// Everything [`build_tape`] varies.
struct TapeSpec<'a> {
    catalog_db: CatalogDb,
    mam_serial: &'a str,
    cartridge_identity_source: Option<&'a str>,
    label: &'a str,
    volume_uuid: &'a str,
    keys: &'a FixtureKeys,
    /// Slices per unit: two everywhere but the one test that pins the
    /// single-slice boundary of `ensure_stage_set`'s step 2.
    slices_per_unit: i64,
}

/// Run the production write session for [`UNITS`] at version 7 into a
/// `MemStore`, in a catalog of its own. Each call stages afresh, so two
/// calls write the same content under different ciphertext — exactly what
/// re-staging a Version for a later copy produces (issue #379).
fn build_tape(spec: TapeSpec<'_>) -> SealedVolume {
    let TapeSpec {
        catalog_db,
        mam_serial,
        cartridge_identity_source,
        label,
        volume_uuid,
        keys,
        slices_per_unit,
    } = spec;
    let db_dir = tempfile::tempdir().unwrap();
    let conn = db::open(&db_dir.path().join("src.db")).unwrap();

    let alpha_name = ALPHA_TENANT_NAME.with(|n| n.get()).unwrap_or("alpha");
    let operator_name = if alpha_name == "operator" {
        "opadmin"
    } else {
        "operator"
    };
    let operator = insert_tenant_keyed(&conn, operator_name, true, copy_keypair(&keys.operator));
    let alpha = insert_tenant_keyed(&conn, alpha_name, false, copy_keypair(&keys.alpha));
    let bravo = insert_tenant_keyed(&conn, "bravo", false, copy_keypair(&keys.bravo));

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000, 'initialized')",
        rusqlite::params![label],
    )
    .unwrap();
    let volume_id = conn.last_insert_rowid();

    // ADR-0005's permanent escrow recipient — a recipient of every SLICE
    // and every envelope, so attestation (trial-decrypting a slice header
    // with the escrow key) has something true to find.
    let escrow = copy_keypair(&keys.escrow);

    let slices_dir = tempfile::tempdir().unwrap();
    // Where the fixture's dar catalogues live while the session builds the
    // envelopes (issue #473); `build` streams them out of here.
    let catalogues_dir = tempfile::tempdir().unwrap();
    let with_catalogues = WITH_CATALOGUES.with(|w| w.get());
    let mut build_units = Vec::new();
    let mut stage_set_ids = Vec::new();
    let mut expected_plain_hashes = Vec::new();
    let mut unit_ids = Vec::new();

    for (i, (unit_name, tenant_name, content)) in UNITS.iter().enumerate() {
        let tenant = if *tenant_name == "alpha" {
            &alpha
        } else {
            &bravo
        };
        let unit_uuid = if with_catalogues {
            format!("c{i:07x}-0000-4000-8000-{i:012x}")
        } else {
            format!("unit-uuid-{i:04}")
        };
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES (?1, ?2, ?3, ?4, 'active')",
            rusqlite::params![unit_uuid, unit_name, tenant.tenant_id, "/src/data"],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        unit_ids.push(unit_id);

        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 7, 'staged', ?2, 2, ?3)",
            rusqlite::params![unit_id, format!("/src/{unit_name}"), content.len() as i64],
        )
        .unwrap();
        let snapshot_id = conn.last_insert_rowid();

        // Two file rows per snapshot, so the `files` assertion has something
        // to count that the manifest alone could never supply.
        for n in 0..2 {
            db::files::fixture::insert(
                &conn,
                snapshot_id,
                &format!("{unit_name}/file{n}.bin"),
                100 + n,
                "regular",
                Some(&sha256_hex(format!("{unit_name}-{n}").as_bytes())),
            );
        }
        if i == 0 && WITH_LINK_AND_SPECIAL.with(|w| w.get()) {
            for entry in link_and_special(unit_name) {
                db::files::fixture::insert_entry(&conn, snapshot_id, entry);
            }
        }

        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 10485760)",
            rusqlite::params![snapshot_id],
        )
        .unwrap();
        let stage_set_id = conn.last_insert_rowid();
        stage_set_ids.push(stage_set_id);
        conn.execute(
            "UPDATE stage_sets SET key_fingerprints = ?1 WHERE id = ?2",
            rusqlite::params![
                serde_json::to_string(&[
                    tenant.public_key.as_str(),
                    operator.public_key.as_str(),
                    escrow.public_key.as_str(),
                ])
                .unwrap(),
                stage_set_id
            ],
        )
        .unwrap();

        // Two slices per unit, so "slice number is not tape position" is a
        // claim the fixture can actually falsify.
        let mut slices = Vec::new();
        for slice_number in 1..=slices_per_unit {
            let mut plaintext = content.to_vec();
            plaintext.extend_from_slice(format!("-slice{slice_number}").as_bytes());
            let recipients = vec![
                tenant.public_key.clone(),
                operator.public_key.clone(),
                escrow.public_key.clone(),
            ];
            let encrypted = staging::encrypt_data(&plaintext, &recipients).unwrap();
            let sha_plain = sha256_hex(&plaintext);
            let sha_enc = sha256_hex(&encrypted);

            conn.execute(
                "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                           sha256_plain, sha256_encrypted)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    stage_set_id,
                    slice_number,
                    plaintext.len() as i64,
                    encrypted.len() as i64,
                    sha_plain,
                    sha_enc
                ],
            )
            .unwrap();
            let slice_id = conn.last_insert_rowid();
            let slice_path = slices_dir.path().join(format!("slice_{slice_id}.age"));
            std::fs::write(&slice_path, &encrypted).unwrap();
            conn.execute(
                "UPDATE stage_slices SET staging_path = ?1 WHERE id = ?2",
                rusqlite::params![slice_path.to_string_lossy(), slice_id],
            )
            .unwrap();

            if slice_number == 1 {
                expected_plain_hashes.push((unit_name.to_string(), sha_plain.clone()));
            }
            slices.push(BuildSlice {
                slice_id,
                slice_number,
                size_bytes: plaintext.len() as i64,
                encrypted_bytes: encrypted.len() as i64,
                sha256_plain: sha_plain,
                sha256_encrypted: sha_enc,
                staging_path: slice_path,
            });
        }

        let catalog_path = with_catalogues.then(|| {
            // `stage create`'s layout: <catalogs>/<uuid8>/s<id>/<uuid8>_v<N>
            let stem = format!("{}_v7", &unit_uuid[..8]);
            let dir = catalogues_dir.path().join(format!("s{stage_set_id}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("{stem}.1.dar")),
                catalogue_bytes(unit_name),
            )
            .unwrap();
            dir.join(&stem).to_string_lossy().into_owned()
        });
        build_units.push(BuildUnit {
            stage_set_id,
            snapshot_id,
            unit_name: unit_name.to_string(),
            unit_uuid,
            tenant_id: tenant.tenant_id,
            dar_version: Some("2.7.20".to_string()),
            dar_command: None,
            catalog_path,
            snapshot_version: 7,
            slices,
        });
    }

    let catalog_dir = tempfile::tempdir().unwrap();
    let catalog_db_path = match catalog_db {
        CatalogDb::None => None,
        CatalogDb::New | CatalogDb::Gen2 | CatalogDb::Old => {
            let p = catalog_dir.path().join("catalog.db");
            db::catalog_snapshot::build_catalog_snapshot(&conn, &stage_set_ids, &p).unwrap();
            match catalog_db {
                CatalogDb::Old => db::ontape_catalog::fixture::downgrade(
                    &p,
                    db::ontape_catalog::Generation::Original,
                ),
                CatalogDb::Gen2 => db::ontape_catalog::fixture::downgrade(
                    &p,
                    db::ontape_catalog::Generation::WithOwnershipAndReceipts,
                ),
                CatalogDb::New | CatalogDb::None => {}
            }
            if let Some(sql) = CATALOG_DB_EDIT.with(|e| e.borrow_mut().take()) {
                rusqlite::Connection::open(&p)
                    .unwrap()
                    .execute_batch(&sql)
                    .unwrap();
            }
            Some(p)
        }
    };

    // The cross-volume index, generated where `volume write` generates it.
    let index_kind = INDEX_KIND.with(|k| k.get());
    let index_work = tempfile::tempdir().unwrap();
    let index = (index_kind != IndexKind::None).then(|| {
        // Two other sealed tapes the catalog knows about. Versions 8 and 9
        // are later than the 7 this tape carries.
        let history = |label: &str, condition: &str, unit: usize, version: i64| {
            conn.execute(
                "INSERT OR IGNORE INTO volumes (label, backend_type, backend_name, media_type,
                                                capacity_bytes, status, observed_condition, uuid,
                                                sealed_at)
                 VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000, 'sealed', ?2, ?3,
                         '2026-09-01 12:00:00')",
                rusqlite::params![label, condition, format!("hist-uuid-{label}")],
            )
            .unwrap();
            let hist_volume: i64 = conn
                .query_row("SELECT id FROM volumes WHERE label = ?1", [label], |r| r.get(0))
                .unwrap();
            conn.execute(
                "INSERT INTO snapshots (unit_id, version, status, source_path)
                 VALUES (?1, ?2, 'current', '/hist')",
                rusqlite::params![unit_ids[unit], version],
            )
            .unwrap();
            let snapshot = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'cleaned', 1)",
                [snapshot],
            )
            .unwrap();
            let set = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                           sha256_plain, sha256_encrypted)
                 VALUES (?1, 1, 7, 7, 'p', 'e')",
                [set],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, 'completed')",
                rusqlite::params![set, snapshot, hist_volume],
            )
            .unwrap();
        };
        history("HIST-A", "ok", 0, 8);
        history("HIST-A", "ok", 2, 8);
        history("HIST-Q", "quarantined", 0, 9);
        let files = tapectl::volume::index::generate_files(
            &conn,
            &tapectl::policy::coverage::IndexThisTape {
                label,
                uuid: volume_uuid,
                cartridge_serial: mam_serial,
                generation: "LTO-6",
            },
            &stage_set_ids,
            &index_work.path().join("index"),
        )
        .unwrap();
        if index_kind == IndexKind::Unreadable {
            std::fs::write(&files.operator_units, "unit\tversion\nphotos/2019\t1\n").unwrap();
        }
        files
    });

    let tenants = [alpha, bravo];
    let inputs = BuildInputs {
        label: label.to_string(),
        volume_uuid: volume_uuid.to_string(),
        media_type: "LTO-6".to_string(),
        tapectl_version: "0.1.0-test".to_string(),
        created_at: "2026-09-11T00:00:00Z".to_string(),
        block_size: BS,
        usable_bytes: 64 * 1024 * 1024,
        enospc_buffer: 1024 * 1024,
        nominal_capacity: 2_400_000_000,
        mam_capacity: 2_400_000_000,
        mam_manufacturer: "TAPECTL-TEST".to_string(),
        mam_serial: mam_serial.to_string(),
        cartridge_identity_source: cartridge_identity_source.map(str::to_string),
        mam_length: 0,
        mam_loads: 0,
        units: build_units.clone(),
        tenants: tenants
            .iter()
            .map(|t| TenantInfo {
                tenant_id: t.tenant_id,
                tenant_name: t.name.clone(),
                public_keys: vec![t.public_key.clone()],
            })
            .collect(),
        operator_public_keys: vec![operator.public_key.clone()],
        escrow_public_key: Some(escrow.public_key.clone()),
        catalog_db_path,
        index,
    };

    let session_dir = tempfile::tempdir().unwrap();
    let built = build::build(&inputs, session_dir.path()).expect("build succeeds");

    let keys = KeyAvailability {
        tenant_ids: tenants.iter().map(|t| t.tenant_id).collect(),
        tenants_with_active_key: tenants.iter().map(|t| t.tenant_id).collect(),
        operator_key_present: true,
        escrow_recipient_present: None,
        stage_sets_lacking_escrow: None,
    };

    let mut store = MemStore::new(BS as usize);
    let validated = built
        .into_validated(&keys, SliceCheck::Size, &mut store)
        .expect("validate");
    let planned = validated
        .plan(&conn, volume_id, &build_units)
        .expect("plan");
    let ready = match planned.execute(&conn, &mut store).expect("execute") {
        ExecuteOutcome::Ready(r) => r,
        ExecuteOutcome::Interrupted(_) => panic!("harness must reach Ready, got Interrupted"),
        ExecuteOutcome::Aborted(a) => panic!("harness must reach Ready, got Aborted: {}", a.reason),
    };
    let sealed = ready.seal(&mut store).expect("seal");
    match sealed
        .confirm(&conn, &mut store, Tier::Integrity)
        .expect("confirm")
    {
        ConfirmOutcome::Sealed(_) => {}
        ConfirmOutcome::Quarantined(q) => panic!("harness must seal, got {:?}", q.reason),
        ConfirmOutcome::Inconclusive(inc) => panic!(
            "harness must seal, got Inconclusive: {:?}",
            inc.evidence.mismatches
        ),
    }

    // The positions the REAL plan chose, read back from the source catalog —
    // the rebuild must arrive at these same numbers from the tape alone.
    let mut expected_positions = Vec::new();
    for (unit_name, _, _) in UNITS {
        let mut stmt = conn
            .prepare(
                "SELECT CAST(wp.position AS INTEGER)
                 FROM write_positions wp
                 JOIN writes w ON w.id = wp.write_id
                 JOIN stage_slices sl ON sl.id = wp.stage_slice_id
                 JOIN stage_sets ss ON ss.id = sl.stage_set_id
                 JOIN snapshots s ON s.id = ss.snapshot_id
                 JOIN units u ON u.id = s.unit_id
                 WHERE u.name = ?1 ORDER BY sl.slice_number",
            )
            .unwrap();
        let positions: Vec<i64> = stmt
            .query_map(rusqlite::params![unit_name], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        expected_positions.push((unit_name.to_string(), positions));
    }

    SealedVolume {
        label: label.to_string(),
        store,
        source_conn: conn,
        _source_dir: db_dir,
        operator_secret: operator.secret_key,
        escrow_secret: escrow.secret_key,
        escrow_public: escrow.public_key,
        tenant_secret: tenants[0].secret_key.clone(),
        expected_positions,
        expected_plain_hashes,
    }
}

/// Write a secret key to a file in the shape `crypto::keys::read_secret_key`
/// expects, and return its path.
fn key_file(dir: &Path, name: &str, secret: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("# tapectl test key\n{secret}\n")).unwrap();
    p
}

fn fresh_db(dir: &Path) -> rusqlite::Connection {
    db::open(&dir.join("rebuilt.db")).unwrap()
}

/// The resolution `volume::restore::restore_unit` itself performs — called,
/// not copied. The point: a rebuild is only useful if restore's own
/// selection finds the slices. This used to be a verbatim copy of the join;
/// issue #315 put a version selection in front of it
/// (`select_write_positions`), and a copy would have kept passing against a
/// query restore no longer runs.
fn restore_resolution_query(
    conn: &rusqlite::Connection,
    unit_name: &str,
) -> Vec<(i64, String, String)> {
    tapectl::volume::restore::select_write_positions(conn, unit_name, LABEL, None)
        .unwrap_or_else(|e| panic!("restore cannot resolve {unit_name} on {LABEL}: {e}"))
        .positions
        .into_iter()
        .map(|p| (p.slice_number, p.position, p.sha256_plain))
        .collect()
}

fn rebuild(
    conn: &rusqlite::Connection,
    vol: &mut SealedVolume,
    secret: &str,
    scratch: &Path,
) -> tapectl::error::Result<rebuild::RebuildReport> {
    // No medium serial: a `MemStore` has no MAM, and a drive that reports
    // none is an absence, which corroborates against nothing (ADR-0012,
    // issue #193) — the DR shape this suite is about.
    rebuild_observing(conn, vol, secret, scratch, None)
}

/// [`rebuild`], but standing in for a live drive that DID read a medium
/// serial this contact (issue #165's operator-identity resolution: a serial
/// observed now can supersede or corroborate a barcode File 0 recorded at
/// write time). `MemStore` still carries no MAM of its own — this is the
/// caller ASSERTING what a real drive would have reported, exactly as
/// `rebuild_from_volume`'s own `medium_serial` parameter does in production.
fn rebuild_observing(
    conn: &rusqlite::Connection,
    vol: &mut SealedVolume,
    secret: &str,
    scratch: &Path,
    observed_serial: Option<&str>,
) -> tapectl::error::Result<rebuild::RebuildReport> {
    let key_dir = tempfile::tempdir().unwrap();
    let key = key_file(key_dir.path(), "k.age.key", secret);
    let secret_str = tapectl::crypto::keys::read_secret_key(&key)?;
    let identity: age::x25519::Identity = secret_str.parse().unwrap();
    // The serial the corroboration check takes and the one the contact
    // records are ONE reading (issue #296), so the test supplies it as the
    // MAM reading it is rather than as a bare string.
    let mam = observed_serial.map(|serial| MamInfo {
        serial: Some(serial.to_string()),
        ..MamInfo::default()
    });
    let site = match &mam {
        Some(mam) => ContactSite::new(
            cfg(),
            Operation::CatalogRebuild,
            "memstore",
            Medium::Observed {
                backend: backend(),
                mam,
            },
        ),
        None => ContactSite::new(
            cfg(),
            Operation::CatalogRebuild,
            "memstore",
            Medium::NoBackend,
        ),
    };
    rebuild::rebuild_from_store(
        conn,
        &mut vol.store,
        &[identity],
        Some(&vol.label),
        "recovered",
        Some("lto0"),
        scratch,
        "memstore",
        site,
    )
}

/// Issue #335: a rebuild whose chip serial names no registered cartridge
/// when the contact opens, and which then registers that cartridge itself,
/// leaves its contact row naming the cartridge -- not "medium serial matches
/// no registered cartridge", which stopped being true when it committed.
#[test]
fn a_rebuild_contact_names_the_cartridge_the_rebuild_registered() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    assert_eq!(cartridge_count(&conn), 0, "premise: nothing registered yet");

    let report = rebuild_observing(
        &conn,
        &mut vol,
        &secret,
        scratch.path(),
        Some("REBUILDSERIAL"),
    )
    .unwrap();
    assert!(
        report.cartridge_registered,
        "premise: the rebuild registered it: {report:?}"
    );

    let cartridge_id: i64 = conn
        .query_row(
            "SELECT id FROM cartridges WHERE serial_number = 'REBUILDSERIAL'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let (contact_cartridge, reason): (Option<i64>, Option<String>) = conn
        .query_row(
            "SELECT cartridge_id, identity_reason FROM cartridge_contacts",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(contact_cartridge, Some(cartridge_id));
    assert_eq!(
        reason, None,
        "the stale 'matches no registered cartridge' is cleared"
    );
}

/// Issue #296: `catalog rebuild` puts a cartridge in a drive and must say so.
///
/// `operation` and `outcome` are asserted BY VALUE — a row exists either way,
/// and "which command, ending how" is the whole question `cartridge_contacts`
/// was added to answer.
///
/// `volume_id` is NULL by construction and that is the point: the rebuild's
/// premise is that the catalog has no row for this volume yet, so the row it
/// creates is the command's OUTPUT, never an input the contact can reference.
#[test]
fn a_rebuild_records_its_contact_as_catalog_rebuild_ok() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    rebuild(&conn, &mut vol, &secret, scratch.path()).expect("rebuild from a sealed volume");

    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM cartridge_contacts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1, "one command holding the drive is one contact");
    let (operation, outcome, volume_id): (String, Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT operation, outcome, volume_id FROM cartridge_contacts",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(operation, "catalog rebuild");
    assert_eq!(outcome.as_deref(), Some("ok"));
    assert_eq!(
        volume_id, None,
        "the volumes row a rebuild creates is its output, not the contact's subject"
    );
}

// ── issue #320: `catalog rebuild` takes ONE post-command health reading ──

/// A log-page source answering from the mhvtl fixture texts: page 0x00
/// lists the three health pages, each read is counted. Written here rather
/// than borrowed because the in-crate `FixtureSource` is `cfg(test)` —
/// which is also a check that the seam is usable from outside the crate.
#[derive(Default)]
struct CountingSource {
    reads: std::collections::BTreeMap<u8, usize>,
}

impl tapectl::tape::log_pages::LogSource for CountingSource {
    fn device_sg(&self) -> &str {
        "/dev/sg-fixture"
    }

    fn read_page(&mut self, page: u8) -> (Vec<String>, std::io::Result<std::process::Output>) {
        use std::os::unix::process::ExitStatusExt;
        *self.reads.entry(page).or_default() += 1;
        let stdout = if page == 0 {
            vec![0x00, 0x00, 0x00, 0x03, 0x02, 0x03, 0x2e]
        } else {
            vec![page, 0x00, 0x00, 0x00]
        };
        (
            vec!["sg_logs".into(), format!("--page=0x{page:02x}")],
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout,
                stderr: Vec::new(),
            }),
        )
    }

    fn decode(&mut self, raw: &[u8]) -> Result<String, String> {
        Ok(match raw[0] & 0x3f {
            0x02 => include_str!("fixtures/sg_logs/mhvtl_td8_sg1/page_0x02.decoded.txt"),
            0x03 => include_str!("fixtures/sg_logs/mhvtl_td8_sg1/page_0x03.decoded.txt"),
            0x2e => include_str!("fixtures/sg_logs/mhvtl_td8_sg1/page_0x2e.decoded.txt"),
            _ => "Supported log pages  [0x0]:\n",
        }
        .to_string())
    }

    fn tool_version(&mut self) -> Option<String> {
        None
    }
}

/// A rebuild through a configured drive takes exactly one sweep and one
/// `restore` reading, both naming its contact — and, like the contact, no
/// volume: the row the rebuild inserts is its output.
#[test]
fn a_rebuild_contact_takes_exactly_one_health_reading() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let key_dir = tempfile::tempdir().unwrap();
    let key = key_file(key_dir.path(), "k.age.key", &secret);
    let identity: age::x25519::Identity = tapectl::crypto::keys::read_secret_key(&key)
        .unwrap()
        .parse()
        .unwrap();

    let mut config = tapectl::config::Config::default();
    let mut bk = backend().clone();
    bk.device_tape = "memstore".to_string();
    config.backends.lto.push(bk);
    let drive = tapectl::tape::drive_identity::DriveIdentity {
        serial: Some("XYZZY_A1".to_string()),
        ..Default::default()
    };
    let src = std::cell::RefCell::new(CountingSource::default());
    rebuild::rebuild_from_store(
        &conn,
        &mut vol.store,
        &[identity],
        Some(LABEL),
        "recovered",
        Some("lto0"),
        scratch.path(),
        "memstore",
        ContactSite::new(
            &config,
            Operation::CatalogRebuild,
            "memstore",
            Medium::NoBackend,
        )
        .with_drive_identity(&drive)
        .with_log_source(&src),
    )
    .expect("rebuild from a sealed volume");

    let reads = src.borrow().reads.clone();
    assert_eq!(
        reads.into_iter().collect::<Vec<_>>(),
        vec![(0x00, 1), (0x02, 1), (0x03, 1), (0x2e, 1)],
        "one sweep: page 0x00 and every page it lists, each once"
    );
    let cid: i64 = conn
        .query_row("SELECT id FROM cartridge_contacts", [], |r| r.get(0))
        .unwrap();
    /// `(contact_id, operation, volume_id, total_uncorrected)`.
    type HealthRow = (Option<i64>, String, Option<i64>, Option<i64>);
    let health: Vec<HealthRow> = conn
        .prepare("SELECT contact_id, operation, volume_id, total_uncorrected FROM health_logs")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        health,
        vec![(Some(cid), "restore".to_string(), None, Some(0))],
        "exactly one reading, of kind 'restore', naming the rebuild's contact"
    );
    let journal: Vec<(Option<i64>, String)> = conn
        .prepare("SELECT contact_id, trigger FROM log_page_journal")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        journal,
        vec![(Some(cid), "catalog rebuild".to_string()); 4],
        "one sweep's journal rows, each naming the contact and the command"
    );
    let drive_serial: Option<String> = conn
        .query_row(
            "SELECT d.serial FROM cartridge_contacts c LEFT JOIN drives d ON d.id = c.drive_id",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(drive_serial.as_deref(), Some("XYZZY_A1"));
}

/// The DR machine this command exists for: keys and no `backend add`. The
/// rebuild SUCCEEDS, and takes no reading — no health row, no journal row —
/// because no configured drive claims the device.
#[test]
fn a_dr_rebuild_with_no_backend_succeeds_and_takes_no_reading() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).expect("the DR rebuild");
    assert!(
        report.volume_inserted,
        "positive control: the rebuild did its work"
    );
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM cartridge_contacts"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM health_logs"), 0);
    assert_eq!(count("SELECT COUNT(*) FROM log_page_journal"), 0);
}

/// The failure half: a rebuild refused because the catalog's cartridge
/// binding disagrees with the drive STILL records the contact, with a
/// non-OK outcome. The contact happened — the tape was read — even though
/// the command then refused.
#[test]
fn a_refused_rebuild_still_records_its_contact_with_a_failed_outcome() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    // A volume row already claiming a DIFFERENT medium serial than the site
    // below reports, so `corroborate_volume` refuses.
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
        rusqlite::params![LABEL],
    )
    .unwrap();
    let volume_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number, status)
         VALUES ('BC-SHELF2', 'LTO-6', 2500000000000, 'SER-SHELF2', 'in_use')",
        [],
    )
    .unwrap();
    let cartridge_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO cartridge_volumes (cartridge_id, volume_id, identity_source)
         VALUES (?1, ?2, 'mam')",
        rusqlite::params![cartridge_id, volume_id],
    )
    .unwrap();

    let key_dir = tempfile::tempdir().unwrap();
    let key = key_file(key_dir.path(), "k.age.key", &secret);
    let secret_str = tapectl::crypto::keys::read_secret_key(&key).unwrap();
    let identity: age::x25519::Identity = secret_str.parse().unwrap();
    let drive_mam = MamInfo {
        serial: Some("SER-IN-DRIVE".to_string()),
        ..MamInfo::default()
    };
    rebuild::rebuild_from_store(
        &conn,
        &mut vol.store,
        &[identity],
        Some(LABEL),
        "recovered",
        Some("lto0"),
        scratch.path(),
        "memstore",
        ContactSite::new(
            cfg(),
            Operation::CatalogRebuild,
            "memstore",
            Medium::Observed {
                backend: backend(),
                mam: &drive_mam,
            },
        ),
    )
    .expect_err("the catalog's binding and the drive disagree");

    let (operation, outcome, cartridge): (String, Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT operation, outcome, cartridge_id FROM cartridge_contacts",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(operation, "catalog rebuild");
    assert_eq!(outcome.as_deref(), Some("failed"));
    assert_eq!(
        cartridge, None,
        "the serial the drive reported matches no registered cartridge, which is an \
         honest absence, not the shelf's belief about which tape this is"
    );
}

#[test]
fn a_rebuilt_catalog_satisfies_the_query_restore_actually_uses() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let report =
        rebuild(&conn, &mut vol, &secret, scratch.path()).expect("rebuild from a sealed volume");

    assert_eq!(report.label, LABEL);
    assert!(report.volume_inserted);
    assert!(report.had_catalog_db);
    assert_eq!(report.units, UNITS.len());
    assert!(
        report.units_without_tenant_envelope.is_empty(),
        "every unit is in a tenant envelope on this tape: {:?}",
        report.units_without_tenant_envelope
    );

    for (unit_name, expected) in &vol.expected_positions {
        let rows = restore_resolution_query(&conn, unit_name);
        assert_eq!(
            rows.len(),
            expected.len(),
            "unit {unit_name}: restore's own query found {} slices, expected {}",
            rows.len(),
            expected.len()
        );
        let got: Vec<i64> = rows
            .iter()
            .map(|(_, pos, _)| pos.parse().unwrap())
            .collect();
        assert_eq!(
            &got, expected,
            "unit {unit_name}: rebuilt tape positions disagree with what the write plan chose"
        );
    }
}

/// Issue #158 (revisited for #242): a label that already exists in the
/// catalog with a non-sealed status (an imported `active` row) must not be
/// silently reused as if it were `sealed`, and must not be silently
/// rewritten to `sealed` either. The rebuild still has to attach every row
/// it can — the mismatch is reported, not treated as a reason to stop
/// short.
///
/// This test now covers the `observed_condition` half specifically (ADR-0012's
/// 2026-09-17 amendment, issue #242): a `quarantined` condition a failed
/// `volume verify` produced deliberately, before the database that recorded
/// WHY was lost. Since that amendment a verify-quarantined row's `status`
/// stays `sealed` — so this scenario no longer trips
/// `volume_status_mismatch` at all; `volume_condition_mismatch` is the field
/// that must catch it. See
/// `rebuild_onto_an_active_row_reports_the_status_mismatch_and_leaves_it_alone`
/// just below for the `status` half this test used to cover as a side
/// effect of using `quarantined` as its vehicle.
#[test]
fn rebuild_onto_a_quarantined_row_reports_the_mismatch_and_leaves_the_status_alone() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    // The destination catalog already knows this label, and quarantined its
    // CONDITION — a fact an operator established on purpose (e.g. a prior
    // failed `volume verify`), before the database that recorded WHY was
    // lost. `status` stays `sealed`, per the 2026-09-17 amendment.
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, \
         status, observed_condition)
         VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000, 'sealed', 'quarantined')",
        rusqlite::params![LABEL],
    )
    .unwrap();

    let report =
        rebuild(&conn, &mut vol, &secret, scratch.path()).expect("rebuild onto an existing row");

    assert!(
        !report.volume_inserted,
        "the row already existed — rebuild must not insert a second one"
    );
    assert_eq!(
        report.volume_status_mismatch, None,
        "status genuinely did not mismatch here -- it stayed 'sealed' (issue #242)"
    );
    assert_eq!(
        report.volume_condition_mismatch,
        Some("quarantined".to_string()),
        "the pre-existing non-ok condition must be surfaced on the report"
    );

    let (status, condition): (String, String) = conn
        .query_row(
            "SELECT status, observed_condition FROM volumes WHERE label = ?1",
            rusqlite::params![LABEL],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "sealed");
    assert_eq!(
        condition, "quarantined",
        "the ratified minimum is report-only: rebuild must never overwrite an \
         operator-established condition on the strength of its own evidence"
    );

    // The mismatch is reported, but the rebuild still did its job: every
    // unit's slices resolve through the exact join `restore_unit` uses.
    for (unit_name, expected) in &vol.expected_positions {
        let rows = restore_resolution_query(&conn, unit_name);
        assert_eq!(
            rows.len(),
            expected.len(),
            "unit {unit_name}: a status mismatch must not stop the rebuild short"
        );
    }
}

/// The `status` half of issue #158, isolated from `observed_condition`
/// entirely: an imported `active` row is exactly the OTHER pre-existing
/// non-sealed shape the doc comment above names, and it must still be
/// surfaced via `volume_status_mismatch` and left alone, the same as before
/// issue #242 split the two facts apart.
#[test]
fn rebuild_onto_an_active_row_reports_the_status_mismatch_and_leaves_it_alone() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    // The destination catalog already knows this label as an IMPORTED
    // volume (`tapectl import`), never sealed by a v2 write session here.
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000, 'active')",
        rusqlite::params![LABEL],
    )
    .unwrap();

    let report =
        rebuild(&conn, &mut vol, &secret, scratch.path()).expect("rebuild onto an existing row");

    assert!(
        !report.volume_inserted,
        "the row already existed — rebuild must not insert a second one"
    );
    assert_eq!(
        report.volume_status_mismatch,
        Some("active".to_string()),
        "the pre-existing non-sealed status must be surfaced on the report"
    );
    assert_eq!(
        report.volume_condition_mismatch, None,
        "the condition genuinely did not mismatch here -- it stayed 'ok'"
    );

    let (status, condition): (String, String) = conn
        .query_row(
            "SELECT status, observed_condition FROM volumes WHERE label = ?1",
            rusqlite::params![LABEL],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        status, "active",
        "the ratified minimum is report-only: rebuild must never overwrite an \
         operator-established status on the strength of its own evidence"
    );
    assert_eq!(condition, "ok");

    for (unit_name, expected) in &vol.expected_positions {
        let rows = restore_resolution_query(&conn, unit_name);
        assert_eq!(
            rows.len(),
            expected.len(),
            "unit {unit_name}: a status mismatch must not stop the rebuild short"
        );
    }
}

/// The trap the #72 retrieval guide names: slice number is not tape position,
/// and slices do not start at 0. A rebuild that stored the slice number in
/// `write_positions.position` would restore the wrong files, and every other
/// assertion here would still pass.
#[test]
fn write_positions_record_the_tape_position_not_the_slice_number() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    let rows = restore_resolution_query(&conn, UNITS[0].0);
    assert_eq!(rows.len(), 2);
    for (slice_number, position, _) in &rows {
        let pos: i64 = position.parse().unwrap();
        assert!(
            pos > *slice_number,
            "slice {slice_number} landed at position {pos}: data slices sit after the \
             envelopes, so a position equal to the slice number means the two were confused"
        );
    }
}

/// `sha256_plain` is on NEITHER the front index (sacred invariant) nor the
/// on-tape `catalog.db` — only the envelope manifest has it. If a rebuild
/// ever sourced slices from the front index, this is what would go missing.
#[test]
fn slice_plaintext_hashes_survive_the_rebuild() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    for (unit_name, expected) in &vol.expected_plain_hashes {
        let rows = restore_resolution_query(&conn, unit_name);
        let (_, _, sha_plain) = &rows[0];
        assert_eq!(
            sha_plain, expected,
            "unit {unit_name}: slice 1's sha256_plain did not survive the rebuild"
        );
    }
}

/// The operator manifest files every unit under the placeholder tenant
/// `"operator"`. Real ownership is only in the tenant envelopes, so a rebuild
/// that read the operator envelope alone would file the whole tape under one
/// invented tenant.
#[test]
fn units_are_filed_under_their_real_tenants_not_the_operator_placeholder() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    for (unit_name, tenant_name, _) in UNITS {
        let got: String = conn
            .query_row(
                "SELECT t.name FROM units u JOIN tenants t ON t.id = u.tenant_id
                 WHERE u.name = ?1",
                rusqlite::params![unit_name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(&got, tenant_name, "unit {unit_name} was filed under {got}");
    }
    let operator_tenant: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tenants WHERE name = 'operator'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        operator_tenant, 0,
        "\"operator\" is the manifest's placeholder for the all-units envelope, \
         not a tenant a rebuild should create"
    );
}

/// #383 review: a rebuilt catalog files units under tenants it holds no key
/// rows for (`ensure_tenant`), while each stage set comes back with its
/// recipient list. "The catalog holds none of the tenant's keys" is not "the
/// tenant cannot open the copy": `tenancy` must not judge it, or `audit`
/// tells the operator after a disaster to re-stage every copy in the
/// archive, when the fix is `key import`. Positive control: a key row that
/// is NOT in the list is judged (the path is exercised), and importing the
/// tenant's real public key clears it.
#[test]
fn a_rebuilt_catalog_with_no_tenant_keys_reports_no_tenancy_disagreement() {
    use tapectl::policy::tenancy::{
        disagreements, tenant_directory, written_stage_sets, Disagreement,
    };
    let mut vol = build_sealed_volume_with(CatalogDb::New);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    let key_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM encryption_keys k JOIN tenants t ON t.id = k.tenant_id
             WHERE t.name IN ('alpha', 'bravo')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(key_rows, 0, "the shape this test is about: keyless tenants");
    let stage_set_disagreements = |conn: &rusqlite::Connection, unit_name: &str| -> usize {
        let unit = db::queries::get_unit_by_name(conn, unit_name)
            .unwrap()
            .unwrap();
        let tenants = tenant_directory(conn).unwrap();
        let (name, keys) = &tenants[&unit.tenant_id];
        let written = written_stage_sets(conn, None).unwrap();
        let mine = written.get(&unit.id).map(Vec::as_slice).unwrap_or_default();
        disagreements(&unit, name, keys, mine)
            .into_iter()
            .filter(|d| matches!(d, Disagreement::StageSet { .. }))
            .count()
    };
    for (unit_name, _, _) in UNITS {
        assert_eq!(
            stage_set_disagreements(&conn, unit_name),
            0,
            "{unit_name}: a keyless tenant is not judged"
        );
    }

    // Positive control: alpha now holds a key, and not the one its copies
    // were encrypted to.
    let alpha: i64 = conn
        .query_row("SELECT id FROM tenants WHERE name = 'alpha'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let stranger = generate_keypair();
    conn.execute(
        "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key)
         VALUES (?1, 'alpha-other', ?2, ?3)",
        rusqlite::params![alpha, stranger.fingerprint, stranger.public_key],
    )
    .unwrap();
    assert_eq!(stage_set_disagreements(&conn, "photos/2019"), 1);

    // `key import` of alpha's real public key: the copy is alpha's again.
    let alpha_public: String = vol
        .source_conn
        .query_row(
            "SELECT k.public_key FROM encryption_keys k JOIN tenants t ON t.id = k.tenant_id
             WHERE t.name = 'alpha'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key)
         VALUES (?1, 'alpha-imported', 'fp-alpha-imported', ?2)",
        rusqlite::params![alpha, alpha_public],
    )
    .unwrap();
    assert_eq!(stage_set_disagreements(&conn, "photos/2019"), 0);
}

/// The per-file index exists only in the operator envelope's `catalog.db`.
#[test]
fn the_file_index_comes_back_from_the_operator_catalog_db() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    assert_eq!(report.files, UNITS.len() * 2);
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM file_versions fv JOIN snapshots s ON s.id = fv.snapshot_id
             JOIN units u ON u.id = s.unit_id WHERE u.name = ?1",
            rusqlite::params![UNITS[0].0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);

    // `source_path` is NOT NULL and lives only in catalog.db — a rebuild
    // without it must say so rather than invent one.
    let source: String = conn
        .query_row(
            "SELECT s.source_path FROM snapshots s JOIN units u ON u.id = s.unit_id
             WHERE u.name = ?1",
            rusqlite::params![UNITS[0].0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(source, format!("/src/{}", UNITS[0].0));
}

/// ADR-0012 2026-10-12 item 48 (#381's on-tape half): a catalog rebuilt
/// from a 1.1.0 tape (generation 3 `catalog.db`) keeps each entry's kind
/// and link target, so a symlink and a FIFO come back as what the walk
/// recorded, not as regular files with data. The same version on a
/// generation-2 tape comes back as regular files: that shape records only
/// `is_directory`, the documented limit for tapes written before 1.1.0.
#[test]
fn a_rebuilt_catalog_keeps_each_kind_and_link_target() {
    use db::files::FileKind;
    let unit = UNITS[0].0;
    for generation in [CatalogDb::New, CatalogDb::Gen2] {
        WITH_LINK_AND_SPECIAL.with(|w| w.set(true));
        let mut vol = build_sealed_volume_with(generation);
        WITH_LINK_AND_SPECIAL.with(|w| w.set(false));
        let dir = tempfile::tempdir().unwrap();
        let conn = fresh_db(dir.path());
        let scratch = tempfile::tempdir().unwrap();
        let secret = vol.operator_secret.clone();
        let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();
        assert_eq!(report.files, UNITS.len() * 2 + 2, "every row came back");

        let rows: Vec<(String, FileKind, Option<String>, bool)> = conn
            .prepare(
                "SELECT p.path, fv.kind, fv.link_target, fv.sha256 IS NOT NULL
                 FROM file_versions fv JOIN paths p ON p.id = fv.path_id
                 WHERE p.path LIKE ?1 || '/%' ORDER BY p.path",
            )
            .unwrap()
            .query_map(rusqlite::params![unit], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let (link_kind, link_target, pipe_kind) = if generation == CatalogDb::New {
            (
                FileKind::Symlink,
                Some("file0.bin".to_string()),
                FileKind::Special,
            )
        } else {
            (FileKind::Regular, None, FileKind::Regular)
        };
        assert_eq!(
            rows,
            vec![
                (format!("{unit}/file0.bin"), FileKind::Regular, None, true),
                (format!("{unit}/file1.bin"), FileKind::Regular, None, true),
                (format!("{unit}/latest"), link_kind, link_target, false),
                (format!("{unit}/pipe"), pipe_kind, None, false),
            ],
            "catalog.db generation {}",
            generation as u8
        );
    }
}

/// Issue #381: a rebuilt version re-staged from source content-validates its
/// regular files. Rebuild used to leave `files.file_type` NULL, and staging
/// validates only `file_type = 'regular'`, so it planned zero files: the
/// size check and the sha256 baseline from the tape were both skipped and
/// the stage went ahead on whatever the source held. Generations 1 and 2 of
/// `catalog.db` carry `is_directory` only and generation 3 the kind itself,
/// so all three are covered.
#[test]
fn a_rebuilt_version_restaged_from_source_validates_its_regular_files() {
    for generation in [CatalogDb::Old, CatalogDb::Gen2, CatalogDb::New] {
        let mut vol = build_sealed_volume_with(generation);
        let dir = tempfile::tempdir().unwrap();
        let conn = fresh_db(dir.path());
        let scratch = tempfile::tempdir().unwrap();
        let secret = vol.operator_secret.clone();
        rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

        let unit = UNITS[0].0;
        let snapshot_id: i64 = conn
            .query_row(
                "SELECT s.id FROM snapshots s JOIN units u ON u.id = s.unit_id
                 WHERE u.name = ?1",
                rusqlite::params![unit],
                |r| r.get(0),
            )
            .unwrap();

        // The source, at the recorded paths and sizes, holding other bytes
        // than the tape's baseline hashes describe.
        let src = tempfile::tempdir().unwrap();
        for n in 0..2u8 {
            let p = src.path().join(format!("{unit}/file{n}.bin"));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, vec![b'x'; 100 + n as usize]).unwrap();
        }

        let err = staging::validate::validate_source(
            &conn,
            snapshot_id,
            src.path().to_str().unwrap(),
            &[],
        )
        .err()
        .unwrap_or_else(|| {
            panic!(
                "a rebuilt version must be content-validated against the tape's \
                 sha256 baseline, so changed bytes are refused (catalog.db \
                 generation {})",
                generation as u8
            )
        })
        .to_string();
        assert!(
            err.contains("BITROT") && err.contains(&format!("{unit}/file")),
            "the refusal names the file whose bytes changed: {err}"
        );
    }
}

/// A tape written before migration 030 can carry a `modified_at` the walk
/// spelled for a file dated past 2262 (or before 1677), which no nanosecond
/// count holds. The rebuild records no mtime for it, as a fresh walk of that
/// file does today, and keeps every other row's; it used to refuse the whole
/// rebuild over that one value. Text the walk never writes is still refused,
/// naming the row (and is this test's positive control that the edit reached
/// the tape).
#[test]
fn a_far_off_modified_at_on_tape_rebuilds_with_no_mtime() {
    let far = "2300-01-01T00:00:00+00:00";
    let near = "2026-09-01T12:00:00+00:00";
    let mut vol = build_sealed_volume_editing_catalog_db(
        CatalogDb::Old,
        &format!(
            "UPDATE files SET modified_at = '{near}';
             UPDATE files SET modified_at = '{far}' WHERE id = (SELECT MIN(id) FROM files);"
        ),
    );
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect("one far-off timestamp must not fail a DR rebuild");

    let mtimes: Vec<(String, Option<i64>)> = conn
        .prepare(
            "SELECT p.path, fv.mtime_ns FROM file_versions fv JOIN paths p ON p.id = fv.path_id
             ORDER BY p.path",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(mtimes.len(), UNITS.len() * 2, "every row came back");
    let near_ns = db::files::mtime_ns_from_secs(1_788_264_000);
    let unset = mtimes.iter().filter(|(_, ns)| ns.is_none()).count();
    assert_eq!(unset, 1, "only the far-off row has no mtime: {mtimes:?}");
    assert!(
        mtimes.iter().all(|(_, ns)| ns.is_none() || *ns == near_ns),
        "every other row keeps its mtime: {mtimes:?}"
    );

    let mut vol = build_sealed_volume_editing_catalog_db(
        CatalogDb::Old,
        "UPDATE files SET modified_at = 'yesterday' WHERE id = (SELECT MIN(id) FROM files);",
    );
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let secret = vol.operator_secret.clone();
    let err = rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect_err("text the walk never writes is refused")
        .to_string();
    assert!(
        err.contains("catalog.db files row") && err.contains("\"yesterday\""),
        "{err}"
    );
}

/// A tape written before #83 carries no `catalog.db`. The restore path must
/// still come back whole — what degrades is the searchable index.
#[test]
fn a_tape_without_a_catalog_db_still_rebuilds_the_restore_path() {
    let mut vol = build_sealed_volume(false);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    assert!(!report.had_catalog_db);
    assert_eq!(report.files, 0);
    assert_eq!(report.units, UNITS.len());

    for (unit_name, expected) in &vol.expected_positions {
        let rows = restore_resolution_query(&conn, unit_name);
        let got: Vec<i64> = rows
            .iter()
            .map(|(_, pos, _)| pos.parse().unwrap())
            .collect();
        assert_eq!(&got, expected, "unit {unit_name}");
    }

    let source: String = conn
        .query_row(
            "SELECT s.source_path FROM snapshots s JOIN units u ON u.id = s.unit_id
             WHERE u.name = ?1",
            rusqlite::params![UNITS[0].0],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        source.contains("unknown"),
        "an unknown source path must say so, not be invented: got {source:?}"
    );
}

/// The front index of a `MemStore` tape, parsed from its recorded bytes.
fn front_index_of(vol: &SealedVolume) -> Vec<tapectl::volume::format::ParsedIndexEntry> {
    let raw = &vol.store.files[3];
    let text = String::from_utf8_lossy(raw);
    tapectl::volume::format::parse_front_index(text.trim_end_matches('\0'))
        .expect("front index parses")
}

/// Flip one byte in the middle of tape file `pos`'s ciphertext: age still
/// matches the key from the header, and the payload no longer authenticates.
fn damage_file(vol: &mut SealedVolume, pos: i32, size: u64) {
    vol.store.files[pos as usize][size as usize / 2] ^= 0xff;
}

/// `volume-format-v2.md` §1.3 (#418 item 2): a damaged tenant envelope falls
/// back to its backup copy. On a tape without `catalog.db` the tenant
/// envelopes are where ownership comes from, so a rebuild that gave up on the
/// damaged primary (as it did before the backup existed) could not file the
/// tenant's units under it.
#[test]
fn a_damaged_tenant_envelope_is_read_from_its_backup() {
    let mut vol = build_sealed_volume(false);
    let fi = front_index_of(&vol);
    let primaries: Vec<_> = fi
        .iter()
        .filter(|e| e.type_label == "tenant_envelope")
        .cloned()
        .collect();
    assert_eq!(primaries.len(), 2, "alpha and bravo");
    for p in &primaries {
        let backup = fi
            .iter()
            .find(|e| {
                e.type_label == "tenant_envelope_backup" && e.sha256_encrypted == p.sha256_encrypted
            })
            .expect("every tenant envelope has a backup on tape");
        assert!(backup.position > p.position);
        damage_file(&mut vol, p.position, p.size_bytes.unwrap());
    }

    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let report = rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect("both tenant envelopes are damaged, and both backups are whole");
    assert!(
        report.units_without_tenant_envelope.is_empty(),
        "{:?}",
        report.units_without_tenant_envelope
    );
    for (unit_name, tenant_name, _) in UNITS {
        let got: String = conn
            .query_row(
                "SELECT t.name FROM units u JOIN tenants t ON t.id = u.tenant_id
                 WHERE u.name = ?1",
                rusqlite::params![unit_name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(&got, tenant_name, "unit {unit_name}");
    }
}

/// Positive control for the test above: with BOTH copies of a tenant
/// envelope damaged there is nothing to fall back to, and the rebuild says
/// so rather than carrying on without that tenant's envelope.
#[test]
fn a_tenant_envelope_damaged_in_both_copies_fails_the_rebuild() {
    let mut vol = build_sealed_volume(false);
    let fi = front_index_of(&vol);
    let primary = fi
        .iter()
        .find(|e| e.type_label == "tenant_envelope")
        .unwrap()
        .clone();
    let backup = fi
        .iter()
        .find(|e| {
            e.type_label == "tenant_envelope_backup"
                && e.sha256_encrypted == primary.sha256_encrypted
        })
        .unwrap()
        .clone();
    damage_file(&mut vol, primary.position, primary.size_bytes.unwrap());
    damage_file(&mut vol, backup.position, backup.size_bytes.unwrap());

    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let err = rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect_err("no whole copy of the envelope is left");
    let msg = err.to_string();
    assert!(
        msg.contains(&format!(
            "the tenant_envelope at position {} could not be opened, and neither could its backup copy",
            primary.position
        )),
        "the error names the damaged envelope and its backup: {msg}"
    );
}

/// Damage every `-> X25519` recipient stanza in tape file `pos`'s age
/// header: the first base64 character of each stanza body (the wrapped file
/// key) is swapped for another valid one. The header still parses, but no
/// key unwraps — and age 0.11 reports a stanza that fails to unwrap exactly
/// as it reports a stanza for someone else, `NoMatchingKeys`. So the only
/// thing that tells this damage from "not your envelope" is the front
/// index's hash of the bytes.
fn damage_stanzas(vol: &mut SealedVolume, pos: i32) {
    let file = &mut vol.store.files[pos as usize];
    let header_end = file
        .windows(5)
        .position(|w| w == b"\n--- ")
        .expect("an age header ends in its MAC line");
    let mut flipped = 0;
    let mut at = 0;
    while let Some(off) = file[at..header_end]
        .windows(10)
        .position(|w| w == b"-> X25519 ")
    {
        let stanza = at + off;
        let body = stanza
            + file[stanza..header_end]
                .iter()
                .position(|b| *b == b'\n')
                .expect("a stanza line ends in a newline")
            + 1;
        file[body] = if file[body] == b'A' { b'B' } else { b'A' };
        flipped += 1;
        at = body;
    }
    assert!(
        flipped > 0,
        "tape file {pos} has no X25519 stanza to damage"
    );
}

/// The envelope of `kind` and its backup (same front-index hash).
fn envelope_pair(
    vol: &SealedVolume,
    kind: &str,
) -> (
    tapectl::volume::format::ParsedIndexEntry,
    tapectl::volume::format::ParsedIndexEntry,
) {
    let fi = front_index_of(vol);
    let primary = fi
        .iter()
        .find(|e| e.type_label == kind)
        .unwrap_or_else(|| panic!("no {kind} on tape"))
        .clone();
    let backup = fi
        .iter()
        .find(|e| {
            e.type_label == format!("{kind}_backup")
                && e.sha256_encrypted == primary.sha256_encrypted
        })
        .unwrap_or_else(|| panic!("no backup of the {kind} on tape"))
        .clone();
    (primary, backup)
}

fn assert_tenant_ownership(conn: &rusqlite::Connection) {
    for (unit_name, tenant_name, _) in UNITS {
        let got: String = conn
            .query_row(
                "SELECT t.name FROM units u JOIN tenants t ON t.id = u.tenant_id
                 WHERE u.name = ?1",
                rusqlite::params![unit_name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(&got, tenant_name, "unit {unit_name}");
    }
}

/// `volume-format-v2.md` §1.3: a copy that "does not match the front index's
/// hash" falls back to its backup. Damage to the operator envelope's
/// recipient stanzas reads, to age, like a key that is not a recipient — and
/// a rebuild that believed age would refuse the operator key as a tenant key
/// instead of reading `operator_envelope_backup`.
#[test]
fn an_operator_envelope_damaged_in_its_stanzas_is_read_from_its_backup() {
    let mut vol = build_sealed_volume(true);
    let (primary, _) = envelope_pair(&vol, "operator_envelope");
    damage_stanzas(&mut vol, primary.position);

    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect("the primary's stanzas are damaged, and its backup is whole");
    assert_tenant_ownership(&conn);
    assert_restorable_from(&conn, &mut vol);
}

/// The same stanza damage on every tenant envelope's primary, on a tape
/// without `catalog.db` (so the tenant envelopes are where ownership comes
/// from): each is read from its backup, not quietly counted as an envelope
/// the operator key does not open.
#[test]
fn a_tenant_envelope_damaged_in_its_stanzas_is_read_from_its_backup() {
    let mut vol = build_sealed_volume(false);
    let primaries: Vec<_> = front_index_of(&vol)
        .into_iter()
        .filter(|e| e.type_label == "tenant_envelope")
        .collect();
    assert_eq!(primaries.len(), 2, "alpha and bravo");
    for p in &primaries {
        damage_stanzas(&mut vol, p.position);
    }

    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let report = rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect("both primaries' stanzas are damaged, and both backups are whole");
    assert!(
        report.units_without_tenant_envelope.is_empty(),
        "{:?}",
        report.units_without_tenant_envelope
    );
    assert_tenant_ownership(&conn);
}

/// Positive control for the two above: a primary that does not read whole
/// and a backup whose stanzas are damaged leave no good copy. The backup's
/// "no matching key" must not clear the primary's failure, nor read as a
/// tenant key — the rebuild fails, naming both copies.
#[test]
fn a_damaged_primary_and_a_backup_damaged_in_its_stanzas_fail_the_rebuild() {
    let mut vol = build_sealed_volume(true);
    let (primary, backup) = envelope_pair(&vol, "operator_envelope");
    damage_file(&mut vol, primary.position, primary.size_bytes.unwrap());
    damage_stanzas(&mut vol, backup.position);

    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let err = rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect_err("no whole copy of the operator envelope is left");
    let msg = err.to_string();
    assert!(
        msg.contains(&format!(
            "the operator_envelope at position {} could not be opened, \
             and neither could its backup copy at position {}",
            primary.position, backup.position
        )),
        "the error names both copies: {msg}"
    );
    assert!(
        msg.contains("does not match the front index"),
        "the backup's damage is named as damage: {msg}"
    );
    assert!(
        !msg.contains("neither an operator key nor the escrow key"),
        "damage is not a tenant key: {msg}"
    );
}

/// Idempotence is the property that makes a rebuild safe to run across a
/// shelf of cartridges, twice, in any order.
#[test]
fn rebuilding_the_same_volume_twice_changes_nothing_the_second_time() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let first = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();
    assert!(!first.is_noop());

    let counts_after_first = row_counts(&conn);
    let second = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    assert!(
        second.is_noop(),
        "a second rebuild inserted rows: {second:?}"
    );
    assert_eq!(
        counts_after_first,
        row_counts(&conn),
        "a second rebuild changed the catalog"
    );
    assert!(
        second.volume_status_mismatch.is_none(),
        "the first rebuild's row is already sealed — a second run must not \
         report a mismatch against itself: {second:?}"
    );
    assert!(
        second.volume_sealed_at_mismatch.is_none(),
        "the first rebuild recorded the tape's own seal time — a second run must \
         not report it as disagreeing with itself: {second:?}"
    );
}

// ---- issue #305: a rebuilt tape is dated by its own bytes -----------------

/// The seal marker's `sealed_at` exactly as the tape carries it, in the
/// catalog's form — read from the last file the write session left in the
/// store, not from anything the rebuild computed.
fn tape_sealed_at(vol: &SealedVolume) -> String {
    let seal = String::from_utf8_lossy(vol.store.files.last().expect("a sealed tape"));
    let parsed = tapectl::volume::format::parse_seal_marker(&seal).expect("the seal parses");
    tapectl::volume::format::catalog_timestamp(&parsed.sealed_at).expect("RFC 3339")
}

type VolumeDates = (String, Option<String>, Option<String>, Option<String>, i64);

fn volume_dates(conn: &rusqlite::Connection, label: &str) -> VolumeDates {
    conn.query_row(
        "SELECT created_at, first_write, last_write, sealed_at, num_data_files
         FROM volumes WHERE label = ?1",
        rusqlite::params![label],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )
    .unwrap()
}

/// Issue #305: a volume row a rebuild INSERTS takes its dates from the tape —
/// File 0's `created_at` (when the write was planned) for `created_at` and
/// `first_write`, the seal marker's `sealed_at` for `sealed_at` and
/// `last_write` — and its data-file count from the front index. Before, it
/// read as created on the day of the rebuild, never written, never sealed.
#[test]
fn a_rebuilt_volume_is_dated_by_its_own_file_0_and_seal_marker() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let sealed = tape_sealed_at(&vol);

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();
    assert!(report.volume_inserted);

    let (created, first, last, sealed_at, data_files) = volume_dates(&conn, LABEL);
    // `build_tape` writes File 0 with created_at = "2026-09-11T00:00:00Z".
    assert_eq!(created, "2026-09-11 00:00:00");
    assert_eq!(first.as_deref(), Some("2026-09-11 00:00:00"));
    assert_eq!(sealed_at.as_deref(), Some(sealed.as_str()));
    assert_eq!(last.as_deref(), Some(sealed.as_str()));
    let slices: usize = vol.expected_positions.iter().map(|(_, p)| p.len()).sum();
    assert_eq!(data_files, slices as i64, "one per data slice on the tape");

    // The report carries what the tape said, for the operator to read.
    assert_eq!(report.written_at.as_deref(), Some("2026-09-11 00:00:00"));
    assert_eq!(report.sealed_at.as_deref(), Some(sealed.as_str()));
    // The fixture writes `mam_loads: 0`, the writer's "unknown".
    assert_eq!(report.load_count_at_write, None);
}

/// The control: a tape whose File 0 carries no `created_at` and which has
/// no seal marker still rebuilds — the dates are simply absent (`created_at`
/// falls back to the row's own default, the rebuild time), never invented.
/// Without this, the test above could not tell "read the tape" from "wrote
/// a plausible constant".
#[test]
fn a_tape_without_the_dates_rebuilds_with_them_absent() {
    let mut vol = build_sealed_volume(true);
    // File 0 without its created_at line (same length, so nothing else in
    // the file moves), and no seal marker: an unsealed tape.
    let thunk = String::from_utf8(vol.store.files[0].clone()).unwrap();
    let line = thunk
        .lines()
        .find(|l| l.starts_with("created_at = "))
        .expect("File 0 carries created_at")
        .to_string();
    vol.store.files[0] = thunk
        .replacen(&line, &" ".repeat(line.len()), 1)
        .into_bytes();
    vol.store.files.pop();
    vol.store.syncs.pop();

    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let report = rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect("a missing date is an absence, never a reason to refuse");

    let (created, first, last, sealed_at, _) = volume_dates(&conn, LABEL);
    assert_ne!(created, "2026-09-11 00:00:00");
    assert_eq!(first, None);
    assert_eq!(last, None);
    assert_eq!(sealed_at, None);
    assert_eq!(report.written_at, None);
    assert_eq!(report.sealed_at, None);
}

/// A row the rebuild merely FINDS is never edited (#158): when its recorded
/// seal time disagrees with the tape's, the report says so and the row
/// keeps its own.
#[test]
fn rebuild_onto_a_row_with_another_seal_time_reports_it_and_leaves_it_alone() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes,
                              status, sealed_at)
         VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000, 'sealed', '2020-01-01 00:00:00')",
        rusqlite::params![LABEL],
    )
    .unwrap();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();
    assert!(!report.volume_inserted);
    assert_eq!(
        report.volume_sealed_at_mismatch.as_deref(),
        Some("2020-01-01 00:00:00")
    );
    assert_eq!(
        report.sealed_at.as_deref(),
        Some(tape_sealed_at(&vol).as_str())
    );
    let (_, _, _, sealed_at, _) = volume_dates(&conn, LABEL);
    assert_eq!(sealed_at.as_deref(), Some("2020-01-01 00:00:00"));
}

fn row_counts(conn: &rusqlite::Connection) -> Vec<(String, i64)> {
    [
        "tenants",
        "units",
        "snapshots",
        "stage_sets",
        "stage_slices",
        "volumes",
        "writes",
        "write_positions",
        "paths",
        "file_versions",
        // Issue #165: without these two, the idempotence test below proves
        // nothing about the cartridge writes a rebuild now makes.
        "cartridges",
        "cartridge_volumes",
    ]
    .iter()
    .map(|t| {
        let n: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
            .unwrap();
        (t.to_string(), n)
    })
    .collect()
}

// ── Issue #165: catalog rebuild binds the cartridge it observed ──────────

fn cartridge_row(
    conn: &rusqlite::Connection,
    barcode: &str,
) -> (String, Option<String>, String, String, i64) {
    conn.query_row(
        "SELECT barcode, serial_number, status, media_type, nominal_capacity
         FROM cartridges WHERE barcode = ?1",
        rusqlite::params![barcode],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )
    .unwrap_or_else(|e| panic!("no cartridge row \"{barcode}\": {e}"))
}

fn open_mount_cartridge(conn: &rusqlite::Connection, volume_label: &str) -> Option<String> {
    conn.query_row(
        "SELECT c.barcode FROM cartridge_volumes cv
         JOIN cartridges c ON c.id = cv.cartridge_id
         JOIN volumes v ON v.id = cv.volume_id
         WHERE v.label = ?1 AND cv.unmounted_at IS NULL",
        rusqlite::params![volume_label],
        |r| r.get(0),
    )
    .optional()
    .unwrap()
}

fn cartridge_count(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM cartridges", [], |r| r.get(0))
        .unwrap()
}

/// The defect itself, fixed: before issue #165, `insert_all` wrote a
/// `volumes` row and its unit chain and NOTHING else — no `cartridges` row,
/// no `cartridge_volumes` mount, so a recovered tape could never be
/// re-bound. This is the `mam`-identity path (ADR-0012): File 0's own chip
/// serial names a cartridge nothing in the catalog knows yet.
#[test]
fn a_rebuild_registers_the_cartridge_file_0_names_and_binds_the_volume_to_it() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    let (barcode, serial, status, media_type, capacity) = cartridge_row(&conn, "REBUILDSERIAL");
    assert_eq!(barcode, "REBUILDSERIAL");
    assert_eq!(serial.as_deref(), Some("REBUILDSERIAL"));
    assert_eq!(status, "in_use");
    assert_eq!(media_type, "LTO-6");
    // Issue #210: the generation table's native LTO-6 figure, never the
    // volume's resolved capacity (2_400_000_000, mhvtl's fiction) that File
    // 0's `nominal_capacity_bytes` carries here.
    assert_eq!(
        capacity,
        tapectl::media::Generation::Lto6.native_capacity_bytes() as i64
    );
    assert_eq!(cartridge_count(&conn), 1);
    assert_eq!(
        open_mount_cartridge(&conn, LABEL).as_deref(),
        Some("REBUILDSERIAL"),
        "the rebuilt volume must carry an open mount onto the registered cartridge"
    );

    assert!(report.cartridge_registered, "{report:?}");
    assert!(report.cartridge_bound, "{report:?}");
    assert_eq!(report.cartridge_barcode.as_deref(), Some("REBUILDSERIAL"));
    assert!(report.unbound_reason.is_none());
}

/// The other half of the same path: a serial File 0 names that the catalog
/// ALREADY has, registered by hand (an operator who registered the
/// cartridge, with its real serial, before ever loading it for a rebuild).
/// No second row, and the existing barcode is kept — rebuild binds to what
/// it finds rather than re-registering under the serial.
#[test]
fn a_rebuild_binds_to_a_cartridge_already_registered_by_serial() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    conn.execute(
        "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number, status)
         VALUES ('L6-0001', 'LTO-6', 2400000000, 'REBUILDSERIAL', 'available')",
        [],
    )
    .unwrap();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    assert_eq!(cartridge_count(&conn), 1, "no second row registered");
    let (barcode, serial, status, _, _) = cartridge_row(&conn, "L6-0001");
    assert_eq!(barcode, "L6-0001", "the existing barcode is kept");
    assert_eq!(serial.as_deref(), Some("REBUILDSERIAL"));
    assert_eq!(status, "in_use");
    assert_eq!(
        open_mount_cartridge(&conn, LABEL).as_deref(),
        Some("L6-0001")
    );

    assert!(!report.cartridge_registered, "no new row this run");
    assert!(report.cartridge_bound);
    assert_eq!(report.cartridge_barcode.as_deref(), Some("L6-0001"));
}

/// Displacement is recorded (never refused) exactly when the serial PROVES
/// the medium — a `mam` identity match is always that proof: the row was
/// found BY the serial File 0 itself carries. The same ADR-0010 record used
/// at `volume init`, now reachable from `catalog rebuild` too.
#[test]
fn a_rebuild_records_the_displacement_the_serial_proves() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    conn.execute(
        "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number, status)
         VALUES ('L6-0001', 'LTO-6', 2400000000, 'REBUILDSERIAL', 'in_use')",
        [],
    )
    .unwrap();
    let cartridge_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes,
                              status)
         VALUES ('L6-STALE', 'lto', 'lto0', 'LTO-6', 2400000000, 'sealed')",
        [],
    )
    .unwrap();
    let stale_vol = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO cartridge_volumes (cartridge_id, volume_id, identity_source)
         VALUES (?1, ?2, 'mam')",
        rusqlite::params![cartridge_id, stale_vol],
    )
    .unwrap();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    assert_eq!(
        report
            .displaced
            .iter()
            .map(|d| d.label.as_str())
            .collect::<Vec<_>>(),
        vec!["L6-STALE"],
        "{report:?}"
    );
    let stale_status: String = conn
        .query_row(
            "SELECT status FROM volumes WHERE id = ?1",
            rusqlite::params![stale_vol],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stale_status, "erased");
    let displaced_event: String = conn
        .query_row(
            "SELECT details FROM events WHERE entity_type = 'cartridge' AND action = 'displaced'",
            [],
            |r| r.get(0),
        )
        .expect("a displaced event");
    assert!(
        displaced_event.contains("catalog rebuild"),
        "{displaced_event}"
    );
    assert!(displaced_event.contains("L6-STALE"), "{displaced_event}");
    assert_eq!(
        open_mount_cartridge(&conn, LABEL).as_deref(),
        Some("L6-0001")
    );
}

/// The un-witnessed-displacement refusal (issue #155's rule, rebuild's own
/// version of it, item 3): an `operator`-identity barcode with no serial to
/// prove it, bound in the catalog to a DIFFERENT live volume, must be
/// refused before any write rather than silently displacing it.
#[test]
fn a_rebuild_refuses_a_barcode_bound_to_another_live_volume_without_serial_proof() {
    let mut vol = build_sealed_volume_full(CatalogDb::New, "OPBARCODE", Some("operator"));
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    conn.execute(
        "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status)
         VALUES ('OPBARCODE', 'LTO-6', 2400000000, 'in_use')",
        [],
    )
    .unwrap();
    let cartridge_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes,
                              status)
         VALUES ('L6-LIVE', 'lto', 'lto0', 'LTO-6', 2400000000, 'sealed')",
        [],
    )
    .unwrap();
    let live_vol = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO cartridge_volumes (cartridge_id, volume_id, identity_source)
         VALUES (?1, ?2, 'operator')",
        rusqlite::params![cartridge_id, live_vol],
    )
    .unwrap();

    let before = row_counts(&conn);
    let err = rebuild(&conn, &mut vol, &secret, scratch.path())
        .unwrap_err()
        .to_string();

    assert!(err.contains("OPBARCODE"), "{err}");
    assert!(err.contains("L6-LIVE"), "{err}");
    assert!(err.contains("no --force"), "{err}");
    assert_eq!(before, row_counts(&conn), "a refusal must write nothing");
}

/// A rebuild that ALREADY has an open mount recorded to a DIFFERENT
/// cartridge than the one this tape's own identity now names — item 3: "a
/// row it finds" is never edited. Reachable only by hand-seeding a
/// contradiction no ordinary sequence of rebuilds could produce; the
/// defence exists anyway.
#[test]
fn a_rebuild_refuses_a_volume_the_catalog_mounts_elsewhere() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes,
                              status)
         VALUES (?1, 'lto', 'lto0', 'LTO-6', 2400000000, 'sealed')",
        rusqlite::params![LABEL],
    )
    .unwrap();
    let volume_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status)
         VALUES ('WRONG-CART', 'LTO-6', 2400000000, 'in_use')",
        [],
    )
    .unwrap();
    let wrong_cart = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO cartridge_volumes (cartridge_id, volume_id, identity_source)
         VALUES (?1, ?2, 'operator')",
        rusqlite::params![wrong_cart, volume_id],
    )
    .unwrap();

    let before = row_counts(&conn);
    let err = rebuild(&conn, &mut vol, &secret, scratch.path())
        .unwrap_err()
        .to_string();

    assert!(err.contains("WRONG-CART"), "{err}");
    assert!(err.contains("never edits a row it finds"), "{err}");
    assert_eq!(before, row_counts(&conn), "a refusal must write nothing");
}

/// The explicit trap (item 3): a legacy File 0 recording NO cartridge
/// identity at all (`cartridge_serial = ""`) must not fail the rebuild. The
/// volume rebuilds unbound, and the report says why.
#[test]
fn a_legacy_file_0_with_no_serial_rebuilds_unbound_and_says_so() {
    let mut vol = build_sealed_volume_full(CatalogDb::New, "", None);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    assert!(report.volume_inserted, "{report:?}");
    assert!(!report.cartridge_registered, "{report:?}");
    assert!(!report.cartridge_bound, "{report:?}");
    assert!(report.cartridge_barcode.is_none(), "{report:?}");
    assert!(
        report.unbound_reason.is_some(),
        "the report must say why it could not bind: {report:?}"
    );
    assert_eq!(cartridge_count(&conn), 0);
    assert_eq!(open_mount_cartridge(&conn, LABEL), None);
}

/// The SAME legacy outcome, but for the OTHER shape `classify_media`
/// treats as unknown: a non-empty `cartridge_serial` with no
/// `cartridge_identity_source` recorded at all — the ADR-0010-to-#192
/// window. Distinct code path from the empty-serial case above; both must
/// land here, and the report should say which.
#[test]
fn a_serial_with_no_identity_source_also_rebuilds_unbound_and_says_so() {
    let mut vol = build_sealed_volume_full(CatalogDb::New, "PRE192SERIAL", None);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    assert!(!report.cartridge_bound, "{report:?}");
    assert_eq!(cartridge_count(&conn), 0);
    let reason = report
        .unbound_reason
        .as_deref()
        .expect("the report must say why");
    assert!(
        reason.contains("PRE192SERIAL"),
        "the reason should name the serial nobody can vouch for: {reason}"
    );
}

/// The recipe `volume_init`'s `AlreadySealed` refusal prints — "retire its
/// current volume, erase the tape IN THE DRIVE …, then run `tapectl
/// cartridge mark-erased`" — must reach a rebuilt tape rather than dead-end on a
/// missing row. Before issue #165 there was no cartridge row to name at all.
///
/// Issue #289 changed what "runnable" means here, and the change is the
/// point. `cartridge mark-erased` now runs the ADR-0008 Tier-3 floor, so
/// calling it DIRECTLY on a rebuilt tape — skipping the `volume retire` the
/// recipe names first — is refused while that tape is some unit's last
/// eligible copy. That is the recipe working, not failing: a catalog
/// rebuilt from one tape necessarily believes that tape is the only copy,
/// and erasing it is exactly the data loss Tier 3 is absolute about. The
/// other half of the same printed recipe, `volume retire`, has refused this
/// case since issue #147; until #289 `mark-erased` was the ungated door,
/// and this test passed only because of it.
///
/// So both halves are asserted. The refusal must be the FLOOR talking and
/// not a missing row (that is #165's actual claim, and `cartridge
/// "{barcode}" not found` would also contain the barcode — so the barcode
/// alone proves nothing). And once the versions are given up on purpose,
/// the recipe must complete.
#[test]
fn the_already_sealed_recipe_reaches_a_rebuilt_tape_and_completes_once_coverage_is_released() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    // Half 1: the rebuilt row IS reachable, and the Tier-3 floor is what
    // stops the erase -- with `force = true`, which reaches nothing here.
    let err = tapectl::cli::operations::cartridge_mark_erased(
        &conn,
        "REBUILDSERIAL",
        true,
        false,
        false,
        false,
    )
    .expect_err(
        "erasing a rebuilt tape that is a unit's last eligible copy must be refused (issue #289)",
    )
    .to_string();
    assert!(
        err.contains("LAST eligible copy"),
        "the refusal must come from the ADR-0008 Tier-3 floor: {err}"
    );
    assert!(
        !err.contains("not found"),
        "the rebuilt cartridge must be NAMEABLE -- a `not found` here would be issue #165 \
         regressing, not the floor firing: {err}"
    );
    assert!(
        err.contains(LABEL),
        "the floor's recovery commands name the VOLUME whose slices must be copied off: {err}"
    );

    // Give the versions up on purpose -- the act the refusal itself names,
    // `tapectl snapshot mark-reclaimable <unit> --version N`, applied here
    // directly because that command's own preconditions (tape-only 2x
    // multiplier, staging state) are a different subject under test.
    // `versions_at_stake` only considers `status = 'current'` snapshots, so
    // this is precisely what the operator's release does to the floor.
    conn.execute("UPDATE snapshots SET status = 'reclaimable'", [])
        .unwrap();

    // Half 2: with nothing live at stake, the recipe completes.
    tapectl::cli::operations::cartridge_mark_erased(
        &conn,
        "REBUILDSERIAL",
        true,
        false,
        false,
        false,
    )
    .expect("once no live version is at stake the rebuilt cartridge must be erasable");
    let status: String = conn
        .query_row(
            "SELECT status FROM cartridges WHERE barcode = 'REBUILDSERIAL'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "available");
    let vol_status: String = conn
        .query_row(
            "SELECT status FROM volumes WHERE label = ?1",
            rusqlite::params![LABEL],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(vol_status, "erased");
}

/// The `operator`-identity path (ADR-0012's other half): File 0 carries a
/// barcode, not a chip serial. A live drive reading a serial THIS contact
/// separately corroborates it — the row learns that serial (`NULL` →
/// value, once).
#[test]
fn a_rebuild_registers_under_the_operator_barcode_and_learns_the_serial() {
    let mut vol = build_sealed_volume_full(CatalogDb::New, "OPBARCODE", Some("operator"));
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let report =
        rebuild_observing(&conn, &mut vol, &secret, scratch.path(), Some("SER-LIVE")).unwrap();

    let (barcode, serial, status, _, _) = cartridge_row(&conn, "OPBARCODE");
    assert_eq!(barcode, "OPBARCODE");
    assert_eq!(serial.as_deref(), Some("SER-LIVE"));
    assert_eq!(status, "in_use");
    assert!(report.cartridge_registered, "{report:?}");
    assert!(report.cartridge_bound, "{report:?}");
    assert_eq!(report.cartridge_barcode.as_deref(), Some("OPBARCODE"));
}

/// Issue #210: the cartridge row a rebuild auto-registers must record the
/// GENERATION TABLE's native figure for the medium (`media::Generation`),
/// never File 0's `nominal_capacity_bytes` — which is whatever
/// `resolve_capacity` decided at write time, drive `capacity_override`
/// first (ADR-0010 decision 3, issue #183). This is the `operator`-identity
/// auto-register arm (`resolve_operator_identity`'s `None` match on
/// `select_cartridge(tx, "barcode", ...)`) — the practically reachable one,
/// since the documented override case is mhvtl, which exposes no medium
/// serial and so takes `--cartridge` at init, landing here rather than in
/// `resolve_mam_identity`.
///
/// The fixture's `BuildInputs.nominal_capacity` (2_400_000_000 — mhvtl's
/// 2400 MB fiction; `binding.rs`'s own issue #183 tests use the identical
/// stand-in) is nowhere near LTO-6's real native capacity, so the two
/// figures are trivially distinguishable.
#[test]
fn a_rebuild_registers_the_cartridge_at_the_generation_table_capacity_not_the_resolved_one() {
    let mut vol = build_sealed_volume_full(CatalogDb::New, "OPBARCODE", Some("operator"));
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    let (_, _, _, media_type, capacity) = cartridge_row(&conn, "OPBARCODE");
    assert_eq!(media_type, "LTO-6");
    assert_eq!(
        capacity,
        tapectl::media::Generation::Lto6.native_capacity_bytes() as i64,
        "the cartridge row must record the generation table's native capacity for LTO-6, \
         never the volume's resolved figure"
    );
    assert_ne!(
        capacity, 2_400_000_000,
        "the resolved (possibly override-inflated) figure File 0 carries must not leak onto \
         the cartridge row"
    );
}

/// Issue #221: a SECOND contact is where a serial is often learnt, not the
/// first. The first rebuild here registers "OPBARCODE" with no serial at
/// all (no live drive observed one); the second contact is where a drive
/// first reports "SER-LIVE" — `rebuild_from_store`'s pre-transaction
/// `binding::corroborate_volume` call (rebuild.rs) records it onto the row
/// (and its `events` row) via `record_medium_serial` BEFORE
/// `resolve_operator_identity` ever runs, so `resolve_operator_identity`'s
/// own learn branch never fires and every write-counter would read zero.
/// The report must say so anyway: it must not read `no_changes: true`
/// about a run that changed `cartridges.serial_number` and said so on its
/// own stderr.
#[test]
fn a_rebuild_that_learns_a_serial_at_contact_reports_it_changed_the_catalog() {
    let mut vol = build_sealed_volume_full(CatalogDb::New, "OPBARCODE", Some("operator"));
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let first = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();
    assert!(!first.is_noop(), "{first:?}");
    let (_, serial_before, _, _, _) = cartridge_row(&conn, "OPBARCODE");
    assert_eq!(
        serial_before, None,
        "the first rebuild observed no serial and must not have recorded one"
    );

    let second =
        rebuild_observing(&conn, &mut vol, &secret, scratch.path(), Some("SER-LIVE")).unwrap();

    let (_, serial_after, _, _, _) = cartridge_row(&conn, "OPBARCODE");
    assert_eq!(
        serial_after.as_deref(),
        Some("SER-LIVE"),
        "the contact must have recorded the serial this drive reported"
    );
    assert!(
        second.serial_learned,
        "the report must say this run learnt a serial: {second:?}"
    );
    assert!(
        !second.is_noop(),
        "a run that wrote cartridges.serial_number and an events row must not report \
         no_changes: true: {second:?}"
    );
}

/// The corroboration half stated as a refusal: an `operator`-identity
/// barcode whose row ALREADY carries a DIFFERENT serial than what this
/// contact observed means another cartridge is wearing that sticker.
#[test]
fn a_rebuild_refuses_an_operator_barcode_whose_row_carries_another_serial() {
    let mut vol = build_sealed_volume_full(CatalogDb::New, "OPBARCODE", Some("operator"));
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    conn.execute(
        "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number, status)
         VALUES ('OPBARCODE', 'LTO-6', 2400000000, 'SER-RECORDED', 'available')",
        [],
    )
    .unwrap();

    let before = row_counts(&conn);
    let err = rebuild_observing(&conn, &mut vol, &secret, scratch.path(), Some("SER-LIVE"))
        .unwrap_err()
        .to_string();

    assert!(err.contains("OPBARCODE"), "{err}");
    assert!(err.contains("SER-RECORDED"), "{err}");
    assert!(err.contains("SER-LIVE"), "{err}");
    assert_eq!(before, row_counts(&conn), "a refusal must write nothing");
}

/// ADR-0012: "the loaded tape *is* that other cartridge" — a live serial
/// this contact observed can match a DIFFERENT row than the barcode File 0
/// recorded. The medium's own serial outranks a sticker; the superseded
/// barcode is reported, not silently dropped.
#[test]
fn a_live_serial_supersedes_the_operator_barcode_when_it_matches_another_row() {
    let mut vol = build_sealed_volume_full(CatalogDb::New, "OPBARCODE", Some("operator"));
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    conn.execute(
        "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number, status)
         VALUES ('REAL-CART', 'LTO-6', 2400000000, 'SER-LIVE', 'available')",
        [],
    )
    .unwrap();

    let report =
        rebuild_observing(&conn, &mut vol, &secret, scratch.path(), Some("SER-LIVE")).unwrap();

    assert_eq!(cartridge_count(&conn), 1, "no second row registered");
    assert_eq!(
        open_mount_cartridge(&conn, LABEL).as_deref(),
        Some("REAL-CART")
    );
    assert_eq!(
        report.cartridge_barcode_superseded.as_deref(),
        Some("OPBARCODE")
    );
    assert_eq!(report.cartridge_barcode.as_deref(), Some("REAL-CART"));
}

/// Item 3's `retired_permanent` carve-out: the mount is recorded as a
/// physical fact, but the cartridge's status is left exactly as found — no
/// amount of contact makes a medium declared permanently unfit fit again
/// (ADR-0011), and rebuild must never call anything resembling
/// `refuse_retired`.
#[test]
fn a_rebuild_mounts_onto_a_retired_permanent_cartridge_without_reviving_it() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    conn.execute(
        "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number, status)
         VALUES ('REBUILDSERIAL', 'LTO-6', 2400000000, 'REBUILDSERIAL', 'retired_permanent')",
        [],
    )
    .unwrap();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect("a retired_permanent cartridge must not refuse the rebuild");

    let (_, _, status, _, _) = cartridge_row(&conn, "REBUILDSERIAL");
    assert_eq!(
        status, "retired_permanent",
        "rebuild must never revive a retired_permanent cartridge's status"
    );
    assert_eq!(
        open_mount_cartridge(&conn, LABEL).as_deref(),
        Some("REBUILDSERIAL"),
        "the mount is a physical fact and is still recorded"
    );
    assert!(report.cartridge_retired, "{report:?}");
    assert!(report.cartridge_bound, "{report:?}");
}

/// The access-control boundary, and the reason it needs no key introspection:
/// the operator envelope's recipients are operator + escrow only, so a tenant
/// key simply cannot open it.
#[test]
fn a_tenant_key_is_refused_and_pointed_at_the_heir_path() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.tenant_secret.clone();

    let err = rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect_err("a tenant key must not rebuild the operator's catalog");
    let msg = err.to_string();
    assert!(
        msg.contains("neither an operator key nor the escrow key"),
        "the refusal must name the reason: {msg}"
    );
    assert!(
        msg.contains("RESTORE.sh"),
        "a refused tenant must be pointed at the path that does serve them: {msg}"
    );

    assert_eq!(
        row_counts(&conn).iter().map(|(_, n)| *n).sum::<i64>(),
        0,
        "a refused rebuild must leave the catalog untouched"
    );
}

/// A rebuild is a claim about where rows came from, and ADR-0001 has the
/// catalog as a ledger of claims — so provenance is an `events` row, not a
/// column on every table.
#[test]
fn the_rebuild_records_its_provenance_as_an_event() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    let details: String = conn
        .query_row(
            "SELECT details FROM events WHERE action = 'catalog_rebuild'",
            [],
            |r| r.get(0),
        )
        .expect("a catalog_rebuild event");
    assert!(details.contains(LABEL), "{details}");
    assert!(details.contains(VOL_UUID), "{details}");
    assert!(details.contains("catalog.db present"), "{details}");
}

/// The defect this test exists for: `audit` scopes every per-unit check to
/// `status = 'active'` (`cli::audit`'s `list_units(conn, None, Some("active"))`).
/// Rebuilt units were first written as `tape_only`, which made a catalog
/// rebuilt after a disaster report ZERO violations where the catalog it
/// replaced reported three `copy_count` violations for the same units on the
/// same single tape. Under-reporting risk to someone who has just lost their
/// database is the worst direction for this to fail in.
///
/// Pinned through the exact call `audit` makes, not through a status string,
/// so it keeps testing the real scoping if that scoping ever moves.
#[test]
fn rebuilt_units_are_visible_to_the_scope_audit_checks() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    let audited = tapectl::db::queries::list_units(&conn, None, Some("active")).unwrap();
    let mut names: Vec<&str> = audited.iter().map(|u| u.name.as_str()).collect();
    names.sort_unstable();
    let mut expected: Vec<&str> = UNITS.iter().map(|(n, _, _)| *n).collect();
    expected.sort_unstable();
    assert_eq!(
        names, expected,
        "every rebuilt unit must fall inside the scope audit checks, or a \
         rebuilt catalog silently reports fewer violations than the truth"
    );

    // `tape_only` is a policy state `unit mark-tape-only` sets deliberately
    // after checking enforced preconditions. A rebuild has read a tape, not
    // looked at anyone's disk, and must not claim it.
    let tape_only: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM units WHERE status = 'tape_only'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tape_only, 0);
}

/// The `volumes` row must name a backend the config actually declares — an
/// invented name would put a backend in the catalog that nothing can resolve.
#[test]
fn the_rebuilt_volume_names_the_configured_backend() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    let (btype, bname): (String, String) = conn
        .query_row(
            "SELECT backend_type, backend_name FROM volumes WHERE label = ?1",
            rusqlite::params![LABEL],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(btype, "lto");
    assert_eq!(bname, "lto0", "the caller's configured backend name");
}

/// The other half of the ratified access rule. `with_escrow` puts the escrow
/// recipient on every envelope (ADR-0005), so the escrow key must rebuild
/// exactly as the operator key does — this is the key an heir actually holds,
/// printed on the kit `key escrow-kit` generates.
#[test]
fn the_escrow_key_rebuilds_as_well_as_the_operator_key() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.escrow_secret.clone();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect("the escrow key must open the operator envelope");
    assert_eq!(report.units, UNITS.len());
    assert!(report.had_catalog_db);

    for (unit_name, expected) in &vol.expected_positions {
        let rows = restore_resolution_query(&conn, unit_name);
        let got: Vec<i64> = rows
            .iter()
            .map(|(_, pos, _)| pos.parse().unwrap())
            .collect();
        assert_eq!(&got, expected, "unit {unit_name}");
    }
}

/// #137: rebuilt stage sets carry `origin = 'rebuilt'`, which is what lets
/// the escrow predicate say "unknown — attest it" instead of "no recorded
/// recipient list" — the same verdict, a different explanation.
#[test]
fn rebuilt_stage_sets_are_marked_as_such() {
    // Old shape on purpose: a NEW-shape tape carries the receipt, and then the
    // predicate answers "yes", not "?" — see `receipts_ride_the_tape...`.
    let mut vol = build_sealed_volume_with(CatalogDb::Old);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    let (rebuilt, total): (i64, i64) = conn
        .query_row(
            "SELECT SUM(origin = 'rebuilt'), COUNT(*) FROM stage_sets",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(total, UNITS.len() as i64);
    assert_eq!(rebuilt, total, "every rebuilt stage set must say so");

    // And through the predicate: unknown, not a gap.
    let fp: Option<String> = conn
        .query_row("SELECT key_fingerprints FROM stage_sets LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        tapectl::policy::escrow::marker(
            fp.as_deref(),
            tapectl::policy::escrow::Origin::Rebuilt,
            Some("age1anything"),
        ),
        "?"
    );
}

/// Review finding 2, option (a): the escrow receipt rides the tape in
/// `catalog.db`, so a rebuild of a new-shape tape records coverage rather
/// than reporting it unknown. This is #137 closed for every tape written
/// after 2026-09-11 — and it is proven through the same predicate `audit`
/// and `catalog locate` use.
#[test]
fn receipts_ride_the_tape_so_a_rebuilt_new_tape_is_escrow_covered() {
    let mut vol = build_sealed_volume_with(CatalogDb::New);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    assert!(report.tenants_from_catalog_db);
    assert_eq!(report.receipts_from_tape, UNITS.len());

    let mut stmt = conn
        .prepare("SELECT key_fingerprints, origin FROM stage_sets")
        .unwrap();
    let rows: Vec<(Option<String>, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(rows.len(), UNITS.len());
    for (fp, origin) in &rows {
        assert_eq!(origin, "rebuilt");
        assert_eq!(
            tapectl::policy::escrow::marker(
                fp.as_deref(),
                tapectl::policy::escrow::Origin::Rebuilt,
                Some(&vol.escrow_public),
            ),
            "yes",
            "a rebuilt stage set with its receipt on the tape is covered, not unknown"
        );
    }
}

/// A tape written before 2026-09-11 carries the old `catalog.db` shape.
/// Ownership must still come back — from the tenant envelopes — and the
/// receipt honestly cannot: those rows are `unknown`, attestable later.
#[test]
fn an_old_shape_catalog_db_still_rebuilds_through_the_envelopes() {
    let mut vol = build_sealed_volume_with(CatalogDb::Old);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    assert!(report.had_catalog_db);
    assert!(!report.tenants_from_catalog_db);
    assert_eq!(report.receipts_from_tape, 0);
    assert_eq!(
        report.files,
        UNITS.len() * 2,
        "the file index still comes from catalog.db"
    );

    for (unit_name, tenant_name, _) in UNITS {
        let got: String = conn
            .query_row(
                "SELECT t.name FROM units u JOIN tenants t ON t.id = u.tenant_id WHERE u.name = ?1",
                rusqlite::params![unit_name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(&got, tenant_name, "ownership from the tenant envelopes");
    }
    for (unit_name, expected) in &vol.expected_positions {
        let rows = restore_resolution_query(&conn, unit_name);
        let got: Vec<i64> = rows
            .iter()
            .map(|(_, pos, _)| pos.parse().unwrap())
            .collect();
        assert_eq!(&got, expected, "unit {unit_name}");
    }
    let fp: Option<String> = conn
        .query_row("SELECT key_fingerprints FROM stage_sets LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        tapectl::policy::escrow::marker(
            fp.as_deref(),
            tapectl::policy::escrow::Origin::Rebuilt,
            Some(&vol.escrow_public),
        ),
        "?"
    );
}

/// Register `public_key` as this catalog's escrow recipient — what
/// `key import --escrow <original>` does, and step one of the DR procedure.
fn register_escrow(conn: &rusqlite::Connection, public_key: &str) {
    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('operator', 1, 'active')",
        [],
    )
    .unwrap();
    let tid = conn.last_insert_rowid();
    tapectl::db::queries::insert_escrow_key(conn, tid, "escrow", public_key, public_key, None)
        .unwrap();
}

fn markers(conn: &rusqlite::Connection, escrow_pub: &str) -> Vec<&'static str> {
    let mut stmt = conn
        .prepare("SELECT key_fingerprints, origin FROM stage_sets ORDER BY id")
        .unwrap();
    stmt.query_map([], |r| {
        Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?))
    })
    .unwrap()
    .map(|r| {
        let (fp, origin) = r.unwrap();
        tapectl::policy::escrow::marker(
            fp.as_deref(),
            tapectl::policy::escrow::Origin::parse(&origin),
            Some(escrow_pub),
        )
    })
    .collect()
}

/// #137, grilling Q2: with the REGISTERED escrow key in hand, a rebuild of
/// an old-shape tape proves coverage by decrypting a slice header, and
/// records it. Proof, not a receipt.
#[test]
fn the_registered_escrow_key_attests_coverage_on_an_old_tape() {
    let mut vol = build_sealed_volume_with(CatalogDb::Old);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    register_escrow(&conn, &vol.escrow_public);
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.escrow_secret.clone();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();
    assert!(report.key_is_escrow);
    assert_eq!(
        report.receipts_from_tape, 0,
        "an old tape carries no receipt"
    );
    assert_eq!(report.attested, UNITS.len());
    assert_eq!(report.unknown_remaining, 0);
    assert!(markers(&conn, &vol.escrow_public)
        .iter()
        .all(|m| *m == "yes"));
}

/// The operator key opens every slice too — and that proves operator
/// coverage, which is not the claim. It must not attest.
#[test]
fn the_operator_key_does_not_attest() {
    let mut vol = build_sealed_volume_with(CatalogDb::Old);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    register_escrow(&conn, &vol.escrow_public);
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();
    assert!(!report.key_is_escrow);
    assert_eq!(report.attested, 0);
    assert_eq!(report.unknown_remaining, UNITS.len() as i64);
    assert!(markers(&conn, &vol.escrow_public).iter().all(|m| *m == "?"));
}

/// Review finding 4 made load-bearing: the escrow key attests only when it
/// is the escrow recipient this catalog has REGISTERED. A post-disaster
/// `init` mints a replacement identity; until the original is imported,
/// nothing attests — and the report says the key was not recognised.
#[test]
fn an_unregistered_escrow_key_does_not_attest() {
    let mut vol = build_sealed_volume_with(CatalogDb::Old);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path()); // nothing registered
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.escrow_secret.clone();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();
    assert!(!report.key_is_escrow);
    assert_eq!(report.attested, 0);
    assert_eq!(report.unknown_remaining, UNITS.len() as i64);
}

/// Q10: attestation lives in `catalog rebuild`, so a second run with the
/// escrow key fills in what the first (operator-key) run left unknown —
/// "insert what is missing", applied to a column — and a third run is a
/// no-op again.
#[test]
fn a_second_run_with_the_escrow_key_fills_what_the_first_left_unknown() {
    let mut vol = build_sealed_volume_with(CatalogDb::Old);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    register_escrow(&conn, &vol.escrow_public);
    let scratch = tempfile::tempdir().unwrap();

    let op = vol.operator_secret.clone();
    let first = rebuild(&conn, &mut vol, &op, scratch.path()).unwrap();
    assert_eq!(first.unknown_remaining, UNITS.len() as i64);

    let esc = vol.escrow_secret.clone();
    let second = rebuild(&conn, &mut vol, &esc, scratch.path()).unwrap();
    assert_eq!(second.units, 0, "nothing new to insert");
    assert_eq!(second.attested, UNITS.len());
    assert!(!second.is_noop(), "attesting is a change worth reporting");
    assert_eq!(second.unknown_remaining, 0);

    let third = rebuild(&conn, &mut vol, &esc, scratch.path()).unwrap();
    assert!(third.is_noop(), "{third:?}");
}

// ── Contact corroboration on the rebuild path (ADR-0012, issues #193/#158) ──
//
// `catalog rebuild` is a contact under CONTEXT.md's definition, and the one
// where issue #193's absence rule matters most: rebuild exists FOR the catalog
// that does not know this tape, so "no claim to corroborate" is the normal
// case, not a failure.

/// The acceptance criterion in as many words: **the DR path works with no
/// catalog row at all.** A rebuilt machine has keys, a fresh database and no
/// `backend add`, so there is no volume row, no cartridge row and no medium
/// serial — three absences, and an absence never refuses.
///
/// Every other test in this file relies on this; it is stated once on its own
/// so that a corroboration change that broke it fails by NAME rather than as
/// nineteen unrelated-looking failures.
#[test]
fn the_dr_path_rebuilds_with_no_catalog_row_at_all() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let existing: i64 = conn
        .query_row("SELECT COUNT(*) FROM volumes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(existing, 0, "the DR case is a catalog that knows nothing");

    let report = rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect("a rebuild with no catalog claim must proceed");
    assert!(report.volume_inserted);
}

/// ...and the contradiction it CAN refuse: a catalog that already binds this
/// volume to a cartridge whose serial is not the one in the drive. Proves the
/// rebuild contact actually calls the rule — "a contact that skips
/// corroboration is the defect returning".
#[test]
fn rebuild_refuses_when_the_catalog_binds_that_volume_to_another_cartridge() {
    // The tape's OWN File 0 must agree with what the drive reports below
    // (`"SER-DRIVE"`) — this test is about the CATALOG's cartridge binding
    // (`SER-SHELF`) disagreeing with the drive, a different contradiction
    // from issue #165 item 4's File-0-vs-drive check. Using the shared
    // `"REBUILDSERIAL"` default here would trip that check first, on a
    // string this test never intended to be a claim about anything.
    let mut vol = build_sealed_volume_full(CatalogDb::New, "SER-DRIVE", Some("mam"));
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
        rusqlite::params![LABEL],
    )
    .unwrap();
    let volume_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number, status)
         VALUES ('BC-SHELF', 'LTO-6', 2500000000000, 'SER-SHELF', 'in_use')",
        [],
    )
    .unwrap();
    let cartridge_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO cartridge_volumes (cartridge_id, volume_id, identity_source)
         VALUES (?1, ?2, 'mam')",
        rusqlite::params![cartridge_id, volume_id],
    )
    .unwrap();

    let key_dir = tempfile::tempdir().unwrap();
    let key = key_file(key_dir.path(), "k.age.key", &vol.operator_secret);
    let secret_str = tapectl::crypto::keys::read_secret_key(&key).unwrap();
    let identity: age::x25519::Identity = secret_str.parse().unwrap();
    let drive_mam = MamInfo {
        serial: Some("SER-DRIVE".to_string()),
        ..MamInfo::default()
    };

    let err = rebuild::rebuild_from_store(
        &conn,
        &mut vol.store,
        &[identity],
        Some(LABEL),
        "recovered",
        Some("lto0"),
        scratch.path(),
        "memstore",
        ContactSite::new(
            cfg(),
            Operation::CatalogRebuild,
            "memstore",
            Medium::Observed {
                backend: backend(),
                mam: &drive_mam,
            },
        ),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("wrong cartridge"), "{err}");
    assert!(
        err.contains("SER-SHELF"),
        "must name the catalog's claim: {err}"
    );
    assert!(
        err.contains("SER-DRIVE"),
        "must name what the drive holds: {err}"
    );
}

/// Seed one unit whose single stage set has a completed write on every
/// volume in `volume_ids` — the shape `retire_impacts` counts (unit →
/// snapshot → stage_set → writes.status = 'completed', and the OTHER
/// volume's own `status` decides whether it still contributes a copy).
fn seed_unit_written_to(
    conn: &rusqlite::Connection,
    tenant_id: i64,
    name: &str,
    volume_ids: &[i64],
) {
    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, current_path, status)
         VALUES (?1, ?2, ?3, ?4, 'tape_only')",
        rusqlite::params![
            format!("displaced-uuid-{name}"),
            name,
            tenant_id,
            format!("/src/{name}")
        ],
    )
    .unwrap();
    let unit_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
         VALUES (?1, 1, 'current', ?2, 1, 10)",
        rusqlite::params![unit_id, format!("/src/{name}")],
    )
    .unwrap();
    let snapshot_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, slice_size, status)
         VALUES (?1, 524288, 'staged')",
        rusqlite::params![snapshot_id],
    )
    .unwrap();
    let stage_set_id = conn.last_insert_rowid();
    for vid in volume_ids {
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'completed')",
            rusqlite::params![stage_set_id, snapshot_id, vid],
        )
        .unwrap();
    }
}

/// The fixture both issue #235 tests share: a cartridge the tape's own
/// serial matches, already bound to a live sealed volume `L6-STALE` that
/// this rebuild will displace. `photos` has its ONLY completed write there;
/// `archive` has a second one on another sealed volume, so it still has a
/// copy when `L6-STALE` is erased. Returns the stale volume's row id.
fn seed_displaced_volume_with_units(conn: &rusqlite::Connection) -> i64 {
    conn.execute(
        "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number, status)
         VALUES ('L6-0001', 'LTO-6', 2400000000, 'REBUILDSERIAL', 'in_use')",
        [],
    )
    .unwrap();
    let cartridge_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes,
                              status)
         VALUES ('L6-STALE', 'lto', 'lto0', 'LTO-6', 2400000000, 'sealed')",
        [],
    )
    .unwrap();
    let stale_vol = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO cartridge_volumes (cartridge_id, volume_id, identity_source)
         VALUES (?1, ?2, 'mam')",
        rusqlite::params![cartridge_id, stale_vol],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes,
                              status)
         VALUES ('L6-ELSEWHERE', 'lto', 'lto0', 'LTO-6', 2400000000, 'sealed')",
        [],
    )
    .unwrap();
    let other_vol = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('displaced-op', 1, 'active')",
        [],
    )
    .unwrap();
    let tenant_id = conn.last_insert_rowid();

    seed_unit_written_to(conn, tenant_id, "photos", &[stale_vol]);
    seed_unit_written_to(conn, tenant_id, "archive", &[stale_vol, other_vol]);
    stale_vol
}

fn rebuild_event_detail(conn: &rusqlite::Connection) -> String {
    conn.query_row(
        "SELECT details FROM events
         WHERE entity_type = 'volume' AND action = 'catalog_rebuild'",
        [],
        |r| r.get(0),
    )
    .expect("a catalog_rebuild event")
}

/// Issue #235, the ADR-0004 Tier-1 promise on the disaster-recovery path:
/// `mount_and_record` computes, BEFORE it flips the row, which units the
/// displacement leaves at zero copies — and `catalog rebuild` discarded it
/// in the same statement that received it, keeping only the label. A unit
/// could reach ZERO copies during an irreversible step and nothing said so.
///
/// Two units on the displaced volume on purpose: naming both would be as
/// wrong as naming neither.
#[test]
fn a_rebuild_names_the_unit_a_displacement_takes_to_zero_copies() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    seed_displaced_volume_with_units(&conn);

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    // --- the structured evidence the report now carries ------------------
    assert_eq!(report.displaced.len(), 1, "{report:?}");
    let d = &report.displaced[0];
    assert_eq!(d.label, "L6-STALE");
    assert_eq!(
        d.units
            .iter()
            .map(|u| (u.unit_name.as_str(), u.other_copies))
            .collect::<Vec<_>>(),
        vec![("archive", 1), ("photos", 0)],
        "`archive` keeps its copy on L6-ELSEWHERE; `photos` has none left"
    );
    assert_eq!(d.zero_copy_units(), vec!["photos"]);

    // --- the text `catalog rebuild` prints, from the one renderer --------
    // Exactly the lines `volume init` prints for the same displacement.
    assert_eq!(
        d.warning,
        vec![
            "warning: cartridge L6-0001 previously held volume \"L6-STALE\"; it is now marked \
             erased because these bytes are being overwritten (ADR-0010)."
                .to_string(),
            "         unit \"archive\" [tape_only]: 1 other copy/copies remain (coverage for \
             unit \"archive\" rests on L6-ELSEWHERE, never verified)"
                .to_string(),
            "         *** unit \"photos\" [tape_only] now has ZERO copies ***".to_string(),
        ]
    );

    // The events row is the ledger of claims (ADR-0001): it must record the
    // zero-copy fact, not merely the displaced label.
    let detail = rebuild_event_detail(&conn);
    assert!(
        detail.contains("L6-STALE"),
        "the events detail must still name the displaced volume: {detail}"
    );
    assert!(
        detail.contains("unit \"photos\"") && detail.contains("ZERO copies"),
        "the events detail must name the unit this displacement took to zero: {detail}"
    );
    assert!(
        !detail.contains("unit \"archive\" [tape_only] now has ZERO copies"),
        "`archive` still has a copy on L6-ELSEWHERE; it must not be called zero: {detail}"
    );
}

/// The other half of #235, and the reason the fixture stages two units: a
/// displaced volume whose units still have coverage elsewhere must NOT be
/// called zero. A warning that names every unit is as useless as one that
/// names none — ADR-0004 Tier 1 is about the fact that matters, not about
/// volume of output.
#[test]
fn a_rebuild_does_not_call_a_still_covered_unit_zero() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let stale_vol = seed_displaced_volume_with_units(&conn);
    // `photos` gets a second completed write on the same eligible sealed
    // volume `archive` already uses, so NOTHING on L6-STALE is at zero.
    let other_vol: i64 = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = 'L6-ELSEWHERE'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let photos_stage_set: i64 = conn
        .query_row(
            "SELECT ss.id FROM stage_sets ss
             JOIN snapshots s ON s.id = ss.snapshot_id
             JOIN units u ON u.id = s.unit_id
             WHERE u.name = 'photos'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let photos_snapshot: i64 = conn
        .query_row(
            "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
            rusqlite::params![photos_stage_set],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
         VALUES (?1, ?2, ?3, 'completed')",
        rusqlite::params![photos_stage_set, photos_snapshot, other_vol],
    )
    .unwrap();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    // The displacement still happened and is still reported — this test is
    // about the CLASSIFICATION, not about suppressing the warning.
    assert_eq!(report.displaced.len(), 1, "{report:?}");
    let d = &report.displaced[0];
    assert_eq!(d.label, "L6-STALE");
    let stale_status: String = conn
        .query_row(
            "SELECT status FROM volumes WHERE id = ?1",
            rusqlite::params![stale_vol],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stale_status, "erased");

    assert!(
        d.zero_copy_units().is_empty(),
        "both units still have a copy on L6-ELSEWHERE: {:?}",
        d.units
    );
    assert!(
        !d.warning.iter().any(|l| l.contains("ZERO copies")),
        "{:?}",
        d.warning
    );
    let detail = rebuild_event_detail(&conn);
    assert!(detail.contains("L6-STALE"), "{detail}");
    assert!(
        !detail.contains("ZERO copies"),
        "nothing went to zero here: {detail}"
    );
}

// ── Issue #379: two stage sets of one Version on two tapes ───────────────

const LABEL_B: &str = "REBUILD02";
const VOL_UUID_B: &str = "66666666-7777-8888-9999-000000000000";

/// Version 7 of every unit, staged and written to tape A, then staged AGAIN
/// and written to tape B — what re-staging a Version for a later copy
/// produces. Same content, same unit uuids, same keys; fresh age file keys,
/// so different ciphertext. Different chip serials: these are two
/// cartridges, and one serial would make tape B's rebuild displace tape A.
fn two_tapes_of_one_version() -> (SealedVolume, SealedVolume) {
    let keys = FixtureKeys::fresh();
    let a = build_tape(TapeSpec {
        catalog_db: CatalogDb::New,
        mam_serial: "SERIAL-A",
        cartridge_identity_source: Some("mam"),
        label: LABEL,
        volume_uuid: VOL_UUID,
        keys: &keys,
        slices_per_unit: 2,
    });
    let b = build_tape(TapeSpec {
        catalog_db: CatalogDb::New,
        mam_serial: "SERIAL-B",
        cartridge_identity_source: Some("mam"),
        label: LABEL_B,
        volume_uuid: VOL_UUID_B,
        keys: &keys,
        slices_per_unit: 2,
    });
    (a, b)
}

/// Restore's own selection for every unit on `vol`, then restore's own
/// ciphertext check on every slice it selects: read the tape file at the
/// catalog's position, bound it to the catalog's `encrypted_bytes`, and
/// compare its sha256 with the catalog's `sha256_encrypted` — what
/// `volume::restore`'s `read_slice_inner` does before anything is
/// decrypted. A catalog whose rows belong to another tape fails here exactly
/// as a restore from that tape would.
fn assert_restorable_from(conn: &rusqlite::Connection, vol: &mut SealedVolume) {
    use tapectl::store::Store;
    for (unit_name, _, _) in UNITS {
        let selection =
            tapectl::volume::restore::select_write_positions(conn, unit_name, &vol.label, None)
                .unwrap_or_else(|e| {
                    panic!("restore cannot resolve {unit_name} on {}: {e}", vol.label)
                });
        assert_eq!(selection.version, 7);
        assert_eq!(selection.positions.len(), 2, "{unit_name} on {}", vol.label);
        for p in &selection.positions {
            let mut bytes = Vec::new();
            vol.store
                .read_file(p.position.parse().unwrap(), &mut bytes)
                .unwrap();
            bytes.truncate(p.encrypted_bytes as usize);
            assert_eq!(
                sha256_hex(&bytes),
                p.sha256_encrypted,
                "{unit_name} slice {} on {}: the tape's ciphertext does not match the catalog's \
                 hash, so a restore would refuse it as corrupt",
                p.slice_number,
                vol.label
            );
        }
    }
}

fn unit_id(conn: &rusqlite::Connection, unit_name: &str) -> i64 {
    conn.query_row(
        "SELECT id FROM units WHERE name = ?1",
        rusqlite::params![unit_name],
        |r| r.get(0),
    )
    .unwrap()
}

/// The unit's copy count through the one shared expression (per Version:
/// a unit is as covered as its least-covered live version).
fn copies(conn: &rusqlite::Connection, unit_name: &str) -> i64 {
    let sql = format!(
        "SELECT {}",
        tapectl::policy::coverage::copy_count_expr(
            &tapectl::policy::coverage::CoverageQuery::current_unit("?1")
        )
    );
    conn.query_row(&sql, rusqlite::params![unit_id(conn, unit_name)], |r| {
        r.get(0)
    })
    .unwrap()
}

fn stage_sets_of_v7(conn: &rusqlite::Connection, unit_name: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM stage_sets ss JOIN snapshots s ON s.id = ss.snapshot_id
         WHERE s.unit_id = ?1 AND s.version = 7",
        rusqlite::params![unit_id(conn, unit_name)],
        |r| r.get(0),
    )
    .unwrap()
}

fn assert_two_tapes_rebuilt(
    conn: &rusqlite::Connection,
    a: &mut SealedVolume,
    b: &mut SealedVolume,
) {
    assert_restorable_from(conn, a);
    assert_restorable_from(conn, b);
    for (unit_name, _, _) in UNITS {
        assert_eq!(
            stage_sets_of_v7(conn, unit_name),
            2,
            "{unit_name}: each tape carries its own stage set of v7"
        );
        assert_eq!(
            copies(conn, unit_name),
            2,
            "{unit_name}: v7 is on two tapes"
        );
    }
}

/// Issue #379: the catalog is lost and rebuilt from both tapes, tape A
/// first. Keyed on the snapshot alone, tape B's positions attached to tape
/// A's slice rows, and every slice of tape B then failed its ciphertext
/// hash against the catalog.
#[test]
fn two_stage_sets_of_one_version_rebuild_apart_a_then_b() {
    let (mut a, mut b) = two_tapes_of_one_version();
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = a.operator_secret.clone();

    rebuild(&conn, &mut a, &secret, scratch.path()).expect("rebuild tape A");
    let report = rebuild(&conn, &mut b, &secret, scratch.path()).expect("rebuild tape B");
    assert_eq!(
        report.stage_sets,
        UNITS.len(),
        "tape B's stage sets are new: {report:?}"
    );
    assert_two_tapes_rebuilt(&conn, &mut a, &mut b);

    // And a re-run of either tape is still a no-op: the key that told the
    // two stage sets apart also finds each one again.
    assert!(rebuild(&conn, &mut a, &secret, scratch.path())
        .unwrap()
        .is_noop());
    assert!(rebuild(&conn, &mut b, &secret, scratch.path())
        .unwrap()
        .is_noop());
}

/// The same, tape B first.
#[test]
fn two_stage_sets_of_one_version_rebuild_apart_b_then_a() {
    let (mut a, mut b) = two_tapes_of_one_version();
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = a.operator_secret.clone();

    rebuild(&conn, &mut b, &secret, scratch.path()).expect("rebuild tape B");
    rebuild(&conn, &mut a, &secret, scratch.path()).expect("rebuild tape A");
    assert_two_tapes_rebuilt(&conn, &mut a, &mut b);
}

/// Issue #379: tape B rebuilt into the LIVE catalog that wrote tape A and
/// still holds tape A's stage set of the same Version.
#[test]
fn a_second_stage_set_rebuilds_beside_the_live_one_it_shares_a_version_with() {
    let (mut a, mut b) = two_tapes_of_one_version();
    let scratch = tempfile::tempdir().unwrap();
    let secret = a.operator_secret.clone();
    // Tape A's live catalog, as the real write session and confirm left it.
    let live = std::mem::replace(
        &mut a.source_conn,
        rusqlite::Connection::open_in_memory().unwrap(),
    );
    for (unit_name, _, _) in UNITS {
        assert_eq!(stage_sets_of_v7(&live, unit_name), 1);
    }

    let report = rebuild(&live, &mut b, &secret, scratch.path()).expect("rebuild tape B");
    assert_eq!(report.stage_sets, UNITS.len(), "{report:?}");
    assert_two_tapes_rebuilt(&live, &mut a, &mut b);
}

/// Issue #379's second criterion: a slice row whose number matches but
/// whose ciphertext does not is never silently reused. Here the catalog's
/// stage set is tape A's (found by its write to this volume), but its second slice
/// row has been altered — the rebuild refuses and changes nothing.
#[test]
fn a_slice_row_with_the_same_number_but_other_ciphertext_is_refused() {
    let (mut a, _) = two_tapes_of_one_version();
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = a.operator_secret.clone();
    rebuild(&conn, &mut a, &secret, scratch.path()).expect("first rebuild");

    conn.execute(
        "UPDATE stage_slices SET sha256_encrypted = 'feedface' WHERE slice_number = 2",
        [],
    )
    .unwrap();
    let before = row_counts(&conn);

    let err = rebuild(&conn, &mut a, &secret, scratch.path())
        .expect_err("a slice row with other ciphertext must not be reused");
    let msg = err.to_string();
    assert!(
        msg.contains("feedface") && msg.contains("rolled back"),
        "the refusal names both hashes: {msg}"
    );
    assert_eq!(
        before,
        row_counts(&conn),
        "a refused rebuild changes nothing"
    );
}

/// Issue #379's asymmetry, closed: a stage set used to be found only by its
/// FIRST slice's ciphertext, so a catalog whose slice 1 disagreed with the
/// tape did not refuse as a slice-2 disagreement does (above) -- it missed
/// the stage set, minted a sibling of the same Version and wrote the
/// Version to this volume a second time. Now the stage set already written
/// to this volume is the one this tape's slices are compared with, every
/// slice of it, and any mismatch refuses naming the slice.
#[test]
fn a_first_slice_row_with_other_ciphertext_is_refused_and_mints_no_sibling() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    rebuild(&conn, &mut vol, &secret, scratch.path()).expect("first rebuild");

    let original: Vec<(i64, String)> = conn
        .prepare("SELECT id, sha256_encrypted FROM stage_slices WHERE slice_number = 1")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(original.len(), UNITS.len(), "positive control");
    conn.execute(
        "UPDATE stage_slices SET sha256_encrypted = 'feedface' WHERE slice_number = 1",
        [],
    )
    .unwrap();
    let before = row_counts(&conn);

    let err = rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect_err("a first slice row with other ciphertext must not be bypassed");
    let msg = err.to_string();
    assert!(
        msg.contains("slice 1 of stage set") && msg.contains("feedface"),
        "the refusal names the slice and both hashes: {msg}"
    );
    assert!(msg.contains("rolled back"), "{msg}");
    assert_eq!(
        before,
        row_counts(&conn),
        "a refused rebuild changes nothing"
    );
    for (unit_name, _, _) in UNITS {
        assert_eq!(
            stage_sets_of_v7(&conn, unit_name),
            1,
            "{unit_name}: no sibling stage set"
        );
    }

    // The positive control: put the hashes back and the same rebuild is the
    // no-op it was.
    for (id, sha) in &original {
        conn.execute(
            "UPDATE stage_slices SET sha256_encrypted = ?1 WHERE id = ?2",
            rusqlite::params![sha, id],
        )
        .unwrap();
    }
    assert!(rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect("restored hashes rebuild")
        .is_noop());
}

/// The same refusal into the LIVE catalog that wrote the tape: its stage
/// set is found by the write that put it on this volume, not by a hash the
/// catalog no longer agrees with.
#[test]
fn a_first_slice_mismatch_in_the_writing_catalog_is_refused() {
    let mut vol = build_sealed_volume(true);
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let live = std::mem::replace(
        &mut vol.source_conn,
        rusqlite::Connection::open_in_memory().unwrap(),
    );
    live.execute(
        "UPDATE stage_slices SET sha256_encrypted = 'feedface' WHERE slice_number = 1",
        [],
    )
    .unwrap();
    let before = row_counts(&live);

    let err = rebuild(&live, &mut vol, &secret, scratch.path())
        .expect_err("the writing catalog's own stage set disagrees with the tape");
    assert!(err.to_string().contains("slice 1 of stage set"), "{err}");
    assert_eq!(
        before,
        row_counts(&live),
        "a refused rebuild changes nothing"
    );
}

/// `ensure_stage_set`'s step 2: with no write row on this volume to find
/// the stage set by (step 1), it is found by ANY of this tape's slices whose
/// ciphertext the catalog shares -- here slice 2, slice 1 having been
/// altered -- and then every slice of it is compared with the tape, so the
/// slice-1 disagreement refuses rather than minting a sibling.
#[test]
fn a_first_slice_mismatch_with_no_write_row_is_found_by_a_later_slice_and_refused() {
    let (mut a, _) = two_tapes_of_one_version();
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = a.operator_secret.clone();
    rebuild(&conn, &mut a, &secret, scratch.path()).expect("first rebuild");
    conn.execute_batch(
        "PRAGMA foreign_keys = OFF; DELETE FROM write_positions; DELETE FROM writes; \
         PRAGMA foreign_keys = ON; \
         UPDATE stage_slices SET sha256_encrypted = 'feedface' WHERE slice_number = 1;",
    )
    .unwrap();
    // The precondition: no write row for step 1 to find the stage set by.
    let writes: i64 = conn
        .query_row("SELECT COUNT(*) FROM writes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(writes, 0);
    let before = row_counts(&conn);

    let err = rebuild(&conn, &mut a, &secret, scratch.path()).expect_err("refused");
    assert!(err.to_string().contains("slice 1 of stage set"), "{err}");
    assert!(err.to_string().contains("feedface"), "{err}");
    assert_eq!(
        before,
        row_counts(&conn),
        "a refused rebuild changes nothing"
    );
    for (unit_name, _, _) in UNITS {
        assert_eq!(
            stage_sets_of_v7(&conn, unit_name),
            1,
            "{unit_name}: no sibling stage set"
        );
    }
}

/// The known boundary of step 2: a stage set is matched by a slice whose
/// ciphertext it shares with the tape, so a ONE-slice unit whose only slice
/// disagrees, with no write row on this volume to find it by, shares
/// nothing -- indistinguishable from a different staging of the same
/// Version (issue #379). A new stage set is minted beside the altered one
/// rather than refused. Pinned so a change to that is a decision, not a
/// surprise.
#[test]
fn known_boundary_a_single_slice_mismatch_with_no_write_row_mints_a_new_stage_set() {
    let keys = FixtureKeys::fresh();
    let mut vol = build_tape(TapeSpec {
        catalog_db: CatalogDb::New,
        mam_serial: "SERIAL-A",
        cartridge_identity_source: Some("mam"),
        label: LABEL,
        volume_uuid: VOL_UUID,
        keys: &keys,
        slices_per_unit: 1,
    });
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    rebuild(&conn, &mut vol, &secret, scratch.path()).expect("first rebuild");
    let slices: i64 = conn
        .query_row("SELECT COUNT(*) FROM stage_slices", [], |r| r.get(0))
        .unwrap();
    assert_eq!(slices, UNITS.len() as i64, "one slice per unit");
    conn.execute_batch(
        "PRAGMA foreign_keys = OFF; DELETE FROM write_positions; DELETE FROM writes; \
         PRAGMA foreign_keys = ON; \
         UPDATE stage_slices SET sha256_encrypted = 'feedface' WHERE slice_number = 1;",
    )
    .unwrap();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect("nothing on this tape names the altered stage set, so nothing refuses");
    assert_eq!(report.stage_sets, UNITS.len(), "{report:?}");
    for (unit_name, _, _) in UNITS {
        assert_eq!(
            stage_sets_of_v7(&conn, unit_name),
            2,
            "{unit_name}: the altered stage set and a new one beside it"
        );
    }
    // The new stage set is the tape's: restore resolves it and its
    // ciphertext checks out.
    use tapectl::store::Store;
    for (unit_name, _, _) in UNITS {
        let selection =
            tapectl::volume::restore::select_write_positions(&conn, unit_name, &vol.label, None)
                .unwrap();
        assert_eq!(selection.positions.len(), 1, "{unit_name}");
        let p = &selection.positions[0];
        let mut bytes = Vec::new();
        vol.store
            .read_file(p.position.parse().unwrap(), &mut bytes)
            .unwrap();
        bytes.truncate(p.encrypted_bytes as usize);
        assert_eq!(sha256_hex(&bytes), p.sha256_encrypted, "{unit_name}");
    }
}

/// A `MemStore` that, before every read, checks from a SECOND connection
/// whether the catalog's write lock is free (`BEGIN IMMEDIATE` with no busy
/// wait). Issue #413: a rebuild must never hold the write lock across tape
/// I/O, which on a real drive takes seconds per positioning and would stall
/// every other tapectl command for that long.
struct LockProbeStore {
    inner: MemStore,
    db: std::path::PathBuf,
    reads: usize,
    locked_reads: Vec<u32>,
}

impl LockProbeStore {
    fn probe(&mut self, position: u32) {
        self.reads += 1;
        let other = rusqlite::Connection::open(&self.db).unwrap();
        other.busy_timeout(std::time::Duration::ZERO).unwrap();
        match other.execute_batch("BEGIN IMMEDIATE") {
            Ok(()) => other.execute_batch("ROLLBACK").unwrap(),
            Err(_) => self.locked_reads.push(position),
        }
    }
}

impl tapectl::store::Store for LockProbeStore {
    fn capacity(&mut self) -> tapectl::error::Result<tapectl::store::CapacityReport> {
        self.inner.capacity()
    }
    fn execute(
        &mut self,
        src: &mut dyn std::io::Read,
        len: u64,
        sync: bool,
    ) -> tapectl::error::Result<u64> {
        self.inner.execute(src, len, sync)
    }
    fn read_file(
        &mut self,
        position: u32,
        sink: &mut dyn std::io::Write,
    ) -> tapectl::error::Result<u64> {
        self.probe(position);
        self.inner.read_file(position, sink)
    }
    fn read_file_head(
        &mut self,
        position: u32,
        max_bytes: u64,
        sink: &mut dyn std::io::Write,
    ) -> tapectl::error::Result<u64> {
        self.probe(position);
        self.inner.read_file_head(position, max_bytes, sink)
    }
    fn reposition_for_resume(&mut self, file_index: u32) -> tapectl::error::Result<()> {
        self.inner.reposition_for_resume(file_index)
    }
}

/// Issue #413: no tape read happens while the rebuild holds the catalog's
/// write lock. Attestation reads one slice header per unit, and it used to
/// run inside the rebuild's one IMMEDIATE transaction.
#[test]
fn a_rebuild_reads_no_tape_while_it_holds_the_write_lock() {
    let vol = build_sealed_volume_with(CatalogDb::Old);
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("rebuilt.db");
    let conn = fresh_db(dir.path());
    register_escrow(&conn, &vol.escrow_public);
    let scratch = tempfile::tempdir().unwrap();

    let key_dir = tempfile::tempdir().unwrap();
    let key = key_file(key_dir.path(), "k.age.key", &vol.escrow_secret);
    let identity: age::x25519::Identity = tapectl::crypto::keys::read_secret_key(&key)
        .unwrap()
        .parse()
        .unwrap();
    let mut store = LockProbeStore {
        inner: vol.store,
        db: db_path,
        reads: 0,
        locked_reads: Vec::new(),
    };

    // Positive control: the probe sees the rebuild connection's write lock.
    let held = db::busy::immediate_tx(&conn).unwrap();
    store.probe(u32::MAX);
    held.rollback().unwrap();
    assert_eq!(
        store.locked_reads,
        vec![u32::MAX],
        "positive control: the probe must see a held write lock on this catalog"
    );
    store.locked_reads.clear();
    store.reads = 0;

    let site = ContactSite::new(
        cfg(),
        Operation::CatalogRebuild,
        "memstore",
        Medium::NoBackend,
    );
    let report = rebuild::rebuild_from_store(
        &conn,
        &mut store,
        &[identity],
        Some(LABEL),
        "recovered",
        Some("lto0"),
        scratch.path(),
        "memstore",
        site,
    )
    .unwrap();

    assert_eq!(
        report.attested,
        UNITS.len(),
        "positive control: attestation read a slice header per unit"
    );
    assert!(
        store.reads > UNITS.len(),
        "positive control: {} reads",
        store.reads
    );
    assert!(
        store.locked_reads.is_empty(),
        "tape positions read while the write lock was held: {:?}",
        store.locked_reads
    );
}

// ── Issue #458: a rebuilt tape with no valid seal is not a copy ──────────
//
// ADR-0012: `catalog rebuild --from-volume` "asserts `sealed` only after
// reading the seal marker; otherwise the row is present-but-unverified and
// ineligible (#158)". The shape a never-sealed tape rebuilds to is the shape
// the catalog gives a native aborted write: the volume `initialized`, its
// seal unrecorded, its writes `aborted`.

/// Remove the seal marker, the last file the write session laid down, as an
/// interrupted or aborted write leaves the tape. The premise is checked: what
/// is removed really is a seal marker.
fn drop_seal_marker(vol: &mut SealedVolume) {
    let seal = String::from_utf8_lossy(vol.store.files.last().expect("a written tape"));
    assert!(
        tapectl::volume::format::parse_seal_marker(&seal).is_ok(),
        "premise: the last file of the tape is its seal marker"
    );
    vol.store.files.pop();
    vol.store.syncs.pop();
}

/// `(status, observed_condition, sealed_at)` of a volume row.
fn volume_state(conn: &rusqlite::Connection, label: &str) -> (String, String, Option<String>) {
    conn.query_row(
        "SELECT status, observed_condition, sealed_at FROM volumes WHERE label = ?1",
        rusqlite::params![label],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

/// How many of a volume's `writes` rows carry `status`, and how many it has.
fn write_counts(conn: &rusqlite::Connection, label: &str, status: &str) -> (i64, i64) {
    conn.query_row(
        "SELECT COALESCE(SUM(w.status = ?2), 0), COUNT(*)
         FROM writes w JOIN volumes v ON v.id = w.volume_id WHERE v.label = ?1",
        rusqlite::params![label, status],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

/// Positions of the volume that claim to be `written`.
fn written_positions(conn: &rusqlite::Connection, label: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM write_positions wp
         JOIN writes w ON w.id = wp.write_id JOIN volumes v ON v.id = w.volume_id
         WHERE v.label = ?1 AND wp.status = 'written'",
        rusqlite::params![label],
        |r| r.get(0),
    )
    .unwrap()
}

/// The catalog ruling itself. Before #458 the new-volume arm hardcoded
/// `'sealed'` and every write `completed`, so a tape whose write never
/// finished (envelopes and the front index precede the slices) rebuilt to a
/// sealed volume with `written` positions for slices that may never have been
/// laid down, and `audit` and `report copies` counted it.
#[test]
fn a_tape_with_no_seal_marker_rebuilds_as_an_aborted_write_that_is_not_a_copy() {
    let mut vol = build_sealed_volume(true);
    drop_seal_marker(&mut vol);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect("an unsealed tape still rebuilds: the rows are present, just not a copy");
    assert!(report.volume_inserted);

    let (status, condition, sealed_at) = volume_state(&conn, LABEL);
    assert_eq!(
        status, "initialized",
        "the rebuild read no seal marker, so it may not assert `sealed`"
    );
    assert_eq!(
        condition, "ok",
        "an incomplete recording is not a bad medium"
    );
    assert_eq!(sealed_at, None, "no seal was read, so none is recorded");

    let (aborted, total) = write_counts(&conn, LABEL, "aborted");
    assert!(total > 0, "positive control: the writes rows exist");
    assert_eq!(aborted, total, "every write is an aborted session");
    assert_eq!(
        written_positions(&conn, LABEL),
        0,
        "the rebuild saw the plan (the front index), not the slices"
    );

    for (unit_name, _, _) in UNITS {
        assert_eq!(copies(&conn, unit_name), 0, "{unit_name}: not a copy");
    }
    assert!(!tapectl::policy::coverage::counts_as_copy(&conn, LABEL).unwrap());
    let vol_id: i64 = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            rusqlite::params![LABEL],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!tapectl::policy::coverage::has_completed_write(&conn, vol_id).unwrap());
    for (unit_name, _, _) in UNITS {
        assert!(
            tapectl::policy::coverage::versions_at_stake(&conn, unit_id(&conn, unit_name), vol_id)
                .unwrap()
                .is_empty(),
            "{unit_name}: retiring this volume removes no coverage, so `volume retire` is not \
             gated on it"
        );
    }
}

fn volume_id_of(conn: &rusqlite::Connection, label: &str) -> i64 {
    conn.query_row(
        "SELECT id FROM volumes WHERE label = ?1",
        rusqlite::params![label],
        |r| r.get(0),
    )
    .unwrap()
}

/// Rewrite the seal marker (the tape's last file) in place.
fn edit_seal_marker(vol: &mut SealedVolume, edit: impl FnOnce(&str) -> String) {
    let file = vol.store.files.last_mut().expect("a written tape");
    let text = String::from_utf8_lossy(file).into_owned();
    let edited = edit(&text);
    assert_ne!(edited, text, "premise: the edit changed the seal marker");
    *file = edited.into_bytes();
}

/// The control the test above cannot be: a SEALED tape still rebuilds to a
/// `sealed` volume with `completed` writes and `written` positions, which
/// counts as a copy. Without it, "an unsealed tape is not a copy" could be
/// "nothing is a copy".
#[test]
fn a_sealed_tape_rebuilds_sealed_and_counts_as_a_copy() {
    let mut vol = build_sealed_volume(true);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();
    assert_eq!(
        report.unsealed_reason, None,
        "the seal read and bound the tape"
    );

    let (status, condition, sealed_at) = volume_state(&conn, LABEL);
    assert_eq!((status.as_str(), condition.as_str()), ("sealed", "ok"));
    assert_eq!(sealed_at.as_deref(), Some(tape_sealed_at(&vol).as_str()));
    let (completed, total) = write_counts(&conn, LABEL, "completed");
    assert!(total > 0 && completed == total, "{completed} of {total}");
    let slices: usize = vol.expected_positions.iter().map(|(_, p)| p.len()).sum();
    assert_eq!(written_positions(&conn, LABEL), slices as i64);
    for (unit_name, _, _) in UNITS {
        assert_eq!(copies(&conn, unit_name), 1, "{unit_name}");
    }
    assert!(tapectl::policy::coverage::counts_as_copy(&conn, LABEL).unwrap());
}

/// The trace of the status chosen for #458 through the commands that read
/// it, by their own public functions rather than a re-derivation. A never-
/// sealed rebuilt volume is catalog-identical to a native aborted write, so
/// every command treats it exactly as it treats that.
#[test]
fn an_unsealed_rebuilt_volume_is_what_every_command_expects_of_an_aborted_write() {
    use tapectl::policy::coverage;
    use tapectl::volume::session::{resume_admission, AbortedAdoption, ResumeAdmission};

    let mut vol = build_sealed_volume(true);
    drop_seal_marker(&mut vol);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();
    assert!(
        report.unsealed_reason.is_some(),
        "the report says the tape is unsealed: {report:?}"
    );
    assert_eq!(
        report.found_volume_status, None,
        "the row was inserted here"
    );
    let vid = volume_id_of(&conn, LABEL);

    // Not a copy, anywhere: the coverage predicates and `holds_sealed_bytes`.
    assert!(!coverage::counts_as_copy(&conn, LABEL).unwrap());
    assert!(!coverage::has_completed_write(&conn, vid).unwrap());
    let holds: bool = conn
        .query_row(
            &format!(
                "SELECT {} FROM volumes WHERE id = ?1",
                coverage::holds_sealed_bytes("volumes")
            ),
            rusqlite::params![vid],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !holds,
        "no sealed bytes are recorded, so retiring it is not gated on them"
    );

    // `volume write` / `volume resume`: an `initialized` volume with no
    // completed write is the target the documented aborted-write remedy
    // (`volume write` starts over from the beginning of the same tape) uses.
    let (status, condition, _) = volume_state(&conn, LABEL);
    assert!(coverage::is_write_target(&status, &condition));
    // `volume resume` does not adopt it: no seal is recorded, so an aborted
    // session is never resumable (ADR-0012, 2026-09-23) ...
    assert_eq!(
        resume_admission(&conn, vid).unwrap(),
        ResumeAdmission::Aborted(AbortedAdoption::NotSealed)
    );
    // ... and #360's adoption of a lost write needs completed writes only.
    assert!(!tapectl::volume::adopt_lost::is_lost_write(&conn, vid).unwrap());

    // `cartridge erase` refuses a volume the catalog has not given up; the
    // way out is `volume retire`, which an aborted write never gates.
    assert!(!coverage::is_given_up(&conn, vid).unwrap());
}

/// A seal marker that reads and parses but binds a DIFFERENT front index is
/// no seal of this tape: the rebuild cannot say what it sealed.
#[test]
fn a_seal_marker_binding_another_front_index_does_not_seal_the_tape() {
    let mut vol = build_sealed_volume(true);
    edit_seal_marker(&mut vol, |text| {
        let line = text
            .lines()
            .find(|l| l.starts_with("front_index_sha256 = "))
            .expect("the seal binds the front index")
            .to_string();
        text.replacen(
            &line,
            &format!("front_index_sha256 = \"{}\"", "0".repeat(64)),
            1,
        )
    });
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();
    let why = report.unsealed_reason.as_deref().expect("not sealed");
    assert!(why.contains("binds a front index hashing to"), "{why}");
    assert_eq!(report.sealed_at, None, "an unbound seal dates nothing");
    assert_eq!(volume_state(&conn, LABEL).0, "initialized");
    assert!(!tapectl::policy::coverage::counts_as_copy(&conn, LABEL).unwrap());
}

/// ... and so is one that names another volume.
#[test]
fn a_seal_marker_naming_another_volume_does_not_seal_the_tape() {
    let mut vol = build_sealed_volume(true);
    edit_seal_marker(&mut vol, |text| {
        text.replacen(
            &format!("volume = \"{LABEL}\""),
            "volume = \"SOMEONEELSE\"",
            1,
        )
    });
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();
    let why = report.unsealed_reason.as_deref().expect("not sealed");
    assert!(why.contains("SOMEONEELSE"), "{why}");
    assert_eq!(volume_state(&conn, LABEL).0, "initialized");
    assert!(!tapectl::policy::coverage::counts_as_copy(&conn, LABEL).unwrap());
}

/// A seal marker requiring a feature this tapectl does not know is REFUSED,
/// as every other reader of the tape refuses it (`volume-format-v2.md` §1.2
/// rule 1): it is not an unsealed tape, it may be a sealed one a newer
/// tapectl wrote, and recording it as an aborted write would be a claim
/// with no evidence. Nothing is written.
#[test]
fn a_seal_marker_requiring_an_unknown_feature_refuses_the_rebuild() {
    let mut vol = build_sealed_volume(true);
    edit_seal_marker(&mut vol, |text| {
        // Since 1.6.0 every seal states `requires = ["short-seal"]`
        // (ADR-0012 item 111); the edit adds an unknown feature beside it.
        let anchor = "requires = [\"short-seal\"]";
        assert_eq!(text.matches(anchor).count(), 1, "{text}");
        text.replacen(anchor, "requires = [\"short-seal\", \"future-feature\"]", 1)
    });
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let err = rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect_err("a seal this tapectl may not read refuses the rebuild")
        .to_string();
    assert!(err.contains("future-feature"), "{err}");
    assert!(err.contains("the seal marker"), "{err}");
    for (table, n) in row_counts(&conn) {
        assert_eq!(n, 0, "nothing was written, but {table} has {n} row(s)");
    }
}

/// A second rebuild of the same unsealed tape adds nothing and says the same
/// thing: the row it finds is the unsealed tape's own shape, which is not a
/// status mismatch (the #360 advice, "verify, then resume seals it", is for
/// a sealed tape under an `initialized` row, and would promise what no
/// verify can do here).
#[test]
fn rebuilding_an_unsealed_tape_twice_is_idempotent_and_not_a_status_mismatch_to_fix() {
    let mut vol = build_sealed_volume(true);
    drop_seal_marker(&mut vol);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let first = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();
    assert!(first.volume_inserted && !first.is_noop());
    let counts = row_counts(&conn);

    let second = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();
    assert!(second.is_noop(), "{second:?}");
    assert_eq!(row_counts(&conn), counts, "a second run inserts nothing");
    assert_eq!(second.found_volume_status.as_deref(), Some("initialized"));
    assert!(
        second.unsealed_reason.is_some(),
        "it still says why: {second:?}"
    );
    assert_eq!(volume_state(&conn, LABEL).0, "initialized");
    assert_eq!(
        write_counts(&conn, LABEL, "aborted").0,
        write_counts(&conn, LABEL, "aborted").1
    );
}

/// A row the catalog already held as `initialized` (a backup taken after
/// `volume init`) under a tape with NO seal: the rebuild leaves the row and
/// attaches the units as `aborted` writes, so #360's lost-write adoption
/// (completed writes only, a valid seal) is not entered and nothing counts.
#[test]
fn an_unsealed_tape_under_an_initialized_row_attaches_aborted_writes() {
    let mut vol = build_sealed_volume(true);
    drop_seal_marker(&mut vol);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    conn.execute(
        "INSERT INTO volumes (label, uuid, backend_type, backend_name, media_type,
                              capacity_bytes, status)
         VALUES (?1, ?2, 'lto', 'lto0', 'LTO-6', 2500000000, 'initialized')",
        rusqlite::params![LABEL, VOL_UUID],
    )
    .unwrap();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();
    assert!(!report.volume_inserted);
    assert_eq!(report.found_volume_status.as_deref(), Some("initialized"));
    assert_eq!(
        report.volume_status_mismatch.as_deref(),
        Some("initialized"),
        "the field keeps its meaning: the found row is not sealed"
    );
    assert!(report.unsealed_reason.is_some());

    let (aborted, total) = write_counts(&conn, LABEL, "aborted");
    assert!(total > 0 && aborted == total, "{aborted} of {total}");
    let vid = volume_id_of(&conn, LABEL);
    assert!(!tapectl::volume::adopt_lost::is_lost_write(&conn, vid).unwrap());
    for (unit_name, _, _) in UNITS {
        assert_eq!(copies(&conn, unit_name), 0, "{unit_name}");
    }
}

// ── Issue #459 item 2: the operator envelope is found by what it is ──────
//
// The operator envelope's manifest files every unit under the placeholder
// tenant "operator". A real tenant may be named that too (`validate_tenant_
// name` accepts it; `operator_name_for_init` falls back to it), and tenant
// envelopes come first on the tape, so identifying the operator envelope by
// the manifest's tenant string picked that tenant's envelope and silently
// dropped every other tenant's units.

/// The front-index positions of the envelopes, in tape order, by kind.
fn envelope_kinds(vol: &SealedVolume) -> Vec<String> {
    let fi = String::from_utf8_lossy(&vol.store.files[3]).into_owned();
    tapectl::volume::format::parse_front_index(&fi)
        .unwrap()
        .into_iter()
        .map(|e| e.type_label)
        .filter(|l| l.contains("envelope"))
        .collect()
}

fn tenant_of_unit(conn: &rusqlite::Connection, unit_name: &str) -> String {
    conn.query_row(
        "SELECT t.name FROM units u JOIN tenants t ON t.id = u.tenant_id WHERE u.name = ?1",
        rusqlite::params![unit_name],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn a_tenant_named_operator_does_not_pass_for_the_operator_envelope() {
    let mut vol = build_sealed_volume_with_a_tenant_named_operator(CatalogDb::New);
    let kinds = envelope_kinds(&vol);
    let first_tenant = kinds.iter().position(|k| k == "tenant_envelope").unwrap();
    let operator = kinds.iter().position(|k| k == "operator_envelope").unwrap();
    assert!(
        first_tenant < operator,
        "premise: a tenant envelope sorts before the operator's: {kinds:?}"
    );
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    assert_eq!(
        report.units,
        UNITS.len(),
        "every tenant's units are rebuilt, not just the one named `operator`"
    );
    assert!(
        report.had_catalog_db,
        "the catalog.db rides in the real operator envelope, not the tenant's"
    );
    for (unit_name, _, _) in UNITS {
        assert_eq!(copies(&conn, unit_name), 1, "{unit_name}");
    }
    assert_eq!(tenant_of_unit(&conn, "photos/2019"), "operator");
    assert_eq!(tenant_of_unit(&conn, "ledgers/fy24"), "bravo");
}

/// The same, where ownership can only come from the tenant envelopes (a
/// catalog.db from before the 2026-09-11 ownership table): the real tenant
/// named `operator` is still a tenant, and its units are filed under it.
#[test]
fn a_tenant_named_operator_still_owns_its_units_when_ownership_comes_from_envelopes() {
    let mut vol = build_sealed_volume_with_a_tenant_named_operator(CatalogDb::Old);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    assert!(
        !report.tenants_from_catalog_db,
        "premise: the catalog.db carries no ownership"
    );
    assert_eq!(report.units, UNITS.len());
    assert!(
        report.units_without_tenant_envelope.is_empty(),
        "the tenant named `operator` has an envelope that names its units: {:?}",
        report.units_without_tenant_envelope
    );
    assert_eq!(tenant_of_unit(&conn, "photos/2019"), "operator");
    assert_eq!(tenant_of_unit(&conn, "photos/2020"), "operator");
    assert_eq!(tenant_of_unit(&conn, "ledgers/fy24"), "bravo");
}

// ── Issue #451: a tenant envelope unreadable in every copy ───────────────
//
// Every envelope has a backup (`volume-format-v2.md` §1.3). When BOTH copies
// of a tenant's envelope are damaged, a generation 2 or 3 rebuild does not
// need them: the operator envelope's `catalog.db` names every unit's owner
// and carries the receipts. It used to refuse anyway.

/// `(type_label, position, sha256_encrypted)` of every envelope in the
/// front index, in tape order.
fn envelope_entries(vol: &SealedVolume) -> Vec<(String, i32, Option<String>)> {
    let fi = String::from_utf8_lossy(&vol.store.files[3]).into_owned();
    tapectl::volume::format::parse_front_index(&fi)
        .unwrap()
        .into_iter()
        .filter(|e| e.type_label.contains("envelope"))
        .map(|e| (e.type_label, e.position, e.sha256_encrypted))
        .collect()
}

/// Damage both copies of the envelope whose primary has the given kind and
/// is the `nth` of its kind: flip a byte in the primary and in the backup
/// carrying the same front-index hash. Returns the two positions.
fn damage_envelope_pair(vol: &mut SealedVolume, primary_kind: &str, nth: usize) -> (i32, i32) {
    let entries = envelope_entries(vol);
    let (_, primary, hash) = entries
        .iter()
        .filter(|(k, ..)| k == primary_kind)
        .nth(nth)
        .cloned()
        .unwrap_or_else(|| panic!("no {primary_kind} #{nth}: {entries:?}"));
    let backup_kind = format!("{primary_kind}_backup");
    let (_, backup, _) = entries
        .iter()
        .find(|(k, _, h)| k == &backup_kind && *h == hash)
        .cloned()
        .unwrap_or_else(|| {
            panic!("premise: {primary_kind} at {primary} has a backup: {entries:?}")
        });
    for pos in [primary, backup] {
        vol.store.files[pos as usize][0] ^= 0xFF;
    }
    (primary, backup)
}

#[test]
fn a_rebuild_goes_on_without_a_tenant_envelope_unreadable_in_every_copy() {
    let mut vol = build_sealed_volume_with(CatalogDb::New);
    let (primary, backup) = damage_envelope_pair(&mut vol, "tenant_envelope", 0);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap_or_else(|e| {
        panic!(
            "a generation 3 catalog.db names every owner, so a lost tenant pair is not fatal: {e}"
        )
    });

    assert_eq!(report.units, UNITS.len(), "every unit is rebuilt");
    assert!(
        report.units_without_tenant_envelope.is_empty(),
        "ownership came from catalog.db, not from the envelope that was lost: {:?}",
        report.units_without_tenant_envelope
    );
    assert_eq!(tenant_of_unit(&conn, "photos/2019"), "alpha");
    assert_eq!(tenant_of_unit(&conn, "ledgers/fy24"), "bravo");
    for (unit_name, _, _) in UNITS {
        assert_eq!(copies(&conn, unit_name), 1, "{unit_name}");
    }
    // The warning names the envelope, where its copies were, and why.
    let warned = report.unreadable_tenant_envelopes.join("\n");
    assert_eq!(report.unreadable_tenant_envelopes.len(), 1, "{warned}");
    assert!(warned.contains(&format!("position {primary}")), "{warned}");
    assert!(warned.contains(&backup.to_string()), "{warned}");
    assert!(warned.contains("tenant_envelope"), "{warned}");
}

/// Generation 2 (2026-09-11 until 1.1.0) names owners too, so it proceeds as
/// well; `catalog.db` generation 1 and a tape with none do not, and keep
/// refusing, now saying why and what else to do.
#[test]
fn a_lost_tenant_pair_is_survivable_only_where_catalog_db_names_the_owners() {
    // Generation 2: ownership and receipts ride in catalog.db.
    let mut vol = build_sealed_volume_with(CatalogDb::Gen2);
    damage_envelope_pair(&mut vol, "tenant_envelope", 1);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let report = rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect("generation 2 names every unit's owner");
    assert_eq!(report.unreadable_tenant_envelopes.len(), 1);
    assert_eq!(report.units, UNITS.len());
    assert!(report.units_without_tenant_envelope.is_empty());

    // Generation 1 and no catalog.db: the tenant envelopes are the only
    // record of ownership, so the rebuild still needs every one.
    for catalog_db in [CatalogDb::Old, CatalogDb::None] {
        let mut vol = build_sealed_volume_with(catalog_db);
        damage_envelope_pair(&mut vol, "tenant_envelope", 0);
        let dir = tempfile::tempdir().unwrap();
        let conn = fresh_db(dir.path());
        let scratch = tempfile::tempdir().unwrap();
        let secret = vol.operator_secret.clone();
        let err = rebuild(&conn, &mut vol, &secret, scratch.path())
            .expect_err("without catalog.db ownership a lost tenant pair refuses the rebuild")
            .to_string();
        assert!(err.contains("could not be opened"), "{err}");
        assert!(
            err.contains("does not name which tenant owns each unit"),
            "{err}"
        );
        assert!(err.contains("RESTORE.sh"), "{err}");
        for (table, n) in row_counts(&conn) {
            assert_eq!(n, 0, "nothing was written, but {table} has {n} row(s)");
        }
    }
}

/// The operator envelope is the one a rebuild cannot do without: lose both
/// of its copies and the rebuild refuses, whatever generation, and says so
/// in the words it always has.
#[test]
fn a_lost_operator_pair_still_refuses_the_rebuild() {
    let mut vol = build_sealed_volume_with(CatalogDb::New);
    let (primary, backup) = damage_envelope_pair(&mut vol, "operator_envelope", 0);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let err = rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect_err("no operator envelope, no rebuild")
        .to_string();
    assert!(
        err.contains(&format!("operator_envelope at position {primary}")),
        "{err}"
    );
    assert!(
        err.contains(&format!("backup copy at position {backup}")),
        "{err}"
    );
    for (table, n) in row_counts(&conn) {
        assert_eq!(n, 0, "{table}");
    }
}

/// A pair damaged on TOP of a lost operator pair does not hide it: the
/// operator envelope is still named.
#[test]
fn a_lost_tenant_pair_does_not_mask_a_lost_operator_pair() {
    let mut vol = build_sealed_volume_with(CatalogDb::New);
    damage_envelope_pair(&mut vol, "tenant_envelope", 0);
    let (primary, _) = damage_envelope_pair(&mut vol, "operator_envelope", 0);
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let err = rebuild(&conn, &mut vol, &secret, scratch.path())
        .expect_err("the operator envelope is lost")
        .to_string();
    assert!(
        err.contains(&format!("operator_envelope at position {primary}")),
        "{err}"
    );
}

// ── Issue #473: a copy written after a rebuild carries its dar catalogues ──

/// A tape whose stage sets had their dar catalogues, built with `keys`.
fn build_tape_with_catalogues(keys: &FixtureKeys) -> SealedVolume {
    WITH_CATALOGUES.with(|w| w.set(true));
    let vol = build_tape(TapeSpec {
        catalog_db: CatalogDb::New,
        mam_serial: "REBUILDSERIAL",
        cartridge_identity_source: Some("mam"),
        label: LABEL,
        volume_uuid: VOL_UUID,
        keys,
        slices_per_unit: 2,
    });
    WITH_CATALOGUES.with(|w| w.set(false));
    vol
}

/// The first eight characters of unit `i`'s uuid in a tape built with
/// catalogues, which name its catalogue file.
fn catalogue_uuid8(i: usize) -> String {
    format!("c{i:07x}")
}

/// A catalog in a tapectl home (a file-backed one: the catalogues are put
/// under the home it lives in), with `vol` rebuilt into it by the operator
/// key. `(home, catalog, report)`.
fn rebuild_into_a_home(
    vol: &mut SealedVolume,
) -> (
    tempfile::TempDir,
    rusqlite::Connection,
    rebuild::RebuildReport,
) {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(&home.path().join("tapectl.db")).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let report = rebuild(&conn, vol, &secret, scratch.path()).unwrap();
    (home, conn, report)
}

fn catalog_path_of(conn: &rusqlite::Connection, unit_name: &str) -> Option<String> {
    conn.query_row(
        "SELECT ss.catalog_path FROM stage_sets ss
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN units u ON u.id = s.unit_id
         WHERE u.name = ?1",
        rusqlite::params![unit_name],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn a_rebuild_puts_each_stage_sets_dar_catalogue_back_where_stage_create_leaves_it() {
    use std::os::unix::fs::PermissionsExt;
    let keys = FixtureKeys::fresh();
    let mut vol = build_tape_with_catalogues(&keys);
    let (home, conn, report) = rebuild_into_a_home(&mut vol);

    assert_eq!(report.catalogues_restored, UNITS.len());
    assert!(
        report.sets_without_catalogue.is_empty(),
        "{:?}",
        report.sets_without_catalogue
    );
    for (i, (unit_name, _, _)) in UNITS.iter().enumerate() {
        let uuid8 = catalogue_uuid8(i);
        let (set_id, unit_uuid): (i64, String) = conn
            .query_row(
                "SELECT ss.id, u.uuid FROM stage_sets ss
                 JOIN snapshots s ON s.id = ss.snapshot_id
                 JOIN units u ON u.id = s.unit_id WHERE u.name = ?1",
                rusqlite::params![unit_name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(unit_uuid.starts_with(&uuid8), "{unit_uuid}");
        // The layout #419 gave `stage create`: per stage set, and `catalog_path`
        // is the stem dar takes `.1.dar` from.
        // The catalog's own spelling of its home: SQLite reports the real
        // path, which differs from a tempdir's when `/tmp` is a link.
        let home_path = home.path().canonicalize().unwrap();
        let dir = home_path.join(format!("catalogs/{uuid8}/s{set_id}"));
        let stem = dir.join(format!("{uuid8}_v7"));
        assert_eq!(
            catalog_path_of(&conn, unit_name).as_deref(),
            Some(stem.to_string_lossy().as_ref()),
            "{unit_name}"
        );
        let file = dir.join(format!("{uuid8}_v7.1.dar"));
        assert_eq!(
            std::fs::read(&file).unwrap(),
            catalogue_bytes(unit_name),
            "{unit_name}: the bytes the tape carried"
        );
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode(&file),
            0o600,
            "{unit_name}: a catalogue lists file names"
        );
        assert_eq!(mode(&dir), 0o700);
    }
    // Idempotent: a second rebuild finds the catalogues there and changes nothing.
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let again = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();
    assert_eq!(again.catalogues_restored, 0);
    assert!(again.is_noop(), "{again:?}");
}

/// A unit this catalog already had under the same NAME but another uuid is
/// the one the tape's sets are filed under (`existing_unit`'s fallback). The
/// next copy's MANIFEST.toml, RECOVERY.md and RESTORE.sh name its catalogue
/// by THAT uuid, so the files are put back under it, not the tape's.
#[test]
fn a_unit_matched_by_name_gets_its_catalogue_under_its_own_uuid() {
    let keys = FixtureKeys::fresh();
    let mut vol = build_tape_with_catalogues(&keys);
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(&home.path().join("tapectl.db")).unwrap();
    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('legacy', 0, 'active')",
        [],
    )
    .unwrap();
    let tenant_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, current_path, status)
         VALUES ('f00dcafe-0000-4000-8000-000000000000', 'photos/2019', ?1, '/src', 'active')",
        rusqlite::params![tenant_id],
    )
    .unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();

    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    assert_eq!(report.catalogues_restored, UNITS.len());
    let path = catalog_path_of(&conn, "photos/2019").expect("a catalog_path");
    assert!(
        path.ends_with("/f00dcafe_v7"),
        "named by the local uuid: {path}"
    );
    assert!(path.contains("/catalogs/f00dcafe/s"), "{path}");
    assert_eq!(
        std::fs::read(format!("{path}.1.dar")).unwrap(),
        catalogue_bytes("photos/2019"),
        "the tape's catalogue bytes"
    );
    // A unit the catalog did not have keeps the tape's own uuid.
    let other = catalog_path_of(&conn, "photos/2020").expect("a catalog_path");
    assert!(
        other.ends_with(&format!("/{}_v7", catalogue_uuid8(1))),
        "{other}"
    );
}

/// A catalog rebuilt before catalogues were put back (or one whose
/// catalogue files were lost) is healed by running the rebuild again.
#[test]
fn a_second_rebuild_gives_a_set_that_lost_its_catalogue_one_again() {
    let keys = FixtureKeys::fresh();
    let mut vol = build_tape_with_catalogues(&keys);
    let (home, conn, first) = rebuild_into_a_home(&mut vol);
    assert_eq!(first.catalogues_restored, UNITS.len());
    // As a pre-fix rebuild left them: no path, nothing on disk.
    conn.execute("UPDATE stage_sets SET catalog_path = NULL", [])
        .unwrap();
    std::fs::remove_dir_all(home.path().join("catalogs")).unwrap();

    let scratch = tempfile::tempdir().unwrap();
    let secret = vol.operator_secret.clone();
    let healed = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();

    assert_eq!(healed.catalogues_restored, UNITS.len());
    assert!(!healed.is_noop());
    for (unit_name, _, _) in UNITS {
        let path = catalog_path_of(&conn, unit_name).expect("a catalog_path again");
        assert!(
            Path::new(&format!("{path}.1.dar")).is_file(),
            "{unit_name}: {path}"
        );
    }
}

/// The operator envelope carries EVERY unit's catalogue, so a tenant
/// envelope unreadable in every copy (#451) costs the rebuild nothing here.
#[test]
fn a_lost_tenant_envelope_pair_does_not_cost_a_unit_its_catalogue() {
    let keys = FixtureKeys::fresh();
    let mut vol = build_tape_with_catalogues(&keys);
    damage_envelope_pair(&mut vol, "tenant_envelope", 0);
    let (_home, conn, report) = rebuild_into_a_home(&mut vol);

    assert_eq!(report.unreadable_tenant_envelopes.len(), 1);
    assert_eq!(report.catalogues_restored, UNITS.len());
    assert!(report.sets_without_catalogue.is_empty());
    for (unit_name, _, _) in UNITS {
        assert!(catalog_path_of(&conn, unit_name).is_some(), "{unit_name}");
    }
}

/// A tape written from sets that had no catalogue (the copy this issue is
/// about, or one from before catalogues were kept) has none to give back,
/// and the report names each such set.
#[test]
fn a_tape_that_carries_no_catalogue_names_the_sets_that_have_none() {
    let mut vol = build_sealed_volume(true);
    let (_home, conn, report) = rebuild_into_a_home(&mut vol);

    assert_eq!(report.catalogues_restored, 0);
    assert_eq!(
        report.sets_without_catalogue,
        vec![
            "photos/2019 v7".to_string(),
            "photos/2020 v7".to_string(),
            "ledgers/fy24 v7".to_string()
        ]
    );
    assert_eq!(catalog_path_of(&conn, "photos/2019"), None);
}

/// Read `entry` off `store` and list its tar members, or `None` when `secret`
/// is not a recipient.
fn envelope_members(
    store: &mut MemStore,
    entry: &tapectl::volume::format::ParsedIndexEntry,
    secret: &str,
) -> Option<Vec<String>> {
    let mut bytes = Vec::new();
    store.read_file(entry.position as u32, &mut bytes).unwrap();
    bytes.truncate(entry.size_bytes.unwrap() as usize);
    let id: age::x25519::Identity = secret.parse().unwrap();
    let decryptor = age::Decryptor::new(&bytes[..]).unwrap();
    let reader = match decryptor.decrypt(std::iter::once(&id as &dyn age::Identity)) {
        Ok(r) => r,
        Err(age::DecryptError::NoMatchingKeys) => return None,
        Err(e) => panic!("decrypt: {e}"),
    };
    Some(
        tar::Archive::new(reader)
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect(),
    )
}

/// Write the stage sets `read-slices` staged in `conn` to a fresh volume
/// `label2` in a `MemStore`, through the production session chain, as
/// `volume write` would.
fn write_staged_sets_to_a_second_volume(
    conn: &rusqlite::Connection,
    keys: &FixtureKeys,
    label2: &str,
) -> MemStore {
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000, 'initialized')",
        rusqlite::params![label2],
    )
    .unwrap();
    let volume_id = conn.last_insert_rowid();

    // `find_staged_data`'s selection.
    let mut build_units = Vec::new();
    let mut stmt = conn
        .prepare(
            "SELECT ss.id, ss.snapshot_id, u.name, u.uuid, u.tenant_id,
                    ss.dar_version, ss.dar_command, ss.catalog_path, s.version
             FROM stage_sets ss
             JOIN snapshots s ON s.id = ss.snapshot_id
             JOIN units u ON u.id = s.unit_id
             WHERE ss.status = 'staged' ORDER BY u.name",
        )
        .unwrap();
    let rows: Vec<_> = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, i64>(8)?,
            ))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    for (
        stage_set_id,
        snapshot_id,
        unit_name,
        unit_uuid,
        tenant_id,
        dar_version,
        dar_command,
        catalog_path,
        version,
    ) in rows
    {
        let mut slice_stmt = conn
            .prepare(
                "SELECT id, slice_number, size_bytes, encrypted_bytes, sha256_plain,
                        sha256_encrypted, staging_path
                 FROM stage_slices WHERE stage_set_id = ?1 AND staging_path IS NOT NULL
                 ORDER BY slice_number",
            )
            .unwrap();
        let slices: Vec<BuildSlice> = slice_stmt
            .query_map(rusqlite::params![stage_set_id], |r| {
                Ok(BuildSlice {
                    slice_id: r.get(0)?,
                    slice_number: r.get(1)?,
                    size_bytes: r.get(2)?,
                    encrypted_bytes: r.get(3)?,
                    sha256_plain: r.get(4)?,
                    sha256_encrypted: r.get(5)?,
                    staging_path: std::path::PathBuf::from(r.get::<_, String>(6)?),
                })
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(
            !slices.is_empty(),
            "{unit_name}: read-slices staged nothing"
        );
        build_units.push(BuildUnit {
            stage_set_id,
            snapshot_id,
            unit_name,
            unit_uuid,
            tenant_id,
            dar_version,
            dar_command,
            catalog_path,
            snapshot_version: version,
            slices,
        });
    }

    let tenant_id = |name: &str| -> i64 {
        conn.query_row(
            "SELECT id FROM tenants WHERE name = ?1",
            rusqlite::params![name],
            |r| r.get(0),
        )
        .unwrap()
    };
    let tenants = [
        TenantInfo {
            tenant_id: tenant_id("alpha"),
            tenant_name: "alpha".to_string(),
            public_keys: vec![keys.alpha.public_key.clone()],
        },
        TenantInfo {
            tenant_id: tenant_id("bravo"),
            tenant_name: "bravo".to_string(),
            public_keys: vec![keys.bravo.public_key.clone()],
        },
    ];
    let inputs = BuildInputs {
        label: label2.to_string(),
        volume_uuid: "66666666-7777-8888-9999-000000000000".to_string(),
        media_type: "LTO-6".to_string(),
        tapectl_version: "0.1.0-test".to_string(),
        created_at: "2026-10-15T00:00:00Z".to_string(),
        block_size: BS,
        usable_bytes: 64 * 1024 * 1024,
        enospc_buffer: 1024 * 1024,
        nominal_capacity: 2_400_000_000,
        mam_capacity: 2_400_000_000,
        mam_manufacturer: "TAPECTL-TEST".to_string(),
        mam_serial: "SECONDSERIAL".to_string(),
        cartridge_identity_source: Some("mam".to_string()),
        mam_length: 0,
        mam_loads: 0,
        units: build_units.clone(),
        tenants: tenants.to_vec(),
        operator_public_keys: vec![keys.operator.public_key.clone()],
        escrow_public_key: Some(keys.escrow.public_key.clone()),
        catalog_db_path: None,
        index: None,
    };
    let session_dir = tempfile::tempdir().unwrap();
    let built = build::build(&inputs, session_dir.path()).expect("build succeeds");
    let available = KeyAvailability {
        tenant_ids: tenants.iter().map(|t| t.tenant_id).collect(),
        tenants_with_active_key: tenants.iter().map(|t| t.tenant_id).collect(),
        operator_key_present: true,
        escrow_recipient_present: None,
        stage_sets_lacking_escrow: None,
    };
    let mut store = MemStore::new(BS as usize);
    let validated = built
        .into_validated(&available, SliceCheck::Size, &mut store)
        .expect("validate");
    let planned = validated.plan(conn, volume_id, &build_units).expect("plan");
    let ready = match planned.execute(conn, &mut store).expect("execute") {
        ExecuteOutcome::Ready(r) => r,
        _ => panic!("the second write must reach Ready"),
    };
    let sealed = ready.seal(&mut store).expect("seal");
    match sealed
        .confirm(conn, &mut store, Tier::Integrity)
        .expect("confirm")
    {
        ConfirmOutcome::Sealed(_) => {}
        _ => panic!("the second write must seal"),
    }
    store
}

/// The whole of issue #473: rebuild from a tape, `read-slices` it back, and
/// write a second volume. Its envelopes carry the dar catalogues — before the
/// fix they carried none, though that volume's RECOVERY.md says each does.
#[test]
fn a_copy_written_after_a_rebuild_and_read_slices_carries_the_dar_catalogues() {
    let keys = FixtureKeys::fresh();
    let mut vol = build_tape_with_catalogues(&keys);
    let (home, conn, report) = rebuild_into_a_home(&mut vol);
    assert_eq!(report.catalogues_restored, UNITS.len());

    // `volume read-slices`, per unit, from the same tape.
    let mut config = tapectl::config::Config::default();
    config.staging.directory = home.path().join("staging").to_string_lossy().into_owned();
    for (unit_name, _, _) in UNITS {
        tapectl::volume::write::read_slices(
            &conn,
            &config,
            LABEL,
            unit_name,
            None,
            &mut vol.store,
            ContactSite::new(
                cfg(),
                Operation::VolumeReadSlices,
                "memstore",
                Medium::NoBackend,
            ),
        )
        .unwrap_or_else(|e| panic!("read-slices {unit_name}: {e}"));
    }

    let mut second = write_staged_sets_to_a_second_volume(&conn, &keys, "REBUILD02");

    let front_index: Vec<_> = {
        let text = String::from_utf8_lossy(&second.files[3]).into_owned();
        tapectl::volume::format::parse_front_index(text.trim_end_matches('\0')).unwrap()
    };
    // The operator envelope carries every unit's catalogue, byte for byte.
    let operator_entry = front_index
        .iter()
        .find(|e| e.type_label == "operator_envelope")
        .unwrap()
        .clone();
    let members =
        envelope_members(&mut second, &operator_entry, &keys.operator.secret_key).unwrap();
    for (i, (unit_name, _, _)) in UNITS.iter().enumerate() {
        let name = format!("catalogs/{}_v7.1.dar", catalogue_uuid8(i));
        assert!(
            members.contains(&name),
            "{unit_name}: the second volume's operator envelope holds {name}: {members:?}"
        );
    }
    let scratch = tempfile::tempdir().unwrap();
    let identity: age::x25519::Identity = keys.operator.secret_key.parse().unwrap();
    let opened = tapectl::volume::envelope::open_envelope(
        &mut second,
        &operator_entry,
        &[identity],
        scratch.path(),
    )
    .map_err(|_| "open the operator envelope")
    .unwrap();
    assert_eq!(opened.catalogues.len(), UNITS.len());
    for c in &opened.catalogues {
        let i = (0..UNITS.len())
            .find(|i| c.file_name == format!("{}_v7.1.dar", catalogue_uuid8(*i)))
            .unwrap_or_else(|| panic!("unexpected catalogue {}", c.file_name));
        assert_eq!(std::fs::read(&c.path).unwrap(), catalogue_bytes(UNITS[i].0));
    }

    // Each tenant's envelope holds its own units' catalogues (alpha owns the
    // first two units, bravo the third) and only those.
    let mut seen = 0;
    for entry in front_index
        .iter()
        .filter(|e| e.type_label == "tenant_envelope")
    {
        if let Some(members) = envelope_members(&mut second, entry, &keys.alpha.secret_key) {
            seen += 1;
            let catalogues: Vec<&String> = members
                .iter()
                .filter(|m| m.starts_with("catalogs/"))
                .collect();
            assert_eq!(
                catalogues,
                vec![
                    &format!("catalogs/{}_v7.1.dar", catalogue_uuid8(0)),
                    &format!("catalogs/{}_v7.1.dar", catalogue_uuid8(1)),
                ],
                "alpha's tenant envelope: {members:?}"
            );
        }
    }
    assert_eq!(seen, 1, "exactly one tenant envelope is alpha's");
}

// ── the cross-volume index (§1.5, #418 R4) ──

/// Row counts of every table a rebuild writes, for comparing two catalogs.
fn rebuild_table_counts(conn: &rusqlite::Connection) -> Vec<(&'static str, i64)> {
    [
        "tenants",
        "units",
        "snapshots",
        "stage_sets",
        "stage_slices",
        "writes",
        "write_positions",
        "volumes",
        "cartridges",
        "cartridge_volumes",
        "paths",
        "file_versions",
    ]
    .into_iter()
    .map(|t| {
        (
            t,
            conn.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
                .unwrap(),
        )
    })
    .collect()
}

/// `catalog rebuild` reads the operator envelope's `units.tsv` and lists the
/// volumes it names that the catalog lacks, with what the index says of each;
/// and creates nothing from it. The control is the same tape without an index,
/// rebuilt into its own fresh catalog: every table the rebuild writes ends with
/// the same number of rows, so the index added no row of any kind.
#[test]
fn the_rebuild_lists_the_tapes_the_index_names_and_creates_no_row_for_them() {
    let mut with = build_sealed_volume_with_index(IndexKind::History);
    let mut without = build_sealed_volume_with_index(IndexKind::None);
    let secret = with.operator_secret.clone();
    let scratch = tempfile::tempdir().unwrap();

    let with_dir = tempfile::tempdir().unwrap();
    let with_conn = fresh_db(with_dir.path());
    let report = rebuild(&with_conn, &mut with, &secret, scratch.path()).unwrap();
    assert!(
        report.had_index && report.index_problem.is_none(),
        "{report:?}"
    );

    let missing: Vec<(&str, &str, &str, &str)> = report
        .index_volumes_missing
        .iter()
        .map(|v| {
            (
                v.label.as_str(),
                v.uuid.as_str(),
                v.state.as_str(),
                v.generation.as_str(),
            )
        })
        .collect();
    assert_eq!(
        missing,
        vec![
            ("HIST-A", "hist-uuid-HIST-A", "sealed", "LTO-6"),
            ("HIST-Q", "hist-uuid-HIST-Q", "quarantined", "LTO-6"),
        ],
        "the two other tapes, and not this one"
    );
    let a = &report.index_volumes_missing[0];
    assert_eq!(a.sealed_at, "2026-09-01T12:00:00Z");
    assert_eq!(
        a.cartridge_serial, "",
        "the history tapes have no cartridge bound"
    );
    let held: Vec<(&str, &str, &[i64])> = a
        .units
        .iter()
        .map(|u| (u.tenant.as_str(), u.unit.as_str(), u.versions.as_slice()))
        .collect();
    assert_eq!(
        held,
        vec![
            ("alpha", "photos/2019", &[8][..]),
            ("bravo", "ledgers/fy24", &[8][..])
        ]
    );
    let q = &report.index_volumes_missing[1];
    assert_eq!(q.units.len(), 1);
    assert_eq!(q.units[0].versions, vec![9]);

    // Nothing was created from the index: only this tape's volume is in the
    // catalog, no version 8 or 9, and every table the rebuild writes is the
    // size it is when the same tape carries no index.
    let labels: Vec<String> = with_conn
        .prepare("SELECT label FROM volumes ORDER BY label")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(labels, vec![LABEL.to_string()]);
    let later: i64 = with_conn
        .query_row(
            "SELECT COUNT(*) FROM snapshots WHERE version > 7",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(later, 0);

    let without_dir = tempfile::tempdir().unwrap();
    let without_conn = fresh_db(without_dir.path());
    let secret = without.operator_secret.clone();
    let control = rebuild(&without_conn, &mut without, &secret, scratch.path()).unwrap();
    assert!(!control.had_index && control.index_volumes_missing.is_empty());
    assert_eq!(
        rebuild_table_counts(&with_conn),
        rebuild_table_counts(&without_conn),
        "the index added rows to the catalog"
    );
    assert!(
        rebuild_table_counts(&with_conn).contains(&("units", 3)),
        "the control compares populated catalogs"
    );
}

/// The to-do list is "volumes the catalog lacks": one it has is not on it. A
/// volume is the catalog's by uuid, or by label when the catalog's row never
/// recorded a uuid; a label held by a different uuid is another volume.
#[test]
fn a_volume_the_catalog_has_is_not_on_the_list() {
    let mut vol = build_sealed_volume_with_index(IndexKind::History);
    let secret = vol.operator_secret.clone();
    let scratch = tempfile::tempdir().unwrap();
    // One tape, read again into a fresh catalog each time (a `MemStore` reads
    // the same bytes as often as it is asked).
    let mut missing_after = |setup: &dyn Fn(&rusqlite::Connection)| -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let conn = fresh_db(dir.path());
        setup(&conn);
        let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();
        report
            .index_volumes_missing
            .into_iter()
            .map(|v| v.label)
            .collect()
    };
    let insert = |conn: &rusqlite::Connection, label: &str, uuid: Option<&str>| {
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes,
                                  status, uuid)
             VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000, 'sealed', ?2)",
            rusqlite::params![label, uuid],
        )
        .unwrap();
    };
    // Positive control: an empty catalog lacks both.
    assert_eq!(missing_after(&|_| {}), ["HIST-A", "HIST-Q"]);
    // Known by uuid.
    assert_eq!(
        missing_after(&|c| insert(c, "HIST-A", Some("hist-uuid-HIST-A"))),
        ["HIST-Q"]
    );
    // Known by label, the catalog's row having never recorded a uuid.
    assert_eq!(missing_after(&|c| insert(c, "HIST-Q", None)), ["HIST-A"]);
    // Known by uuid under another label (a relabelled tape): still known.
    assert_eq!(
        missing_after(&|c| insert(c, "RELABELLED", Some("hist-uuid-HIST-A"))),
        ["HIST-Q"]
    );
    // The label held by a DIFFERENT uuid is a different volume: still listed.
    assert_eq!(
        missing_after(&|c| insert(c, "HIST-Q", Some("another-uuid"))),
        ["HIST-A", "HIST-Q"]
    );
}

/// A tape written before 1.3.0 has no index: read exactly as before, with
/// nothing to say about it.
#[test]
fn a_tape_without_an_index_is_read_as_before() {
    let mut vol = build_sealed_volume_with_index(IndexKind::None);
    let secret = vol.operator_secret.clone();
    let scratch = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();
    assert!(!report.had_index);
    assert!(report.index_problem.is_none());
    assert!(report.index_volumes_missing.is_empty());
    assert_eq!(report.units, 3, "{report:?}");
}

/// A damaged aid does not stop the recovery it exists to speed: the rebuild
/// completes, and says the index could not be used, and why.
#[test]
fn an_unreadable_index_is_reported_and_the_rebuild_goes_on() {
    let mut vol = build_sealed_volume_with_index(IndexKind::Unreadable);
    let secret = vol.operator_secret.clone();
    let scratch = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let report = rebuild(&conn, &mut vol, &secret, scratch.path()).unwrap();
    assert!(report.had_index);
    let why = report
        .index_problem
        .as_deref()
        .expect("the problem is named");
    assert!(why.contains("no `volume` column"), "{why}");
    assert!(report.index_volumes_missing.is_empty());
    assert_eq!(
        report.units, 3,
        "the rebuild itself is complete: {report:?}"
    );
}

/// Rebuilding the tapes the index named, one after another, empties the list:
/// the to-do list a disaster recovery works through.
#[test]
fn rebuilding_a_listed_tape_takes_it_off_the_list() {
    let mut newest = build_sealed_volume_with_index(IndexKind::History);
    let secret = newest.operator_secret.clone();
    let scratch = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh_db(dir.path());
    let first = rebuild(&conn, &mut newest, &secret, scratch.path()).unwrap();
    assert_eq!(first.index_volumes_missing.len(), 2);

    // The same tape again: the catalog now has it, and the list is unchanged
    // (the history tapes are still not in the catalog).
    let again = rebuild(&conn, &mut newest, &secret, scratch.path()).unwrap();
    assert!(again.is_noop(), "{again:?}");
    assert_eq!(again.index_volumes_missing.len(), 2);

    // Bring one of the listed tapes into the catalog (its row, as a rebuild of
    // that tape would insert it) and it leaves the list.
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes,
                              status, uuid)
         VALUES ('HIST-A', 'lto', 'lto0', 'LTO-6', 2500000000, 'sealed', 'hist-uuid-HIST-A')",
        [],
    )
    .unwrap();
    let after = rebuild(&conn, &mut newest, &secret, scratch.path()).unwrap();
    let labels: Vec<&str> = after
        .index_volumes_missing
        .iter()
        .map(|v| v.label.as_str())
        .collect();
    assert_eq!(labels, ["HIST-Q"]);
}
