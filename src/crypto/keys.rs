use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use age::secrecy::ExposeSecret;
use age::x25519;
use rusqlite::Connection;

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

/// Resolve a public key argument that may be either a literal `age1...`
/// string or a path to a file containing one. `key import --escrow <pubkey>`
/// accepts both: an operator retyping a printed key has no file to point at,
/// while the common case (a `.pub` file from another tool) should still
/// work. Falls back to `read_public_key`'s file-based path and its
/// validation (and its error) when the value doesn't look like a literal key.
pub fn read_or_parse_public_key(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.starts_with("age1") {
        return Ok(trimmed.to_string());
    }
    read_public_key(Path::new(value))
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
    secret_key_line(&content).ok_or_else(|| {
        TapectlError::Encryption(format!("no secret key found in {}", path.display()))
    })
}

/// The secret key line [`read_secret_key`] takes from a key file's text:
/// the first line starting with `AGE-SECRET-KEY-`, trimmed. The one rule
/// every reader of `keys/` uses, so adoption (#424) can check that the file
/// it stores is read back as the identity it validated.
fn secret_key_line(content: &str) -> Option<String> {
    content
        .lines()
        .find(|l| l.starts_with("AGE-SECRET-KEY-"))
        .map(|l| l.trim().to_string())
}

/// The most an identity file offered for adoption, or given with `--key`,
/// may hold. One age identity with age-keygen's comment lines is about 200
/// bytes; anything near this is not an identity file.
const ADOPT_MAX_BYTES: usize = 64 * 1024;

/// An existing age X25519 identity file, validated for adoption as a
/// tenant's key (issue #424, ADR-0012 2026-10-12 item 42).
///
/// Holds the file's bytes exactly as read, so the copy stored under `keys/`
/// is byte-identical to what the key holder kept, and the public key derived
/// from its one identity. `Debug` never shows the bytes.
pub struct AdoptableIdentity {
    bytes: Vec<u8>,
    /// The `age1…` public key of the file's one identity.
    pub public_key: String,
}

impl std::fmt::Debug for AdoptableIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdoptableIdentity")
            .field("public_key", &self.public_key)
            .finish_non_exhaustive()
    }
}

impl AdoptableIdentity {
    /// The identity file's bytes, exactly as read.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Read and validate an identity file offered for adoption (#424).
///
/// Refuses, naming the reason, anything but a single native age X25519
/// identity: no identity, more than one, a malformed `AGE-SECRET-KEY-1…`
/// line, a plugin identity, a public key, an age-encrypted (passphrase
/// protected) file, or any other non-comment line. Blank lines and `#`
/// comments (what `age-keygen -o` writes around the key) are allowed and
/// kept.
pub fn read_adoptable_identity(path: &Path) -> Result<AdoptableIdentity> {
    let refuse = |why: String| {
        TapectlError::Encryption(format!(
            "{} is not a single age X25519 identity: {why}",
            path.display()
        ))
    };
    let meta = fs::metadata(path)
        .map_err(|e| TapectlError::Encryption(format!("cannot read {}: {e}", path.display())))?;
    if !meta.is_file() {
        return Err(refuse("it is not a regular file".into()));
    }
    if meta.len() > ADOPT_MAX_BYTES as u64 {
        return Err(refuse(format!(
            "it is {} bytes, far more than an identity file holds",
            meta.len()
        )));
    }
    let bytes = fs::read(path)
        .map_err(|e| TapectlError::Encryption(format!("cannot read {}: {e}", path.display())))?;
    parse_adoptable_identity(bytes).map_err(refuse)
}

/// Why an identity file is not one: the line-level findings the adoption
/// reader and the `--key` reader share, each rendering them for its own
/// purpose.
#[derive(Debug, PartialEq, Eq)]
enum IdentityFault {
    /// An age-encrypted file (a passphrase-protected identity).
    Encrypted,
    NotText,
    /// A plugin identity, on this 1-based line.
    Plugin(usize),
    /// A public key, on this line.
    PublicKey(usize),
    /// Neither a comment nor an identity, on this line.
    NotAnIdentityLine(usize),
    /// An `AGE-SECRET-KEY-` line that does not parse, on this line.
    Invalid(usize),
}

/// Every native X25519 identity in `bytes`, in file order. Blank lines and
/// `#` comments (what `age-keygen -o` writes around the key) are skipped;
/// anything else that is not an identity is a fault. Holding no identity is
/// not one: the caller decides what an empty file means.
fn scan_identity_lines(bytes: &[u8]) -> std::result::Result<Vec<x25519::Identity>, IdentityFault> {
    if bytes.starts_with(b"age-encryption.org/") {
        return Err(IdentityFault::Encrypted);
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Err(IdentityFault::NotText);
    };
    let mut identities: Vec<x25519::Identity> = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        let line = raw.trim_end();
        let lead = line.trim_start();
        if lead.is_empty() || lead.starts_with('#') {
            continue;
        }
        if lead.starts_with("-----BEGIN AGE ENCRYPTED FILE-----") {
            return Err(IdentityFault::Encrypted);
        }
        if lead.starts_with("AGE-PLUGIN-") {
            return Err(IdentityFault::Plugin(n));
        }
        if lead.starts_with("age1") {
            return Err(IdentityFault::PublicKey(n));
        }
        if !line.starts_with("AGE-SECRET-KEY-") {
            return Err(IdentityFault::NotAnIdentityLine(n));
        }
        match line.parse::<x25519::Identity>() {
            Ok(id) => identities.push(id),
            Err(_) => return Err(IdentityFault::Invalid(n)),
        }
    }
    Ok(identities)
}

/// The pure half of [`read_adoptable_identity`]: `Err` is the reason.
fn parse_adoptable_identity(bytes: Vec<u8>) -> std::result::Result<AdoptableIdentity, String> {
    const ENCRYPTED: &str = "it is an age-encrypted file (a passphrase-protected identity?) — \
                             decrypt it with `age -d` first and adopt the plain identity file";
    let mut identities = scan_identity_lines(&bytes).map_err(|fault| match fault {
        IdentityFault::Encrypted => ENCRYPTED.to_string(),
        IdentityFault::NotText => "it is not a text file".to_string(),
        IdentityFault::Plugin(n) => format!(
            "line {n} is a plugin identity — tapectl adopts only native X25519 identities \
             (AGE-SECRET-KEY-1…)"
        ),
        IdentityFault::PublicKey(n) => format!(
            "line {n} is a public key — adoption needs the SECRET identity file \
             (AGE-SECRET-KEY-1…); `tapectl key import` registers a public key"
        ),
        IdentityFault::NotAnIdentityLine(n) => {
            format!("line {n} is neither a comment nor an age identity at the start of its line")
        }
        IdentityFault::Invalid(n) => format!("line {n} is not a valid age X25519 identity"),
    })?;
    let identity = match identities.len() {
        0 => return Err("it holds no age identity".into()),
        1 => identities.pop().expect("one"),
        k => {
            return Err(format!(
                "it holds {k} identities — adopt exactly one, from a file of its own"
            ))
        }
    };
    // What every reader of `keys/` will take from the stored copy must be
    // this very identity, or the adopted file would load as something else.
    let text = std::str::from_utf8(&bytes).expect("the scan accepted it as text");
    let read_back = secret_key_line(text)
        .and_then(|s| s.parse::<x25519::Identity>().ok())
        .map(|id| id.to_public().to_string());
    let public_key = identity.to_public().to_string();
    if read_back.as_deref() != Some(public_key.as_str()) {
        return Err("its identity line would not be read back as written".into());
    }
    Ok(AdoptableIdentity { bytes, public_key })
}

/// An age identity file given to one command with `--key FILE` (ADR-0012
/// 2026-10-19 item 107): read, parsed, and held for that command's
/// decryption only.
///
/// It is never copied into the home's `keys/`, never registered as a key
/// of any tenant, and never the operator's or the escrow's identity
/// (ADR-0005: no command replaces a registered escrow identity), so an
/// heir holding only the escrow secret can read a tape without any of
/// that. `Debug` shows the path and the count, never a secret.
#[derive(Clone)]
pub struct KeyFile {
    path: std::path::PathBuf,
    identities: Vec<x25519::Identity>,
}

impl std::fmt::Debug for KeyFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyFile")
            .field("path", &self.path)
            .field("identities", &self.identities.len())
            .finish()
    }
}

impl KeyFile {
    /// Read and parse the identity file at `path`: one or more native
    /// X25519 identities (`AGE-SECRET-KEY-1…`), with blank lines and `#`
    /// comments allowed, as `age-keygen -o` writes them.
    ///
    /// Refused, naming the file and why: one that cannot be read, is too
    /// large to be an identity file, is age-encrypted, holds a plugin
    /// identity or a public key, has any other non-comment line, or holds
    /// no identity at all. A refusal never echoes a line of the file.
    pub fn read(path: &Path) -> Result<KeyFile> {
        let refuse = |why: String| {
            TapectlError::Encryption(format!(
                "--key {} is not an age X25519 identity file: {why}",
                path.display()
            ))
        };
        let unreadable = |e: std::io::Error| {
            TapectlError::Encryption(format!("--key: cannot read {}: {e}", path.display()))
        };
        let mut bytes = Vec::new();
        {
            use std::io::Read;
            fs::File::open(path)
                .map_err(unreadable)?
                .take(ADOPT_MAX_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(unreadable)?;
        }
        if bytes.len() > ADOPT_MAX_BYTES {
            return Err(refuse(
                "it is far more than an identity file holds".to_string(),
            ));
        }
        let identities = scan_identity_lines(&bytes).map_err(|fault| {
            refuse(match fault {
                IdentityFault::Encrypted => "it is an age-encrypted file (a passphrase-protected \
                     identity?) — decrypt it with `age -d` first and give the plain identity file"
                    .to_string(),
                IdentityFault::NotText => "it is not a text file".to_string(),
                IdentityFault::Plugin(n) => format!(
                    "line {n} is a plugin identity — tapectl reads only native X25519 \
                     identities (AGE-SECRET-KEY-1…)"
                ),
                IdentityFault::PublicKey(n) => format!(
                    "line {n} is a public key — --key needs the SECRET identity file \
                     (AGE-SECRET-KEY-1…)"
                ),
                IdentityFault::NotAnIdentityLine(n) => format!(
                    "line {n} is neither a comment nor an age identity at the start of its line"
                ),
                IdentityFault::Invalid(n) => {
                    format!("line {n} is not a valid age X25519 identity")
                }
            })
        })?;
        if identities.is_empty() {
            return Err(refuse("it holds no age identity".to_string()));
        }
        Ok(KeyFile {
            path: path.to_path_buf(),
            identities,
        })
    }

    /// Read every file named, in order, stopping at the first refusal. The
    /// one place a command's `--key` list becomes identities, called before
    /// the drive is opened.
    pub fn read_all(paths: &[std::path::PathBuf]) -> Result<Vec<KeyFile>> {
        paths.iter().map(|p| KeyFile::read(p)).collect()
    }

    /// The file this came from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The identities in the file, in file order.
    pub fn identities(&self) -> &[x25519::Identity] {
        &self.identities
    }

    /// Every identity of every file, for trial decryption.
    pub fn all_identities(files: &[KeyFile]) -> Vec<x25519::Identity> {
        files
            .iter()
            .flat_map(|f| f.identities.iter().cloned())
            .collect()
    }

    /// The files' paths as one line, in the order given: how a refusal
    /// names the keys that were tried.
    pub fn describe(files: &[KeyFile]) -> String {
        files
            .iter()
            .map(|f| f.path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Store an adopted identity (#424) where a minted one lives: the secret at
/// `keys/<tenant>-<alias>.age.key`, its bytes exactly as given, created mode
/// 0600 by this process as [`save_secret_key`] does; the public half beside
/// it as `.age.pub`. Refuses to replace either file. On a failure part-way,
/// removes what it wrote.
pub fn save_adopted_keypair(
    keys_dir: &Path,
    tenant_name: &str,
    alias: &str,
    identity: &AdoptableIdentity,
) -> Result<()> {
    let (pub_path, key_path) = key_paths(keys_dir, tenant_name, alias);
    if pub_path.exists() || key_path.exists() {
        return Err(TapectlError::KeyAlreadyExists(format!(
            "{tenant_name}-{alias}"
        )));
    }
    fs::create_dir_all(keys_dir)?;
    let write_secret = || -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&key_path)?;
        file.write_all(identity.bytes())?;
        file.sync_all()
    };
    if let Err(e) = write_secret() {
        let _ = fs::remove_file(&key_path);
        return Err(e.into());
    }
    if let Err(e) = save_public_key(&pub_path, &identity.public_key) {
        let _ = fs::remove_file(&key_path);
        let _ = fs::remove_file(&pub_path);
        return Err(e);
    }
    Ok(())
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

/// Which key files under `keys/` belong to which tenant (issue #350).
///
/// A key file is `{alias}.age.key`, and an alias is `{tenant}-{short}` — but
/// tenant names may themselves contain `-`, so a file name alone cannot say
/// whether `family-old-primary` is tenant `family`'s key `old-primary` or
/// tenant `family-old`'s key `primary`. The rules, in order:
///
/// 1. **The key's own row settles it.** `encryption_keys.alias` is exactly
///    the file stem (unique across tenants), and its `tenant_id` is the
///    answer. Deactivated rows count: a key a rotation retired still
///    decrypts everything written before it. This is the ordinary case —
///    every key `tenant add`, `key generate` and `key rotate` write has a row
///    — and it is where `family` stops loading `family-old`'s keys.
/// 2. **With no row, the plain `{tenant}-` prefix decides**, exactly as the
///    old scan did, so an ambiguous file goes to EVERY tenant whose name
///    prefixes it. This is the `catalog rebuild` case: a rebuild recreates
///    tenant rows but deliberately no key rows (no tape records a recipient
///    list, #137), and then nothing on this machine can say whose
///    `family-old-laptop.age.key` is. Guessing — the longest matching tenant
///    name, say — would hand `family`'s own `old-laptop` key to `family-old`
///    and leave a restore of `family` unable to open what was written only
///    to it. An extra identity costs one failed trial decryption; a missing
///    one costs the data. So after a rebuild, ownership is exact again only
///    once each key is back in the catalog (`key import`).
///
/// Rule 1 only ever takes a file AWAY from the prefix scan, and only when a
/// row names another tenant; rule 2 is the prefix scan. So a tenant's own
/// key file is never withheld from it.
pub struct KeyOwners {
    by_alias: HashMap<String, String>,
}

impl KeyOwners {
    /// Read every key alias the catalog knows, with its tenant.
    pub fn from_catalog(conn: &Connection) -> Result<Self> {
        let mut by_alias = HashMap::new();
        let mut stmt = conn.prepare(
            "SELECT k.alias, t.name FROM encryption_keys k JOIN tenants t ON t.id = k.tenant_id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (alias, tenant) = row?;
            by_alias.insert(alias, tenant);
        }
        Ok(Self { by_alias })
    }

    /// Whether key file stem `stem` is `tenant`'s, by the rules above.
    fn is_owned_by(&self, stem: &str, tenant: &str) -> bool {
        match self.by_alias.get(stem) {
            Some(owner) => owner == tenant,
            None => stem
                .strip_prefix(tenant)
                .is_some_and(|rest| rest.starts_with('-')),
        }
    }
}

/// Load every secret key file under `keys_dir` that belongs to
/// `tenant_name`, by [`KeyOwners`]' rules — exactly its own while the
/// catalog has key rows, every file it might own when it has none — as age
/// identities, for trial decryption. Deactivated keys are included (they
/// still open what was written before a rotation). Files are read from disk,
/// never from the database: the catalog only says whose a file is.
///
/// Returns an empty vec if the tenant has no key files — the caller decides
/// whether that's an error.
pub fn load_tenant_identities(
    conn: &Connection,
    keys_dir: &Path,
    tenant_name: &str,
) -> Result<Vec<age::x25519::Identity>> {
    let owners = KeyOwners::from_catalog(conn)?;
    load_identities_owned_by(keys_dir, tenant_name, &owners)
}

/// [`load_tenant_identities`] with the ownership map supplied — the pure
/// half, so the rules are testable without a catalog.
fn load_identities_owned_by(
    keys_dir: &Path,
    tenant_name: &str,
    owners: &KeyOwners,
) -> Result<Vec<age::x25519::Identity>> {
    let entries = match fs::read_dir(keys_dir) {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()),
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".age.key"))
                .is_some_and(|stem| owners.is_owned_by(stem, tenant_name))
        })
        .collect();
    paths.sort();

    let mut identities = Vec::with_capacity(paths.len());
    for path in paths {
        let secret_str = read_secret_key(&path)?;
        let identity: age::x25519::Identity = secret_str.parse().map_err(|e| {
            TapectlError::Encryption(format!("invalid key in {}: {e}", path.display()))
        })?;
        identities.push(identity);
    }
    Ok(identities)
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
    fn read_or_parse_public_key_accepts_literal_string() {
        let kp = generate_keypair();
        let resolved = read_or_parse_public_key(&kp.public_key).unwrap();
        assert_eq!(resolved, kp.public_key);
    }

    #[test]
    fn read_or_parse_public_key_trims_whitespace_on_literal() {
        let kp = generate_keypair();
        let padded = format!("  {}  \n", kp.public_key);
        let resolved = read_or_parse_public_key(&padded).unwrap();
        assert_eq!(resolved, kp.public_key);
    }

    #[test]
    fn read_or_parse_public_key_accepts_file_path() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("k.age.pub");
        let kp = generate_keypair();
        save_public_key(&path, &kp.public_key).unwrap();
        let resolved = read_or_parse_public_key(path.to_str().unwrap()).unwrap();
        assert_eq!(resolved, kp.public_key);
    }

    #[test]
    fn read_or_parse_public_key_errors_on_missing_path() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ghost.pub");
        assert!(read_or_parse_public_key(path.to_str().unwrap()).is_err());
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

    // --- adoption (#424) ----------------------------------------------------

    /// An identity file shaped as `age-keygen -o` writes one: two comment
    /// lines, then the key.
    fn keygen_file(dir: &Path, name: &str) -> (std::path::PathBuf, String) {
        let kp = generate_keypair();
        let path = dir.join(name);
        fs::write(
            &path,
            format!(
                "# created: 2026-10-12T00:00:00Z\n# public key: {}\n{}\n",
                kp.public_key, kp.secret_key
            ),
        )
        .unwrap();
        (path, kp.public_key)
    }

    fn refusal(dir: &Path, content: &[u8]) -> String {
        let path = dir.join("offered.key");
        fs::write(&path, content).unwrap();
        read_adoptable_identity(&path).unwrap_err().to_string()
    }

    #[test]
    fn an_age_keygen_file_is_adoptable_and_kept_byte_for_byte() {
        let tmp = TempDir::new().unwrap();
        let (path, public) = keygen_file(tmp.path(), "fam.key");
        let id = read_adoptable_identity(&path).unwrap();
        assert_eq!(id.public_key, public);
        assert_eq!(id.bytes(), fs::read(&path).unwrap().as_slice());
        assert!(
            !format!("{id:?}").contains("AGE-SECRET-KEY"),
            "Debug must not show the secret"
        );
    }

    #[test]
    fn a_bare_key_line_without_newline_or_comments_is_adoptable() {
        let tmp = TempDir::new().unwrap();
        let kp = generate_keypair();
        let path = tmp.path().join("bare.key");
        fs::write(&path, &kp.secret_key).unwrap();
        assert_eq!(
            read_adoptable_identity(&path).unwrap().public_key,
            kp.public_key
        );
    }

    #[test]
    fn adoption_refuses_anything_but_one_x25519_identity_naming_why() {
        let tmp = TempDir::new().unwrap();
        let a = generate_keypair();
        let b = generate_keypair();
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (b"# only a comment\n\n".to_vec(), "holds no age identity"),
            (b"".to_vec(), "holds no age identity"),
            (
                format!("{}\n{}\n", a.secret_key, b.secret_key).into_bytes(),
                "holds 2 identities",
            ),
            (
                format!("{}\n", a.public_key).into_bytes(),
                "is a public key",
            ),
            (b"AGE-PLUGIN-YUBIKEY-1QQQQQQ\n".to_vec(), "plugin identity"),
            (
                b"AGE-SECRET-KEY-1NOTREALLYAKEY\n".to_vec(),
                "line 1 is not a valid age X25519 identity",
            ),
            (
                format!("# ok\nhello\n{}\n", a.secret_key).into_bytes(),
                "line 2 is neither a comment nor an age identity",
            ),
            (
                format!("  {}\n", a.secret_key).into_bytes(),
                "line 1 is neither a comment nor an age identity",
            ),
            (
                b"age-encryption.org/v1\n-> scrypt xyz 18\n".to_vec(),
                "age-encrypted file",
            ),
            (
                b"-----BEGIN AGE ENCRYPTED FILE-----\nYWdl\n".to_vec(),
                "age-encrypted file",
            ),
            (vec![0xff, 0xfe, 0x00, 0x41], "not a text file"),
        ];
        for (content, want) in cases {
            let msg = refusal(tmp.path(), &content);
            assert!(msg.contains("is not a single age X25519 identity"), "{msg}");
            assert!(msg.contains(want), "want {want:?} in: {msg}");
            assert!(
                !msg.contains(&a.secret_key) && !msg.contains(&b.secret_key),
                "a refusal never echoes a secret"
            );
        }
    }

    #[test]
    fn adoption_refuses_a_missing_file_a_directory_and_an_oversized_file() {
        let tmp = TempDir::new().unwrap();
        assert!(read_adoptable_identity(&tmp.path().join("ghost.key"))
            .unwrap_err()
            .to_string()
            .contains("cannot read"));
        assert!(read_adoptable_identity(tmp.path())
            .unwrap_err()
            .to_string()
            .contains("not a regular file"));
        let big = tmp.path().join("big.key");
        fs::write(&big, vec![b'#'; ADOPT_MAX_BYTES + 1]).unwrap();
        assert!(read_adoptable_identity(&big)
            .unwrap_err()
            .to_string()
            .contains("far more than an identity file holds"));
    }

    // --- `--key FILE` (ADR-0012 2026-10-19 item 107) ------------------------

    fn key_refusal(dir: &Path, content: &[u8]) -> String {
        let path = dir.join("offered.key");
        fs::write(&path, content).unwrap();
        KeyFile::read(&path).unwrap_err().to_string()
    }

    #[test]
    fn a_key_file_reads_as_age_keygen_writes_it_and_names_only_its_path_in_debug() {
        let tmp = TempDir::new().unwrap();
        let (path, public) = keygen_file(tmp.path(), "heir.key");
        let file = KeyFile::read(&path).unwrap();
        assert_eq!(file.path(), path.as_path());
        assert_eq!(file.identities().len(), 1);
        assert_eq!(file.identities()[0].to_public().to_string(), public);
        let shown = format!("{file:?}");
        assert!(shown.contains("heir.key"), "{shown}");
        assert!(!shown.contains("AGE-SECRET-KEY"), "{shown}");
        assert!(!shown.contains(&public), "{shown}");
    }

    /// An identity file may hold several (age's own `-i` takes them), and a
    /// bare key line without a newline is a file too.
    #[test]
    fn a_key_file_may_hold_several_identities_or_a_bare_line() {
        let tmp = TempDir::new().unwrap();
        let (a, b) = (generate_keypair(), generate_keypair());
        let two = tmp.path().join("two.key");
        fs::write(&two, format!("# first\n{}\n\n{}\n", a.secret_key, b.secret_key)).unwrap();
        let file = KeyFile::read(&two).unwrap();
        let publics: Vec<String> = file
            .identities()
            .iter()
            .map(|i| i.to_public().to_string())
            .collect();
        assert_eq!(publics, [a.public_key.clone(), b.public_key.clone()]);

        let bare = tmp.path().join("bare.key");
        fs::write(&bare, &a.secret_key).unwrap();
        assert_eq!(KeyFile::read(&bare).unwrap().identities().len(), 1);

        let all = KeyFile::all_identities(&[file, KeyFile::read(&bare).unwrap()]);
        assert_eq!(all.len(), 3, "every identity of every file, in order");
    }

    #[test]
    fn a_key_file_that_is_not_an_identity_is_refused_by_name_and_never_echoed() {
        let tmp = TempDir::new().unwrap();
        let a = generate_keypair();
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (b"# only a comment\n\n".to_vec(), "holds no age identity"),
            (b"".to_vec(), "holds no age identity"),
            (format!("{}\n", a.public_key).into_bytes(), "is a public key"),
            (b"AGE-PLUGIN-YUBIKEY-1QQQQQQ\n".to_vec(), "plugin identity"),
            (
                b"AGE-SECRET-KEY-1NOTREALLYAKEY\n".to_vec(),
                "line 1 is not a valid age X25519 identity",
            ),
            (
                format!("# ok\nhello\n{}\n", a.secret_key).into_bytes(),
                "line 2 is neither a comment nor an age identity",
            ),
            (
                b"age-encryption.org/v1\n-> scrypt xyz 18\n".to_vec(),
                "age-encrypted file",
            ),
            (vec![0xff, 0xfe, 0x00, 0x41], "not a text file"),
        ];
        for (content, want) in cases {
            let msg = key_refusal(tmp.path(), &content);
            assert!(msg.contains("offered.key"), "names the file: {msg}");
            assert!(msg.contains("is not an age X25519 identity file"), "{msg}");
            assert!(msg.contains(want), "want {want:?} in: {msg}");
            assert!(!msg.contains(&a.secret_key), "never echoes a secret: {msg}");
        }
    }

    #[test]
    fn a_missing_a_directory_and_an_oversized_key_file_are_refused_by_name() {
        let tmp = TempDir::new().unwrap();
        let ghost = KeyFile::read(&tmp.path().join("ghost.key"))
            .unwrap_err()
            .to_string();
        assert!(ghost.contains("ghost.key") && ghost.contains("cannot read"), "{ghost}");
        let dir = KeyFile::read(tmp.path()).unwrap_err().to_string();
        assert!(dir.contains("cannot read"), "{dir}");
        let big = tmp.path().join("big.key");
        fs::write(&big, vec![b'#'; ADOPT_MAX_BYTES + 1]).unwrap();
        let msg = KeyFile::read(&big).unwrap_err().to_string();
        assert!(msg.contains("big.key") && msg.contains("far more than"), "{msg}");
    }

    /// `read_all` stops at the first refusal and names that file, so a list
    /// with one bad entry never reaches the drive.
    #[test]
    fn read_all_refuses_the_list_at_its_first_bad_file() {
        let tmp = TempDir::new().unwrap();
        let (good, _) = keygen_file(tmp.path(), "good.key");
        let bad = tmp.path().join("bad.key");
        fs::write(&bad, "not a key\n").unwrap();
        assert_eq!(KeyFile::read_all(&[good.clone()]).unwrap().len(), 1);
        let err = KeyFile::read_all(&[good, bad]).unwrap_err().to_string();
        assert!(err.contains("bad.key"), "{err}");
    }

    #[test]
    fn an_adopted_keypair_is_stored_as_a_minted_one_and_loads_back() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let (path, public) = keygen_file(tmp.path(), "in.key");
        let id = read_adoptable_identity(&path).unwrap();
        let keys_dir = tmp.path().join("keys");
        save_adopted_keypair(&keys_dir, "fam", "primary", &id).unwrap();

        let (pub_path, key_path) = key_paths(&keys_dir, "fam", "primary");
        assert_eq!(fs::read(&key_path).unwrap(), fs::read(&path).unwrap());
        let mode = fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(read_public_key(&pub_path).unwrap(), public);
        // Read back by the same rule every restore uses.
        let owners = KeyOwners {
            by_alias: HashMap::new(),
        };
        let loaded = load_identities_owned_by(&keys_dir, "fam", &owners).unwrap();
        assert_eq!(publics(&loaded), vec![public]);

        let err = save_adopted_keypair(&keys_dir, "fam", "primary", &id).unwrap_err();
        assert!(matches!(err, TapectlError::KeyAlreadyExists(_)));
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

    /// The catalog a restore sees, as far as key ownership goes: tenant rows,
    /// plus whichever key rows it has (`keys` = (alias, tenant, active)).
    fn catalog(tenants: &[&str], keys: &[(&str, &str, bool)]) -> Connection {
        let conn = crate::db::open_memory().unwrap();
        for (i, t) in tenants.iter().enumerate() {
            crate::db::queries::insert_tenant(&conn, t, None, i == 0).unwrap();
        }
        for (alias, tenant, active) in keys {
            let tid: i64 = conn
                .query_row("SELECT id FROM tenants WHERE name = ?1", [tenant], |r| {
                    r.get(0)
                })
                .unwrap();
            let pk = generate_keypair().public_key;
            conn.execute(
                "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key, is_active)
                 VALUES (?1, ?2, ?3, ?3, ?4)",
                rusqlite::params![tid, alias, pk, active],
            )
            .unwrap();
        }
        conn
    }

    fn publics(ids: &[age::x25519::Identity]) -> Vec<String> {
        let mut v: Vec<String> = ids.iter().map(|i| i.to_public().to_string()).collect();
        v.sort();
        v
    }

    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }

    /// Issue #350(d): tenant names may contain `-`, so `family`'s prefix
    /// `family-` also matches every one of `family-old`'s key files. The
    /// same files with no key rows are the positive control: the file names
    /// alone still show the hazard, so it is the rows that settle it.
    #[test]
    fn tenant_family_does_not_load_tenant_family_old_keys() {
        let tmp = TempDir::new().unwrap();
        let family = generate_and_save(tmp.path(), "family", "primary").unwrap();
        let old = generate_and_save(tmp.path(), "family-old", "primary").unwrap();
        let conn = catalog(
            &["op", "family", "family-old"],
            &[
                ("family-primary", "family", true),
                ("family-old-primary", "family-old", true),
            ],
        );

        let rowless = catalog(&["op", "family", "family-old"], &[]);
        assert_eq!(
            load_tenant_identities(&rowless, tmp.path(), "family")
                .unwrap()
                .len(),
            2,
            "control: without key rows the file names alone give family both"
        );
        assert_eq!(
            publics(&load_tenant_identities(&conn, tmp.path(), "family").unwrap()),
            vec![family.public_key],
            "family loaded another tenant's key"
        );
        assert_eq!(
            publics(&load_tenant_identities(&conn, tmp.path(), "family-old").unwrap()),
            vec![old.public_key],
        );
    }

    /// After `catalog rebuild` the catalog has tenant rows but NO key rows
    /// (no tape records a recipient list, #137), and restore must still find
    /// each tenant's files on disk. A file whose name only one tenant
    /// prefixes is that tenant's alone (`family-old` does not get `family`'s
    /// keys). `family-old-primary.age.key` is prefixed by BOTH names, and
    /// with no row nothing can say which it is, so `family` gets it too —
    /// the old prefix scan's answer, and the only one that cannot withhold a
    /// tenant's own key (see the rowless-ambiguous test below).
    #[test]
    fn a_rebuilt_catalog_with_no_key_rows_loads_every_file_a_tenant_might_own() {
        let tmp = TempDir::new().unwrap();
        let family = generate_and_save(tmp.path(), "family", "primary").unwrap();
        let family_b = generate_and_save(tmp.path(), "family", "backup").unwrap();
        let old = generate_and_save(tmp.path(), "family-old", "primary").unwrap();
        let conn = catalog(&["op", "family", "family-old"], &[]);

        assert_eq!(
            publics(&load_tenant_identities(&conn, tmp.path(), "family").unwrap()),
            sorted(vec![
                family.public_key,
                family_b.public_key,
                old.public_key.clone()
            ]),
        );
        assert_eq!(
            publics(&load_tenant_identities(&conn, tmp.path(), "family-old").unwrap()),
            vec![old.public_key],
        );
    }

    /// Where the file name is genuinely ambiguous — tenant `family`'s key
    /// `old-primary` is `family-old-primary.age.key`, the very name tenant
    /// `family-old`'s `primary` would have — the key's own row decides, not
    /// the longest prefix. Deactivated rows count: a key a rotation retired
    /// still opens what was written before it.
    #[test]
    fn a_key_row_settles_an_ambiguous_file_name_even_when_deactivated() {
        let tmp = TempDir::new().unwrap();
        let fam_old_primary = generate_and_save(tmp.path(), "family", "old-primary").unwrap();
        let old_backup = generate_and_save(tmp.path(), "family-old", "backup").unwrap();
        let conn = catalog(
            &["op", "family", "family-old"],
            &[
                ("family-old-primary", "family", false),
                ("family-old-backup", "family-old", true),
            ],
        );

        assert_eq!(
            publics(&load_tenant_identities(&conn, tmp.path(), "family").unwrap()),
            vec![fam_old_primary.public_key],
        );
        assert_eq!(
            publics(&load_tenant_identities(&conn, tmp.path(), "family-old").unwrap()),
            vec![old_backup.public_key],
        );
    }

    /// The DR case the reviewer of #350 found: tenant `family` has a key
    /// `old-laptop` (`key generate` accepts dashes), so its file is
    /// `family-old-laptop.age.key` — and tenant `family-old` also exists.
    /// After `catalog rebuild` there is no key row to say whose that file
    /// is, and nothing else can: a longest-prefix guess hands `family`'s own
    /// key to `family-old`, and a restore of `family` then cannot open what
    /// was written only to it. With no row, both tenants get the file — an
    /// extra identity costs a trial decryption; a missing one costs the data.
    #[test]
    fn a_rowless_ambiguous_file_is_not_taken_from_the_shorter_tenant_name() {
        let tmp = TempDir::new().unwrap();
        let laptop = generate_and_save(tmp.path(), "family", "old-laptop").unwrap();
        let family = generate_and_save(tmp.path(), "family", "primary").unwrap();
        let old = generate_and_save(tmp.path(), "family-old", "primary").unwrap();
        let conn = catalog(&["op", "family", "family-old"], &[]);

        let fam = publics(&load_tenant_identities(&conn, tmp.path(), "family").unwrap());
        assert!(
            fam.contains(&laptop.public_key),
            "family lost its own key family-old-laptop after a rebuild: {fam:?}"
        );
        assert!(fam.contains(&family.public_key), "{fam:?}");
        // Still no key of a tenant that does not prefix the file.
        let fam_old = publics(&load_tenant_identities(&conn, tmp.path(), "family-old").unwrap());
        assert!(fam_old.contains(&old.public_key), "{fam_old:?}");
        assert!(!fam_old.contains(&family.public_key), "{fam_old:?}");
    }

    /// Positive control, ported from the old loader: every rotation's key
    /// files are the tenant's own, including aliases that contain `-`.
    #[test]
    fn load_tenant_identities_finds_every_rotated_key() {
        let tmp = TempDir::new().unwrap();
        generate_and_save(tmp.path(), "alice", "primary").unwrap();
        generate_and_save(tmp.path(), "alice", "backup").unwrap();
        generate_and_save(tmp.path(), "alice", "rotated-primary-2").unwrap();
        generate_and_save(tmp.path(), "bob", "primary").unwrap();
        let conn = catalog(&["op", "alice", "bob"], &[]);

        assert_eq!(
            load_tenant_identities(&conn, tmp.path(), "alice")
                .unwrap()
                .len(),
            3
        );
        assert!(load_tenant_identities(&conn, tmp.path(), "nobody")
            .unwrap()
            .is_empty());
    }
}
