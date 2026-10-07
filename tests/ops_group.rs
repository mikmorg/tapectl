//! Issue #393: an `[ops] group` lets another account watch sessions with
//! `tapectl status`, and opens nothing else in the home.
//!
//! Through the real binary and the very `[ops]` text `scripts/first-run.sh`
//! step 7 appends, on a fresh `init`: the strict loader takes it, the next
//! command gives the home and `logs/` the group with the modes the docs
//! state, every other entry of the home stays closed, and `config check`
//! reports the setup. The group is this process's own primary group, the
//! only one a test can be sure it may `chgrp` to. What this cannot prove
//! ungated is the other side: a second uid reading through the group (that
//! needs a second account and sudo).

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn tapectl(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .arg("--home")
        .arg(home)
        .args(args)
        .env_remove("TAPECTL_HOME")
        .output()
        .expect("spawn tapectl")
}

/// What step 7 appends for `group`: its own `printf` line, run by bash.
fn first_run_ops_text(group: &str) -> String {
    let script =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/first-run.sh"))
            .unwrap();
    let line = script
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("printf '\\n[ops]"))
        .expect("first-run.sh step 7 appends an [ops] table with printf");
    let printf = line.split(" | ").next().unwrap();
    let out = Command::new("bash")
        .arg("-c")
        .arg(printf)
        .env("OPS_GROUP", group)
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

#[test]
fn an_ops_group_from_first_run_shares_the_logs_and_nothing_else() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("h");
    let init = tapectl(&home, &["init", "--operator", "op"]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    // Private by default (issue #41's 0700 home).
    assert_eq!(mode(&home), 0o700);

    let gid = nix::unistd::getegid();
    let group = nix::unistd::Group::from_gid(gid).unwrap().unwrap().name;
    let text = first_run_ops_text(&group);
    assert_eq!(text, format!("\n[ops]\ngroup = \"{group}\"\n"));
    let cfg = home.join("config.toml");
    let mut config = std::fs::read_to_string(&cfg).unwrap();
    config.push_str(&text);
    std::fs::write(&cfg, config).unwrap();
    std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o644)).unwrap();

    // The strict loader takes the table (`config show` fails on any key it
    // does not know), and this very command applies the modes.
    let show = tapectl(&home, &["config", "show"]);
    assert!(
        show.status.success(),
        "{}",
        String::from_utf8_lossy(&show.stderr)
    );

    assert_eq!(mode(&home), 0o710, "the group may traverse the home");
    assert_eq!(mode(&home.join("logs")), 0o2750, "and read logs/, setgid");
    assert_eq!(
        std::fs::metadata(home.join("logs")).unwrap().gid(),
        gid.as_raw()
    );
    for entry in std::fs::read_dir(&home).unwrap() {
        let p = entry.unwrap().path();
        if p == home.join("logs") {
            continue;
        }
        assert_eq!(
            mode(&p) & 0o077,
            0,
            "{} must stay closed to the group",
            p.display()
        );
    }

    let check = tapectl(&home, &["config", "check"]);
    let said = String::from_utf8_lossy(&check.stdout);
    assert!(
        said.contains(&format!("ops: group \"{group}\" may read the session logs")),
        "{said}"
    );

    // A long command's session log is written group-readable, and records
    // how it ended: a stage of a unit that does not exist opens its session
    // and fails at once (no dar, no device).
    let stage = tapectl(&home, &["stage", "create", "no-such-unit"]);
    assert!(!stage.status.success());
    let logs: Vec<_> = std::fs::read_dir(home.join("logs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "log"))
        .collect();
    assert_eq!(logs.len(), 1, "{logs:?}");
    assert_eq!(mode(&logs[0]), 0o640);
    assert_eq!(std::fs::metadata(&logs[0]).unwrap().gid(), gid.as_raw());

    let status = tapectl(&home, &["status", "--json"]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let s = &v["sessions"][0];
    assert_eq!(s["command"], "stage create no-such-unit");
    assert_eq!(s["state"], "ended");
    assert!(
        s["outcome"].as_str().unwrap().starts_with("failed — "),
        "{s}"
    );
}
