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

    /// Rotate keys for a tenant (deactivate old, generate new)
    Rotate {
        /// Tenant name
        #[arg(long)]
        tenant: String,
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
        KeyCommands::Rotate { tenant } => {
            let t = crate::tenant::require_tenant(conn, tenant)?;

            // Serial suffix keeps every rotation's aliases (and key filenames)
            // unique, so a repeat rotation never hits KeyAlreadyExists. That
            // collision was the H13 bug: it failed *after* the deactivation had
            // already committed, stranding the tenant with zero active keys.
            // Key count only grows (rotation deactivates, never deletes), so
            // this is monotonic and collision-free.
            let seq: i64 = conn.query_row(
                "SELECT COUNT(*) FROM encryption_keys WHERE tenant_id = ?1",
                rusqlite::params![t.id],
                |r| r.get(0),
            )?;
            let p_suffix = format!("rotated-primary-{seq}");
            let b_suffix = format!("rotated-backup-{seq}");
            let p_alias = format!("{tenant}-{p_suffix}");
            let b_alias = format!("{tenant}-{b_suffix}");

            // Generate the key files first (filesystem side effects live outside
            // the DB transaction; the unique suffixes guarantee no collision).
            let primary = keys::generate_and_save(&paths.keys_dir, tenant, &p_suffix)?;
            let backup = keys::generate_and_save(&paths.keys_dir, tenant, &b_suffix)?;

            // Deactivate + insert atomically: a failure anywhere rolls the whole
            // rotation back rather than leaving the tenant keyless.
            let tx = conn.unchecked_transaction()?;
            let deactivated: usize = tx.execute(
                "UPDATE encryption_keys SET is_active = 0 WHERE tenant_id = ?1 AND is_active = 1",
                rusqlite::params![t.id],
            )?;
            let p_id = queries::insert_key(
                &tx,
                t.id,
                &p_alias,
                &primary.fingerprint,
                &primary.public_key,
                "primary",
                None,
            )?;
            events::log_created(&tx, "encryption_key", p_id, &p_alias, Some(t.id))?;
            let b_id = queries::insert_key(
                &tx,
                t.id,
                &b_alias,
                &backup.fingerprint,
                &backup.public_key,
                "backup",
                None,
            )?;
            events::log_created(&tx, "encryption_key", b_id, &b_alias, Some(t.id))?;
            tx.commit()?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "tenant": tenant, "deactivated": deactivated,
                        "new_primary": p_alias, "new_backup": b_alias,
                    })
                );
            } else {
                println!("rotated keys for \"{tenant}\": {deactivated} deactivated, 2 new keys generated");
            }
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
