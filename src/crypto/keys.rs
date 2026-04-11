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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn generate_keypair_produces_age_keys() {
        let kp = generate_keypair();
        assert!(kp.public_key.starts_with("age1"));
        assert!(kp.secret_key.starts_with("AGE-SECRET-KEY-"));
        assert_eq!(kp.fingerprint, kp.public_key);
    }

    #[test]
    fn generate_keypair_produces_distinct_keys() {
        let a = generate_keypair();
        let b = generate_keypair();
        assert_ne!(a.public_key, b.public_key);
        assert_ne!(a.secret_key, b.secret_key);
    }

    #[test]
    fn public_key_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("k.age.pub");
        let kp = generate_keypair();
        save_public_key(&path, &kp.public_key).unwrap();
        let read = read_public_key(&path).unwrap();
        assert_eq!(read, kp.public_key);
    }

    #[test]
    fn secret_key_round_trip_and_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("k.age.key");
        let kp = generate_keypair();
        save_secret_key(&path, &kp.secret_key).unwrap();
        let read = read_secret_key(&path).unwrap();
        assert_eq!(read, kp.secret_key);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secret key file must be 0600");
    }

    #[test]
    fn read_public_key_rejects_non_age() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.pub");
        fs::write(&path, "not a real key\n").unwrap();
        assert!(read_public_key(&path).is_err());
    }

    #[test]
    fn read_secret_key_rejects_file_without_marker() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.key");
        fs::write(&path, "# comment\nno key here\n").unwrap();
        let err = read_secret_key(&path).unwrap_err();
        assert!(matches!(err, TapectlError::Encryption(_)));
    }

    #[test]
    fn read_secret_key_on_missing_file_errors() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ghost.key");
        assert!(read_secret_key(&path).is_err());
    }

    #[test]
    fn read_secret_key_tolerates_surrounding_lines() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("wrapped.key");
        let kp = generate_keypair();
        let content = format!("# created: now\n# alias: primary\n{}\n", kp.secret_key);
        fs::write(&path, content).unwrap();
        let read = read_secret_key(&path).unwrap();
        assert_eq!(read, kp.secret_key);
    }

    #[test]
    fn key_paths_derivation() {
        let dir = Path::new("/tmp/keys");
        let (pub_p, sec_p) = key_paths(dir, "alice", "primary");
        assert_eq!(pub_p, Path::new("/tmp/keys/alice-primary.age.pub"));
        assert_eq!(sec_p, Path::new("/tmp/keys/alice-primary.age.key"));
    }

    #[test]
    fn generate_and_save_refuses_overwrite() {
        let tmp = TempDir::new().unwrap();
        generate_and_save(tmp.path(), "alice", "primary").unwrap();
        let err = generate_and_save(tmp.path(), "alice", "primary")
            .err()
            .unwrap();
        assert!(matches!(err, TapectlError::KeyAlreadyExists(_)));
    }
}
