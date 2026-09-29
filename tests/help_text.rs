//! Operator-facing help text (issue #357).
//!
//! Every `about`, `long_about` and argument help string in the clap tree is
//! read by an operator (and by an heir following `docs/cli/`), so it must
//! not carry GitHub issue numbers — the *why* an operator needs is said in
//! words or with an ADR reference — and it must not teach a `--device
//! /dev/nstN` example: tape device numbering is not stable across reboots,
//! so examples use the `/dev/tape/by-id/…-nst` path. This walks the whole
//! tree rather than a list of commands, so a new subcommand is covered the
//! day it is added.

use clap::CommandFactory;
use tapectl::cli::Cli;

/// `(command path, text)` for every help string under `cmd`.
fn help_texts(cmd: &clap::Command, path: &str, out: &mut Vec<(String, String)>) {
    let own = [
        cmd.get_about(),
        cmd.get_long_about(),
        cmd.get_before_help(),
        cmd.get_before_long_help(),
        cmd.get_after_help(),
        cmd.get_after_long_help(),
    ];
    for text in own.into_iter().flatten() {
        out.push((path.to_string(), text.to_string()));
    }
    for arg in cmd.get_arguments() {
        for text in [arg.get_help(), arg.get_long_help()].into_iter().flatten() {
            out.push((format!("{path} --{}", arg.get_id()), text.to_string()));
        }
    }
    for sub in cmd.get_subcommands() {
        help_texts(sub, &format!("{path} {}", sub.get_name()), out);
    }
}

fn all_help_texts() -> Vec<(String, String)> {
    let mut out = Vec::new();
    help_texts(&Cli::command(), "tapectl", &mut out);
    out
}

/// `#` immediately followed by a digit: an issue reference like `#139`.
fn issue_reference(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    (0..bytes.len().saturating_sub(1))
        .find(|&i| bytes[i] == b'#' && bytes[i + 1].is_ascii_digit())
        .map(|i| {
            let end = (i + 1..bytes.len())
                .find(|&j| !bytes[j].is_ascii_digit())
                .unwrap_or(bytes.len());
            &text[i..end]
        })
}

#[test]
fn help_text_carries_no_issue_numbers() {
    let texts = all_help_texts();
    // Positive control: the walk must actually reach help text, or "found
    // nothing" would pass by searching nothing.
    assert!(
        texts.len() > 200,
        "the clap walk found only {} help strings",
        texts.len()
    );
    assert!(issue_reference("see #139 here") == Some("#139"));
    assert!(issue_reference("slice 1 of 3, ADR-0012").is_none());

    let offenders: Vec<String> = texts
        .iter()
        .filter_map(|(path, text)| issue_reference(text).map(|n| format!("{path}: {n}")))
        .collect();
    assert!(
        offenders.is_empty(),
        "help text an operator reads carries issue numbers (say the why in words, or \
         cite the ADR):\n{}",
        offenders.join("\n")
    );
}
