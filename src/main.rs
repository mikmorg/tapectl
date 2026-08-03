use tapectl::{cli, config, db, error, signal, tenant};

use anyhow::{bail, Context};
use clap::Parser;

use cli::{Cli, Commands};
use config::{Config, TapectlPaths};

fn main() {
    let cli = Cli::parse();

    init_tracing(cli.verbose);
    signal::install_handler();

    if let Err(err) = run(cli) {
        error::exit_with_error(&err);
    }
}

/// Install the global tracing subscriber (issue #45/H10 — closes the "no
/// sink" gap: `grep -rn tracing_subscriber` returned zero hits before this,
/// so every `tracing::warn!`/`info!` call in the codebase, including #32's
/// and #36's, went nowhere).
///
/// Writes to **stderr**, never stdout: every command's `--json` mode prints
/// machine-readable output to stdout, and `scripts/mhvtl-verify-gate.sh`
/// pipes `volume verify --json` straight into `tee` + a JSON parser under
/// `pipefail` — a log line on stdout would corrupt that stream for every
/// consumer.
///
/// Default level is WARN, so the existing warn!-only fixes surface without
/// burying a command's own output; `--verbose` raises it to DEBUG (closes
/// #3's "`--verbose` parsed but ignored").
///
/// Non-fatal by design: `try_init` (not `init`) so a failed or repeated
/// install — e.g. this binary embedded in a future test harness that
/// already set a subscriber — is silently ignored rather than panicking a
/// command that would otherwise work fine.
fn init_tracing(verbose: bool) {
    let level = if verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::WARN
    };
    let _ = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Flush stdout, then exit with `code` if it is non-zero (issue #45/H10).
/// A code of 0 is a no-op — the healthy/clean path returns normally rather
/// than calling `process::exit(0)`. The explicit flush guards against
/// `println!`'s buffered output being dropped when stdout is a pipe (exactly
/// how `scripts/mhvtl-verify-gate.sh` invokes this binary, under
/// `pipefail`, so a truncated line is a real failure mode).
fn exit_if_nonzero(code: i32) {
    if code > 0 {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::process::exit(code);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    // Resolve paths (issue #109).
    //
    // Three inputs, in precedence order: --home, TAPECTL_HOME, then the
    // legacy "--config relocates everything" behaviour, then the default.
    //
    // The legacy behaviour is KEPT rather than removed. `--config` deriving
    // the whole home from the config file's parent is genuinely surprising —
    // point it at a config in an unexpected directory and you get an empty
    // catalog instead of an error — but it is also exactly how every test
    // harness obtains an isolated home. Removing it would mean a script that
    // used `--config` for isolation silently starts operating on the REAL
    // ~/.tapectl, which is far worse than the surprise being fixed. So it
    // still works, and now says so; `--home` is the explicit way to mean it.
    let paths = if let Some(home) = cli
        .home
        .clone()
        .or_else(|| std::env::var("TAPECTL_HOME").ok())
    {
        let mut p = TapectlPaths::new(std::path::PathBuf::from(home));
        if let Some(ref config_path) = cli.config {
            // Both given: --home selects the archive, --config selects the
            // file within it. No warning — this combination is unambiguous.
            p.config_file = std::path::PathBuf::from(config_path);
        }
        p
    } else if let Some(ref config_path) = cli.config {
        let config_file = std::path::Path::new(config_path);
        let home = config_file
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf();
        tracing::warn!(
            home = %home.display(),
            "--config given without --home: the tapectl home (database, keys, \
             catalogs, receipts) is being taken from the config file's parent \
             directory. Pass --home to say that explicitly."
        );
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

    // Issue #41: `ensure_dirs` tightens `~/.tapectl`'s directory tree to
    // 0700 (idempotent, warn-not-fail internally — see its doc comment).
    // `cmd_init` below already calls it for a brand-new home, but that
    // alone would only ever protect installs created *after* this fix.
    // Calling it here too is what makes the tightening reach an
    // already-initialized `~/.tapectl` on ordinary use, not just a fresh
    // `tapectl init`.
    paths
        .ensure_dirs()
        .context("failed to secure tapectl home directories")?;

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
        Commands::Collection { ref command } => {
            cli::collection::run(&conn, &paths, &cfg, command, cli.json)?;
        }
        Commands::Snapshot { ref command } => {
            cli::snapshot::run(&conn, &paths, &cfg, command, cli.json)?;
        }
        Commands::Stage { ref command } => {
            cli::stage::run(&conn, &paths, &cfg, command, cli.json)?;
        }
        Commands::Staging { ref command } => {
            cli::staging::run(&conn, &paths, &cfg, command, cli.json)?;
        }
        Commands::Volume { ref command } => {
            // issue #45/H10: `volume::run` now returns a process exit code
            // (0=clean, 1=warning, 2=violation) for `Verify`; every other
            // subcommand returns EXIT_SUCCESS. Mirrors the Audit arm below.
            let exit_code =
                cli::volume::run(&conn, &paths, &cfg, command, cli.json, cli.yes, cli.dry_run)?;
            exit_if_nonzero(exit_code);
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
            cli::cartridge::run(&conn, command, cli.json, cli.yes, cli.dry_run)?;
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
            cli::operations::volume_import(
                &conn,
                &cfg,
                label,
                backend,
                media_type,
                capacity,
                notes.as_deref(),
                cli.json,
            )?;
        }
        Commands::QuickArchive {
            ref path,
            ref tenant,
            ref volume,
            ref tag,
            ref device,
        } => {
            cli::operations::quick_archive(
                &conn, &paths, &cfg, path, tenant, volume, tag, device, cli.json,
            )?;
        }
        Commands::Db { ref command } => {
            // Body lives in `cli::db` (issue #112). The exit CODE comes back
            // here because acting on it — terminating the process — is the
            // binary's job, not the library's.
            let exit_code = cli::db::run(&conn, &paths, command, cli.json, cli.yes, cli.dry_run)?;
            exit_if_nonzero(exit_code);
        }
        Commands::Config { ref command } => {
            cli::config::run(&conn, &paths, command, cli.json)?;
        }
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
