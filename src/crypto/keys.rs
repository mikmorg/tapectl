use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use age::secrecy::ExposeSecret;
use age::x25519;

use crate::error::{Result, TapectlError};

/// Generated keypair with public key, secret key string, and fingerprint.
pub struct GeneratedKeypair {
    pub public_key: String,
    pub secret_key: String,
    /// Fingerprint: the full public key string (age1...) serves as the fingerprint.
    pub fingerprint: String,
}

/// Generate an X25519 age keypair.
pub fn generate_keypair() -> GeneratedKeypair {
    let secret = x25519::Identity::generate();
    let public = secret.to_public();
    let public_str = public.to_string();
    let secret_str = secret.to_string().expose_secret().to_string();

    GeneratedKeypair {
        fingerprint: public_str.clone(),
        public_key: public_str,
        secret_key: secret_str,
    }
}

/// Save a public key to a file.
pub fn save_public_key(path: &Path, public_key: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, format!("{public_key}\n"))?;
    Ok(())
}

/// Save a secret key to a file with restrictive permissions.
pub fn save_secret_key(path: &Path, secret_key: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(format!("{secret_key}\n").as_bytes())?;
    Ok(())
}

/// Read a public key from a file.
pub fn read_public_key(path: &Path) -> Result<String> {
    let content = fs::read_to_string(path)?;
    let key = content.trim().to_string();
    if !key.starts_with("age1") {
        return Err(TapectlError::Encryption(format!(
            "invalid public key in {}: does not start with age1",
            path.display()
        )));
    }
    Ok(key)
}

/// Read a secret key from a file.
pub fn read_secret_key(path: &Path) -> Result<String> {
    let content = fs::read_to_string(path)?;
    let key = content
        .lines()
        .find(|l| l.starts_with("AGE-SECRET-KEY-"))
        .ok_or_else(|| {
            TapectlError::Encryption(format!("no secret key found in {}", path.display()))
        })?
        .trim()
        .to_string();
    Ok(key)
}

/// Derive the key file paths for a given tenant + alias.
pub fn key_paths(
    keys_dir: &Path,
    tenant_name: &str,
    alias: &str,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let base = format!("{tenant_name}-{alias}");
    let pub_path = keys_dir.join(format!("{base}.age.pub"));
    let key_path = keys_dir.join(format!("{base}.age.key"));
    (pub_path, key_path)
}

/// Generate a keypair, save to disk, and return the generated data.
pub fn generate_and_save(
    keys_dir: &Path,
    tenant_name: &str,
    alias: &str,
) -> Result<GeneratedKeypair> {
    let kp = generate_keypair();
    let (pub_path, key_path) = key_paths(keys_dir, tenant_name, alias);

    if pub_path.exists() || key_path.exists() {
        return Err(TapectlError::KeyAlreadyExists(format!(
            "{tenant_name}-{alias}"
        )));
    }

    save_public_key(&pub_path, &kp.public_key)?;
    save_secret_key(&key_path, &kp.secret_key)?;

    Ok(kp)
}
