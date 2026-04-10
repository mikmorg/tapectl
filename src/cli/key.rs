use std::path::Path;

use clap::Subcommand;
use rusqlite::Connection;
use tabled::{Table, Tabled};

use crate::config::TapectlPaths;
use crate::crypto::keys;
use crate::db::{events, queries};
use crate::error::{Result, TapectlError};

#[derive(Subcommand, Debug)]
pub enum KeyCommands {
    /// Generate a new keypair for a tenant
    Generate {
        /// Tenant name
        #[arg(long)]
        tenant: String,
        /// Key alias (e.g., "primary", "backup", "2026")
        #[arg(long)]
        alias: String,
        /// Key type
        #[arg(long, default_value = "primary")]
        key_type: String,
        /// Description
        #[arg(long)]
        description: Option<String>,
    },

    /// List keys for a tenant
    List {
        /// Tenant name
        #[arg(long)]
        tenant: String,
    },

    /// Export a public key to stdout
    Export {
        /// Key alias
        alias: String,
    },

    /// Import a public key from a file
    Import {
        /// Tenant name
        #[arg(long)]
        tenant: String,
        /// Key alias
        #[arg(long)]
        alias: String,
        /// Path to public key file
        path: String,
        /// Key type
        #[arg(long, default_value = "primary")]
        key_type: String,
    },
}

#[derive(Tabled)]
struct KeyRow {
    #[tabled(rename = "Alias")]
    alias: String,
    #[tabled(rename = "Type")]
    key_type: String,
    #[tabled(rename = "Active")]
    is_active: String,
    #[tabled(rename = "Fingerprint")]
    fingerprint: String,
    #[tabled(rename = "Created")]
    created_at: String,
}

pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    command: &KeyCommands,
    json_output: bool,
) -> Result<()> {
    match command {
        KeyCommands::Generate {
            tenant,
            alias,
            key_type,
            description,
        } => {
            let t = crate::tenant::require_tenant(conn, tenant)?;
            let kp = keys::generate_and_save(&paths.keys_dir, tenant, alias)?;

            let full_alias = format!("{tenant}-{alias}");
            let key_id = queries::insert_key(
                conn,
                t.id,
                &full_alias,
                &kp.fingerprint,
                &kp.public_key,
                key_type,
                description.as_deref(),
            )?;
            events::log_created(conn, "encryption_key", key_id, &full_alias, Some(t.id))?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "alias": full_alias,
                        "fingerprint": kp.fingerprint,
                        "public_key": kp.public_key,
                    })
                );
            } else {
                println!("key \"{full_alias}\" generated");
                println!("  public:  {}", kp.public_key);
                println!(
                    "  files:   {}/{tenant}-{alias}.age.{{pub,key}}",
                    paths.keys_dir.display(),
                );
            }
        }
        KeyCommands::List { tenant } => {
            let t = crate::tenant::require_tenant(conn, tenant)?;
            let key_list = queries::list_keys_for_tenant(conn, t.id)?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&key_list).unwrap());
            } else if key_list.is_empty() {
                println!("no keys found for tenant \"{tenant}\"");
            } else {
                let rows: Vec<KeyRow> = key_list
                    .into_iter()
                    .map(|k| KeyRow {
                        alias: k.alias,
                        key_type: k.key_type,
                        is_active: if k.is_active {
                            "yes".into()
                        } else {
                            "no".into()
                        },
                        fingerprint: truncate_fingerprint(&k.fingerprint),
                        created_at: k.created_at,
                    })
                    .collect();
                println!("{}", Table::new(rows));
            }
        }
        KeyCommands::Export { alias } => {
            let key = queries::get_key_by_alias(conn, alias)?
                .ok_or_else(|| TapectlError::KeyNotFound(alias.clone()))?;
            println!("{}", key.public_key);
        }
        KeyCommands::Import {
            tenant,
            alias,
            path,
            key_type,
        } => {
            let t = crate::tenant::require_tenant(conn, tenant)?;
            let pub_key = keys::read_public_key(Path::new(path))?;
            let fingerprint = pub_key.clone();

            let full_alias = format!("{tenant}-{alias}");
            let key_id = queries::insert_key(
                conn,
                t.id,
                &full_alias,
                &fingerprint,
                &pub_key,
                key_type,
                None,
            )?;
            events::log_created(conn, "encryption_key", key_id, &full_alias, Some(t.id))?;

            // Save a copy of the public key
            let pub_path = paths.keys_dir.join(format!("{full_alias}.age.pub"));
            keys::save_public_key(&pub_path, &pub_key)?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "alias": full_alias,
                        "fingerprint": fingerprint,
                    })
                );
            } else {
                println!("key \"{full_alias}\" imported");
            }
        }
    }
    Ok(())
}

fn truncate_fingerprint(fp: &str) -> String {
    if fp.len() > 24 {
        format!("{}...", &fp[..24])
    } else {
        fp.to_string()
    }
}
