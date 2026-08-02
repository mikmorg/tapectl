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
    /// Generate a new keypair for a tenant, or the permanent escrow
    /// recipient with --escrow (ADR-0005)
    Generate {
        /// Tenant name (required unless --escrow)
        #[arg(long, required_unless_present = "escrow", conflicts_with = "escrow")]
        tenant: Option<String>,
        /// Key alias (e.g., "primary", "backup", "2026") (required unless --escrow)
        #[arg(long, required_unless_present = "escrow", conflicts_with = "escrow")]
        alias: Option<String>,
        /// Key type
        #[arg(long, default_value = "primary")]
        key_type: String,
        /// Description
        #[arg(long)]
        description: Option<String>,
        /// Generate the permanent escrow recipient identity instead of a
        /// per-tenant key (ADR-0005). Prints the secret exactly once, for
        /// paper transcription — tapectl never stores it. Refuses if an
        /// escrow identity is already registered; there is only ever one,
        /// for the life of the archive.
        #[arg(long)]
        escrow: bool,
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

    /// Rotate keys for a tenant (deactivate old, generate new). Refuses
    /// unless a permanent escrow recipient is registered (ADR-0005); never
    /// deactivates or replaces the escrow key itself.
    Rotate {
        /// Tenant name
        #[arg(long)]
        tenant: String,
    },

    /// Import a public key from a file, or adopt an existing one as the
    /// permanent escrow recipient with --escrow (ADR-0005)
    Import {
        /// Tenant name (required unless --escrow)
        #[arg(long, required_unless_present = "escrow", conflicts_with = "escrow")]
        tenant: Option<String>,
        /// Key alias (required unless --escrow)
        #[arg(long, required_unless_present = "escrow", conflicts_with = "escrow")]
        alias: Option<String>,
        /// Path to public key file — or, with --escrow, either a path or
        /// the literal age1... public key
        path: String,
        /// Key type
        #[arg(long, default_value = "primary")]
        key_type: String,
        /// Adopt this public key as the permanent escrow recipient
        /// (ADR-0005). Refuses if one is already registered.
        #[arg(long)]
        escrow: bool,
    },

    /// Generate the printed Heir Kit and the encrypted catalog bundle
    /// (ADR-0005 / ADR-0009, issue #69).
    ///
    /// Writes three files: COVER.txt (the plain-text cover sheet — the
    /// artifact with the decades-scale claim), escrow-kit.html (the same
    /// content with an inline QR, for printing), and catalog.db.age (the
    /// whole catalog encrypted to the escrow recipient).
    ///
    /// This command stops at the files. Printing them, sealing them into
    /// tamper-evident envelopes and distributing them across at least two
    /// independent failure domains is the operator's part, and the cover
    /// sheet states those requirements. Re-run after each production write
    /// session; `audit` warns when the kit has fallen behind the tapes.
    EscrowKit {
        /// Directory to write the kit into (created if absent, mode 0700)
        #[arg(long)]
        out: String,
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
    #[tabled(rename = "Escrow")]
    escrow: String,
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
            escrow,
        } => {
            if *escrow {
                generate_escrow_key(conn, paths, description.as_deref(), json_output)?;
            } else {
                let (tenant, alias) = require_tenant_and_alias(tenant, alias)?;

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
                        escrow: if k.is_escrow {
                            "ESCROW (ADR-0005)".into()
                        } else {
                            String::new()
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
            // ADR-0005: escrow presence is a precondition for rotation — a
            // rotated tenant key with no escrow recipient in its future
            // encryptions would defeat the whole point of having one.
            if queries::escrow_public_key(conn)?.is_none() {
                return Err(TapectlError::Other(
                    "key rotate refuses: no escrow recipient is registered (ADR-0005) — run \
                     `tapectl key generate --escrow` (or `key import --escrow`) before \
                     rotating any keys"
                        .into(),
                ));
            }

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
            // is_escrow = 0 exempts the escrow row from rotation (ADR-0005):
            // without it, rotating the OPERATOR tenant would deactivate the
            // escrow key too, since its own row's tenant_id is the
            // operator's (see queries::get_active_keys_for_tenant's doc).
            let deactivated: usize = tx.execute(
                "UPDATE encryption_keys SET is_active = 0
                 WHERE tenant_id = ?1 AND is_active = 1 AND is_escrow = 0",
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
            escrow,
        } => {
            if *escrow {
                import_escrow_key(conn, paths, path, json_output)?;
            } else {
                let (tenant, alias) = require_tenant_and_alias(tenant, alias)?;

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

        KeyCommands::EscrowKit { out } => {
            let report = crate::crypto::escrow_kit::generate(
                conn,
                &paths.db_file,
                std::path::Path::new(out),
            )?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "out_dir": report.out_dir,
                        "cover_txt": report.cover_txt,
                        "html": report.html,
                        "catalog_age": report.catalog_age,
                        "catalog_bytes": report.catalog_bytes,
                        "sealed_volumes": report.sealed_volumes,
                        "escrow_public_key": report.escrow_public_key,
                    })
                );
            } else {
                println!("heir kit written to {}", report.out_dir.display());
                println!("  COVER.txt        the printable cover sheet (print this)");
                println!("  escrow-kit.html  same content with a QR, for a browser's print dialog");
                println!(
                    "  catalog.db.age   encrypted catalog, {} bytes, covering {} sealed volume(s)",
                    report.catalog_bytes, report.sealed_volumes
                );
                println!();
                // ADR-0005 puts the custody requirements on the printed sheet,
                // but the operator is standing here now — and the kit has no
                // value at all until these three things happen.
                println!("still to do, and only you can do it:");
                println!("  1. print COVER.txt (or the HTML page)");
                println!("  2. seal it in a tamper-evident envelope");
                println!("  3. store copies in at least TWO independent failure domains");
            }
        }
    }
    Ok(())
}

/// `key generate --escrow`: mint the permanent escrow identity (ADR-0005).
///
/// The secret half exists only in a local variable long enough to be printed
/// once — it is never written to the database, a config file, a key file on
/// disk, or a log/trace call. Only the public half is persisted: a DB row
/// (`is_escrow=1`, `is_active=1`) and a `.age.pub` file, for parity with how
/// ordinary keys are stored (the public key is not sensitive).
fn generate_escrow_key(
    conn: &Connection,
    paths: &TapectlPaths,
    description: Option<&str>,
    json_output: bool,
) -> Result<()> {
    if queries::escrow_key_exists(conn)? {
        return Err(escrow_already_registered_error());
    }
    let operator = queries::get_operator_tenant(conn)?.ok_or_else(|| {
        TapectlError::Other("no operator tenant — run `tapectl init` first".into())
    })?;

    // In-memory only (crypto::keys::generate_keypair never touches disk) —
    // deliberately NOT keys::generate_and_save, which would write the secret
    // to a key file.
    let kp = keys::generate_keypair();

    let full_alias = format!("{}-escrow", operator.name);
    let desc = description
        .map(str::to_string)
        .unwrap_or_else(|| "Permanent escrow recipient (ADR-0005)".to_string());
    let key_id = queries::insert_escrow_key(
        conn,
        operator.id,
        &full_alias,
        &kp.fingerprint,
        &kp.public_key,
        Some(&desc),
    )?;
    events::log_created(
        conn,
        "encryption_key",
        key_id,
        &full_alias,
        Some(operator.id),
    )?;

    let pub_path = paths.keys_dir.join(format!("{full_alias}.age.pub"));
    keys::save_public_key(&pub_path, &kp.public_key)?;

    print_escrow_secret_warning(&kp.public_key, &kp.secret_key);

    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "alias": full_alias,
                "escrow": true,
                "public_key": kp.public_key,
            })
        );
    }
    Ok(())
}

/// `key import --escrow <pubkey>`: adopt an existing public key as the
/// permanent escrow recipient, under the same refuse-if-exists rule as
/// `key generate --escrow`. `value` may be a literal `age1...` string or a
/// path to a file containing one (`crypto::keys::read_or_parse_public_key`).
fn import_escrow_key(
    conn: &Connection,
    paths: &TapectlPaths,
    value: &str,
    json_output: bool,
) -> Result<()> {
    if queries::escrow_key_exists(conn)? {
        return Err(escrow_already_registered_error());
    }
    let operator = queries::get_operator_tenant(conn)?.ok_or_else(|| {
        TapectlError::Other("no operator tenant — run `tapectl init` first".into())
    })?;
    let pub_key = keys::read_or_parse_public_key(value)?;

    let full_alias = format!("{}-escrow", operator.name);
    let key_id = queries::insert_escrow_key(
        conn,
        operator.id,
        &full_alias,
        &pub_key,
        &pub_key,
        Some("Permanent escrow recipient (ADR-0005), imported"),
    )?;
    events::log_created(
        conn,
        "encryption_key",
        key_id,
        &full_alias,
        Some(operator.id),
    )?;

    let pub_path = paths.keys_dir.join(format!("{full_alias}.age.pub"));
    keys::save_public_key(&pub_path, &pub_key)?;

    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "alias": full_alias,
                "escrow": true,
                "public_key": pub_key,
            })
        );
    } else {
        println!("escrow recipient \"{full_alias}\" registered (ADR-0005)");
        println!("  public: {pub_key}");
    }
    Ok(())
}

/// Resolve `--tenant`/`--alias` for a non-escrow `Generate`/`Import`, with a
/// consistent error if either is missing. clap's `required_unless_present`
/// enforces this at the CLI-parse level in the common case, but both fields
/// are still `Option<String>` here (they're genuinely optional when
/// `--escrow` is set), so this runtime check is still load-bearing, not
/// redundant. Extracted since `Generate` and `Import`'s non-escrow arms
/// repeated this identical pair of checks verbatim (T6 review finding #3).
fn require_tenant_and_alias<'a>(
    tenant: &'a Option<String>,
    alias: &'a Option<String>,
) -> Result<(&'a str, &'a str)> {
    let tenant = tenant
        .as_deref()
        .ok_or_else(|| TapectlError::Other("--tenant is required (or pass --escrow)".into()))?;
    let alias = alias
        .as_deref()
        .ok_or_else(|| TapectlError::Other("--alias is required (or pass --escrow)".into()))?;
    Ok((tenant, alias))
}

fn escrow_already_registered_error() -> TapectlError {
    TapectlError::Other(
        "an escrow recipient is already registered — ADR-0005 permits exactly one \
         permanent escrow identity for the life of the archive; replacing it is a \
         deliberate, separate act, not automated by this command"
            .into(),
    )
}

/// Print the escrow secret exactly once, framed so it cannot be missed or
/// skimmed past. Per ADR-0005 this is the ONLY time the secret is ever
/// shown: tapectl never persists it anywhere (not the database, not a
/// config file, not a key file on disk, not a log or trace line).
fn print_escrow_secret_warning(public_key: &str, secret_key: &str) {
    println!();
    println!("================================================================================");
    println!("  ESCROW IDENTITY GENERATED -- THIS SECRET IS SHOWN EXACTLY ONCE, RIGHT NOW");
    println!("================================================================================");
    println!();
    println!("  tapectl does NOT store this secret anywhere: not in the database, not in");
    println!("  a config file, not in any file on this machine. Close this terminal without");
    println!("  transcribing it and it is gone forever -- the escrow recipient becomes");
    println!("  useless for every future encryption it was meant to protect.");
    println!();
    println!("  Per ADR-0005: copy the secret below onto paper NOW. Store that paper in at");
    println!("  least two independent physical locations. Verify the transcription");
    println!("  character-by-character before doing anything else.");
    println!();
    println!("  SECRET -- transcribe this line:");
    println!();
    println!("    {secret_key}");
    println!();
    println!("  Public key (already saved to disk and the database -- safe to keep there):");
    println!();
    println!("    {public_key}");
    println!();
    println!("================================================================================");
    println!();
}

fn truncate_fingerprint(fp: &str) -> String {
    if fp.len() > 24 {
        format!("{}...", &fp[..24])
    } else {
        fp.to_string()
    }
}
