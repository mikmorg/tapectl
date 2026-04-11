use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;

use super::events;
use super::models::{EncryptionKey, Tenant, Unit};

// ── Tenants ──

pub fn insert_tenant(
    conn: &Connection,
    name: &str,
    description: Option<&str>,
    is_operator: bool,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO tenants (name, description, is_operator) VALUES (?1, ?2, ?3)",
        params![name, description, is_operator],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn get_tenant_by_name(conn: &Connection, name: &str) -> Result<Option<Tenant>> {
    conn.query_row(
        "SELECT id, name, description, is_operator, status, created_at, notes
         FROM tenants WHERE name = ?1",
        params![name],
        |row| {
            Ok(Tenant {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                is_operator: row.get(3)?,
                status: row.get(4)?,
                created_at: row.get(5)?,
                notes: row.get(6)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

pub fn get_tenant_by_id(conn: &Connection, id: i64) -> Result<Option<Tenant>> {
    conn.query_row(
        "SELECT id, name, description, is_operator, status, created_at, notes
         FROM tenants WHERE id = ?1",
        params![id],
        |row| {
            Ok(Tenant {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                is_operator: row.get(3)?,
                status: row.get(4)?,
                created_at: row.get(5)?,
                notes: row.get(6)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

pub fn list_tenants(conn: &Connection, include_deleted: bool) -> Result<Vec<Tenant>> {
    let sql = if include_deleted {
        "SELECT id, name, description, is_operator, status, created_at, notes
         FROM tenants ORDER BY name"
    } else {
        "SELECT id, name, description, is_operator, status, created_at, notes
         FROM tenants WHERE status != 'deleted' ORDER BY name"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(Tenant {
            id: row.get(0)?,
            name: row.get(1)?,
            description: row.get(2)?,
            is_operator: row.get(3)?,
            status: row.get(4)?,
            created_at: row.get(5)?,
            notes: row.get(6)?,
        })
    })?;
    let mut tenants = Vec::new();
    for row in rows {
        tenants.push(row?);
    }
    Ok(tenants)
}

pub fn get_operator_tenant(conn: &Connection) -> Result<Option<Tenant>> {
    conn.query_row(
        "SELECT id, name, description, is_operator, status, created_at, notes
         FROM tenants WHERE is_operator = 1 LIMIT 1",
        [],
        |row| {
            Ok(Tenant {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                is_operator: row.get(3)?,
                status: row.get(4)?,
                created_at: row.get(5)?,
                notes: row.get(6)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

// ── Encryption Keys ──

pub fn insert_key(
    conn: &Connection,
    tenant_id: i64,
    alias: &str,
    fingerprint: &str,
    public_key: &str,
    key_type: &str,
    description: Option<&str>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key, key_type, description)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![tenant_id, alias, fingerprint, public_key, key_type, description],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn list_keys_for_tenant(conn: &Connection, tenant_id: i64) -> Result<Vec<EncryptionKey>> {
    let mut stmt = conn.prepare(
        "SELECT id, tenant_id, alias, fingerprint, public_key, key_type,
                is_active, created_at, description
         FROM encryption_keys WHERE tenant_id = ?1 ORDER BY alias",
    )?;
    let rows = stmt.query_map(params![tenant_id], |row| {
        Ok(EncryptionKey {
            id: row.get(0)?,
            tenant_id: row.get(1)?,
            alias: row.get(2)?,
            fingerprint: row.get(3)?,
            public_key: row.get(4)?,
            key_type: row.get(5)?,
            is_active: row.get(6)?,
            created_at: row.get(7)?,
            description: row.get(8)?,
        })
    })?;
    let mut keys = Vec::new();
    for row in rows {
        keys.push(row?);
    }
    Ok(keys)
}

pub fn get_active_keys_for_tenant(conn: &Connection, tenant_id: i64) -> Result<Vec<EncryptionKey>> {
    let mut stmt = conn.prepare(
        "SELECT id, tenant_id, alias, fingerprint, public_key, key_type,
                is_active, created_at, description
         FROM encryption_keys WHERE tenant_id = ?1 AND is_active = 1 ORDER BY alias",
    )?;
    let rows = stmt.query_map(params![tenant_id], |row| {
        Ok(EncryptionKey {
            id: row.get(0)?,
            tenant_id: row.get(1)?,
            alias: row.get(2)?,
            fingerprint: row.get(3)?,
            public_key: row.get(4)?,
            key_type: row.get(5)?,
            is_active: row.get(6)?,
            created_at: row.get(7)?,
            description: row.get(8)?,
        })
    })?;
    let mut keys = Vec::new();
    for row in rows {
        keys.push(row?);
    }
    Ok(keys)
}

pub fn get_key_by_alias(conn: &Connection, alias: &str) -> Result<Option<EncryptionKey>> {
    conn.query_row(
        "SELECT id, tenant_id, alias, fingerprint, public_key, key_type,
                is_active, created_at, description
         FROM encryption_keys WHERE alias = ?1",
        params![alias],
        |row| {
            Ok(EncryptionKey {
                id: row.get(0)?,
                tenant_id: row.get(1)?,
                alias: row.get(2)?,
                fingerprint: row.get(3)?,
                public_key: row.get(4)?,
                key_type: row.get(5)?,
                is_active: row.get(6)?,
                created_at: row.get(7)?,
                description: row.get(8)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

// ── Units ──

pub fn insert_unit(
    conn: &Connection,
    uuid: &str,
    name: &str,
    tenant_id: i64,
    current_path: &str,
    checksum_mode: &str,
    encrypt: bool,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, current_path, checksum_mode, encrypt)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![uuid, name, tenant_id, current_path, checksum_mode, encrypt],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn get_unit_by_name(conn: &Connection, name: &str) -> Result<Option<Unit>> {
    conn.query_row(
        "SELECT id, uuid, name, tenant_id, archive_set_id, current_path,
                checksum_mode, encrypt, status, created_at, last_scanned, notes
         FROM units WHERE name = ?1",
        params![name],
        |row| {
            Ok(Unit {
                id: row.get(0)?,
                uuid: row.get(1)?,
                name: row.get(2)?,
                tenant_id: row.get(3)?,
                archive_set_id: row.get(4)?,
                current_path: row.get(5)?,
                checksum_mode: row.get(6)?,
                encrypt: row.get(7)?,
                status: row.get(8)?,
                created_at: row.get(9)?,
                last_scanned: row.get(10)?,
                notes: row.get(11)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

pub fn get_unit_by_uuid(conn: &Connection, uuid: &str) -> Result<Option<Unit>> {
    conn.query_row(
        "SELECT id, uuid, name, tenant_id, archive_set_id, current_path,
                checksum_mode, encrypt, status, created_at, last_scanned, notes
         FROM units WHERE uuid = ?1",
        params![uuid],
        |row| {
            Ok(Unit {
                id: row.get(0)?,
                uuid: row.get(1)?,
                name: row.get(2)?,
                tenant_id: row.get(3)?,
                archive_set_id: row.get(4)?,
                current_path: row.get(5)?,
                checksum_mode: row.get(6)?,
                encrypt: row.get(7)?,
                status: row.get(8)?,
                created_at: row.get(9)?,
                last_scanned: row.get(10)?,
                notes: row.get(11)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

pub fn get_unit_by_path(conn: &Connection, path: &str) -> Result<Option<Unit>> {
    conn.query_row(
        "SELECT id, uuid, name, tenant_id, archive_set_id, current_path,
                checksum_mode, encrypt, status, created_at, last_scanned, notes
         FROM units WHERE current_path = ?1",
        params![path],
        |row| {
            Ok(Unit {
                id: row.get(0)?,
                uuid: row.get(1)?,
                name: row.get(2)?,
                tenant_id: row.get(3)?,
                archive_set_id: row.get(4)?,
                current_path: row.get(5)?,
                checksum_mode: row.get(6)?,
                encrypt: row.get(7)?,
                status: row.get(8)?,
                created_at: row.get(9)?,
                last_scanned: row.get(10)?,
                notes: row.get(11)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

pub fn list_units(
    conn: &Connection,
    tenant_id: Option<i64>,
    status_filter: Option<&str>,
) -> Result<Vec<Unit>> {
    let mut sql = String::from(
        "SELECT id, uuid, name, tenant_id, archive_set_id, current_path,
                checksum_mode, encrypt, status, created_at, last_scanned, notes
         FROM units WHERE 1=1",
    );
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(tid) = tenant_id {
        sql.push_str(" AND tenant_id = ?");
        param_values.push(Box::new(tid));
    }
    if let Some(status) = status_filter {
        sql.push_str(" AND status = ?");
        param_values.push(Box::new(status.to_string()));
    }
    sql.push_str(" ORDER BY name");

    let params: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params.as_slice(), |row| {
        Ok(Unit {
            id: row.get(0)?,
            uuid: row.get(1)?,
            name: row.get(2)?,
            tenant_id: row.get(3)?,
            archive_set_id: row.get(4)?,
            current_path: row.get(5)?,
            checksum_mode: row.get(6)?,
            encrypt: row.get(7)?,
            status: row.get(8)?,
            created_at: row.get(9)?,
            last_scanned: row.get(10)?,
            notes: row.get(11)?,
        })
    })?;
    let mut units = Vec::new();
    for row in rows {
        units.push(row?);
    }
    Ok(units)
}

pub fn update_unit_name(conn: &Connection, unit_id: i64, new_name: &str) -> Result<()> {
    let old_name: Option<String> = conn
        .query_row(
            "SELECT name FROM units WHERE id = ?1",
            params![unit_id],
            |row| row.get(0),
        )
        .optional()?;
    conn.execute(
        "UPDATE units SET name = ?1 WHERE id = ?2",
        params![new_name, unit_id],
    )?;
    events::log_field_change(
        conn,
        "unit",
        unit_id,
        new_name,
        "rename",
        "name",
        old_name.as_deref(),
        new_name,
        None,
    )?;
    Ok(())
}

pub fn update_unit_path(conn: &Connection, unit_id: i64, path: &str) -> Result<()> {
    let (name, old_path): (String, Option<String>) = conn.query_row(
        "SELECT name, current_path FROM units WHERE id = ?1",
        params![unit_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    conn.execute(
        "UPDATE units SET current_path = ?1 WHERE id = ?2",
        params![path, unit_id],
    )?;
    // Record path history
    conn.execute(
        "INSERT INTO unit_path_history (unit_id, path) VALUES (?1, ?2)",
        params![unit_id, path],
    )?;
    events::log_field_change(
        conn,
        "unit",
        unit_id,
        &name,
        "path_change",
        "current_path",
        old_path.as_deref(),
        path,
        None,
    )?;
    Ok(())
}

// ── Tags ──

pub fn get_or_create_tag(conn: &Connection, name: &str) -> Result<i64> {
    if let Some(id) = conn
        .query_row(
            "SELECT id FROM tags WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )
        .optional()?
    {
        return Ok(id);
    }
    conn.execute("INSERT INTO tags (name) VALUES (?1)", params![name])?;
    Ok(conn.last_insert_rowid())
}

pub fn add_tag_to_unit(conn: &Connection, unit_id: i64, tag_name: &str) -> Result<()> {
    let tag_id = get_or_create_tag(conn, tag_name)?;
    conn.execute(
        "INSERT OR IGNORE INTO unit_tags (unit_id, tag_id) VALUES (?1, ?2)",
        params![unit_id, tag_id],
    )?;
    Ok(())
}

pub fn remove_tag_from_unit(conn: &Connection, unit_id: i64, tag_name: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM unit_tags WHERE unit_id = ?1
         AND tag_id = (SELECT id FROM tags WHERE name = ?2)",
        params![unit_id, tag_name],
    )?;
    Ok(())
}

pub fn get_tags_for_unit(conn: &Connection, unit_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT t.name FROM tags t
         JOIN unit_tags ut ON ut.tag_id = t.id
         WHERE ut.unit_id = ?1 ORDER BY t.name",
    )?;
    let rows = stmt.query_map(params![unit_id], |row| row.get(0))?;
    let mut tags = Vec::new();
    for row in rows {
        tags.push(row?);
    }
    Ok(tags)
}

// ── Unit count for tenant (used by tenant delete guard) ──

pub fn count_active_units_for_tenant(conn: &Connection, tenant_id: i64) -> Result<i64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM units WHERE tenant_id = ?1 AND status = 'active'",
        params![tenant_id],
        |row| row.get(0),
    )?;
    Ok(count)
}

/// Check if any unit in the database has a path that is a parent or child of the given path.
pub fn check_nesting_conflict(conn: &Connection, path: &str) -> Result<Option<String>> {
    let mut stmt =
        conn.prepare("SELECT name, current_path FROM units WHERE status != 'retired'")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })?;

    let check_path = std::path::Path::new(path);
    for row in rows {
        let (name, existing_path) = row?;
        if let Some(existing) = existing_path {
            let existing = std::path::Path::new(&existing);
            if check_path.starts_with(existing) {
                return Ok(Some(format!(
                    "{path} is inside existing unit \"{name}\" at {}",
                    existing.display()
                )));
            }
            if existing.starts_with(check_path) {
                return Ok(Some(format!(
                    "existing unit \"{name}\" at {} is inside {path}",
                    existing.display()
                )));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        let schema = include_str!("migrations/001_initial.sql");
        conn.execute_batch(schema).unwrap();
        conn
    }

    #[test]
    fn insert_and_get_tenant_by_name() {
        let conn = fresh_conn();
        let id = insert_tenant(&conn, "alice", Some("test user"), false).unwrap();
        assert!(id > 0);
        let got = get_tenant_by_name(&conn, "alice").unwrap().unwrap();
        assert_eq!(got.id, id);
        assert_eq!(got.name, "alice");
        assert_eq!(got.description.as_deref(), Some("test user"));
        assert!(!got.is_operator);
    }

    #[test]
    fn get_tenant_by_name_missing() {
        let conn = fresh_conn();
        assert!(get_tenant_by_name(&conn, "nope").unwrap().is_none());
    }

    #[test]
    fn get_operator_tenant_finds_only_operator() {
        let conn = fresh_conn();
        insert_tenant(&conn, "alice", None, false).unwrap();
        let op_id = insert_tenant(&conn, "operator", None, true).unwrap();
        let op = get_operator_tenant(&conn).unwrap().unwrap();
        assert_eq!(op.id, op_id);
        assert!(op.is_operator);
    }

    #[test]
    fn update_unit_name_and_path_and_history() {
        let conn = fresh_conn();
        let tid = insert_tenant(&conn, "op", None, true).unwrap();
        let uid =
            insert_unit(&conn, "u-uuid", "origname", tid, "/old/path", "mtime_size", true).unwrap();

        update_unit_name(&conn, uid, "newname").unwrap();
        let u = get_unit_by_name(&conn, "newname").unwrap().unwrap();
        assert_eq!(u.id, uid);
        assert!(get_unit_by_name(&conn, "origname").unwrap().is_none());

        update_unit_path(&conn, uid, "/new/path").unwrap();
        let u = get_unit_by_path(&conn, "/new/path").unwrap().unwrap();
        assert_eq!(u.id, uid);

        let history: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM unit_path_history WHERE unit_id = ?1",
                params![uid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(history, 1);
    }

    #[test]
    fn get_or_create_tag_is_idempotent() {
        let conn = fresh_conn();
        let a = get_or_create_tag(&conn, "media").unwrap();
        let b = get_or_create_tag(&conn, "media").unwrap();
        assert_eq!(a, b);
        let c = get_or_create_tag(&conn, "photos").unwrap();
        assert_ne!(a, c);
    }

    #[test]
    fn add_and_remove_tag_for_unit() {
        let conn = fresh_conn();
        let tid = insert_tenant(&conn, "op", None, true).unwrap();
        let uid = insert_unit(&conn, "u1", "u1", tid, "/p", "mtime_size", true).unwrap();

        add_tag_to_unit(&conn, uid, "media").unwrap();
        add_tag_to_unit(&conn, uid, "media").unwrap(); // idempotent via INSERT OR IGNORE
        add_tag_to_unit(&conn, uid, "photos").unwrap();
        let tags = get_tags_for_unit(&conn, uid).unwrap();
        assert_eq!(tags, vec!["media".to_string(), "photos".to_string()]);

        remove_tag_from_unit(&conn, uid, "media").unwrap();
        let tags = get_tags_for_unit(&conn, uid).unwrap();
        assert_eq!(tags, vec!["photos".to_string()]);
    }

    #[test]
    fn count_active_units_for_tenant_counts_only_active() {
        let conn = fresh_conn();
        let tid = insert_tenant(&conn, "op", None, true).unwrap();
        insert_unit(&conn, "u1", "u1", tid, "/a", "mtime_size", true).unwrap();
        insert_unit(&conn, "u2", "u2", tid, "/b", "mtime_size", true).unwrap();
        // Mark one retired
        conn.execute(
            "UPDATE units SET status='retired' WHERE name='u2'",
            [],
        )
        .unwrap();
        assert_eq!(count_active_units_for_tenant(&conn, tid).unwrap(), 1);
    }

    #[test]
    fn check_nesting_conflict_detects_parent_and_child() {
        let conn = fresh_conn();
        let tid = insert_tenant(&conn, "op", None, true).unwrap();
        insert_unit(&conn, "u1", "u1", tid, "/data/photos", "mtime_size", true).unwrap();

        // Child-of-existing
        assert!(check_nesting_conflict(&conn, "/data/photos/2024")
            .unwrap()
            .is_some());
        // Parent-of-existing
        assert!(check_nesting_conflict(&conn, "/data").unwrap().is_some());
        // Unrelated
        assert!(check_nesting_conflict(&conn, "/other").unwrap().is_none());
    }
}
