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

// Stub modules for future milestones
mod backend {
    pub mod _stub {}
}
mod cartridge {
    pub mod _stub {}
}
mod policy {
    pub mod _stub {}
}
mod verify {
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
        Commands::Export { ref unit, ref to } => {
            cli::operations::export_unit(&conn, unit, to, cli.json)?;
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
        },
        Commands::Init { .. } | Commands::Completions { .. } => {
            unreachable!()
        }
        // Future milestone stubs
        _ => {
            bail!("command not yet implemented — see milestone roadmap");
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
