//! CLI smoke layer (issue #44).
//!
//! Two jobs:
//!   1. `clap`'s own `debug_assert()` catches malformed derive definitions
//!      (conflicting arg ids, bad defaults, etc.) that only surface at
//!      startup, before any test ever exercises the command.
//!   2. A handful of `try_parse_from` cases pin the flag surface for
//!      commands reachable from this crate, and a process-level smoke run
//!      exercises the real binary end-to-end against a throwaway `HOME` —
//!      never the operator's real `~/.tapectl`.
//!
//! `#[cfg(test)]` items in the lib crate (e.g. `db::open_memory`) are not
//! visible from here — this is a separate integration-test binary, so
//! process-level tests are the only way to reach `main`'s dispatch.

use clap::{CommandFactory, Parser};
use std::process::Command;
use tapectl::cli::Cli;
use tempfile::TempDir;

/// clap's own consistency check: catches conflicting ids, invalid
/// defaults, duplicate short flags, etc. across the whole derive tree.
#[test]
fn cli_debug_assert() {
    Cli::command().debug_assert();
}

#[test]
fn parses_bare_init() {
    let cli = Cli::try_parse_from(["tapectl", "init"]).expect("init should parse");
    assert!(matches!(cli.command, tapectl::cli::Commands::Init { .. }));
    assert!(!cli.json);
}

#[test]
fn parses_init_with_operator() {
    let cli = Cli::try_parse_from(["tapectl", "init", "--operator", "alice"])
        .expect("init --operator should parse");
    match cli.command {
        tapectl::cli::Commands::Init { operator } => {
            assert_eq!(operator.as_deref(), Some("alice"));
        }
        _ => panic!("expected Init"),
    }
}

#[test]
fn parses_global_json_flag_before_and_after_subcommand() {
    // `--json` is declared `#[arg(long, global = true)]` on `Cli`, so clap
    // propagates it into every subcommand: both orderings must parse AND
    // both must land on the same top-level `cli.json` field. The process
    // smoke below invokes `audit --json` (flag last), so if propagation
    // ever regressed, that ordering would start silently emitting human
    // output instead of JSON — pin both sides explicitly.
    let before = Cli::try_parse_from(["tapectl", "--json", "audit"]).expect("--json before");
    assert!(before.json);
    let after = Cli::try_parse_from(["tapectl", "audit", "--json"]).expect("--json after");
    assert!(after.json);
}

#[test]
fn parses_audit_flags() {
    let cli = Cli::try_parse_from(["tapectl", "audit", "--action-plan", "--unit", "photos"])
        .expect("audit flags should parse");
    match cli.command {
        tapectl::cli::Commands::Audit { action_plan, unit } => {
            assert!(action_plan);
            assert_eq!(unit.as_deref(), Some("photos"));
        }
        _ => panic!("expected Audit"),
    }
}

#[test]
fn parses_db_fsck_repair_flag() {
    let cli = Cli::try_parse_from(["tapectl", "db", "fsck", "--repair"])
        .expect("db fsck --repair should parse");
    match cli.command {
        tapectl::cli::Commands::Db { command } => {
            assert!(matches!(
                command,
                tapectl::cli::DbCommands::Fsck { repair: true }
            ));
        }
        _ => panic!("expected Db"),
    }
}

#[test]
fn parses_config_check() {
    let cli =
        Cli::try_parse_from(["tapectl", "config", "check"]).expect("config check should parse");
    match cli.command {
        tapectl::cli::Commands::Config { command } => {
            assert!(matches!(command, tapectl::cli::ConfigCommands::Check));
        }
        _ => panic!("expected Config"),
    }
}

/// Run the real `tapectl` binary with `arg` and a `HOME` pinned to `home`,
/// so nothing touches the operator's real `~/.tapectl`.
fn run_tapectl(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("failed to spawn tapectl binary")
}

/// End-to-end process smoke: init -> audit --json -> config check --json
/// -> db fsck, entirely inside a throwaway HOME.
///
/// This is the load-bearing test for issue #44 — see the negative-control
/// evidence in the commit/PR description: a stray `println!` after any
/// `--json` block, or a change that makes `db fsck` exit 0 on a genuine
/// integrity failure, must turn this test red.
#[test]
fn cli_smoke_sequence_against_a_throwaway_home() {
    let home = TempDir::new().expect("tempdir");

    // 1. init
    let init_out = run_tapectl(home.path(), &["init"]);
    assert!(
        init_out.status.success(),
        "init failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&init_out.stdout),
        String::from_utf8_lossy(&init_out.stderr)
    );

    // Positive proof the redirect actually worked — init must have written
    // under the throwaway HOME, not the operator's real one. An
    // exit-code-only assertion would pass just as happily if HOME were
    // silently ignored.
    let db_path = home.path().join(".tapectl").join("tapectl.db");
    assert!(
        db_path.exists(),
        "expected {} to exist after init — HOME redirect did not take effect",
        db_path.display()
    );

    // 2. audit --json — must be clean (exit 0) on a freshly initialized,
    // unit-less database, and stdout must be *pure* JSON (issue #56 defect
    // shape: a println! human trailer alongside the JSON payload).
    let audit_out = run_tapectl(home.path(), &["audit", "--json"]);
    assert_eq!(
        audit_out.status.code(),
        Some(0),
        "audit --json on a clean fresh db should exit 0: stderr={}",
        String::from_utf8_lossy(&audit_out.stderr)
    );
    let audit_stdout = String::from_utf8_lossy(&audit_out.stdout);
    serde_json::from_str::<serde_json::Value>(&audit_stdout).unwrap_or_else(|e| {
        panic!("audit --json stdout did not parse as pure JSON: {e}\nstdout={audit_stdout:?}")
    });

    // 3. config check --json — same pure-JSON requirement.
    let config_out = run_tapectl(home.path(), &["config", "check", "--json"]);
    assert!(
        config_out.status.success(),
        "config check --json failed: stderr={}",
        String::from_utf8_lossy(&config_out.stderr)
    );
    let config_stdout = String::from_utf8_lossy(&config_out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&config_stdout).unwrap_or_else(|e| {
        panic!(
            "config check --json stdout did not parse as pure JSON: {e}\nstdout={config_stdout:?}"
        )
    });
    assert_eq!(parsed["valid"], serde_json::json!(true));

    // 3b. issue #62 depth checks: the new fields must be present and the
    // whole payload must still parse as one JSON object (the #56 defect
    // class — a stray println! outside the json branch corrupts the
    // stream) with the exit code unchanged (asserted above via
    // `.success()`, i.e. still 0 with these fields added).
    assert!(
        parsed.get("dar").is_some(),
        "config check --json missing 'dar' depth-check field: {parsed}"
    );
    assert!(
        parsed.get("staging").is_some(),
        "config check --json missing 'staging' depth-check field: {parsed}"
    );
    assert!(
        parsed.get("tape_devices").is_some(),
        "config check --json missing 'tape_devices' field: {parsed}"
    );
    assert!(
        parsed.get("decorative_keys").is_some(),
        "config check --json missing 'decorative_keys' field: {parsed}"
    );
    // Issue #97: the pre-existing-unsupported-compression advisory field
    // must be present (empty here — a fresh db has no archive_sets rows)
    // and, per its contract, must never affect the exit code (already
    // asserted above via `.success()`).
    assert_eq!(
        parsed.get("unsupported_compression"),
        Some(&serde_json::json!([])),
        "config check --json missing/non-empty 'unsupported_compression' field on a fresh db: {parsed}"
    );
    // Fresh init's default dar.binary (/opt/dar/bin/dar) does not exist on
    // this machine — the headline case this ticket exists for. It must be
    // reported, not silently absent, and it must not fail the command.
    assert_eq!(
        parsed["dar"]["status"],
        serde_json::json!("missing"),
        "expected the default dar.binary to be reported missing: {parsed}"
    );

    // 4. db fsck — a clean fresh db must exit 0. (#45 fixed fsck so it
    // cannot exit 0 while reporting a real integrity failure or issues;
    // this pins the healthy-path exit code so a regression that makes
    // fsck *always* report success, healthy or not, would not be masked by
    // only ever testing the failure path.)
    let fsck_out = run_tapectl(home.path(), &["db", "fsck"]);
    assert_eq!(
        fsck_out.status.code(),
        Some(0),
        "db fsck on a clean fresh db should exit 0: stdout={}\nstderr={}",
        String::from_utf8_lossy(&fsck_out.stdout),
        String::from_utf8_lossy(&fsck_out.stderr)
    );
}

/// Calling `db fsck` before `init` must fail loudly (non-zero exit), not
/// silently report success against a database that doesn't exist yet.
/// A cheap companion to the exit-code pin above: this is the "never exits
/// 0 on failure" side, exercised without needing to corrupt a real db.
#[test]
fn db_fsck_before_init_is_not_a_silent_success() {
    let home = TempDir::new().expect("tempdir");
    let out = run_tapectl(home.path(), &["db", "fsck"]);
    assert_ne!(
        out.status.code(),
        Some(0),
        "db fsck against an uninitialized home must not exit 0"
    );
}

/// Issue #61: `db export` must emit one complete JSON document — schema
/// version plus every table — to stdout, not the old seven hardcoded
/// per-table counts. Cheap on purpose (init + one tenant only): the point
/// is proving the whole-stdout-parses and shape-present properties, not
/// exercising a large table.
#[test]
fn db_export_emits_one_parseable_json_document() {
    let home = TempDir::new().expect("tempdir");

    let init_out = run_tapectl(home.path(), &["init"]);
    assert!(
        init_out.status.success(),
        "init failed: stderr={}",
        String::from_utf8_lossy(&init_out.stderr)
    );

    let tenant_out = run_tapectl(home.path(), &["tenant", "add", "acme"]);
    assert!(
        tenant_out.status.success(),
        "tenant add failed: stderr={}",
        String::from_utf8_lossy(&tenant_out.stderr)
    );

    let export_out = run_tapectl(home.path(), &["db", "export"]);
    assert_eq!(
        export_out.status.code(),
        Some(0),
        "db export should exit 0: stderr={}",
        String::from_utf8_lossy(&export_out.stderr)
    );

    let stdout = String::from_utf8_lossy(&export_out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("db export stdout did not parse as one JSON document: {e}\nstdout={stdout:?}")
    });

    assert!(
        parsed.get("schema_version").is_some(),
        "db export JSON missing 'schema_version': {parsed}"
    );
    let tables = parsed
        .get("tables")
        .unwrap_or_else(|| panic!("db export JSON missing 'tables': {parsed}"));
    assert!(
        tables.get("tenants").is_some(),
        "db export 'tables' missing 'tenants': {tables}"
    );
    let tenants = tables["tenants"].as_array().expect("tenants is an array");
    assert!(
        tenants.iter().any(|row| row["name"] == "acme"),
        "expected tenant 'acme' in exported tenants table: {tenants:?}"
    );
}

// ---------------------------------------------------------------------
// Issue #98: two-real-process concurrency tests for the stage_create flock.
//
// A same-process test with two `Connection`s would prove nothing about
// process death — the kernel only releases a `flock` when the *process*
// holding it exits, so these tests spawn the real binary as a genuine
// child process, kill (or leave running) that child, and observe the
// effect from a second, independent `tapectl` invocation / DB connection.
// ---------------------------------------------------------------------

/// Bring up a throwaway `HOME` far enough to run `stage create`: init,
/// operator tenant already created by `init`, a fresh tenant + key, a unit
/// pointing at `source_dir`, and one snapshot of it. Also repoints
/// `config.toml`'s `staging.directory` at `staging_dir` — `init`'s default
/// (`/mnt/staging`) is an arbitrary system path this test must never touch.
///
/// Returns the unit name to pass to `stage create`.
fn prepare_home_for_staging(
    home: &std::path::Path,
    source_dir: &std::path::Path,
    staging_dir: &std::path::Path,
) -> String {
    // Fail FAST if dar is missing (issue #43). Without this, `stage create`
    // dies immediately but the callers' `poll_stage_set_status` sits out its
    // full 60-second timeout first, so a dar-less machine pays two silent
    // 60s hangs before seeing an error that never names dar.
    // `tests/test_dependencies.rs` reports the dependency properly; this is
    // only here so these two tests fail in milliseconds rather than minutes.
    assert!(
        Command::new("dar")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
        "`dar` not on PATH — see tests/test_dependencies.rs for what this costs"
    );

    let init_out = run_tapectl(home, &["init"]);
    assert!(
        init_out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );

    // Repoint staging.directory at our own TempDir before anything stages,
    // and dar.binary at wherever this box actually has dar installed —
    // `Config::default`'s `/opt/dar/bin/dar` is very unlikely to exist.
    let config_path = home.join(".tapectl").join("config.toml");
    let mut cfg = tapectl::config::Config::load(&config_path).expect("load freshly-init'd config");
    cfg.staging.directory = staging_dir.to_string_lossy().to_string();
    cfg.dar.binary = std::env::var("TAPECTL_TEST_DAR_BIN").unwrap_or_else(|_| "dar".to_string());
    cfg.save(&config_path).expect("save repointed config");

    // `tenant add` already generates a keypair automatically (see
    // `TenantCommands::Add`'s own doc comment) — no separate `key generate`
    // needed, and calling one anyway would collide on the default alias.
    let tenant_out = run_tapectl(home, &["tenant", "add", "acme"]);
    assert!(
        tenant_out.status.success(),
        "tenant add failed: {}",
        String::from_utf8_lossy(&tenant_out.stderr)
    );

    let unit_name = "unit1";
    let unit_out = run_tapectl(
        home,
        &[
            "unit",
            "init",
            &source_dir.to_string_lossy(),
            "--tenant",
            "acme",
            "--name",
            unit_name,
        ],
    );
    assert!(
        unit_out.status.success(),
        "unit init failed: {}",
        String::from_utf8_lossy(&unit_out.stderr)
    );

    let snap_out = run_tapectl(home, &["snapshot", "create", unit_name]);
    assert!(
        snap_out.status.success(),
        "snapshot create failed: {}",
        String::from_utf8_lossy(&snap_out.stderr)
    );

    unit_name.to_string()
}

/// A source directory with one large-ish, incompressible file — big enough
/// that `stage create`'s sha256 validation + dar run take a real,
/// observable amount of wall-clock time, giving the polling loops below a
/// wide window to catch the process mid-flight. Zero-filled data compresses
/// to nothing and would race dar to completion in milliseconds; pseudo-random
/// bytes don't.
fn make_slow_source_dir() -> tempfile::TempDir {
    use std::io::Write;
    let dir = tempfile::TempDir::new().expect("source tempdir");
    let path = dir.path().join("bulk.bin");
    let mut f = std::fs::File::create(&path).expect("create bulk file");
    let mut buf = vec![0u8; 1024 * 1024];
    for i in 0..250 {
        // A cheap, non-cryptographic fill that isn't just zeros or a
        // repeating byte (both of which dar's compression would flatten to
        // near-nothing): vary the byte value per megabyte per offset.
        for (j, b) in buf.iter_mut().enumerate() {
            *b = ((i * 2654435761u64 + j as u64) % 251) as u8;
        }
        f.write_all(&buf).expect("write bulk chunk");
    }
    dir
}

/// Poll `stage_sets` (via a fresh, independent `rusqlite::Connection` — a
/// second process's-eye view of the same DB, not the harness's own
/// `tapectl::db::open`, which would run the sweep itself) until a row
/// reaches `want_status`, returning `(stage_set_id, staging_path_of_slice_1)`
/// once found. Bounded by `timeout`; panics with a clear message on
/// expiry rather than hanging the suite.
fn poll_stage_set_status(
    db_path: &std::path::Path,
    want_status: &str,
    timeout: std::time::Duration,
) -> i64 {
    let start = std::time::Instant::now();
    loop {
        if let Ok(conn) = rusqlite::Connection::open(db_path) {
            let row: Option<(i64, String)> = conn
                .query_row(
                    "SELECT id, status FROM stage_sets ORDER BY id DESC LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok();
            if let Some((id, status)) = row {
                if status == want_status {
                    return id;
                }
            }
        }
        if start.elapsed() > timeout {
            panic!(
                "timed out after {:?} waiting for a stage_sets row at status='{want_status}'",
                timeout
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Poll `staging_dir` until at least one plaintext `.dar` file appears —
/// proof dar has actually started producing output, not just that
/// `stage_create` got as far as its initial INSERT. Bounded by `timeout`.
fn poll_for_any_dar_file(
    staging_dir: &std::path::Path,
    timeout: std::time::Duration,
) -> std::path::PathBuf {
    let start = std::time::Instant::now();
    loop {
        if let Ok(entries) = std::fs::read_dir(staging_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".dar") {
                    return entry.path();
                }
            }
        }
        if start.elapsed() > timeout {
            panic!(
                "timed out after {:?} waiting for a .dar file to appear under {}",
                timeout,
                staging_dir.display()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// SIGKILL a child by pid via the system `kill` utility — deliberately NOT
/// `nix::sys::signal::kill` (the `nix` crate here has no `signal` feature
/// enabled, and issue #98's guardrails forbid adding one).
fn sigkill(pid: u32) {
    let status = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .expect("failed to invoke kill(1)");
    assert!(status.success(), "kill -KILL {pid} failed");
}

/// Test A (issue #98): a stage crashed by SIGKILL is detected as such by
/// the next `db::open()` sweep — its row moves 'staging' -> 'failed' — and
/// `staging clean` then removes the plaintext it left behind.
#[test]
fn crashed_stage_is_detected_and_then_cleanable() {
    let home = TempDir::new().expect("home tempdir");
    let staging_dir = TempDir::new().expect("staging tempdir");
    let source_dir = make_slow_source_dir();

    let unit_name = prepare_home_for_staging(home.path(), source_dir.path(), staging_dir.path());
    let db_path = home.path().join(".tapectl").join("tapectl.db");

    let mut child = Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(["stage", "create", &unit_name])
        .env("HOME", home.path())
        .env_remove("XDG_CONFIG_HOME")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to spawn stage create");

    // Precondition 1: the row exists and is 'staging' — proves the INSERT
    // + flock + COMMIT sequence has happened.
    poll_stage_set_status(&db_path, "staging", std::time::Duration::from_secs(60));
    // Precondition 2: dar has actually produced plaintext output — gives
    // `staging clean`'s prefix scan something real to find and remove.
    let dar_file = poll_for_any_dar_file(staging_dir.path(), std::time::Duration::from_secs(60));
    assert!(dar_file.exists());

    sigkill(child.id());
    let _ = child.wait();

    // Trigger the sweep via any other command's `db::open()`.
    let status_out = run_tapectl(home.path(), &["staging", "status"]);
    assert!(
        status_out.status.success(),
        "staging status failed: {}",
        String::from_utf8_lossy(&status_out.stderr)
    );

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let status: String = conn
        .query_row(
            "SELECT status FROM stage_sets ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        status, "failed",
        "a SIGKILLed stage must be swept to 'failed' by the next db::open()"
    );
    drop(conn);

    assert!(
        dar_file.exists(),
        "the sweep must mark status only, never touch files"
    );

    let clean_out = run_tapectl(home.path(), &["staging", "clean"]);
    assert!(
        clean_out.status.success(),
        "staging clean failed: {}",
        String::from_utf8_lossy(&clean_out.stderr)
    );
    assert!(
        !dar_file.exists(),
        "staging clean must remove the crashed stage's plaintext .dar file"
    );
}

/// Test B (issue #98) — the safety property: a LIVE stage (still running,
/// lock held) must NOT be disturbed by a concurrent read-only command's
/// `db::open()` sweep. Its row must stay 'staging' and its plaintext files
/// must stay on disk.
#[test]
fn live_stage_is_not_disturbed_by_a_concurrent_read_only_command() {
    let home = TempDir::new().expect("home tempdir");
    let staging_dir = TempDir::new().expect("staging tempdir");
    let source_dir = make_slow_source_dir();

    let unit_name = prepare_home_for_staging(home.path(), source_dir.path(), staging_dir.path());
    let db_path = home.path().join(".tapectl").join("tapectl.db");

    let mut child = Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(["stage", "create", &unit_name])
        .env("HOME", home.path())
        .env_remove("XDG_CONFIG_HOME")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to spawn stage create");

    poll_stage_set_status(&db_path, "staging", std::time::Duration::from_secs(60));
    let dar_file = poll_for_any_dar_file(staging_dir.path(), std::time::Duration::from_secs(60));
    assert!(dar_file.exists());

    // The live stage is still running (never killed) — run a read-only
    // command in a second, independent process while it's in flight.
    let report_out = run_tapectl(home.path(), &["db", "fsck"]);
    assert!(
        report_out.status.success(),
        "db fsck failed: {}",
        String::from_utf8_lossy(&report_out.stderr)
    );

    // The safety assertion: the live stage's row and files must be
    // untouched by that concurrent sweep.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let status: String = conn
        .query_row(
            "SELECT status FROM stage_sets ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    drop(conn);
    assert_eq!(
        status, "staging",
        "a live stage's row must not be marked 'failed' by a concurrent sweep"
    );
    assert!(
        dar_file.exists(),
        "a live stage's plaintext files must not be touched by a concurrent sweep"
    );

    // Clean up: this test never needs the child to finish, and must not
    // leak it. Kill it now that the assertions are done.
    sigkill(child.id());
    let _ = child.wait();
}

/// `staging clean --json` (issue #95) must emit *pure* JSON (issue #56
/// defect shape) that includes the new session-dir/lockfile reclamation
/// counters, on a freshly initialized database with nothing to clean.
#[test]
fn staging_clean_json_is_pure_and_reports_session_and_lockfile_counters() {
    let home = TempDir::new().expect("home tempdir");

    let init_out = run_tapectl(home.path(), &["init"]);
    assert!(
        init_out.status.success(),
        "init failed: stderr={}",
        String::from_utf8_lossy(&init_out.stderr)
    );

    let clean_out = run_tapectl(home.path(), &["staging", "clean", "--json"]);
    assert!(
        clean_out.status.success(),
        "staging clean --json failed: stderr={}",
        String::from_utf8_lossy(&clean_out.stderr)
    );

    // Parse the WHOLE stdout — a stray println! trailer alongside the JSON
    // payload must fail this, per the #56 defect class.
    let stdout = String::from_utf8_lossy(&clean_out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("staging clean --json stdout did not parse as pure JSON: {e}\nstdout={stdout:?}")
    });

    for key in [
        "sets_cleaned",
        "files_removed",
        "bytes_freed",
        "errors",
        "session_dirs_reclaimed",
        "session_dirs_retained",
        "session_dirs_orphaned",
        "lockfiles_reclaimed",
    ] {
        assert!(
            parsed.get(key).is_some(),
            "staging clean --json missing key '{key}': {parsed}"
        );
    }
    assert_eq!(parsed["session_dirs_reclaimed"], 0);
    assert_eq!(parsed["lockfiles_reclaimed"], 0);
}
