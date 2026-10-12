use rusqlite::Connection;

use crate::config::TapectlPaths;
use crate::crypto::keys;
use crate::db::{events, queries};
use crate::error::{Result, TapectlError};

/// Add a new tenant with auto-generated keypair.
pub fn add_tenant(
    conn: &Connection,
    paths: &TapectlPaths,
    name: &str,
    description: Option<&str>,
    is_operator: bool,
) -> Result<i64> {
    // Creation-time name validation (issue #103). This one matters most:
    // the name becomes part of a key FILENAME via `keys::key_paths`, so an
    // unvalidated name can put a private key outside `keys/`.
    crate::naming::validate_tenant_name(name)?;

    // Check for duplicate
    if queries::get_tenant_by_name(conn, name)?.is_some() {
        return Err(TapectlError::TenantAlreadyExists(name.to_string()));
    }
    // Before the row: a stray key file would otherwise fail
    // `generate_and_save` only after the tenant row is written.
    refuse_stray_key_files(paths, name, MINTED_ALIASES)?;

    let tenant_id = queries::insert_tenant(conn, name, description, is_operator)?;
    events::log_created(conn, "tenant", tenant_id, name, None)?;

    // Generate primary keypair
    let alias = "primary";
    let kp = keys::generate_and_save(&paths.keys_dir, name, alias)?;
    let key_id = queries::insert_key(
        conn,
        tenant_id,
        &format!("{name}-{alias}"),
        &kp.fingerprint,
        &kp.public_key,
        "primary",
        Some("Auto-generated primary key"),
    )?;
    events::log_created(
        conn,
        "encryption_key",
        key_id,
        &format!("{name}-{alias}"),
        Some(tenant_id),
    )?;

    // Generate backup keypair
    let backup_alias = "backup";
    let backup_kp = keys::generate_and_save(&paths.keys_dir, name, backup_alias)?;
    let backup_key_id = queries::insert_key(
        conn,
        tenant_id,
        &format!("{name}-{backup_alias}"),
        &backup_kp.fingerprint,
        &backup_kp.public_key,
        "backup",
        Some("Auto-generated backup key"),
    )?;
    events::log_created(
        conn,
        "encryption_key",
        backup_key_id,
        &format!("{name}-{backup_alias}"),
        Some(tenant_id),
    )?;

    Ok(tenant_id)
}

/// The alias an adopted identity is registered under: the tenant's primary.
const ADOPTED_ALIAS: &str = "primary";

/// Every refusal `tenant add --adopt-key` (and `init --operator-key`) makes,
/// decided before anything is written (issue #424): the dry run calls this
/// and stops, the real add calls it and then writes. `pending_escrow` is an
/// escrow public key about to be registered but not yet in the catalog —
/// `init --escrow-public-key`'s, since `init` creates the operator tenant
/// first.
///
/// Refuses: an invalid or taken tenant name; a public key the catalog
/// already has, for any tenant, active or not; the escrow identity's secret
/// (registered or pending) — ADR-0005 keeps that secret off every machine;
/// and a key file already at the path the adopted key would take.
pub fn plan_adoption(
    conn: &Connection,
    paths: &TapectlPaths,
    name: &str,
    identity: &keys::AdoptableIdentity,
    pending_escrow: Option<&str>,
) -> Result<()> {
    crate::naming::validate_tenant_name(name)?;
    if queries::get_tenant_by_name(conn, name)?.is_some() {
        return Err(TapectlError::TenantAlreadyExists(name.to_string()));
    }
    refuse_escrow_secret(identity, pending_escrow)?;
    let pk = &identity.public_key;
    if let Some(existing) = queries::get_key_by_fingerprint(conn, pk)? {
        if existing.is_escrow {
            return Err(escrow_secret_refusal(pk));
        }
        let owner = queries::get_tenant_by_id(conn, existing.tenant_id)?
            .map(|t| t.name)
            .unwrap_or_else(|| format!("#{}", existing.tenant_id));
        let state = if existing.is_active {
            "active"
        } else {
            "deactivated"
        };
        return Err(TapectlError::Other(format!(
            "adoption refuses: public key {pk} is already registered as \"{}\", a key of \
             tenant \"{owner}\" ({state}) — a key belongs to one tenant, so it cannot also be \
             tenant \"{name}\"'s",
            existing.alias
        )));
    }
    refuse_stray_key_files(paths, name, ADOPTED_ALIASES)
}

/// The aliases a minted tenant's key files are written under.
pub const MINTED_ALIASES: &[&str] = &["primary", "backup"];

/// The aliases an adopted tenant's key files are written under.
pub const ADOPTED_ALIASES: &[&str] = &[ADOPTED_ALIAS];

/// Refuse when a key file is already where tenant `name`'s `aliases` would
/// be written: a file left over from an earlier home. Callers have already
/// established that `name` has no tenant row (or, at `init`, that there is
/// no catalog yet). It reads only the filesystem, so `init` calls it before
/// it creates anything and before its dry-run branch: the dry run refuses
/// what the real run refuses, and a refused `init` leaves no catalog or
/// config behind.
pub fn refuse_stray_key_files(paths: &TapectlPaths, name: &str, aliases: &[&str]) -> Result<()> {
    for alias in aliases {
        let (pub_path, key_path) = keys::key_paths(&paths.keys_dir, name, alias);
        for p in [&key_path, &pub_path] {
            if p.exists() {
                return Err(TapectlError::Other(format!(
                    "refused: {} already exists — tenant \"{name}\" has no row, so that file \
                     is left over from an earlier home; move it out of keys/ first",
                    p.display()
                )));
            }
        }
    }
    Ok(())
}

/// Refuse `identity` when it is the secret of `escrow_public_key` — the
/// escrow identity not yet in the catalog (`init --escrow-public-key`, which
/// registers it only after the operator tenant). Pure: no catalog.
pub fn refuse_escrow_secret(
    identity: &keys::AdoptableIdentity,
    escrow_public_key: Option<&str>,
) -> Result<()> {
    match escrow_public_key {
        Some(escrow) if escrow == identity.public_key => {
            Err(escrow_secret_refusal(&identity.public_key))
        }
        _ => Ok(()),
    }
}

fn escrow_secret_refusal(public_key: &str) -> TapectlError {
    TapectlError::Other(format!(
        "adoption refuses: this is the escrow identity's secret (its public key {public_key} \
         is the escrow recipient) — ADR-0005 keeps the escrow secret on paper, in the Heir \
         Kit, and never on this machine, so it cannot be a tenant key"
    ))
}

/// Add a tenant whose primary key is an existing age identity instead of a
/// minted pair (issue #424, ADR-0012 2026-10-12 item 42). Mints nothing: the
/// tenant has the one key it adopted, registered as `<name>-primary`, stored
/// as [`keys::save_adopted_keypair`] stores it. Every refusal is
/// [`plan_adoption`]'s and comes before any write; the rows and the files go
/// in together or not at all.
pub fn add_tenant_adopting(
    conn: &Connection,
    paths: &TapectlPaths,
    name: &str,
    description: Option<&str>,
    is_operator: bool,
    identity: &keys::AdoptableIdentity,
    pending_escrow: Option<&str>,
) -> Result<i64> {
    plan_adoption(conn, paths, name, identity, pending_escrow)?;

    let tx = conn.unchecked_transaction()?;
    let tenant_id = queries::insert_tenant(&tx, name, description, is_operator)?;
    events::log_created(&tx, "tenant", tenant_id, name, None)?;
    let full_alias = format!("{name}-{ADOPTED_ALIAS}");
    let key_id = queries::insert_key(
        &tx,
        tenant_id,
        &full_alias,
        &identity.public_key,
        &identity.public_key,
        "primary",
        Some("Adopted from an existing identity file"),
    )?;
    events::log_created(&tx, "encryption_key", key_id, &full_alias, Some(tenant_id))?;
    keys::save_adopted_keypair(&paths.keys_dir, name, ADOPTED_ALIAS, identity)?;
    if let Err(e) = tx.commit() {
        let (pub_path, key_path) = keys::key_paths(&paths.keys_dir, name, ADOPTED_ALIAS);
        let _ = std::fs::remove_file(key_path);
        let _ = std::fs::remove_file(pub_path);
        return Err(e.into());
    }
    Ok(tenant_id)
}

/// Get tenant info by name, returning an error if not found.
pub fn require_tenant(conn: &Connection, name: &str) -> Result<crate::db::models::Tenant> {
    queries::get_tenant_by_name(conn, name)?
        .ok_or_else(|| TapectlError::TenantNotFound(name.to_string()))
}

/// Get tenant info by id, returning an error if not found.
#[allow(dead_code)]
pub fn require_tenant_by_id(conn: &Connection, id: i64) -> Result<crate::db::models::Tenant> {
    queries::get_tenant_by_id(conn, id)?
        .ok_or_else(|| TapectlError::TenantNotFound(format!("id={id}")))
}

/// Delete a tenant (soft delete — marks as 'deleted').
/// Fails if the tenant has active units.
pub fn delete_tenant(conn: &Connection, name: &str) -> Result<()> {
    let tenant = require_tenant(conn, name)?;

    let active_count = queries::count_active_units_for_tenant(conn, tenant.id)?;
    if active_count > 0 {
        return Err(TapectlError::TenantHasActiveUnits);
    }

    conn.execute(
        "UPDATE tenants SET status = 'deleted' WHERE id = ?1",
        rusqlite::params![tenant.id],
    )?;

    events::log_field_change(
        conn,
        "tenant",
        tenant.id,
        name,
        "deleted",
        "status",
        Some("active"),
        "deleted",
        None,
    )?;

    Ok(())
}

/// The operator tenant's name for `tapectl init` (issue #357).
///
/// `--operator` when given. Without it, the login name (`$USER`) — but NOT
/// under a system account: run as a service user, `init` used to name the
/// operator tenant after the account (`tapectl`) silently. A uid below
/// `uid_min` (the system's `UID_MIN`, see [`uid_min_from_login_defs`]) is a
/// system account, and `init` refuses there until `--operator` names the
/// person.
///
/// Pure: `main()` passes the real flag, `$USER`, effective uid and
/// `UID_MIN`, so the decision is testable with any uid.
pub fn operator_name_for_init(
    operator_flag: Option<&str>,
    user_env: Option<&str>,
    uid: u32,
    uid_min: u32,
) -> Result<String> {
    if let Some(name) = operator_flag {
        return Ok(name.to_string());
    }
    let user = user_env.filter(|u| !u.is_empty());
    if uid < uid_min {
        let account = user.map(|u| format!("\"{u}\", ")).unwrap_or_default();
        return Err(TapectlError::Other(format!(
            "init: --operator is required when run as a system account ({account}uid {uid}, \
             below UID_MIN {uid_min}). Without it the operator tenant would be named after \
             the account rather than the person who operates this archive. Re-run with \
             --operator <name>, e.g. `tapectl init --operator alice`."
        )));
    }
    Ok(user.unwrap_or("operator").to_string())
}

/// `UID_MIN` from the text of `/etc/login.defs`, or 1000 (the shadow-utils
/// default) when the file is absent or does not set it. Only an
/// uncommented `UID_MIN` line counts — not `SYS_UID_MIN`, `SUB_UID_MIN` or
/// a commented example.
pub fn uid_min_from_login_defs(text: Option<&str>) -> u32 {
    const DEFAULT_UID_MIN: u32 = 1000;
    text.and_then(|text| {
        text.lines().find_map(|line| {
            let mut words = line.split_whitespace();
            match (words.next(), words.next()) {
                (Some("UID_MIN"), Some(value)) => value.parse().ok(),
                _ => None,
            }
        })
    })
    .unwrap_or(DEFAULT_UID_MIN)
}

/// [`uid_min_from_login_defs`] over the real `/etc/login.defs`.
pub fn system_uid_min() -> u32 {
    uid_min_from_login_defs(std::fs::read_to_string("/etc/login.defs").ok().as_deref())
}

#[cfg(test)]
mod tests {
    //! Wiring tests for issue #103. `naming.rs` proves the *rule*; these
    //! prove `add_tenant` actually applies it — and, more importantly, that
    //! rejection happens BEFORE any key material touches the disk. A
    //! validator that runs after `generate_and_save` would pass every unit
    //! test in `naming.rs` while still writing the private key it exists to
    //! keep inside `keys/`.
    use super::*;
    use tempfile::TempDir;

    fn fixture() -> (Connection, TempDir, TapectlPaths) {
        let tmp = TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().join(".tapectl"));
        paths.ensure_dirs().unwrap();
        let conn = crate::db::open_memory().unwrap();
        (conn, tmp, paths)
    }

    #[test]
    fn a_traversing_tenant_name_is_rejected_and_writes_no_key_anywhere() {
        let (conn, tmp, paths) = fixture();

        let err = add_tenant(&conn, &paths, "../../escaped", None, false)
            .expect_err("a tenant name containing .. must be refused");
        assert!(err.to_string().contains("invalid tenant name"), "{err}");

        // The whole point: nothing was written, inside keys/ or out of it.
        let stray: Vec<_> = walkdir::WalkDir::new(tmp.path())
            .into_iter()
            .flatten()
            .filter(|e| {
                e.file_type().is_file()
                    && e.path()
                        .extension()
                        .is_some_and(|x| x == "key" || x == "pub")
            })
            .map(|e| e.path().display().to_string())
            .collect();
        assert!(
            stray.is_empty(),
            "key material was written despite the name being rejected: {stray:?}"
        );

        // ...and no row was inserted either.
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM tenants", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "a rejected tenant must leave no row behind");
    }

    #[test]
    fn an_ordinary_tenant_name_still_works_end_to_end() {
        let (conn, _tmp, paths) = fixture();
        add_tenant(&conn, &paths, "alice", Some("test"), false)
            .expect("a normal name must still be accepted");
        let (pub_path, key_path) =
            crate::crypto::keys::key_paths(&paths.keys_dir, "alice", "primary");
        assert!(pub_path.exists() && key_path.exists());
    }

    /// A key file left in keys/ at a path the minted pair would take is
    /// refused before the tenant row is written: a refused add leaves no
    /// row, writes no key, and leaves the stray file as it was.
    #[test]
    fn a_stray_key_file_refuses_a_minted_tenant_before_any_row() {
        let (conn, _tmp, paths) = fixture();
        let stray = paths.keys_dir.join("alice-backup.age.pub");
        std::fs::write(&stray, "left over\n").unwrap();
        let msg = add_tenant(&conn, &paths, "alice", None, false)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("alice-backup.age.pub already exists"), "{msg}");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM tenants", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "a refused add leaves no tenant row");
        let files: Vec<_> = std::fs::read_dir(&paths.keys_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files, vec!["alice-backup.age.pub".to_string()]);
        assert_eq!(std::fs::read_to_string(&stray).unwrap(), "left over\n");
    }

    // --- adoption (issue #424) ---------------------------------------------

    /// An identity file as `age-keygen -o` writes it, outside the home.
    fn identity_file(tmp: &TempDir, name: &str) -> (std::path::PathBuf, String) {
        let kp = keys::generate_keypair();
        let path = tmp.path().join(name);
        std::fs::write(
            &path,
            format!(
                "# created: 2026-10-12T00:00:00Z\n# public key: {}\n{}\n",
                kp.public_key, kp.secret_key
            ),
        )
        .unwrap();
        (path, kp.public_key)
    }

    fn counts(conn: &Connection) -> (i64, i64) {
        let t = conn
            .query_row("SELECT COUNT(*) FROM tenants", [], |r| r.get(0))
            .unwrap();
        let k = conn
            .query_row("SELECT COUNT(*) FROM encryption_keys", [], |r| r.get(0))
            .unwrap();
        (t, k)
    }

    fn key_files(paths: &TapectlPaths) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&paths.keys_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn an_adopted_tenant_has_exactly_the_one_key_it_was_given() {
        let (conn, tmp, paths) = fixture();
        let (path, public) = identity_file(&tmp, "fam.key");
        let id = keys::read_adoptable_identity(&path).unwrap();
        let tid = add_tenant_adopting(&conn, &paths, "fam", None, false, &id, None).unwrap();

        let rows = queries::list_keys_for_tenant(&conn, tid).unwrap();
        assert_eq!(rows.len(), 1, "adoption mints nothing");
        assert_eq!(rows[0].alias, "fam-primary");
        assert_eq!(rows[0].key_type, "primary");
        assert_eq!(rows[0].fingerprint, public);
        assert_eq!(rows[0].public_key, public);
        assert!(rows[0].is_active && !rows[0].is_escrow);
        assert_eq!(
            key_files(&paths),
            vec!["fam-primary.age.key", "fam-primary.age.pub"]
        );
        let (_, key_path) = keys::key_paths(&paths.keys_dir, "fam", "primary");
        assert_eq!(
            std::fs::read(key_path).unwrap(),
            std::fs::read(&path).unwrap()
        );
        let loaded = keys::load_tenant_identities(&conn, &paths.keys_dir, "fam").unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].to_public().to_string(), public);
    }

    #[test]
    fn every_adoption_refusal_leaves_no_tenant_key_row_or_file_behind() {
        let (conn, tmp, paths) = fixture();
        add_tenant(&conn, &paths, "op", None, true).unwrap();
        let (taken, _) = identity_file(&tmp, "taken.key");
        let taken_id = keys::read_adoptable_identity(&taken).unwrap();
        add_tenant_adopting(&conn, &paths, "alice", None, false, &taken_id, None).unwrap();
        let (escrow, escrow_pub) = identity_file(&tmp, "escrow.key");
        let op = queries::get_operator_tenant(&conn).unwrap().unwrap();
        queries::insert_escrow_key(&conn, op.id, "op-escrow", &escrow_pub, &escrow_pub, None)
            .unwrap();
        let escrow_id = keys::read_adoptable_identity(&escrow).unwrap();
        let (fresh, fresh_pub) = identity_file(&tmp, "fresh.key");
        let fresh_id = keys::read_adoptable_identity(&fresh).unwrap();

        let before = counts(&conn);
        let files_before = key_files(&paths);

        let refuse = |name: &str, id: &keys::AdoptableIdentity, pending: Option<&str>| {
            let planned = plan_adoption(&conn, &paths, name, id, pending)
                .expect_err("the plan refuses")
                .to_string();
            let added = add_tenant_adopting(&conn, &paths, name, None, false, id, pending)
                .expect_err("the add refuses")
                .to_string();
            assert_eq!(planned, added, "the dry run refuses what the add refuses");
            assert_eq!(counts(&conn), before, "{added}");
            assert_eq!(key_files(&paths), files_before, "{added}");
            added
        };

        // Registered to another tenant (and to the same name it is taken).
        let msg = refuse("bob", &taken_id, None);
        assert!(
            msg.contains("already registered as \"alice-primary\""),
            "{msg}"
        );
        assert!(msg.contains("tenant \"alice\""), "{msg}");
        // A deactivated key is still registered.
        conn.execute(
            "UPDATE encryption_keys SET is_active = 0 WHERE alias = 'alice-primary'",
            [],
        )
        .unwrap();
        let msg = refuse("bob", &taken_id, None);
        assert!(msg.contains("(deactivated)"), "{msg}");
        // The escrow secret, registered.
        let msg = refuse("bob", &escrow_id, None);
        assert!(msg.contains("escrow identity's secret"), "{msg}");
        assert!(msg.contains("ADR-0005"), "{msg}");
        // The escrow secret, about to be registered (init's order).
        let msg = refuse("bob", &fresh_id, Some(&fresh_pub));
        assert!(msg.contains("escrow identity's secret"), "{msg}");
        // A taken tenant name, an invalid one.
        let msg = refuse("alice", &fresh_id, None);
        assert!(msg.contains("alice"), "{msg}");
        let msg = refuse("../escaped", &fresh_id, None);
        assert!(msg.contains("invalid tenant name"), "{msg}");
        // A left-over key file at the path the adopted key would take.
        let stray = paths.keys_dir.join("carol-primary.age.key");
        std::fs::write(&stray, "left over\n").unwrap();
        let files_before_stray = key_files(&paths);
        let msg = plan_adoption(&conn, &paths, "carol", &fresh_id, None)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("carol-primary.age.key already exists"),
            "{msg}"
        );
        add_tenant_adopting(&conn, &paths, "carol", None, false, &fresh_id, None).unwrap_err();
        assert_eq!(counts(&conn), before);
        assert_eq!(key_files(&paths), files_before_stray);
        assert_eq!(std::fs::read_to_string(&stray).unwrap(), "left over\n");
    }

    // --- operator_name_for_init (issue #357) -------------------------------

    #[test]
    fn init_operator_flag_wins_even_under_a_system_account() {
        let name = operator_name_for_init(Some("alice"), Some("tapectl"), 998, 1000).unwrap();
        assert_eq!(name, "alice");
    }

    #[test]
    fn init_operator_defaults_to_user_for_a_login_account() {
        let name = operator_name_for_init(None, Some("mike"), 1000, 1000).unwrap();
        assert_eq!(name, "mike");
        let name = operator_name_for_init(None, None, 1001, 1000).unwrap();
        assert_eq!(name, "operator", "no $USER: the old fallback stands");
    }

    #[test]
    fn init_without_operator_refuses_under_a_system_account() {
        for uid in [0, 998, 999] {
            let err = operator_name_for_init(None, Some("tapectl"), uid, 1000)
                .expect_err("a system account must not silently name the operator");
            let msg = err.to_string();
            assert!(msg.contains("--operator"), "must name the flag: {msg}");
            assert!(msg.contains(&uid.to_string()), "must name the uid: {msg}");
        }
        // The boundary follows the system's UID_MIN, not a constant.
        assert!(operator_name_for_init(None, Some("svc"), 499, 500).is_err());
        assert!(operator_name_for_init(None, Some("svc"), 500, 500).is_ok());
    }

    #[test]
    fn uid_min_is_read_from_login_defs_and_defaults_to_1000() {
        let debian = "#\n# Min/max values for automatic uid selection in useradd\n#\n\
                      UID_MIN\t\t\t 1000\nUID_MAX\t\t\t60000\n\
                      #SYS_UID_MIN\t\t  100\nSUB_UID_MIN\t\t   100000\n";
        assert_eq!(uid_min_from_login_defs(Some(debian)), 1000);
        assert_eq!(uid_min_from_login_defs(Some("UID_MIN 500\n")), 500);
        assert_eq!(
            uid_min_from_login_defs(Some("SYS_UID_MIN 201\nSUB_UID_MIN 100000\n")),
            1000,
            "only UID_MIN itself counts"
        );
        assert_eq!(
            uid_min_from_login_defs(Some("# UID_MIN 500\n")),
            1000,
            "a commented line does not count"
        );
        assert_eq!(uid_min_from_login_defs(Some("UID_MIN nonsense\n")), 1000);
        assert_eq!(uid_min_from_login_defs(None), 1000);
    }
}
