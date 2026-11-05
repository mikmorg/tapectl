//! ADR-0012 item 212 (OQ7): under `--json` every error is one JSON document
//! on stdout, `{"error": …, "exit_code": N}`, with the exit code the command
//! would have exited with anyway: clap's own usage errors (read from the
//! command line before clap parses it), a removed spelling's refusal (before
//! clap too), and an error the command itself returned. Without `--json`,
//! nothing changes: `error: …` on stderr and nothing on stdout.

use std::process::{Command, Output};

use tempfile::TempDir;

fn tapectl(home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .arg("--home")
        .arg(home)
        .args(args)
        .env_remove("TAPECTL_HOME")
        .output()
        .expect("spawn tapectl")
}

/// The JSON error `out` printed: the whole of stdout is one document of
/// exactly the two keys, its code the process's exit code, stderr empty.
fn json_error(out: &Output) -> String {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let doc: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}): {stdout:?}"));
    let obj = doc.as_object().expect("an object");
    assert_eq!(obj.len(), 2, "exactly error and exit_code: {doc}");
    assert_eq!(
        doc["exit_code"].as_i64(),
        out.status.code().map(i64::from),
        "the document's exit_code is the process's: {doc}"
    );
    assert!(
        out.stderr.is_empty(),
        "nothing on stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("\n  \"error\""), "pretty-printed: {stdout}");
    doc["error"]
        .as_str()
        .expect("error is a string")
        .to_string()
}

#[test]
fn a_usage_error_under_json_is_a_json_error_with_clap_s_code() {
    let home = TempDir::new().unwrap();
    // A missing argument, an unknown flag, an unknown command: clap's 2.
    for (args, says) in [
        (&["--json", "volume", "info"][..], "required"),
        (&["volume", "info", "--json", "--bogus"], "--bogus"),
        (&["--json", "frobnicate"], "frobnicate"),
        (&["--json", "volume"], "a subcommand is required"),
    ] {
        let out = tapectl(home.path(), args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let error = json_error(&out);
        assert!(error.contains(says), "{args:?}: {error}");
        assert!(!error.starts_with("error:"), "{args:?}: {error}");
    }
    // The contracts' own usage codes hold: verify's 3, audit's 70.
    let out = tapectl(home.path(), &["--json", "volume", "verify", "--bogus"]);
    assert_eq!(out.status.code(), Some(3));
    json_error(&out);
    let out = tapectl(home.path(), &["--json", "audit", "--bogus"]);
    assert_eq!(out.status.code(), Some(70));
    json_error(&out);
}

#[test]
fn a_removed_spelling_under_json_is_a_json_error() {
    let home = TempDir::new().unwrap();
    for args in [
        &["--json", "staging", "clean"][..],
        &["db", "backup", "--to", "/x/t.db", "--json"],
    ] {
        let out = tapectl(home.path(), args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let error = json_error(&out);
        assert!(
            error.contains("was renamed in 1.9.0: use `tapectl "),
            "{error}"
        );
    }
    assert!(
        !home.path().join("tapectl.db").exists(),
        "the refusal made nothing"
    );
}

#[test]
fn a_command_s_error_under_json_is_a_json_error() {
    let home = TempDir::new().unwrap();
    // Not initialized: the error `run` returns before any command module.
    let out = tapectl(home.path(), &["--json", "tenant", "list"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(json_error(&out).contains("tapectl is not initialized at"));

    let init = tapectl(home.path(), &["init", "--no-escrow", "--operator", "op"]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    // A command's own error, completed at render (`error::render`).
    let out = tapectl(home.path(), &["volume", "info", "NOPE-0001", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(json_error(&out).contains("volume not found"));

    // The positive control: without --json the same errors are text on
    // stderr, as always, and stdout is empty.
    for args in [&["volume", "info", "NOPE-0001"][..], &["staging", "clean"]] {
        let out = tapectl(home.path(), args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).starts_with("error: "),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // Help under --json stays help, on stdout, exit 0.
    let out = tapectl(home.path(), &["--json", "volume", "info", "--help"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("Usage:"));
}
