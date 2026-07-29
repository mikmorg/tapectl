use std::path::Path;

use rusqlite::Connection;
use tracing::{info, warn};
use walkdir::WalkDir;

use crate::db::{events, queries};
use crate::error::Result;

use super::dotfile;

/// Scan watch_roots for .tapectl-unit.toml dotfiles and sync with DB.
/// Returns the number of units discovered/updated.
pub fn discover(conn: &Connection, watch_roots: &[String]) -> Result<DiscoverReport> {
    let mut report = DiscoverReport::default();

    for root in watch_roots {
        let root_path = Path::new(root);
        if !root_path.is_dir() {
            warn!(root = %root, "watch root does not exist, skipping");
            report.skipped_roots.push(root.clone());
            continue;
        }

        for entry in WalkDir::new(root_path)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_name() != ".tapectl-unit.toml" {
                continue;
            }

            let dotfile_path = entry.path();
            let unit_dir = match dotfile_path.parent() {
                Some(p) => p,
                None => continue,
            };

            match dotfile::read_dotfile(dotfile_path) {
                Ok(df) => {
                    let dir_str = unit_dir.to_string_lossy().to_string();
                    match sync_discovered_unit(conn, &df, &dir_str) {
                        Ok(SyncAction::Created) => {
                            info!(uuid = %df.uuid, name = %df.name, "discovered new unit");
                            report.created += 1;
                        }
                        Ok(SyncAction::Updated) => {
                            info!(uuid = %df.uuid, name = %df.name, "updated unit path");
                            report.updated += 1;
                        }
                        Ok(SyncAction::Unchanged) => {
                            report.unchanged += 1;
                        }
                        Err(e) => {
                            warn!(path = %dotfile_path.display(), error = %e, "failed to sync unit");
                            report
                                .errors
                                .push(format!("{}: {e}", dotfile_path.display()));
                        }
                    }
                }
                Err(e) => {
                    warn!(path = %dotfile_path.display(), error = %e, "failed to read dotfile");
                    report
                        .errors
                        .push(format!("{}: {e}", dotfile_path.display()));
                }
            }
        }
    }

    Ok(report)
}

#[derive(Debug, Default)]
pub struct DiscoverReport {
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub skipped_roots: Vec<String>,
    pub errors: Vec<String>,
}

enum SyncAction {
    Created,
    Updated,
    Unchanged,
}

/// Sync a discovered dotfile with the database. DB wins on conflict per design.
fn sync_discovered_unit(
    conn: &Connection,
    df: &dotfile::UnitDotfile,
    dir_path: &str,
) -> crate::error::Result<SyncAction> {
    // Look up by UUID first
    if let Some(existing) = queries::get_unit_by_uuid(conn, &df.uuid)? {
        // Unit exists — check if path changed
        if existing.current_path.as_deref() != Some(dir_path) {
            queries::update_unit_path(conn, existing.id, dir_path)?;
            events::log_field_change(
                conn,
                "unit",
                existing.id,
                &existing.name,
                "discover_path_update",
                "current_path",
                existing.current_path.as_deref(),
                dir_path,
                Some(existing.tenant_id),
            )?;
            return Ok(SyncAction::Updated);
        }
        return Ok(SyncAction::Unchanged);
    }

    // Unit not in DB — resolve tenant and register
    let tenant = match queries::get_tenant_by_name(conn, &df.tenant)? {
        Some(t) => t,
        None => {
            return Err(crate::error::TapectlError::TenantNotFound(
                df.tenant.clone(),
            ));
        }
    };

    // Resolve the dotfile's own `archive_set` the same way `unit init`
    // does (issue #48 item 3), so discovery and explicit init agree. A
    // dotfile naming an archive set that no longer exists (deleted since
    // the dotfile was written, or hand-edited) surfaces as an `Err` here,
    // which the caller (`discover`) already treats as a per-unit failure —
    // logged and skipped, not fatal to the whole scan.
    let archive_set_id = queries::resolve_archive_set(conn, df.archive_set.as_deref())?;

    let unit_id = queries::insert_unit(
        conn,
        &df.uuid,
        &df.name,
        tenant.id,
        archive_set_id,
        dir_path,
        &df.checksum_mode,
        true,
    )?;
    events::log_created(conn, "unit", unit_id, &df.name, Some(tenant.id))?;

    // Tags
    for tag in &df.tags {
        queries::add_tag_to_unit(conn, unit_id, tag)?;
    }

    Ok(SyncAction::Created)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh_conn() -> Connection {
        crate::db::open_memory().unwrap()
    }

    fn write_test_dotfile(
        dir: &std::path::Path,
        archive_set: Option<&str>,
    ) -> dotfile::UnitDotfile {
        let df = dotfile::UnitDotfile {
            uuid: uuid::Uuid::new_v4().to_string(),
            name: "discovered-unit".to_string(),
            created: chrono::Utc::now().to_rfc3339(),
            tags: vec![],
            tenant: "alice".to_string(),
            archive_set: archive_set.map(|s| s.to_string()),
            checksum_mode: "mtime_size".to_string(),
            compression: "none".to_string(),
            exclude_patterns: vec![],
        };
        dotfile::write_dotfile(&dir.join(".tapectl-unit.toml"), &df).unwrap();
        df
    }

    /// Issue #48 item 3: `sync_discovered_unit` used to never even read
    /// `df.archive_set` — a dotfile-recorded archive set silently vanished
    /// the moment `unit discover` (re)registered a not-yet-known unit,
    /// disagreeing with `unit init`, which (post-fix) persists it.
    #[test]
    fn discover_resolves_archive_set_from_an_existing_dotfile() {
        let conn = fresh_conn();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('alice', 0, 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO archive_sets (name, min_copies) VALUES ('cold', 7)",
            [],
        )
        .unwrap();

        let tmp = TempDir::new().unwrap();
        let unit_dir = tmp.path().join("unitdir");
        std::fs::create_dir_all(&unit_dir).unwrap();
        let df = write_test_dotfile(&unit_dir, Some("cold"));

        let report = discover(&conn, &[tmp.path().to_string_lossy().to_string()]).unwrap();
        assert_eq!(report.created, 1, "errors: {:?}", report.errors);

        let unit = queries::get_unit_by_uuid(&conn, &df.uuid).unwrap().unwrap();
        let as_id: i64 = conn
            .query_row(
                "SELECT id FROM archive_sets WHERE name = 'cold'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            unit.archive_set_id,
            Some(as_id),
            "discover must resolve and persist archive_set_id from the dotfile"
        );
    }

    /// Discovery must agree with `unit init`'s validation (issue #48 item
    /// 3), but — unlike `unit init`, which is a single explicit operation —
    /// a single bad dotfile must not abort the whole scan: matches the
    /// existing per-unit error handling `discover()` already has for e.g.
    /// an unknown tenant (`sync_discovered_unit`'s `TenantNotFound` path).
    #[test]
    fn discover_rejects_a_dotfile_naming_an_unknown_archive_set_but_keeps_scanning() {
        let conn = fresh_conn();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('alice', 0, 'active')",
            [],
        )
        .unwrap();

        let tmp = TempDir::new().unwrap();
        let unit_dir = tmp.path().join("unitdir");
        std::fs::create_dir_all(&unit_dir).unwrap();
        let df = write_test_dotfile(&unit_dir, Some("does-not-exist"));

        let report = discover(&conn, &[tmp.path().to_string_lossy().to_string()]).unwrap();
        assert_eq!(report.created, 0);
        assert_eq!(report.errors.len(), 1, "errors: {:?}", report.errors);
        assert!(report.errors[0].contains("does-not-exist"));
        assert!(queries::get_unit_by_uuid(&conn, &df.uuid)
            .unwrap()
            .is_none());
    }
}
