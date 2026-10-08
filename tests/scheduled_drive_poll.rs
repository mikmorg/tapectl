//! Issue #309 (ADR-0012, 2026-10-07 item 29): the daily drive poll's
//! systemd units and wrapper.
//!
//! - `contrib/systemd/tapectl-scheduled-drive-poll.sh` reads `tapectl drive
//!   poll`'s exit code: 0 a clean reading (success ping), 1 the drive
//!   reported a problem (`/fail` — this check exists to go red), 75 a command
//!   holds the drive (no ping, not a failure), anything else no reading
//!   (`/fail`). Run for real over a stub `tapectl` and a stub `curl`, as
//!   `tests/scheduled_audit.rs` does.
//! - The unit files: the poll's service may open `/dev/sg*` and nothing
//!   else, and the audit's service still carries `PrivateDevices=true` —
//!   asserted together, so "made the poll work" can never have meant
//!   "relaxed the audit's guard".
//!
//! Ungated: no device, no catalog.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

struct Run {
    code: i32,
    output: String,
    /// `<path suffix> <body>` per ping, in order ("" is the success ping).
    pings: Vec<String>,
}

fn stub(dir: &Path, name: &str, body: &str) {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Run the wrapper over a `tapectl` whose `drive poll` prints a line and
/// exits `rc`.
fn run(rc: i32) -> Run {
    let dir = TempDir::new().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    stub(
        &bin,
        "tapectl",
        &format!("[ \"$1 $2\" = \"drive poll\" ] || exit 64\necho \"poll said {rc}\"\nexit {rc}"),
    );
    let pings = dir.path().join("pings");
    stub(
        &bin,
        "curl",
        &format!(
            "for a; do url=$a; done\nbody=$(cat)\necho \"${{url#http://hc.invalid/x}}|$body\" >> {}",
            pings.display()
        ),
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let out = Command::new("bash")
        .arg("contrib/systemd/tapectl-scheduled-drive-poll.sh")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("PATH", path)
        .env("TAPECTL_BIN", bin.join("tapectl"))
        .env("TAPECTL_DRIVE_HEALTHCHECK_URL", "http://hc.invalid/x")
        .output()
        .expect("bash must be available");
    let pings = std::fs::read_to_string(&pings)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    Run {
        code: out.status.code().expect("exited"),
        output: format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        pings,
    }
}

/// Negative AND positive control, by name: a clean reading pings success,
/// a reported problem pings `/fail` — so the wrapper can pass neither by
/// always alerting nor by never alerting. The poll's output is the body.
#[test]
fn a_reported_problem_fails_and_a_clean_reading_does_not() {
    let clean = run(0);
    assert_eq!(clean.code, 0);
    assert!(
        clean.output.contains("reported nothing"),
        "{}",
        clean.output
    );
    assert_eq!(clean.pings, ["/start|", "|poll said 0"]);

    let raised = run(1);
    assert_eq!(raised.code, 1);
    assert!(
        raised.output.contains("THE DRIVE REPORTED A PROBLEM"),
        "{}",
        raised.output
    );
    assert_eq!(raised.pings, ["/start|", "/fail|poll said 1"]);
}

/// 75: a command holds the drive, nothing was read — logged, no verdict
/// ping either way.
#[test]
fn a_held_drive_is_skipped_without_a_ping() {
    let busy = run(75);
    assert_eq!(busy.code, 75);
    assert!(busy.output.contains("skipped"), "{}", busy.output);
    assert_eq!(busy.pings, ["/start|"]);
}

/// Every other code is no reading: a failure, so a poll that cannot look
/// does not go quiet.
#[test]
fn any_other_code_is_no_reading_and_fails() {
    for rc in [2, 70, 101] {
        let r = run(rc);
        assert_eq!(r.code, rc);
        assert!(
            r.output.contains(&format!("no reading (exit {rc})")),
            "exit {rc}: {}",
            r.output
        );
        assert_eq!(r.pings.len(), 2, "exit {rc}: {:?}", r.pings);
        assert!(r.pings[1].starts_with("/fail|"), "exit {rc}: {:?}", r.pings);
    }
}

fn unit(name: &str) -> Vec<String> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("contrib/systemd")
        .join(name);
    std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("{}: {e}", p.display()))
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The device boundary, as data: the poll's service opens the sg group and
/// nothing else, and never hides it behind PrivateDevices; the audit's
/// service still has PrivateDevices=true and no device allowance at all.
#[test]
fn the_poll_may_open_sg_nodes_only_and_the_audit_still_none() {
    let poll = unit("tapectl-drive-poll.service");
    let device_lines: Vec<&String> = poll
        .iter()
        .filter(|l| l.starts_with("Device") || l.starts_with("PrivateDevices"))
        .collect();
    assert_eq!(
        device_lines,
        ["DevicePolicy=closed", "DeviceAllow=char-sg rw"],
        "the poll's service: the sg group and nothing else"
    );
    assert!(
        poll.iter()
            .any(|l| l == "ExecStart=/usr/local/lib/tapectl/tapectl-scheduled-drive-poll.sh"),
        "{poll:?}"
    );
    assert!(poll.iter().any(|l| l == "SuccessExitStatus=75"), "{poll:?}");

    for audit_side in ["tapectl-audit.service", "tapectl-backup.service"] {
        let lines = unit(audit_side);
        assert!(
            lines.iter().any(|l| l == "PrivateDevices=true"),
            "{audit_side} keeps PrivateDevices=true: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.starts_with("DeviceAllow")),
            "{audit_side} allows no device: {lines:?}"
        );
    }

    let timer = unit("tapectl-drive-poll.timer");
    assert!(timer.iter().any(|l| l == "Unit=tapectl-drive-poll.service"));
    assert!(
        timer.iter().any(|l| l.starts_with("OnCalendar=*-*-* ")),
        "daily: {timer:?}"
    );
}

/// The installer ships all three: a unit left out of its lists would be in
/// the repository and never on a host.
#[test]
fn the_installer_installs_the_poll() {
    let script = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install-systemd.sh"),
    )
    .unwrap();
    for needle in [
        "tapectl-drive-poll.service tapectl-drive-poll.timer)",
        "TIMERS=(tapectl-audit.timer tapectl-backup.timer tapectl-drive-poll.timer)",
        "tapectl-scheduled-drive-poll.sh)",
    ] {
        assert!(
            script.contains(needle),
            "install-systemd.sh lacks {needle:?}"
        );
    }
}
