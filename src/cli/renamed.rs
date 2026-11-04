//! The spellings 1.9.0 removed, and the one refusal that names each one's
//! replacement (ADR-0012 items 203 and 209).
//!
//! A renamed command or flag loses its old spelling at once: typed, the old
//! spelling is refused by name, with the new spelling and the release that
//! renamed it, and exits 2, as a stale config key is refused by name.
//! [`RENAMED`] is the one table; nothing else spells an old name.
//!
//! The refusal is reached however the old spelling is typed: as a command,
//! with `--help`, through `help` (as in `help staging clean`), or as a bare
//! old group. `main` asks [`find`] about the raw command line before clap
//! parses it and before any home is resolved, so a refusal reads nothing
//! and creates nothing, on any home or none. clap would otherwise print the
//! help of a hidden command and exit 0.
//!
//! The removed commands stay in the clap tree as hidden stubs that take any
//! arguments ([`Removed`]), so the tree itself knows every old command
//! spelling: a stub that is somehow parsed is refused through [`parsed`]
//! with the same words, the tests can tell a stub from a live command, and
//! nothing generated from the tree (man pages, the command reference,
//! completions) shows one. A removed FLAG has no stub: it is not in the
//! tree at all, and [`find`] alone refuses it.
//!
//! Never in this table: the three commands every tape's File 1 prints
//! (`init --operator … --escrow-public-key …`, `catalog rebuild
//! --from-volume --device … --key …`, `restore unit --unit … --device … --to
//! … --key …`), which work for good.

use std::ffi::OsString;

use crate::error::TapectlError;

/// One spelling 1.9.0 removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Renamed {
    /// The old spelling's command words, e.g. `["staging", "clean"]`, or the
    /// command a removed flag belonged to.
    pub path: &'static [&'static str],
    /// The removed flag (`--destination`), when the flag is what was
    /// renamed and the command stays; `None` for a removed command.
    pub flag: Option<&'static str>,
    /// The new spelling, as the refusal names it after `tapectl `.
    pub new: &'static str,
}

impl Renamed {
    /// The old spelling as typed, after `tapectl `.
    pub fn old(&self) -> String {
        let mut old = self.path.join(" ");
        if let Some(flag) = self.flag {
            old.push(' ');
            old.push_str(flag);
        }
        old
    }

    /// The refusal's words (exit 2).
    pub fn message(&self) -> String {
        format!(
            "`tapectl {}` was renamed in {RENAMED_IN}: use `tapectl {}`",
            self.old(),
            self.new
        )
    }

    /// The refusal as an error, for `main` to print and exit 2 on.
    pub fn refusal(&self) -> TapectlError {
        TapectlError::Other(self.message())
    }
}

/// The release that renamed every spelling in [`RENAMED`].
pub const RENAMED_IN: &str = "1.9.0";

/// Every removed spelling. Groups come before their commands; the longest
/// matching command wins.
pub const RENAMED: &[Renamed] = &[
    cmd(&["backend"], "drive"),
    cmd(&["backend", "add"], "drive add"),
    cmd(&["staging"], "stage"),
    cmd(&["staging", "clean"], "stage clean"),
    cmd(&["staging", "status"], "stage list"),
    cmd(&["report", "pending"], "stage list --owing"),
    cmd(&["report", "summary"], "status"),
    cmd(&["report", "supersedable"], "report reclaimable"),
    cmd(&["unit", "status"], "unit info"),
    cmd(&["catalog", "ls"], "catalog list"),
    cmd(&["export"], "unit export UNIT --to DIR"),
    cmd(&["import"], "volume import LABEL"),
    flag(
        &["volume", "compact-write"],
        "--destination",
        "volume compact-write --to",
    ),
    flag(
        &["volume", "read-slices"],
        "--unit",
        "volume read-slices UNIT --from LABEL",
    ),
    flag(&["host", "check"], "--unit", "host check --systemd-unit"),
    flag(&["db", "backup"], "--to", "db backup PATH"),
    flag(&["key", "escrow-kit"], "--out", "key escrow-kit DIR"),
    flag(
        &["collection", "run"],
        "--collection",
        "collection run NAME",
    ),
];

const fn cmd(path: &'static [&'static str], new: &'static str) -> Renamed {
    Renamed {
        path,
        flag: None,
        new,
    }
}

const fn flag(path: &'static [&'static str], flag: &'static str, new: &'static str) -> Renamed {
    Renamed {
        path,
        flag: Some(flag),
        new,
    }
}

/// The removed command whose words are exactly `path`.
pub fn command(path: &[&str]) -> Option<&'static Renamed> {
    RENAMED.iter().find(|r| r.flag.is_none() && r.path == path)
}

/// The refusal of the removed command `path`, for a stub's own dispatch arm
/// (never reached through `main`, which refuses first).
pub fn refused(path: &[&str]) -> TapectlError {
    match command(path) {
        Some(r) => r.refusal(),
        None => TapectlError::Other(format!(
            "`tapectl {}` is not a command of this release",
            path.join(" ")
        )),
    }
}

/// The arguments of a removed command's stub: anything at all, so the
/// refusal names the new spelling rather than clap reporting a usage error
/// against a command that no longer exists.
#[derive(clap::Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct Removed {
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        hide = true,
        num_args = 0..
    )]
    pub rest: Vec<String>,
}

/// The global flags that take a value (`--home DIR`), so [`find`] reads
/// their value as a value and not as a command word. A unit test holds this
/// to the clap definition.
const GLOBAL_VALUE_FLAGS: &[&str] = &["--config", "--home"];

/// The removed spelling `args` (argv, the program name first) types, if it
/// types one: as a command (`staging clean …`), a bare old group
/// (`staging`), with `--help` anywhere, through `help` (`help staging
/// clean`), or as a removed flag of a command that stays (`volume
/// compact-write --destination L`, `--destination=L`). Global flags may sit
/// anywhere among the command words. Nothing after `--` is read.
pub fn find(args: &[OsString]) -> Option<&'static Renamed> {
    let tokens: Vec<String> = args
        .iter()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    // The command words, as clap reads the subcommand chain: each word kept
    // while the words so far begin some row's path.
    let mut words: Vec<&str> = Vec::new();
    let mut after_words = 0;
    let mut help = false;
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        if t == "--" {
            break;
        }
        if t.starts_with('-') && t.len() > 1 {
            i += if GLOBAL_VALUE_FLAGS.contains(&t) {
                2
            } else {
                1
            };
            continue;
        }
        if words.is_empty() && !help && t == "help" {
            help = true;
            i += 1;
            after_words = i;
            continue;
        }
        let begins = RENAMED.iter().any(|r| {
            r.path.len() > words.len()
                && r.path[..words.len()] == words[..]
                && r.path[words.len()] == t
        });
        if !begins {
            break;
        }
        words.push(t);
        i += 1;
        after_words = i;
    }

    if let Some(r) = (1..=words.len()).rev().find_map(|n| command(&words[..n])) {
        return Some(r);
    }
    if help {
        // `help volume compact-write` asks for the help of a command that
        // stays; only a removed command's help is refused.
        return None;
    }
    let rest = &tokens[after_words.min(tokens.len())..];
    let rest = rest.iter().take_while(|t| t.as_str() != "--");
    for t in rest {
        if let Some(r) = RENAMED.iter().find(|r| {
            r.path == &words[..]
                && r.flag.is_some_and(|f| {
                    t == f || t.strip_prefix(f).is_some_and(|v| v.starts_with('='))
                })
        }) {
            return Some(r);
        }
    }
    None
}

/// The removed command a parsed command line names: a hidden stub clap
/// matched. `main` refuses through [`find`] before clap runs, so this is the
/// second line: a stub that is parsed is refused with the same words.
pub fn parsed(cli_command: &crate::cli::Commands) -> Option<&'static Renamed> {
    use crate::cli::{catalog, report, unit, BackendStub, Commands, StagingStub};
    let path: &[&str] = match cli_command {
        Commands::Backend { command } => match command {
            BackendStub::Add(_) => &["backend", "add"],
        },
        Commands::Staging { command } => match command {
            StagingStub::Clean(_) => &["staging", "clean"],
            StagingStub::Status(_) => &["staging", "status"],
        },
        Commands::Export(_) => &["export"],
        Commands::Import(_) => &["import"],
        Commands::Report {
            command: report::ReportCommands::Pending(_),
        } => &["report", "pending"],
        Commands::Report {
            command: report::ReportCommands::Summary(_),
        } => &["report", "summary"],
        Commands::Report {
            command: report::ReportCommands::Supersedable(_),
        } => &["report", "supersedable"],
        Commands::Unit {
            command: unit::UnitCommands::Status(_),
        } => &["unit", "status"],
        Commands::Catalog {
            command: catalog::CatalogCommands::Ls(_),
        } => &["catalog", "ls"],
        _ => return None,
    };
    command(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// The program name, not spelt as a literal: `tests/refusal_recipes.rs`
    /// joins this file's string literals and would read a literal one
    /// followed by an old spelling in the next as a mention of it.
    const ARGV0: &str = env!("CARGO_PKG_NAME");

    fn argv(line: &str) -> Vec<OsString> {
        std::iter::once(ARGV0)
            .chain(line.split_whitespace())
            .map(OsString::from)
            .collect()
    }

    fn found(line: &str) -> Option<String> {
        find(&argv(line)).map(Renamed::old)
    }

    #[test]
    fn the_global_flags_that_take_a_value_are_the_ones_the_scan_skips() {
        let cli = crate::cli::Cli::command();
        let mut taking: Vec<String> = cli
            .get_arguments()
            .filter(|a| a.is_global_set() && a.get_action().takes_values())
            .map(|a| {
                assert!(
                    a.get_short().is_none(),
                    "a global flag with a short form takes a value: teach `find` its short form"
                );
                format!("--{}", a.get_long().expect("a global flag has a long form"))
            })
            .collect();
        taking.sort();
        assert_eq!(taking, GLOBAL_VALUE_FLAGS);
    }

    #[test]
    fn a_removed_command_is_found_however_it_is_typed() {
        for (line, old) in [
            ("staging clean --force", "staging clean"),
            ("staging clean --help", "staging clean"),
            ("staging clean -h", "staging clean"),
            ("help staging clean", "staging clean"),
            ("staging", "staging"),
            ("help staging", "staging"),
            ("staging bogus", "staging"),
            ("staging help clean", "staging"),
            (
                "--home /h --json staging --dry-run status",
                "staging status",
            ),
            ("--home=/h -n staging status", "staging status"),
            ("report --json pending", "report pending"),
            ("report summary", "report summary"),
            ("report supersedable", "report supersedable"),
            ("unit status keepsake --dirty", "unit status"),
            ("catalog ls u --version 2", "catalog ls"),
            ("export --unit u --to /d", "export"),
            ("import --label L --generation LTO-6", "import"),
            ("backend", "backend"),
            ("backend add --name d", "backend add"),
            ("--config /c/config.toml backend add", "backend add"),
        ] {
            assert_eq!(found(line).as_deref(), Some(old), "{line}");
        }
    }

    #[test]
    fn a_removed_flag_is_found_after_its_command_only() {
        for (line, old) in [
            (
                "volume compact-write --destination L",
                "volume compact-write --destination",
            ),
            (
                "volume compact-write --destination=L",
                "volume compact-write --destination",
            ),
            (
                "volume read-slices --from L --unit u",
                "volume read-slices --unit",
            ),
            ("host check --unit ci.timer", "host check --unit"),
            ("db backup --to /b/t.db", "db backup --to"),
            ("key escrow-kit --out /k", "key escrow-kit --out"),
            (
                "collection run --collection c --label L",
                "collection run --collection",
            ),
            (
                "--json collection run --label L --collection=c",
                "collection run --collection",
            ),
        ] {
            assert_eq!(found(line).as_deref(), Some(old), "{line}");
        }
    }

    #[test]
    fn a_new_spelling_and_the_frozen_spellings_are_not_found() {
        for line in [
            "",
            "help",
            "help volume compact-write",
            "stage clean --force",
            "stage list --owing",
            "status",
            "unit info u",
            "unit export u --to /d",
            "unit list --tag status",
            "catalog list u",
            "report reclaimable",
            "volume import L --generation LTO-6",
            "volume compact-write --to L",
            "volume read-slices u --from L",
            "host check --systemd-unit x",
            "db backup /b/t.db",
            "db import /b/t.db",
            "key escrow-kit /k",
            "collection run c --label L",
            "drive add --name d",
            "quick-archive export --tenant t --volume V",
            // A removed flag's name is only refused after its command.
            "restore volume L --to /o --unit u",
            "volume write L -- --destination",
            // The three commands every tape's File 1 prints.
            "init --operator OP --escrow-public-key age1x",
            "catalog rebuild --from-volume --device /dev/nst0 --key K",
            "restore unit --unit U --device /dev/nst0 --to /d --key K",
        ] {
            assert_eq!(found(line), None, "{line}");
        }
    }

    #[test]
    fn the_message_names_both_spellings_and_the_release() {
        let r = command(&["staging", "clean"]).unwrap();
        let m = r.message();
        // Built from parts: a literal old spelling after `tapectl ` here
        // would itself fail `tests/refusal_recipes.rs`'s scan of src.
        assert!(m.contains(&format!("`{ARGV0} {}`", r.old())), "{m}");
        assert_eq!(r.old(), ["staging", "clean"].join(" "));
        assert!(m.contains("1.9.0"), "{m}");
        assert!(m.ends_with("use `tapectl stage clean`"), "{m}");
    }

    /// The tree and the table agree: every hidden command in the clap tree
    /// is a removed command of the table, and every removed command of the
    /// table is a hidden command of the tree.
    #[test]
    fn every_hidden_command_is_a_removed_command_and_back() {
        fn walk(cmd: &clap::Command, path: Vec<String>, hidden: bool, out: &mut Vec<Vec<String>>) {
            for sub in cmd.get_subcommands() {
                let mut p = path.clone();
                p.push(sub.get_name().to_string());
                let h = hidden || sub.is_hide_set();
                if h {
                    out.push(p.clone());
                }
                walk(sub, p, h, out);
            }
        }
        let mut hidden = Vec::new();
        walk(&crate::cli::Cli::command(), Vec::new(), false, &mut hidden);
        hidden.sort();
        let mut table: Vec<Vec<String>> = RENAMED
            .iter()
            .filter(|r| r.flag.is_none())
            .map(|r| r.path.iter().map(|s| s.to_string()).collect())
            .collect();
        table.sort();
        assert_eq!(hidden, table);
    }

    /// Every removed flag is gone from its command, and its command stays.
    #[test]
    fn every_removed_flag_is_gone_from_a_command_that_stays() {
        let cli = crate::cli::Cli::command();
        for r in RENAMED.iter().filter(|r| r.flag.is_some()) {
            let mut node = &cli;
            for word in r.path {
                node = node
                    .find_subcommand(word)
                    .unwrap_or_else(|| panic!("{}: its command is gone", r.old()));
            }
            assert!(!node.is_hide_set(), "{}", r.old());
            let long = r.flag.unwrap().trim_start_matches("--");
            assert!(
                node.get_arguments().all(|a| a.get_long() != Some(long)),
                "{}: the flag is still there",
                r.old()
            );
        }
    }

    /// The stubs parse, and `parsed` names each one.
    #[test]
    fn every_stub_parses_and_is_refused_by_name() {
        use clap::Parser;
        for r in RENAMED.iter().filter(|r| r.flag.is_none()) {
            let words: Vec<&str> = r.path.to_vec();
            let is_group = RENAMED
                .iter()
                .any(|o| o.path.len() > words.len() && o.path.starts_with(&words));
            if is_group {
                continue;
            }
            let mut line = vec![ARGV0];
            line.extend(&words);
            line.extend(["--anything", "x", "--help"]);
            let cli =
                crate::cli::Cli::try_parse_from(&line).unwrap_or_else(|e| panic!("{line:?}: {e}"));
            assert_eq!(parsed(&cli.command), Some(r), "{line:?}");
        }
    }
}
