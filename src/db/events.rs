use rusqlite::{params, Connection};

use crate::error::Result;

/// Log an event to the audit trail.
#[allow(clippy::too_many_arguments)]
pub fn log_event(
    conn: &Connection,
    entity_type: &str,
    entity_id: i64,
    entity_label: Option<&str>,
    action: &str,
    field: Option<&str>,
    old_value: Option<&str>,
    new_value: Option<&str>,
    details: Option<&str>,
    tenant_id: Option<i64>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO events (entity_type, entity_id, entity_label, action,
                             field, old_value, new_value, details, tenant_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            entity_type,
            entity_id,
            entity_label,
            action,
            field,
            old_value,
            new_value,
            details,
            tenant_id,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Convenience: log a "created" event.
pub fn log_created(
    conn: &Connection,
    entity_type: &str,
    entity_id: i64,
    entity_label: &str,
    tenant_id: Option<i64>,
) -> Result<i64> {
    log_event(
        conn,
        entity_type,
        entity_id,
        Some(entity_label),
        "created",
        None,
        None,
        None,
        None,
        tenant_id,
    )
}

/// Convenience: log a field change event.
#[allow(clippy::too_many_arguments)]
pub fn log_field_change(
    conn: &Connection,
    entity_type: &str,
    entity_id: i64,
    entity_label: &str,
    action: &str,
    field: &str,
    old_value: Option<&str>,
    new_value: &str,
    tenant_id: Option<i64>,
) -> Result<i64> {
    log_event(
        conn,
        entity_type,
        entity_id,
        Some(entity_label),
        action,
        Some(field),
        old_value,
        Some(new_value),
        None,
        tenant_id,
    )
}
