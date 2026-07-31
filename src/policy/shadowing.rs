//! Advisory scan for dotfiles that still shadow archive_set policy fields.
//!
//! Ratified fix for issue #92 ("Recast of v4.0 §2.2" in
//! `docs/design-errata.md`): newly-written dotfiles omit `[policy]`
//! `checksum_mode`/`compression` unless the operator sets them explicitly,
//! so those fields defer to archive_set/defaults. Pre-existing dotfiles
//! written before this fix (or hand-edited since) may still carry concrete
//! values that shadow their archive set — those are left alone (the
//! operator owns the file) but `config check` should flag them.

use std::path::PathBuf;

use rusqlite::Connection;

use crate::unit::dotfile;

/// A unit whose on-disk dotfile still sets one or both `[policy]` fields,
/// shadowing its archive_set (if any).
#[derive(Debug, Clone)]
pub struct ShadowingDotfile {
    pub unit_name: String,
    pub dotfile_path: PathBuf,
    pub checksum_mode_set: bool,
    pub compression_set: bool,
}

/// Scan every unit with a known `current_path` for a `.tapectl-unit.toml`
/// whose `[policy]` table sets `checksum_mode` and/or `compression`.
/// Unreadable or malformed dotfiles are skipped silently — this is
/// advisory, like the policy audit, never a hard error.
pub fn scan(conn: &Connection) -> Vec<ShadowingDotfile> {
    let mut out = Vec::new();

    let mut stmt =
        match conn.prepare("SELECT name, current_path FROM units WHERE current_path IS NOT NULL") {
            Ok(s) => s,
            Err(_) => return out,
        };

    let rows = match stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) {
        Ok(r) => r,
        Err(_) => return out,
    };

    for row in rows.flatten() {
        let (unit_name, current_path) = row;
        let dotfile_path = std::path::Path::new(&current_path).join(".tapectl-unit.toml");
        if !dotfile_path.exists() {
            continue;
        }
        let Ok(df) = dotfile::read_dotfile(&dotfile_path) else {
            continue;
        };
        let checksum_mode_set = df.checksum_mode.is_some();
        let compression_set = df.compression.is_some();
        if checksum_mode_set || compression_set {
            out.push(ShadowingDotfile {
                unit_name,
                dotfile_path,
                checksum_mode_set,
                compression_set,
            });
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::dotfile::UnitDotfile;
    use tempfile::TempDir;

    fn fresh_conn() -> Connection {
        crate::db::open_memory().unwrap()
    }

    fn insert_unit(conn: &Connection, name: &str, path: &str) {
        conn.execute("INSERT INTO tenants (name) VALUES ('alice')", [])
            .ok();
        let tenant_id: i64 = conn
            .query_row("SELECT id FROM tenants WHERE name = 'alice'", [], |row| {
                row.get(0)
            })
            .unwrap();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, checksum_mode, encrypt)
             VALUES (?1, ?2, ?3, ?4, 'mtime_size', 1)",
            rusqlite::params![uuid::Uuid::new_v4().to_string(), name, tenant_id, path],
        )
        .unwrap();
    }

    fn base_dotfile() -> UnitDotfile {
        UnitDotfile {
            uuid: uuid::Uuid::new_v4().to_string(),
            name: "u".into(),
            created: "2026-01-01T00:00:00Z".into(),
            tags: vec![],
            tenant: "alice".into(),
            archive_set: None,
            checksum_mode: None,
            compression: None,
            warehouse_copies: None,
            exclude_patterns: vec![],
        }
    }

    #[test]
    fn unit_with_policy_bearing_dotfile_is_reported() {
        let conn = fresh_conn();
        let tmp = TempDir::new().unwrap();
        let mut df = base_dotfile();
        df.compression = Some("gzip".into());
        dotfile::write_dotfile(&tmp.path().join(".tapectl-unit.toml"), &df).unwrap();

        insert_unit(&conn, "photos", tmp.path().to_str().unwrap());

        let hits = scan(&conn);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].unit_name, "photos");
        assert!(hits[0].compression_set);
        assert!(!hits[0].checksum_mode_set);
    }

    #[test]
    fn unit_with_policy_free_dotfile_is_not_reported() {
        let conn = fresh_conn();
        let tmp = TempDir::new().unwrap();
        let df = base_dotfile();
        dotfile::write_dotfile(&tmp.path().join(".tapectl-unit.toml"), &df).unwrap();

        insert_unit(&conn, "docs", tmp.path().to_str().unwrap());

        assert!(scan(&conn).is_empty());
    }

    #[test]
    fn unit_with_no_dotfile_is_not_reported() {
        let conn = fresh_conn();
        let tmp = TempDir::new().unwrap();

        insert_unit(&conn, "orphan", tmp.path().to_str().unwrap());

        assert!(scan(&conn).is_empty());
    }
}
