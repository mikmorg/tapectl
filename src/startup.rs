//! The startup path: which tapectl home an invocation operates on, and the
//! pre-subscriber `[logging]` peek that decides how loudly it talks.
//!
//! Extracted from `src/main.rs` (issue #228). It lived there as two private
//! `fn`s in the **bin** target, which put the single most consequential
//! resolution in the program — *which archive am I about to write to* —
//! beyond the reach of every test that does not spawn a subprocess with
//! bespoke environment. `CLAUDE.md` is explicit that `main.rs` is a thin
//! wrapper and logic belongs in the library; a path-precedence engine and a
//! second TOML parser are not dispatch.
//!
//! The shape that makes it testable is [`resolve_from`]: the environment
//! arrives as **parameters**, not as `std::env` reads, so the whole input
//! table is exercised by ordinary parallel unit tests with no `set_var`
//! (which is `unsafe` since Rust 2024 and racy in a threaded test binary
//! regardless). [`resolve`] is the thin wrapper that reads the real
//! environment and is the only impure thing here.
//!
//! ## Precedence
//!
//! `--home` > `TAPECTL_HOME` > `--config`'s parent directory > an existing
//! `$HOME/.tapectl` > the system home pointer (below) > `$HOME/.tapectl`
//! as the place a new home will be made.
//!
//! The **system home pointer** (issue #437, ADR-0012 2026-10-15 item 62) is
//! `/etc/tapectl/home`: one absolute path, the service user's tapectl home,
//! written by `scripts/install-systemd.sh` and first-run step 7. It is what
//! lets an `[ops] group` member run a plain `tapectl status` — their `$HOME`
//! is their own, not the service user's. It is consulted only when nothing
//! above it names a home AND this account has no `~/.tapectl` of its own, so
//! every invocation that resolved before it existed (the mhvtl gate's
//! `--config` isolation, a test's `HOME=<tmp>`, the service user itself)
//! resolves exactly as it did. Three rules keep it from changing which
//! archive is written:
//!
//! - **Never as root** (euid 0): root acting on the service home would leave
//!   root-owned files in it. The pointer is not even read. And since root
//!   would otherwise open `/root/.tapectl`, an archive nobody chose, root is
//!   refused outright on a host that has a pointer unless `--home` or
//!   `TAPECTL_HOME` names the home ([`root_refusal`], ADR-0012 item 213):
//!   the service user's commands go through `tapectl-op`.
//! - **A pointer that is present but wrong is an error naming the file** — a
//!   relative path, an empty file, a target that is missing or not a
//!   directory, a file that cannot be read. Falling through to
//!   `$HOME/.tapectl` would create a fresh archive somewhere nobody chose.
//!   A pointer file that is simply absent (`ENOENT`) is "no pointer".
//! - **The location is a parameter.** `TAPECTL_HOME_POINTER` overrides
//!   `/etc/tapectl/home`; it is read once, in [`resolve`], like every other
//!   environment input, so no test touches `/etc` or sets a variable. Point
//!   it at a path that does not exist to switch the pointer off.
//!
//! `--config` on its own relocating the entire home is deliberate and
//! load-bearing (issue #109): it is how every test harness — and
//! `scripts/mhvtl-verify-gate.sh` — obtains an isolated archive. Removing it
//! would mean a script that used `--config` for isolation silently starts
//! operating on the operator's REAL `~/.tapectl`. So it still works, and it
//! announces itself through [`ambiguous_config_notice`].
//!
//! ## Leniency, and where it stops
//!
//! tapectl must stay usable on a rebuilt machine that has keys and no
//! `drive add` (ADR-0005's DR path), so leniency is not a defect in
//! itself. The line this module draws is narrower: **leniency that changes
//! which home is used is a defect.** Hence an empty `TAPECTL_HOME` falls
//! back (it is an unset variable a wrapper forgot to expand, and falling
//! back reaches the home the operator already has), while a non-UTF-8 one
//! is refused (silently swapping in the production catalog is exactly the
//! failure the variable was meant to avoid), and an unset `HOME` is refused
//! rather than guessed as `/root`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::config::{self, LoggingConfig, TapectlPaths};
use crate::error::{Result, TapectlError};

/// Everything the startup path resolves, in one value.
#[derive(Debug, Clone)]
pub struct Startup {
    /// The archive this invocation operates on.
    pub paths: TapectlPaths,

    /// `Some(home)` when `--config` alone derived the home — the subject of
    /// [`ambiguous_config_notice`]. `None` whenever the home was named
    /// explicitly (`--home`/`TAPECTL_HOME`), came from the system home
    /// pointer, or defaulted, because none of those
    /// is surprising and a notice on the invocation that got it right is
    /// just noise.
    pub ambiguous_config_home: Option<PathBuf>,

    /// Which rule chose the home (ADR-0012 item 213): `status`, `config
    /// check` and the "not initialized" error name it.
    pub source: HomeSource,

    /// The system home pointer's file when one is present (a stat, never a
    /// read), whether or not it chose the home: an installed host's home is
    /// not made with `tapectl init`, so the "not initialized" error offers
    /// `init` only where there is none.
    pub pointer_file: Option<PathBuf>,

    /// Whether a non-empty `TAPECTL_HOME` was set, whatever outranked it.
    pub tapectl_home_set: bool,
}

/// The rule that chose an invocation's home, one per rung of the precedence
/// in the module doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeSource {
    /// `--home`.
    HomeFlag,
    /// `TAPECTL_HOME`.
    TapectlHomeEnv,
    /// `--config` alone: its file's directory.
    ConfigFlag,
    /// This account's `~/.tapectl`, which existed.
    OwnHome,
    /// The system home pointer, at this path.
    Pointer(PathBuf),
    /// `~/.tapectl` because nothing else named a home: it did not exist, and
    /// no pointer named another (or, as root, none was read).
    Default,
}

impl HomeSource {
    /// Why this home, in a few words: `home: <path> (<why>)`.
    pub fn describe(&self) -> String {
        match self {
            HomeSource::HomeFlag => "named by --home".to_string(),
            HomeSource::TapectlHomeEnv => "named by TAPECTL_HOME".to_string(),
            HomeSource::ConfigFlag => "the directory of --config".to_string(),
            HomeSource::OwnHome => "this account's ~/.tapectl".to_string(),
            HomeSource::Pointer(file) => {
                format!("named by the home pointer {}", file.display())
            }
            HomeSource::Default => {
                "~/.tapectl, the default: nothing named another home".to_string()
            }
        }
    }

    /// The `--json` spelling (`home_source`).
    pub fn key(&self) -> &'static str {
        match self {
            HomeSource::HomeFlag => "home_flag",
            HomeSource::TapectlHomeEnv => "tapectl_home",
            HomeSource::ConfigFlag => "config_flag",
            HomeSource::OwnHome => "own_home",
            HomeSource::Pointer(_) => "pointer",
            HomeSource::Default => "default",
        }
    }
}

impl Startup {
    /// `home: <path> (<why>)`, the line `status` and `config check` begin
    /// with.
    pub fn home_line(&self) -> String {
        format!(
            "home: {} ({})",
            self.paths.home.display(),
            self.source.describe()
        )
    }
}

/// Where the system-wide home pointer lives unless `TAPECTL_HOME_POINTER`
/// says otherwise (issue #437).
pub const DEFAULT_HOME_POINTER: &str = "/etc/tapectl/home";

/// The inputs the system home pointer needs beyond the environment: where it
/// is, and who is asking. Passed in so the rule is a function of its
/// arguments — a test names a temp file and any euid it likes, with no root
/// and no `/etc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomePointer {
    /// The pointer file: [`DEFAULT_HOME_POINTER`], or `TAPECTL_HOME_POINTER`.
    pub path: PathBuf,
    /// The effective uid of this process. 0 switches the pointer off.
    pub euid: u32,
}

/// The pointer file's location from `TAPECTL_HOME_POINTER`: unset or empty
/// is the default, anything else is the path (an empty value is a variable
/// a wrapper forgot to expand, as with `TAPECTL_HOME`).
fn pointer_path_from_env(value: Option<&OsStr>) -> PathBuf {
    match value {
        Some(raw) if !raw.is_empty() => PathBuf::from(raw),
        _ => PathBuf::from(DEFAULT_HOME_POINTER),
    }
}

/// Resolve against the real process environment. The only impure function
/// in this module; everything it does is [`resolve_from`].
pub fn resolve(home_flag: Option<&str>, config_flag: Option<&str>) -> Result<Startup> {
    let tapectl_home = std::env::var_os("TAPECTL_HOME");
    let home_env = std::env::var_os("HOME");
    let pointer = HomePointer {
        path: pointer_path_from_env(std::env::var_os("TAPECTL_HOME_POINTER").as_deref()),
        euid: nix::unistd::geteuid().as_raw(),
    };
    resolve_from(
        home_flag,
        config_flag,
        tapectl_home.as_deref(),
        home_env.as_deref(),
        Some(&pointer),
    )
}

/// The resolution itself — a function of exactly five inputs, which is what
/// lets the input table be a unit test rather than a subprocess.
///
/// `tapectl_home_env` and `home_env` are `OsStr`, not `str`, on purpose:
/// the difference between "unset", "set to the empty string" and "set to
/// something that is not UTF-8" is precisely where two of issue #228's
/// defects lived, and `std::env::var(..).ok()` collapses all three into
/// `None`-or-`Some`.
///
/// `pointer` is the system home pointer (issue #437); `None` switches it off
/// outright. When it is `Some`, the only filesystem reads are the ones its
/// rule needs, and only when nothing ranked above it has named a home.
pub fn resolve_from(
    home_flag: Option<&str>,
    config_flag: Option<&str>,
    tapectl_home_env: Option<&OsStr>,
    home_env: Option<&OsStr>,
    pointer: Option<&HomePointer>,
) -> Result<Startup> {
    // Facts the messages need whatever rule wins: a stat of the pointer
    // file (never a read, so a broken pointer still cannot break what
    // outranks it) and whether TAPECTL_HOME was set.
    let pointer_file = pointer.map(|p| p.path.clone()).filter(|p| exists(p));
    let tapectl_home_set = tapectl_home_env.is_some_and(|v| !v.is_empty());
    let startup =
        |paths: TapectlPaths, ambiguous_config_home: Option<PathBuf>, source: HomeSource| Startup {
            paths,
            ambiguous_config_home,
            source,
            pointer_file: pointer_file.clone(),
            tapectl_home_set,
        };

    // --home wins outright, and is checked BEFORE the environment: an
    // explicit flag must not be blocked by a broken `TAPECTL_HOME` it
    // overrides anyway.
    let named_home: Option<(PathBuf, HomeSource)> = match home_flag {
        Some("") => return Err(empty_path_error("--home")),
        Some(home) => Some((PathBuf::from(home), HomeSource::HomeFlag)),
        None => {
            tapectl_home_from_env(tapectl_home_env)?.map(|home| (home, HomeSource::TapectlHomeEnv))
        }
    };

    if let Some((home, source)) = named_home {
        let mut paths = TapectlPaths::new(home);
        if let Some(config_path) = config_flag {
            // Both given: --home selects the archive, --config selects the
            // file within it. Unambiguous, so no notice.
            paths.config_file = PathBuf::from(non_empty_config(config_path)?);
        }
        return Ok(startup(paths, None, source));
    }

    if let Some(config_path) = config_flag {
        let home = config_home(non_empty_config(config_path)?);
        return Ok(startup(
            TapectlPaths::new(home.clone()),
            Some(home),
            HomeSource::ConfigFlag,
        ));
    }

    // Nothing named a home. An account that already has a `~/.tapectl` keeps
    // it (the service user, a developer, every harness that sets HOME); only
    // an account with none of its own is sent to the system pointer.
    let own_home_exists = || config::default_home_from(home_env).is_some_and(|d| exists(&d));
    if let Some(pointer) = pointer.filter(|p| p.euid != 0) {
        if !own_home_exists() {
            if let Some(home) = read_home_pointer(&pointer.path)? {
                return Ok(startup(
                    TapectlPaths::new(home),
                    None,
                    HomeSource::Pointer(pointer.path.clone()),
                ));
            }
        }
    }

    let home = default_home(home_env)?;
    let source = if own_home_exists() {
        HomeSource::OwnHome
    } else {
        HomeSource::Default
    };
    Ok(startup(TapectlPaths::new(home), None, source))
}

/// The refusal for root on a host with a home pointer (ADR-0012 item 213,
/// the 1.9.0 coordinator's OQ10), or `None`. Root is refused only when the
/// pointer file is present and neither `--home` nor a non-empty
/// `TAPECTL_HOME` names the home: as root the pointer is never read (item
/// 62), so such a command would open `/root/.tapectl`, an archive nobody
/// chose, instead of the service user's. `main` asks before resolving the
/// home, for every command but `host check` (`--help`, `--version` and
/// `completions` never reach it).
pub fn root_refusal(
    euid: u32,
    pointer_file: &Path,
    home_flag: Option<&str>,
    tapectl_home_env: Option<&OsStr>,
) -> Option<String> {
    let named = home_flag.is_some() || tapectl_home_env.is_some_and(|v| !v.is_empty());
    if euid != 0 || named || !exists(pointer_file) {
        return None;
    }
    // Named when it can be read; the refusal stands either way.
    let home = std::fs::read_to_string(pointer_file)
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty() && !t.contains('\n'))
        .map(|t| format!(" ({t})"))
        .unwrap_or_default();
    Some(format!(
        "refusing to run as root: the home pointer {} names this host's tapectl home{home}, \
         which belongs to the service user, and as root tapectl would open /root/.tapectl \
         instead. Run it as the service user: tapectl-op <command>, e.g. `tapectl-op status`. \
         To act on a home as root on purpose, name it with --home <dir> or TAPECTL_HOME.",
        pointer_file.display()
    ))
}

/// [`root_refusal`] for this process: its euid, `TAPECTL_HOME_POINTER` and
/// `TAPECTL_HOME`.
pub fn root_refusal_now(home_flag: Option<&str>) -> Option<String> {
    root_refusal(
        nix::unistd::geteuid().as_raw(),
        &pointer_path_from_env(std::env::var_os("TAPECTL_HOME_POINTER").as_deref()),
        home_flag,
        std::env::var_os("TAPECTL_HOME").as_deref(),
    )
}

/// The "not initialized" error (ADR-0012 item 213): the path looked at, the
/// rule that chose it, and `tapectl init` only where this looks like a new
/// archive — no home pointer on the host and no `TAPECTL_HOME`. Where one
/// is set, the archive is meant to exist already, and an `init` would make a
/// second one beside it.
pub fn not_initialized_message(startup: &Startup) -> String {
    let home = &startup.paths.home;
    let db = &startup.paths.db_file;
    // An account that cannot see into the home is told so, not that there is
    // no archive there.
    if let Err(e) = std::fs::symlink_metadata(db) {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            return format!(
                "cannot reach the catalog {} ({}): permission denied. The home belongs to \
                 another account: run the command as that account (`tapectl-op <command>` \
                 runs it as the service user), or, as a member of its [ops] group, use \
                 `tapectl status`.",
                db.display(),
                startup.source.describe()
            );
        }
    }
    let rule = match &startup.source {
        HomeSource::HomeFlag => "--home named it".to_string(),
        HomeSource::TapectlHomeEnv => "TAPECTL_HOME named it".to_string(),
        HomeSource::ConfigFlag => {
            "it is the directory of --config (--config alone moves the whole home)".to_string()
        }
        HomeSource::OwnHome => format!(
            "it is this account's ~/.tapectl, which exists{}",
            match &startup.pointer_file {
                Some(p) => format!(
                    " and so outranks the home pointer {}: remove the empty ~/.tapectl to use \
                     the home the pointer names",
                    p.display()
                ),
                None => String::new(),
            }
        ),
        HomeSource::Pointer(p) => format!("the home pointer {} named it", p.display()),
        HomeSource::Default => {
            "nothing named a home (no --home, TAPECTL_HOME or --config, no ~/.tapectl and no \
             home pointer), so it is ~/.tapectl"
                .to_string()
        }
    };
    let mut msg = format!(
        "tapectl is not initialized at {}: there is no catalog {} there. The home was chosen \
         because {rule}.",
        home.display(),
        db.display()
    );
    if startup.source == HomeSource::TapectlHomeEnv {
        msg.push_str(
            " TAPECTL_HOME is set, so the archive is meant to be there already: check that it \
             names the directory holding tapectl.db.",
        );
    } else if let (Some(p), false) = (&startup.pointer_file, startup.source == HomeSource::OwnHome)
    {
        msg.push_str(&format!(
            " This host has a home pointer ({}), so its archive is meant to exist already: \
             check the pointer, or run the command as the service user (`tapectl-op \
             <command>`).",
            p.display()
        ));
    } else if startup.pointer_file.is_none() && !startup.tapectl_home_set {
        let init = match &startup.source {
            HomeSource::HomeFlag | HomeSource::ConfigFlag => {
                format!("`tapectl --home {} init`", home.display())
            }
            _ => "`tapectl init`".to_string(),
        };
        msg.push_str(&format!(" To start a new archive there: {init}."));
    }
    msg
}

/// Whether something is at `path`. Only a definite "not there" is false: a
/// dangling symlink, or a path this account cannot stat, still counts as
/// present, so the pointer can never redirect an account that has a
/// `~/.tapectl` it merely cannot read — that account keeps today's error.
fn exists(path: &Path) -> bool {
    !matches!(
        std::fs::symlink_metadata(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound
    )
}

/// The home the system pointer names: `Ok(None)` when there is no pointer
/// file, `Ok(Some(dir))` for a usable one, and an error naming the file for
/// any pointer that is present but wrong (issue #437).
///
/// Strict on purpose. The caller has found no home anywhere else, so the
/// alternative to an error is `$HOME/.tapectl`, which a read-only command
/// would not create but `init` or `unit init` would — a second, unintended
/// archive. Only an absent file (`ENOENT`) is "no pointer".
fn read_home_pointer(file: &Path) -> Result<Option<PathBuf>> {
    let bad = |why: String| {
        TapectlError::Config(format!(
            "the tapectl home pointer {} {why}. It names the tapectl home an account without \
             a ~/.tapectl uses; fix or remove it, or name the home with --home <dir> \
             (or TAPECTL_HOME).",
            file.display()
        ))
    };
    let bytes = match std::fs::read(file) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(bad(format!("cannot be read ({e})"))),
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| bad("is not valid UTF-8".to_string()))?
        .trim();
    if text.is_empty() {
        return Err(bad("is empty; it must hold one absolute path".to_string()));
    }
    if text.contains('\n') {
        return Err(bad(
            "holds more than one line; it must hold one absolute path".to_string(),
        ));
    }
    let home = PathBuf::from(text);
    if !home.is_absolute() {
        return Err(bad(format!(
            "names '{text}', which is not an absolute path (a relative path would mean a \
             different home for every directory the command is run from)"
        )));
    }
    match std::fs::metadata(&home) {
        Ok(meta) if meta.is_dir() => Ok(Some(home)),
        Ok(_) => Err(bad(format!(
            "names {}, which is not a directory",
            home.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(bad(format!(
            "names {}, which does not exist",
            home.display()
        ))),
        Err(e) => Err(bad(format!(
            "names {}, which this account cannot reach ({e}); an `[ops] group` member can, \
             anyone else is not meant to",
            home.display()
        ))),
    }
}

/// The home `--config <path>` implies: the config file's directory.
///
/// Issue #228, finding 4: `Path::new("config.toml").parent()` returns
/// `Some("")`, **not** `None`, so an `unwrap_or(".")` guard alone never
/// fires for the common bare-relative case. The empty home that produced
/// then rendered as nothing at all in the one message whose whole job is to
/// name the home, and went on to reach `secure_path(Path::new(""), 0o700)`,
/// whose ENOENT surfaced as a second, entirely confusing warning about
/// permissions on a path the operator never named. Filtering the empty
/// parent out is what makes both go away — the resolution is identical
/// either way, since `""` and `"."` name the same directory.
fn config_home(config_path: &str) -> PathBuf {
    Path::new(config_path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

/// `TAPECTL_HOME`, read strictly enough that it cannot silently name a
/// different archive than the operator meant (issue #228, finding 2).
///
/// - **Unset** → `None`, fall through the precedence chain.
/// - **Empty** → `None`. `std::env::var` reports an empty-but-set variable
///   as `Ok("")`, which `TapectlPaths::new` then resolves *relative to the
///   current working directory* — so `TAPECTL_HOME=$ARCHIVE_ROOT tapectl
///   init` with `ARCHIVE_ROOT` unset did not fall back to `~/.tapectl`, it
///   invented a complete new archive (db, keys, config) wherever the
///   process happened to be started, and printed `tapectl initialized at `
///   with nothing after it. Treating it as unset reaches the archive the
///   operator already has.
/// - **Not UTF-8** → refused. The same value passed as `--home` is a hard
///   clap error (`Error::invalid_utf8`); `.ok()` on `std::env::var` instead
///   swallowed `VarError::NotUnicode` and fell through to the REAL
///   `~/.tapectl`, with no message of any kind. The flag refusing loudly
///   while the variable fails silently into the production catalog is the
///   asymmetry worth removing.
fn tapectl_home_from_env(value: Option<&OsStr>) -> Result<Option<PathBuf>> {
    match value {
        None => Ok(None),
        Some(raw) if raw.is_empty() => Ok(None),
        Some(raw) => match raw.to_str() {
            Some(home) => Ok(Some(PathBuf::from(home))),
            None => Err(TapectlError::Config(format!(
                "TAPECTL_HOME is not valid UTF-8 (bytes, lossily: {}). The same value passed \
                 as --home is refused by the argument parser, so it is refused here too \
                 rather than being ignored — ignoring it would silently operate on \
                 ~/.tapectl instead, which is the archive TAPECTL_HOME was set to avoid.",
                raw.to_string_lossy()
            ))),
        },
    }
}

/// `$HOME/.tapectl`, or a refusal (issue #228, finding 2, same class).
///
/// [`config::default_home_from`] answers `None` when `HOME` names nothing
/// usable; the pre-#228 code turned that into `/root` — so a cron, systemd
/// or container invocation silently operated on `/root/.tapectl`, an
/// archive nobody chose. It is refused here instead, and the refusal names
/// the two ways to say what was meant.
///
/// The check is **lazy** on purpose: it is only reached when `HOME` is
/// genuinely what the home would be derived from. `--home`/`TAPECTL_HOME`
/// with `HOME` unset is precisely the cron/systemd invocation this refusal
/// exists to help, and must keep working.
///
/// A non-UTF-8 `HOME` is accepted (a `PathBuf` holds it exactly) rather
/// than refused: unlike `TAPECTL_HOME` it has no flag counterpart to stay
/// consistent with, and using the operator's actual home directory is the
/// right answer where the pre-#228 `std::env::var` path quietly substituted
/// `/root`.
fn default_home(home_env: Option<&OsStr>) -> Result<PathBuf> {
    config::default_home_from(home_env).ok_or_else(|| {
        TapectlError::Config(
            "HOME is not set (or is empty), so there is no ~/.tapectl to default to. Name \
             the archive explicitly with --home <dir>, or set TAPECTL_HOME=<dir> — a cron, \
             systemd or container invocation normally needs one of the two. Refusing rather \
             than guessing /root/.tapectl, which is not an archive anyone chose."
                .to_string(),
        )
    })
}

fn non_empty_config(config_path: &str) -> Result<&str> {
    if config_path.is_empty() {
        return Err(empty_path_error("--config"));
    }
    Ok(config_path)
}

fn empty_path_error(flag: &str) -> TapectlError {
    TapectlError::Config(format!(
        "{flag} was given an empty path. An empty path resolves relative to the current \
         working directory, so tapectl would create or open an archive wherever it happened \
         to be run from — name the directory explicitly."
    ))
}

/// The "`--config` given without `--home`" notice, as text.
///
/// Issue #228, finding 1: this notice is emitted by `main()` through
/// `eprintln!`, **not** `tracing::warn!`, and that is the whole point of it
/// being a function returning a `String`.
///
/// Wiring `logging.level` to the subscriber (issue #172) is correct and
/// deliberately governs every other `warn!` in the codebase. But the level
/// is read out of the very file `--config` names, so `logging.level =
/// "error"` in a relocated config silenced the notice announcing that that
/// same file had relocated the home — leaving `tapectl --config
/// /mnt/usb/archive/config.toml audit` printing nothing at all. Before
/// issue #172's range the notice was unconditional; a config file must not
/// be able to switch off the message about the resolution it itself caused.
/// The precedent for stepping outside the tracing pipeline for a
/// must-be-seen notice is `cli::key::print_escrow_secret_warning`.
///
/// The trailing `home=<path>` token is the pre-#228 tracing field
/// rendering, kept so the message still names its subject in a form tests
/// can assert on without pinning the prose.
pub fn ambiguous_config_notice(home: &Path) -> String {
    format!(
        "warning: --config given without --home: the tapectl home (database, keys, \
         catalogs, stage reports) is being taken from the config file's parent directory. \
         Pass --home to say that explicitly. home={}",
        home.display()
    )
}

/// Whether log output on stderr carries ANSI colour codes (issue #357).
///
/// Only when stderr is a terminal: a log captured to a file, a pipe, a
/// systemd journal or a transcript otherwise fills with raw escape
/// sequences. And never when `NO_COLOR` is set to a non-empty value
/// (<https://no-color.org>) — tracing-subscriber honours that variable by
/// default, but only until `with_ansi` is called, which overrides it; so
/// this decision has to honour it itself. An empty `NO_COLOR` is "unset",
/// as the convention specifies.
///
/// Pure, like [`resolve_from`]: `main()` passes the real
/// `stderr().is_terminal()` and `var_os("NO_COLOR")`.
pub fn log_ansi(stderr_is_terminal: bool, no_color: Option<&OsStr>) -> bool {
    let no_color_requested = no_color.is_some_and(|value| !value.is_empty());
    stderr_is_terminal && !no_color_requested
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
pub fn peek_logging_config(paths: &TapectlPaths) -> LoggingConfig {
    std::fs::read_to_string(&paths.config_file)
        .ok()
        .and_then(|content| content.parse::<toml::Value>().ok())
        .and_then(|value| value.get("logging").cloned())
        .and_then(|logging_value| toml::to_string(&logging_value).ok())
        .and_then(|logging_str| toml::from_str(&logging_str).ok())
        .unwrap_or_default()
}

/// `[ops] group` from the config file, read leniently like
/// [`peek_logging_config`] (issue #393): `main` needs it before the strict
/// load, because the home's modes are set and the session log is created
/// before that load runs. A file that is unreadable or does not parse, or
/// an `[ops]` table without a string `group`, is `None` — the private
/// default; the strict load then refuses a malformed table by name.
pub fn peek_ops_group(paths: &TapectlPaths) -> Option<String> {
    std::fs::read_to_string(&paths.config_file)
        .ok()
        .and_then(|content| content.parse::<toml::Value>().ok())
        .and_then(|value| value.get("ops")?.get("group")?.as_str().map(str::to_string))
        .filter(|g| !g.trim().is_empty())
}

#[cfg(test)]
mod tests {
    //! Issue #228, finding 3: the input table, as a test.
    //!
    //! Before the extraction these rows were reachable only by spawning the
    //! binary with bespoke environment, which is why `tests/cli_smoke.rs`
    //! covered four of them and why nobody could check that `main`'s
    //! pre-subscriber resolution and `run`'s authoritative one agree — the
    //! check was not expressible. Every row below is now an ordinary,
    //! parallel-safe unit test, because the environment is a parameter.
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStrExt;

    /// A row of the precedence table (`--home`, `--config`, `TAPECTL_HOME`,
    /// `HOME` — exactly [`resolve_from`]'s parameters) plus the two outputs
    /// the whole-table test below pins:
    /// the resolved home, and `ambiguous_config_home` (`None`, or `Some`
    /// naming the same path as the home).
    type ExpectedRow<'a> = (
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a OsStr>,
        Option<&'a OsStr>,
        &'a str,
        Option<&'a str>,
    );

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    /// No flags, no `TAPECTL_HOME`, `HOME` set: `$HOME/.tapectl`.
    #[test]
    fn neither_flag_defaults_under_home() {
        let home = os("/home/op");
        let r = resolve_from(None, None, None, Some(&home), None).expect("should resolve");
        assert_eq!(r.paths.home, PathBuf::from("/home/op/.tapectl"));
        assert_eq!(
            r.paths.config_file,
            PathBuf::from("/home/op/.tapectl/config.toml")
        );
        assert_eq!(
            r.paths.db_file,
            PathBuf::from("/home/op/.tapectl/tapectl.db")
        );
        assert!(r.ambiguous_config_home.is_none());
    }

    /// `--home` alone selects the archive and takes its config from within it.
    #[test]
    fn home_flag_alone_selects_the_archive() {
        let home = os("/home/op");
        let r = resolve_from(Some("/mnt/archive"), None, None, Some(&home), None).expect("resolve");
        assert_eq!(r.paths.home, PathBuf::from("/mnt/archive"));
        assert_eq!(
            r.paths.config_file,
            PathBuf::from("/mnt/archive/config.toml")
        );
        assert!(r.ambiguous_config_home.is_none());
    }

    /// `--config` alone still relocates the whole home (issue #109's
    /// compatibility guarantee) — and says so.
    #[test]
    fn config_flag_alone_relocates_the_home_and_reports_it() {
        let home = os("/home/op");
        let r = resolve_from(
            None,
            Some("/mnt/archive/config.toml"),
            None,
            Some(&home),
            None,
        )
        .expect("resolve");
        assert_eq!(r.paths.home, PathBuf::from("/mnt/archive"));
        assert_eq!(r.paths.db_file, PathBuf::from("/mnt/archive/tapectl.db"));
        assert_eq!(
            r.ambiguous_config_home,
            Some(PathBuf::from("/mnt/archive")),
            "the derived home is the notice's subject"
        );
    }

    /// A `--config` that points nowhere near a tapectl home behaves the
    /// same way — the surprise the notice exists for.
    #[test]
    fn config_flag_outside_any_home_still_derives_from_its_parent() {
        let home = os("/home/op");
        let r = resolve_from(
            None,
            Some("/etc/somewhere/config.toml"),
            None,
            Some(&home),
            None,
        )
        .expect("resolve");
        assert_eq!(r.paths.home, PathBuf::from("/etc/somewhere"));
        assert_eq!(
            r.ambiguous_config_home,
            Some(PathBuf::from("/etc/somewhere"))
        );
    }

    /// Both flags, agreeing: `--home` selects the archive, `--config` the
    /// file within it, and there is nothing ambiguous to announce.
    #[test]
    fn both_flags_agreeing_do_not_warn() {
        let home = os("/home/op");
        let r = resolve_from(
            Some("/mnt/archive"),
            Some("/mnt/archive/config.toml"),
            None,
            Some(&home),
            None,
        )
        .expect("resolve");
        assert_eq!(r.paths.home, PathBuf::from("/mnt/archive"));
        assert_eq!(
            r.paths.config_file,
            PathBuf::from("/mnt/archive/config.toml")
        );
        assert!(r.ambiguous_config_home.is_none());
    }

    /// Both flags, disagreeing: `--home` decides the archive, and
    /// `--config` is taken verbatim — it does NOT drag the home with it.
    #[test]
    fn both_flags_disagreeing_let_home_decide_the_archive() {
        let home = os("/home/op");
        let r = resolve_from(
            Some("/mnt/archive"),
            Some("/etc/elsewhere/other.toml"),
            None,
            Some(&home),
            None,
        )
        .expect("resolve");
        assert_eq!(r.paths.home, PathBuf::from("/mnt/archive"));
        assert_eq!(r.paths.db_file, PathBuf::from("/mnt/archive/tapectl.db"));
        assert_eq!(
            r.paths.config_file,
            PathBuf::from("/etc/elsewhere/other.toml"),
            "--config must be honoured literally when --home is explicit"
        );
        assert!(r.ambiguous_config_home.is_none());
    }

    /// A relative `--config` with a directory component resolves to that
    /// directory, relative to the working directory — unchanged.
    #[test]
    fn relative_config_derives_its_directory() {
        let home = os("/home/op");
        let r = resolve_from(None, Some("sub/dir/config.toml"), None, Some(&home), None)
            .expect("resolve");
        assert_eq!(r.paths.home, PathBuf::from("sub/dir"));
        assert_eq!(r.ambiguous_config_home, Some(PathBuf::from("sub/dir")));
    }

    /// Issue #228, finding 4. `Path::new("config.toml").parent()` is
    /// `Some("")`, so the home used to come out as the empty string: the
    /// notice rendered `home=` with nothing after it, and `secure_path("")`
    /// added a spurious ENOENT permissions warning. `"."` names the same
    /// directory and renders.
    #[test]
    fn bare_relative_config_yields_dot_not_an_empty_home() {
        let home = os("/home/op");
        let r = resolve_from(None, Some("config.toml"), None, Some(&home), None).expect("resolve");
        assert_eq!(r.paths.home, PathBuf::from("."));
        assert_eq!(r.ambiguous_config_home, Some(PathBuf::from(".")));
        assert!(
            !r.paths.home.as_os_str().is_empty(),
            "an empty home is what finding 4 was"
        );
        let notice = ambiguous_config_notice(r.ambiguous_config_home.as_ref().unwrap());
        assert!(notice.ends_with("home=."), "notice was: {notice}");
    }

    /// `--config /` is finding 4's mirror: `Path::new("/").parent()` really
    /// IS `None`, so the `unwrap_or(".")` guard fires and the home becomes
    /// `"."`. Pinned as-is, not changed — `/` is not a config file and
    /// every path from here fails loudly on the read.
    #[test]
    fn config_at_the_filesystem_root_falls_back_to_dot() {
        let home = os("/home/op");
        let r = resolve_from(None, Some("/"), None, Some(&home), None).expect("resolve");
        assert_eq!(r.paths.home, PathBuf::from("."));
        assert_eq!(r.ambiguous_config_home, Some(PathBuf::from(".")));
    }

    /// Pre-existing and deliberately NOT changed (issue #228 flags it, the
    /// task's trap section forbids touching the derivation): with `--config`
    /// alone, `TapectlPaths::new(home)` rebuilds `config_file` as
    /// `<home>/config.toml`, so a non-`config.toml` basename is discarded.
    /// Pinned so the next person meets it as a recorded fact, not a
    /// surprise.
    #[test]
    fn config_alone_discards_a_non_default_basename_pre_existing() {
        let home = os("/home/op");
        let r = resolve_from(
            None,
            Some("/mnt/archive/other.toml"),
            None,
            Some(&home),
            None,
        )
        .expect("resolve");
        assert_eq!(r.paths.home, PathBuf::from("/mnt/archive"));
        assert_eq!(
            r.paths.config_file,
            PathBuf::from("/mnt/archive/config.toml"),
            "pre-existing: the basename is discarded unless --home is also given"
        );
    }

    /// An explicitly empty `--config` would name a file relative to the
    /// working directory and derive a home from it. Refused.
    #[test]
    fn empty_config_flag_is_refused() {
        let home = os("/home/op");
        let err = resolve_from(None, Some(""), None, Some(&home), None).expect_err("must refuse");
        assert!(err.to_string().contains("--config"), "{err}");
    }

    /// Likewise an explicitly empty `--home`, which would create an archive
    /// in the working directory.
    #[test]
    fn empty_home_flag_is_refused() {
        let home = os("/home/op");
        let err = resolve_from(Some(""), None, None, Some(&home), None).expect_err("must refuse");
        assert!(err.to_string().contains("--home"), "{err}");
    }

    /// An empty `--home` is refused even when `--config` could have
    /// answered instead — an explicit flag with a bad value is an error,
    /// not an invitation to pick something else.
    #[test]
    fn empty_home_flag_is_refused_even_with_a_config_flag() {
        let home = os("/home/op");
        let err = resolve_from(
            Some(""),
            Some("/mnt/archive/config.toml"),
            None,
            Some(&home),
            None,
        )
        .expect_err("must refuse");
        assert!(err.to_string().contains("--home"), "{err}");
    }

    /// `TAPECTL_HOME` does what `--home` does, so a shell can export it once.
    #[test]
    fn tapectl_home_env_selects_the_archive() {
        let home = os("/home/op");
        let env = os("/mnt/archive");
        let r = resolve_from(None, None, Some(&env), Some(&home), None).expect("resolve");
        assert_eq!(r.paths.home, PathBuf::from("/mnt/archive"));
        assert!(r.ambiguous_config_home.is_none());
    }

    /// `--home` outranks `TAPECTL_HOME`.
    #[test]
    fn home_flag_outranks_the_env_var() {
        let home = os("/home/op");
        let env = os("/mnt/from-env");
        let r = resolve_from(Some("/mnt/from-flag"), None, Some(&env), Some(&home), None)
            .expect("resolve");
        assert_eq!(r.paths.home, PathBuf::from("/mnt/from-flag"));
    }

    /// Issue #228, finding 2(a). `TAPECTL_HOME=""` is a wrapper's unset
    /// variable, not a request to build an archive in the working
    /// directory. It falls back to `~/.tapectl`.
    #[test]
    fn empty_tapectl_home_env_is_treated_as_unset() {
        let home = os("/home/op");
        let env = os("");
        let r = resolve_from(None, None, Some(&env), Some(&home), None).expect("resolve");
        assert_eq!(
            r.paths.home,
            PathBuf::from("/home/op/.tapectl"),
            "an empty TAPECTL_HOME must not resolve relative to the working directory"
        );
        assert!(r.ambiguous_config_home.is_none());
    }

    /// An empty `TAPECTL_HOME` falls through the whole chain, so `--config`
    /// gets its usual turn.
    #[test]
    fn empty_tapectl_home_env_falls_through_to_the_config_flag() {
        let home = os("/home/op");
        let env = os("");
        let r = resolve_from(
            None,
            Some("/mnt/archive/config.toml"),
            Some(&env),
            Some(&home),
            None,
        )
        .expect("resolve");
        assert_eq!(r.paths.home, PathBuf::from("/mnt/archive"));
        assert_eq!(r.ambiguous_config_home, Some(PathBuf::from("/mnt/archive")));
    }

    /// Issue #228, finding 2(b). The same value as `--home` is a hard clap
    /// error; via the environment it must not silently become the real
    /// `~/.tapectl`.
    #[test]
    fn non_utf8_tapectl_home_env_is_refused() {
        let home = os("/home/op");
        let bad = OsString::from(OsStr::from_bytes(b"/mnt/archive-\xff"));
        let err = resolve_from(None, None, Some(&bad), Some(&home), None).expect_err("must refuse");
        assert!(
            err.to_string().contains("TAPECTL_HOME"),
            "the refusal must name the variable: {err}"
        );
    }

    /// ...but an explicit `--home` never consults the variable at all, so a
    /// broken one in the ambient environment cannot block the flag that
    /// overrides it.
    #[test]
    fn non_utf8_tapectl_home_env_does_not_block_an_explicit_home_flag() {
        let home = os("/home/op");
        let bad = OsString::from(OsStr::from_bytes(b"/mnt/archive-\xff"));
        let r = resolve_from(Some("/mnt/archive"), None, Some(&bad), Some(&home), None)
            .expect("resolve");
        assert_eq!(r.paths.home, PathBuf::from("/mnt/archive"));
    }

    /// Issue #228, finding 2, `src/config.rs` half: `HOME` unset used to
    /// mean `/root/.tapectl` — an archive nobody chose.
    #[test]
    fn unset_home_env_is_refused_rather_than_guessed() {
        let err = resolve_from(None, None, None, None, None).expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("HOME"), "{msg}");
        assert!(
            msg.contains("--home") && msg.contains("TAPECTL_HOME"),
            "the refusal must name both ways to say what was meant: {msg}"
        );
    }

    /// An empty `HOME` is the same situation as an unset one.
    #[test]
    fn empty_home_env_is_refused_rather_than_guessed() {
        let empty = os("");
        let err = resolve_from(None, None, None, Some(&empty), None).expect_err("must refuse");
        assert!(err.to_string().contains("HOME"), "{err}");
    }

    /// The refusal is LAZY: `HOME` is only consulted when it is actually
    /// what the home would come from. These three rows are the cron,
    /// systemd and container invocations the refusal exists to help, and
    /// every one of them must keep working.
    #[test]
    fn unset_home_env_is_harmless_when_the_home_comes_from_elsewhere() {
        let from_flag = resolve_from(Some("/mnt/archive"), None, None, None, None).expect("--home");
        assert_eq!(from_flag.paths.home, PathBuf::from("/mnt/archive"));

        let env = os("/mnt/archive");
        let from_env = resolve_from(None, None, Some(&env), None, None).expect("TAPECTL_HOME");
        assert_eq!(from_env.paths.home, PathBuf::from("/mnt/archive"));

        let from_config = resolve_from(None, Some("/mnt/archive/config.toml"), None, None, None)
            .expect("--config");
        assert_eq!(from_config.paths.home, PathBuf::from("/mnt/archive"));
    }

    /// A non-UTF-8 `HOME` is USED, not refused and not swapped for `/root`:
    /// it names the operator's actual home directory, and a `PathBuf` holds
    /// it exactly. (`std::env::var` could not, which is how `/root` got
    /// substituted before.)
    #[test]
    fn non_utf8_home_env_is_used_as_given() {
        let weird = OsString::from(OsStr::from_bytes(b"/home/op-\xff"));
        let r = resolve_from(None, None, None, Some(&weird), None).expect("resolve");
        assert_eq!(
            r.paths.home,
            PathBuf::from(OsStr::from_bytes(b"/home/op-\xff/.tapectl"))
        );
    }

    /// The notice names the fix and its subject — the two things
    /// `tests/cli_smoke.rs` and the operator both look for.
    #[test]
    fn the_notice_names_both_the_flag_and_the_home() {
        let notice = ambiguous_config_notice(Path::new("/mnt/archive"));
        assert!(notice.contains("--home"), "{notice}");
        assert!(notice.contains("home=/mnt/archive"), "{notice}");
    }

    /// Issue #361: what the relocated home holds is listed with the words
    /// the rest of tapectl uses — stage reports, not "receipts", which now
    /// means only the recipient list a stage set was encrypted to.
    #[test]
    fn the_notice_lists_stage_reports_not_receipts() {
        let notice = ambiguous_config_notice(Path::new("/mnt/archive"));
        assert!(notice.contains("stage reports"), "{notice}");
        assert!(!notice.to_lowercase().contains("receipt"), "{notice}");
    }

    /// **Issue #258.** This test used to call [`resolve_from`] twice with
    /// the identical literal arguments and assert the two results equal
    /// each other -- `f(x) == f(x)` on a function with no interior
    /// mutability, no I/O and no randomness. That cannot fail for any
    /// implementation: a mutation that resolves every row to the wrong
    /// home just as consistently would still pass, because "consistent
    /// with itself" and "correct" are different properties, and only the
    /// first was ever checked. (The doc comment's actual claim --  that
    /// `main`'s pre-subscriber peek and `run`'s authoritative resolution
    /// cannot diverge -- is true, but for a reason no unit test proves: both
    /// call sites in `main.rs` are the identical `startup::resolve(...)`
    /// expression, so there is only ever one implementation to run.)
    ///
    /// Rewritten to pin what "the whole table" should have meant: each
    /// row's ACTUAL resolved `home` and `ambiguous_config_home`, against
    /// the same expected values the single-scenario tests above assert
    /// one precedence rule at a time. A wrong precedence decision now
    /// reddens this test, not just a nondeterministic one.
    #[test]
    fn the_resolution_matches_the_whole_precedence_table() {
        let home = os("/home/op");
        let env = os("/mnt/from-env");
        let empty = os("");
        let rows: &[ExpectedRow<'_>] = &[
            // neither flag: $HOME/.tapectl
            (
                None,
                None,
                None,
                Some(home.as_os_str()),
                "/home/op/.tapectl",
                None,
            ),
            // --home alone
            (
                Some("/mnt/a"),
                None,
                None,
                Some(home.as_os_str()),
                "/mnt/a",
                None,
            ),
            // --config alone relocates the home and is ambiguous about it
            (
                None,
                Some("/mnt/a/config.toml"),
                None,
                Some(home.as_os_str()),
                "/mnt/a",
                Some("/mnt/a"),
            ),
            // both flags, disagreeing: --home decides, unambiguously
            (
                Some("/mnt/a"),
                Some("/mnt/b/other.toml"),
                None,
                Some(home.as_os_str()),
                "/mnt/a",
                None,
            ),
            // bare relative --config: empty parent becomes "."
            (
                None,
                Some("config.toml"),
                None,
                Some(home.as_os_str()),
                ".",
                Some("."),
            ),
            // --config at the filesystem root: no parent, falls back to "."
            (
                None,
                Some("/"),
                None,
                Some(home.as_os_str()),
                ".",
                Some("."),
            ),
            // TAPECTL_HOME selects the archive like --home does
            (
                None,
                None,
                Some(env.as_os_str()),
                Some(home.as_os_str()),
                "/mnt/from-env",
                None,
            ),
            // empty TAPECTL_HOME is treated as unset, falls back to $HOME
            (
                None,
                None,
                Some(empty.as_os_str()),
                Some(home.as_os_str()),
                "/home/op/.tapectl",
                None,
            ),
            // --home alone needs no HOME at all
            (Some("/mnt/a"), None, None, None, "/mnt/a", None),
            // --home outranks TAPECTL_HOME too, not just --config
            (
                Some("/mnt/a"),
                None,
                Some(env.as_os_str()),
                Some(home.as_os_str()),
                "/mnt/a",
                None,
            ),
            // TAPECTL_HOME outranks --config
            (
                None,
                Some("/mnt/b/config.toml"),
                Some(env.as_os_str()),
                Some(home.as_os_str()),
                "/mnt/from-env",
                None,
            ),
        ];
        for (home_flag, config_flag, tapectl_home, home_env, expected_home, expected_ambiguous) in
            rows
        {
            let r = resolve_from(*home_flag, *config_flag, *tapectl_home, *home_env, None)
                .unwrap_or_else(|e| {
                    panic!("row {home_flag:?}/{config_flag:?} should resolve: {e}")
                });
            assert_eq!(
                r.paths.home,
                PathBuf::from(expected_home),
                "row {home_flag:?}/{config_flag:?}/{tapectl_home:?}/{home_env:?} \
                 resolved the wrong home"
            );
            assert_eq!(
                r.ambiguous_config_home,
                expected_ambiguous.map(PathBuf::from),
                "row {home_flag:?}/{config_flag:?} disagreed on ambiguous_config_home"
            );
        }
    }

    // ---- the system home pointer (issue #437) ----
    //
    // Every test names its pointer file inside a temp dir and its euid as a
    // parameter: nothing here reads /etc, sets a variable, or needs root.

    /// A row of the pointer's ranking: `--home`, `--config`, `TAPECTL_HOME`,
    /// `HOME`, the expected home, and what the row demonstrates.
    type PointerRow<'a> = (
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a OsStr>,
        &'a OsStr,
        PathBuf,
        &'a str,
    );

    /// A service home, an account with a `~/.tapectl` of its own, an account
    /// without one, and a way to write the pointer file.
    struct Pointer {
        dir: tempfile::TempDir,
        written: std::cell::Cell<usize>,
    }

    impl Pointer {
        const OPERATOR: u32 = 1000;

        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("svc/.tapectl")).unwrap();
            std::fs::create_dir_all(dir.path().join("dev/.tapectl")).unwrap();
            std::fs::create_dir_all(dir.path().join("op")).unwrap();
            Pointer {
                dir,
                written: std::cell::Cell::new(0),
            }
        }

        /// The service user's tapectl home, which a good pointer names.
        fn svc(&self) -> PathBuf {
            self.dir.path().join("svc/.tapectl")
        }

        /// `HOME` of an account with a `~/.tapectl` already.
        fn home_with_own(&self) -> OsString {
            self.dir.path().join("dev").into_os_string()
        }

        /// `HOME` of an account (the operator) with no `~/.tapectl`.
        fn home_without_own(&self) -> OsString {
            self.dir.path().join("op").into_os_string()
        }

        /// `<HOME>/.tapectl` for the account without one: where a fresh
        /// home would be made, and so what a fall-through would produce.
        fn fresh_default(&self) -> PathBuf {
            self.dir.path().join("op/.tapectl")
        }

        /// Write a pointer file holding `content` and return the input. Each
        /// call writes its own file, so a table of them can be built first.
        fn file(&self, content: impl AsRef<[u8]>, euid: u32) -> HomePointer {
            self.written.set(self.written.get() + 1);
            let path = self
                .dir
                .path()
                .join(format!("pointer-{}", self.written.get()));
            std::fs::write(&path, content).unwrap();
            HomePointer { path, euid }
        }

        /// A pointer whose file was never written.
        fn absent(&self, euid: u32) -> HomePointer {
            HomePointer {
                path: self.dir.path().join("no-such-pointer"),
                euid,
            }
        }

        fn good(&self) -> HomePointer {
            self.file(format!("{}\n", self.svc().display()), Self::OPERATOR)
        }
    }

    /// The whole ranking, one row per source that can compete with the
    /// pointer. The pointer is valid in every row, so each row shows what
    /// outranks it and the last shows it winning when nothing does.
    #[test]
    fn the_pointer_ranks_below_every_other_source() {
        let fx = Pointer::new();
        let ptr = fx.good();
        let env = os("/mnt/from-env");
        let own = fx.home_with_own();
        let none = fx.home_without_own();
        let own_default = PathBuf::from(&own).join(".tapectl");
        let rows: Vec<PointerRow<'_>> = vec![
            (
                Some("/mnt/flag"),
                None,
                None,
                none.as_os_str(),
                PathBuf::from("/mnt/flag"),
                "--home",
            ),
            (
                Some("/mnt/flag"),
                None,
                Some(env.as_os_str()),
                none.as_os_str(),
                PathBuf::from("/mnt/flag"),
                "--home over TAPECTL_HOME",
            ),
            (
                None,
                None,
                Some(env.as_os_str()),
                none.as_os_str(),
                PathBuf::from("/mnt/from-env"),
                "TAPECTL_HOME",
            ),
            (
                None,
                Some("/mnt/cfg/config.toml"),
                None,
                none.as_os_str(),
                PathBuf::from("/mnt/cfg"),
                "--config alone (the gate's isolation)",
            ),
            (
                None,
                None,
                None,
                own.as_os_str(),
                own_default.clone(),
                "an existing ~/.tapectl",
            ),
            (
                None,
                None,
                None,
                none.as_os_str(),
                fx.svc(),
                "nothing else: the pointer",
            ),
        ];
        for (home_flag, config_flag, env_home, home_env, expected, what) in rows {
            let r = resolve_from(home_flag, config_flag, env_home, Some(home_env), Some(&ptr))
                .unwrap_or_else(|e| panic!("{what}: should resolve: {e}"));
            assert_eq!(r.paths.home, expected, "{what}");
        }
    }

    /// Wiring: a pointer resolves like `--home` does — its config and
    /// database are inside it — and is not an ambiguous `--config`.
    #[test]
    fn a_pointer_home_is_an_ordinary_home() {
        let fx = Pointer::new();
        let home = fx.home_without_own();
        let r = resolve_from(None, None, None, Some(&home), Some(&fx.good())).unwrap();
        assert_eq!(r.paths.config_file, fx.svc().join("config.toml"));
        assert_eq!(r.paths.db_file, fx.svc().join("tapectl.db"));
        assert!(r.ambiguous_config_home.is_none());
    }

    /// Everything above the pointer wins even when the pointer is broken:
    /// a bad pointer must not turn a working invocation into an error.
    #[test]
    fn a_broken_pointer_cannot_break_what_outranks_it() {
        let fx = Pointer::new();
        let broken = fx.file("relative/home\n", Pointer::OPERATOR);
        let env = os("/mnt/from-env");
        let none = fx.home_without_own();
        let own = fx.home_with_own();
        let ok = |home_flag, config_flag, env_home, home_env: &OsString| {
            resolve_from(
                home_flag,
                config_flag,
                env_home,
                Some(home_env),
                Some(&broken),
            )
            .expect("outranks the pointer, so the pointer is never read")
        };
        assert_eq!(
            ok(Some("/mnt/flag"), None, None, &none).paths.home,
            PathBuf::from("/mnt/flag")
        );
        assert_eq!(
            ok(None, None, Some(env.as_os_str()), &none).paths.home,
            PathBuf::from("/mnt/from-env")
        );
        assert_eq!(
            ok(None, Some("/mnt/cfg/config.toml"), None, &none)
                .paths
                .home,
            PathBuf::from("/mnt/cfg")
        );
        assert_eq!(
            ok(None, None, None, &own).paths.home,
            PathBuf::from(&own).join(".tapectl")
        );
    }

    /// ADR-0012 2026-10-15 item 62: as root the pointer is ignored — not
    /// read, so not even a broken one is an error — and the home is the one
    /// root would have had without it.
    #[test]
    fn root_ignores_the_pointer_entirely() {
        let fx = Pointer::new();
        let none = fx.home_without_own();
        for (what, ptr) in [
            ("a valid pointer", fx.file(fx.svc().to_str().unwrap(), 0)),
            ("a broken pointer", fx.file("relative\n", 0)),
        ] {
            let r = resolve_from(None, None, None, Some(&none), Some(&ptr))
                .unwrap_or_else(|e| panic!("root with {what} must resolve: {e}"));
            assert_eq!(r.paths.home, fx.fresh_default(), "root with {what}");
        }
        // The same valid pointer for a non-root euid is what root skipped.
        let operator = fx.good();
        let r = resolve_from(None, None, None, Some(&none), Some(&operator)).unwrap();
        assert_eq!(
            r.paths.home,
            fx.svc(),
            "positive control: euid 1000 uses it"
        );
    }

    /// The pointer being switched off (`None`) is the pre-#437 behaviour.
    #[test]
    fn no_pointer_input_is_the_old_resolution() {
        let fx = Pointer::new();
        let none = fx.home_without_own();
        let r = resolve_from(None, None, None, Some(&none), None).unwrap();
        assert_eq!(r.paths.home, fx.fresh_default());
    }

    /// No pointer FILE is "no pointer" — every developer box and every
    /// harness — and resolves exactly as before, a fresh `~/.tapectl`.
    #[test]
    fn an_absent_pointer_file_changes_nothing() {
        let fx = Pointer::new();
        let none = fx.home_without_own();
        let r = resolve_from(
            None,
            None,
            None,
            Some(&none),
            Some(&fx.absent(Pointer::OPERATOR)),
        )
        .unwrap();
        assert_eq!(r.paths.home, fx.fresh_default());
    }

    /// A pointer that is present but wrong stops the command, naming the
    /// file — never a quiet fresh `~/.tapectl`.
    #[test]
    fn a_pointer_that_is_present_but_wrong_is_an_error_naming_it() {
        let fx = Pointer::new();
        let none = fx.home_without_own();
        let a_file = fx.dir.path().join("a-file");
        std::fs::write(&a_file, "x").unwrap();
        let missing = fx.dir.path().join("svc/gone");
        let cases: Vec<(&str, HomePointer, &str)> = vec![
            (
                "relative path",
                fx.file("svc/.tapectl", 1000),
                "not an absolute path",
            ),
            (
                "dot-relative path",
                fx.file("./svc/.tapectl\n", 1000),
                "not an absolute path",
            ),
            ("empty file", fx.file("", 1000), "empty"),
            ("blank file", fx.file("  \n\n", 1000), "empty"),
            (
                "two lines",
                fx.file(format!("{}\n/srv/other\n", fx.svc().display()), 1000),
                "more than one line",
            ),
            (
                "missing target",
                fx.file(missing.to_str().unwrap(), 1000),
                "does not exist",
            ),
            (
                "target is a file",
                fx.file(a_file.to_str().unwrap(), 1000),
                "not a directory",
            ),
            (
                "not UTF-8",
                fx.file(b"/srv/\xff\xfe", 1000),
                "not valid UTF-8",
            ),
            (
                // Readable by no one: a directory in the file's place.
                "unreadable",
                HomePointer {
                    path: fx.dir.path().join("svc"),
                    euid: 1000,
                },
                "cannot be read",
            ),
        ];
        for (what, ptr, reason) in cases {
            let err = resolve_from(None, None, None, Some(&none), Some(&ptr))
                .expect_err(&format!("{what}: must refuse"))
                .to_string();
            assert!(
                err.contains(&ptr.path.display().to_string()),
                "{what}: must name the pointer file: {err}"
            );
            assert!(err.contains(reason), "{what}: want '{reason}': {err}");
            assert!(err.contains("--home"), "{what}: must say how out: {err}");
            assert!(
                !fx.fresh_default().exists(),
                "{what}: resolving must not create a home"
            );
        }
    }

    /// Surrounding blanks and the newline `echo` adds are not part of the
    /// path.
    #[test]
    fn the_pointer_path_is_trimmed() {
        let fx = Pointer::new();
        let none = fx.home_without_own();
        for content in [
            format!("{}", fx.svc().display()),
            format!("{}\n", fx.svc().display()),
            format!("  {}  \r\n", fx.svc().display()),
        ] {
            let ptr = fx.file(content.as_bytes(), 1000);
            let r = resolve_from(None, None, None, Some(&none), Some(&ptr)).unwrap();
            assert_eq!(r.paths.home, fx.svc(), "{content:?}");
        }
    }

    /// A dangling `~/.tapectl` (or one this account cannot stat) is still
    /// "has a `~/.tapectl`": the pointer never redirects it.
    #[test]
    fn a_dangling_own_home_still_outranks_the_pointer() {
        let fx = Pointer::new();
        let dangling = fx.dir.path().join("dangling");
        std::fs::create_dir_all(&dangling).unwrap();
        std::os::unix::fs::symlink(fx.dir.path().join("nowhere"), dangling.join(".tapectl"))
            .unwrap();
        let home = dangling.clone().into_os_string();
        let r = resolve_from(None, None, None, Some(&home), Some(&fx.good())).unwrap();
        assert_eq!(r.paths.home, dangling.join(".tapectl"));
    }

    /// No `HOME` means there is no `~/.tapectl` to find, so the pointer
    /// applies — and with no pointer the old refusal stands.
    #[test]
    fn an_unset_home_uses_the_pointer_or_is_refused_as_before() {
        let fx = Pointer::new();
        let r = resolve_from(None, None, None, None, Some(&fx.good())).unwrap();
        assert_eq!(r.paths.home, fx.svc());
        let err = resolve_from(None, None, None, None, Some(&fx.absent(1000)))
            .expect_err("no HOME and no pointer must still refuse");
        assert!(err.to_string().contains("HOME is not set"), "{err}");
        let root = resolve_from(None, None, None, None, Some(&fx.file("/x", 0)))
            .expect_err("root with no HOME refuses too: the pointer is off");
        assert!(root.to_string().contains("HOME is not set"), "{root}");
    }

    /// `TAPECTL_HOME_POINTER` is read in `resolve`, here as a pure function.
    #[test]
    fn the_pointer_location_defaults_and_is_overridable() {
        assert_eq!(DEFAULT_HOME_POINTER, "/etc/tapectl/home");
        assert_eq!(
            pointer_path_from_env(None),
            PathBuf::from("/etc/tapectl/home")
        );
        assert_eq!(
            pointer_path_from_env(Some(OsStr::new(""))),
            PathBuf::from("/etc/tapectl/home"),
            "an empty variable is an unexpanded one: the default"
        );
        assert_eq!(
            pointer_path_from_env(Some(OsStr::new("/tmp/x/home"))),
            PathBuf::from("/tmp/x/home")
        );
    }

    // ---- the home's source, root, and "not initialized" (ADR-0012 item 213) ----

    /// `--home`, `--config`, `TAPECTL_HOME`, `HOME`, and the source expected.
    type SourceRow<'a> = (
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a OsStr>,
        &'a OsStr,
        HomeSource,
    );

    /// Every rung of the precedence names itself, so `status`, `config
    /// check` and the "not initialized" error can say why this home.
    #[test]
    fn each_rule_names_its_source() {
        let fx = Pointer::new();
        let ptr = fx.good();
        let env = os("/mnt/from-env");
        let own = fx.home_with_own();
        let none = fx.home_without_own();
        let rows: Vec<SourceRow<'_>> = vec![
            (
                Some("/mnt/flag"),
                None,
                None,
                none.as_os_str(),
                HomeSource::HomeFlag,
            ),
            (
                Some("/mnt/flag"),
                None,
                Some(env.as_os_str()),
                none.as_os_str(),
                HomeSource::HomeFlag,
            ),
            (
                None,
                None,
                Some(env.as_os_str()),
                none.as_os_str(),
                HomeSource::TapectlHomeEnv,
            ),
            (
                None,
                Some("/mnt/cfg/config.toml"),
                None,
                none.as_os_str(),
                HomeSource::ConfigFlag,
            ),
            (None, None, None, own.as_os_str(), HomeSource::OwnHome),
            (
                None,
                None,
                None,
                none.as_os_str(),
                HomeSource::Pointer(ptr.path.clone()),
            ),
        ];
        for (home_flag, config_flag, env_home, home_env, want) in rows {
            let r =
                resolve_from(home_flag, config_flag, env_home, Some(home_env), Some(&ptr)).unwrap();
            assert_eq!(r.source, want, "{home_flag:?}/{config_flag:?}/{env_home:?}");
        }
        // No pointer file and no ~/.tapectl: the default, where init makes one.
        let r = resolve_from(None, None, None, Some(&none), Some(&fx.absent(1000))).unwrap();
        assert_eq!(r.source, HomeSource::Default);
        assert_eq!(r.pointer_file, None);
        // The pointer file is recorded whatever chose the home.
        let r = resolve_from(Some("/mnt/flag"), None, None, Some(&none), Some(&ptr)).unwrap();
        assert_eq!(r.pointer_file.as_deref(), Some(ptr.path.as_path()));
    }

    /// The line `status` and `config check` print names the home and why.
    #[test]
    fn the_home_line_names_the_path_and_the_rule() {
        let fx = Pointer::new();
        let ptr = fx.good();
        let none = fx.home_without_own();
        let r = resolve_from(None, None, None, Some(&none), Some(&ptr)).unwrap();
        let line = r.home_line();
        assert!(
            line.starts_with(&format!("home: {} (", fx.svc().display())),
            "{line}"
        );
        assert!(
            line.contains(&format!("the home pointer {}", ptr.path.display())),
            "{line}"
        );
        let env = os("/mnt/e");
        let r = resolve_from(None, None, Some(&env), Some(&none), None).unwrap();
        assert_eq!(r.home_line(), "home: /mnt/e (named by TAPECTL_HOME)");
        let r = resolve_from(Some("/mnt/f"), None, None, Some(&none), None).unwrap();
        assert_eq!(r.home_line(), "home: /mnt/f (named by --home)");
        assert_eq!(r.source.key(), "home_flag");
    }

    /// OQ10: root is refused only with a pointer file present and neither
    /// `--home` nor a non-empty `TAPECTL_HOME`; it names `tapectl-op`.
    #[test]
    fn root_is_refused_beside_a_pointer_unless_the_home_is_named() {
        let fx = Pointer::new();
        let ptr = fx.good().path;
        let absent = fx.absent(0).path;
        let env = os("/mnt/e");
        let empty = os("");
        let refused = root_refusal(0, &ptr, None, None).expect("root beside a pointer");
        assert!(refused.contains("tapectl-op"), "{refused}");
        assert!(refused.contains(&ptr.display().to_string()), "{refused}");
        assert!(
            refused.contains(&fx.svc().display().to_string()),
            "names the home: {refused}"
        );
        assert!(refused.contains("--home"), "{refused}");
        // An empty TAPECTL_HOME is an unset one.
        assert!(root_refusal(0, &ptr, None, Some(&empty)).is_some());
        // Named: not refused.
        assert_eq!(root_refusal(0, &ptr, Some("/srv/h"), None), None);
        assert_eq!(root_refusal(0, &ptr, None, Some(&env)), None);
        // No pointer, or not root: not refused.
        assert_eq!(root_refusal(0, &absent, None, None), None);
        assert_eq!(root_refusal(1000, &ptr, None, None), None);
        // A pointer root cannot parse still refuses (it is not read for the home).
        let broken = fx.file("relative\n", 0).path;
        assert!(root_refusal(0, &broken, None, None).is_some());
    }

    fn not_initialized(
        home_flag: Option<&str>,
        env: Option<&OsStr>,
        home_env: &OsStr,
        pointer: Option<&HomePointer>,
    ) -> String {
        not_initialized_message(
            &resolve_from(home_flag, None, env, Some(home_env), pointer).unwrap(),
        )
    }

    /// Item 213: the error names the path and the rule, and offers `init`
    /// only where there is no pointer and no `TAPECTL_HOME`.
    #[test]
    fn not_initialized_names_the_path_and_rule_and_offers_init_only_for_a_new_archive() {
        let fx = Pointer::new();
        let none = fx.home_without_own();
        let absent = fx.absent(1000);

        // A new archive: the default ~/.tapectl, no pointer.
        let msg = not_initialized(None, None, &none, Some(&absent));
        assert!(msg.contains("not initialized"), "{msg}");
        assert!(
            msg.contains(&fx.fresh_default().display().to_string()),
            "{msg}"
        );
        assert!(msg.contains("nothing named a home"), "{msg}");
        assert!(msg.contains("`tapectl init`"), "{msg}");

        // --home, no pointer: init offered with the same --home.
        let msg = not_initialized(Some("/mnt/new"), None, &none, Some(&absent));
        assert!(msg.contains("--home named it"), "{msg}");
        assert!(msg.contains("`tapectl --home /mnt/new init`"), "{msg}");

        // TAPECTL_HOME: the archive should exist; no init.
        let env = os("/mnt/env-home");
        let msg = not_initialized(None, Some(&env), &none, Some(&absent));
        assert!(msg.contains("/mnt/env-home"), "{msg}");
        assert!(msg.contains("TAPECTL_HOME named it"), "{msg}");
        assert!(!msg.contains("init`"), "{msg}");

        // A home pointer on the host (here it names an empty directory): no init.
        let empty_home = fx.dir.path().join("empty-home");
        std::fs::create_dir_all(&empty_home).unwrap();
        let ptr = fx.file(empty_home.to_str().unwrap(), 1000);
        let msg = not_initialized(None, None, &none, Some(&ptr));
        assert!(msg.contains(&empty_home.display().to_string()), "{msg}");
        assert!(
            msg.contains(&format!("the home pointer {} named it", ptr.path.display())),
            "{msg}"
        );
        assert!(msg.contains("tapectl-op"), "{msg}");
        assert!(!msg.contains("init`"), "{msg}");

        // --home on a host with a pointer: still no init.
        let msg = not_initialized(Some("/mnt/typo"), None, &none, Some(&ptr));
        assert!(msg.contains("--home named it"), "{msg}");
        assert!(!msg.contains("init`"), "{msg}");

        // An empty ~/.tapectl beside a pointer: it outranks the pointer, said so.
        let own = fx.home_with_own();
        let msg = not_initialized(None, None, &own, Some(&ptr));
        assert!(msg.contains("outranks the home pointer"), "{msg}");
        assert!(!msg.contains("init`"), "{msg}");
    }

    // ---- peek_logging_config ----

    /// No config file at all (a fresh machine, pre-`init`) must not be an
    /// error — it is the bootstrap case.
    #[test]
    fn peeking_a_missing_config_yields_the_defaults() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().join("nonexistent"));
        let logging = peek_logging_config(&paths);
        assert_eq!(logging.level, LoggingConfig::default().level);
        assert_eq!(logging.format, LoggingConfig::default().format);
    }

    /// The peek's actual job: a `[logging]` table governs the subscriber.
    #[test]
    fn peeking_reads_the_logging_table() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().to_path_buf());
        std::fs::write(
            &paths.config_file,
            "[logging]\nlevel = \"error\"\nformat = \"json\"\n",
        )
        .unwrap();
        let logging = peek_logging_config(&paths);
        assert_eq!(logging.level, "error");
        assert_eq!(logging.format, "json");
    }

    /// Unparseable TOML defaults rather than failing: the authoritative
    /// error still comes from `Config::load`, which must be allowed to be
    /// the one that reports it.
    #[test]
    fn peeking_unparseable_toml_yields_the_defaults() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().to_path_buf());
        std::fs::write(&paths.config_file, "this is not [ valid toml").unwrap();
        assert_eq!(
            peek_logging_config(&paths).level,
            LoggingConfig::default().level
        );
    }

    /// Issue #357: colour only on a terminal, and never under a non-empty
    /// `NO_COLOR`. The whole table, since each input is a parameter.
    #[test]
    fn log_colour_follows_the_terminal_and_no_color() {
        let set = OsStr::new("1");
        let empty = OsStr::new("");
        let rows: [(bool, Option<&OsStr>, bool); 6] = [
            (true, None, true),
            (true, Some(empty), true),
            (true, Some(set), false),
            (false, None, false),
            (false, Some(empty), false),
            (false, Some(set), false),
        ];
        for (tty, no_color, want) in rows {
            assert_eq!(
                log_ansi(tty, no_color),
                want,
                "stderr_is_terminal={tty}, NO_COLOR={no_color:?}"
            );
        }
    }
}
