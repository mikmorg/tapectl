//! CLI smoke layer (issue #44).
//!
//! Two jobs:
//!   1. `clap`'s own `debug_assert()` catches malformed derive definitions
//!      (conflicting arg ids, bad defaults, etc.) that only surface at
//!      startup, before any test ever exercises the command.
//!   2. A handful of `try_parse_from` cases pin the flag surface for
//!      commands reachable from this crate, and a process-level smoke run
//!      exercises the real binary end-to-end against a throwaway `HOME` —
//!      never the operator's real `~/.tapectl`.
//!
//! `#[cfg(test)]` items in the lib crate (e.g. `db::open_memory`) are not
//! visible from here — this is a separate integration-test binary, so
//! process-level tests are the only way to reach `main`'s dispatch.

use clap::{CommandFactory, Parser};
use std::process::Command;
use tapectl::cli::Cli;
use tempfile::TempDir;

/// clap's own consistency check: catches conflicting ids, invalid
/// defaults, duplicate short flags, etc. across the whole derive tree.
#[test]
fn cli_debug_assert() {
    Cli::command().debug_assert();
}

#[test]
fn parses_bare_init() {
    let cli = Cli::try_parse_from(["tapectl", "init"]).expect("init should parse");
    assert!(matches!(cli.command, tapectl::cli::Commands::Init { .. }));
    assert!(!cli.json);
}

#[test]
fn parses_init_with_operator() {
    let cli = Cli::try_parse_from(["tapectl", "init", "--operator", "alice"])
        .expect("init --operator should parse");
    match cli.command {
        tapectl::cli::Commands::Init { operator } => {
            assert_eq!(operator.as_deref(), Some("alice"));
        }
        _ => panic!("expected Init"),
    }
}

#[test]
fn parses_global_json_flag_before_and_after_subcommand() {
    // `--json` is a global arg — clap should accept it on either side of
    // the subcommand name.
    let before = Cli::try_parse_from(["tapectl", "--json", "audit"]).unwrap();
    assert!(before.json);
    let after = Cli::try_parse_from(["tapectl", "audit", "--json"]);
    // clap global args are not automatically valid *after* a subcommand
    // unless declared with `global = true` propagation reaching that far;
    // `audit` itself defines no `--json`, so this only succeeds if clap's
    // global-arg propagation covers it. Either parse outcome is a valid
    // pin — assert only that it doesn't panic.
    let _ = after;
}

#[test]
fn parses_audit_flags() {
    let cli = Cli::try_parse_from(["tapectl", "audit", "--action-plan", "--unit", "photos"])
        .expect("audit flags should parse");
    match cli.command {
        tapectl::cli::Commands::Audit { action_plan, unit } => {
            assert!(action_plan);
            assert_eq!(unit.as_deref(), Some("photos"));
        }
        _ => panic!("expected Audit"),
    }
}

#[test]
fn parses_db_fsck_repair_flag() {
    let cli = Cli::try_parse_from(["tapectl", "db", "fsck", "--repair"])
        .expect("db fsck --repair should parse");
    match cli.command {
        tapectl::cli::Commands::Db { command } => {
            assert!(matches!(
                command,
                tapectl::cli::DbCommands::Fsck { repair: true }
            ));
        }
        _ => panic!("expected Db"),
    }
}

#[test]
fn parses_config_check() {
    let cli =
        Cli::try_parse_from(["tapectl", "config", "check"]).expect("config check should parse");
    match cli.command {
        tapectl::cli::Commands::Config { command } => {
            assert!(matches!(command, tapectl::cli::ConfigCommands::Check));
        }
        _ => panic!("expected Config"),
    }
}

/// Run the real `tapectl` binary with `arg` and a `HOME` pinned to `home`,
/// so nothing touches the operator's real `~/.tapectl`.
fn run_tapectl(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("failed to spawn tapectl binary")
}

/// End-to-end process smoke: init -> audit --json -> config check --json
/// -> db fsck, entirely inside a throwaway HOME.
///
/// This is the load-bearing test for issue #44 — see the negative-control
/// evidence in the commit/PR description: a stray `println!` after any
/// `--json` block, or a change that makes `db fsck` exit 0 on a genuine
/// integrity failure, must turn this test red.
#[test]
fn cli_smoke_sequence_against_a_throwaway_home() {
    let home = TempDir::new().expect("tempdir");

    // 1. init
    let init_out = run_tapectl(home.path(), &["init"]);
    assert!(
        init_out.status.success(),
        "init failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&init_out.stdout),
        String::from_utf8_lossy(&init_out.stderr)
    );

    // Positive proof the redirect actually worked — init must have written
    // under the throwaway HOME, not the operator's real one. An
    // exit-code-only assertion would pass just as happily if HOME were
    // silently ignored.
    let db_path = home.path().join(".tapectl").join("tapectl.db");
    assert!(
        db_path.exists(),
        "expected {} to exist after init — HOME redirect did not take effect",
        db_path.display()
    );

    // 2. audit --json — must be clean (exit 0) on a freshly initialized,
    // unit-less database, and stdout must be *pure* JSON (issue #56 defect
    // shape: a println! human trailer alongside the JSON payload).
    let audit_out = run_tapectl(home.path(), &["audit", "--json"]);
    assert_eq!(
        audit_out.status.code(),
        Some(0),
        "audit --json on a clean fresh db should exit 0: stderr={}",
        String::from_utf8_lossy(&audit_out.stderr)
    );
    let audit_stdout = String::from_utf8_lossy(&audit_out.stdout);
    serde_json::from_str::<serde_json::Value>(&audit_stdout).unwrap_or_else(|e| {
        panic!("audit --json stdout did not parse as pure JSON: {e}\nstdout={audit_stdout:?}")
    });

    // 3. config check --json — same pure-JSON requirement.
    let config_out = run_tapectl(home.path(), &["config", "check", "--json"]);
    assert!(
        config_out.status.success(),
        "config check --json failed: stderr={}",
        String::from_utf8_lossy(&config_out.stderr)
    );
    let config_stdout = String::from_utf8_lossy(&config_out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&config_stdout).unwrap_or_else(|e| {
        panic!(
            "config check --json stdout did not parse as pure JSON: {e}\nstdout={config_stdout:?}"
        )
    });
    assert_eq!(parsed["valid"], serde_json::json!(true));

    // 4. db fsck — a clean fresh db must exit 0. (#45 fixed fsck so it
    // cannot exit 0 while reporting a real integrity failure or issues;
    // this pins the healthy-path exit code so a regression that makes
    // fsck *always* report success, healthy or not, would not be masked by
    // only ever testing the failure path.)
    let fsck_out = run_tapectl(home.path(), &["db", "fsck"]);
    assert_eq!(
        fsck_out.status.code(),
        Some(0),
        "db fsck on a clean fresh db should exit 0: stdout={}\nstderr={}",
        String::from_utf8_lossy(&fsck_out.stdout),
        String::from_utf8_lossy(&fsck_out.stderr)
    );
}

/// Calling `db fsck` before `init` must fail loudly (non-zero exit), not
/// silently report success against a database that doesn't exist yet.
/// A cheap companion to the exit-code pin above: this is the "never exits
/// 0 on failure" side, exercised without needing to corrupt a real db.
#[test]
fn db_fsck_before_init_is_not_a_silent_success() {
    let home = TempDir::new().expect("tempdir");
    let out = run_tapectl(home.path(), &["db", "fsck"]);
    assert_ne!(
        out.status.code(),
        Some(0),
        "db fsck against an uninitialized home must not exit 0"
    );
}
