//! Issue #414's acceptance scenario, `scripts/first-run-reentry-mhvtl.sh`,
//! writes a tape: it runs only against an mhvtl drive, by hand or from the
//! gate chain. What the ungated suite can check without a device is that the
//! script parses and that its preconditions fail closed before it opens
//! anything: no opt-in, no drive named, or a drive no mhvtl link names, and
//! it stops with exit 2 having done nothing.

use std::process::{Command, Output};

const SCRIPT: &str = "scripts/first-run-reentry-mhvtl.sh";

fn run(opt_in: bool, args: &[&str]) -> Output {
    let mut cmd = Command::new("bash");
    cmd.arg(SCRIPT)
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_remove("TAPECTL_GATE_TAPE")
        .env_remove("TAPECTL_MHVTL");
    if opt_in {
        cmd.env("TAPECTL_MHVTL", "1");
    }
    cmd.output().expect("bash must be available")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn the_scenario_parses() {
    let out = Command::new("bash")
        .args(["-n", SCRIPT])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("bash must be available");
    assert!(out.status.success(), "bash -n: {}", stderr(&out));
}

#[test]
fn without_the_mhvtl_opt_in_it_refuses() {
    let out = run(false, &["--device", "/dev/null"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("TAPECTL_MHVTL=1 not set"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn with_no_drive_named_it_refuses() {
    let out = run(true, &[]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("--device not given"),
        "{}",
        stderr(&out)
    );
}

/// The positive control for the two refusals above: with the opt-in and a
/// device named, the script gets as far as the mhvtl check, and refuses a
/// node no `/dev/tape/by-id/*XYZZY*-nst` link names (here `/dev/null`, which
/// is never a tape) — the check that keeps it off the real drive.
#[test]
fn a_drive_that_is_not_mhvtl_is_refused_before_anything_opens_it() {
    let out = run(true, &["--device", "/dev/null"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("is not an mhvtl drive"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn only_kill_and_int_are_signals_it_sends() {
    let out = run(true, &["--device", "/dev/null", "--signal", "TERM"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("--signal must be KILL or INT"),
        "{}",
        stderr(&out)
    );
}
