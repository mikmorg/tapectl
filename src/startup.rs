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
//! `--home` > `TAPECTL_HOME` > `--config`'s parent directory > `$HOME/.tapectl`.
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
//! `backend add` (ADR-0005's DR path), so leniency is not a defect in
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
    /// explicitly (`--home`/`TAPECTL_HOME`) or defaulted, because neither
    /// is surprising and a notice on the invocation that got it right is
    /// just noise.
    pub ambiguous_config_home: Option<PathBuf>,
}

/// Resolve against the real process environment. The only impure function
/// in this module; everything it does is [`resolve_from`].
pub fn resolve(home_flag: Option<&str>, config_flag: Option<&str>) -> Result<Startup> {
    let tapectl_home = std::env::var_os("TAPECTL_HOME");
    let home_env = std::env::var_os("HOME");
    resolve_from(
        home_flag,
        config_flag,
        tapectl_home.as_deref(),
        home_env.as_deref(),
    )
}

/// The resolution itself — a pure function of exactly four inputs, which is
/// what lets the input table be a unit test rather than a subprocess.
///
/// `tapectl_home_env` and `home_env` are `OsStr`, not `str`, on purpose:
/// the difference between "unset", "set to the empty string" and "set to
/// something that is not UTF-8" is precisely where two of issue #228's
/// defects lived, and `std::env::var(..).ok()` collapses all three into
/// `None`-or-`Some`.
pub fn resolve_from(
    home_flag: Option<&str>,
    config_flag: Option<&str>,
    tapectl_home_env: Option<&OsStr>,
    home_env: Option<&OsStr>,
) -> Result<Startup> {
    // --home wins outright, and is checked BEFORE the environment: an
    // explicit flag must not be blocked by a broken `TAPECTL_HOME` it
    // overrides anyway.
    let named_home: Option<PathBuf> = match home_flag {
        Some("") => return Err(empty_path_error("--home")),
        Some(home) => Some(PathBuf::from(home)),
        None => tapectl_home_from_env(tapectl_home_env)?,
    };

    if let Some(home) = named_home {
        let mut paths = TapectlPaths::new(home);
        if let Some(config_path) = config_flag {
            // Both given: --home selects the archive, --config selects the
            // file within it. Unambiguous, so no notice.
            paths.config_file = PathBuf::from(non_empty_config(config_path)?);
        }
        return Ok(Startup {
            paths,
            ambiguous_config_home: None,
        });
    }

    if let Some(config_path) = config_flag {
        let home = config_home(non_empty_config(config_path)?);
        return Ok(Startup {
            paths: TapectlPaths::new(home.clone()),
            ambiguous_config_home: Some(home),
        });
    }

    Ok(Startup {
        paths: TapectlPaths::new(default_home(home_env)?),
        ambiguous_config_home: None,
    })
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
         catalogs, receipts) is being taken from the config file's parent directory. \
         Pass --home to say that explicitly. home={}",
        home.display()
    )
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
        let r = resolve_from(None, None, None, Some(&home)).expect("should resolve");
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
        let r = resolve_from(Some("/mnt/archive"), None, None, Some(&home)).expect("resolve");
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
        let r = resolve_from(None, Some("/mnt/archive/config.toml"), None, Some(&home))
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
        let r = resolve_from(None, Some("/etc/somewhere/config.toml"), None, Some(&home))
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
        let r =
            resolve_from(None, Some("sub/dir/config.toml"), None, Some(&home)).expect("resolve");
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
        let r = resolve_from(None, Some("config.toml"), None, Some(&home)).expect("resolve");
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
        let r = resolve_from(None, Some("/"), None, Some(&home)).expect("resolve");
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
        let r = resolve_from(None, Some("/mnt/archive/other.toml"), None, Some(&home))
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
        let err = resolve_from(None, Some(""), None, Some(&home)).expect_err("must refuse");
        assert!(err.to_string().contains("--config"), "{err}");
    }

    /// Likewise an explicitly empty `--home`, which would create an archive
    /// in the working directory.
    #[test]
    fn empty_home_flag_is_refused() {
        let home = os("/home/op");
        let err = resolve_from(Some(""), None, None, Some(&home)).expect_err("must refuse");
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
        )
        .expect_err("must refuse");
        assert!(err.to_string().contains("--home"), "{err}");
    }

    /// `TAPECTL_HOME` does what `--home` does, so a shell can export it once.
    #[test]
    fn tapectl_home_env_selects_the_archive() {
        let home = os("/home/op");
        let env = os("/mnt/archive");
        let r = resolve_from(None, None, Some(&env), Some(&home)).expect("resolve");
        assert_eq!(r.paths.home, PathBuf::from("/mnt/archive"));
        assert!(r.ambiguous_config_home.is_none());
    }

    /// `--home` outranks `TAPECTL_HOME`.
    #[test]
    fn home_flag_outranks_the_env_var() {
        let home = os("/home/op");
        let env = os("/mnt/from-env");
        let r =
            resolve_from(Some("/mnt/from-flag"), None, Some(&env), Some(&home)).expect("resolve");
        assert_eq!(r.paths.home, PathBuf::from("/mnt/from-flag"));
    }

    /// Issue #228, finding 2(a). `TAPECTL_HOME=""` is a wrapper's unset
    /// variable, not a request to build an archive in the working
    /// directory. It falls back to `~/.tapectl`.
    #[test]
    fn empty_tapectl_home_env_is_treated_as_unset() {
        let home = os("/home/op");
        let env = os("");
        let r = resolve_from(None, None, Some(&env), Some(&home)).expect("resolve");
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
        let err = resolve_from(None, None, Some(&bad), Some(&home)).expect_err("must refuse");
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
        let r = resolve_from(Some("/mnt/archive"), None, Some(&bad), Some(&home)).expect("resolve");
        assert_eq!(r.paths.home, PathBuf::from("/mnt/archive"));
    }

    /// Issue #228, finding 2, `src/config.rs` half: `HOME` unset used to
    /// mean `/root/.tapectl` — an archive nobody chose.
    #[test]
    fn unset_home_env_is_refused_rather_than_guessed() {
        let err = resolve_from(None, None, None, None).expect_err("must refuse");
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
        let err = resolve_from(None, None, None, Some(&empty)).expect_err("must refuse");
        assert!(err.to_string().contains("HOME"), "{err}");
    }

    /// The refusal is LAZY: `HOME` is only consulted when it is actually
    /// what the home would come from. These three rows are the cron,
    /// systemd and container invocations the refusal exists to help, and
    /// every one of them must keep working.
    #[test]
    fn unset_home_env_is_harmless_when_the_home_comes_from_elsewhere() {
        let from_flag = resolve_from(Some("/mnt/archive"), None, None, None).expect("--home");
        assert_eq!(from_flag.paths.home, PathBuf::from("/mnt/archive"));

        let env = os("/mnt/archive");
        let from_env = resolve_from(None, None, Some(&env), None).expect("TAPECTL_HOME");
        assert_eq!(from_env.paths.home, PathBuf::from("/mnt/archive"));

        let from_config =
            resolve_from(None, Some("/mnt/archive/config.toml"), None, None).expect("--config");
        assert_eq!(from_config.paths.home, PathBuf::from("/mnt/archive"));
    }

    /// A non-UTF-8 `HOME` is USED, not refused and not swapped for `/root`:
    /// it names the operator's actual home directory, and a `PathBuf` holds
    /// it exactly. (`std::env::var` could not, which is how `/root` got
    /// substituted before.)
    #[test]
    fn non_utf8_home_env_is_used_as_given() {
        let weird = OsString::from(OsStr::from_bytes(b"/home/op-\xff"));
        let r = resolve_from(None, None, None, Some(&weird)).expect("resolve");
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
        ];
        for (home_flag, config_flag, tapectl_home, home_env, expected_home, expected_ambiguous) in
            rows
        {
            let r = resolve_from(*home_flag, *config_flag, *tapectl_home, *home_env)
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
}
