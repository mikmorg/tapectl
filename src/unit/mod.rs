pub mod discovery;
pub mod dotfile;
pub mod nesting;

use std::path::Path;

use rusqlite::Connection;
use uuid::Uuid;

use crate::config::TapectlPaths;
use crate::db::{events, queries};
use crate::error::{Result, TapectlError};

/// Initialize a single unit at the given directory path.
pub fn init_unit(
    conn: &Connection,
    _paths: &TapectlPaths,
    dir_path: &str,
    tenant_name: &str,
    name_override: Option<&str>,
    tags: &[String],
    archive_set: Option<&str>,
) -> Result<i64> {
    let abs_path = std::fs::canonicalize(dir_path)
        .map_err(|_| TapectlError::UnitPathNotFound(dir_path.to_string()))?;
    let abs_str = abs_path.to_string_lossy().to_string();

    // Check the directory exists
    if !abs_path.is_dir() {
        return Err(TapectlError::UnitPathNotFound(abs_str));
    }

    // Check for nesting conflicts
    nesting::check_nesting(conn, &abs_str)?;

    // Check for existing dotfile
    let dotfile_path = abs_path.join(".tapectl-unit.toml");
    if dotfile_path.exists() {
        return Err(TapectlError::UnitAlreadyExists(abs_str));
    }

    // Resolve tenant
    let tenant = crate::tenant::require_tenant(conn, tenant_name)?;

    // Resolve --archive-set to an id, validating it exists (issue #48): a
    // typo'd or nonexistent name must never be silently accepted and then
    // sit permanently inert as a NULL `archive_set_id` — the exact bug this
    // issue diagnosed. `None` (no --archive-set given) always resolves
    // cleanly to `None`, the common case.
    let archive_set_id = queries::resolve_archive_set(conn, archive_set)?;

    // Auto-name from path or use override
    let unit_name = if let Some(name) = name_override {
        name.to_string()
    } else {
        auto_name_from_path(&abs_str)
    };

    // Check name uniqueness
    if queries::get_unit_by_name(conn, &unit_name)?.is_some() {
        return Err(TapectlError::UnitAlreadyExists(unit_name));
    }

    let uuid = Uuid::new_v4().to_string();

    // Insert into DB
    let unit_id = queries::insert_unit(
        conn,
        &uuid,
        &unit_name,
        tenant.id,
        archive_set_id,
        &abs_str,
        "mtime_size",
        true,
    )?;
    events::log_created(conn, "unit", unit_id, &unit_name, Some(tenant.id))?;

    // Record initial path history
    queries::update_unit_path(conn, unit_id, &abs_str)?;

    // Add tags
    for tag in tags {
        queries::add_tag_to_unit(conn, unit_id, tag)?;
    }

    // Create dotfile
    let dotfile_data = dotfile::UnitDotfile {
        uuid: uuid.clone(),
        name: unit_name.clone(),
        created: chrono::Utc::now().to_rfc3339(),
        tags: tags.to_vec(),
        tenant: tenant_name.to_string(),
        archive_set: archive_set.map(|s| s.to_string()),
        checksum_mode: "mtime_size".to_string(),
        compression: "none".to_string(),
        exclude_patterns: Vec::new(),
    };
    dotfile::write_dotfile(&dotfile_path, &dotfile_data)?;

    Ok(unit_id)
}

/// Bulk-initialize units from a list of directories under a parent.
pub fn init_bulk(
    conn: &Connection,
    paths: &TapectlPaths,
    parent_dir: &str,
    tenant_name: &str,
    tags: &[String],
    _depth: usize,
) -> Result<Vec<(String, std::result::Result<i64, TapectlError>)>> {
    let parent = std::fs::canonicalize(parent_dir)
        .map_err(|_| TapectlError::UnitPathNotFound(parent_dir.to_string()))?;

    let mut results = Vec::new();

    // Walk immediate subdirectories (depth 1 by default)
    let entries = std::fs::read_dir(&parent)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Skip hidden directories
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }

        let path_str = path.to_string_lossy().to_string();
        let result = init_unit(conn, paths, &path_str, tenant_name, None, tags, None);
        results.push((path_str, result));
    }

    Ok(results)
}

/// Rename a unit.
pub fn rename_unit(conn: &Connection, current_name: &str, new_name: &str) -> Result<()> {
    let unit = queries::get_unit_by_name(conn, current_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(current_name.to_string()))?;

    // Check new name doesn't conflict
    if queries::get_unit_by_name(conn, new_name)?.is_some() {
        return Err(TapectlError::UnitAlreadyExists(new_name.to_string()));
    }

    queries::update_unit_name(conn, unit.id, new_name)?;

    events::log_field_change(
        conn,
        "unit",
        unit.id,
        new_name,
        "renamed",
        "name",
        Some(current_name),
        new_name,
        Some(unit.tenant_id),
    )?;

    // Update dotfile if path exists
    if let Some(ref path) = unit.current_path {
        let dotfile_path = Path::new(path).join(".tapectl-unit.toml");
        if dotfile_path.exists() {
            if let Ok(mut df) = dotfile::read_dotfile(&dotfile_path) {
                df.name = new_name.to_string();
                let _ = dotfile::write_dotfile(&dotfile_path, &df);
            }
        }
    }

    Ok(())
}

/// Generate an auto-name from a filesystem path.
/// Strips common prefixes like /media/, /mnt/ and joins remaining components with "/".
fn auto_name_from_path(path: &str) -> String {
    let path = Path::new(path);
    let components: Vec<&str> = path
        .components()
        .filter_map(|c| {
            if let std::path::Component::Normal(s) = c {
                Some(s.to_str().unwrap_or(""))
            } else {
                None
            }
        })
        .collect();

    // Strip common mount prefixes
    let skip = if components.first() == Some(&"media") || components.first() == Some(&"mnt") {
        1
    } else {
        0
    };

    components[skip..].join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_name_media() {
        assert_eq!(auto_name_from_path("/media/tv/bb/s01"), "tv/bb/s01");
    }

    #[test]
    fn test_auto_name_mnt() {
        assert_eq!(auto_name_from_path("/mnt/archive/photos"), "archive/photos");
    }

    #[test]
    fn test_auto_name_other() {
        assert_eq!(auto_name_from_path("/home/user/data"), "home/user/data");
    }

    // ── archive_set_id wiring (issue #48) ──

    fn harness() -> (Connection, tempfile::TempDir, TapectlPaths) {
        let conn = crate::db::open_memory().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        (conn, tmp, paths)
    }

    /// The end-to-end proof issue #48 asks for: a unit created with
    /// `--archive-set X` must have `archive_set_id` actually persisted (not
    /// just written to the dotfile, which is all that happened before this
    /// fix), and `policy::resolve` must then return X's values instead of
    /// silently falling back to the global defaults it would use forever
    /// if `archive_set_id` stayed NULL.
    ///
    /// Deliberately asserts on `min_copies`/`required_locations`, not
    /// `compression`/`checksum_mode`: `init_unit`'s dotfile always carries
    /// concrete values for the latter two (see the design doc's own §2.2
    /// example dotfile), and `policy::resolve`'s dotfile layer outranks
    /// archive_set whenever the dotfile is present — so for a real,
    /// dotfile-backed unit those two fields are structurally shadowed
    /// regardless of this fix. That is a separate, pre-existing defect
    /// (the dotfile *writer* baking in what looks like a per-unit
    /// override that was never actually chosen), not something #48 claims
    /// to fix — see the commit message / final report for the full
    /// analysis. `min_copies` and `required_locations` are never written
    /// into the dotfile, so they resolve through archive_set exactly as
    /// designed and are the correct fields to prove this wiring on.
    #[test]
    fn init_unit_with_archive_set_persists_id_and_unlocks_policy_layer_2() {
        let (conn, tmp, paths) = harness();
        conn.execute(
            "INSERT INTO archive_sets (name, min_copies, required_locations) \
             VALUES ('cold', 5, '[\"vault\"]')",
            [],
        )
        .unwrap();

        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();

        let unit_id = init_unit(
            &conn,
            &paths,
            src.to_str().unwrap(),
            "alice",
            Some("unit1"),
            &[],
            Some("cold"),
        )
        .unwrap();

        let unit = queries::get_unit_by_name(&conn, "unit1").unwrap().unwrap();
        assert_eq!(unit.id, unit_id);
        assert!(
            unit.archive_set_id.is_some(),
            "archive_set_id must be persisted to the DB, not left NULL"
        );

        let config = crate::config::Config::default();
        let resolved = crate::policy::resolve(&conn, &config, &unit);
        assert_eq!(
            resolved.min_copies, 5,
            "must resolve the archive_set's min_copies, not the global default (2)"
        );
        assert_eq!(
            resolved.required_locations,
            vec!["vault".to_string()],
            "must resolve the archive_set's required_locations, not the global default (empty)"
        );
    }

    /// Issue #48 item 2: a typo'd or nonexistent `--archive-set` name must
    /// never be silently accepted and left permanently inert — it must
    /// fail loudly, naming the unknown set, and the unit must not be
    /// created at all (no partial/incorrect state).
    #[test]
    fn init_unit_rejects_unknown_archive_set() {
        let (conn, tmp, paths) = harness();

        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();

        let err = init_unit(
            &conn,
            &paths,
            src.to_str().unwrap(),
            "alice",
            Some("unit1"),
            &[],
            Some("nonexistent"),
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("nonexistent"),
            "error must name the unknown archive set, got: {msg}"
        );
        assert!(
            queries::get_unit_by_name(&conn, "unit1").unwrap().is_none(),
            "no unit row should be created when --archive-set is invalid"
        );
    }

    /// The common case, and issue #48's own explicit "do NOT break this"
    /// requirement: no `--archive-set` at all must keep resolving cleanly
    /// to system defaults, completely unaffected by this fix.
    #[test]
    fn init_unit_without_archive_set_still_resolves_defaults() {
        let (conn, tmp, paths) = harness();

        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();

        let unit_id = init_unit(
            &conn,
            &paths,
            src.to_str().unwrap(),
            "alice",
            Some("unit1"),
            &[],
            None,
        )
        .unwrap();

        let unit = queries::get_unit_by_name(&conn, "unit1").unwrap().unwrap();
        assert_eq!(unit.id, unit_id);
        assert!(unit.archive_set_id.is_none());

        let config = crate::config::Config::default();
        let resolved = crate::policy::resolve(&conn, &config, &unit);
        assert_eq!(resolved.min_copies, 2);
        assert_eq!(resolved.compression, "none");
    }
}
