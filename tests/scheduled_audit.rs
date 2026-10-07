//! Issue #408: `contrib/systemd/tapectl-scheduled-audit.sh` reads `tapectl
//! audit`'s exit code, and only 2 is a violation. Every code the audit's
//! contract does not name — a panic (101), an error exit, a code a later
//! release adds — is "no verdict", never "VIOLATIONS": a wrapper that pages
//! an operator about policy violations when the audit never reached a
//! verdict sends them to the wrong problem.
//!
//! Runs the real script with `TAPECTL_BIN` pointing at a stub that exits
//! with the code under test, and a `curl` stub on `PATH` that records each
//! healthcheck ping. Ungated: no device, no catalog.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

/// What one run of the wrapper did.
struct Run {
    code: i32,
    output: String,
    /// The healthcheck paths pinged, in order ("" is the success ping).
    pings: Vec<String>,
}

fn stub(dir: &Path, name: &str, body: &str) {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Run the wrapper over a `tapectl` whose `audit` exits `audit_rc`.
fn run(audit_rc: i32) -> Run {
    let dir = TempDir::new().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    stub(
        &bin,
        "tapectl",
        &format!("[ \"$1\" = audit ] && exit {audit_rc}\nexit 0"),
    );
    let pings = dir.path().join("pings");
    // The last argument is the URL; record what follows the base.
    stub(
        &bin,
        "curl",
        &format!(
            "for a; do url=$a; done\necho \"${{url#http://hc.invalid/x}}\" >> {}",
            pings.display()
        ),
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let out = Command::new("bash")
        .arg("contrib/systemd/tapectl-scheduled-audit.sh")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("PATH", path)
        .env("TAPECTL_BIN", bin.join("tapectl"))
        .env("TAPECTL_HEALTHCHECK_URL", "http://hc.invalid/x")
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

/// The audit's own contract, unchanged: 0 clean, 1 warnings (both a
/// success ping), 2 violations (a fail ping), 75 catalog busy (no verdict
/// ping). The wrapper exits with the audit's code.
#[test]
fn the_audits_named_codes_keep_their_meaning() {
    let clean = run(0);
    assert_eq!(clean.code, 0);
    assert!(clean.output.contains("audit: clean"), "{}", clean.output);
    assert_eq!(clean.pings, ["/start", ""]);

    let warn = run(1);
    assert_eq!(warn.code, 1);
    assert!(warn.output.contains("warnings only"), "{}", warn.output);
    assert_eq!(warn.pings, ["/start", ""]);

    let violations = run(2);
    assert_eq!(violations.code, 2);
    assert!(
        violations.output.contains("audit: VIOLATIONS"),
        "{}",
        violations.output
    );
    assert_eq!(violations.pings, ["/start", "/fail"]);

    let busy = run(75);
    assert_eq!(busy.code, 75);
    assert!(busy.output.contains("catalog busy"), "{}", busy.output);
    assert_eq!(busy.pings, ["/start"]);
}

/// ADR-0012, 2026-10-07 item 19: `tapectl audit` exits 70 (`EX_SOFTWARE`)
/// when it stops on an error — the code it used to share with violations.
/// The wrapper names it as the audit's own error: no verdict, a fail ping,
/// the code passed through, and never "VIOLATIONS".
#[test]
fn exit_70_is_the_audit_s_own_error_not_violations() {
    let r = run(70);
    assert_eq!(r.code, 70);
    assert!(
        r.output
            .contains("audit: no verdict (exit 70) — the audit stopped on an error"),
        "{}",
        r.output
    );
    assert!(!r.output.contains("VIOLATIONS"), "{}", r.output);
    assert_eq!(r.pings, ["/start", "/fail"]);
}

/// Any other code is a run that reached no verdict: never reported as
/// violations, still a failure (a fail ping, the code passed through) so a
/// broken audit does not go quiet.
#[test]
fn any_other_code_is_no_verdict_never_violations() {
    for rc in [3, 70, 101, 127] {
        let r = run(rc);
        assert_eq!(r.code, rc);
        assert!(
            !r.output.contains("VIOLATIONS"),
            "exit {rc} reported as violations: {}",
            r.output
        );
        assert!(
            r.output.contains(&format!("audit: no verdict (exit {rc})")),
            "exit {rc}: {}",
            r.output
        );
        assert_eq!(r.pings, ["/start", "/fail"], "exit {rc}");
    }
}
