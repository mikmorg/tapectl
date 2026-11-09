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

use cli::{Cli, Commands, ConfigCommands};
use config::{Config, TapectlPaths};

fn main() {
    let args: Vec<OsString> = std::env::args_os().collect();
    // ADR-0012 item 212 (OQ7): under `--json` every error is one JSON
    // document on stdout, those raised before clap has parsed the line
    // included, so whether it was asked for is read from the line first.
    let json = cli::json_requested(&args);
    // ADR-0012 item 203: a spelling 1.9.0 removed is refused by name,
    // however it is typed (`--help` and `help <old>` included), before clap
    // parses the line and before any home is resolved, so the refusal reads
    // nothing and creates nothing.
    if let Some(renamed) = cli::renamed::find(&args) {
        error::exit_reporting(&renamed.message(), error::EXIT_ERROR, json);
    }
    // The grouped top-level help (ADR-0012 item 211) is `cli::command()`'s.
    let cli = match cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(err) => {
            // Exactly `clap::Error::exit`, with the code decided here: it
            // prints the message unchanged (help and version to stdout, a
            // usage error to stderr) and exits. Under `--json` a usage error
            // is the JSON error instead; help and version stay text.
            let code = parse_error_exit_code(&err, &args);
            if json && err.use_stderr() {
                error::exit_reporting(&cli::usage_error_text(&err), code, true);
            }
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

    // Decided before `run` consumes `cli` (issue #356): how this
    // invocation's errors map to an exit code. `volume verify`'s exit
    // contract reserves 2 for "the medium is proven bad" and `audit`'s for
    // "at least one violation" (issue #408), so every error those
    // invocations return — their own, or the database's or the config's
    // before they ever ran — exits their "no verdict" code instead. (A
    // command line that did not parse was mapped above, in
    // `parse_error_exit_code`.)
    let contract = cli::error_contract(&cli.command);
    // Item 212: how the error is reported, decided before `run` consumes
    // `cli` too.
    let json = cli.json;
    // Issue #393: the progress session lives here, not in `run`, so its log
    // records how the command ended (`session result:`) before its end line.
    let mut session = None;
    let result = run(cli, &mut session);
    let text = result
        .as_ref()
        .err()
        .map(|e| render_error(e, resolved.as_ref()));
    if let Some(s) = &session {
        s.note_result(match &text {
            None => Ok(()),
            Some(t) => Err(t.as_str()),
        });
    }
    drop(session);
    if let (Err(err), Some(text)) = (result, text) {
        // Issue #377: a busy catalog is "retry later", not a failure of what
        // the command checks, so it exits 75 — except where a command's own
        // contract already has a code for "no verdict, try again" (`volume
        // verify`'s 3), which it keeps. `ErrorContract::exit_code` decides.
        error::exit_reporting_with(
            &text,
            contract.exit_code(&err),
            json,
            error::json_payload(&err),
        );
    }
}

/// The error as the operator reads it: its own text, completed from the
/// catalog where that names the next step better (ADR-0012 items 216 and
/// 220, [`error::render`]) — the nearest label for a `volume not found`,
/// the command that proceeds after a write session only planned. The
/// catalog is opened read-only, and only for those errors; the exit code
/// is decided from the error itself, never from this text.
fn render_error(err: &anyhow::Error, resolved: Option<&startup::Startup>) -> String {
    let catalog = resolved
        .filter(|_| error::render_reads_catalog(err))
        .and_then(|r| error::open_catalog_for_render(&r.paths.db_file));
    error::render(err, catalog.as_ref())
}

/// The exit code for a command line that did not parse (issues #356, #408).
///
/// clap's own is 0 for `--help`/`--version` and 2 for a usage error, and 2
/// is what `volume verify` reserves for "the medium is proven bad, the
/// volume is quarantined" and `audit` for "at least one violation". A verify
/// or an audit whose command line is wrong — a missing label, a mistyped
/// flag — has read nothing and proved nothing, so it exits that command's
/// "no verdict" code ([`error::ErrorContract::usage_error_code`]), exactly
/// as every other error of the invocation does (`cli::error_contract`).
/// Every other command keeps clap's code — the write family's table gives a
/// usage error 2 too — and help keeps 0. Which command a line that did not
/// parse names is `cli::invocation_contract`'s lenient reparse.
fn parse_error_exit_code(err: &clap::Error, args: &[OsString]) -> i32 {
    if !err.use_stderr() {
        return err.exit_code();
    }
    cli::invocation_contract(args)
        .usage_error_code()
        .unwrap_or_else(|| err.exit_code())
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
    // ADR-0012 item 203: the second line behind `main`'s scan of the raw
    // command line. A removed command's hidden stub that clap matched is
    // refused with the same words, still before any home is resolved.
    if let Some(renamed) = cli::renamed::parsed(&cli.command) {
        return Err(renamed.refusal().into());
    }

    if let Commands::Completions { shell } = cli.command {
        // Without the hidden stubs of removed spellings (ADR-0012 item 203).
        let mut cmd = cli::completion_command();
        // ADR-0012 item 220: `clap_complete` `expect`s its own writes, so
        // given stdout itself `completions zsh | head -3` panicked (exit 101)
        // once `head` had gone. Generated into memory, written once through
        // the closed-pipe-tolerant writer.
        let mut script = Vec::new();
        clap_complete::generate(shell, &mut cmd, "tapectl", &mut script);
        tapectl::output::stdout_bytes(&script);
        return Ok(());
    }

    // ADR-0012 item 213 (OQ10): root on a host with a home pointer is
    // refused unless --home or TAPECTL_HOME names the home, naming
    // `tapectl-op`; as root the pointer is never read, so the command would
    // open /root/.tapectl. `host check` reads no home and stays open to
    // root; `--help`, `--version` and `completions` never get here.
    if !matches!(cli.command, Commands::Host { .. }) {
        if let Some(refusal) = startup::root_refusal_now(cli.home.as_deref()) {
            bail!("{refusal}");
        }
    }

    // Resolve paths (issue #109) — `--home`, `TAPECTL_HOME`, the legacy
    // "`--config` relocates everything" behaviour, then `$HOME/.tapectl`.
    //
    // The precedence, the reasons it is shaped that way, and the three
    // refusals that keep leniency from silently choosing a DIFFERENT
    // archive all live in `tapectl::startup` (issue #228), together with
    // the input table as tests. `main()` above ran this same call before
    // the subscriber existed, to peek `[logging]`, and emitted the
    // ambiguous-`--config` notice from there. The rule that chose the home
    // (`Startup::source`) is kept for the messages that name it (item 213).
    let home = startup::resolve(cli.home.as_deref(), cli.config.as_deref())?;
    let paths = home.paths.clone();

    // ADR-0012 item 227: `setup` makes the home itself, step by step, and
    // finishes one a run before it began, so it runs before the
    // initialization gate, as `init` does.
    if let Commands::Setup(ref args) = cli.command {
        cli::setup::run(&home, args, cli.json, cli.dry_run, cli.yes)?;
        return Ok(());
    }

    // Init is special — it creates everything from scratch
    if let Commands::Init {
        ref operator,
        no_escrow,
        ref escrow_public_key,
        ref operator_key,
    } = cli.command
    {
        return cmd_init(
            &paths,
            operator.as_deref(),
            no_escrow,
            escrow_public_key.as_deref(),
            operator_key.as_deref(),
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

    // Issue #393: `status` runs for an account that can only read
    // `<home>/logs/` (a member of the `[ops] group`), so it is dispatched
    // before the initialization gate and `ensure_dirs` below, neither of
    // which such an account could pass. It opens the catalog itself when it
    // may (ADR-0012 item 213) and decides its own exit code: 0 when it read
    // the catalog or showed such an account its view, 2 when it could not
    // read it (OQ6).
    if let Commands::Status { last } = cli.command {
        let exit_code = cli::status::run(&home, last, cli.json)?;
        exit_if_nonzero(exit_code);
        return Ok(());
    }

    // Everything else requires initialization. The error names the path,
    // the rule that chose it, and `init` only for a new archive (item 213).
    if !paths.is_initialized() {
        bail!("{}", startup::not_initialized_message(&home));
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
            // Issue #404: a long run over bare ssh dies with the connection;
            // say so before it starts. A warning, never a refusal. Said at
            // the first phase or tape wait (ADR-0012 item 220), so a command
            // that fails on its arguments (a mistyped label) never says it.
            if let Some(warning) = signal::unprotected_ssh_warning(&name, |k| std::env::var(k).ok())
            {
                tapectl::progress::say_before_work(&warning);
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
        // Item 213: it names the home it checked, and why that one.
        let exit_code = cli::config::check(&conn, &home, cli.json)?;
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
    //     machine has keys and no `drive add` yet). `src/cli/restore.rs`.
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
    //   - `Stage { List }`: `report pending` and `staging status` until 1.9.0
    //     (ADR-0012 items 207, 209), so it keeps the door `report pending`
    //     had. It reads `config.defaults`/`config.compaction` for the copy
    //     floor (`cli::stage::stage_set_rows`) and the staged files on disk,
    //     and never calls a device resolver. `src/cli/stage.rs`.
    //
    // Every other command (`Volume`, `Collection`, `Snapshot`, the rest of
    // `Stage`, `Tenant`, `Key`, `Unit`, `Cartridge`, `ArchiveSet`,
    // `QuickArchive`, `Drive`, `Db::{Export,Import,Stats}`, `Config::Show`)
    // keeps the strict loader below unconditionally —
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
            | Commands::Stage {
                command: cli::stage::StageCommands::List { .. }
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
    // Issue #309 (ADR-0012, 2026-10-07 item 29): `drive poll` takes the
    // drive lock BEFORE the catalog is opened, so a poll that finds a
    // command holding the drive exits 75 having touched nothing at all; it
    // opens the catalog itself once it holds the lock.
    if let Commands::Drive { ref command } = cli.command {
        let exit_code = cli::drive::run(&paths, &cfg, command, cli.json, cli.dry_run)?;
        exit_if_nonzero(exit_code);
        return Ok(());
    }
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
            // ADR-0012 item 220: `unit check-integrity` exits 1 on a damaged
            // or missing file; every other unit command returns 0.
            let exit_code =
                cli::unit::run(&conn, &paths, &cfg, command, cli.json, cli.dry_run, cli.yes)?;
            exit_if_nonzero(exit_code);
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
        Commands::Volume { ref command } => {
            // issue #45/H10: `volume::run` now returns a process exit code
            // for `Verify` (issue #356: 0 = passed, 2 = the medium proven
            // bad and quarantined, 3 = inconclusive); every other
            // subcommand returns EXIT_SUCCESS. Mirrors the Audit arm below.
            // A verify's ERRORS exit 3 too — see `cli::error_contract`.
            let exit_code =
                cli::volume::run(&conn, &paths, &cfg, command, cli.json, cli.yes, cli.dry_run)?;
            exit_if_nonzero(exit_code);
        }
        Commands::Restore { ref command } => {
            cli::restore::run(&conn, &paths, &cfg, command, cli.json, cli.dry_run)?;
        }
        Commands::Catalog { ref command } => {
            cli::catalog::run_with_paths(&conn, &paths, &cfg, command, cli.json, cli.dry_run)?;
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
            // 0 clean, 1 warnings, 2 violations — its ERRORS exit 70
            // (`cli::error_contract`, issue #408), so 2 is only ever a verdict.
            let exit_code = cli::audit::run(&conn, &cfg, unit.as_deref(), action_plan, cli.json)?;
            exit_if_nonzero(exit_code);
        }
        Commands::Report { ref command } => {
            cli::report::run(&conn, &cfg, command, cli.json)?;
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
        Commands::Db { ref command } => {
            // Body lives in `cli::db` (issue #112). The exit CODE comes back
            // here because acting on it — terminating the process — is the
            // binary's job, not the library's.
            let exit_code = cli::db::run(&conn, &paths, command, cli.json, cli.yes, cli.dry_run)?;
            exit_if_nonzero(exit_code);
        }
        Commands::Config { ref command } => {
            // `Check`, `Set`, `Add` and `Remove` are intercepted above, before
            // the strict `Config::load` (#173, #143); only `Show` and `Get` reach here.
            let exit_code = cli::config::run(&conn, &paths, command, cli.json, cli.dry_run)?;
            exit_if_nonzero(exit_code);
        }
        Commands::Init { .. }
        | Commands::Setup(_)
        | Commands::Completions { .. }
        | Commands::Host { .. }
        | Commands::Drive { .. }
        | Commands::Status { .. }
        | Commands::Staging { .. }
        | Commands::Backend { .. }
        | Commands::Export(_)
        | Commands::Import(_) => {
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
    operator_key: Option<&std::path::Path>,
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

    // #424: the same for --operator-key, and its one cross-check, against the
    // escrow key above — the escrow row does not exist yet when the operator
    // tenant is created, so the catalog cannot catch it. A refused `init`
    // leaves no home behind.
    let adopted_operator_key = operator_key
        .map(tapectl::crypto::keys::read_adoptable_identity)
        .transpose()
        .context("invalid --operator-key")?;
    if let Some(ref identity) = adopted_operator_key {
        tenant::refuse_escrow_secret(identity, adopted_escrow_public_key.as_deref())
            .context("invalid --operator-key")?;
    }

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

    // #424 review: a key file left in keys/ where the operator's key would be
    // written is refused here, ahead of the dry-run branch and of every side
    // effect, so the dry run refuses it too and a refused init leaves no
    // catalog or config behind (the tenant code repeats the check later).
    tenant::refuse_stray_key_files(
        paths,
        &op_name,
        if adopted_operator_key.is_some() {
            tenant::ADOPTED_ALIASES
        } else {
            tenant::MINTED_ALIASES
        },
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
        let operator_key_desc = match (&adopted_operator_key, operator_key) {
            (Some(identity), Some(path)) => format!(
                "would adopt {} as \"{op_name}-primary\" ({}); nothing minted",
                path.display(),
                identity.public_key
            ),
            _ => "would mint a primary and a backup key pair".to_string(),
        };
        if json_output {
            println!(
                "{}",
                serde_json::json!({
                    "home": paths.home.display().to_string(),
                    "operator": op_name,
                    "operator_keys": operator_key_desc,
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
            println!("  keys:     {operator_key_desc}");
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

    // The directories, config.toml with the staging directory made, and the
    // catalog's schema: the skeleton `tapectl setup` makes too, through the
    // same function (ADR-0012 item 227).
    let (cfg, conn) = tapectl::setup::make_home(paths, true)?;

    // Create operator tenant with keypairs: minted, or the one adopted
    // (#424), checked again against the catalog (empty here) and against the
    // escrow key about to be adopted.
    let tenant_id = match adopted_operator_key {
        Some(ref identity) => tenant::add_tenant_adopting(
            &conn,
            paths,
            &op_name,
            Some("System operator"),
            true,
            identity,
            adopted_escrow_public_key.as_deref(),
        )?,
        None => tenant::add_tenant(&conn, paths, &op_name, Some("System operator"), true)?,
    };

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
                "operator_key_adopted": adopted_operator_key.as_ref().map(|i| &i.public_key),
                "dar_available": dar_ok,
                "escrow_public_key": escrow_public_key,
                "next": INIT_NEXT_STEPS.iter().map(|(_, cmd)| cmd).collect::<Vec<_>>(),
            })
        );
    } else {
        println!("tapectl initialized at {}", paths.home.display());
        println!("  operator: {op_name}");
        if let Some(identity) = &adopted_operator_key {
            println!(
                "  keys:     adopted \"{op_name}-primary\" ({}); no operator key was minted",
                identity.public_key
            );
        }
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
        print!("{}", init_next_steps());
    }

    Ok(())
}

/// What a new archive needs next, in order (ADR-0012 item 213): a tenant,
/// the drive, what to archive, where its copies go, then a copy. `--move-to`
/// takes a registered location, so `location add` comes before the copy.
const INIT_NEXT_STEPS: &[(&str, &str)] = &[
    (
        "a tenant, whose data is archived",
        "tapectl tenant add <TENANT>",
    ),
    (
        "the tape drive",
        "tapectl drive add --name <NAME> --device-tape /dev/tape/by-id/<DRIVE>-nst",
    ),
    (
        "what to archive, a folder of units",
        "tapectl collection add <NAME> <ROOT> --tenant <TENANT>",
    ),
    (
        "or one folder as a unit",
        "tapectl unit init <PATH> --tenant <TENANT>",
    ),
    ("where copies are kept", "tapectl location add <PLACE>"),
    (
        "a copy, once units are staged",
        "tapectl volume write <NEW-LABEL> --init --full-confirm --move-to <PLACE>",
    ),
];

/// `init`'s ending: [`INIT_NEXT_STEPS`], aligned, and where to look next.
fn init_next_steps() -> String {
    let width = INIT_NEXT_STEPS
        .iter()
        .map(|(what, _)| what.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::from("\nnext steps:\n");
    for (what, cmd) in INIT_NEXT_STEPS {
        out.push_str(&format!("  {what:<width$}  {cmd}\n"));
    }
    out.push_str("`tapectl status` shows where things stand and the next command at every step.\n");
    out
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
