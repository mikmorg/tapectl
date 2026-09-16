use tapectl::{cli, config, db, error, signal, tenant};

use anyhow::{bail, Context};
use clap::Parser;

use cli::{Cli, Commands};
use config::{Config, TapectlPaths};

fn main() {
    let cli = Cli::parse();

    // Issue #172: peek `[logging]` before installing the subscriber, so
    // `logging.level`/`logging.format` actually govern it instead of only
    // ever seeing the `--verbose`-driven bootstrap default. This calls
    // `resolve_paths` a second time (`run` below resolves it again,
    // authoritatively) rather than threading paths through — cheap, pure,
    // and it keeps this peek from having to also reproduce `run`'s
    // ambiguous-`--config`-without-`--home` warning, which cannot fire yet
    // anyway: there is no subscriber for it to reach.
    let (paths, _) = resolve_paths(&cli);
    let logging = peek_logging_config(&paths);
    init_tracing(cli.verbose, &logging);
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
/// Level and format come from `logging` (issue #172: `[logging]` in
/// config.toml, previously parsed and never read) — `--verbose` still
/// raises the floor to DEBUG (closes #3's "`--verbose` parsed but
/// ignored"), but never LOWERS it: `.max()` against the configured level so
/// `logging.level = "trace"` plus `-v` stays TRACE rather than being
/// demoted to DEBUG. `logging`'s own default reproduces the pre-#172
/// hardcoded behaviour (WARN, tracing_subscriber's plain "full" formatter),
/// so an install that has never touched `[logging]` sees no change here.
///
/// Non-fatal by design: `try_init` (not `init`) so a failed or repeated
/// install — e.g. this binary embedded in a future test harness that
/// already set a subscriber — is silently ignored rather than panicking a
/// command that would otherwise work fine.
fn init_tracing(verbose: bool, logging: &config::LoggingConfig) {
    let configured = logging.tracing_level();
    let level = if verbose {
        configured.max(tracing::Level::DEBUG)
    } else {
        configured
    };

    let builder = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr);

    // Each `tracing_subscriber::fmt` formatter method returns a distinct
    // builder type, so the four `logging.format` values each need their own
    // `try_init()` call rather than one shared one — there is no common
    // supertype to build once and format last. `Config::load`'s
    // `validate_closed_sets` already rejects anything outside
    // `config::VALID_LOG_FORMATS`, so the wildcard arm is "full" (the
    // default) plus the bootstrap-before-any-config-file case, never a
    // silent fallback for a typo that should have failed at load.
    let _ = match logging.format.as_str() {
        "compact" => builder.compact().try_init(),
        "pretty" => builder.pretty().try_init(),
        "json" => builder.json().try_init(),
        _ => builder.try_init(),
    };
}

/// Resolve `~/.tapectl` (or wherever `--home`/`--config`/`TAPECTL_HOME`
/// point) — the precedence `run()` has always used, extracted (issue #172)
/// so `main()` can peek `[logging]` before a tracing subscriber exists.
///
/// The second return value is the ambiguous "`--config` given without
/// `--home`" warning's subject (the home it derived), for the caller that
/// actually has a subscriber to emit it through — telling the operator
/// where `--config` implies `--home` sits **is** this resolution, so the
/// warning cannot itself wait for `init_tracing` to run first.
fn resolve_paths(cli: &Cli) -> (TapectlPaths, Option<std::path::PathBuf>) {
    if let Some(home) = cli
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
        (p, None)
    } else if let Some(ref config_path) = cli.config {
        let config_file = std::path::Path::new(config_path);
        let home = config_file
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf();
        (TapectlPaths::new(home.clone()), Some(home))
    } else {
        (TapectlPaths::default_paths(), None)
    }
}

/// Best-effort peek at `[logging]` before a subscriber exists (issue #172).
///
/// Reads just the `logging` table, not the full `Config` — deserializing
/// the whole file here would mean a stale key ANYWHERE else in it (which
/// `run()`'s `Config::load` still rejects, loudly, as the one authoritative
/// parse) also swallows the very `logging.level = "debug"` an operator set
/// to go diagnose that failure.
///
/// Never fails: a missing home, unreadable file, unparseable TOML, absent
/// `[logging]` table, or a `[logging]` table that itself fails to
/// deserialize all fall back to `LoggingConfig::default()` silently. That
/// silence is intentional — this is a convenience for picking the right
/// verbosity/format, not a second validation pass; the authoritative error
/// for a genuinely broken config still surfaces once `run()` loads it for
/// real.
fn peek_logging_config(paths: &TapectlPaths) -> config::LoggingConfig {
    std::fs::read_to_string(&paths.config_file)
        .ok()
        .and_then(|content| content.parse::<toml::Value>().ok())
        .and_then(|value| value.get("logging").cloned())
        .and_then(|logging_value| toml::to_string(&logging_value).ok())
        .and_then(|logging_str| toml::from_str(&logging_str).ok())
        .unwrap_or_default()
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
    //
    // The precedence itself lives in `resolve_paths` (issue #172), shared
    // with `main()`'s pre-subscriber `[logging]` peek; this call is what
    // actually emits the ambiguous-`--config` warning, now that a
    // subscriber is guaranteed to exist to receive it.
    let (paths, ambiguous_config_home) = resolve_paths(&cli);
    if let Some(home) = ambiguous_config_home {
        tracing::warn!(
            home = %home.display(),
            "--config given without --home: the tapectl home (database, keys, \
             catalogs, receipts) is being taken from the config file's parent \
             directory. Pass --home to say that explicitly."
        );
    }

    // Init is special — it creates everything from scratch
    if let Commands::Init {
        ref operator,
        no_escrow,
        ref escrow_public_key,
    } = cli.command
    {
        return cmd_init(
            &paths,
            operator.as_deref(),
            no_escrow,
            escrow_public_key.as_deref(),
            cli.json,
        );
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
    // Issue #172: a real, generically useful DEBUG-level log line — proof,
    // observable from `logging.level = "debug"` alone with no tape/write
    // path involved, that the wired keys actually reach the subscriber
    // `main()` built from them. See `tests/cli_smoke.rs`'s
    // `logging_level_debug_surfaces_this_line`.
    tracing::debug!(config = %paths.config_file.display(), "loaded config");
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
            cli::catalog::run(&conn, &cfg, command, cli.json)?;
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
            ref generation,
            ref capacity,
            ref device,
            ref notes,
        } => {
            cli::operations::volume_import(
                &conn,
                &cfg,
                label,
                backend,
                generation,
                capacity,
                device.as_deref(),
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
                &conn,
                &paths,
                &cfg,
                path,
                tenant,
                volume,
                tag,
                device.as_deref(),
                cli.json,
            )?;
        }
        Commands::Backend { ref command } => {
            cli::backend::run(&paths, command, cli.json)?;
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
    no_escrow: bool,
    escrow_public_key_arg: Option<&str>,
    json_output: bool,
) -> anyhow::Result<()> {
    if paths.is_initialized() {
        bail!("tapectl is already initialized at {}", paths.home.display());
    }

    // #139: parse a supplied --escrow-public-key before any side effect below
    // (directories, config, database) so a bad value leaves nothing behind —
    // the disaster-recovery path depends on a failed `init` creating nothing
    // to clean up. clap's `conflicts_with` already rules out this being set
    // together with --no-escrow.
    let adopted_escrow_public_key = escrow_public_key_arg
        .map(tapectl::crypto::keys::read_or_parse_public_key)
        .transpose()
        .context("invalid --escrow-public-key")?;

    // Create directory structure
    paths.ensure_dirs()?;

    // Write default config, then append the commented [[backends.lto]] example.
    // It has to go on after serialization: an empty Vec serializes to nothing,
    // and toml round-trips drop comments, so the section cannot be carried in
    // the struct — leaving a fresh config with no hint that a tape drive must
    // be declared at all, or what it needs (#124).
    let mut cfg = Config::default();
    // Issue #140: the staging default is `<tapectl home>/staging`, and the
    // home a fresh init is actually using may be `--home`/`TAPECTL_HOME`
    // rather than the default one the serde default can see. Record the real
    // one, and CREATE it here with the same 0700 discipline as the rest of
    // the home — a default path that does not exist is what made
    // `config check`'s staging warning fire on every stock machine.
    let staging_dir = paths.home.join("staging");
    std::fs::create_dir_all(&staging_dir).context("failed to create the staging directory")?;
    tapectl::config::secure_path(&staging_dir, 0o700);
    cfg.staging.directory = staging_dir.to_string_lossy().into_owned();
    cfg.save(&paths.config_file)?;
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&paths.config_file)
            .context("failed to reopen config to append the backend example")?;
        f.write_all(tapectl::config::LTO_BACKEND_EXAMPLE.as_bytes())
            .context("failed to append the backend example")?;
    }

    // Create database with schema
    let conn = db::open(&paths.db_file).context("failed to create database")?;

    // Determine operator name
    let op_name = operator_name
        .map(String::from)
        .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "operator".to_string()));

    // Create operator tenant with keypairs
    let tenant_id = tenant::add_tenant(&conn, paths, &op_name, Some("System operator"), true)?;

    // Create the permanent escrow recipient (ADR-0005) as part of first-run
    // setup (CTO decision 2026-09-10): `stage create` and `volume write` refuse
    // to run without one (issue #115), and `volume write` is otherwise the
    // first command that even mentions escrow — which is exactly how a tape
    // was once sealed unrecoverable-by-escrow. Doing it here closes that
    // window at the source. `--no-escrow` opts out (adopt an existing identity
    // with `key import --escrow` instead). `--escrow-public-key` is a third
    // arm (CTO decision 2026-09-11, #139): the disaster-recovery form, where a
    // rebuilt machine adopts the ORIGINAL escrow identity from the heir kit's
    // cover sheet instead of minting a replacement that every existing tape
    // was never encrypted to. In the mint arm the SECRET is printed once to
    // STDERR and stored nowhere; the adopt arm has no secret to print — its
    // secret half lives only on the heir kit.
    let escrow_public_key: Option<String> = if no_escrow {
        None
    } else if let Some(ref value) = adopted_escrow_public_key {
        let adopted = tapectl::cli::key::adopt_escrow_recipient(&conn, paths, value)
            .context("failed to adopt the supplied escrow public key")?;
        Some(adopted)
    } else {
        let created = tapectl::cli::key::create_escrow_recipient(&conn, paths, None)
            .context("failed to create the escrow recipient")?;
        tapectl::cli::key::print_escrow_secret_warning(
            &created.keypair.public_key,
            &created.keypair.secret_key,
        );
        Some(created.keypair.public_key)
    };

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
                "escrow_public_key": escrow_public_key,
            })
        );
    } else {
        println!("tapectl initialized at {}", paths.home.display());
        println!("  operator: {op_name}");
        println!("  database: {}", paths.db_file.display());
        println!("  config:   {}", paths.config_file.display());
        match &escrow_public_key {
            Some(pk) if adopted_escrow_public_key.is_some() => {
                println!(
                    "  escrow:   adopted {pk} (imported — its secret lives on the heir kit, not here)"
                );
            }
            Some(pk) => {
                println!("  escrow:   {pk}");
                println!(
                    "            (the SECRET was printed above — transcribe it now onto paper; ADR-0005)"
                );
            }
            None => println!(
                "  escrow:   not created (--no-escrow) — register one with `key generate --escrow` or `key import --escrow` before staging"
            ),
        }
        if dar_ok {
            // Issue #119/#124: use the same PATH-resolution helper `config
            // check` uses, so a bare, PATH-resolved dar.binary (the default
            // as of issue #124) shows the operator where it actually
            // resolved to, not just the bare name they configured.
            let found_at = if dar_path.contains('/') {
                None
            } else {
                std::env::var_os("PATH")
                    .and_then(|path_var| {
                        tapectl::policy::depth_check::resolve_on_path(dar_path, &path_var)
                    })
                    .map(|p| p.display().to_string())
            };
            match found_at {
                Some(found) => println!("  dar:      {dar_path} (found at {found})"),
                None => println!("  dar:      {dar_path} (ok)"),
            }
        } else if dar_path.contains('/') {
            println!("  dar:      {dar_path} (NOT FOUND — install before staging)");
        } else {
            println!("  dar:      {dar_path} (NOT FOUND on PATH — install before staging)");
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
