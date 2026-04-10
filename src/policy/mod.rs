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
