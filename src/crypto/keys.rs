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

/// Load all secret keys for a tenant from disk as age identities.
///
/// Scans `keys_dir` for files matching `{tenant_name}-*.age.key` and parses
/// each into an `age::x25519::Identity`. Returns an empty vec if no key files
/// are found — the caller decides whether that's an error.
///
/// **Superseded by [`load_tenant_identities`] (issue #350).** Tenant names
/// may contain `-`, so this prefix scan hands tenant `family` every key of
/// tenant `family-old` as well. Kept only until its last caller
/// (`volume::restore`) moves to the successor; do not add new callers.
pub fn load_all_identities(
    keys_dir: &Path,
    tenant_name: &str,
) -> Result<Vec<age::x25519::Identity>> {
    let prefix = format!("{tenant_name}-");
    let mut identities = Vec::new();

    let entries = match fs::read_dir(keys_dir) {
        Ok(e) => e,
        Err(_) => return Ok(identities),
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with(&prefix) && name_str.ends_with(".age.key") {
            let secret_str = read_secret_key(&entry.path())?;
            let identity: age::x25519::Identity = secret_str.parse().map_err(|e| {
                TapectlError::Encryption(format!("invalid key in {}: {e}", entry.path().display()))
            })?;
            identities.push(identity);
        }
    }

    Ok(identities)
}

/// Which tenant each key file under `keys/` belongs to (issue #350).
///
/// A key file is `{alias}.age.key`, and an alias is `{tenant}-{short}` — but
/// tenant names may themselves contain `-`, so a file name alone cannot say
/// whether `family-old-primary` is tenant `family`'s key `old-primary` or
/// tenant `family-old`'s key `primary`. Two sources settle it, in order:
///
/// 1. **The key's own row.** `encryption_keys.alias` is exactly the file
///    stem, and its `tenant_id` is the answer. Deactivated rows count: a key
///    a rotation retired still decrypts everything written before it.
/// 2. **The longest tenant name that prefixes the stem** (followed by `-`).
///    This is what survives `catalog rebuild`, which recreates tenant rows
///    but deliberately no key rows (no tape records a recipient list, #137)
///    — so restore after a rebuild still finds every tenant's files on disk,
///    and still does not hand `family` the files of `family-old`. Tenants of
///    every status count: a deleted `family-old`'s keys are still not
///    `family`'s.
///
/// Both rules only ever take a file AWAY from the plain `{tenant}-` prefix
/// scan [`load_all_identities`] did, never add one: the tenant asked about
/// is always a candidate, so its own files match at minimum.
pub struct KeyOwners {
    by_alias: HashMap<String, String>,
    tenants: Vec<String>,
}

impl KeyOwners {
    /// Read every key alias and every tenant name the catalog knows.
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
        let mut stmt = conn.prepare("SELECT name FROM tenants")?;
        let tenants = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Self { by_alias, tenants })
    }

    /// The tenant that owns key file stem `stem`, or `None` when no known
    /// tenant (nor `also_consider`, the tenant being asked about) prefixes it.
    fn owner_of<'a>(&'a self, stem: &str, also_consider: &'a str) -> Option<&'a str> {
        if let Some(t) = self.by_alias.get(stem) {
            return Some(t.as_str());
        }
        self.tenants
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(also_consider))
            .filter(|t| {
                stem.len() > t.len() + 1 && stem.starts_with(t) && stem.as_bytes()[t.len()] == b'-'
            })
            .max_by_key(|t| t.len())
    }
}

/// Load every secret key file under `keys_dir` that belongs to
/// `tenant_name` — exactly its own, by [`KeyOwners`]' rules — as age
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
                .is_some_and(|stem| owners.owner_of(stem, tenant_name) == Some(tenant_name))
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

    #[test]
    fn load_all_identities_finds_multiple_keys() {
        let tmp = TempDir::new().unwrap();
        // Simulate pre-rotation + post-rotation keys
        generate_and_save(tmp.path(), "alice", "primary").unwrap();
        generate_and_save(tmp.path(), "alice", "backup").unwrap();
        generate_and_save(tmp.path(), "alice", "rotated-primary").unwrap();

        let ids = load_all_identities(tmp.path(), "alice").unwrap();
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn load_all_identities_ignores_other_tenants() {
        let tmp = TempDir::new().unwrap();
        generate_and_save(tmp.path(), "alice", "primary").unwrap();
        generate_and_save(tmp.path(), "bob", "primary").unwrap();

        let ids = load_all_identities(tmp.path(), "alice").unwrap();
        assert_eq!(ids.len(), 1);
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
    /// old loader is kept as the positive control: it still shows the hazard.
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

        assert_eq!(
            load_all_identities(tmp.path(), "family").unwrap().len(),
            2,
            "control: the prefix scan is the hazard this test exists for"
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
    /// each tenant's files on disk — and still only its own.
    #[test]
    fn a_rebuilt_catalog_with_no_key_rows_still_loads_each_tenants_own_keys() {
        let tmp = TempDir::new().unwrap();
        let family = generate_and_save(tmp.path(), "family", "primary").unwrap();
        let family_b = generate_and_save(tmp.path(), "family", "backup").unwrap();
        let old = generate_and_save(tmp.path(), "family-old", "primary").unwrap();
        let conn = catalog(&["op", "family", "family-old"], &[]);

        assert_eq!(
            publics(&load_tenant_identities(&conn, tmp.path(), "family").unwrap()),
            sorted(vec![family.public_key, family_b.public_key]),
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

    #[test]
    fn load_all_identities_empty_for_missing_tenant() {
        let tmp = TempDir::new().unwrap();
        let ids = load_all_identities(tmp.path(), "nobody").unwrap();
        assert!(ids.is_empty());
    }
}
