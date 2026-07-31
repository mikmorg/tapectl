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
use tapectl::crypto::keys::generate_keypair;
use tapectl::db;
use tapectl::staging;
use tapectl::store::{MemStore, MismatchKind, Store, Tier};
use tapectl::volume::build::{self, BuildInputs, BuildSlice, BuildUnit, BuiltLayout, TenantInfo};
use tapectl::volume::format;
use tapectl::volume::layout_model::{KeyAvailability, Layout, ZoneKind};
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
    secret_key: String,
}

/// One unit's ground truth, kept so the final assertions can check restored
/// bytes against what the fixture generator actually produced.
struct UnitPlain {
    unit_name: String,
    unit_uuid: String,
    tenant_id: i64,
    plaintext: Vec<u8>,
}

/// Everything downstream assertions need from a fully sealed session.
struct Harness {
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
        secret_key: kp.secret_key,
    }
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

    // `db::open_memory` is `#[cfg(test)]`-gated (unit-test-only), so it does
    // not exist in the normal rlib an integration test links against. A real
    // temp-file DB via the unconditionally-`pub` `db::open` is functionally
    // identical (same `configure`/`migrate`/`recover_orphaned_sessions`
    // path) and needs no `src/` change.
    let db_dir = tempfile::tempdir().unwrap();
    let conn = db::open(&db_dir.path().join("t7.db")).unwrap();
    let operator = insert_tenant(&conn, "operator", true);
    let tenant_a = insert_tenant(&conn, "alpha", false);
    let tenant_b = insert_tenant(&conn, "bravo", false);
    let tenants = vec![tenant_a, tenant_b];

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES ('T7MICRO', 'lto', 'lto0', 'LTO-6', 2500000000, 'active')",
        [],
    )
    .unwrap();
    let volume_id = conn.last_insert_rowid();

    let slices_dir = tempfile::tempdir().unwrap();
    let half = fixtures.len() / 2;
    let mut build_units = Vec::with_capacity(fixtures.len());
    let mut units_plain = Vec::with_capacity(fixtures.len());

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
        let recipients = vec![tenant.public_key.clone(), operator.public_key.clone()];
        let encrypted = staging::encrypt_data(&plaintext, &recipients).unwrap();
        // sha256_plain is never read back by any of this suite's assertions
        // or by the validate/execute/confirm code paths under test (only
        // `sha256_encrypted` is — `volume-format-v2.md` §3's front index
        // carries ciphertext hashes only); a placeholder skips a second full
        // ~N MiB debug-mode hash pass per unit purely for test wall-clock,
        // matching `build.rs`'s own test fixture's identical shortcut
        // (`sha_hex(b"plaintext hash is not exercised by this fixture")`).
        let sha_plain = sha256_hex(b"plaintext hash is not exercised by this harness");
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
                sha256_plain: sha_plain,
                sha256_encrypted: sha_enc,
                staging_path: slice_path,
            }],
        });
        units_plain.push(UnitPlain {
            unit_name: fixture.folder_name.clone(),
            unit_uuid,
            tenant_id: tenant.tenant_id,
            plaintext,
        });
    }

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
        escrow_public_key: None,
        catalog_db_path: None,
    };

    let session_dir = tempfile::tempdir().unwrap();
    let built = build::build(&inputs, session_dir.path()).expect("build succeeds");
    // Snapshot BEFORE the typestate chain below consumes `built` — see the
    // `Harness::layout` doc comment; `confirm`'s success arm does not hand
    // the Layout back.
    let layout_snapshot = built.layout.clone();

    let keys = KeyAvailability {
        tenant_ids: tenants.iter().map(|t| t.tenant_id).collect(),
        tenants_with_active_key: tenants.iter().map(|t| t.tenant_id).collect(),
        operator_key_present: true,
        escrow_recipient_present: None,
    };

    let mut store = MemStore::new(BS as usize);
    let validated = built
        .into_validated(&keys, &mut store)
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
    }

    Harness {
        layout: layout_snapshot,
        store,
        tenants,
        units: units_plain,
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
                "tenant_envelope" | "operator_envelope" | "operator_envelope_backup"
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
