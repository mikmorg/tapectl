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
