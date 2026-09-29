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
