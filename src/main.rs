mod cli;
mod config;
mod crypto;
mod db;
mod error;
mod signal;
mod tenant;
mod unit;

mod dar;
mod staging;
mod tape;
mod volume;

mod policy;

// Stub modules for future milestones
mod backend {
    pub mod _stub {}
}

use anyhow::{bail, Context};
use clap::Parser;

use cli::{Cli, Commands};
use config::{Config, TapectlPaths};

fn main() {
    let cli = Cli::parse();

    signal::install_handler();

    if let Err(err) = run(cli) {
        error::exit_with_error(&err);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    // Resolve paths
    let paths = if let Some(ref config_path) = cli.config {
        // If --config is given, derive home from the config file's parent
        let config_file = std::path::Path::new(config_path);
        let home = config_file
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf();
        TapectlPaths::new(home)
    } else {
        TapectlPaths::default_paths()
    };

    // Init is special — it creates everything from scratch
    if let Commands::Init { ref operator } = cli.command {
        return cmd_init(&paths, operator.as_deref(), cli.json);
    }

    // Completions don't need DB
    if let Commands::Completions { shell } = cli.command {
        let mut cmd = <Cli as clap::CommandFactory>::command();
        clap_complete::generate(shell, &mut cmd, "tapectl", &mut std::io::stdout());
        return Ok(());
    }

    // Everything else requires initialization
    if !paths.is_initialized() {
        bail!("tapectl is not initialized — run `tapectl init` first");
    }

    let cfg = Config::load(&paths.config_file).context("failed to load config")?;
    let conn = db::open(&paths.db_file).context("failed to open database")?;

    match cli.command {
        Commands::Tenant { ref command } => {
            cli::tenant::run(&conn, &paths, command, cli.json)?;
        }
        Commands::Key { ref command } => {
            cli::key::run(&conn, &paths, command, cli.json)?;
        }
        Commands::Unit { ref command } => {
            cli::unit::run(&conn, &paths, &cfg, command, cli.json)?;
        }
        Commands::Snapshot { ref command } => {
            cli::snapshot::run(&conn, &paths, &cfg, command, cli.json)?;
        }
        Commands::Stage { ref command } => {
            cli::stage::run(&conn, &paths, &cfg, command, cli.json)?;
        }
        Commands::Staging { ref command } => {
            cli::staging::run(&conn, command, cli.json)?;
        }
        Commands::Volume { ref command } => {
            cli::volume::run(&conn, &paths, &cfg, command, cli.json)?;
        }
        Commands::Restore { ref command } => {
            cli::restore::run(&conn, &paths, &cfg, command, cli.json)?;
        }
        Commands::Catalog { ref command } => {
            cli::catalog::run(&conn, command, cli.json)?;
        }
        Commands::Location { ref command } => {
            cli::location::run(&conn, command, cli.json)?;
        }
        Commands::Cartridge { ref command } => {
            cli::cartridge::run(&conn, command, cli.json)?;
        }
        Commands::ArchiveSet { ref command } => {
            cli::archive_set::run(&conn, &cfg, command, cli.json)?;
        }
        Commands::Audit {
            action_plan,
            ref unit,
        } => {
            let exit_code = cli::audit::run(&conn, &cfg, unit.as_deref(), action_plan, cli.json)?;
            if exit_code > 0 {
                std::process::exit(exit_code);
            }
        }
        Commands::Report { ref command } => {
            cli::report::run(&conn, &cfg, command, cli.json)?;
        }
        Commands::Export { ref unit, ref to } => {
            cli::operations::export_unit(&conn, unit, to, cli.json)?;
        }
        Commands::Import {
            ref label,
            ref backend,
            ref media_type,
            ref capacity,
            ref notes,
        } => {
            let cap_bytes = crate::staging::parse_size_to_bytes(capacity);
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status, notes)
                 VALUES (?1, ?2, ?2, ?3, ?4, 'active', ?5)",
                rusqlite::params![label, backend, media_type, cap_bytes, notes],
            )?;
            let vol_id = conn.last_insert_rowid();
            crate::db::events::log_created(&conn, "volume", vol_id, label, None)?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({"id": vol_id, "label": label, "status": "imported"})
                );
            } else {
                println!("volume \"{label}\" imported (id={vol_id}, {media_type}, {capacity})");
            }
        }
        Commands::QuickArchive {
            ref path,
            ref tenant,
            ref volume,
            ref tag,
            ref device,
        } => {
            // Step 1: init unit
            let unit_id = crate::unit::init_unit(&conn, &paths, path, tenant, None, tag, None)?;
            let unit_name: String = conn.query_row(
                "SELECT name FROM units WHERE id = ?1",
                rusqlite::params![unit_id],
                |row| row.get(0),
            )?;
            println!("unit \"{unit_name}\" initialized");
            // Step 2: snapshot
            let snap_id = crate::staging::snapshot_create(&conn, &unit_name)?;
            println!("snapshot created (id={snap_id})");
            // Step 3: stage
            let ss_id = crate::staging::stage_create(&conn, &paths, &cfg, snap_id)?;
            println!("staged (stage_set={ss_id})");
            // Step 4: write
            crate::volume::write::volume_write(&conn, &paths, &cfg, volume, device, 512 * 1024)?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({"unit": unit_name, "volume": volume, "status": "completed"})
                );
            } else {
                println!("quick-archive complete: \"{unit_name}\" written to \"{volume}\"");
            }
        }
        Commands::Db { ref command } => match command {
            cli::DbCommands::Backup { to } => {
                cli::operations::db_backup(&paths, to)?;
                if cli.json {
                    println!("{}", serde_json::json!({"backup": to}));
                } else {
                    println!("database backed up to {to}");
                }
            }
            cli::DbCommands::Fsck { repair } => {
                let report = cli::operations::db_fsck(&conn, *repair)?;
                if cli.json {
                    println!(
                        "{}",
                        serde_json::json!({"integrity_ok": report.integrity_ok, "issues": report.issues, "repaired": report.repaired})
                    );
                } else {
                    println!(
                        "fsck: integrity={}, issues={}, repaired={}",
                        if report.integrity_ok { "ok" } else { "FAIL" },
                        report.issues.len(),
                        report.repaired,
                    );
                    for issue in &report.issues {
                        println!("  {issue}");
                    }
                }
            }
            cli::DbCommands::Export => {
                // Export key table counts as JSON
                let tables = [
                    "tenants",
                    "units",
                    "snapshots",
                    "stage_sets",
                    "volumes",
                    "writes",
                    "events",
                ];
                let mut counts = serde_json::Map::new();
                for table in &tables {
                    let count: i64 =
                        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                            row.get(0)
                        })?;
                    counts.insert(table.to_string(), serde_json::json!(count));
                }
                println!("{}", serde_json::to_string_pretty(&counts).unwrap());
            }
            cli::DbCommands::Import { path: import_path } => {
                // Import = restore from backup
                let src = rusqlite::Connection::open(import_path)?;
                let mut dst = rusqlite::Connection::open(&paths.db_file)?;
                let backup = rusqlite::backup::Backup::new(&src, &mut dst)?;
                backup
                    .run_to_completion(100, std::time::Duration::from_millis(10), None)
                    .map_err(|e| anyhow::anyhow!("import failed: {e}"))?;
                println!("database imported from {import_path}");
            }
            cli::DbCommands::Stats => {
                let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
                let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
                let db_size = page_count * page_size;
                let table_count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
                    [],
                    |r| r.get(0),
                )?;
                if cli.json {
                    println!(
                        "{}",
                        serde_json::json!({"size_bytes": db_size, "tables": table_count, "pages": page_count})
                    );
                } else {
                    println!(
                        "database: {} KB, {table_count} tables, {page_count} pages",
                        db_size / 1024
                    );
                }
            }
        },
        Commands::Config { ref command } => match command {
            cli::ConfigCommands::Show => {
                let toml_str = std::fs::read_to_string(&paths.config_file)?;
                if cli.json {
                    let val: toml::Value = toml_str
                        .parse()
                        .unwrap_or(toml::Value::String(toml_str.clone()));
                    println!("{}", serde_json::to_string_pretty(&val).unwrap());
                } else {
                    print!("{toml_str}");
                }
            }
            cli::ConfigCommands::Check => {
                let toml_str = std::fs::read_to_string(&paths.config_file)?;
                match toml_str.parse::<toml::Value>() {
                    Ok(_) => {
                        let _ = config::Config::load(&paths.config_file)?;
                        if cli.json {
                            println!("{}", serde_json::json!({"valid": true}));
                        } else {
                            println!("config: valid");
                        }
                    }
                    Err(e) => {
                        if cli.json {
                            println!(
                                "{}",
                                serde_json::json!({"valid": false, "error": e.to_string()})
                            );
                        } else {
                            println!("config: INVALID — {e}");
                        }
                    }
                }
            }
        },
        Commands::Init { .. } | Commands::Completions { .. } => {
            unreachable!()
        }
    }

    Ok(())
}

/// `tapectl init` — bootstrap everything.
fn cmd_init(
    paths: &TapectlPaths,
    operator_name: Option<&str>,
    json_output: bool,
) -> anyhow::Result<()> {
    if paths.is_initialized() {
        bail!("tapectl is already initialized at {}", paths.home.display());
    }

    // Create directory structure
    paths.ensure_dirs()?;

    // Write default config
    let cfg = Config::default();
    cfg.save(&paths.config_file)?;

    // Create database with schema
    let conn = db::open(&paths.db_file).context("failed to create database")?;

    // Determine operator name
    let op_name = operator_name
        .map(String::from)
        .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "operator".to_string()));

    // Create operator tenant with keypairs
    let tenant_id = tenant::add_tenant(&conn, paths, &op_name, Some("System operator"), true)?;

    // Validate dar availability (non-fatal warning)
    let dar_path = &cfg.dar.binary;
    let dar_ok = check_dar(dar_path);

    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "home": paths.home.display().to_string(),
                "operator": op_name,
                "operator_id": tenant_id,
                "dar_available": dar_ok,
            })
        );
    } else {
        println!("tapectl initialized at {}", paths.home.display());
        println!("  operator: {op_name}");
        println!("  database: {}", paths.db_file.display());
        println!("  config:   {}", paths.config_file.display());
        if dar_ok {
            println!("  dar:      {dar_path} (ok)");
        } else {
            println!("  dar:      {dar_path} (NOT FOUND — install before staging)");
        }
    }

    Ok(())
}

/// Check if dar is available at the configured path.
fn check_dar(dar_path: &str) -> bool {
    std::process::Command::new(dar_path)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
