use rusqlite::{params, Connection};

use crate::error::Result;

/// The action of the event a write session logs on its volume each time the
/// drive refuses a write with Logical Block Protection armed, at the moment
/// the session meets it, before item 174 asks the drive's position: in the
/// execute loop, at the seal or in the re-send before a retried seal (#388,
/// ADR-0012 item 186). Its details name the tape file whose write was refused
/// and whether the run's one retry was still unspent then:
/// `{"refused_in": 9, "retry_due": true}`. Whether the retry was made is the
/// `write_lbp_retry` event's.
pub const WRITE_LBP_REFUSAL: &str = "write_lbp_refusal";

/// Whether the drive refused a write with Logical Block Protection armed in
/// a write session of volume `volume_id`: any [`WRITE_LBP_REFUSAL`] event on
/// it (ADR-0012 item 186). Read from the catalog, never from memory, so a
/// `volume resume` in another process sees a refusal the first run met. The
/// volume is the scope: `volume init` mints a new row for every cartridge it
/// initialises (labels are unique), so the only writers of a volume row's
/// tape are its own sessions.
pub fn lbp_refusal_seen(conn: &Connection, volume_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM events
                        WHERE entity_type = 'volume' AND entity_id = ?1 AND action = ?2)",
        params![volume_id, WRITE_LBP_REFUSAL],
        |r| r.get(0),
    )?)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0012 item 186: a `write_lbp_refusal` event on a volume is a
    /// refusal seen on that volume, and only on it; another action on it, or
    /// the same action on another volume, is not.
    #[test]
    fn a_refusal_is_seen_only_on_its_own_volume() {
        let conn = crate::db::open_memory().unwrap();
        assert!(!lbp_refusal_seen(&conn, 7).unwrap(), "an empty catalog");
        log_event(
            &conn,
            "volume",
            7,
            Some("L6-0007"),
            "write_lbp_retry",
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        log_event(
            &conn,
            "unit",
            7,
            Some("u"),
            WRITE_LBP_REFUSAL,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(
            !lbp_refusal_seen(&conn, 7).unwrap(),
            "another action, or another entity type"
        );
        log_event(
            &conn,
            "volume",
            8,
            Some("L6-0008"),
            WRITE_LBP_REFUSAL,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(!lbp_refusal_seen(&conn, 7).unwrap(), "another volume");
        assert!(lbp_refusal_seen(&conn, 8).unwrap(), "positive control");
    }
}
