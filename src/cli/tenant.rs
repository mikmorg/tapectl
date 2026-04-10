use clap::Subcommand;
use rusqlite::Connection;
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

    /// Delete a tenant (soft delete)
    Delete {
        /// Tenant name
        name: String,
    },
}

#[derive(Tabled)]
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
