use clap::Subcommand;
use rusqlite::Connection;
use serde::Serialize;
use tabled::{Table, Tabled};

use crate::config::TapectlPaths;
use crate::db::queries;
use crate::error::Result;

#[derive(Subcommand, Debug)]
pub enum TenantCommands {
    /// Add a new tenant (generates keypair automatically)
    Add {
        /// Tenant name
        name: String,
        /// Description
        #[arg(long, short)]
        description: Option<String>,
    },

    /// List all tenants
    List {
        /// Include deleted tenants
        #[arg(long)]
        all: bool,
    },

    /// Show tenant details
    Info {
        /// Tenant name
        name: String,
    },

    /// Reassign all units from one tenant to another
    Reassign {
        /// Source tenant name
        source: String,
        /// Destination tenant name
        #[arg(long)]
        to: String,
    },

    /// Delete a tenant (soft delete)
    Delete {
        /// Tenant name
        name: String,
    },
}

/// Table-only: `tenant list --json` serializes `db::models::Tenant` directly
/// (already `Serialize`, and richer than this display row -- it carries
/// `id` and the raw `is_operator` bool that `TenantRow` reformats for the
/// table), so there is no hand-rolled JSON derived from `TenantRow` to keep
/// in sync. The `Serialize` derive and its pin below exist for structural
/// parity with the other ten row structs; nothing in `run()` calls it.
#[derive(Tabled, Serialize)]
struct TenantRow {
    #[tabled(rename = "Name")]
    name: String,
    #[tabled(rename = "Status")]
    status: String,
    #[tabled(rename = "Operator")]
    is_operator: String,
    #[tabled(rename = "Created")]
    created_at: String,
    #[tabled(rename = "Description")]
    description: String,
}

pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    command: &TenantCommands,
    json_output: bool,
) -> Result<()> {
    match command {
        TenantCommands::Add { name, description } => {
            let id = crate::tenant::add_tenant(conn, paths, name, description.as_deref(), false)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"id": id, "name": name, "status": "created"})
                );
            } else {
                println!("tenant \"{name}\" created (id={id}) with primary and backup keys");
            }
        }
        TenantCommands::List { all } => {
            let tenants = queries::list_tenants(conn, *all)?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&tenants).unwrap());
            } else if tenants.is_empty() {
                println!("no tenants found");
            } else {
                let rows: Vec<TenantRow> = tenants
                    .into_iter()
                    .map(|t| TenantRow {
                        name: t.name,
                        status: t.status,
                        is_operator: if t.is_operator {
                            "yes".into()
                        } else {
                            "".into()
                        },
                        created_at: t.created_at,
                        description: t.description.unwrap_or_default(),
                    })
                    .collect();
                println!("{}", Table::new(rows));
            }
        }
        TenantCommands::Info { name } => {
            let tenant = crate::tenant::require_tenant(conn, name)?;
            let keys = queries::list_keys_for_tenant(conn, tenant.id)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "tenant": tenant,
                        "keys": keys,
                    })
                );
            } else {
                println!("Tenant: {}", tenant.name);
                println!("  Status:      {}", tenant.status);
                println!(
                    "  Operator:    {}",
                    if tenant.is_operator { "yes" } else { "no" }
                );
                println!("  Created:     {}", tenant.created_at);
                if let Some(ref desc) = tenant.description {
                    println!("  Description: {desc}");
                }
                println!("  Keys:");
                for key in &keys {
                    println!(
                        "    {} [{}] {}  active={}",
                        key.alias,
                        key.key_type,
                        &key.fingerprint[..20],
                        key.is_active,
                    );
                }
            }
        }
        TenantCommands::Reassign { source, to } => {
            let src = crate::tenant::require_tenant(conn, source)?;
            let dst = crate::tenant::require_tenant(conn, to)?;
            let moved: usize = conn.execute(
                "UPDATE units SET tenant_id = ?1 WHERE tenant_id = ?2",
                rusqlite::params![dst.id, src.id],
            )?;
            // Issue #243 / ADR-0012's move-event ruling: the value fields
            // carry tenant NAMES on both sides, never a raw id -- a reader
            // of `report events` (and `location.rs`'s mover, the pattern
            // this follows) must never have to query a table the event was
            // supposed to spare them, and an id here would name nothing at
            // all once either tenant is renamed or deleted.
            crate::db::events::log_field_change(
                conn,
                "tenant",
                src.id,
                source,
                "reassign",
                "tenant",
                Some(source),
                to,
                None,
            )?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"from": source, "to": to, "units_moved": moved})
                );
            } else {
                println!("{moved} unit(s) reassigned from \"{source}\" to \"{to}\"");
            }
        }
        TenantCommands::Delete { name } => {
            crate::tenant::delete_tenant(conn, name)?;
            if json_output {
                println!("{}", serde_json::json!({"name": name, "status": "deleted"}));
            } else {
                println!("tenant \"{name}\" deleted");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TenantRow`'s own `Serialize` shape (issue: C2 row-listing drift).
    /// Not wired into `tenant list --json`, which already serializes
    /// `db::models::Tenant` directly -- see the doc comment on `TenantRow`.
    #[test]
    fn pin_tenant_rows_json_shape() {
        let rows = vec![
            TenantRow {
                name: "alice".to_string(),
                status: "active".to_string(),
                is_operator: "yes".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                description: "primary".to_string(),
            },
            TenantRow {
                name: "bob".to_string(),
                status: "deleted".to_string(),
                is_operator: String::new(),
                created_at: "2026-02-01T00:00:00Z".to_string(),
                description: String::new(),
            },
        ];
        let value = serde_json::to_value(&rows).unwrap();
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"[{"created_at":"2026-01-01T00:00:00Z","description":"primary","is_operator":"yes","name":"alice","status":"active"},{"created_at":"2026-02-01T00:00:00Z","description":"","is_operator":"","name":"bob","status":"deleted"}]"#
        );
    }

    /// Issue #243: `tenant reassign` used to log the raw database ids in the
    /// event's `old_value`/`new_value`, so `report events` rendered
    /// `tenant/acme reassign.tenant_id: 3 -> 7` -- unreadable once either
    /// tenant is renamed or deleted, and exactly the shape ADR-0012's
    /// move-event ruling forbids (`location.rs`'s mover logs location NAMES
    /// on both sides, never an id). The event must carry tenant NAMES on
    /// both sides instead.
    #[test]
    fn reassign_logs_tenant_names_not_ids() {
        let conn = crate::db::open_memory().unwrap();
        queries::insert_tenant(&conn, "acme", None, false).unwrap();
        queries::insert_tenant(&conn, "othertenant", None, false).unwrap();
        let paths = TapectlPaths::new(std::path::PathBuf::from("/nonexistent-pm243-test"));

        run(
            &conn,
            &paths,
            &TenantCommands::Reassign {
                source: "acme".to_string(),
                to: "othertenant".to_string(),
            },
            false,
        )
        .unwrap();

        let (old_value, new_value): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT old_value, new_value FROM events
                 WHERE entity_type = 'tenant' AND action = 'reassign'
                 ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(old_value.as_deref(), Some("acme"));
        assert_eq!(new_value.as_deref(), Some("othertenant"));
    }
}
