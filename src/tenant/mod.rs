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
