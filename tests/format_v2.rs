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

use std::sync::OnceLock;

use sha2::{Digest, Sha256};

use common::{generate_library, MicroSpec, UnitFixture, MICRO_BLOCK};
use tapectl::crypto::keys::generate_keypair;
use tapectl::db;
use tapectl::staging;
use tapectl::store::{MemStore, Tier};
use tapectl::volume::build::{self, BuildInputs, BuildSlice, BuildUnit, TenantInfo};
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
    #[allow(dead_code)] // read starting with assertion 7 (keyed heir restore)
    tenant_id: i64,
    #[allow(dead_code)] // read starting with assertion 7
    name: String,
    public_key: String,
    #[allow(dead_code)] // read starting with assertion 7
    secret_key: String,
}

/// One unit's ground truth, kept so the final assertions can check restored
/// bytes against what the fixture generator actually produced.
#[allow(dead_code)] // fields read starting with assertion 7 (keyed heir restore)
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
    #[allow(dead_code)] // read starting with assertion 7
    tenants: Vec<TenantFixture>,
    #[allow(dead_code)] // read starting with assertion 7
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
    let fixtures = generate_library(root.path(), &MicroSpec { n_units, seed });

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
