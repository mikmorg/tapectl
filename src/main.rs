// Issue #404's follow-up: the closed-pipe-tolerant shadows of `std`'s
// printing macros that the library defines (`tapectl::output`), for this
// crate too.
#[cfg(not(test))]
#[allow(unused_macros)]
macro_rules! println {
    () => { tapectl::output::stdout(format_args!(""), true) };
    ($($arg:tt)*) => { tapectl::output::stdout(format_args!($($arg)*), true) };
}
#[cfg(not(test))]
#[allow(unused_macros)]
macro_rules! print {
    ($($arg:tt)*) => { tapectl::output::stdout(format_args!($($arg)*), false) };
}
#[cfg(not(test))]
#[allow(unused_macros)]
macro_rules! eprintln {
    () => { tapectl::output::stderr(format_args!(""), true) };
    ($($arg:tt)*) => { tapectl::output::stderr(format_args!($($arg)*), true) };
}
#[cfg(not(test))]
#[allow(unused_macros)]
macro_rules! eprint {
    ($($arg:tt)*) => { tapectl::output::stderr(format_args!($($arg)*), false) };
}

use tapectl::{cli, config, db, error, signal, startup, tenant};

use std::ffi::OsString;

use anyhow::{bail, Context};
use clap::{CommandFactory, Parser};

use cli::{Cli, Commands, ConfigCommands};
use config::{Config, TapectlPaths};

fn main() {
    let args: Vec<OsString> = std::env::args_os().collect();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(err) => {
            // Exactly `clap::Error::exit`, with the code decided here: it
            // prints the message unchanged (help and version to stdout, a
            // usage error to stderr) and exits.
            let code = parse_error_exit_code(&err, &args);
            let _ = err.print();
            std::process::exit(code);
        }
    };

    // Issue #172: peek `[logging]` before installing the subscriber, so
    // `logging.level`/`logging.format` actually govern it instead of only
    // ever seeing the `--verbose`-driven bootstrap default. This resolves
    // the home a second time (`run` below resolves it again,
    // authoritatively) rather than threading paths through — cheap, pure,
    // and now (issue #228) an ordinary library call whose whole input
    // table is unit-tested in `startup`, which is what makes "the two
    // resolutions cannot diverge" a checked property rather than an
    // argument.
    //
    // A resolution ERROR is deliberately not reported from here: `run`
    // resolves again and reports it through the single `exit_with_error`
    // path below, so there is exactly one place a bad `TAPECTL_HOME` or an
    // unset `HOME` is explained. Peeking just falls back to the defaults.
    let resolved = startup::resolve(cli.home.as_deref(), cli.config.as_deref()).ok();
    let logging = resolved
        .as_ref()
        .map(|r| startup::peek_logging_config(&r.paths))
        .unwrap_or_default();
    init_tracing(cli.verbose, &logging);

    // Issue #228: this ONE notice goes to stderr directly rather than
    // through `tracing::warn!`. The subscriber's level was just taken from
    // the very file `--config` named, so `logging.level = "error"` in a
    // relocated config silenced the message announcing that that same file
    // had relocated the home — and before issue #172's range the notice was
    // unconditional. Every OTHER `warn!` staying subject to `logging.level`
    // is correct and deliberate; this one is about the resolution the
    // config file itself caused, so it cannot be the config file's to
    // suppress. `cli::key::print_escrow_secret_warning` is the existing
    // precedent for a must-be-seen notice bypassing the pipeline.
    if let Some(home) = resolved
        .as_ref()
        .and_then(|r| r.ambiguous_config_home.as_ref())
    {
        eprintln!("{}", startup::ambiguous_config_notice(home));
    }

    signal::install_handler();

    // Decided before `run` consumes `cli` (issue #356): `volume verify`'s
    // exit contract reserves 2 for "the medium is proven bad", so every
    // error that invocation returns — its own, or the database's or the
    // config's before it ever ran — exits 3, "inconclusive", instead. (A
    // verify whose command line did not parse already exited 3 above, in
    // `parse_error_exit_code`.)
    let error_code = error_exit_code(&cli.command);
    // Issue #393: the progress session lives here, not in `run`, so its log
    // records how the command ended (`session result:`) before its end line.
    let mut session = None;
    let result = run(cli, &mut session);
    if let Some(s) = &session {
        let text = result.as_ref().err().map(|e| format!("{e:#}"));
        s.note_result(match &text {
            None => Ok(()),
            Some(t) => Err(t.as_str()),
        });
    }
    drop(session);
    if let Err(err) = result {
        // Issue #377: a busy catalog is "retry later", not a failure of what
        // the command checks, so it exits 75 — except where a command's own
        // contract already has a code for "no verdict, try again" (`volume
        // verify`'s 3), which it keeps.
        let code = if error_code == error::EXIT_ERROR && db::busy::is_catalog_busy(&err) {
            error::EXIT_CATALOG_BUSY
        } else {
            error_code
        };
        error::exit_with_error_code(&err, code);
    }
}

/// The exit code for an error from `command` — [`error::EXIT_ERROR`] for
/// everything except `volume verify` (see `cli::volume::error_exit_code`).
fn error_exit_code(command: &Commands) -> i32 {
    match command {
        Commands::Volume { command } => cli::volume::error_exit_code(command),
        _ => error::EXIT_ERROR,
    }
}

/// The exit code for a command line that did not parse (issue #356).
///
/// clap's own is 0 for `--help`/`--version` and 2 for a usage error, and 2
/// is what `volume verify` now reserves for "the medium is proven bad, the
/// volume is quarantined". A verify whose command line is wrong — a missing
/// label, a mistyped flag — has read nothing and proved nothing, so it exits
/// [`error::EXIT_VERIFY_INCONCLUSIVE`], exactly as every other error of a
/// verify invocation does ([`error_exit_code`]). Every other command keeps
/// clap's code, and help keeps 0.
///
/// A failed parse leaves no [`Cli`] to ask which command it was, so the
/// same arguments are parsed again leniently (`ignore_errors`), which keeps
/// the subcommand chain clap had matched before it hit the error. Scanning
/// argv by hand instead would have to know which global flags take a value
/// (`--home X volume verify`); the lenient parse knows because it IS the
/// definition. If even that cannot place the invocation under `volume
/// verify` — `volume verfy`, say — clap's code stands.
fn parse_error_exit_code(err: &clap::Error, args: &[OsString]) -> i32 {
    if err.use_stderr() && is_volume_verify_invocation(args) {
        error::EXIT_VERIFY_INCONCLUSIVE
    } else {
        err.exit_code()
    }
}

/// Whether `args` (argv, program name first) names `volume verify`, whether
/// or not the rest of it parses.
fn is_volume_verify_invocation(args: &[OsString]) -> bool {
    let Ok(matches) = Cli::command()
        .ignore_errors(true)
        .try_get_matches_from(args)
    else {
        return false;
    };
    matches!(
        matches.subcommand(),
        Some(("volume", volume)) if volume.subcommand_name() == Some("verify")
    )
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
    use tracing_subscriber::filter::LevelFilter;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{fmt, Layer, Registry};

    let configured = logging.tracing_level();
    let level = if verbose {
        configured.max(tracing::Level::DEBUG)
    } else {
        configured
    };

    // Issue #357: no ANSI colour unless stderr is a terminal (and never
    // under NO_COLOR) — a captured log must not carry raw escape codes.
    let ansi = {
        use std::io::IsTerminal;
        startup::log_ansi(
            std::io::stderr().is_terminal(),
            std::env::var_os("NO_COLOR").as_deref(),
        )
    };
    // Issue #386: the stderr writer clears a drawn progress line before an
    // event lands, so a warning printed mid-phase starts on a clean line.
    //
    // Each `fmt` formatter method returns a distinct layer type, so the four
    // `logging.format` values are boxed to one. `Config::load`'s
    // `validate_closed_sets` already rejects anything outside
    // `config::VALID_LOG_FORMATS`, so the wildcard arm is "full" (the
    // default) plus the bootstrap-before-any-config-file case, never a
    // silent fallback for a typo that should have failed at load.
    let stderr_writer = || tapectl::progress::ClearingStderr;
    let stderr_layer: Box<dyn Layer<Registry> + Send + Sync> = match logging.format.as_str() {
        "compact" => fmt::layer()
            .compact()
            .with_ansi(ansi)
            .with_writer(stderr_writer)
            .boxed(),
        "pretty" => fmt::layer()
            .pretty()
            .with_ansi(ansi)
            .with_writer(stderr_writer)
            .boxed(),
        "json" => fmt::layer()
            .json()
            .with_ansi(ansi)
            .with_writer(stderr_writer)
            .boxed(),
        _ => fmt::layer()
            .with_ansi(ansi)
            .with_writer(stderr_writer)
            .boxed(),
    };

    // Issue #386: every event at INFO and above (DEBUG under `--verbose`,
    // never less than the configured level) is also teed into the session
    // log of a long operation, whatever stderr shows — the log is where an
    // unexplained gap is looked up afterwards. It writes nowhere while no
    // session is open.
    let file_level = level.max(tracing::Level::INFO);
    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_writer(|| tapectl::progress::SessionLogWriter)
        .with_filter(LevelFilter::from_level(file_level));

    let _ = tracing_subscriber::registry()
        .with(stderr_layer.with_filter(LevelFilter::from_level(level)))
        .with(file_layer)
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
        tapectl::progress::note_exit(code);
        let _ = std::io::stdout().flush();
        std::process::exit(code);
    }
}

fn run(
    cli: Cli,
    progress_slot: &mut Option<tapectl::progress::SessionGuard>,
) -> anyhow::Result<()> {
    // Completions need neither a database nor a resolved home, so they are
    // dispatched BEFORE the resolution below (issue #228): an unset `HOME`
    // is now a refusal, and `tapectl completions bash` in the very cron,
    // systemd or container shell that lacks one must keep working — it
    // reads nothing and writes nothing but the script.
    if let Commands::Completions { shell } = cli.command {
        let mut cmd = <Cli as clap::CommandFactory>::command();
        clap_complete::generate(shell, &mut cmd, "tapectl", &mut std::io::stdout());
        return Ok(());
    }

    // Resolve paths (issue #109) — `--home`, `TAPECTL_HOME`, the legacy
    // "`--config` relocates everything" behaviour, then `$HOME/.tapectl`.
    //
    // The precedence, the reasons it is shaped that way, and the three
    // refusals that keep leniency from silently choosing a DIFFERENT
    // archive all live in `tapectl::startup` (issue #228), together with
    // the input table as tests. `main()` above ran this same call before
    // the subscriber existed, to peek `[logging]`, and emitted the
    // ambiguous-`--config` notice from there.
    let paths = startup::resolve(cli.home.as_deref(), cli.config.as_deref())?.paths;

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
            cli.dry_run,
        );
    }

    // `host check` reads /proc and systemd and nothing of the tapectl home
    // but its `[host_check]` table (ADR-0012, 2026-09-24 amendment, item
    // 7), so it runs before the initialization gate: `first-run.sh` and an
    // operator can ask whether the host is quiet on a machine not yet
    // `init`ed. A config that exists is still loaded with every check
    // `Config::load` makes but the backend-collision one, which guards
    // `resolve_lto_backend` — a path this command never reaches (the
    // #261 reasoning) — so an unknown `[host_check]` key is refused here
    // exactly as everywhere else. No config file: every default.
    if let Commands::Host { ref command } = cli.command {
        let cfg = if paths.config_file.exists() {
            Some(
                Config::load_tolerating_backend_ambiguity(&paths.config_file)
                    .context("failed to load config")?,
            )
        } else {
            None
        };
        let exit_code = cli::host::run(cfg.as_ref(), command, cli.json)?;
        exit_if_nonzero(exit_code);
        return Ok(());
    }

    // Issue #393: `status` reads the session logs and nothing else — no
    // catalog, no config, no chmod — so it runs for an account that can
    // only read `<home>/logs/` (a member of the `[ops] group`), before the
    // initialization gate and `ensure_dirs` below, neither of which such an
    // account could pass.
    if let Commands::Status { last } = cli.command {
        cli::status::run(&paths.logs_dir, last, cli.json)?;
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
    //
    // Issue #393: an `[ops] group` shares the session logs with that group
    // (the home 0710, `logs/` 2750, each log 0640) and nothing else. Peeked
    // like `[logging]`, before the strict load below, because the session
    // log is created before it.
    let ops_group = startup::peek_ops_group(&paths);
    paths
        .ensure_dirs_shared(ops_group.as_deref())
        .context("failed to secure tapectl home directories")?;
    tapectl::progress::share_logs_with_group(ops_group.is_some());

    // Issue #386: a long operation — stage create, volume write/resume/
    // verify/read-slices, restore — runs inside a progress session: live
    // progress on stderr (a redrawn line on a terminal, plain periodic lines
    // otherwise, nothing under `--quiet`) and a session log under
    // `<home>/logs/`. Never under `--dry-run`, which moves nothing. Held
    // by `main` past `run`'s return (issue #393), so the log's last lines
    // are the command's result and the session's end.
    *progress_slot = match cli::progress_session_name(&cli.command) {
        Some(name) if !cli.dry_run => {
            use std::io::IsTerminal;
            // Issue #404: a long run over bare ssh dies with the connection;
            // say so before it starts. A warning, never a refusal.
            if let Some(warning) = signal::unprotected_ssh_warning(&name, |k| std::env::var(k).ok())
            {
                eprintln!("{warning}");
            }
            let display = tapectl::progress::Display::choose(
                cli.quiet,
                std::io::stderr().is_terminal(),
                std::env::var("TERM").ok().as_deref(),
            );
            let session =
                tapectl::progress::start_session(Some(&paths.logs_dir), &name, display, true);
            if let Some(log) = session.log_path() {
                tracing::debug!(log = %log.display(), "session log");
            }
            Some(session)
        }
        _ => None,
    };

    // Issue #173: `config check`'s whole job is diagnosing a config that
    // fails to load — so it must not be gated behind the strict
    // `Config::load` two lines below the way every other command
    // (including `config show`) still is. It gets its own view of the
    // config via `policy::lenient_config` instead, so it is dispatched
    // here, before that load, the same way `Init`/`Completions` are
    // special-cased above.
    //
    // `config show` deliberately stays on the normal path below: it reads
    // the raw file either way, so it does not need the lenient parser, and
    // issue #172's own acceptance test depends on `Config::load` actually
    // running for it (the "loaded config" DEBUG line only fires from there).
    if matches!(
        cli.command,
        Commands::Config {
            command: ConfigCommands::Check
        }
    ) {
        let conn = db::open(&paths.db_file).context("failed to open database")?;
        let exit_code = cli::config::run(&conn, &paths, &ConfigCommands::Check, cli.json)?;
        exit_if_nonzero(exit_code);
        return Ok(());
    }

    // Issue #143: `config set`/`add`/`remove` are dispatched before the
    // strict load for `check`'s reason — they are how a config that fails
    // to load gets repaired, and they validate the edited text themselves
    // (`config_edit`). They need no catalog.
    if let Commands::Config {
        command:
            ref command @ (ConfigCommands::Set { .. }
            | ConfigCommands::Add { .. }
            | ConfigCommands::Remove { .. }),
    } = cli.command
    {
        cli::config::run_edit(&paths, command, cli.json, cli.dry_run)?;
        return Ok(());
    }

    // Issue #261 (precedent: #233's `db::open_for_repair`): `Config::load`'s
    // backend-collision refusal (`validate_backends`) calls
    // `std::fs::canonicalize` on both sides of every `[[backends.lto]]`
    // pair, so whether it fires depends on what `/dev` looks like at this
    // instant — CLAUDE.md's own warning that device numbering is not
    // stable on this VM. That refusal is load-bearing for the WRITE path
    // (it is what keeps `resolve_lto_backend`'s `.find(...)` from silently
    // picking the wrong drive, issue #222) and stays exactly as strict as
    // it always was for every command NOT named below.
    //
    // The named set is every command that can be shown to never reach
    // `resolve_lto_backend`/`cli::write_device` — the strict resolver —
    // so the collision protects nothing for them while still being able to
    // brick them outright:
    //   - `Restore` (`Unit`/`File`/`RawVolume`): all three resolve their
    //     device via `cli::read_device` -> `config::resolve_device`, the
    //     LENIENT resolver, by design (ADR-0010's DR path — a rebuilt
    //     machine has keys and no `backend add` yet). `src/cli/restore.rs`.
    //   - `Catalog` (`Ls`/`Search`/`Locate`/`Stats`/`Rebuild`): only
    //     `Rebuild` touches `config` at all, and only via
    //     `config::resolve_device` + `tape::media_detect::check_read_contact`
    //     (itself `resolve_device`) — both lenient, both reads. It rebuilds
    //     the DATABASE from tape; it never writes a TAPE. `src/cli/catalog.rs`.
    //   - `Report` (every subcommand): reads `config.defaults` /
    //     `config.compaction` for policy math only; no subcommand ever
    //     calls a device resolver. `src/cli/report.rs`.
    //   - `Audit`: same — reads `config.defaults`/`config.compaction`
    //     for policy resolution, never a device. `src/cli/audit.rs`.
    //   - `Db { Backup }` and `Db { Fsck { repair: true } }`: `cli::db::run`
    //     doesn't take a `Config` parameter at all (`src/cli/db.rs`) — there
    //     is no code path from either arm to any resolver. `repair: true`
    //     only, mirroring #233's own specificity: a plain `fsck` (no
    //     `--repair`) never mutates and was never the blocked command in
    //     the first place.
    //
    // Every other command (`Volume`, `Collection`, `Snapshot`, `Stage`,
    // `Tenant`, `Key`, `Unit`, `Cartridge`, `ArchiveSet`, `Export`,
    // `Import`, `QuickArchive`, `Backend`, `Db::{Export,Import,Stats}`,
    // `Config::Show`) keeps the strict loader below unconditionally —
    // several of them (`Volume`, `Collection`, `QuickArchive`) reach
    // `resolve_lto_backend` directly and must never see this door.
    let lenient_backend_ok = matches!(
        cli.command,
        Commands::Restore { .. }
            | Commands::Catalog { .. }
            | Commands::Report { .. }
            | Commands::Audit { .. }
            | Commands::Db {
                command: cli::DbCommands::Backup { .. }
            }
            | Commands::Db {
                command: cli::DbCommands::Fsck { repair: true }
            }
    );
    let cfg = match Config::load(&paths.config_file) {
        Ok(cfg) => cfg,
        Err(_) if lenient_backend_ok => {
            Config::load_tolerating_backend_ambiguity(&paths.config_file)
                .context("failed to load config")?
        }
        Err(e) => return Err(e).context("failed to load config"),
    };
    // Issue #172: a real, generically useful DEBUG-level log line — proof,
    // observable from `logging.level = "debug"` alone with no tape/write
    // path involved, that the wired keys actually reach the subscriber
    // `main()` built from them. See `tests/cli_smoke.rs`'s
    // `logging_level_debug_surfaces_this_line`.
    tracing::debug!(config = %paths.config_file.display(), "loaded config");
    // Issue #233: a pre-existing orphan anywhere in the database makes
    // `.foreign_key_check()`'s whole-database check abort `migrate()` the
    // instant any pending migration carrying it runs (003/012/013/017),
    // which otherwise made `db::open` fail for every command — including
    // `db fsck --repair`, the one command that can fix it. Only that exact
    // invocation, and only for this named, repairable condition (never a
    // generic migration failure — see `db::migrate`), is let in through
    // `db::open_for_repair`, a connection that never calls `migrate()`.
    let conn = match db::open(&paths.db_file) {
        Ok(conn) => conn,
        Err(error::TapectlError::DatabaseNeedsRepair(_))
            if matches!(
                cli.command,
                Commands::Db {
                    command: cli::DbCommands::Fsck { repair: true }
                }
            ) =>
        {
            db::open_for_repair(&paths.db_file).context("failed to open database for repair")?
        }
        Err(e) => return Err(e).context("failed to open database"),
    };

    match cli.command {
        Commands::Tenant { ref command } => {
            cli::tenant::run(&conn, &paths, command, cli.json, cli.dry_run)?;
        }
        Commands::Key { ref command } => {
            cli::key::run(&conn, &paths, command, cli.json, cli.dry_run)?;
        }
        Commands::Unit { ref command } => {
            cli::unit::run(&conn, &paths, &cfg, command, cli.json, cli.dry_run, cli.yes)?;
        }
        Commands::Collection { ref command } => {
            // Issue #285: `collection::run` now returns a process exit code
            // (0=clean, 1=warning) when a per-unit dotfile fault refused one
            // unit while every other unit still ran — mirrors the
            // Volume/Config::Check arms, the established mechanism for
            // "work completed, non-zero status" (never an `Err`, which
            // would abort before the healthy units ran).
            let exit_code =
                cli::collection::run(&conn, &paths, &cfg, command, cli.json, cli.dry_run, cli.yes)?;
            exit_if_nonzero(exit_code);
        }
        Commands::Snapshot { ref command } => {
            cli::snapshot::run(&conn, &paths, &cfg, command, cli.json, cli.dry_run, cli.yes)?;
        }
        Commands::Stage { ref command } => {
            cli::stage::run(&conn, &paths, &cfg, command, cli.json, cli.dry_run, cli.yes)?;
        }
        Commands::Staging { ref command } => {
            cli::staging::run(&conn, &paths, &cfg, command, cli.json, cli.dry_run)?;
        }
        Commands::Volume { ref command } => {
            // issue #45/H10: `volume::run` now returns a process exit code
            // for `Verify` (issue #356: 0 = passed, 2 = the medium proven
            // bad and quarantined, 3 = inconclusive); every other
            // subcommand returns EXIT_SUCCESS. Mirrors the Audit arm below.
            // A verify's ERRORS exit 3 too — see `error_exit_code` above.
            let exit_code =
                cli::volume::run(&conn, &paths, &cfg, command, cli.json, cli.yes, cli.dry_run)?;
            exit_if_nonzero(exit_code);
        }
        Commands::Restore { ref command } => {
            cli::restore::run(&conn, &paths, &cfg, command, cli.json, cli.dry_run)?;
        }
        Commands::Catalog { ref command } => {
            cli::catalog::run(&conn, &cfg, command, cli.json, cli.dry_run)?;
        }
        Commands::Location { ref command } => {
            cli::location::run(&conn, command, cli.json, cli.dry_run)?;
        }
        Commands::Cartridge { ref command } => {
            cli::cartridge::run(&conn, &cfg, command, cli.json, cli.yes, cli.dry_run)?;
        }
        Commands::ArchiveSet { ref command } => {
            cli::archive_set::run(&conn, &cfg, command, cli.json, cli.dry_run)?;
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
            cli::operations::export_unit(&conn, unit, to, cli.json, cli.dry_run)?;
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
                capacity.as_deref(),
                device.as_deref(),
                notes.as_deref(),
                cli.json,
                cli.dry_run,
            )?;
        }
        Commands::QuickArchive {
            ref path,
            ref tenant,
            ref volume,
            ref tag,
            ref device,
            prewrite_hash,
            fill_ceiling,
            full_confirm,
        } => {
            cli::operations::quick_archive(
                &conn,
                &paths,
                // Issue #391: `--fill-ceiling` for this command's write only.
                &cfg.with_fill_ceiling(fill_ceiling),
                path,
                tenant,
                volume,
                tag,
                device.as_deref(),
                prewrite_hash,
                full_confirm,
                cli.json,
                cli.dry_run,
                cli.yes,
            )?;
        }
        Commands::Backend { ref command } => {
            cli::backend::run(&paths, command, cli.json, cli.dry_run)?;
        }
        Commands::Db { ref command } => {
            // Body lives in `cli::db` (issue #112). The exit CODE comes back
            // here because acting on it — terminating the process — is the
            // binary's job, not the library's.
            let exit_code = cli::db::run(&conn, &paths, command, cli.json, cli.yes, cli.dry_run)?;
            exit_if_nonzero(exit_code);
        }
        Commands::Config { ref command } => {
            // `Check`, `Set`, `Add` and `Remove` are intercepted above, before
            // the strict `Config::load` (#173, #143); only `Show` reaches here.
            let exit_code = cli::config::run(&conn, &paths, command, cli.json)?;
            exit_if_nonzero(exit_code);
        }
        Commands::Init { .. }
        | Commands::Completions { .. }
        | Commands::Host { .. }
        | Commands::Status { .. } => {
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
    dry_run: bool,
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

    // No side effect — reads only the CLI arg, $USER, the effective uid and
    // /etc/login.defs — so it is safe to compute ahead of the dry-run branch
    // below even though the real path does not need it until the tenant is
    // created, later. Issue #357: under a system account (a service user)
    // it refuses rather than naming the operator tenant after the account,
    // and a dry run refuses identically.
    let op_name = tenant::operator_name_for_init(
        operator_name,
        std::env::var("USER").ok().as_deref(),
        nix::unistd::geteuid().as_raw(),
        tenant::system_uid_min(),
    )?;

    // Issue #247: `init` writes config, keys and the database — a dry run
    // must report all three and create NONE of them (never a half-created
    // home), so this sits above `paths.ensure_dirs()`, the first side
    // effect, exactly the way #139's escrow-key parse above it stays pure.
    if dry_run {
        let staging_dir = paths.home.join("staging");
        let escrow_desc = if no_escrow {
            "not created (--no-escrow)".to_string()
        } else if let Some(ref value) = adopted_escrow_public_key {
            format!("would adopt the supplied key ({value})")
        } else {
            "would mint a new escrow identity (its secret is shown once, at real init time)"
                .to_string()
        };
        if json_output {
            println!(
                "{}",
                serde_json::json!({
                    "home": paths.home.display().to_string(),
                    "operator": op_name,
                    "config": paths.config_file.display().to_string(),
                    "database": paths.db_file.display().to_string(),
                    "staging": staging_dir.display().to_string(),
                    "escrow": escrow_desc,
                    "dry_run": true,
                })
            );
        } else {
            println!(
                "tapectl would be initialized at {} (DRY RUN — no changes made)",
                paths.home.display()
            );
            println!("  operator: {op_name}");
            println!("  database: {} (would be created)", paths.db_file.display());
            println!(
                "  config:   {} (would be created)",
                paths.config_file.display()
            );
            println!("  staging:  {} (would be created)", staging_dir.display());
            println!("  escrow:   {escrow_desc}");
        }
        return Ok(());
    }

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
        // ADR-0012, 2026-09-24 amendment, item 7: the quiet-host keys,
        // documented where the operator will look, commented out so the
        // defaults stay the defaults.
        f.write_all(tapectl::config::HOST_CHECK_EXAMPLE.as_bytes())
            .context("failed to append the host-check example")?;
    }

    // Create database with schema
    let conn = db::open(&paths.db_file).context("failed to create database")?;

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
