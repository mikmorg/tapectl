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
            let results = crate::unit::init_bulk(conn, paths, path, tenant, tag, 1)?;
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

        UnitCommands::Status { name } => {
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
