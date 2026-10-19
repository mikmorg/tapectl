//! T7 — the synthetic-heir harness (the v2 format's acceptance suite).
//!
//! Builds a microcosm batch (`tests/common/mod.rs`, T3), runs the **full**
//! `docs/design/v2-open-questions.md` §9 write session against `MemStore`,
//! then — using ONLY the bytes recorded in `MemStore.files`, never the
//! in-memory `Layout` — verifies the tape the way an heir with no database
//! and no tapectl would: parse File 3, walk the hash chain to the seal
//! marker, and reconstruct one tenant's data by trial-decryption alone.
//!
//! The ONE fact this suite is allowed to take from the in-memory `Layout`
//! rather than from recorded bytes is File 3's own true (pre-padding) length
//! (`front_index_position_and_true_len`, below) — the tape deliberately
//! omits it from File 3's own `[[files]]` entry (`volume-format-v2.md` §3:
//! "File 3's length is self-referential"), so an heir recovers it by
//! stripping trailing NUL padding instead (`tr -d '\0'`); the Rust side of
//! the same §4 byte contract uses the Layout's recorded value. Every other
//! fact used below — every other file's position/type/size/hash, the seal's
//! binding, `file_count` — comes only from parsing recorded bytes.
//!
//! If this suite is green and honest, the on-tape byte layout is
//! heir-readable with no catalog and no tapectl.
mod common;

use std::io::Read;
use std::sync::OnceLock;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use common::{generate_collection, MicroSpec, UnitFixture, MICRO_BLOCK};
use tapectl::crypto::keys::{generate_keypair, GeneratedKeypair};
use tapectl::db;
use tapectl::policy::coverage::IndexThisTape;
use tapectl::staging;
use tapectl::store::{MemStore, MismatchKind, Store, Tier};
use tapectl::volume::build::{self, BuildInputs, BuildSlice, BuildUnit, BuiltLayout, TenantInfo};
use tapectl::volume::format;
use tapectl::volume::index;
use tapectl::volume::layout_model::{KeyAvailability, Layout, SliceCheck, ZoneKind};
use tapectl::volume::session::{ConfirmOutcome, ExecuteOutcome};

/// Format constant (never scaled, per `v2-open-questions.md` §8) — reused
/// from the T3 fixture module rather than redeclared.
const BS: u64 = MICRO_BLOCK;

/// Microcosm scale (§8: "small N (6-10 units)").
const N_UNITS_MAIN: usize = 6;
const SEED_MAIN: u64 = 424_242;
const VOLUME_UUID_MAIN: &str = "a1b2c3d4-e5f6-4789-a123-456789abcdef";

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// One content tenant's identity, kept around so the keyed heir-restore leg
/// (assertion 7) can decrypt — mirrors what a real heir would hold (their own
/// age secret key), nothing else.
struct TenantFixture {
    tenant_id: i64,
    name: String,
    public_key: String,
    /// What the catalog records as the key's fingerprint. Today that is the
    /// `age1…` public key itself; kept as its own field so a scan for it
    /// stays a scan for the fingerprint if that ever changes.
    fingerprint: String,
    secret_key: String,
}

/// One unit's ground truth, kept so the final assertions can check restored
/// bytes against what the fixture generator actually produced.
struct UnitPlain {
    unit_name: String,
    unit_uuid: String,
    tenant_id: i64,
    /// The unit's files as the catalog's `paths` record them.
    file_names: Vec<String>,
    /// The sha256 of each of `file_names`' bytes, in the same order: what the
    /// catalog recorded for them and the index carries.
    file_hashes: Vec<String>,
    plaintext: Vec<u8>,
    /// The `sha256_plain` the catalog recorded for the unit's one slice: the
    /// real hash of `plaintext` when the build asked for it
    /// ([`HarnessSpec::real_plaintext_hash`]), else the harness placeholder.
    sha256_plain: String,
}

/// What a sealed-harness build is made of beyond its fixture units: the names
/// the tenants are known by, and whether the plaintext hash and the escrow
/// recipient are real. The default is the long-standing shared harness
/// (`operator`, `alpha`, `bravo`, a placeholder `sha256_plain`, no escrow);
/// the plaintext-zone scan (issue #450) asks for unique names, real hashes
/// and an escrow recipient, so every needle it searches for is a string only
/// the fixture could have put on the tape.
struct HarnessSpec<'a> {
    operator: &'a str,
    /// The two content tenants; the fixture's first half of units goes to
    /// the first, the rest to the second.
    tenants: [&'a str; 2],
    /// Hash each unit's real plaintext into `stage_slices.sha256_plain`
    /// instead of the placeholder every other test shares (a second full
    /// hash pass per unit, wanted only where the hash is a needle).
    real_plaintext_hash: bool,
    /// Register an escrow recipient: slices and envelopes are then also
    /// encrypted to it (`volume-format-v2.md` §1: enc(t+op+esc)).
    escrow: bool,
    /// The labels of two OTHER sealed volumes, each holding a later version
    /// of one tenant's unit (the first volume the first tenant's first unit,
    /// the second the second tenant's last): what the cross-volume index
    /// (§1.5) lists beyond this tape, and what must not cross tenants.
    history: [&'a str; 2],
}

impl Default for HarnessSpec<'_> {
    fn default() -> Self {
        HarnessSpec {
            operator: "operator",
            tenants: ["alpha", "bravo"],
            real_plaintext_hash: false,
            escrow: false,
            history: ["T7HIST-ALPHA", "T7HIST-BRAVO"],
        }
    }
}

/// Everything downstream assertions need from a fully sealed session.
struct Harness {
    /// The operator's identity, and the escrow recipient when the build
    /// registered one: with `tenants`, every key whose fingerprint or `age1…`
    /// string must stay off the plaintext zones.
    operator: TenantFixture,
    escrow: Option<GeneratedKeypair>,
    /// Snapshot of the sealed `BuiltLayout`'s `Layout`, taken BEFORE it was
    /// consumed by the typestate chain (`into_validated` -> ... -> `confirm`
    /// does not hand it back). Its only legitimate use downstream is File 3's
    /// own true length (see the module doc comment) plus calling the real
    /// `Store::confirm` (assertion 6's second half, which needs a `&Layout`
    /// by its own production signature).
    layout: Layout,
    store: MemStore,
    tenants: Vec<TenantFixture>,
    units: Vec<UnitPlain>,
    /// The two history volumes of [`HarnessSpec::history`], as `(label,
    /// tenant_id, the unit it holds a later version of)`.
    history: Vec<(String, i64, String)>,
}

/// Concatenate a fixture unit's files (in the generator's own order) into one
/// buffer — this is the "unit content" this harness age-encrypts directly as
/// a single staged slice, standing in for the real dar+stage pipeline
/// (explicitly out of scope for T7: no dar, no real staging).
fn concat_unit_bytes(fixture: &UnitFixture) -> Vec<u8> {
    let mut buf = Vec::with_capacity(fixture.total_bytes as usize);
    for (name, _size) in &fixture.files {
        buf.extend(std::fs::read(fixture.path.join(name)).expect("read fixture file"));
    }
    buf
}

/// Insert a tenant + one active primary key, returning the identity data the
/// harness needs. Alias must be distinct per tenant
/// (`encryption_keys.alias` is a bare `UNIQUE` column, not scoped by
/// tenant_id) — `{name}-primary` rather than a shared literal like
/// `"primary"`.
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
        fingerprint: kp.fingerprint,
        secret_key: kp.secret_key,
    }
}

/// `files` (and one `extra`) as the paths of `snapshot_id`, regular files.
/// Each of `files` records the sha256 of its bytes in `dir` (what staging
/// records: the index carries it, ADR-0012 item 103); the `extra` records
/// none.
fn insert_paths(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
    dir: &std::path::Path,
    files: &[(String, u64)],
    extra: Option<(&str, u64)>,
) {
    let entries = files
        .iter()
        .map(|(n, size)| {
            let hash = sha256_hex(&std::fs::read(dir.join(n)).unwrap());
            (
                n.clone(),
                *size,
                Some(db::files::sha256_from_hex(&hash).unwrap()),
            )
        })
        .chain(extra.map(|(n, size)| (n.to_string(), size, None)));
    db::files::insert_version(
        conn,
        snapshot_id,
        entries.map(|(path, size, sha256)| {
            Ok(db::files::FileEntry {
                path,
                kind: db::files::FileKind::Regular,
                size_bytes: size as i64,
                mtime_ns: None,
                sha256,
                link_target: None,
            })
        }),
    )
    .unwrap();
}

/// Nothing in `dir` (recursively) is, or holds, the index's plaintext: no
/// `units.tsv`/`paths.tsv` by name, and none of their header lines in any
/// file's bytes. The routine the positive control below runs too.
fn index_plaintext_in(dir: &std::path::Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let bytes = std::fs::read(&path).unwrap();
            let has = |needle: &str| bytes.windows(needle.len()).any(|w| w == needle.as_bytes());
            if name.ends_with("units.tsv")
                || name.ends_with("paths.tsv")
                || has(&index::UNITS_COLUMNS.join("\t"))
                || has(&index::PATHS_COLUMNS.join("\t"))
            {
                found.push(path.display().to_string());
            }
        }
    }
    found
}

fn assert_no_index_plaintext(dir: &std::path::Path) {
    // Positive control: the same scan finds a planted index file.
    let control = tempfile::tempdir().unwrap();
    std::fs::write(
        control.path().join("anything"),
        format!("{}\n", index::PATHS_COLUMNS.join("\t")),
    )
    .unwrap();
    assert_eq!(
        index_plaintext_in(control.path()).len(),
        1,
        "the scan sees a header"
    );
    let found = index_plaintext_in(dir);
    assert!(
        found.is_empty(),
        "index plaintext outside an envelope: {found:?}"
    );
}

/// Build a microcosm batch of `n_units` split across 2 tenants, run the
/// FULL v2 write session (build -> validate -> plan -> execute -> seal ->
/// confirm at `Tier::Integrity`) against a `MemStore`, and return everything
/// a downstream assertion needs. Panics (via `expect`) unless the session
/// reaches `ConfirmOutcome::Sealed` — a harness that doesn't seal can't
/// stand in for "a sealed volume" for any of the assertions built on it.
///
/// Per the T7 brief's "cheap way": no dar, no real staging pipeline. Each
/// unit becomes exactly ONE staged slice by age-encrypting its concatenated
/// file bytes directly via `staging::encrypt_data`.
fn build_sealed_harness(seed: u64, n_units: usize, volume_uuid: &str) -> Harness {
    let root = tempfile::tempdir().unwrap();
    let fixtures = generate_collection(root.path(), &MicroSpec { n_units, seed });
    build_sealed_harness_from(&fixtures, &HarnessSpec::default(), volume_uuid)
}

/// [`build_sealed_harness`] over fixture units the caller made, named and
/// keyed as `spec` says. The fixtures' files must still exist: their bytes
/// are read here.
fn build_sealed_harness_from(
    fixtures: &[UnitFixture],
    spec: &HarnessSpec<'_>,
    volume_uuid: &str,
) -> Harness {
    // `db::open_memory` is `#[cfg(test)]`-gated (unit-test-only), so it does
    // not exist in the normal rlib an integration test links against. A real
    // temp-file DB via the unconditionally-`pub` `db::open` is functionally
    // identical (same `configure`/`migrate`/`recover_orphaned_sessions`
    // path) and needs no `src/` change.
    let db_dir = tempfile::tempdir().unwrap();
    let conn = db::open(&db_dir.path().join("t7.db")).unwrap();
    let operator = insert_tenant(&conn, spec.operator, true);
    let tenant_a = insert_tenant(&conn, spec.tenants[0], false);
    let tenant_b = insert_tenant(&conn, spec.tenants[1], false);
    let tenants = vec![tenant_a, tenant_b];
    let escrow = spec.escrow.then(generate_keypair);
    let escrow_public_key = escrow.as_ref().map(|k| k.public_key.clone());

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES ('T7MICRO', 'lto', 'lto0', 'LTO-6', 2500000000, 'initialized')",
        [],
    )
    .unwrap();
    let volume_id = conn.last_insert_rowid();

    let slices_dir = tempfile::tempdir().unwrap();
    let half = fixtures.len() / 2;
    let mut build_units = Vec::with_capacity(fixtures.len());
    let mut units_plain = Vec::with_capacity(fixtures.len());
    let mut unit_rows: Vec<(i64, std::path::PathBuf, Vec<(String, u64)>)> =
        Vec::with_capacity(fixtures.len());

    for (i, fixture) in fixtures.iter().enumerate() {
        let tenant = if i < half { &tenants[0] } else { &tenants[1] };
        let plaintext = concat_unit_bytes(fixture);
        let unit_uuid = format!("unit-uuid-{i:04}");

        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES (?1, ?2, ?3, ?4, 'active')",
            rusqlite::params![
                unit_uuid,
                fixture.folder_name,
                tenant.tenant_id,
                fixture.path.to_string_lossy()
            ],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'staged', ?2, ?3, ?4)",
            rusqlite::params![
                unit_id,
                fixture.path.to_string_lossy(),
                fixture.files.len() as i64,
                plaintext.len() as i64
            ],
        )
        .unwrap();
        let snapshot_id = conn.last_insert_rowid();
        insert_paths(&conn, snapshot_id, &fixture.path, &fixture.files, None);
        unit_rows.push((unit_id, fixture.path.clone(), fixture.files.clone()));
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 10485760)",
            rusqlite::params![snapshot_id],
        )
        .unwrap();
        let stage_set_id = conn.last_insert_rowid();

        // Skip dar + the real staging pipeline (T7 brief): age-encrypt the
        // unit's concatenated bytes directly as its one slice, to tenant +
        // operator (`volume-format-v2.md` §1: data slices are enc(t+op+esc);
        // escrow is skipped here, same as session.rs's own fixture, since
        // `escrow_recipient_present: None` below opts the check out).
        let mut recipients = vec![tenant.public_key.clone(), operator.public_key.clone()];
        recipients.extend(escrow_public_key.clone());
        let encrypted = staging::encrypt_data(&plaintext, &recipients).unwrap();
        // sha256_plain is never read back by any of this suite's assertions
        // or by the validate/execute/confirm code paths under test (only
        // `sha256_encrypted` is — `volume-format-v2.md` §3's front index
        // carries ciphertext hashes only); a placeholder skips a second full
        // ~N MiB debug-mode hash pass per unit purely for test wall-clock,
        // matching `build.rs`'s own test fixture's identical shortcut
        // (`sha_hex(b"plaintext hash is not exercised by this fixture")`).
        let sha_plain = if spec.real_plaintext_hash {
            sha256_hex(&plaintext)
        } else {
            sha256_hex(b"plaintext hash is not exercised by this harness")
        };
        let sha_enc = sha256_hex(&encrypted);

        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                        sha256_plain, sha256_encrypted)
             VALUES (?1, 1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                stage_set_id,
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

        build_units.push(BuildUnit {
            stage_set_id,
            snapshot_id,
            unit_name: fixture.folder_name.clone(),
            unit_uuid: unit_uuid.clone(),
            tenant_id: tenant.tenant_id,
            dar_version: None,
            dar_command: None,
            catalog_path: None,
            snapshot_version: 1,
            slices: vec![BuildSlice {
                slice_id,
                slice_number: 1,
                size_bytes: plaintext.len() as i64,
                encrypted_bytes: encrypted.len() as i64,
                sha256_plain: sha_plain.clone(),
                sha256_encrypted: sha_enc,
                staging_path: slice_path,
            }],
        });
        units_plain.push(UnitPlain {
            unit_name: fixture.folder_name.clone(),
            unit_uuid,
            tenant_id: tenant.tenant_id,
            file_names: fixture.files.iter().map(|(n, _)| n.clone()).collect(),
            file_hashes: fixture
                .files
                .iter()
                .map(|(n, _)| sha256_hex(&std::fs::read(fixture.path.join(n)).unwrap()))
                .collect(),
            plaintext,
            sha256_plain: sha_plain,
        });
    }

    // The archive's other tapes (§1.5): two sealed volumes, each holding a
    // later, current version of one tenant's unit: the first tenant's first
    // unit, the second tenant's last. The index lists them next to this tape,
    // and each belongs to one tenant alone.
    let mut history = Vec::new();
    for (label, which) in spec.history.iter().zip([0, fixtures.len() - 1]) {
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes,
                                  status, uuid, sealed_at)
             VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000, 'sealed', ?2, '2026-09-01 12:00:00')",
            rusqlite::params![label, format!("hist-uuid-{label}")],
        )
        .unwrap();
        let hist_volume = conn.last_insert_rowid();
        let (unit_id, fixture_dir, files) = &unit_rows[which];
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 2, 'current', '/hist', ?2, 1)",
            rusqlite::params![unit_id, files.len() as i64 + 1],
        )
        .unwrap();
        let hist_snapshot = conn.last_insert_rowid();
        insert_paths(
            &conn,
            hist_snapshot,
            fixture_dir,
            files,
            Some(("history-only.dat", 4242)),
        );
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'cleaned', 1)",
            rusqlite::params![hist_snapshot],
        )
        .unwrap();
        let hist_set = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                       sha256_plain, sha256_encrypted)
             VALUES (?1, 1, 7, 7, 'p', 'e')",
            rusqlite::params![hist_set],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'completed')",
            rusqlite::params![hist_set, hist_snapshot, hist_volume],
        )
        .unwrap();
        history.push((
            label.to_string(),
            units_plain[which].tenant_id,
            units_plain[which].unit_name.clone(),
        ));
    }

    // The cross-volume index, generated where `volume write` generates it: in
    // a work directory of its own, never in the session directory `build()`
    // fills (which stands in for staging here).
    let stage_set_ids: Vec<i64> = build_units.iter().map(|u| u.stage_set_id).collect();
    let index_work = tempfile::tempdir().unwrap();
    let index_files = index::generate_files(
        &conn,
        &IndexThisTape {
            label: "T7MICRO",
            uuid: volume_uuid,
            cartridge_serial: "T7SERIAL",
            generation: "LTO-6",
        },
        &stage_set_ids,
        &index_work.path().join("index"),
    )
    .unwrap();

    let inputs = BuildInputs {
        label: "T7MICRO".to_string(),
        volume_uuid: volume_uuid.to_string(),
        media_type: "LTO-6".to_string(),
        tapectl_version: "0.1.0-test".to_string(),
        created_at: "2026-07-22T20:09:00Z".to_string(),
        block_size: BS,
        // Comfortably above the worst case (n_units * <15M) at microcosm
        // scale; not the property under test here (T5b/T6 own capacity math).
        usable_bytes: 256 * 1024 * 1024,
        enospc_buffer: 8 * 1024 * 1024, // matches common::MICRO_ENOSPC
        nominal_capacity: 2_400_000_000, // matches common::MICRO_TAPE_NOMINAL
        mam_capacity: 2_400_000_000,
        mam_manufacturer: "TAPECTL-TEST".to_string(),
        mam_serial: "T7SERIAL".to_string(),
        // A chip-reported serial (issue #192): the sealed harness stands in
        // for a volume bound from MAM, so File 0 must attest "mam" and an
        // heir reading the recorded bytes alone must be able to see that.
        cartridge_identity_source: Some("mam".to_string()),
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
        escrow_public_key: escrow_public_key.clone(),
        catalog_db_path: None,
        index: Some(index_files),
    };

    let session_dir = tempfile::tempdir().unwrap();
    let built = build::build(&inputs, session_dir.path()).expect("build succeeds");
    // The index is plaintext until it is in an envelope: none of it may be in
    // the directory `build()` fills (staging's stand-in), under any name.
    assert_no_index_plaintext(session_dir.path());
    // Snapshot BEFORE the typestate chain below consumes `built` — see the
    // `Harness::layout` doc comment; `confirm`'s success arm does not hand
    // the Layout back.
    let layout_snapshot = built.layout.clone();

    let keys = KeyAvailability {
        tenant_ids: tenants.iter().map(|t| t.tenant_id).collect(),
        tenants_with_active_key: tenants.iter().map(|t| t.tenant_id).collect(),
        operator_key_present: true,
        escrow_recipient_present: escrow_public_key.as_ref().map(|_| true),
        stage_sets_lacking_escrow: None,
    };

    let mut store = MemStore::new(BS as usize);
    let validated = built
        .into_validated(&keys, SliceCheck::Size, &mut store)
        .expect("validate should pass for a well-formed microcosm build");
    let planned = validated
        .plan(&conn, volume_id, &build_units)
        .expect("plan should insert writes/write_positions rows");
    let ready = match planned
        .execute(&conn, &mut store)
        .expect("execute should not error")
    {
        ExecuteOutcome::Ready(r) => r,
        ExecuteOutcome::Interrupted(_) => panic!("unexpected interruption building the harness"),
        ExecuteOutcome::Aborted(a) => panic!("unexpected abort building the harness: {}", a.reason),
    };
    let sealed_pending = ready.seal(&mut store).expect("seal should succeed");
    let outcome = sealed_pending
        .confirm(&conn, &mut store, Tier::Integrity)
        .expect("confirm should not error");
    match outcome {
        ConfirmOutcome::Sealed(_) => {}
        ConfirmOutcome::Quarantined(q) => {
            panic!("harness must end Sealed, got Quarantined: {:?}", q.reason)
        }
        ConfirmOutcome::Inconclusive(inc) => {
            panic!(
                "harness must end Sealed, got Inconclusive: {:?}",
                inc.evidence.mismatches
            )
        }
    }

    Harness {
        operator,
        escrow,
        layout: layout_snapshot,
        store,
        tenants,
        units: units_plain,
        history,
    }
}

/// One `build_sealed_harness` run, shared read-only across every test that
/// needs a full sealed tape (assertions localise on what each test asserts,
/// not on separately rebuilding an expensive fixture 5 times). Tests that
/// need to mutate the recorded bytes (e.g. dropping the seal marker) clone
/// `h.store.files` first — nothing ever mutates this shared value in place.
static HARNESS: OnceLock<Harness> = OnceLock::new();

fn shared_harness() -> &'static Harness {
    HARNESS.get_or_init(|| build_sealed_harness(SEED_MAIN, N_UNITS_MAIN, VOLUME_UUID_MAIN))
}

// ── build()-only helper for content-independent properties (assertions 4-5) ──

/// Build a tiny 2-tenant, 2-unit `BuiltLayout` with NO session run — for
/// properties that live entirely in `build()`: envelope permutation order
/// (§2.1) and frozen-zone byte-identity (§2.2). Deliberately uses tiny
/// literal slice content rather than the MB-scale T3 fixture generator:
/// neither property depends on slice content or size at all (permutation is
/// a function of `volume_uuid`/`tenant_id`; the frozen zones checked are
/// id_thunk/system_guide/restore_sh, none of which touch slice bytes), and
/// real age-encrypting fixture-scale content here would pay the same
/// debug-mode crypto cost as the main harness for zero additional coverage —
/// mirrors `build.rs`'s own `two_tenant_inputs` test fixture for the same
/// reason.
///
/// Tenant ids are hardcoded 1 and 2 (not DB-assigned — this helper touches no
/// DB) because the probe `volume_uuid`s the permutation test uses are
/// precomputed against exactly this pair, matching build.rs's own
/// `tenant_envelope_permutation_is_deterministic_and_uuid_sensitive` test.
fn build_layout_only(volume_uuid: &str, created_at: &str) -> BuiltLayout {
    let tenant_a_key = generate_keypair();
    let tenant_b_key = generate_keypair();
    let op_key = generate_keypair();

    let slices_dir = tempfile::tempdir().unwrap();
    let mut units = Vec::new();
    for (i, (tenant_id, pubkey, content)) in [
        (1i64, &tenant_a_key.public_key, &b"alpha unit content"[..]),
        (
            2i64,
            &tenant_b_key.public_key,
            &b"bravo unit content, a bit longer"[..],
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let encrypted = staging::encrypt_data(content, std::slice::from_ref(pubkey)).unwrap();
        let path = slices_dir.path().join(format!("slice_{i}.age"));
        std::fs::write(&path, &encrypted).unwrap();
        units.push(BuildUnit {
            stage_set_id: i as i64 + 1,
            snapshot_id: i as i64 + 1,
            unit_name: format!("unit-{i}"),
            unit_uuid: format!("perm-unit-uuid-{i}"),
            tenant_id,
            dar_version: None,
            dar_command: None,
            catalog_path: None,
            snapshot_version: 1,
            slices: vec![BuildSlice {
                slice_id: i as i64 + 1,
                slice_number: 1,
                size_bytes: content.len() as i64,
                encrypted_bytes: encrypted.len() as i64,
                sha256_plain: sha256_hex(content),
                sha256_encrypted: sha256_hex(&encrypted),
                staging_path: path,
            }],
        });
    }

    let inputs = BuildInputs {
        label: "PERMTEST".to_string(),
        volume_uuid: volume_uuid.to_string(),
        media_type: "LTO-6".to_string(),
        tapectl_version: "0.1.0-test".to_string(),
        created_at: created_at.to_string(),
        block_size: BS,
        usable_bytes: 100 * BS,
        enospc_buffer: BS,
        nominal_capacity: 2_400_000_000,
        mam_capacity: 0,
        mam_manufacturer: String::new(),
        mam_serial: String::new(),
        cartridge_identity_source: None,
        mam_length: 0,
        mam_loads: 0,
        units,
        tenants: vec![
            TenantInfo {
                tenant_id: 1,
                tenant_name: "alpha".to_string(),
                public_keys: vec![tenant_a_key.public_key],
            },
            TenantInfo {
                tenant_id: 2,
                tenant_name: "bravo".to_string(),
                public_keys: vec![tenant_b_key.public_key],
            },
        ],
        operator_public_keys: vec![op_key.public_key],
        escrow_public_key: None,
        catalog_db_path: None,
        index: None,
    };

    let session_dir = tempfile::tempdir().unwrap();
    build::build(&inputs, session_dir.path()).expect("build succeeds")
}

// ── the keyless reader (assertions 1 and 6) ──

/// The ONE fact this suite is allowed to take from the in-memory `Layout`
/// rather than from recorded bytes: File 3's own true (pre-padding) length
/// (`volume-format-v2.md` §3 — the tape omits it from File 3's own entry).
/// `fi_pos` itself (always 3) is a format invariant, not tape-specific data —
/// File 0 says so in plaintext ("the map is File 3") and every reader
/// (RESTORE.sh included) hardcodes it the same way; only `fi_true_len` is the
/// genuine exception.
fn front_index_position_and_true_len(layout: &Layout) -> (usize, usize) {
    let fi = layout
        .entries
        .iter()
        .find(|e| matches!(e.kind, ZoneKind::FrontIndex))
        .expect("layout must have a front_index entry");
    (
        fi.position as usize,
        fi.size_bytes.expect("front_index entry must be sized") as usize,
    )
}

/// Verdict of the from-scratch keyless chain walk below — never panics,
/// matching the §2.5 fail-safe reader precedence: an absent/unparseable seal
/// marker is the NORMAL signal for "not sealed" (a torn/interrupted tape),
/// not an error condition.
#[derive(Debug)]
enum ChainWalkVerdict {
    /// Seal marker present, parses, binds File 3, and every content file's
    /// on-tape bytes (truncated to the front index's claimed size) hash to
    /// what the front index claims.
    Sealed {
        parsed_front_index: Vec<format::ParsedIndexEntry>,
    },
    /// Seal marker absent, unreadable, unparseable, or the chain broke
    /// anywhere downstream of it.
    Unsealed { reason: String },
}

/// The keyless chain walk (`volume-format-v2.md` §5), reimplemented from
/// scratch against ONLY `files` — exactly as an heir with `dd`+`sha256sum`
/// and no tapectl would run it. Deliberately does NOT call
/// `store::chain_walk`/`Store::confirm` (that is exercised separately, as
/// the real production code — see assertion 6's second half): this is an
/// INDEPENDENT proof that the byte format is right, the same spirit as
/// RESTORE.sh's own bash reimplementation of this same algorithm
/// (`v2-open-questions.md` §10 — "deliberate duplication; heir independence
/// IS the property").
fn keyless_chain_walk(files: &[Vec<u8>], fi_pos: usize, fi_true_len: usize) -> ChainWalkVerdict {
    let Some(seal_bytes) = files.last() else {
        return ChainWalkVerdict::Unsealed {
            reason: "no files recorded at all".to_string(),
        };
    };
    let seal = match format::parse_seal_marker(&String::from_utf8_lossy(seal_bytes)) {
        Ok(s) => s,
        Err(e) => {
            return ChainWalkVerdict::Unsealed {
                reason: format!("seal marker unparseable: {e}"),
            }
        }
    };

    let Some(fi_padded) = files.get(fi_pos) else {
        return ChainWalkVerdict::Unsealed {
            reason: format!("no file recorded at front-index position {fi_pos}"),
        };
    };
    if fi_true_len > fi_padded.len() {
        return ChainWalkVerdict::Unsealed {
            reason: "front index shorter than its recorded true length".to_string(),
        };
    }
    let fi_true_bytes = &fi_padded[..fi_true_len];
    let fi_hash = sha256_hex(fi_true_bytes);
    if fi_hash != seal.front_index_sha256 {
        return ChainWalkVerdict::Unsealed {
            reason: format!(
                "front index hash {fi_hash} != seal binding {}",
                seal.front_index_sha256
            ),
        };
    }

    let parsed = match format::parse_front_index(&String::from_utf8_lossy(fi_true_bytes)) {
        Ok(p) => p,
        Err(e) => {
            return ChainWalkVerdict::Unsealed {
                reason: format!("front index unparseable: {e}"),
            }
        }
    };
    let violations = format::validate_consistency(&parsed);
    if !violations.is_empty() {
        return ChainWalkVerdict::Unsealed {
            reason: format!("front index inconsistent: {violations:?}"),
        };
    }

    for entry in &parsed {
        if entry.type_label == "front_index" || entry.type_label == "seal_marker" {
            continue;
        }
        let (Some(size), Some(want_hash)) = (entry.size_bytes, entry.sha256_encrypted.as_ref())
        else {
            return ChainWalkVerdict::Unsealed {
                reason: format!(
                    "position {} missing size/hash in front index",
                    entry.position
                ),
            };
        };
        let Some(padded) = files.get(entry.position as usize) else {
            return ChainWalkVerdict::Unsealed {
                reason: format!("no file recorded at position {}", entry.position),
            };
        };
        if size as usize > padded.len() {
            return ChainWalkVerdict::Unsealed {
                reason: format!("position {} shorter than recorded size", entry.position),
            };
        }
        let actual_hash = sha256_hex(&padded[..size as usize]);
        if &actual_hash != want_hash {
            return ChainWalkVerdict::Unsealed {
                reason: format!(
                    "content hash mismatch at position {}: expected {want_hash}, got {actual_hash}",
                    entry.position
                ),
            };
        }
    }

    ChainWalkVerdict::Sealed {
        parsed_front_index: parsed,
    }
}

/// Parse File 3 from recorded bytes alone (using only the one Layout fact
/// above). Does not verify the seal binding or any content hash — that is
/// `keyless_chain_walk`'s job; this is the minimal "what does the map say"
/// step assertions 2, 3, and 7 build on, so a hash-chain regression doesn't
/// also fail unrelated properties.
fn parse_front_index_from_recorded_bytes(
    files: &[Vec<u8>],
    fi_pos: usize,
    fi_true_len: usize,
) -> Vec<format::ParsedIndexEntry> {
    let padded = &files[fi_pos];
    assert!(
        fi_true_len <= padded.len(),
        "recorded front index shorter than its true length"
    );
    let true_bytes = &padded[..fi_true_len];
    format::parse_front_index(&String::from_utf8_lossy(true_bytes))
        .expect("front index must parse from recorded bytes")
}

// ── MANIFEST.toml parsing (assertion 7 — the heir has only this + a key) ──

#[derive(Deserialize)]
struct ManifestSliceDoc {
    tape_position: i32,
}

#[derive(Deserialize)]
struct ManifestUnitDoc {
    uuid: String,
    #[serde(default)]
    slices: Vec<ManifestSliceDoc>,
}

#[derive(Deserialize)]
struct ManifestDoc {
    units: Vec<ManifestUnitDoc>,
}

// ── the 7 required assertions ──

/// Assertion 1 (plan T7 / sheet §4.1): parse File 3, verify the seal marker
/// binds it, then verify every other file's on-tape bytes against File 3's
/// claim — all from `MemStore.files` alone.
#[test]
fn keyless_chain_walk_from_recorded_bytes_verifies_every_file() {
    let h = shared_harness();
    let (fi_pos, fi_true_len) = front_index_position_and_true_len(&h.layout);

    match keyless_chain_walk(&h.store.files, fi_pos, fi_true_len) {
        ChainWalkVerdict::Sealed { parsed_front_index } => {
            assert_eq!(
                parsed_front_index.len(),
                h.store.files.len(),
                "front index must enumerate every recorded file"
            );
        }
        ChainWalkVerdict::Unsealed { reason } => {
            panic!("expected Sealed on a freshly-sealed harness, got Unsealed: {reason}")
        }
    }
}

/// Assertion 2 (§2.5): front-index self-consistency on the PARSED index, plus
/// the seal's `file_count` matching both the parsed entry count and the
/// actual number of recorded files.
#[test]
fn front_index_self_consistency_and_seal_file_count_match() {
    let h = shared_harness();
    let (fi_pos, fi_true_len) = front_index_position_and_true_len(&h.layout);
    let parsed = parse_front_index_from_recorded_bytes(&h.store.files, fi_pos, fi_true_len);

    let violations = format::validate_consistency(&parsed);
    assert!(
        violations.is_empty(),
        "front index has consistency violations: {violations:?}"
    );

    // Positions strictly increasing from 0 — validate_consistency already
    // enforces this, asserted again explicitly since the task names it as a
    // required property in its own right, not just an implementation detail.
    for (i, e) in parsed.iter().enumerate() {
        assert_eq!(
            e.position, i as i32,
            "position out of sequence at index {i}"
        );
    }

    let front_indexes: Vec<_> = parsed
        .iter()
        .filter(|e| e.type_label == "front_index")
        .collect();
    assert_eq!(
        front_indexes.len(),
        1,
        "exactly one front_index entry required"
    );
    assert_eq!(
        front_indexes[0].position, 3,
        "front_index must sit at position 3"
    );

    let seal_markers: Vec<_> = parsed
        .iter()
        .filter(|e| e.type_label == "seal_marker")
        .collect();
    assert_eq!(
        seal_markers.len(),
        1,
        "exactly one seal_marker entry required"
    );
    assert_eq!(
        seal_markers[0].position as usize,
        parsed.len() - 1,
        "seal_marker must be the last entry"
    );

    let seal = format::parse_seal_marker(&String::from_utf8_lossy(h.store.files.last().unwrap()))
        .expect("seal marker parses");
    assert_eq!(
        seal.file_count as usize,
        parsed.len(),
        "seal's file_count must match the front index's entry count"
    );
    assert_eq!(
        seal.file_count as usize,
        h.store.files.len(),
        "seal's file_count must match the actual number of recorded files"
    );
}

/// §1.1 (ADR-0012, issue #192): File 0 records HOW its `cartridge_serial`
/// was established, and an heir can read that with **no key and no
/// database** — from the recorded bytes of File 0 alone, exactly as
/// `mt rewind; dd | tr -d '\0'` would hand them over.
///
/// This is the question no other plaintext file can answer: whether
/// `cartridge_serial` is verifiable against the MEDIUM (load the tape, read
/// the MAM, compare) or only against a catalog the heir may not have. A
/// provenance field with no reader records something nothing consumes, so
/// the reader is proven here, on real recorded bytes, rather than only in a
/// generator round-trip.
///
/// Uses `shared_harness()` deliberately: its `mam_serial` is `"T7SERIAL"`
/// with a `"mam"` binding behind it. `build_layout_only`'s serial is the
/// empty string, so the same assertions there would pass vacuously.
#[test]
fn file_0_tells_a_keyless_heir_how_the_cartridge_serial_was_established() {
    let h = shared_harness();

    // Everything below comes from position 0's recorded bytes. No Layout, no
    // key, no DB — the heir strips the block padding and reads the TOML.
    let raw = &h.store.files[0];
    let text = String::from_utf8_lossy(raw);
    let text = text.trim_end_matches('\0');

    let media = format::parse_id_thunk_media(text).expect("File 0's [media] table parses");
    assert_eq!(
        media.cartridge_serial, "T7SERIAL",
        "File 0 must name the cartridge this volume was written to"
    );
    assert_eq!(
        media.cartridge_identity_source.as_deref(),
        Some("mam"),
        "the harness binds from a chip-reported serial, so the tape must say so — \
         this is what tells an heir the serial is checkable against the medium itself"
    );

    // §1.1: `[media]` must stay the LAST table. RESTORE.sh's `toml_val` is
    // table-blind and first-match-wins across the tables its `sed`
    // concatenates, so a key appended anywhere but the tail could shadow one
    // an earlier table owns. Proven from the bytes, not from the generator.
    let media_at = text.find("[media]").expect("File 0 has a [media] table");
    assert!(
        !text[media_at + "[media]".len()..].contains('['),
        "[media] must remain the last table in File 0"
    );

    // And the new key sits inside `[media]`, after `cartridge_serial` —
    // never in `[layout]`, whose key set is pinned to exactly three.
    let serial_at = text
        .find("cartridge_serial = ")
        .expect("File 0 carries cartridge_serial");
    let source_at = text
        .find("cartridge_identity_source = ")
        .expect("File 0 carries cartridge_identity_source");
    assert!(media_at < serial_at && serial_at < source_at);
}

/// Assertion 3 (§1): v2 zone order — the fixed front files, envelopes
/// strictly before slices, seal marker strictly last, and no
/// `planning_header` entry anywhere (folded into the operator envelope's
/// PLAN.toml — v2 removes it as a standalone tape file).
#[test]
fn v2_zone_order_envelopes_precede_slices_no_planning_header() {
    let h = shared_harness();
    let (fi_pos, fi_true_len) = front_index_position_and_true_len(&h.layout);
    let parsed = parse_front_index_from_recorded_bytes(&h.store.files, fi_pos, fi_true_len);

    assert_eq!(parsed[0].type_label, "id_thunk");
    assert_eq!(parsed[1].type_label, "system_guide");
    assert_eq!(parsed[2].type_label, "restore_sh");
    assert_eq!(parsed[3].type_label, "front_index");

    let last_envelope_idx = parsed
        .iter()
        .rposition(|e| {
            matches!(
                e.type_label.as_str(),
                "tenant_envelope"
                    | "operator_envelope"
                    | "operator_envelope_backup"
                    | "tenant_envelope_backup"
            )
        })
        .expect("harness must have at least one envelope");
    let first_slice_idx = parsed
        .iter()
        .position(|e| e.type_label == "data_slice")
        .expect("harness must have at least one data slice");
    assert!(
        last_envelope_idx < first_slice_idx,
        "every envelope must precede every data slice"
    );

    assert_eq!(parsed.last().unwrap().type_label, "seal_marker");

    assert!(
        parsed.iter().all(|e| e.type_label != "planning_header"),
        "v2 must never emit a planning_header entry on tape (folded into the \
         operator envelope's PLAN.toml, volume-format-v2.md §8)"
    );

    // 2 tenants => 2 distinct tenant_envelope entries, proving the
    // permutation-exercise setup actually produced multiple envelopes here.
    let tenant_envelope_count = parsed
        .iter()
        .filter(|e| e.type_label == "tenant_envelope")
        .count();
    assert_eq!(
        tenant_envelope_count, 2,
        "harness must use exactly 2 tenants"
    );

    // §1.3: the middle zone is the tenant envelopes, the operator envelope
    // and its backup, then one backup per tenant envelope in the primaries'
    // order — each the same bytes, so the same size and hash in File 3.
    let of = |label: &str| -> Vec<&format::ParsedIndexEntry> {
        parsed.iter().filter(|e| e.type_label == label).collect()
    };
    let primaries = of("tenant_envelope");
    let backups = of("tenant_envelope_backup");
    let op = of("operator_envelope");
    let op_backup = of("operator_envelope_backup");
    assert_eq!(
        backups.len(),
        primaries.len(),
        "one backup per tenant envelope"
    );
    assert_eq!((op.len(), op_backup.len()), (1, 1));
    let expected_positions: Vec<i32> = (4..4 + 2 * primaries.len() as i32 + 2).collect();
    let actual_positions: Vec<i32> = primaries
        .iter()
        .chain(&op)
        .chain(&op_backup)
        .chain(&backups)
        .map(|e| e.position)
        .collect();
    assert_eq!(
        actual_positions, expected_positions,
        "tenant envelopes, operator envelope, its backup, the tenant backups: contiguous from File 4"
    );
    for (p, b) in primaries.iter().zip(&backups) {
        assert_eq!(
            (p.size_bytes, &p.sha256_encrypted),
            (b.size_bytes, &b.sha256_encrypted),
            "the backup at {} must carry the size and hash of the primary at {}, in order",
            b.position,
            p.position
        );
        assert_eq!(
            h.store.files[p.position as usize], h.store.files[b.position as usize],
            "a tenant envelope's backup is its exact bytes on tape"
        );
    }
    assert_eq!(
        (op[0].size_bytes, &op[0].sha256_encrypted),
        (op_backup[0].size_bytes, &op_backup[0].sha256_encrypted)
    );
}

/// Assertion 4 (§2.1): the envelope permutation is a deterministic function
/// of `volume_uuid` (same uuid => same order every build) that is NOT the
/// raw `tenant_id` sequence (a different uuid changes the order) — proving
/// permutation actually happened rather than the test passing vacuously.
/// uuids and the expected orders are precomputed against the §2.1 algorithm
/// (sha256(volume_uuid_bytes || 0x00 || le64(tenant_id)), hex-sorted) for
/// tenant_ids [1, 2] — the same values `build.rs`'s own equivalent test
/// cross-checked via Python hashlib.
#[test]
fn tenant_envelope_permutation_is_deterministic_and_uuid_sensitive() {
    let uuid_a = "11111111-1111-1111-1111-111111111111"; // -> [2, 1]
    let uuid_b = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"; // -> [1, 2]
    let created_at = "2026-07-22T20:09:00Z";

    let envelope_order = |built: &BuiltLayout| -> Vec<i64> {
        built
            .layout
            .entries
            .iter()
            .filter_map(|e| match e.kind {
                ZoneKind::TenantEnvelope { tenant_id } => Some(tenant_id),
                _ => None,
            })
            .collect()
    };

    let order_a1 = envelope_order(&build_layout_only(uuid_a, created_at));
    let order_a2 = envelope_order(&build_layout_only(uuid_a, created_at));
    let order_b = envelope_order(&build_layout_only(uuid_b, created_at));

    assert_eq!(
        order_a1,
        vec![2, 1],
        "permutation must match the §2.1 hex-sort algorithm"
    );
    assert_ne!(
        order_a1,
        vec![1, 2],
        "must not be a no-op identity permutation for this fixture (proves \
         permutation actually happened, not just tenant_id order by coincidence)"
    );
    assert_eq!(
        order_a1, order_a2,
        "same volume_uuid must give the same order every build"
    );
    assert_eq!(order_b, vec![1, 2]);
    assert_ne!(
        order_a1, order_b,
        "a different volume_uuid must change the order"
    );
}

/// Assertion 5 (§2.2 "materialize-to-staging"): building twice from
/// identical inputs (including the injected `created_at`) yields
/// byte-identical frozen zones for id_thunk/system_guide/restore_sh — this
/// is what makes resume safe (frozen zones are re-read, never regenerated).
/// Checked via `(size_bytes, sha256)` equality rather than re-reading bytes
/// from disk: sha256 equality IS the byte-identity proof (the same
/// reasoning the whole keyless chain relies on). Envelopes are age-encrypted
/// (`age::Encryptor` is randomized per call — a fresh ephemeral key exchange
/// each time) and therefore NOT deterministic; excluded here, as build.rs's
/// own equivalent test also documents empirically.
#[test]
fn frozen_deterministic_zones_are_byte_identical_across_two_builds() {
    let uuid = "550e8400-e29b-41d4-a716-446655440000";
    let created_at = "2026-07-22T20:09:00Z";
    let built1 = build_layout_only(uuid, created_at);
    let built2 = build_layout_only(uuid, created_at);

    for kind in [
        ZoneKind::IdThunk,
        ZoneKind::SystemGuide,
        ZoneKind::RestoreSh,
    ] {
        let e1 = built1
            .layout
            .entries
            .iter()
            .find(|e| e.kind == kind)
            .unwrap();
        let e2 = built2
            .layout
            .entries
            .iter()
            .find(|e| e.kind == kind)
            .unwrap();
        assert_eq!(
            e1.size_bytes, e2.size_bytes,
            "{kind:?} size differs across builds"
        );
        assert_eq!(
            e1.sha256, e2.sha256,
            "{kind:?} hash differs across builds -- byte-identity proof"
        );
    }
}

/// Assertion 6 (§2.5 fail-safe precedence): dropping the seal marker from
/// the recorded bytes must verdict UNSEALED — never "sealed", never a panic
/// — both from this suite's own independent reader (`keyless_chain_walk`)
/// AND from the real production `Store::confirm`, which must report the
/// failure via `Evidence.mismatches` rather than an `Err`.
#[test]
fn dropping_the_seal_marker_yields_unsealed_never_a_panic() {
    let h = shared_harness();
    let (fi_pos, fi_true_len) = front_index_position_and_true_len(&h.layout);

    let mut truncated_files = h.store.files.clone();
    let dropped = truncated_files.pop();
    assert!(
        dropped.is_some(),
        "harness must have recorded a seal marker to drop"
    );

    match keyless_chain_walk(&truncated_files, fi_pos, fi_true_len) {
        ChainWalkVerdict::Unsealed { .. } => {}
        ChainWalkVerdict::Sealed { .. } => {
            panic!("dropping the seal marker must never verify as Sealed")
        }
    }

    let mut truncated_store = MemStore::new(BS as usize);
    truncated_store.files = truncated_files;
    let evidence = truncated_store
        .confirm(&h.layout, Tier::Integrity)
        .expect("Store::confirm must report failure via Evidence.mismatches, never Err");
    assert!(
        !evidence.mismatches.is_empty(),
        "dropping the seal marker must produce at least one mismatch"
    );
    assert!(
        evidence
            .mismatches
            .iter()
            .any(|m| m.kind == MismatchKind::SealUnreadable),
        "expected a SealUnreadable mismatch, got {:?}",
        evidence.mismatches
    );
}

/// Assertion 7 (§6, the Heir Path): pick one tenant, trial-decrypt the
/// (permuted) tenant-envelope positions with that tenant's fixture identity
/// to find theirs, parse its MANIFEST.toml, use it to locate that unit's
/// slice position(s), decrypt those slices from the recorded bytes, and
/// assert the plaintext matches the fixture generator's original bytes
/// exactly. This is the end-to-end proof: keyless verification (assertions
/// 1-3, 6) first, then keyed recovery.
#[test]
fn keyed_heir_restore_matches_original_plaintext_exactly() {
    let h = shared_harness();
    let (fi_pos, fi_true_len) = front_index_position_and_true_len(&h.layout);
    let parsed = parse_front_index_from_recorded_bytes(&h.store.files, fi_pos, fi_true_len);

    // The heir has exactly this: their own age secret key, nothing else.
    let tenant = &h.tenants[0];
    let identity: age::x25519::Identity = tenant
        .secret_key
        .parse()
        .expect("fixture secret key parses as an age identity");

    let envelope_positions: Vec<i32> = parsed
        .iter()
        .filter(|e| e.type_label == "tenant_envelope")
        .map(|e| e.position)
        .collect();
    assert!(!envelope_positions.is_empty(), "no tenant envelopes found");

    // Trial-decrypt: try every tenant envelope position with THIS tenant's
    // key; the one that decrypts is theirs. This is exactly why an heir
    // cannot just assume "my envelope is at position 4" — the §2.1
    // permutation means they must try each in turn.
    let mut found_tar: Option<Vec<u8>> = None;
    for pos in &envelope_positions {
        let entry = parsed.iter().find(|e| e.position == *pos).unwrap();
        let size = entry.size_bytes.expect("envelope entry must be sized") as usize;
        let padded = &h.store.files[*pos as usize];
        assert!(
            size <= padded.len(),
            "envelope at {pos} shorter than recorded size"
        );
        let true_bytes = &padded[..size];
        let Ok(decryptor) = age::Decryptor::new(true_bytes) else {
            continue;
        };
        let Ok(mut reader) = decryptor.decrypt(std::iter::once(&identity as &dyn age::Identity))
        else {
            continue;
        };
        let mut tar_bytes = Vec::new();
        if reader.read_to_end(&mut tar_bytes).is_ok() {
            found_tar = Some(tar_bytes);
            break;
        }
    }
    let tar_bytes = found_tar.unwrap_or_else(|| {
        panic!(
            "tenant {}'s own envelope must be found among the {} permuted tenant_envelope positions",
            tenant.name,
            envelope_positions.len()
        )
    });

    // Extract MANIFEST.toml from the decrypted tar.
    let mut archive = tar::Archive::new(std::io::Cursor::new(tar_bytes));
    let mut manifest_str = None;
    for entry in archive.entries().expect("tar entries") {
        let mut entry = entry.expect("tar entry");
        let path = entry
            .path()
            .expect("tar entry path")
            .to_string_lossy()
            .into_owned();
        if path == "MANIFEST.toml" {
            let mut s = String::new();
            entry.read_to_string(&mut s).expect("read MANIFEST.toml");
            manifest_str = Some(s);
            break;
        }
    }
    let manifest_str = manifest_str.expect("envelope tar must contain MANIFEST.toml");
    let manifest: ManifestDoc = toml::from_str(&manifest_str).expect("MANIFEST.toml parses");

    // This tenant's units per the harness's own ground truth (an heir
    // wouldn't have this — they'd just trust the manifest — but the test
    // needs it to know what plaintext to expect back).
    let expected: Vec<&UnitPlain> = h
        .units
        .iter()
        .filter(|u| u.tenant_id == tenant.tenant_id)
        .collect();
    assert!(!expected.is_empty(), "tenant {} owns no units", tenant.name);
    assert_eq!(
        manifest.units.len(),
        expected.len(),
        "manifest must list exactly this tenant's units, no more, no fewer"
    );

    for exp_unit in &expected {
        let manifest_unit = manifest
            .units
            .iter()
            .find(|u| u.uuid == exp_unit.unit_uuid)
            .unwrap_or_else(|| panic!("manifest missing unit {}", exp_unit.unit_uuid));
        assert_eq!(
            manifest_unit.slices.len(),
            1,
            "harness uses exactly one slice per unit"
        );
        let slice_pos = manifest_unit.slices[0].tape_position;

        let fi_claim = parsed
            .iter()
            .find(|e| e.position == slice_pos)
            .unwrap_or_else(|| panic!("front index missing position {slice_pos}"));
        assert_eq!(fi_claim.type_label, "data_slice");
        let size = fi_claim.size_bytes.expect("slice entry must be sized") as usize;
        let padded = &h.store.files[slice_pos as usize];
        assert!(
            size <= padded.len(),
            "slice at {slice_pos} shorter than recorded size"
        );
        let ciphertext = &padded[..size];

        let decryptor = age::Decryptor::new(ciphertext).expect("slice decryptor");
        let mut reader = decryptor
            .decrypt(std::iter::once(&identity as &dyn age::Identity))
            .expect("tenant's own key must decrypt their own slice");
        let mut plaintext = Vec::new();
        reader
            .read_to_end(&mut plaintext)
            .expect("read decrypted slice");

        assert_eq!(
            plaintext, exp_unit.plaintext,
            "restored plaintext must exactly match the fixture generator's original \
             bytes for unit {}",
            exp_unit.unit_name
        );
    }
}

/// Trial-decrypt one envelope's recorded bytes with `identity`, checking
/// them against the front index's keyless hash first, as an heir does.
/// `None` when the bytes do not match File 3 or do not decrypt.
fn open_recorded_envelope(
    files: &[Vec<u8>],
    entry: &format::ParsedIndexEntry,
    identity: &age::x25519::Identity,
) -> Option<Vec<u8>> {
    let size = entry.size_bytes.expect("envelope entry must be sized") as usize;
    let padded = files.get(entry.position as usize)?;
    let true_bytes = padded.get(..size)?;
    if Some(sha256_hex(true_bytes)) != entry.sha256_encrypted {
        return None;
    }
    let decryptor = age::Decryptor::new(true_bytes).ok()?;
    let mut reader = decryptor
        .decrypt(std::iter::once(identity as &dyn age::Identity))
        .ok()?;
    let mut tar_bytes = Vec::new();
    reader.read_to_end(&mut tar_bytes).ok()?;
    Some(tar_bytes)
}

/// §1.3, #418 item 2: a tenant envelope damaged on tape does not strand the
/// tenant. Keyless, the heir sees the primary no longer matches File 3; the
/// `tenant_envelope_backup` with the same hash is the same envelope, and it
/// opens with the tenant's own key and lists the tenant's units. Recorded
/// bytes only, as everywhere in this suite.
#[test]
fn a_damaged_tenant_envelope_is_recovered_from_its_backup() {
    let h = shared_harness();
    let (fi_pos, fi_true_len) = front_index_position_and_true_len(&h.layout);
    let parsed = parse_front_index_from_recorded_bytes(&h.store.files, fi_pos, fi_true_len);
    let tenant = &h.tenants[0];
    let identity: age::x25519::Identity = tenant.secret_key.parse().unwrap();

    // Positive control: on the undamaged tape the primary opens.
    let primary = parsed
        .iter()
        .filter(|e| e.type_label == "tenant_envelope")
        .find(|e| open_recorded_envelope(&h.store.files, e, &identity).is_some())
        .expect("the tenant's own primary envelope opens on an undamaged tape")
        .clone();

    // Damage it: one byte in the middle of the ciphertext.
    let mut files = h.store.files.clone();
    let size = primary.size_bytes.unwrap() as usize;
    files[primary.position as usize][size / 2] ^= 0xff;
    assert!(
        open_recorded_envelope(&files, &primary, &identity).is_none(),
        "the damaged primary must no longer open"
    );

    // The heir's fallback: the backup carrying the primary's hash.
    let backup = parsed
        .iter()
        .find(|e| {
            e.type_label == "tenant_envelope_backup"
                && e.sha256_encrypted == primary.sha256_encrypted
        })
        .expect("every tenant envelope has a backup with its hash");
    assert!(backup.position > primary.position + 1);
    let tar_bytes = open_recorded_envelope(&files, backup, &identity)
        .expect("the backup opens with the tenant's own key");

    let mut archive = tar::Archive::new(std::io::Cursor::new(tar_bytes));
    let mut manifest_str = None;
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        if entry.path().unwrap().to_string_lossy() == "MANIFEST.toml" {
            let mut s = String::new();
            entry.read_to_string(&mut s).unwrap();
            manifest_str = Some(s);
        }
    }
    let manifest: ManifestDoc =
        toml::from_str(&manifest_str.expect("MANIFEST.toml in the backup")).unwrap();
    let mut listed: Vec<&str> = manifest.units.iter().map(|u| u.uuid.as_str()).collect();
    let mut expected: Vec<&str> = h
        .units
        .iter()
        .filter(|u| u.tenant_id == tenant.tenant_id)
        .map(|u| u.unit_uuid.as_str())
        .collect();
    listed.sort_unstable();
    expected.sort_unstable();
    assert_eq!(
        listed, expected,
        "the backup lists exactly this tenant's units"
    );

    // And the keyless chain names the damage, at the primary.
    match keyless_chain_walk(&files, fi_pos, fi_true_len) {
        ChainWalkVerdict::Unsealed { reason } => assert!(
            reason.contains(&format!(
                "content hash mismatch at position {}",
                primary.position
            )),
            "{reason}"
        ),
        ChainWalkVerdict::Sealed { .. } => panic!("a damaged envelope must fail the chain walk"),
    }
}

/// ADR-0012 amendment item 15: a seal marker that requires a feature this
/// tapectl does not know stops `Store::confirm` as inconclusive, naming the
/// feature — never a misread, and never evidence that the medium is bad
/// (which would quarantine a sound cartridge a newer tapectl wrote). A key
/// nobody requires is read past, and the tape still confirms.
#[test]
fn a_seal_requiring_an_unknown_feature_is_inconclusive_not_medium_bad() {
    let h = shared_harness();
    let last = h.store.files.len() - 1;
    let seal = String::from_utf8_lossy(&h.store.files[last]).to_string();
    assert!(seal.contains("\nrequires = []\n"), "a 1.1.0 seal states it");

    let with = |s: String| {
        let mut store = MemStore::new(BS as usize);
        store.files = h.store.files.clone();
        let mut bytes = s.into_bytes();
        bytes.resize(h.store.files[last].len(), 0);
        store.files[last] = bytes;
        store
            .confirm(&h.layout, Tier::Integrity)
            .expect("Store::confirm reports via Evidence, never Err")
    };

    let evidence = with(seal.replacen("requires = []", "requires = [\"short-seal\"]", 1));
    assert!(
        evidence
            .mismatches
            .iter()
            .any(|m| m.kind == MismatchKind::SealUnreadable && m.actual.contains("\"short-seal\"")),
        "{:?}",
        evidence.mismatches
    );
    assert!(!evidence.proves_medium_bad(), "{:?}", evidence.mismatches);

    // Control: an unknown key that is not required changes nothing (the
    // padded seal bytes are not hashed by anything on the tape).
    let evidence = with(seal.replacen("requires = []", "requires = []\nended_after = 7", 1));
    assert!(evidence.mismatches.is_empty(), "{:?}", evidence.mismatches);
}

/// ADR-0012 2026-10-16 item 73, from a REAL `build()`: the 1.2.0 re-pin
/// changes generated text, not what a reader must understand. File 0 and the
/// seal marker still say `requires = []`, and the File 2 the build wrote still
/// knows no feature and still names "tapectl 1.1.0" as the version a newer
/// tape would need.
#[test]
fn the_re_pin_leaves_requires_empty_and_the_newer_tape_text_at_1_1_0() {
    let h = shared_harness();
    let text_of = |pos: usize| {
        String::from_utf8_lossy(&h.store.files[pos])
            .trim_end_matches('\0')
            .to_string()
    };
    let last = h.store.files.len() - 1;
    assert!(text_of(0).contains("\nrequires = []\n"), "File 0");
    assert!(text_of(last).contains("\nrequires = []\n"), "seal marker");
    let restore_sh = text_of(2);
    assert!(
        restore_sh.contains("\nKNOWN_FEATURES=\"\"\n"),
        "RESTORE.sh knows no feature"
    );
    assert!(
        restore_sh.contains("This RESTORE.sh (layout v2, tapectl 1.1.0) cannot read it safely"),
        "die_newer_needed says 1.1.0"
    );
}

/// File 1 of a REAL `build()`, for a volume whose `media_type` is LTO-6: the
/// build passes the generation through, so the tape says an LTO-6 or LTO-7
/// drive reads it (#448's File 1 half). Recorded bytes only.
#[test]
fn file_1_of_a_built_lto6_tape_names_the_drives_that_read_it() {
    let h = shared_harness();
    let guide = String::from_utf8_lossy(&h.store.files[1]).to_string();
    let guide = guide.trim_end_matches('\0');
    let flat = guide.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains("This tape is LTO-6"), "{flat}");
    assert!(
        flat.contains("only if it is an LTO-6 or LTO-7 drive"),
        "{flat}"
    );
    assert!(flat.contains("SAS"), "{flat}");
    assert!(flat.contains("--all --to /destination"), "{flat}");
}

// ── Issue #450 — the plaintext zones, scanned without a tape ──────────────

const VOLUME_UUID_NEEDLES: &str = "0f1e2d3c-4b5a-4968-8776-a5b4c3d2e1f0";

/// Unique strings: nothing the system guide, RESTORE.sh, the front index or
/// the ID thunk say in their own words can contain one, so a hit is a leak
/// and not a coincidence of vocabulary. (The shared harness's `alpha`,
/// `operator` and `movie.mkv` are ordinary words, which RESTORE.sh and the
/// guide use; scanning for them would only ever prove the scan noisy.)
const NEEDLE_OPERATOR: &str = "op-needle-w3z8v";
const NEEDLE_TENANT_A: &str = "tnt-alpha-q7x2k";
const NEEDLE_TENANT_B: &str = "tnt-bravo-m4k9p";
const NEEDLE_UNIT_A: &str = "unit-alpha-q7x2k";
const NEEDLE_UNIT_B: &str = "unit-bravo-m4k9p";
const NEEDLE_VOLUME_A: &str = "vol-alpha-q7x2k";
const NEEDLE_VOLUME_B: &str = "vol-bravo-m4k9p";
const NEEDLE_FILES_A: [&str; 2] = ["secret-report-q7x2k-alpha.pdf", "diary-q7x2k-alpha.txt"];
const NEEDLE_FILES_B: [&str; 1] = ["secret-report-m4k9p-bravo.pdf"];

/// One fixture unit: a folder `name` holding `files`, each with content that
/// is its own (so its plaintext hash is its own).
fn needle_unit(root: &std::path::Path, name: &str, files: &[&str]) -> UnitFixture {
    let path = root.join(name);
    std::fs::create_dir_all(&path).unwrap();
    let mut listed = Vec::new();
    for (i, file) in files.iter().enumerate() {
        let body = format!("{name} / {file} / part {i}\n")
            .repeat(64)
            .into_bytes();
        std::fs::write(path.join(file), &body).unwrap();
        listed.push((file.to_string(), body.len() as u64));
    }
    UnitFixture {
        folder_name: name.to_string(),
        path,
        total_bytes: listed.iter().map(|(_, n)| n).sum(),
        files: listed,
    }
}

/// Every needle in `bytes`, as raw bytes (no decoding: a lossy decode is
/// where a needle split across a multi-byte sequence could be swallowed).
/// The scan's ONE routine: the assertion on the real tape and every positive
/// control below call it, so a control that passes proves the very search
/// the assertion runs.
fn find_needles<'a>(bytes: &[u8], needles: &'a [String]) -> Vec<&'a str> {
    needles
        .iter()
        .filter(|n| bytes.windows(n.len()).any(|w| w == n.as_bytes()))
        .map(String::as_str)
        .collect()
}

/// `(position, needle)` for every needle found in the files at `positions`.
fn leaks_in(files: &[Vec<u8>], positions: &[usize], needles: &[String]) -> Vec<(usize, String)> {
    positions
        .iter()
        .flat_map(|&p| {
            find_needles(&files[p], needles)
                .into_iter()
                .map(move |n| (p, n.to_string()))
        })
        .collect()
}

/// Everything the isolation invariant (`volume-format-v2.md` §2) says no
/// plaintext file may reveal, for this fixture: tenant and unit names, file
/// names, plaintext hashes (each unit's `sha256_plain` and each file's own),
/// and key fingerprints, with the `age1…` recipient strings.
fn plaintext_zone_needles(h: &Harness, fixtures: &[UnitFixture]) -> Vec<String> {
    let mut needles = Vec::new();
    for t in h.tenants.iter().chain(std::iter::once(&h.operator)) {
        needles.extend([t.name.clone(), t.public_key.clone(), t.fingerprint.clone()]);
    }
    if let Some(escrow) = &h.escrow {
        needles.extend([escrow.public_key.clone(), escrow.fingerprint.clone()]);
    }
    for f in fixtures {
        needles.push(f.folder_name.clone());
        for (file, _) in &f.files {
            needles.push(file.clone());
            needles.push(sha256_hex(&std::fs::read(f.path.join(file)).unwrap()));
        }
    }
    needles.extend(h.units.iter().map(|u| u.sha256_plain.clone()));
    // The labels of the OTHER volumes the cross-volume index names (§1.5):
    // encrypted in the envelopes, and nowhere in the clear.
    needles.extend(h.history.iter().map(|(label, _, _)| label.clone()));
    needles.sort();
    needles.dedup();
    needles
}

/// Issue #450: the isolation invariant, machine-checked in CI. The only
/// scans of a volume's plaintext zones were `mhvtl_no_plaintext_tenant_
/// metadata` (mhvtl-gated) and the gate's leak scan; nothing ungated looked,
/// and nothing looked for key fingerprints or `age1…` strings at all. This
/// seals a volume onto a `MemStore` whose fixture carries unique tenant,
/// unit and file names, real plaintext hashes, three keys and an escrow
/// recipient, and scans File 0 (ID thunk), File 1 (system guide), File 2
/// (RESTORE.sh), File 3 (front index) and the seal marker for every one.
///
/// Every assertion is an absence, and an absence cannot tell "searched and
/// found nothing" from "searched nothing", so each is paired with a control
/// that runs the same routine over bytes that DO hold the needle.
#[test]
fn no_plaintext_zone_reveals_a_tenant_unit_file_hash_or_key() {
    let root = tempfile::tempdir().unwrap();
    let fixtures = vec![
        needle_unit(root.path(), NEEDLE_UNIT_A, &NEEDLE_FILES_A),
        needle_unit(root.path(), NEEDLE_UNIT_B, &NEEDLE_FILES_B),
    ];
    let spec = HarnessSpec {
        operator: NEEDLE_OPERATOR,
        tenants: [NEEDLE_TENANT_A, NEEDLE_TENANT_B],
        real_plaintext_hash: true,
        escrow: true,
        history: [NEEDLE_VOLUME_A, NEEDLE_VOLUME_B],
    };
    let h = build_sealed_harness_from(&fixtures, &spec, VOLUME_UUID_NEEDLES);
    let needles = plaintext_zone_needles(&h, &fixtures);

    // The needle set is what the invariant lists, and none of it is trivial.
    assert!(
        needles.iter().all(|n| n.len() >= 15),
        "a short needle matches ordinary vocabulary: {needles:?}"
    );
    assert_eq!(
        needles.iter().filter(|n| n.starts_with("age1")).count(),
        4,
        "the operator's, both tenants' and the escrow recipient's age1 strings: {needles:?}"
    );
    for want in [
        NEEDLE_TENANT_A,
        NEEDLE_TENANT_B,
        NEEDLE_OPERATOR,
        NEEDLE_UNIT_A,
        NEEDLE_FILES_A[1],
        NEEDLE_FILES_B[0],
        NEEDLE_VOLUME_A,
        NEEDLE_VOLUME_B,
    ] {
        assert!(needles.iter().any(|n| n == want), "{want} is not a needle");
    }
    for u in &h.units {
        assert_eq!(
            u.sha256_plain,
            sha256_hex(&u.plaintext),
            "the recorded plaintext hash is the real one, so the scan looks for the real one"
        );
        assert!(needles.contains(&u.sha256_plain));
    }

    // The plaintext-by-design positions (§2): 0, 1, 2, the front index at 3,
    // and the seal marker, last.
    let (front_index, _) = front_index_position_and_true_len(&h.layout);
    assert_eq!(front_index, 3, "the front index sits at File 3");
    let files = &h.store.files;
    let seal_marker = files.len() - 1;
    let plaintext_zones = [0usize, 1, 2, front_index, seal_marker];

    // The assertion: none of it is on the real recorded bytes.
    let leaks = leaks_in(files, &plaintext_zones, &needles);
    assert!(leaks.is_empty(), "plaintext zones reveal: {leaks:?}");

    // Control 1: with every needle planted mid-file in a copy of each zone's
    // own bytes, the same routine finds every one. Mid-file on purpose: it
    // can split a multi-byte sequence.
    let planted_all: Vec<u8> = needles
        .iter()
        .flat_map(|n| format!("\n{n}\n").into_bytes())
        .collect();
    for &zone in &plaintext_zones {
        let mid = files[zone].len() / 2;
        let planted = [&files[zone][..mid], &planted_all[..], &files[zone][mid..]].concat();
        assert_eq!(
            find_needles(&planted, &needles).len(),
            needles.len(),
            "File {zone}: the scan missed a planted needle"
        );
    }

    // Control 2 (the failing case): a tenant name planted into File 1 is
    // reported at File 1, and only that name, at only that file.
    let mut tampered = files.clone();
    let mid = tampered[1].len() / 2;
    tampered[1].splice(mid..mid, NEEDLE_TENANT_A.bytes());
    assert_eq!(
        leaks_in(&tampered, &plaintext_zones, &needles),
        vec![(1, NEEDLE_TENANT_A.to_string())],
        "a tenant name planted into File 1 must be the one leak, found at File 1"
    );

    // Control 3: the scan does see what IS plaintext by design. File 0 names
    // the volume's label and uuid, so a scan that reads File 0 finds them.
    let by_design = ["T7MICRO".to_string(), VOLUME_UUID_NEEDLES.to_string()];
    assert_eq!(
        find_needles(&files[0], &by_design),
        ["T7MICRO", VOLUME_UUID_NEEDLES],
        "File 0 must carry the label and uuid the scan is shown to find"
    );

    // Everything else is age ciphertext, and holds no needle either.
    let age_magic = b"age-encryption.org/v1";
    for (pos, data) in files.iter().enumerate() {
        if plaintext_zones.contains(&pos) {
            continue;
        }
        assert!(
            data.starts_with(age_magic),
            "File {pos} is not age-encrypted: {:?}",
            &data[..data.len().min(32)]
        );
        assert!(
            find_needles(data, &needles).is_empty(),
            "encrypted File {pos} holds a needle in the clear"
        );
    }
    assert!(
        files.len() > plaintext_zones.len(),
        "no encrypted file scanned"
    );
}

// ── the cross-volume index (§1.5, #418 R4) ──

/// An envelope's tar members, in tar order, from RECORDED bytes: `(name,
/// bytes)`. `None` when `identity` is not a recipient or the bytes do not
/// match File 3's hash.
fn envelope_members(
    files: &[Vec<u8>],
    entry: &format::ParsedIndexEntry,
    identity: &age::x25519::Identity,
) -> Option<Vec<(String, Vec<u8>)>> {
    let tar_bytes = open_recorded_envelope(files, entry, identity)?;
    let mut archive = tar::Archive::new(std::io::Cursor::new(tar_bytes));
    let mut members = Vec::new();
    for member in archive.entries().unwrap() {
        let mut member = member.unwrap();
        let name = member.path().unwrap().to_string_lossy().to_string();
        let mut bytes = Vec::new();
        member.read_to_end(&mut bytes).unwrap();
        members.push((name, bytes));
    }
    Some(members)
}

fn member<'a>(members: &'a [(String, Vec<u8>)], name: &str) -> Option<&'a str> {
    members
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, b)| std::str::from_utf8(b).unwrap())
}

fn names(members: &[(String, Vec<u8>)]) -> Vec<&str> {
    members.iter().map(|(n, _)| n.as_str()).collect()
}

/// The harness tape's File 3 entries, by recorded bytes.
fn recorded_entries(h: &Harness) -> Vec<format::ParsedIndexEntry> {
    let (fi_pos, fi_true_len) = front_index_position_and_true_len(&h.layout);
    parse_front_index_from_recorded_bytes(&h.store.files, fi_pos, fi_true_len)
}

/// Every envelope of the tape, both copies, opens with the keys that should
/// open it and carries the index members §1.5 gives it, after everything
/// else in the tar: the tenant's `units.tsv` and `paths.tsv` in each of its
/// two copies, the operator's `units.tsv` alone in its two. Each backup is
/// its primary's bytes, so the index in it is the same index.
#[test]
fn every_envelope_carries_its_index_members_and_the_backups_are_byte_copies() {
    let h = shared_harness();
    let entries = recorded_entries(h);
    let files = &h.store.files;

    for t in &h.tenants {
        let identity: age::x25519::Identity = t.secret_key.parse().unwrap();
        let primary = entries
            .iter()
            .filter(|e| e.type_label == "tenant_envelope")
            .find(|e| open_recorded_envelope(files, e, &identity).is_some())
            .expect("the tenant's primary opens with its own key");
        let backup = entries
            .iter()
            .find(|e| {
                e.type_label == "tenant_envelope_backup"
                    && e.sha256_encrypted == primary.sha256_encrypted
            })
            .expect("and has a backup with its hash");
        // A backup is its primary's bytes (§1.3), so the sizes and the
        // ciphertexts are equal, not just the plaintexts.
        let size = primary.size_bytes.unwrap() as usize;
        assert_eq!(backup.size_bytes, primary.size_bytes);
        assert_eq!(
            files[primary.position as usize][..size],
            files[backup.position as usize][..size],
            "the tenant backup is not a byte copy of its primary"
        );
        for entry in [primary, backup] {
            let members = envelope_members(files, entry, &identity).unwrap();
            assert_eq!(
                names(&members),
                [
                    "MANIFEST.toml",
                    "RECOVERY.md",
                    "index/units.tsv",
                    "index/paths.tsv"
                ],
                "{} at {}: the index follows everything else",
                entry.type_label,
                entry.position
            );
            let units = member(&members, "index/units.tsv").unwrap();
            assert_eq!(
                units.lines().next().unwrap(),
                index::UNITS_COLUMNS.join("\t")
            );
            let paths = member(&members, "index/paths.tsv").unwrap();
            assert_eq!(
                paths.lines().next().unwrap(),
                index::PATHS_COLUMNS.join("\t")
            );
        }
    }

    let op: age::x25519::Identity = h.operator.secret_key.parse().unwrap();
    let primary = entries
        .iter()
        .find(|e| e.type_label == "operator_envelope")
        .unwrap();
    let backup = entries
        .iter()
        .find(|e| e.type_label == "operator_envelope_backup")
        .unwrap();
    let size = primary.size_bytes.unwrap() as usize;
    assert_eq!(
        files[primary.position as usize][..size],
        files[backup.position as usize][..size],
        "the operator backup is not a byte copy of its primary"
    );
    for entry in [primary, backup] {
        let members = envelope_members(files, entry, &op).unwrap();
        assert_eq!(
            names(&members),
            [
                "MANIFEST.toml",
                "RECOVERY.md",
                "PLAN.toml",
                "index/units.tsv"
            ],
            "{} at {}: the operator map, and no paths.tsv",
            entry.type_label,
            entry.position
        );
        let units = member(&members, "index/units.tsv").unwrap();
        assert_eq!(
            units.lines().next().unwrap(),
            format!("tenant\t{}", index::UNITS_COLUMNS.join("\t"))
        );
    }
}

/// What the index says of the tape it is on matches File 0 and the write:
/// this tape's label, uuid, serial and generation, `this-tape`, no
/// `sealed_at`, and the version's real slice count and ciphertext bytes.
#[test]
fn the_index_names_this_tape_as_file_0_does() {
    let h = shared_harness();
    let entries = recorded_entries(h);
    let op: age::x25519::Identity = h.operator.secret_key.parse().unwrap();
    let primary = entries
        .iter()
        .find(|e| e.type_label == "operator_envelope")
        .unwrap();
    let members = envelope_members(&h.store.files, primary, &op).unwrap();
    let rows = index::parse_units(member(&members, "index/units.tsv").unwrap()).unwrap();

    let file0 = String::from_utf8_lossy(&h.store.files[0]).to_string();
    let this: Vec<_> = rows.iter().filter(|r| r.state == "this-tape").collect();
    assert_eq!(this.len(), h.units.len(), "every unit of this write, once");
    for r in &this {
        assert_eq!(r.volume, "T7MICRO");
        assert_eq!(r.volume_uuid, VOLUME_UUID_MAIN);
        assert!(file0.contains(&r.volume) && file0.contains(&r.volume_uuid));
        assert_eq!(r.cartridge_serial, "T7SERIAL");
        assert!(file0.contains(&r.cartridge_serial));
        assert_eq!(r.generation, "LTO-6");
        assert_eq!(r.sealed_at, "");
        assert_eq!(
            (r.num_slices, r.first_slice, r.last_slice),
            (Some(1), Some(1), Some(1))
        );
        assert_eq!(r.version, 1);
    }
    // The history volumes hold version 2, sealed, with the catalog's facts.
    for (label, tenant_id, unit) in &h.history {
        let r = rows.iter().find(|r| &r.volume == label).unwrap();
        assert_eq!(
            (r.unit.as_str(), r.version, r.state.as_str()),
            (unit.as_str(), 2, "sealed")
        );
        assert_eq!(r.sealed_at, "2026-09-01T12:00:00Z");
        assert_eq!(r.volume_uuid, format!("hist-uuid-{label}"));
        let tenant = h
            .tenants
            .iter()
            .find(|t| t.tenant_id == *tenant_id)
            .unwrap();
        assert_eq!(r.tenant, tenant.name);
    }
}

/// Tenant isolation, on the decrypted index (§1.5, §2): a tenant's envelope
/// holds that tenant's units, files and the tapes holding them and nothing of
/// the other tenant; the operator map holds both. The other tenant's key does
/// not open this tenant's envelope at all, and the escrow-less operator key
/// reads the cross-tenant map from the operator envelope alone.
#[test]
fn a_tenants_index_holds_none_of_another_tenants_units_paths_or_tapes() {
    let h = shared_harness();
    let entries = recorded_entries(h);
    let files = &h.store.files;
    let op: age::x25519::Identity = h.operator.secret_key.parse().unwrap();

    for (mine, theirs) in [(0usize, 1usize), (1, 0)] {
        let (me, other) = (&h.tenants[mine], &h.tenants[theirs]);
        let identity: age::x25519::Identity = me.secret_key.parse().unwrap();
        let other_identity: age::x25519::Identity = other.secret_key.parse().unwrap();
        let primary = entries
            .iter()
            .filter(|e| e.type_label == "tenant_envelope")
            .find(|e| open_recorded_envelope(files, e, &identity).is_some())
            .unwrap();
        assert!(
            open_recorded_envelope(files, primary, &other_identity).is_none(),
            "the other tenant's key opens {}'s envelope",
            me.name
        );
        let members = envelope_members(files, primary, &identity).unwrap();
        let units = member(&members, "index/units.tsv").unwrap();
        let paths = member(&members, "index/paths.tsv").unwrap();

        let mine_units: Vec<&UnitPlain> = h
            .units
            .iter()
            .filter(|u| u.tenant_id == me.tenant_id)
            .collect();
        let other_units: Vec<&UnitPlain> = h
            .units
            .iter()
            .filter(|u| u.tenant_id == other.tenant_id)
            .collect();
        let my_history: Vec<_> = h
            .history
            .iter()
            .filter(|(_, t, _)| *t == me.tenant_id)
            .collect();
        let other_history: Vec<_> = h
            .history
            .iter()
            .filter(|(_, t, _)| *t == other.tenant_id)
            .collect();
        assert_eq!(my_history.len(), 1);
        assert_eq!(other_history.len(), 1);

        // Everything of mine is there.
        for u in &mine_units {
            assert!(units.contains(&u.unit_name) && units.contains(&u.unit_uuid));
            for f in &u.file_names {
                assert!(
                    paths.contains(f.as_str()),
                    "{f} missing from {}'s paths",
                    me.name
                );
            }
        }
        for (label, _, unit) in &my_history {
            assert!(units.contains(label.as_str()) && units.contains(unit.as_str()));
        }
        // Nothing of the other tenant's is, in either file.
        for text in [units, paths] {
            for u in &other_units {
                assert!(
                    !text.contains(&u.unit_name),
                    "{} leaked into {}'s index",
                    u.unit_name,
                    me.name
                );
                assert!(!text.contains(&u.unit_uuid));
                // File names are checked on the needle fixture below: this
                // generator repeats them from unit to unit.
            }
            for (label, _, _) in &other_history {
                assert!(
                    !text.contains(label.as_str()),
                    "{label} leaked into {}'s index",
                    me.name
                );
            }
            assert!(!text.contains(&other.name));
        }
    }

    // The operator map has both tenants, every tape, and no path at all.
    let primary = entries
        .iter()
        .find(|e| e.type_label == "operator_envelope")
        .unwrap();
    let members = envelope_members(files, primary, &op).unwrap();
    let text = member(&members, "index/units.tsv").unwrap();
    for t in &h.tenants {
        assert!(text.contains(&t.name));
    }
    for u in &h.units {
        assert!(text.contains(&u.unit_name));
    }
    for (label, _, _) in &h.history {
        assert!(text.contains(label.as_str()));
    }
    assert!(member(&members, "index/paths.tsv").is_none());
}

/// `paths.tsv` lists every path of every version listed in the tenant's
/// `units.tsv`: this write's version 1 (the unit's files) and, for the unit
/// with a history tape, version 2 (the same files and `history-only.dat`).
#[test]
fn a_tenants_paths_cover_every_listed_version() {
    let h = shared_harness();
    let entries = recorded_entries(h);
    let tenant = &h.tenants[0];
    let identity: age::x25519::Identity = tenant.secret_key.parse().unwrap();
    let primary = entries
        .iter()
        .filter(|e| e.type_label == "tenant_envelope")
        .find(|e| open_recorded_envelope(&h.store.files, e, &identity).is_some())
        .unwrap();
    let members = envelope_members(&h.store.files, primary, &identity).unwrap();
    let rows = index::parse_paths(member(&members, "index/paths.tsv").unwrap()).unwrap();

    let (_, _, history_unit) = h
        .history
        .iter()
        .find(|(_, t, _)| *t == tenant.tenant_id)
        .unwrap();
    for u in h.units.iter().filter(|u| u.tenant_id == tenant.tenant_id) {
        let v1: Vec<&str> = rows
            .iter()
            .filter(|r| r.unit == u.unit_name && r.version == 1)
            .map(|r| r.path.as_str())
            .collect();
        let mut want: Vec<&str> = u.file_names.iter().map(String::as_str).collect();
        want.sort_unstable();
        assert_eq!(v1, want, "{} v1: sorted, every file, once", u.unit_name);
        // Item 103: a regular file's row carries its recorded sha256, 64
        // lowercase hex characters, equal to the staged file's own hash.
        for (name, hash) in u.file_names.iter().zip(&u.file_hashes) {
            let row = rows
                .iter()
                .find(|r| r.unit == u.unit_name && r.version == 1 && &r.path == name)
                .unwrap();
            assert_eq!(&row.sha256, hash, "{} v1 {name}", u.unit_name);
            assert_eq!(row.sha256.len(), 64, "{name}: hex, not a prefix");
            assert!(
                row.sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "{name}: lowercase hex: {}",
                row.sha256
            );
        }
        let v2: Vec<&str> = rows
            .iter()
            .filter(|r| r.unit == u.unit_name && r.version == 2)
            .map(|r| r.path.as_str())
            .collect();
        if &u.unit_name == history_unit {
            want.push("history-only.dat");
            want.sort_unstable();
            assert_eq!(v2, want, "{} v2", u.unit_name);
        } else {
            assert!(
                v2.is_empty(),
                "{} has no second version listed",
                u.unit_name
            );
        }
    }
    assert!(rows.iter().all(|r| r.kind == "f"));
    // A path with no hash recorded states none (the column is empty, not a
    // placeholder), and the control: it is the only such path.
    let unhashed: Vec<&str> = rows
        .iter()
        .filter(|r| r.sha256.is_empty())
        .map(|r| r.path.as_str())
        .collect();
    assert_eq!(unhashed, vec!["history-only.dat"]);
}

/// The index reaches the leak scan's three claims at once: it is in the
/// operator envelope, in the clear nowhere, and a scan of the plaintext zones
/// for the labels it names (the history volumes' needles, planted in the
/// catalog) finds nothing there. The scan itself is
/// `no_plaintext_zone_reveals_a_tenant_unit_file_hash_or_key`'s; this is its
/// positive control that the needles ARE on the tape, encrypted.
#[test]
fn the_history_labels_are_on_the_tape_only_inside_the_operator_envelope() {
    let root = tempfile::tempdir().unwrap();
    let fixtures = vec![
        needle_unit(root.path(), NEEDLE_UNIT_A, &NEEDLE_FILES_A),
        needle_unit(root.path(), NEEDLE_UNIT_B, &NEEDLE_FILES_B),
    ];
    let spec = HarnessSpec {
        operator: NEEDLE_OPERATOR,
        tenants: [NEEDLE_TENANT_A, NEEDLE_TENANT_B],
        real_plaintext_hash: false,
        escrow: true,
        history: [NEEDLE_VOLUME_A, NEEDLE_VOLUME_B],
    };
    let h = build_sealed_harness_from(&fixtures, &spec, VOLUME_UUID_NEEDLES);
    let labels = [NEEDLE_VOLUME_A.to_string(), NEEDLE_VOLUME_B.to_string()];

    // Not a byte of any file holds a label in the clear...
    for (pos, data) in h.store.files.iter().enumerate() {
        assert!(
            find_needles(data, &labels).is_empty(),
            "File {pos} holds a label in the clear"
        );
    }
    // ...and decrypted, the operator's map holds both, each tenant's own one.
    let entries = recorded_entries(&h);
    let op: age::x25519::Identity = h.operator.secret_key.parse().unwrap();
    let primary = entries
        .iter()
        .find(|e| e.type_label == "operator_envelope")
        .unwrap();
    let members = envelope_members(&h.store.files, primary, &op).unwrap();
    let map = member(&members, "index/units.tsv").unwrap();
    assert_eq!(find_needles(map.as_bytes(), &labels).len(), 2);
    // Names unique to one tenant (this fixture's files and units are):
    // tenant A's paths hold A's files and none of B's, and the other way
    // round, and the operator's map holds no path at all.
    let files_a: Vec<String> = NEEDLE_FILES_A.iter().map(|f| f.to_string()).collect();
    let files_b: Vec<String> = NEEDLE_FILES_B.iter().map(|f| f.to_string()).collect();
    let units_ab = [NEEDLE_UNIT_A.to_string(), NEEDLE_UNIT_B.to_string()];
    for (i, t) in h.tenants.iter().enumerate() {
        let identity: age::x25519::Identity = t.secret_key.parse().unwrap();
        let primary = entries
            .iter()
            .filter(|e| e.type_label == "tenant_envelope")
            .find(|e| open_recorded_envelope(&h.store.files, e, &identity).is_some())
            .unwrap();
        let members = envelope_members(&h.store.files, primary, &identity).unwrap();
        let units = member(&members, "index/units.tsv").unwrap();
        let paths = member(&members, "index/paths.tsv").unwrap();
        assert_eq!(
            find_needles(units.as_bytes(), &labels),
            [labels[i].as_str()],
            "{}'s index names exactly its own history tape",
            t.name
        );
        let (mine, theirs) = if i == 0 {
            (&files_a, &files_b)
        } else {
            (&files_b, &files_a)
        };
        assert_eq!(find_needles(paths.as_bytes(), mine).len(), mine.len());
        assert!(
            find_needles(paths.as_bytes(), theirs).is_empty(),
            "{}'s paths",
            t.name
        );
        assert_eq!(
            find_needles(units.as_bytes(), &units_ab),
            [units_ab[i].as_str()],
            "{}'s units",
            t.name
        );
        assert!(find_needles(paths.as_bytes(), &units_ab[1 - i..2 - i]).is_empty());
    }
    let all_files: Vec<String> = files_a.iter().chain(&files_b).cloned().collect();
    assert!(
        find_needles(map.as_bytes(), &all_files).is_empty(),
        "the operator's map names a path"
    );
    assert_eq!(find_needles(map.as_bytes(), &units_ab).len(), 2);
}
