//! Failure-mode tests. Fast, pure-Rust, no mhvtl required.
//!
//! These complement the inline unit tests by exercising library-level
//! failure paths end-to-end: age encryption misuse, ciphertext tampering,
//! and DB crash recovery via the real `tapectl::db::open` entry point.

use std::fs;
use std::io::Read;
use std::path::PathBuf;

use rusqlite::params;
use tempfile::TempDir;

use tapectl::crypto::keys;
use tapectl::db;
use tapectl::staging::encrypt_data;

fn decrypt_with(ciphertext: &[u8], secret: &str) -> Result<Vec<u8>, String> {
    let identity: age::x25519::Identity = secret.parse().map_err(|e| format!("parse: {e}"))?;
    let decryptor = age::Decryptor::new(ciphertext).map_err(|e| format!("decryptor: {e}"))?;
    let mut reader = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(|e| format!("decrypt: {e}"))?;
    let mut out = Vec::new();
    reader
        .read_to_end(&mut out)
        .map_err(|e| format!("read: {e}"))?;
    Ok(out)
}

#[test]
fn wrong_key_cannot_decrypt() {
    let alice = keys::generate_keypair();
    let mallory = keys::generate_keypair();

    let plaintext = b"secret archive data";
    let ct = encrypt_data(plaintext, &[alice.public_key.clone()]).unwrap();

    assert_eq!(decrypt_with(&ct, &alice.secret_key).unwrap(), plaintext);
    assert!(
        decrypt_with(&ct, &mallory.secret_key).is_err(),
        "wrong key must not decrypt"
    );
}

#[test]
fn tampered_ciphertext_fails_decrypt() {
    let kp = keys::generate_keypair();
    let plaintext = vec![42u8; 4096];
    let mut ct = encrypt_data(&plaintext, &[kp.public_key.clone()]).unwrap();

    // Flip a byte in the payload region (skip the header — header corruption
    // produces a parse error rather than the authentication failure we want
    // to exercise).
    let idx = ct.len() - 32;
    ct[idx] ^= 0xff;

    assert!(
        decrypt_with(&ct, &kp.secret_key).is_err(),
        "tampered ciphertext must not decrypt"
    );
}

#[test]
fn multi_recipient_either_key_decrypts() {
    let alice = keys::generate_keypair();
    let bob = keys::generate_keypair();

    let plaintext = b"shared archive";
    let ct = encrypt_data(
        plaintext,
        &[alice.public_key.clone(), bob.public_key.clone()],
    )
    .unwrap();

    assert_eq!(decrypt_with(&ct, &alice.secret_key).unwrap(), plaintext);
    assert_eq!(decrypt_with(&ct, &bob.secret_key).unwrap(), plaintext);
}

#[test]
fn encrypt_rejects_malformed_pubkey() {
    let res = encrypt_data(b"data", &["not-an-age-key".to_string()]);
    assert!(res.is_err(), "malformed pubkey must error");
}

#[test]
fn db_crash_recovery_sweeps_orphaned_rows() {
    let tmp = TempDir::new().unwrap();
    let db_path: PathBuf = tmp.path().join("tapectl.db");

    // First open: runs migrations, then seed orphan rows.
    {
        let conn = db::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status)
             VALUES ('op', 1, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, checksum_mode, status)
             VALUES ('u1', 'u', ?1, '/tmp/u', 'mtime_size', 'active')",
            [tid],
        )
        .unwrap();
        let uid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, source_path, file_count, total_size, status)
             VALUES (?1, 1, '/tmp/u', 0, 0, 'created')",
            [uid],
        )
        .unwrap();
        let sid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, slice_size, status)
             VALUES (?1, 524288, 'staging')",
            [sid],
        )
        .unwrap();
        let ssid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
             VALUES ('L1', 'lto', 'mhvtl', 0, 'active')",
            [],
        )
        .unwrap();
        let vid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'in_progress')",
            params![ssid, sid, vid],
        )
        .unwrap();
        // Drop conn at end of scope — simulates crash.
    }

    // Simulate shutdown: remove the WAL + SHM so the next open sees committed state.
    let _ = fs::remove_file(db_path.with_extension("db-wal"));
    let _ = fs::remove_file(db_path.with_extension("db-shm"));

    // Second open: the recovery sweep should promote orphan rows.
    let conn = db::open(&db_path).unwrap();
    let write_status: String = conn
        .query_row("SELECT status FROM writes", [], |r| r.get(0))
        .unwrap();
    let stage_status: String = conn
        .query_row("SELECT status FROM stage_sets", [], |r| r.get(0))
        .unwrap();
    assert_eq!(write_status, "aborted");
    assert_eq!(stage_status, "failed");
}
