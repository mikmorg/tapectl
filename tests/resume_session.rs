//! Cross-process resume of an interrupted volume write (issue #25, playbook
//! T8 remainder).
//!
//! `session.rs`'s own unit tests already cover resume *within one process*:
//! they hold the `InterruptedSession` value that `execute_checking` returned
//! and call `.resume()` on it directly. That can never fail the way the real
//! operational path fails, because the in-memory `BuiltLayout` — and with it
//! the frozen ID-thunk/front-index bytes — is still right there.
//!
//! This suite covers the path that actually matters: the process is GONE. The
//! only things that survive an interrupted `tapectl volume write` are the
//! SQLite rows and the frozen session staging directory. Everything is
//! rehydrated from those two, and nothing else.
//!
//! Why rehydration rather than calling `build()` again — the load-bearing
//! fact this suite pins:
//!
//! * `BuildInputs::created_at` is `chrono::Utc::now()` at `volume_write` call
//!   time and is persisted nowhere. A second `build()` produces a different
//!   `created_at`, therefore different ID-thunk bytes, therefore a different
//!   sha256 for File 0.
//! * `BuildInputs::mam_loads` comes from `tape::mam::read_mam`'s `load_count`,
//!   which increments on every cartridge load. Any resume that involved
//!   reloading the tape sees a different value — again, different ID-thunk
//!   bytes.
//!
//! `SealedPending::confirm` diffs the front index read back off the medium
//! against the Layout it holds. A Layout rebuilt with drifted `created_at` /
//! `mam_loads` would therefore report a File-0 (and File-3) mismatch and
//! QUARANTINE a perfectly good tape. That is silent process corruption, not a
//! loud failure — which is exactly what
//! `resume_after_restart_seals_rather_than_quarantines` asserts against.
//!
//! `layout_model.rs`'s `ContentSource` doc comment states the same conclusion
//! ("the ID thunk and seal marker embed real timestamps, so regenerating them
//! on resume would silently produce different bytes than what is already on
//! tape"), and `docs/design/layout-session.md`'s Resume rule says the frozen
//! generated zones "re-hash byte-identical" — re-hash the frozen files, not
//! re-generate them.
//!
//! `MemStore` stands in for the cartridge: it is the one thing deliberately
//! NOT dropped between the interruption and the resume, because a physical
//! tape is not dropped either.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

use tapectl::db;
use tapectl::store::{MemStore, Tier};
use tapectl::volume::build::{self, BuildInputs, BuildSlice, BuildUnit, BuiltLayout, TenantInfo};
use tapectl::volume::layout_model::KeyAvailability;
use tapectl::volume::session::{
    ConfirmOutcome, ExecuteOutcome, InterruptedSession, QuarantineReason, ResumeOutcome,
};

const BS: u64 = 512 * 1024;

fn sha_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Everything that survives a process restart, plus the temp guards that keep
/// it on disk for the duration of the test.
struct Fixture {
    db_path: PathBuf,
    volume_id: i64,
    units: Vec<BuildUnit>,
    built: BuiltLayout,
    _db_dir: tempfile::TempDir,
    _slices_dir: tempfile::TempDir,
    _session_dir: tempfile::TempDir,
}

/// A minimal but REAL v2 write-session fixture: a temp-file DB opened through
/// the production `db::open` (so every migration and the crash-recovery sweep
/// run exactly as they do for the CLI), one operator tenant, one content
/// tenant with a unit/snapshot/stage_set and two staged slices whose on-disk
/// bytes match their recorded hashes, one volume row, and the `BuiltLayout`
/// that `build()` produced from them.
///
/// Modelled on `session.rs`'s in-module `make_fixture`, but deliberately NOT
/// shared with it: that one uses `db::open_memory` (`#[cfg(test)]`-gated, so
/// invisible to an integration test) and, more importantly, an in-memory DB
/// cannot express this suite's whole premise — a second `Connection` opened
/// against the same durable database after the first one is gone.
/// `format_v2.rs`'s `build_sealed_harness` is likewise unusable here: it
/// drives every session to `Sealed` by construction, and this suite needs one
/// stopped mid-flight.
fn make_fixture() -> Fixture {
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("resume.db");
    let conn = db::open(&db_path).unwrap();

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('operator', 1, 'active')",
        [],
    )
    .unwrap();
    let operator_id = conn.last_insert_rowid();
    let op_key = tapectl::crypto::keys::generate_keypair();
    conn.execute(
        "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key, key_type, is_active)
         VALUES (?1, 'operator-primary', ?2, ?3, 'primary', 1)",
        params![operator_id, op_key.fingerprint, op_key.public_key],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('alpha', 0, 'active')",
        [],
    )
    .unwrap();
    let tenant_id = conn.last_insert_rowid();
    let tenant_key = tapectl::crypto::keys::generate_keypair();
    conn.execute(
        "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key, key_type, is_active)
         VALUES (?1, 'alpha-primary', ?2, ?3, 'primary', 1)",
        params![tenant_id, tenant_key.fingerprint, tenant_key.public_key],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, current_path, status)
         VALUES ('unit-uuid-1', 'unit-alpha', ?1, '/tmp/unit-alpha', 'active')",
        params![tenant_id],
    )
    .unwrap();
    let unit_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
         VALUES (?1, 1, 'staged', '/tmp/unit-alpha', 1, 32)",
        params![unit_id],
    )
    .unwrap();
    let snapshot_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
        params![snapshot_id],
    )
    .unwrap();
    let stage_set_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES ('RESUMETEST', 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
        [],
    )
    .unwrap();
    let volume_id = conn.last_insert_rowid();

    let slices_dir = tempfile::tempdir().unwrap();
    let slice_1 = fake_slice(
        &conn,
        slices_dir.path(),
        stage_set_id,
        1,
        b"first staged slice bytes",
    );
    let slice_2 = fake_slice(
        &conn,
        slices_dir.path(),
        stage_set_id,
        2,
        b"second staged slice bytes, a bit longer",
    );

    let build_unit = BuildUnit {
        stage_set_id,
        snapshot_id,
        unit_name: "unit-alpha".to_string(),
        unit_uuid: "unit-uuid-1".to_string(),
        tenant_id,
        dar_version: Some("2.7.20".to_string()),
        dar_command: Some("dar -c base -R /src".to_string()),
        catalog_path: None,
        snapshot_version: 1,
        slices: vec![slice_1, slice_2],
    };

    let inputs = BuildInputs {
        label: "RESUMETEST".to_string(),
        volume_uuid: "550e8400-e29b-41d4-a716-446655440000".to_string(),
        media_type: "LTO-6".to_string(),
        tapectl_version: "0.1.0-test".to_string(),
        // The value whose non-reproducibility is the whole reason this suite
        // exists: in production this is `chrono::Utc::now()` and is persisted
        // nowhere. Pinned here only so the fixture is deterministic.
        created_at: "2026-07-22T20:09:00Z".to_string(),
        block_size: BS,
        usable_bytes: 1000 * BS,
        enospc_buffer: BS,
        nominal_capacity: 2_500_000_000_000,
        mam_capacity: 0,
        mam_manufacturer: String::new(),
        mam_serial: String::new(),
        mam_length: 0,
        // Likewise: `read_mam`'s load_count increments on every cartridge
        // load, so a resume that reloaded the tape could never reproduce it.
        mam_loads: 0,
        units: vec![build_unit.clone()],
        tenants: vec![TenantInfo {
            tenant_id,
            tenant_name: "alpha".to_string(),
            public_keys: vec![tenant_key.public_key],
        }],
        operator_public_keys: vec![op_key.public_key],
        escrow_public_key: None,
    };

    let session_dir = tempfile::tempdir().unwrap();
    let built = build::build(&inputs, session_dir.path()).unwrap();

    drop(conn);

    Fixture {
        db_path,
        volume_id,
        units: vec![build_unit],
        built,
        _db_dir: db_dir,
        _slices_dir: slices_dir,
        _session_dir: session_dir,
    }
}

fn fake_slice(
    conn: &Connection,
    dir: &Path,
    stage_set_id: i64,
    slice_number: i64,
    content: &[u8],
) -> BuildSlice {
    let sha_plain = sha_hex(b"plaintext hash is not exercised by this fixture");
    let sha_enc = sha_hex(content);
    conn.execute(
        "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                    sha256_plain, sha256_encrypted)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            stage_set_id,
            slice_number,
            content.len() as i64,
            content.len() as i64,
            sha_plain,
            sha_enc,
        ],
    )
    .unwrap();
    let slice_id = conn.last_insert_rowid();

    let path = dir.join(format!("slice_{slice_id}.age"));
    std::fs::File::create(&path)
        .unwrap()
        .write_all(content)
        .unwrap();
    conn.execute(
        "UPDATE stage_slices SET staging_path = ?1 WHERE id = ?2",
        params![path.to_string_lossy(), slice_id],
    )
    .unwrap();

    BuildSlice {
        slice_id,
        slice_number,
        size_bytes: content.len() as i64,
        encrypted_bytes: content.len() as i64,
        sha256_plain: sha_plain,
        sha256_encrypted: sha_enc,
        staging_path: path,
    }
}

/// Reassemble `KeyAvailability` the way a restarted CLI process must: from
/// the DB and the rehydrated Layout's own tenant-envelope entries
/// (`KeyAvailability::tenant_ids` is documented as "every tenant that has an
/// envelope on this volume"), never from a value carried over in memory.
fn rebuild_keys(conn: &Connection, tenant_ids: Vec<i64>) -> KeyAvailability {
    let mut with_key = HashSet::new();
    for &t in &tenant_ids {
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM encryption_keys WHERE tenant_id = ?1 AND is_active = 1",
                params![t],
                |r| r.get(0),
            )
            .unwrap();
        if n > 0 {
            with_key.insert(t);
        }
    }
    KeyAvailability {
        tenant_ids,
        tenants_with_active_key: with_key,
        operator_key_present: true,
        escrow_recipient_present: None,
    }
}

/// Drive the fixture to `ExecuteOutcome::Interrupted`, firing the injected
/// predicate once `calls_before` between-entry checks have already passed —
/// i.e. after exactly `calls_before` entries have landed on the store.
/// Returns the store, which is the one value that must survive (it is the
/// cartridge).
fn interrupt_after(f: &Fixture, calls_before: u32) -> MemStore {
    let conn = db::open(&f.db_path).unwrap();
    let mut store = MemStore::new(BS as usize);

    let validated = f
        .built
        .clone()
        .into_validated(&rebuild_keys(&conn, vec![f.units[0].tenant_id]), &mut store)
        .expect("validate should pass for a well-formed fixture");
    let planned = validated.plan(&conn, f.volume_id, &f.units).unwrap();

    let calls = AtomicU32::new(0);
    let is_interrupted = move || calls.fetch_add(1, Ordering::SeqCst) >= calls_before;

    match planned
        .execute_checking(&conn, &mut store, is_interrupted)
        .expect("execute_checking should not error on a clean interruption")
    {
        ExecuteOutcome::Interrupted(session) => {
            // Drop EVERY in-memory session value: this is the process exiting.
            // Nothing below may consult anything but the DB and the frozen
            // session directory.
            drop(session);
        }
        ExecuteOutcome::Ready(_) => panic!("expected Interrupted, got Ready"),
        ExecuteOutcome::Aborted(a) => panic!("expected Interrupted, got Aborted: {}", a.reason),
    }
    drop(conn);

    store
}

/// The regression that matters: after a full process restart, the rehydrated
/// Layout must reproduce the File-0 / File-3 bytes ALREADY on the medium, so
/// confirm's diff passes and the tape seals. A resume that re-ran `build()`
/// would drift `created_at`/`mam_loads`, and this assertion would see
/// `Quarantined` — a good tape condemned by the software.
#[test]
fn resume_after_restart_seals_rather_than_quarantines() {
    let f = make_fixture();

    // The fixture's content entries (seal excluded) are, in order: id_thunk,
    // system_guide, restore_sh, front_index, tenant_envelope,
    // operator_envelope, operator_envelope_backup, slice_1, slice_2. Firing
    // on check #9 (0-indexed >= 8) stops AFTER slice_1 landed and BEFORE
    // slice_2 is opened — one slice written, which is the two-case cursor
    // rule's "reposition and continue" branch.
    let mut store = interrupt_after(&f, 8);
    assert_eq!(
        store.files.len(),
        8,
        "8 entries landed before the interrupt"
    );

    // --- the process restarts: a fresh Connection is all we carry forward ---
    let conn = db::open(&f.db_path).unwrap();

    let written: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM write_positions WHERE status = 'written'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(written, 1, "exactly one slice recorded written");

    let session = InterruptedSession::rehydrate(&conn, f.volume_id)
        .expect("rehydrate should not error")
        .expect("an interrupted session for this volume should be resumable");

    let tenant_ids = vec![f.units[0].tenant_id];
    let keys = rebuild_keys(&conn, tenant_ids);

    let ready = match session
        .resume(&conn, &keys, &mut store)
        .expect("resume should not error")
    {
        ResumeOutcome::Ready(r) => r,
        ResumeOutcome::Quarantined(q) => panic!(
            "a rehydrated resume must NOT quarantine a good tape — that is the \
             regenerate-instead-of-rehydrate bug: {:?}",
            q.reason
        ),
        ResumeOutcome::Interrupted(_) => panic!("expected Ready, got Interrupted"),
        ResumeOutcome::Aborted(a) => panic!("expected Ready, got Aborted: {}", a.reason),
    };

    let sealed_pending = ready.seal(&mut store).expect("seal should succeed");
    match sealed_pending
        .confirm(&conn, &mut store, Tier::Integrity)
        .expect("confirm should not error")
    {
        ConfirmOutcome::Sealed(s) => assert_eq!(s.label, "RESUMETEST"),
        ConfirmOutcome::Quarantined(q) => panic!(
            "confirm quarantined a tape written by this very session — the rehydrated \
             Layout does not reproduce the on-tape bytes: {}",
            match q.reason {
                QuarantineReason::ConfirmFailed(e) => format!("{:?}", e.mismatches),
                other => format!("{other:?}"),
            }
        ),
    }

    let volume_status: String = conn
        .query_row(
            "SELECT status FROM volumes WHERE id = ?1",
            params![f.volume_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(volume_status, "sealed");
    let write_status: String = conn
        .query_row(
            "SELECT status FROM writes WHERE volume_id = ?1",
            params![f.volume_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(write_status, "completed");
    let written: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM write_positions WHERE status = 'written'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(written, 2, "both slices written after the resume");

    // Sacred: `layout.json` is a staging-side artifact. It is never a
    // LayoutEntry and never reaches the medium — the store holds exactly the
    // Layout's entries, no more.
    let entry_count = f.built.layout.entries.len();
    assert_eq!(store.files.len(), entry_count);
}

/// The two-case cursor rule's other branch, across a process restart: with
/// ZERO slices recorded `written`, resume restarts from BOT — the front zone
/// re-writes byte-identically from the frozen staging files.
#[test]
fn resume_with_zero_slices_written_restarts_from_bot() {
    let f = make_fixture();

    // Stop after 5 entries (id_thunk .. operator_envelope): plenty written,
    // but the first slice is at index 7, so no slice has a 'written' cursor
    // row. Per the cursor rule that means restart from BOT, not "continue at
    // 5" — and MemStore's reposition truncates, so the final file count
    // proves the whole layout was re-laid from position 0.
    let mut store = interrupt_after(&f, 5);
    assert_eq!(store.files.len(), 5);

    let conn = db::open(&f.db_path).unwrap();
    let written: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM write_positions WHERE status = 'written'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(written, 0, "no slice was reached before the interrupt");

    let session = InterruptedSession::rehydrate(&conn, f.volume_id)
        .expect("rehydrate should not error")
        .expect("an interrupted session for this volume should be resumable");
    let keys = rebuild_keys(&conn, vec![f.units[0].tenant_id]);

    let ready = match session.resume(&conn, &keys, &mut store).unwrap() {
        ResumeOutcome::Ready(r) => r,
        ResumeOutcome::Quarantined(q) => panic!("unexpected quarantine: {:?}", q.reason),
        ResumeOutcome::Interrupted(_) => panic!("expected Ready, got Interrupted"),
        ResumeOutcome::Aborted(a) => panic!("expected Ready, got Aborted: {}", a.reason),
    };
    let sealed_pending = ready.seal(&mut store).unwrap();
    match sealed_pending
        .confirm(&conn, &mut store, Tier::Integrity)
        .unwrap()
    {
        ConfirmOutcome::Sealed(_) => {}
        ConfirmOutcome::Quarantined(q) => panic!("expected Sealed, got quarantine: {:?}", q.reason),
    }

    assert_eq!(
        store.files.len(),
        f.built.layout.entries.len(),
        "restarting from BOT re-lays every entry exactly once — a resume that had \
         continued from position 5 instead would leave a longer recording"
    );
    let volume_status: String = conn
        .query_row(
            "SELECT status FROM volumes WHERE id = ?1",
            params![f.volume_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(volume_status, "sealed");
}

/// Change 3's acceptance, kept next to the tests that depend on it: `build()`
/// writes a `layout.json` sidecar that round-trips to an equal `Layout`, and
/// that sidecar is NOT part of the Layout (so it can never reach the medium).
#[test]
fn build_writes_a_layout_json_sidecar_that_is_never_a_layout_entry() {
    let f = make_fixture();
    let sidecar = f.built.session_dir.join("layout.json");
    assert!(sidecar.is_file(), "build() must write {sidecar:?}");

    let text = std::fs::read_to_string(&sidecar).unwrap();
    let round_tripped: tapectl::volume::layout_model::Layout = serde_json::from_str(&text).unwrap();
    assert_eq!(round_tripped, f.built.layout);

    for e in &f.built.layout.entries {
        let path = match &e.source {
            tapectl::volume::layout_model::ContentSource::Staged(p)
            | tapectl::volume::layout_model::ContentSource::Materialized(p) => p.clone(),
            tapectl::volume::layout_model::ContentSource::Generated => continue,
        };
        assert_ne!(
            path, sidecar,
            "layout.json is a staging-side artifact and must never be a LayoutEntry"
        );
    }
}
