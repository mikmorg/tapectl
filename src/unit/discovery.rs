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

    let unit_id = queries::insert_unit(
        conn,
        &df.uuid,
        &df.name,
        tenant.id,
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
