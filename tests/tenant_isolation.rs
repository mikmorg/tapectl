//! Tenant isolation tests that exercise the crypto boundary without tape.
//!
//! The mhvtl-gated tests in `mhvtl_e2e.rs` prove end-to-end isolation on real
//! volume layouts. These tests pin the underlying guarantee: a tenant cannot
//! decrypt another tenant's age ciphertext, which is what the whole envelope
//! trial-decrypt scheme rests on.

use std::io::Read;

use age::x25519::{Identity, Recipient};
use tapectl::staging::encrypt_data;

fn new_keypair() -> (Identity, Recipient) {
    let id = Identity::generate();
    let recip = id.to_public();
    (id, recip)
}

#[test]
fn cross_tenant_decrypt_is_rejected() {
    let (_alice_id, alice_pub) = new_keypair();
    let (bob_id, _bob_pub) = new_keypair();

    let plaintext = b"alice-only secret manifest";
    let ct = encrypt_data(plaintext, &[alice_pub.to_string()]).unwrap();

    // Bob's key must not decrypt alice's envelope. age rejects at either
    // decryptor construction or stream-read; both count as rejection.
    let result = (|| -> Result<Vec<u8>, String> {
        let decryptor = age::Decryptor::new(&ct[..]).map_err(|e| e.to_string())?;
        let mut reader = decryptor
            .decrypt(std::iter::once(&bob_id as &dyn age::Identity))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        reader.read_to_end(&mut out).map_err(|e| e.to_string())?;
        Ok(out)
    })();
    assert!(
        result.is_err(),
        "bob decrypted alice's ciphertext — isolation broken"
    );
}

#[test]
fn owning_tenant_still_decrypts() {
    let (alice_id, alice_pub) = new_keypair();
    let plaintext = b"alice-only secret manifest";
    let ct = encrypt_data(plaintext, &[alice_pub.to_string()]).unwrap();

    let decryptor = age::Decryptor::new(&ct[..]).unwrap();
    let mut reader = decryptor
        .decrypt(std::iter::once(&alice_id as &dyn age::Identity))
        .unwrap();
    let mut out = Vec::new();
    reader.read_to_end(&mut out).unwrap();
    assert_eq!(out, plaintext);
}

#[test]
fn multi_recipient_each_tenant_decrypts_independently() {
    // Operator envelopes are encrypted to multiple recipients; each recipient
    // must be able to unwrap the same ciphertext with only their own identity.
    let (alice_id, alice_pub) = new_keypair();
    let (bob_id, bob_pub) = new_keypair();
    let plaintext = b"shared operator envelope";

    let ct = encrypt_data(plaintext, &[alice_pub.to_string(), bob_pub.to_string()]).unwrap();

    let identities: [&dyn age::Identity; 2] = [&alice_id, &bob_id];
    for identity in identities {
        let decryptor = age::Decryptor::new(&ct[..]).unwrap();
        let mut reader = decryptor.decrypt(std::iter::once(identity)).unwrap();
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, plaintext);
    }
}

#[test]
fn tampered_ciphertext_fails_to_decrypt() {
    // age authenticates plaintext; a single-byte flip must cause decrypt to
    // fail rather than silently returning garbage.
    let (alice_id, alice_pub) = new_keypair();
    let plaintext = b"bitrot canary";
    let mut ct = encrypt_data(plaintext, &[alice_pub.to_string()]).unwrap();

    // Flip a byte in the body (past the age header).
    let flip_at = ct.len() - 8;
    ct[flip_at] ^= 0xff;

    let result = (|| -> Result<Vec<u8>, String> {
        let decryptor = age::Decryptor::new(&ct[..]).map_err(|e| e.to_string())?;
        let mut reader = decryptor
            .decrypt(std::iter::once(&alice_id as &dyn age::Identity))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        reader.read_to_end(&mut out).map_err(|e| e.to_string())?;
        Ok(out)
    })();
    assert!(result.is_err(), "tampered ciphertext decrypted cleanly");
}

#[test]
fn age_header_is_plaintext_marker() {
    // The test that scans raw tape bytes for leaked tenant names also relies
    // on being able to tell "is this file encrypted?" by matching the age
    // header magic. Pin that marker so the scan test stays honest if age's
    // format ever changes.
    let (_id, pubkey) = new_keypair();
    let ct = encrypt_data(b"x", &[pubkey.to_string()]).unwrap();
    assert!(
        ct.starts_with(b"age-encryption.org/v1"),
        "age header changed — update mhvtl plaintext-leak scan"
    );
}
