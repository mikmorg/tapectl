//! The commands an heir is told to type keep working (ADR-0012 item 203).
//!
//! Every tape's File 1 prints three commands that work for good (`init
//! --operator … --escrow-public-key …`, `catalog rebuild --from-volume
//! --device … --key …`, `restore unit --unit … --device … --to … --key …`),
//! and the Heir Kit's cover sheet names commands for the tapectl it carries.
//! A rename that touched any of them would leave a tape or a kit telling
//! its reader to type a command that is refused. So every `tapectl …`
//! command line is read out of the text the generators make (File 1 and
//! File 2 from `volume::layout`, which this test only calls, never edits;
//! the cover sheet from `crypto::escrow_kit`), and each must parse against
//! the real clap tree and resolve to a live command, never to a stub of a
//! removed spelling. Each mention in running text (`` `tapectl volume
//! identify` ``) must name a live command too.

use std::process::Command;

use clap::{CommandFactory, Parser};
use tapectl::cli::{renamed, Cli};
use tempfile::TempDir;

/// The `tapectl …` command lines of `text`: a line (after joining a
/// trailing `\` continuation) that is `tapectl` and a top-level command,
/// hidden ones included, so a removed spelling is caught and not skipped.
fn command_lines(text: &str) -> Vec<String> {
    let top: Vec<String> = Cli::command()
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .collect();
    let mut logical = Vec::new();
    let mut pending = String::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(head) = t.strip_suffix('\\') {
            pending.push_str(head.trim_end());
            pending.push(' ');
            continue;
        }
        pending.push_str(t);
        logical.push(std::mem::take(&mut pending));
    }
    logical
        .into_iter()
        .filter(|l| {
            let mut words = l.split_whitespace();
            words.next() == Some("tapectl")
                && words.next().is_some_and(|w| top.iter().any(|c| c == w))
        })
        // A shell comment after the command is not part of it.
        .map(|l| match l.find(" #") {
            Some(i) => l[..i].trim_end().to_string(),
            None => l,
        })
        .collect()
}

/// The `` `tapectl …` `` mentions in running text: the words inside each
/// backtick span that begins `tapectl `.
fn inline_mentions(text: &str) -> Vec<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .filter(|span| span.starts_with("tapectl "))
        .map(str::to_string)
        .collect()
}

/// `line` parses as a whole command line and is no stub of a removed
/// spelling, or what is wrong with it.
fn check_line(line: &str) -> Option<String> {
    let argv: Vec<&str> = line.split_whitespace().collect();
    let os: Vec<std::ffi::OsString> = argv.iter().map(Into::into).collect();
    if let Some(r) = renamed::find(&os) {
        return Some(format!(
            "{line:?} types `tapectl {}`, removed in 1.9.0",
            r.old()
        ));
    }
    match Cli::try_parse_from(&argv) {
        Err(e) => Some(format!("{line:?} does not parse: {e}")),
        Ok(cli) => renamed::parsed(&cli.command)
            .map(|r| format!("{line:?} resolves to the stub of `tapectl {}`", r.old())),
    }
}

/// The command words of `mention` name a live (not hidden) command.
fn check_mention(mention: &str) -> Option<String> {
    let cli = Cli::command();
    let mut node = &cli;
    let mut matched = 0;
    for word in mention.split_whitespace().skip(1) {
        match node.find_subcommand(word) {
            Some(sub) => {
                if sub.is_hide_set() {
                    return Some(format!("{mention:?} names a removed spelling"));
                }
                node = sub;
                matched += 1;
            }
            None => break,
        }
    }
    (matched == 0).then(|| format!("{mention:?} names no command"))
}

fn file_1() -> String {
    tapectl::volume::layout::generate_system_guide_v2("L6-0001", 12)
}

fn file_2() -> String {
    tapectl::volume::layout::generate_restore_script_v2("L6-0001", 12)
}

/// A real cover sheet: a home made by the binary's own `init` (which mints
/// the escrow recipient the kit is addressed to), and the kit generated
/// from its catalog with a small stand-in for the program it carries.
fn cover_sheet() -> String {
    let root = TempDir::new().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(["--home", root.path().join("home").to_str().unwrap(), "init"])
        .env("HOME", root.path())
        .env_remove("TAPECTL_HOME")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let db_path = root.path().join("home").join("tapectl.db");
    let conn = tapectl::db::open(&db_path).unwrap();
    let program = root.path().join("stand-in");
    std::fs::write(&program, b"a stand-in for the tapectl program").unwrap();
    let kit = root.path().join("kit");
    tapectl::crypto::escrow_kit::generate_with_program(&conn, &db_path, &kit, &program).unwrap();
    std::fs::read_to_string(kit.join("COVER.txt")).unwrap()
}

fn assert_all_live(what: &str, text: &str) -> (Vec<String>, Vec<String>) {
    let lines = command_lines(text);
    let mentions = inline_mentions(text);
    let failures: Vec<String> = lines
        .iter()
        .filter_map(|l| check_line(l))
        .chain(mentions.iter().filter_map(|m| check_mention(m)))
        .collect();
    assert!(
        failures.is_empty(),
        "{what} names a command that does not run:\n{}",
        failures.join("\n")
    );
    (lines, mentions)
}

#[test]
fn file_1s_three_commands_parse_and_are_live() {
    let (lines, _) = assert_all_live("File 1", &file_1());
    // The positive control: the extraction found the three commands that
    // work for good, word for word.
    for frozen in [
        "tapectl init --operator OPERATOR_NAME --escrow-public-key ESCROW_PUBLIC",
        "tapectl catalog rebuild --from-volume --device /dev/nst0 --key YOUR_KEY.age.key",
        "tapectl restore unit --unit UNIT --device /dev/nst0 --to /destination/UNIT --key YOUR_KEY.age.key",
    ] {
        assert!(lines.iter().any(|l| l == frozen), "{frozen:?} not in {lines:#?}");
    }
}

#[test]
fn file_2s_commands_parse_and_are_live() {
    assert_all_live("File 2 (RESTORE.sh)", &file_2());
}

#[test]
fn the_heir_kits_cover_sheet_commands_parse_and_are_live() {
    let (lines, _) = assert_all_live("the Heir Kit's cover sheet", &cover_sheet());
    for named in [
        "tapectl catalog rebuild --from-volume --device /dev/nst0 --key escrow.key",
        "tapectl restore volume --device /dev/nst0 --to ./out --key escrow.key",
        "tapectl restore raw-volume --device /dev/nst0 --to ./out",
        "tapectl volume identify --device /dev/nst0",
    ] {
        assert!(
            lines.iter().any(|l| l == named),
            "{named:?} not in {lines:#?}"
        );
    }
}

/// The checks catch what they are for: a removed spelling, a bare removed
/// group and a misspelt command, in a line and in running text.
#[test]
fn the_checks_catch_a_removed_spelling() {
    let text = "    tapectl staging clean --force\n    tapectl backend add --name d \\\n        \
                --device-tape /dev/x --generation LTO-6\n    tapectl stage clean --force\n\
                see `tapectl report summary` and `tapectl status`";
    let lines = command_lines(text);
    assert_eq!(
        lines,
        vec![
            "tapectl staging clean --force",
            "tapectl backend add --name d --device-tape /dev/x --generation LTO-6",
            "tapectl stage clean --force",
        ]
    );
    assert!(check_line(&lines[0]).is_some());
    assert!(check_line(&lines[1]).is_some());
    assert!(
        check_line(&lines[2]).is_none(),
        "{:?}",
        check_line(&lines[2])
    );
    let mentions = inline_mentions(text);
    assert_eq!(mentions, vec!["tapectl report summary", "tapectl status"]);
    assert!(check_mention(&mentions[0]).is_some());
    assert!(check_mention(&mentions[1]).is_none());
    assert!(check_line("tapectl restore unit --unit U --to /d --bogus").is_some());
}
