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

#[test]
fn help_examples_never_name_a_numbered_tape_device() {
    let offenders: Vec<String> = all_help_texts()
        .into_iter()
        .filter(|(_, text)| {
            text.contains("--device /dev/nst") || text.contains("--device-tape /dev/nst")
        })
        .map(|(path, text)| format!("{path}: {text}"))
        .collect();
    assert!(
        offenders.is_empty(),
        "help examples must use /dev/tape/by-id/…-nst — /dev/nstN numbering is not \
         stable across reboots:\n{}",
        offenders.join("\n")
    );
}

/// Issue #361: one meaning per word. "Receipt" is the recipient list a
/// stage set was encrypted to (CONTEXT.md), and a warehouse provider's own
/// receipt for a deposit; the per-stage-set text files are stage reports,
/// in `<home>/stage-reports/`. The two help strings that list the home's
/// contents said "receipts" until then.
#[test]
fn receipt_in_help_means_only_the_recipients_or_a_deposit() {
    // `(path, why the word is right there)`.
    const ALLOWED: &[(&str, &str)] = &[
        (
            "tapectl init --escrow_public_key",
            "the escrow receipt: every tape's recipient list names this key",
        ),
        (
            "tapectl volume deposit add --receipt",
            "the warehouse provider's own receipt for a deposit",
        ),
    ];
    let texts = all_help_texts();
    let mentions = |text: &str| text.to_lowercase().contains("receipt");

    // Positive control: each allowance is still used, so the search reads
    // the texts it claims to and the list does not outlive its reasons.
    for (path, why) in ALLOWED {
        assert!(
            texts.iter().any(|(p, t)| p == path && mentions(t)),
            "{path} no longer says \"receipt\" ({why}) — drop it from ALLOWED"
        );
    }
    let offenders: Vec<String> = texts
        .iter()
        .filter(|(p, t)| mentions(t) && !ALLOWED.iter().any(|(a, _)| a == p))
        .map(|(p, t)| format!("{p}: {t}"))
        .collect();
    assert!(
        offenders.is_empty(),
        "\"receipt\" means the recipient list (CONTEXT.md) or a deposit's receipt; the \
         per-stage-set files are stage reports:\n{}",
        offenders.join("\n")
    );

    // The two home listings name the directory by its current name.
    for flag in ["tapectl --home", "tapectl --config"] {
        let said: Vec<String> = texts
            .iter()
            .filter(|(p, _)| p == flag)
            .map(|(_, t)| t.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();
        assert!(
            said.iter()
                .any(|t| t.contains("stage-reports") || t.contains("stage reports")),
            "{flag}'s help lists the home's contents, stage reports among them: {said:?}"
        );
    }
}
