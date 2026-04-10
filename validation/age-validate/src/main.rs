//! Milestone 0: age crate validation
//!
//! Tests from the design document:
//! - Generate X25519 keypair
//! - Multi-recipient encrypt (2 recipients)
//! - Decrypt with each recipient independently
//! - Streaming encrypt/decrypt of 1 GB file
//! - Verify age CLI interop (encrypt with crate, decrypt with `age` binary)

use std::io::{Read, Write};
use std::process::Command;
use std::time::Instant;

use age::secrecy::ExposeSecret;
use age::x25519;
use sha2::{Digest, Sha256};

fn main() {
    println!("=== Milestone 0: age crate validation ===\n");

    let mut passed = 0;
    let mut failed = 0;

    if test_keypair_generation() {
        passed += 1;
    } else {
        failed += 1;
    }

    if test_multi_recipient_encrypt_decrypt() {
        passed += 1;
    } else {
        failed += 1;
    }

    if test_streaming_large_file() {
        passed += 1;
    } else {
        failed += 1;
    }

    if test_cli_interop() {
        passed += 1;
    } else {
        failed += 1;
    }

    println!("\n=== Results: {passed} passed, {failed} failed ===");
    if failed > 0 {
        std::process::exit(1);
    }
}

fn test_keypair_generation() -> bool {
    print!("test: keypair generation ... ");
    let id = x25519::Identity::generate();
    let pubkey = id.to_public();
    let pubkey_str = pubkey.to_string();
    let secret_str = id.to_string().expose_secret().to_string();

    if !pubkey_str.starts_with("age1") {
        println!("FAIL - public key doesn't start with age1: {pubkey_str}");
        return false;
    }
    if !secret_str.starts_with("AGE-SECRET-KEY-") {
        println!("FAIL - secret key doesn't start with AGE-SECRET-KEY-");
        return false;
    }

    // Verify round-trip: parse the secret key string back
    let parsed: x25519::Identity = secret_str.parse().expect("failed to parse secret key");
    let repub = parsed.to_public().to_string();
    if repub != pubkey_str {
        println!("FAIL - round-trip public key mismatch: {repub} != {pubkey_str}");
        return false;
    }

    println!("ok");
    println!("  public:  {pubkey_str}");
    println!("  secret:  AGE-SECRET-KEY-...(len={})", secret_str.len());
    true
}

fn test_multi_recipient_encrypt_decrypt() -> bool {
    print!("test: multi-recipient encrypt + independent decrypt ... ");

    let id_a = x25519::Identity::generate();
    let id_b = x25519::Identity::generate();
    let pub_a = id_a.to_public();
    let pub_b = id_b.to_public();

    let plaintext = b"tapectl milestone 0 multi-recipient test payload";

    // Encrypt to both recipients
    let recipients: Vec<Box<dyn age::Recipient + Send>> =
        vec![Box::new(pub_a.clone()), Box::new(pub_b.clone())];

    let encrypted = {
        let encryptor = age::Encryptor::with_recipients(
            recipients.iter().map(|r| r.as_ref() as &dyn age::Recipient),
        )
        .expect("failed to create encryptor");
        let mut output = vec![];
        let mut writer = encryptor
            .wrap_output(&mut output)
            .expect("failed to wrap output");
        writer.write_all(plaintext).expect("failed to write");
        writer.finish().expect("failed to finish");
        output
    };

    // Decrypt with recipient A
    let decrypted_a = decrypt_with_identity(&encrypted, &id_a);
    if decrypted_a.as_deref() != Some(plaintext.as_slice()) {
        println!("FAIL - decrypt with recipient A failed");
        return false;
    }

    // Decrypt with recipient B
    let decrypted_b = decrypt_with_identity(&encrypted, &id_b);
    if decrypted_b.as_deref() != Some(plaintext.as_slice()) {
        println!("FAIL - decrypt with recipient B failed");
        return false;
    }

    println!("ok");
    println!(
        "  encrypted {} bytes -> {} bytes",
        plaintext.len(),
        encrypted.len()
    );
    println!("  decrypted independently with both recipients");
    true
}

fn test_streaming_large_file() -> bool {
    // Use 100MB for fast validation; set LARGE_FILE_MB=1024 for 1GB production test.
    let size_mb: usize = std::env::var("LARGE_FILE_MB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let size_bytes = size_mb * 1024 * 1024;

    print!("test: streaming encrypt/decrypt of {size_mb} MB ... ");

    let id = x25519::Identity::generate();
    let pubkey = id.to_public();

    let start = Instant::now();

    // Generate deterministic data and compute its hash
    let mut source_hasher = Sha256::new();
    let chunk = vec![0xABu8; 1024 * 1024]; // 1MB chunks

    // Encrypt
    let recipients: Vec<Box<dyn age::Recipient + Send>> = vec![Box::new(pubkey)];
    let encryptor = age::Encryptor::with_recipients(
        recipients.iter().map(|r| r.as_ref() as &dyn age::Recipient),
    )
    .expect("failed to create encryptor");

    let mut encrypted = vec![];
    let mut writer = encryptor
        .wrap_output(&mut encrypted)
        .expect("failed to wrap output");

    for _ in 0..size_mb {
        writer.write_all(&chunk).expect("failed to write chunk");
        source_hasher.update(&chunk);
    }
    writer.finish().expect("failed to finish encryption");

    let encrypt_elapsed = start.elapsed();
    let source_hash = hex_encode(source_hasher.finalize().as_slice());

    // Decrypt
    let decrypt_start = Instant::now();
    let decryptor =
        age::Decryptor::new(&encrypted[..]).expect("failed to create decryptor");

    let mut reader = decryptor
        .decrypt(std::iter::once(&id as &dyn age::Identity))
        .expect("failed to decrypt");

    let mut decrypt_hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut total_read = 0;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                decrypt_hasher.update(&buf[..n]);
                total_read += n;
            }
            Err(e) => {
                println!("FAIL - read error: {e}");
                return false;
            }
        }
    }
    let decrypt_elapsed = decrypt_start.elapsed();
    let decrypt_hash = hex_encode(decrypt_hasher.finalize().as_slice());

    if total_read != size_bytes {
        println!("FAIL - size mismatch: expected {size_bytes}, got {total_read}");
        return false;
    }
    if source_hash != decrypt_hash {
        println!("FAIL - hash mismatch");
        return false;
    }

    let encrypt_speed = size_mb as f64 / encrypt_elapsed.as_secs_f64();
    let decrypt_speed = size_mb as f64 / decrypt_elapsed.as_secs_f64();

    println!("ok");
    println!(
        "  {size_mb} MB -> {} MB encrypted",
        encrypted.len() / (1024 * 1024)
    );
    println!("  encrypt: {encrypt_elapsed:.1?} ({encrypt_speed:.0} MB/s)");
    println!("  decrypt: {decrypt_elapsed:.1?} ({decrypt_speed:.0} MB/s)");
    println!("  sha256 match: {}...", &source_hash[..16]);
    true
}

fn test_cli_interop() -> bool {
    print!("test: age CLI interop ... ");

    // Check if age binary is available
    let age_bin = match which_age() {
        Some(path) => path,
        None => {
            println!("SKIP - age CLI binary not found in PATH");
            println!("  install with: apt install age  or  cargo install rage");
            return true; // non-fatal skip
        }
    };

    let id = x25519::Identity::generate();
    let secret_str = id.to_string().expose_secret().to_string();

    let plaintext = b"age CLI interop test tapectl milestone 0";

    // Encrypt with the Rust crate
    let recipients: Vec<Box<dyn age::Recipient + Send>> = vec![Box::new(id.to_public())];
    let encryptor = age::Encryptor::with_recipients(
        recipients.iter().map(|r| r.as_ref() as &dyn age::Recipient),
    )
    .expect("failed to create encryptor");
    let mut encrypted = vec![];
    let mut writer = encryptor
        .wrap_output(&mut encrypted)
        .expect("failed to wrap output");
    writer.write_all(plaintext).expect("failed to write");
    writer.finish().expect("failed to finish");

    // Write encrypted data and key to temp files
    let dir = tempfile::tempdir().expect("failed to create tempdir");
    let enc_path = dir.path().join("test.age");
    let key_path = dir.path().join("key.txt");
    std::fs::write(&enc_path, &encrypted).expect("failed to write encrypted file");
    std::fs::write(&key_path, &secret_str).expect("failed to write key file");

    // Decrypt with age CLI
    let output = Command::new(&age_bin)
        .arg("-d")
        .arg("-i")
        .arg(&key_path)
        .arg(&enc_path)
        .output();

    match output {
        Ok(result) if result.status.success() => {
            if result.stdout == plaintext {
                println!("ok");
                println!("  encrypted with crate, decrypted with {age_bin}");
                true
            } else {
                println!("FAIL - decrypted content doesn't match");
                false
            }
        }
        Ok(result) => {
            println!("FAIL - age CLI exited with {}", result.status);
            println!("  stderr: {}", String::from_utf8_lossy(&result.stderr));
            false
        }
        Err(e) => {
            println!("FAIL - failed to run age CLI: {e}");
            false
        }
    }
}

fn decrypt_with_identity(encrypted: &[u8], id: &x25519::Identity) -> Option<Vec<u8>> {
    let decryptor = age::Decryptor::new(encrypted).ok()?;

    let mut reader = decryptor
        .decrypt(std::iter::once(id as &dyn age::Identity))
        .ok()?;

    let mut output = vec![];
    reader.read_to_end(&mut output).ok()?;
    Some(output)
}

fn which_age() -> Option<String> {
    for name in ["age", "rage"] {
        if let Ok(output) = Command::new("which").arg(name).output() {
            if output.status.success() {
                return Some(String::from_utf8_lossy(&output.stdout).trim().to_string());
            }
        }
    }
    None
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
