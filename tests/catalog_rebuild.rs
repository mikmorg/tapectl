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

use tapectl::crypto::keys::generate_keypair;
use tapectl::db;
use tapectl::staging;
use tapectl::store::{MemStore, Tier};
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
        usable_capacity_factor: 0.95,
        enospc_buffer: "1GiB".to_string(),
    })
}
use tapectl::volume::build::{self, BuildInputs, BuildSlice, BuildUnit, TenantInfo};
use tapectl::volume::layout_model::KeyAvailability;
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
    /// The pre-2026-09-11 shape: no `tenants`, no `key_fingerprints`, no
    /// `sha256_plain`. Derived from the REAL generator's output by dropping
    /// those, not hand-rolled, so it cannot drift from what old tapes hold.
    Old,
    /// What `build_catalog_snapshot` writes today.
    New,
}

struct SealedVolume {
    store: MemStore,
    operator_secret: String,
    escrow_secret: String,
    escrow_public: String,
    tenant_secret: String,
    /// Unit name -> the tape positions its slices landed on, in slice order.
    expected_positions: Vec<(String, Vec<i64>)>,
    /// Unit name -> its slice's `sha256_plain`, as staged.
    expected_plain_hashes: Vec<(String, String)>,
}

fn insert_tenant(conn: &rusqlite::Connection, name: &str, is_operator: bool) -> TenantFixture {
    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES (?1, ?2, 'active')",
        rusqlite::params![name, is_operator as i64],
    )
    .unwrap();
    let tenant_id = conn.last_insert_rowid();
    let kp = generate_keypair();
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
    let db_dir = tempfile::tempdir().unwrap();
    let conn = db::open(&db_dir.path().join("src.db")).unwrap();

    let operator = insert_tenant(&conn, "operator", true);
    let alpha = insert_tenant(&conn, "alpha", false);
    let bravo = insert_tenant(&conn, "bravo", false);

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000, 'active')",
        rusqlite::params![LABEL],
    )
    .unwrap();
    let volume_id = conn.last_insert_rowid();

    // ADR-0005's permanent escrow recipient — a recipient of every SLICE
    // and every envelope, so attestation (trial-decrypting a slice header
    // with the escrow key) has something true to find.
    let escrow = generate_keypair();

    let slices_dir = tempfile::tempdir().unwrap();
    let mut build_units = Vec::new();
    let mut stage_set_ids = Vec::new();
    let mut expected_plain_hashes = Vec::new();

    for (i, (unit_name, tenant_name, content)) in UNITS.iter().enumerate() {
        let tenant = if *tenant_name == "alpha" {
            &alpha
        } else {
            &bravo
        };
        let unit_uuid = format!("unit-uuid-{i:04}");
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES (?1, ?2, ?3, ?4, 'active')",
            rusqlite::params![unit_uuid, unit_name, tenant.tenant_id, "/src/data"],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();

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
            conn.execute(
                "INSERT INTO files (snapshot_id, path, size_bytes, sha256, is_directory)
                 VALUES (?1, ?2, ?3, ?4, 0)",
                rusqlite::params![
                    snapshot_id,
                    format!("{unit_name}/file{n}.bin"),
                    100 + n,
                    sha256_hex(format!("{unit_name}-{n}").as_bytes()),
                ],
            )
            .unwrap();
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
        for slice_number in 1..=2i64 {
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

        build_units.push(BuildUnit {
            stage_set_id,
            snapshot_id,
            unit_name: unit_name.to_string(),
            unit_uuid,
            tenant_id: tenant.tenant_id,
            dar_version: Some("2.7.20".to_string()),
            dar_command: None,
            catalog_path: None,
            snapshot_version: 7,
            slices,
        });
    }

    let catalog_dir = tempfile::tempdir().unwrap();
    let catalog_db_path = match catalog_db {
        CatalogDb::None => None,
        CatalogDb::New | CatalogDb::Old => {
            let p = catalog_dir.path().join("catalog.db");
            db::catalog_snapshot::build_catalog_snapshot(&conn, &stage_set_ids, &p).unwrap();
            if catalog_db == CatalogDb::Old {
                let c = rusqlite::Connection::open(&p).unwrap();
                c.execute_batch(
                    "DROP TABLE tenants;
                     ALTER TABLE stage_sets DROP COLUMN key_fingerprints;
                     ALTER TABLE stage_slices DROP COLUMN sha256_plain;",
                )
                .unwrap();
            }
            Some(p)
        }
    };

    let tenants = [alpha, bravo];
    let inputs = BuildInputs {
        label: LABEL.to_string(),
        volume_uuid: VOL_UUID.to_string(),
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
    let validated = built.into_validated(&keys, &mut store).expect("validate");
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
        store,
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
        Some(LABEL),
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
            "SELECT COUNT(*) FROM files f JOIN snapshots s ON s.id = f.snapshot_id
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
        "files",
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
/// current volume, bulk-erase the physical tape, then run `tapectl cartridge
/// mark-erased`" — must reach a rebuilt tape rather than dead-end on a
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
