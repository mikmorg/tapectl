//! Shared destructive-operation consent gate (ADR-0008, issue #38).
//!
//! ADR-0008 splits destructive-operation consent into three tiers:
//!
//! - **Tier 1** (staleness) is display-only and never reaches this module —
//!   evidence age is printed wherever a destructive operation consumes copy
//!   coverage, and it never blocks (ADR-0004, unchanged).
//! - **Tier 2** (degraded but non-zero coverage) funnels through
//!   [`confirm`] — the single mechanic every Tier-2 destructive command
//!   calls, so the TTY / `--yes` / `--force` logic lives in exactly one
//!   place instead of one divergent copy per command.
//! - **Tier 3** (zero coverage) must NEVER call [`confirm`]. Those
//!   conditions are unconditional refusals that no flag or prompt may
//!   waive — see `src/volume/session.rs`'s `AlreadySealed`, whose check
//!   (`check_tape_contact`) takes no `force` parameter at all, so it is
//!   structurally impossible to defeat. Routing a Tier-3 condition through
//!   this module would itself be the bug: it would let `--yes` waive a
//!   guard ADR-0008 says nothing may waive.

use std::io::{IsTerminal, Write};

use crate::error::{Result, TapectlError};

/// Gate a Tier-2 destructive action on consent.
///
/// `action` names the operation for the prompt/refusal text (e.g.
/// `"retire volume \"L6-0007\""`). `facts` are the ADR-0004 coverage facts
/// to show at the moment consent is asked, one already-formatted line
/// each. `assume_yes` is the caller's own OR of every flag that should
/// waive the prompt for this command (`--force`, `--yes`, or both —
/// see call sites).
///
/// Behavior:
/// - `assume_yes`: proceeds without touching stdin at all.
/// - stdin is a terminal: prints `facts`, then prompts; proceeds only on
///   an explicit "y"/"yes".
/// - stdin is NOT a terminal and `!assume_yes`: refuses immediately. This
///   is the half of the contract that matters most (issue #33's class of
///   bug: a read that blocks forever on a handle that will never produce
///   input) — this branch must never attempt to read stdin.
pub fn confirm(action: &str, facts: &[String], assume_yes: bool) -> Result<()> {
    confirm_with(
        action,
        facts,
        assume_yes,
        std::io::stdin().is_terminal(),
        || {
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            Ok(input)
        },
    )
}

/// [`confirm`] with the TTY check and the stdin read both injected, so
/// tests can exercise every branch — including "non-TTY refuses" — without
/// ever touching a real file descriptor. `read_answer` is only ever
/// invoked from the `is_tty` branch; tests prove the non-TTY branch never
/// reaches it by passing a closure that panics if called.
fn confirm_with(
    action: &str,
    facts: &[String],
    assume_yes: bool,
    is_tty: bool,
    read_answer: impl FnOnce() -> Result<String>,
) -> Result<()> {
    if assume_yes {
        return Ok(());
    }

    if !is_tty {
        return Err(TapectlError::Other(format!(
            "{action} refused: non-interactive session with no confirmation given — \
             refusing rather than assuming consent (re-run with --yes to proceed)"
        )));
    }

    for fact in facts {
        eprintln!("{fact}");
    }
    eprint!("{action} — proceed? [y/N] ");
    let _ = std::io::stderr().flush();

    let input = read_answer()?;
    match input.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        _ => Err(TapectlError::Other(format!(
            "{action}: aborted, not confirmed"
        ))),
    }
}

#[cfg(test)]
mod tests {
    //! These tests exercise `confirm_with` exclusively, never the real
    //! `confirm()`. Calling the real `confirm()` with `assume_yes: false`
    //! from a test would read `std::io::stdin().is_terminal()` for real —
    //! if the test binary happens to be run attached to an actual terminal
    //! (a developer running `cargo test` interactively without redirecting
    //! stdin), that would attempt a real prompt and read, hanging the
    //! suite. That is precisely the issue #33 class of bug this module
    //! exists to prevent, so these tests inject both the TTY answer and
    //! the reader instead of depending on the ambient environment.
    use super::*;

    #[test]
    fn non_tty_without_yes_refuses_and_never_reads_stdin() {
        // The crux of issue #33's trap: prove the refusal path cannot
        // hang by proving it never even attempts the read that would
        // block on a handle nobody will ever write to.
        let result = confirm_with("test op", &[], false, false, || {
            panic!("must never attempt to read stdin when non-TTY and consent not assumed")
        });
        let err = result.expect_err("non-TTY without consent must refuse");
        assert!(
            err.to_string().contains("refused"),
            "refusal message must say so: {err}"
        );
        assert!(
            err.to_string().contains("--yes"),
            "refusal message must name the override: {err}"
        );
    }

    #[test]
    fn non_tty_with_yes_proceeds_without_reading_stdin() {
        let result = confirm_with("test op", &[], true, false, || {
            panic!("assume_yes must short-circuit before any stdin read")
        });
        assert!(
            result.is_ok(),
            "assume_yes must proceed even non-interactively"
        );
    }

    #[test]
    fn tty_with_yes_still_short_circuits_before_reading() {
        // assume_yes must win regardless of TTY-ness -- no prompt at all,
        // even when a prompt would otherwise be possible.
        let result = confirm_with("test op", &[], true, true, || {
            panic!("assume_yes must short-circuit even on a TTY")
        });
        assert!(result.is_ok());
    }

    #[test]
    fn tty_confirmed_with_y_proceeds() {
        let result = confirm_with("test op", &["fact one".to_string()], false, true, || {
            Ok("y\n".to_string())
        });
        assert!(result.is_ok());
    }

    #[test]
    fn tty_confirmed_with_yes_proceeds() {
        let result = confirm_with("test op", &[], false, true, || Ok("yes\n".to_string()));
        assert!(result.is_ok());
    }

    #[test]
    fn tty_declined_refuses() {
        let result = confirm_with("test op", &[], false, true, || Ok("n\n".to_string()));
        assert!(result.is_err());
    }

    #[test]
    fn tty_empty_answer_refuses() {
        // Default must be "no" -- a bare Enter must not be read as consent.
        let result = confirm_with("test op", &[], false, true, || Ok("\n".to_string()));
        assert!(result.is_err());
    }
}
