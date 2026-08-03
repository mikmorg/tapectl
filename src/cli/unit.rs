use clap::Subcommand;
use rusqlite::Connection;
use tabled::{Table, Tabled};

use crate::config::{Config, TapectlPaths};
use crate::db::queries;
use crate::error::{Result, TapectlError};

#[derive(Subcommand, Debug)]
pub enum UnitCommands {
    /// Initialize a directory as an archival unit
    Init {
        /// Path to directory
        path: String,
        /// Tenant name
        #[arg(long)]
        tenant: String,
        /// Override auto-generated name
        #[arg(long)]
        name: Option<String>,
        /// Tags to apply
        #[arg(long, short)]
        tag: Vec<String>,
        /// Archive set name
        #[arg(long)]
        archive_set: Option<String>,
    },

    /// Bulk-initialize subdirectories as units
    InitBulk {
        /// Parent directory to scan
        path: String,
        /// Tenant name
        #[arg(long)]
        tenant: String,
        /// Tags to apply to all
        #[arg(long, short)]
        tag: Vec<String>,
    },

    /// List units
    List {
        /// Filter by tenant name
        #[arg(long)]
        tenant: Option<String>,
        /// Filter by status
        #[arg(long)]
        status: Option<String>,
        /// Filter by tag
        #[arg(long, short)]
        tag: Option<String>,
    },

    /// Show unit status/details
    Status {
        /// Unit name or path
        name: String,
        /// Show dirty/clean/new status via an on-disk fingerprint scan
        /// (checksum_mode-aware — see `unit init`'s checksum_mode) instead
        /// of the usual detail view
        #[arg(long)]
        dirty: bool,
    },

    /// Add/remove tags
    Tag {
        /// Unit name
        name: String,
        /// Tags to add
        #[arg(long)]
        add: Vec<String>,
        /// Tags to remove
        #[arg(long)]
        remove: Vec<String>,
    },

    /// Rename a unit
    Rename {
        /// Current name
        current: String,
        /// New name
        new: String,
    },

    /// Scan watch_roots for .tapectl-unit.toml dotfiles
    Discover,

    /// Check file integrity against staged checksums
    CheckIntegrity {
        /// Unit name
        name: String,
    },

    /// Mark unit as tape-only (local data can be deleted)
    MarkTapeOnly {
        /// Unit name
        name: String,
        /// Override copy/location requirements
        #[arg(long)]
        force: bool,
    },
}

#[derive(Tabled)]
struct UnitRow {
    #[tabled(rename = "Name")]
    name: String,
    #[tabled(rename = "Status")]
    status: String,
    #[tabled(rename = "Tenant")]
    tenant: String,
    #[tabled(rename = "Path")]
    path: String,
    #[tabled(rename = "Tags")]
    tags: String,
}

pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    command: &UnitCommands,
    json_output: bool,
) -> Result<()> {
    match command {
        UnitCommands::Init {
            path,
            tenant,
            name,
            tag,
            archive_set,
        } => {
            let unit_id = crate::unit::init_unit(
                conn,
                paths,
                path,
                tenant,
                name.as_deref(),
                tag,
                archive_set.as_deref(),
            )?;
            if json_output {
                let unit =
                    queries::get_unit_by_name(conn, &resolve_unit_name(conn, unit_id)?)?.unwrap();
                println!("{}", serde_json::to_string_pretty(&unit).unwrap());
            } else {
                let unit_name = resolve_unit_name(conn, unit_id)?;
                println!("unit \"{unit_name}\" initialized (id={unit_id})");
            }
        }

        UnitCommands::InitBulk { path, tenant, tag } => {
            let results = crate::unit::init_bulk(conn, paths, path, tenant, tag)?;
            let mut success = 0;
            let mut failed = 0;
            for (dir, result) in &results {
                match result {
                    Ok(id) => {
                        if !json_output {
                            println!("  ok: {dir} (id={id})");
                        }
                        success += 1;
                    }
                    Err(e) => {
                        if !json_output {
                            println!("  skip: {dir}: {e}");
                        }
                        failed += 1;
                    }
                }
            }
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"success": success, "failed": failed})
                );
            } else {
                println!("{success} units created, {failed} skipped");
            }
        }

        UnitCommands::List {
            tenant,
            status,
            tag,
        } => {
            let tenant_id = if let Some(name) = tenant {
                Some(crate::tenant::require_tenant(conn, name)?.id)
            } else {
                None
            };
            let units = queries::list_units(conn, tenant_id, status.as_deref())?;

            // If filtering by tag, do it in memory (simpler than a join query for now)
            let units = if let Some(tag_filter) = tag {
                units
                    .into_iter()
                    .filter(|u| {
                        queries::get_tags_for_unit(conn, u.id)
                            .unwrap_or_default()
                            .contains(tag_filter)
                    })
                    .collect()
            } else {
                units
            };

            if json_output {
                println!("{}", serde_json::to_string_pretty(&units).unwrap());
            } else if units.is_empty() {
                println!("no units found");
            } else {
                let mut rows = Vec::new();
                for u in &units {
                    let tenant_name = queries::get_tenant_by_id(conn, u.tenant_id)?
                        .map(|t| t.name)
                        .unwrap_or_else(|| "?".to_string());
                    let tags = queries::get_tags_for_unit(conn, u.id)?.join(", ");
                    rows.push(UnitRow {
                        name: u.name.clone(),
                        status: u.status.clone(),
                        tenant: tenant_name,
                        path: u.current_path.clone().unwrap_or_default(),
                        tags,
                    });
                }
                println!("{}", Table::new(rows));
            }
        }

        UnitCommands::Status { name, dirty } if *dirty => {
            show_dirty_status(conn, name, json_output, &config.defaults.global_excludes)?;
        }

        UnitCommands::Status { name, .. } => {
            let unit = resolve_unit(conn, name)?;
            let tags = queries::get_tags_for_unit(conn, unit.id)?;
            let tenant = queries::get_tenant_by_id(conn, unit.tenant_id)?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "unit": unit,
                        "tags": tags,
                        "tenant": tenant,
                    })
                );
            } else {
                println!("Unit: {}", unit.name);
                println!("  UUID:          {}", unit.uuid);
                println!("  Status:        {}", unit.status);
                println!(
                    "  Tenant:        {}",
                    tenant.map(|t| t.name).unwrap_or_else(|| "?".into())
                );
                println!(
                    "  Path:          {}",
                    unit.current_path.as_deref().unwrap_or("(none)")
                );
                println!("  Checksum mode: {}", unit.checksum_mode);
                println!("  Encrypted:     {}", unit.encrypt);
                println!("  Created:       {}", unit.created_at);
                if !tags.is_empty() {
                    println!("  Tags:          {}", tags.join(", "));
                }
            }
        }

        UnitCommands::Tag { name, add, remove } => {
            let unit = resolve_unit(conn, name)?;
            for tag in add {
                queries::add_tag_to_unit(conn, unit.id, tag)?;
            }
            for tag in remove {
                queries::remove_tag_from_unit(conn, unit.id, tag)?;
            }
            let tags = queries::get_tags_for_unit(conn, unit.id)?;
            if json_output {
                println!("{}", serde_json::json!({"name": unit.name, "tags": tags}));
            } else {
                println!("unit \"{}\": tags = [{}]", unit.name, tags.join(", "));
            }
        }

        UnitCommands::Rename { current, new } => {
            crate::unit::rename_unit(conn, current, new)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"old_name": current, "new_name": new})
                );
            } else {
                println!("unit \"{current}\" renamed to \"{new}\"");
            }
        }

        UnitCommands::Discover => {
            let report = crate::unit::discovery::discover(conn, &config.discovery.watch_roots)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "created": report.created,
                        "updated": report.updated,
                        "unchanged": report.unchanged,
                        "errors": report.errors,
                    })
                );
            } else {
                println!(
                    "discover: {} created, {} updated, {} unchanged",
                    report.created, report.updated, report.unchanged
                );
                if !report.skipped_roots.is_empty() {
                    println!("  skipped roots: {}", report.skipped_roots.join(", "));
                }
                for err in &report.errors {
                    println!("  error: {err}");
                }
            }
        }

        UnitCommands::CheckIntegrity { name } => {
            crate::cli::operations::unit_check_integrity(conn, name, json_output)?;
        }

        UnitCommands::MarkTapeOnly { name, force } => {
            crate::cli::operations::unit_mark_tape_only(conn, config, name, *force, json_output)?;
        }
    }
    Ok(())
}

/// Resolve a unit by name or path.
fn resolve_unit(conn: &Connection, name_or_path: &str) -> Result<crate::db::models::Unit> {
    // Try by name first
    if let Some(u) = queries::get_unit_by_name(conn, name_or_path)? {
        return Ok(u);
    }
    // Try by path
    if let Ok(abs) = std::fs::canonicalize(name_or_path) {
        if let Some(u) = queries::get_unit_by_path(conn, &abs.to_string_lossy())? {
            return Ok(u);
        }
    }
    Err(TapectlError::UnitNotFound(name_or_path.to_string()))
}

fn resolve_unit_name(conn: &Connection, unit_id: i64) -> Result<String> {
    let unit = conn.query_row(
        "SELECT name FROM units WHERE id = ?1",
        rusqlite::params![unit_id],
        |row| row.get(0),
    )?;
    Ok(unit)
}

/// One unit's dirty-scan verdict — split out from `show_dirty_status`'s
/// printing so the result is directly assertable in tests without
/// capturing stdout.
struct DirtyStatus {
    state: &'static str, // "clean" | "new" | "dirty"
    changes: crate::collection::fingerprint::FingerprintDiff,
}

/// `unit status --dirty`'s scan: reuses `fingerprint::classify` — the same
/// scan the Collection layer (`collection sync|status|plan`) and `report
/// dirty` use — so this can never disagree with them about whether a
/// unit's disk matches its last snapshot (issue #36/H10). `global_excludes`
/// is `config.defaults.global_excludes` (issue #49), kept in lockstep with
/// those other callers.
fn dirty_status(
    conn: &Connection,
    unit: &crate::db::models::Unit,
    global_excludes: &[String],
) -> Result<DirtyStatus> {
    use crate::collection::fingerprint::{self, PendingReason};

    Ok(match fingerprint::classify(conn, unit, global_excludes)? {
        None => DirtyStatus {
            state: "clean",
            changes: fingerprint::FingerprintDiff::default(),
        },
        Some(p) if p.reason == PendingReason::New => DirtyStatus {
            state: "new",
            changes: fingerprint::FingerprintDiff::default(),
        },
        Some(p) => DirtyStatus {
            state: "dirty",
            changes: p.changes,
        },
    })
}

fn show_dirty_status(
    conn: &Connection,
    name: &str,
    json_output: bool,
    global_excludes: &[String],
) -> Result<()> {
    let unit = resolve_unit(conn, name)?;
    let status = dirty_status(conn, &unit, global_excludes)?;

    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "unit": unit.name,
                "state": status.state,
                "added": status.changes.added,
                "removed": status.changes.removed,
                "modified": status.changes.modified,
            })
        );
    } else {
        match status.state {
            "clean" => println!("unit \"{}\": clean", unit.name),
            "new" => println!("unit \"{}\": new — never archived", unit.name),
            _ => {
                println!(
                    "unit \"{}\": dirty ({} added, {} removed, {} modified)",
                    unit.name,
                    status.changes.added.len(),
                    status.changes.removed.len(),
                    status.changes.modified.len(),
                );
                // The audit's own wording ("shows specific changes") is why
                // this exists at all — a bare "dirty" doesn't tell an
                // operator whether it's safe to delete local data.
                for p in &status.changes.added {
                    println!("  + {p}");
                }
                for p in &status.changes.removed {
                    println!("  - {p}");
                }
                for p in &status.changes.modified {
                    println!("  ~ {p}");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::TempDir;

    fn setup_unit(current_path: &str, checksum_mode: &str) -> (Connection, i64) {
        // Full migration set (not just 001) — snapshot_create's real walk
        // writes files.file_type/link_target, added by migration 005.
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, checksum_mode, encrypt, status)
             VALUES ('u1', 'unit1', ?1, ?2, ?3, 1, 'active')",
            params![tid, current_path, checksum_mode],
        )
        .unwrap();
        let uid = conn.last_insert_rowid();
        (conn, uid)
    }

    #[test]
    fn a_clean_unit_reports_clean() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), b"hello").unwrap();
        let (conn, _uid) = setup_unit(tmp.path().to_str().unwrap(), "mtime_size");
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        let unit = queries::get_unit_by_name(&conn, "unit1").unwrap().unwrap();
        let status = dirty_status(&conn, &unit, &[]).unwrap();
        assert_eq!(status.state, "clean");
        assert!(status.changes.is_empty());
    }

    #[test]
    fn a_never_archived_unit_reports_new() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), b"hello").unwrap();
        let (conn, _uid) = setup_unit(tmp.path().to_str().unwrap(), "mtime_size");
        // No snapshot_create call — never archived.

        let unit = queries::get_unit_by_name(&conn, "unit1").unwrap().unwrap();
        let status = dirty_status(&conn, &unit, &[]).unwrap();
        assert_eq!(status.state, "new");
    }

    #[test]
    fn a_modified_unit_reports_dirty_and_names_the_changed_file() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("f.txt");
        std::fs::write(&file_path, b"hello").unwrap();
        let (conn, _uid) = setup_unit(tmp.path().to_str().unwrap(), "mtime_size");
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        std::fs::write(&file_path, b"hello, world! now a different size").unwrap();

        let unit = queries::get_unit_by_name(&conn, "unit1").unwrap().unwrap();
        let status = dirty_status(&conn, &unit, &[]).unwrap();
        assert_eq!(status.state, "dirty");
        assert_eq!(status.changes.modified, vec!["f.txt".to_string()]);
    }

    #[test]
    fn show_dirty_status_runs_end_to_end_for_a_dirty_unit() {
        // Wiring smoke test: the CLI-facing function must run to completion
        // (JSON and plain) against a real dirty unit, not just the
        // underlying dirty_status() helper the tests above exercise
        // directly.
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("f.txt");
        std::fs::write(&file_path, b"hello").unwrap();
        let (conn, _uid) = setup_unit(tmp.path().to_str().unwrap(), "mtime_size");
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        std::fs::write(&file_path, b"hello, world! now a different size").unwrap();

        show_dirty_status(&conn, "unit1", false, &[]).expect("plain output must succeed");
        show_dirty_status(&conn, "unit1", true, &[]).expect("json output must succeed");
    }
}
