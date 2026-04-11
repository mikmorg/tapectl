use rusqlite::{params, Connection};

use crate::config::Config;
use crate::db::models::Unit;

/// Resolved policy for a unit after 3-level resolution:
/// unit dotfile [policy] > archive_set > system [defaults].
#[derive(Debug, Clone)]
pub struct ResolvedPolicy {
    pub min_copies: i64,
    pub required_locations: Vec<String>,
    pub encrypt: bool,
    pub compression: String,
    pub checksum_mode: String,
    pub slice_size: i64,
    pub verify_interval_days: Option<i64>,
    pub preserve_xattrs: bool,
    pub preserve_acls: bool,
    pub preserve_fsa: bool,
    pub dirty_on_metadata_change: bool,
}

/// Resolve the effective policy for a unit.
///
/// Resolution order (first non-NULL wins):
/// 1. Unit dotfile [policy] section (read from disk if available)
/// 2. Archive set (from DB via unit.archive_set_id)
/// 3. System defaults (from config.toml)
pub fn resolve(conn: &Connection, config: &Config, unit: &Unit) -> ResolvedPolicy {
    let defaults = &config.defaults;

    // Start with system defaults
    let mut policy = ResolvedPolicy {
        min_copies: defaults.min_copies_for_tape_only as i64,
        required_locations: Vec::new(),
        encrypt: defaults.encrypt,
        compression: defaults.compression.clone(),
        checksum_mode: defaults.checksum_mode.clone(),
        slice_size: crate::staging::parse_size_to_bytes(&defaults.slice_size),
        verify_interval_days: None,
        preserve_xattrs: defaults.preserve_xattrs,
        preserve_acls: defaults.preserve_acls,
        preserve_fsa: defaults.preserve_fsa,
        dirty_on_metadata_change: defaults.dirty_on_metadata_change,
    };

    // Layer 2: Archive set (if unit has one)
    if let Some(as_id) = unit.archive_set_id {
        if let Ok(row) = conn.query_row(
            "SELECT min_copies, required_locations, encrypt, compression, checksum_mode,
                    slice_size, verify_interval_days, preserve_xattrs, preserve_acls,
                    preserve_fsa, dirty_on_metadata_change
             FROM archive_sets WHERE id = ?1",
            params![as_id],
            |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                    row.get::<_, Option<i64>>(9)?,
                    row.get::<_, Option<i64>>(10)?,
                ))
            },
        ) {
            let (
                min_copies,
                locations_json,
                encrypt,
                compression,
                checksum_mode,
                slice_size,
                verify_days,
                preserve_xattrs,
                preserve_acls,
                preserve_fsa,
                dirty_on_meta,
            ) = row;

            if let Some(v) = min_copies {
                policy.min_copies = v;
            }
            if let Some(locs) = locations_json {
                if let Ok(arr) = serde_json::from_str::<Vec<String>>(&locs) {
                    policy.required_locations = arr;
                }
            }
            if let Some(v) = encrypt {
                policy.encrypt = v != 0;
            }
            if let Some(v) = compression {
                policy.compression = v;
            }
            if let Some(v) = checksum_mode {
                policy.checksum_mode = v;
            }
            if let Some(v) = slice_size {
                policy.slice_size = v;
            }
            if let Some(v) = verify_days {
                policy.verify_interval_days = Some(v);
            }
            if let Some(v) = preserve_xattrs {
                policy.preserve_xattrs = v != 0;
            }
            if let Some(v) = preserve_acls {
                policy.preserve_acls = v != 0;
            }
            if let Some(v) = preserve_fsa {
                policy.preserve_fsa = v != 0;
            }
            if let Some(v) = dirty_on_meta {
                policy.dirty_on_metadata_change = v != 0;
            }
        }
    }

    // Layer 1: Unit dotfile [policy] section (highest priority)
    // Read from disk if the unit has a path
    if let Some(ref path) = unit.current_path {
        let dotfile_path = std::path::Path::new(path).join(".tapectl-unit.toml");
        if dotfile_path.exists() {
            if let Ok(contents) = std::fs::read_to_string(&dotfile_path) {
                if let Ok(toml) = contents.parse::<toml::Table>() {
                    if let Some(pol) = toml.get("policy").and_then(|v| v.as_table()) {
                        if let Some(v) = pol.get("checksum_mode").and_then(|v| v.as_str()) {
                            policy.checksum_mode = v.to_string();
                        }
                        if let Some(v) = pol.get("compression").and_then(|v| v.as_str()) {
                            policy.compression = v.to_string();
                        }
                        if let Some(v) = pol.get("slice_size").and_then(|v| v.as_str()) {
                            policy.slice_size = crate::staging::parse_size_to_bytes(v);
                        }
                    }
                }
            }
        }
    }

    policy
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::Unit;
    use tempfile::TempDir;

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        let schema = include_str!("../db/migrations/001_initial.sql");
        conn.execute_batch(schema).unwrap();
        conn
    }

    fn make_unit(archive_set_id: Option<i64>, path: Option<String>) -> Unit {
        Unit {
            id: 1,
            uuid: "u".into(),
            name: "unit".into(),
            tenant_id: 1,
            archive_set_id,
            current_path: path,
            checksum_mode: "mtime_size".into(),
            encrypt: true,
            status: "active".into(),
            created_at: "2026-01-01".into(),
            last_scanned: None,
            notes: None,
        }
    }

    #[test]
    fn resolve_defaults_when_no_archive_set_no_dotfile() {
        let conn = fresh_conn();
        let config = Config::default();
        let unit = make_unit(None, None);

        let p = resolve(&conn, &config, &unit);
        assert_eq!(p.min_copies, 2);
        assert_eq!(p.checksum_mode, "mtime_size");
        assert_eq!(p.compression, "none");
        assert!(p.encrypt);
        assert!(p.required_locations.is_empty());
        assert!(p.verify_interval_days.is_none());
    }

    #[test]
    fn resolve_archive_set_overrides_defaults() {
        let conn = fresh_conn();
        conn.execute(
            "INSERT INTO archive_sets (name, min_copies, required_locations, compression, checksum_mode, verify_interval_days)
             VALUES ('media', 3, '[\"home\",\"offsite\"]', 'lzma', 'sha256', 90)",
            [],
        )
        .unwrap();
        let as_id = conn.last_insert_rowid();

        let config = Config::default();
        let unit = make_unit(Some(as_id), None);
        let p = resolve(&conn, &config, &unit);

        assert_eq!(p.min_copies, 3);
        assert_eq!(p.compression, "lzma");
        assert_eq!(p.checksum_mode, "sha256");
        assert_eq!(p.required_locations, vec!["home", "offsite"]);
        assert_eq!(p.verify_interval_days, Some(90));
    }

    #[test]
    fn resolve_archive_set_null_fields_inherit_defaults() {
        let conn = fresh_conn();
        // Only min_copies set; everything else NULL must fall through to defaults.
        conn.execute(
            "INSERT INTO archive_sets (name, min_copies) VALUES ('partial', 4)",
            [],
        )
        .unwrap();
        let as_id = conn.last_insert_rowid();

        let config = Config::default();
        let unit = make_unit(Some(as_id), None);
        let p = resolve(&conn, &config, &unit);

        assert_eq!(p.min_copies, 4);
        assert_eq!(p.compression, "none"); // from defaults
        assert_eq!(p.checksum_mode, "mtime_size"); // from defaults
    }

    #[test]
    fn resolve_dotfile_overrides_archive_set() {
        let conn = fresh_conn();
        conn.execute(
            "INSERT INTO archive_sets (name, compression, checksum_mode)
             VALUES ('media', 'lzma', 'sha256')",
            [],
        )
        .unwrap();
        let as_id = conn.last_insert_rowid();

        let tmp = TempDir::new().unwrap();
        let unit_path = tmp.path().to_str().unwrap().to_string();
        std::fs::write(
            tmp.path().join(".tapectl-unit.toml"),
            r#"
[policy]
checksum_mode = "full_hash"
compression = "gzip"
slice_size = "500M"
"#,
        )
        .unwrap();

        let config = Config::default();
        let unit = make_unit(Some(as_id), Some(unit_path));
        let p = resolve(&conn, &config, &unit);

        // Dotfile wins
        assert_eq!(p.checksum_mode, "full_hash");
        assert_eq!(p.compression, "gzip");
        assert_eq!(p.slice_size, 500 * 1024 * 1024);
    }

    #[test]
    fn resolve_missing_archive_set_id_falls_back_to_defaults() {
        let conn = fresh_conn();
        let config = Config::default();
        // archive_set_id points to a non-existent row — resolver must not panic
        let unit = make_unit(Some(999), None);
        let p = resolve(&conn, &config, &unit);
        assert_eq!(p.min_copies, 2);
    }
}
