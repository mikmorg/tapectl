//! The spellings 1.9.0 removed are refused by name (ADR-0012 items 203 and
//! 209), through the real binary.
//!
//! Every row of the one table (`tapectl::cli::renamed::RENAMED`) is typed
//! on a HOME that holds nothing, and must exit 2 with "`tapectl <old>` was
//! renamed in 1.9.0: use `tapectl <new>`" on stderr, and leave the HOME
//! exactly as empty as it was: the refusal comes before any home is
//! resolved, so it reads nothing and creates nothing. A removed command is
//! typed four ways (OQ4): as a command, with `--help`, through `help`, and
//! as a bare old group where it is one; clap would otherwise print a hidden
//! command's help and exit 0. Each new spelling is the positive control:
//! typed on the same empty HOME it is NOT refused as a rename.

use std::path::Path;
use std::process::{Command, Output};

use tapectl::cli::renamed::{Renamed, RENAMED};
use tempfile::TempDir;

fn tapectl(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(args)
        .env("HOME", home)
        .env_remove("TAPECTL_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("failed to spawn tapectl")
}

/// The refusal `r` must give for `args`, or what was wrong.
fn refused_by_name(home: &Path, r: &Renamed, args: &[&str]) -> Option<String> {
    let out = tapectl(home, args);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let want = format!(
        "`tapectl {}` was renamed in 1.9.0: use `tapectl {}`",
        r.old(),
        r.new
    );
    if out.status.code() == Some(2) && stderr.contains(&want) && out.stdout.is_empty() {
        None
    } else {
        Some(format!(
            "`tapectl {}` exited {:?}; want exit 2 and {want:?} on stderr, nothing on stdout\n\
             stdout={}\nstderr={stderr}",
            args.join(" "),
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
        ))
    }
}

/// The ways a row is typed: a removed command as itself, with `--help`
/// and through `help`; a removed flag after its command, both as `--flag
/// VALUE` and `--flag=VALUE`, and with `--help` after it.
fn typings(r: &Renamed) -> Vec<Vec<String>> {
    let path: Vec<String> = r.path.iter().map(|s| s.to_string()).collect();
    let with = |extra: &[&str]| -> Vec<String> {
        let mut v = path.clone();
        v.extend(extra.iter().map(|s| s.to_string()));
        v
    };
    match r.flag {
        None => {
            let mut help = vec!["help".to_string()];
            help.extend(path.clone());
            vec![
                with(&[]),
                with(&["--dry-run"]),
                with(&["--help"]),
                with(&["-h"]),
                help,
                {
                    let mut v = vec!["--json".to_string()];
                    v.extend(path.clone());
                    v
                },
            ]
        }
        Some(flag) => vec![
            with(&[flag, "X"]),
            with(&[&format!("{flag}=X")]),
            with(&[flag, "X", "--help"]),
            {
                let mut v = vec!["--home".to_string(), "/nonexistent/tapectl-renamed".into()];
                v.extend(with(&[flag, "X"]));
                v
            },
        ],
    }
}

#[test]
fn every_removed_spelling_is_refused_by_name_on_an_empty_home() {
    let home = TempDir::new().unwrap();
    let mut failures = Vec::new();
    let mut typed = 0;
    for r in RENAMED {
        for args in typings(r) {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            typed += 1;
            if let Some(f) = refused_by_name(home.path(), r, &args) {
                failures.push(f);
            }
        }
    }
    let left: Vec<_> = std::fs::read_dir(home.path()).unwrap().collect();
    assert!(left.is_empty(), "a refusal created {left:?}");
    assert!(
        !Path::new("/nonexistent/tapectl-renamed").exists(),
        "a refusal created the --home it was given"
    );
    assert!(typed >= RENAMED.len() * 4, "only {typed} typings were run");
    assert!(
        failures.is_empty(),
        "{} of {typed} typing(s) were not refused by name:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// The table names the renames the ADR rules (item 209) and the positional
/// arguments of item 210, each once: a row lost or added by mistake shows
/// here, not only as a missing refusal.
#[test]
fn the_table_is_the_rulings() {
    let mut rows: Vec<(String, &str)> = RENAMED.iter().map(|r| (r.old(), r.new)).collect();
    rows.sort();
    let mut want = vec![
        ("backend".to_string(), "drive"),
        ("backend add".to_string(), "drive add"),
        ("catalog ls".to_string(), "catalog list"),
        (
            "collection run --collection".to_string(),
            "collection run NAME",
        ),
        ("db backup --to".to_string(), "db backup PATH"),
        ("export".to_string(), "unit export UNIT --to DIR"),
        ("host check --unit".to_string(), "host check --systemd-unit"),
        ("import".to_string(), "volume import LABEL"),
        ("key escrow-kit --out".to_string(), "key escrow-kit DIR"),
        ("report pending".to_string(), "stage list --owing"),
        ("report summary".to_string(), "status"),
        ("report supersedable".to_string(), "report reclaimable"),
        ("staging".to_string(), "stage"),
        ("staging clean".to_string(), "stage clean"),
        ("staging status".to_string(), "stage list"),
        ("unit status".to_string(), "unit info"),
        (
            "volume compact-write --destination".to_string(),
            "volume compact-write --to",
        ),
        (
            "volume read-slices --unit".to_string(),
            "volume read-slices UNIT --from LABEL",
        ),
    ];
    want.sort();
    assert_eq!(rows, want);
}

/// The positive control: each new spelling, on the same empty HOME, is not
/// refused as a rename. It fails for want of an initialised home (or runs,
/// for the commands that need none), but never with the rename's words.
#[test]
fn every_new_spelling_runs_as_itself() {
    let home = TempDir::new().unwrap();
    for args in [
        &["drive", "add", "--help"][..],
        &["stage", "clean", "--help"],
        &["stage", "list", "--help"],
        &["stage", "list", "--owing", "--all", "--json"],
        &["unit", "info", "u"],
        &["catalog", "list", "u"],
        &["report", "reclaimable"],
        &["unit", "export", "u", "--to", "/nonexistent/d"],
        &["volume", "import", "L", "--generation", "LTO-6"],
        &["volume", "compact-write", "--to", "L"],
        &["volume", "read-slices", "u", "--from", "L"],
        &["host", "check", "--systemd-unit", "x.timer", "--json"],
        &["db", "backup", "/nonexistent/t.db"],
        &["key", "escrow-kit", "/nonexistent/kit"],
        &["collection", "run", "c", "--label", "L"],
        &["status"],
        &["help", "volume", "compact-write"],
    ] {
        let out = tapectl(home.path(), args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("was renamed in"),
            "`tapectl {}` was refused as a rename: {stderr}",
            args.join(" ")
        );
        assert_ne!(
            out.status.code(),
            None,
            "`tapectl {}` died on a signal",
            args.join(" ")
        );
    }
}

/// `-n` is `--dry-run` everywhere, including on the subcommands that
/// declare their own `dry_run` (`collection sync`, `restore unit`, `restore
/// volume`), where clap would not carry the global's short form over.
#[test]
fn dash_n_is_dry_run_on_every_command_that_declares_its_own() {
    use clap::Parser;
    use tapectl::cli::{Cli, Commands};
    for argv in [
        &["tapectl", "-n", "collection", "sync"][..],
        &["tapectl", "collection", "sync", "-n"],
        &[
            "tapectl", "restore", "unit", "u", "--from", "L", "--to", "/d", "-n",
        ],
        &["tapectl", "restore", "volume", "L", "--to", "/d", "-n"],
        &["tapectl", "stage", "clean", "-n"],
        &["tapectl", "-n", "volume", "plan"],
    ] {
        let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        let own = match &cli.command {
            Commands::Collection {
                command: tapectl::cli::collection::CollectionCommands::Sync { dry_run },
            } => Some(*dry_run),
            Commands::Restore {
                command:
                    tapectl::cli::restore::RestoreCommands::Unit { dry_run, .. }
                    | tapectl::cli::restore::RestoreCommands::Volume { dry_run, .. },
            } => Some(*dry_run),
            _ => None,
        };
        assert!(
            cli.dry_run || own == Some(true),
            "{argv:?}: -n did not set --dry-run"
        );
    }
}
