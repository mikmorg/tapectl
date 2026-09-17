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
use rusqlite::params;
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
        tapectl::cli::Commands::Init { operator, .. } => {
            assert_eq!(operator.as_deref(), Some("alice"));
        }
        _ => panic!("expected Init"),
    }
}

/// Issue #140: `init` used to write `staging.directory = "/mnt/staging"` —
/// a path that does not exist on a stock machine and is root-owned where it
/// does, so `config check` warned about the default on every fresh install.
/// It now writes `<home>/staging` and CREATES it, 0700 like the rest of the
/// home, under whatever `--home` was actually used.
#[test]
fn init_creates_a_staging_directory_under_the_home_it_was_given() {
    use std::os::unix::fs::PermissionsExt;

    let home = TempDir::new().unwrap();
    let out = run_tapectl(home.path(), &["init"]);
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // `run_tapectl` sets HOME, so the tapectl home is `$HOME/.tapectl`.
    let staging = home.path().join(".tapectl").join("staging");
    assert!(
        staging.is_dir(),
        "init did not create {}",
        staging.display()
    );
    let mode = std::fs::metadata(&staging).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "staging dir mode is {mode:o}, expected 700");

    let config = std::fs::read_to_string(home.path().join(".tapectl").join("config.toml")).unwrap();
    assert!(
        config.contains(staging.to_str().unwrap()),
        "config.toml does not point at the staging dir init created:\n{config}"
    );
    assert!(
        !config.contains("/mnt/staging"),
        "the old unusable default is still being written:\n{config}"
    );

    // And `config check` is now quiet about staging on a fresh install,
    // which is the whole point — a default that is always wrong turns its
    // own warning into noise.
    let check = run_tapectl(home.path(), &["config", "check"]);
    let text = String::from_utf8_lossy(&check.stdout);
    assert!(
        !text.contains("staging directory") || !text.contains("does not exist"),
        "config check still warns about staging on a fresh init:\n{text}"
    );
}

#[test]
fn init_creates_the_escrow_recipient_and_prints_its_secret_to_stderr() {
    // Q3 (CTO 2026-09-10): `init` creates the permanent escrow recipient so the
    // stage-before-escrow window (issue #115) never opens. The once-only secret
    // is a human ceremony warning — stderr, never stdout — so `init --json`
    // stays parseable.
    let home = TempDir::new().unwrap();
    let out = run_tapectl(home.path(), &["init"]);
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains("AGE-SECRET-KEY"),
        "the escrow secret must NOT be on stdout:\n{stdout}"
    );
    assert!(
        stderr.contains("AGE-SECRET-KEY"),
        "the escrow secret must be printed to stderr:\n{stderr}"
    );
    assert!(
        stdout.contains("escrow:"),
        "init stdout should name the escrow public key:\n{stdout}"
    );

    // A second `key generate --escrow` must refuse — init already made one.
    let again = run_tapectl(home.path(), &["key", "generate", "--escrow"]);
    assert!(
        !again.status.success(),
        "escrow should already be registered by init; a second generate must refuse"
    );

    // `--no-escrow` opts out entirely (no secret, no registration).
    let home2 = TempDir::new().unwrap();
    let out2 = run_tapectl(home2.path(), &["init", "--no-escrow"]);
    assert!(out2.status.success());
    assert!(
        !String::from_utf8_lossy(&out2.stderr).contains("AGE-SECRET-KEY"),
        "--no-escrow must not generate an escrow secret"
    );
}

/// #139: a rebuilt machine registers the ORIGINAL escrow identity at init
/// instead of minting a replacement — the disaster-recovery arm. No secret
/// exists on this machine to print; the secret half lives only on the heir
/// kit that carries the public key being adopted here.
#[test]
fn init_with_escrow_public_key_adopts_it_and_prints_no_secret() {
    let kp = tapectl::crypto::keys::generate_keypair();
    let home = TempDir::new().unwrap();
    let out = run_tapectl(
        home.path(),
        &["init", "--escrow-public-key", &kp.public_key, "--json"],
    );
    assert!(
        out.status.success(),
        "init --escrow-public-key failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("AGE-SECRET-KEY-"),
        "adopting an existing escrow key must not print a secret:\n{stderr}"
    );
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("init --json stdout did not parse as whole JSON: {e}\n{stdout}")
    });
    assert_eq!(
        parsed["escrow_public_key"].as_str(),
        Some(kp.public_key.as_str()),
        "escrow_public_key should carry the adopted key, not a freshly minted one"
    );

    let db_path = home.path().join(".tapectl").join("tapectl.db");
    let conn = tapectl::db::open(&db_path).expect("open the initialized db");
    let registered = tapectl::db::queries::escrow_public_key(&conn)
        .expect("query escrow_public_key")
        .expect("an escrow key should be registered");
    assert_eq!(registered, kp.public_key);
}

/// Same as above, but the key is supplied as a path to a `.pub` file — the
/// form `read_or_parse_public_key` also accepts.
#[test]
fn init_with_escrow_public_key_accepts_a_pub_file() {
    let kp = tapectl::crypto::keys::generate_keypair();
    let home = TempDir::new().unwrap();
    let pub_file = home.path().join("original-escrow.age.pub");
    std::fs::write(&pub_file, format!("{}\n", kp.public_key)).unwrap();

    let out = run_tapectl(
        home.path(),
        &[
            "init",
            "--escrow-public-key",
            pub_file.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        out.status.success(),
        "init --escrow-public-key <file> failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("init --json stdout did not parse as whole JSON: {e}\n{stdout}")
    });
    assert_eq!(
        parsed["escrow_public_key"].as_str(),
        Some(kp.public_key.as_str())
    );

    let db_path = home.path().join(".tapectl").join("tapectl.db");
    let conn = tapectl::db::open(&db_path).expect("open the initialized db");
    let registered = tapectl::db::queries::escrow_public_key(&conn)
        .expect("query escrow_public_key")
        .expect("an escrow key should be registered");
    assert_eq!(registered, kp.public_key);
}

/// clap's `conflicts_with` must reject the combination before anything runs.
#[test]
fn init_rejects_escrow_public_key_together_with_no_escrow() {
    let kp = tapectl::crypto::keys::generate_keypair();
    let home = TempDir::new().unwrap();
    let out = run_tapectl(
        home.path(),
        &["init", "--no-escrow", "--escrow-public-key", &kp.public_key],
    );
    assert!(
        !out.status.success(),
        "clap should refuse --no-escrow together with --escrow-public-key"
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "clap arg-conflict errors exit 2: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let db_path = home.path().join(".tapectl").join("tapectl.db");
    assert!(
        !db_path.exists(),
        "a rejected init must create nothing: {} exists",
        db_path.display()
    );
}

/// A bad value must fail before any side effect — no half-initialised home
/// left behind to "delete it and start over" (the fallback this flag exists
/// to avoid needing).
#[test]
fn init_with_a_bad_escrow_public_key_creates_nothing() {
    let home = TempDir::new().unwrap();
    let out = run_tapectl(home.path(), &["init", "--escrow-public-key", "not-a-key"]);
    assert!(
        !out.status.success(),
        "init must refuse a value that is neither an age1... literal nor a readable .pub file"
    );
    let db_path = home.path().join(".tapectl").join("tapectl.db");
    assert!(
        !db_path.exists(),
        "a bad --escrow-public-key must leave nothing behind: {} exists",
        db_path.display()
    );
}

/// ADR-0005: exactly one escrow identity, ever. Adopting at init registers
/// it just as surely as `key generate --escrow`/`key import --escrow` would,
/// so a later `key import --escrow` must refuse exactly the same way.
#[test]
fn a_subsequent_key_import_escrow_is_refused_after_adopting_at_init() {
    let original = tapectl::crypto::keys::generate_keypair();
    let another = tapectl::crypto::keys::generate_keypair();
    let home = TempDir::new().unwrap();

    let init_out = run_tapectl(
        home.path(),
        &["init", "--escrow-public-key", &original.public_key],
    );
    assert!(
        init_out.status.success(),
        "init --escrow-public-key failed: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );

    let import_out = run_tapectl(
        home.path(),
        &["key", "import", "--escrow", &another.public_key],
    );
    assert!(
        !import_out.status.success(),
        "key import --escrow must refuse once init has already adopted an escrow recipient"
    );
    assert!(
        String::from_utf8_lossy(&import_out.stderr).contains("already registered"),
        "expected the already-registered message, got: {}",
        String::from_utf8_lossy(&import_out.stderr)
    );
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

/// Issue #172's single most important check: `init` must not write a config
/// `tapectl` itself then refuses to load. Before this issue, `init` wrote six
/// keys (`logging.level`, `logging.format`, `labels.format`,
/// `packing.strategy`, `packing.fill_threshold`, `defaults.hash`) that
/// nothing read — harmless only because nothing validated them either. Now
/// that four of the six are deleted and `#[serde(deny_unknown_fields)]`
/// (issue #171) rejects any leftover, a fresh `init` writing even one of them
/// would make `tapectl` unable to read back its own freshly written config —
/// broken on first run. Both commands must exit 0, and the generated file
/// must carry `[logging]` (wired, so it stays) but none of the four deleted
/// sections/keys.
#[test]
fn init_config_show_roundtrip_exits_zero_with_no_deleted_keys() {
    let home = TempDir::new().expect("tempdir");

    let init_out = run_tapectl(home.path(), &["init"]);
    assert!(
        init_out.status.success(),
        "tapectl init failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&init_out.stdout),
        String::from_utf8_lossy(&init_out.stderr)
    );

    let show_out = run_tapectl(home.path(), &["config", "show"]);
    assert!(
        show_out.status.success(),
        "tapectl config show failed on init's own config: stdout={}\nstderr={}",
        String::from_utf8_lossy(&show_out.stdout),
        String::from_utf8_lossy(&show_out.stderr)
    );

    let shown = String::from_utf8_lossy(&show_out.stdout);
    assert!(shown.contains("[logging]"), "{shown}");
    assert!(shown.contains("level = \"warn\""), "{shown}");
    assert!(shown.contains("format = \"full\""), "{shown}");
    for dead in [
        "[packing]",
        "[labels]",
        "strategy",
        "fill_threshold",
        "hash",
    ] {
        assert!(
            !shown.contains(dead),
            "init's config still writes deleted key/section {dead:?}:\n{shown}"
        );
    }
}

/// Acceptance criterion (issue #172): "`logging.level = \"debug\"` actually
/// changes the emitted level". Exercised against the real binary rather than
/// only the pure `LoggingConfig::tracing_level` unit test in `config.rs`, to
/// prove the wiring reaches an installed subscriber end to end — via
/// `config show`, so nothing here touches a write/restore/tape path.
#[test]
fn logging_level_debug_surfaces_the_wiring_debug_line() {
    let home = TempDir::new().expect("tempdir");
    let init_out = run_tapectl(home.path(), &["init"]);
    assert!(init_out.status.success());

    let config_path = home.path().join(".tapectl").join("config.toml");
    let default_config = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        default_config.contains("level = \"warn\""),
        "fixture assumption broken — init no longer writes level = \"warn\":\n{default_config}"
    );

    // Default level (warn): the DEBUG "loaded config" line from main.rs must
    // not appear.
    let quiet = run_tapectl(home.path(), &["config", "show"]);
    assert!(quiet.status.success());
    assert!(
        !String::from_utf8_lossy(&quiet.stderr).contains("loaded config"),
        "default logging.level = \"warn\" should not surface the debug line: {:?}",
        String::from_utf8_lossy(&quiet.stderr)
    );

    // Raise logging.level to "debug" in the config init just wrote.
    std::fs::write(
        &config_path,
        default_config.replace("level = \"warn\"", "level = \"debug\""),
    )
    .unwrap();

    let debug_run = run_tapectl(home.path(), &["config", "show"]);
    assert!(debug_run.status.success());
    assert!(
        String::from_utf8_lossy(&debug_run.stderr).contains("loaded config"),
        "logging.level = \"debug\" should surface the debug line: {:?}",
        String::from_utf8_lossy(&debug_run.stderr)
    );
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
    // Fresh init's default dar.binary is now the bare, PATH-resolved "dar"
    // (issue #124 — the old shipped default /opt/dar/bin/dar existed on no
    // mainstream distro). `dar` is a hard runtime dependency guaranteed on
    // PATH for this whole suite (CLAUDE.md; enforced by
    // tests/test_dependencies.rs), and `config check` now resolves a bare
    // name via PATH the same way the runtime does (issue #119), so a fresh
    // init's dar status must come back "ok", not "missing" — the field
    // still must be present and reported either way, and must not fail the
    // command. Coverage for the "reported, not silently absent, when
    // actually missing" case moved to depth_check.rs's own unit tests.
    assert_eq!(
        parsed["dar"]["status"],
        serde_json::json!("ok"),
        "expected a fresh init's default dar.binary ('dar') to resolve via PATH: {parsed}"
    );
    assert!(
        parsed["dar"]["path"]
            .as_str()
            .is_some_and(|p| p.starts_with('/')),
        "expected config check to report the PATH-resolved absolute path, not the bare name: {parsed}"
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
/// `config.toml`'s `staging.directory` at `staging_dir` — `init` creates
/// `<home>/staging` (issue #140), and this test wants its own tmpdir so the
/// staged bytes land where the test can see and clean them.
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

    // `init --no-escrow`: this flow registers the escrow recipient explicitly
    // below (to exercise `key generate --escrow`), so opt out of init's default
    // creation (issue #115 / CTO 2026-09-10) to avoid a double-registration.
    let init_out = run_tapectl(home, &["init", "--no-escrow"]);
    assert!(
        init_out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );

    // Repoint staging.directory at our own TempDir before anything stages.
    // `Config::default`'s dar.binary is the bare, PATH-resolved "dar"
    // (issue #124), which already works on this box; TAPECTL_TEST_DAR_BIN
    // lets a caller override it to an explicit path instead.
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

    // Issue #115 / ADR-0005: `stage create` now refuses without a registered
    // escrow recipient, because slices staged before one exists are exactly
    // the material the escrow line is supposed to be able to open — and
    // `volume write` refuses them too. Not fixture decoration: this is the
    // state every real staging run requires. `init` above created the
    // operator tenant this key hangs off, and `--escrow` prints the secret
    // once to stdout (a throwaway HOME here) rather than storing it.
    let escrow_out = run_tapectl(home, &["key", "generate", "--escrow"]);
    assert!(
        escrow_out.status.success(),
        "key generate --escrow failed: {}",
        String::from_utf8_lossy(&escrow_out.stderr)
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

// --- issue #109: --home vs --config ------------------------------------

/// `--home` selects the archive. The database must land under it, not under
/// `$HOME/.tapectl` — that is the whole point of the flag.
#[test]
fn home_flag_places_the_database_under_the_given_directory() {
    let real_home = TempDir::new().expect("tempdir");
    let archive = TempDir::new().expect("tempdir");

    let out = Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(["--home", archive.path().to_str().unwrap(), "init"])
        .env("HOME", real_home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("TAPECTL_HOME")
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "init --home failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        archive.path().join("tapectl.db").exists(),
        "--home must place the database in the named directory"
    );
    // And it must NOT have touched the real home — an exit-code-only
    // assertion would pass just as happily if it had.
    assert!(
        !real_home.path().join(".tapectl/tapectl.db").exists(),
        "--home must not fall back to $HOME/.tapectl"
    );
}

/// `TAPECTL_HOME` does the same thing, so a shell can export it once.
#[test]
fn tapectl_home_env_var_is_honoured() {
    let real_home = TempDir::new().expect("tempdir");
    let archive = TempDir::new().expect("tempdir");

    let out = Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(["init"])
        .env("HOME", real_home.path())
        .env("TAPECTL_HOME", archive.path())
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn");
    assert!(out.status.success());
    assert!(archive.path().join("tapectl.db").exists());
    assert!(!real_home.path().join(".tapectl/tapectl.db").exists());
}

/// The compatibility guarantee. `--config` alone still relocates the whole
/// home to the config file's parent — every harness relied on that, and
/// breaking it would mean a script using `--config` for isolation silently
/// starts writing to the operator's REAL archive. It warns now, but it works.
#[test]
fn config_alone_still_relocates_the_home_and_says_so() {
    let real_home = TempDir::new().expect("tempdir");
    let archive = TempDir::new().expect("tempdir");
    let cfg = archive.path().join("config.toml");

    let out = Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(["--config", cfg.to_str().unwrap(), "init"])
        .env("HOME", real_home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("TAPECTL_HOME")
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "the legacy --config behaviour must keep working: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        archive.path().join("tapectl.db").exists(),
        "--config alone must still derive the home from the config's parent"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--home"),
        "the surprising behaviour must announce itself and name the fix; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `--home` plus `--config` is unambiguous, so it must NOT warn — otherwise
/// the warning becomes noise on the very invocation that got it right.
#[test]
fn home_plus_config_does_not_warn() {
    let real_home = TempDir::new().expect("tempdir");
    let archive = TempDir::new().expect("tempdir");

    let out = Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args([
            "--home",
            archive.path().to_str().unwrap(),
            "--config",
            archive.path().join("config.toml").to_str().unwrap(),
            "init",
        ])
        .env("HOME", real_home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("TAPECTL_HOME")
        .output()
        .expect("spawn");
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("without --home"),
        "an explicit --home must not trigger the deprecation warning; stderr={stderr}"
    );
}

/// ADR-0010, "Read paths stay usable without a configured drive": the
/// machine that most needs `identify`/`verify`/`read-slices`/`restore`/
/// `catalog rebuild` is the rebuilt one that has keys and no `backend add`
/// yet (ADR-0005's DR path). An explicit `--device` must therefore be taken
/// exactly as given, with the backend treated as optional — never refused
/// for want of configuration.
///
/// The discriminator is WHICH error comes back: reaching the tape layer and
/// failing to open a nonexistent device proves the lenient resolver let the
/// path through. A "no [[backends.lto]] entry ..." error would prove the
/// strict resolver was wired to a read path.
#[test]
fn read_paths_accept_an_explicit_device_with_zero_backends_configured() {
    let home = TempDir::new().unwrap();
    let init = run_tapectl(home.path(), &["init"]);
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    const DEV: &str = "/nonexistent/tapectl-dr-path-nst";
    for args in [
        vec!["volume", "identify", "--device", DEV],
        vec!["restore", "raw-volume", "--device", DEV, "--to", "/tmp"],
    ] {
        let out = run_tapectl(home.path(), &args);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?} unexpectedly succeeded");
        assert!(
            err.contains(DEV),
            "{args:?} must fail at the DEVICE, naming it; got:\n{err}"
        );
        assert!(
            !err.contains("[[backends.lto]]") && !err.contains("no drive configured"),
            "{args:?} is a read path and must not demand a configured backend \
             (ADR-0010's DR path); got:\n{err}"
        );
    }
}

/// The other half of the same split: a WRITE path with zero backends is
/// refused by name, because it genuinely needs the drive's usable-capacity
/// factor, ENOSPC buffer and sg node.
#[test]
fn write_paths_refuse_an_unconfigured_device_by_name() {
    let home = TempDir::new().unwrap();
    let init = run_tapectl(home.path(), &["init"]);
    assert!(init.status.success());

    let out = run_tapectl(
        home.path(),
        &[
            "volume",
            "init",
            "DRGATE01",
            "--device",
            "/nonexistent/tapectl-dr-path-nst",
        ],
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "write path unexpectedly succeeded");
    assert!(
        err.contains("[[backends.lto]]"),
        "a write path with no configured backend must say so; got:\n{err}"
    );
}

/// Issue #173's whole defect, as a negative control: before the fix, EVERY
/// one of these four tests failed the same way — `tapectl` (via `main.rs`'s
/// common dispatch, which loads the config strictly before any subcommand
/// runs at all) aborted with a bare `error: failed to load config: ...` at
/// exit 2, and `config check`'s own body — the `Check` arm in
/// `cli::config::run` — never ran, `--json` included. `config check` is now
/// dispatched before that strict load and reports every problem it finds
/// via its own lenient parse path (`policy::lenient_config`).
fn config_toml_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".tapectl").join("config.toml")
}

/// Parse `stdout` as one JSON object, the way `--json config check` must
/// always produce — on a valid config as much as a broken one (issue #173
/// fix item 3: "Keep `--json` — it must emit its object on a broken config,
/// not a bare error string").
fn parse_json_stdout(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap_or_else(|e| {
        panic!(
            "config check --json must emit one JSON object, valid or not: {e}\nstdout={:?}\nstderr={:?}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        )
    })
}

#[test]
fn config_check_reports_a_misspelled_key_and_exits_nonzero() {
    let home = TempDir::new().unwrap();
    assert!(run_tapectl(home.path(), &["init"]).status.success());
    let cfg = config_toml_path(home.path());
    let original = std::fs::read_to_string(&cfg).unwrap();
    let broken = original.replacen("[defaults]\n", "[defaults]\nbadkey_typo = 1\n", 1);
    assert_ne!(
        broken, original,
        "fixture assumption broken — init no longer writes a bare [defaults] header"
    );
    std::fs::write(&cfg, &broken).unwrap();

    let human = run_tapectl(home.path(), &["config", "check"]);
    assert_eq!(
        human.status.code(),
        Some(2),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&human.stdout),
        String::from_utf8_lossy(&human.stderr)
    );
    assert!(
        String::from_utf8_lossy(&human.stdout).contains("badkey_typo"),
        "{}",
        String::from_utf8_lossy(&human.stdout)
    );

    let json_run = run_tapectl(home.path(), &["--json", "config", "check"]);
    assert_eq!(json_run.status.code(), Some(2));
    let parsed = parse_json_stdout(&json_run);
    assert_eq!(parsed["valid"], serde_json::json!(false));
    let problems = parsed["problems"]
        .as_array()
        .expect("problems must be an array");
    assert!(
        problems
            .iter()
            .any(|p| p.as_str().unwrap().contains("badkey_typo")),
        "{parsed}"
    );
}

#[test]
fn config_check_reports_a_bad_closed_set_value_and_exits_nonzero() {
    let home = TempDir::new().unwrap();
    assert!(run_tapectl(home.path(), &["init"]).status.success());
    let cfg = config_toml_path(home.path());
    let original = std::fs::read_to_string(&cfg).unwrap();
    let broken = original.replace("compression = \"none\"", "compression = \"banana\"");
    assert_ne!(
        broken, original,
        "fixture assumption broken — init no longer writes defaults.compression = \"none\""
    );
    std::fs::write(&cfg, &broken).unwrap();

    let human = run_tapectl(home.path(), &["config", "check"]);
    assert_eq!(human.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&human.stdout).contains("banana"),
        "{}",
        String::from_utf8_lossy(&human.stdout)
    );

    let json_run = run_tapectl(home.path(), &["--json", "config", "check"]);
    assert_eq!(json_run.status.code(), Some(2));
    let parsed = parse_json_stdout(&json_run);
    assert_eq!(parsed["valid"], serde_json::json!(false));
    assert!(
        parsed["problems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p.as_str().unwrap().contains("banana")),
        "{parsed}"
    );
}

/// Acceptance criterion, verbatim: "A config so malformed it is not valid
/// TOML reports that, once."
#[test]
fn config_check_on_syntactically_invalid_toml_reports_it_exactly_once() {
    let home = TempDir::new().unwrap();
    assert!(run_tapectl(home.path(), &["init"]).status.success());
    let cfg = config_toml_path(home.path());
    std::fs::write(&cfg, "this is not [ valid toml {{{\n").unwrap();

    let json_run = run_tapectl(home.path(), &["--json", "config", "check"]);
    assert_eq!(json_run.status.code(), Some(2));
    let parsed = parse_json_stdout(&json_run);
    assert_eq!(parsed["valid"], serde_json::json!(false));
    let problems = parsed["problems"].as_array().unwrap();
    assert_eq!(
        problems.len(),
        1,
        "a syntactically broken file must report its one problem once: {parsed}"
    );
}

/// The end-to-end acceptance demo (issue #173, process step 6): an unknown
/// key, a bad closed-set value, and a missing staging directory, all at
/// once, must ALL THREE be reported in a single run, with a non-zero exit
/// and a still-valid `--json` object.
#[test]
fn config_check_reports_all_three_problems_from_one_broken_config_at_once() {
    let home = TempDir::new().unwrap();
    assert!(run_tapectl(home.path(), &["init"]).status.success());
    let cfg = config_toml_path(home.path());
    let original = std::fs::read_to_string(&cfg).unwrap();

    // (a) unknown key, (b) bad closed-set value.
    let mut broken = original.replacen("[defaults]\n", "[defaults]\nbadkey_typo = 1\n", 1);
    broken = broken.replace("compression = \"none\"", "compression = \"banana\"");
    assert_ne!(broken, original, "fixture assumption broken");
    std::fs::write(&cfg, &broken).unwrap();

    // (c) missing staging directory — remove the one `init` created.
    let staging_dir = home.path().join(".tapectl").join("staging");
    assert!(
        staging_dir.is_dir(),
        "fixture assumption: init creates staging"
    );
    std::fs::remove_dir_all(&staging_dir).unwrap();

    let human = run_tapectl(home.path(), &["config", "check"]);
    assert_eq!(
        human.status.code(),
        Some(2),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&human.stdout),
        String::from_utf8_lossy(&human.stderr)
    );
    let human_text = String::from_utf8_lossy(&human.stdout);
    assert!(human_text.contains("badkey_typo"), "{human_text}");
    assert!(human_text.contains("banana"), "{human_text}");
    assert!(
        human_text.contains("staging") && human_text.contains("does not exist"),
        "{human_text}"
    );

    let json_run = run_tapectl(home.path(), &["--json", "config", "check"]);
    assert_eq!(json_run.status.code(), Some(2));
    let parsed = parse_json_stdout(&json_run);
    assert_eq!(parsed["valid"], serde_json::json!(false));
    let problems = parsed["problems"].as_array().unwrap();
    assert!(
        problems
            .iter()
            .any(|p| p.as_str().unwrap().contains("badkey_typo")),
        "{parsed}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.as_str().unwrap().contains("banana")),
        "{parsed}"
    );
    assert_eq!(
        parsed["staging"]["status"],
        serde_json::json!("missing"),
        "{parsed}"
    );
}

// --- issue #228: the startup path's four defects ------------------------
//
// Every test here spawns the real binary, because that is the only place
// `main()`'s pre-subscriber resolution and `run()`'s authoritative one both
// run. They are named with a common `issue_228_` prefix so the whole group
// runs in one `cargo test --test cli_smoke issue_228`.
//
// All five were confirmed RED against the pre-fix binary before the fix
// landed; see the issue for the recorded output.

/// Spawn the binary with a fully-scrubbed environment: an inherited
/// `TAPECTL_HOME` would mask the very rows these tests exercise, and
/// `current_dir` matters because two of the defects resolve the home
/// *relative to the working directory*.
fn spawn_scrubbed(
    cwd: &std::path::Path,
    args: &[&str],
    envs: &[(&str, &std::ffi::OsStr)],
) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tapectl"));
    cmd.args(args)
        .current_dir(cwd)
        .env_remove("HOME")
        .env_remove("TAPECTL_HOME")
        .env_remove("XDG_CONFIG_HOME");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("failed to spawn tapectl binary")
}

/// Finding 1. The notice that says "`--config` relocated your home" must not
/// be suppressible by the config file that caused the relocation.
///
/// `logging.level` filtering every other `warn!` is by design (issue #172).
/// This ONE notice is different: it is about the resolution the file itself
/// caused, and before issue #172's range it was unconditional. So it is
/// emitted outside the tracing pipeline, the way
/// `print_escrow_secret_warning` already is.
#[test]
fn issue_228_a_relocated_config_cannot_silence_its_own_relocation_notice() {
    let real_home = TempDir::new().expect("tempdir");
    let archive = TempDir::new().expect("tempdir");
    let cwd = TempDir::new().expect("tempdir");
    let cfg = archive.path().join("config.toml");

    let init = spawn_scrubbed(
        cwd.path(),
        &["--config", cfg.to_str().unwrap(), "init"],
        &[("HOME", real_home.path().as_os_str())],
    );
    assert!(
        init.status.success(),
        "init --config failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    // `init` writes a `[logging]` table with `level = "warn"`; flip it in
    // place (appending a second `[logging]` would be a TOML duplicate-key
    // error, not a config an operator could actually have).
    let written = std::fs::read_to_string(&cfg).expect("read init-written config");
    assert!(
        written.contains("level = \"warn\""),
        "init must write logging.level for this test to mean anything; config was:\n{written}"
    );
    std::fs::write(
        &cfg,
        written.replace("level = \"warn\"", "level = \"error\""),
    )
    .expect("rewrite config");

    let out = spawn_scrubbed(
        cwd.path(),
        &["--config", cfg.to_str().unwrap(), "config", "show"],
        &[("HOME", real_home.path().as_os_str())],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "config show failed: stderr={stderr}\nstdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        stderr.contains("--home"),
        "logging.level = \"error\" must not silence the notice that this very config \
         relocated the home; stderr={stderr:?}"
    );
}

/// Finding 2(a). `TAPECTL_HOME=""` is a variable a wrapper forgot to set —
/// `TAPECTL_HOME=$ARCHIVE_ROOT tapectl init` with `ARCHIVE_ROOT` unset. It
/// must fall back to `~/.tapectl`, not invent a whole new archive in
/// whatever directory the process happened to be started in.
#[test]
fn issue_228_empty_tapectl_home_falls_back_instead_of_inventing_an_archive_in_cwd() {
    let real_home = TempDir::new().expect("tempdir");
    let cwd = TempDir::new().expect("tempdir");

    let out = spawn_scrubbed(
        cwd.path(),
        &["init"],
        &[
            ("HOME", real_home.path().as_os_str()),
            ("TAPECTL_HOME", std::ffi::OsStr::new("")),
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "init failed: {stderr}");
    assert!(
        !cwd.path().join("tapectl.db").exists(),
        "an empty TAPECTL_HOME created an archive in the working directory; stderr={stderr}"
    );
    assert!(
        real_home.path().join(".tapectl/tapectl.db").exists(),
        "an empty TAPECTL_HOME must be treated as unset and fall back to ~/.tapectl; \
         stderr={stderr}"
    );
}

/// Finding 2(b). The same value passed as `--home` is a hard clap error
/// (`Error::invalid_utf8`). Via the environment it was silently discarded
/// and the REAL `~/.tapectl` used instead — the flag refuses loudly, the
/// env var failed silently into the production catalog.
#[test]
fn issue_228_non_utf8_tapectl_home_is_refused_not_silently_swapped_for_the_real_home() {
    use std::os::unix::ffi::OsStrExt;

    let real_home = TempDir::new().expect("tempdir");
    let cwd = TempDir::new().expect("tempdir");
    let bad = std::ffi::OsStr::from_bytes(b"/nonexistent/tapectl-\xff-archive");

    let out = spawn_scrubbed(
        cwd.path(),
        &["init"],
        &[
            ("HOME", real_home.path().as_os_str()),
            ("TAPECTL_HOME", bad),
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a non-UTF-8 TAPECTL_HOME must be refused, not ignored; stderr={stderr}"
    );
    assert!(
        stderr.contains("TAPECTL_HOME"),
        "the refusal must name the variable it is refusing; stderr={stderr:?}"
    );
    assert!(
        !real_home.path().join(".tapectl/tapectl.db").exists(),
        "a non-UTF-8 TAPECTL_HOME silently initialized the REAL home; stderr={stderr}"
    );
}

/// Finding 2, same class, `src/config.rs`. With `HOME` unset — cron,
/// systemd, a container — the default home was `/root/.tapectl`: a guess,
/// and the wrong archive. Refuse and name the variable instead.
#[test]
fn issue_228_unset_home_is_refused_rather_than_guessed_as_root() {
    let cwd = TempDir::new().expect("tempdir");

    let out = spawn_scrubbed(cwd.path(), &["init"], &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "an unset HOME must be refused, not guessed; stderr={stderr}"
    );
    assert!(
        stderr.contains("HOME"),
        "the refusal must name HOME (and suggest --home/TAPECTL_HOME); stderr={stderr:?}"
    );
}

/// Finding 2, the lazy half. An unset `HOME` must only be fatal when it is
/// actually what the home would be derived from — `--home`/`TAPECTL_HOME`
/// is precisely the cron/systemd invocation the refusal above exists for,
/// and it must keep working.
#[test]
fn issue_228_unset_home_is_fine_when_the_home_is_named_explicitly() {
    let archive = TempDir::new().expect("tempdir");
    let cwd = TempDir::new().expect("tempdir");

    let out = spawn_scrubbed(
        cwd.path(),
        &["--home", archive.path().to_str().unwrap(), "init"],
        &[],
    );
    assert!(
        out.status.success(),
        "--home with HOME unset must work: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(archive.path().join("tapectl.db").exists());

    let env_run = spawn_scrubbed(
        cwd.path(),
        &["--home", archive.path().to_str().unwrap(), "config", "show"],
        &[],
    );
    assert!(
        env_run.status.success(),
        "a second command with HOME unset must work too: {}",
        String::from_utf8_lossy(&env_run.stderr)
    );
}

/// Finding 4. `Path::new("config.toml").parent()` is `Some("")`, not
/// `None`, so the `unwrap_or(".")` guard never fired for the common
/// bare-relative case and the one message whose entire job is to name the
/// home rendered it as nothing at all.
///
/// The assertion is deliberately format-independent: whatever the notice
/// looks like, the text after `home=` must start with a real character.
#[test]
fn issue_228_a_bare_relative_config_names_a_non_empty_home_in_the_notice() {
    let real_home = TempDir::new().expect("tempdir");
    let archive = TempDir::new().expect("tempdir");

    let out = spawn_scrubbed(
        archive.path(),
        &["--config", "config.toml", "init"],
        &[("HOME", real_home.path().as_os_str())],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "init failed: {stderr}");

    let after = stderr
        .split("home=")
        .nth(1)
        .unwrap_or_else(|| panic!("the relocation notice must name a home; stderr={stderr:?}"));
    assert!(
        after.starts_with(|c: char| !c.is_whitespace()),
        "the notice rendered an EMPTY home for a bare-relative --config; stderr={stderr:?}"
    );

    // And the derivation itself is unchanged: the home is still the config
    // file's directory, which for a bare-relative path is the cwd.
    assert!(
        archive.path().join("tapectl.db").exists(),
        "--config must still relocate the home to the config's directory; stderr={stderr}"
    );
}

/// Finding 4's knock-on. An empty home string reached `create_dir_all("")`
/// (a documented no-op) and then `secure_path(Path::new(""), 0o700)`, whose
/// ENOENT surfaced as a second, entirely confusing warning about
/// permissions on a path the operator never named.
#[test]
fn issue_228_a_bare_relative_config_emits_no_spurious_permissions_warning() {
    let real_home = TempDir::new().expect("tempdir");
    let archive = TempDir::new().expect("tempdir");

    let out = spawn_scrubbed(
        archive.path(),
        &["--config", "config.toml", "init"],
        &[("HOME", real_home.path().as_os_str())],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "init failed: {stderr}");
    assert!(
        !stderr.contains("could not set restrictive permissions"),
        "the empty derived home produced a second, confusing warning; stderr={stderr:?}"
    );
}

/// Issue #237's investigation: `volume abort`'s `--yes`/`-y` promise against
/// the global flag (`src/cli/mod.rs`'s `Cli::yes`,
/// `#[arg(long, short, global = true)]`) versus `VolumeCommands::Abort`'s
/// own local `#[arg(long)] yes: bool`.
///
/// Like `run_tapectl`, but with stdin explicitly detached rather than
/// inherited, so `cli::consent::confirm`'s non-interactive branch is
/// exercised deterministically no matter what stdin this suite itself
/// happens to be run with.
fn run_tapectl_noninteractive(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("failed to spawn tapectl binary")
}

/// Minimal FK-satisfying fixture for `volume abort`'s Tier-2 gate: a
/// `writes` row in `planned` status is all `volume_abort`
/// (`src/volume/write.rs`) needs to reach `cli::consent::confirm` — no
/// `write_positions` row is needed (0 planned slice positions aborts fine).
/// Column set modelled on `tests/resume_session.rs::make_fixture`, which is
/// kept correct against the post-migration schema; unlike that fixture, this
/// one never drives an actual write session, so it skips the
/// `volume::build`/`session` machinery entirely. Every row is namespaced by
/// `label` so the helper can be called more than once against the same DB.
fn seed_planned_write_session(home: &std::path::Path, label: &str) {
    let db_path = home.join(".tapectl").join("tapectl.db");
    let conn = tapectl::db::open(&db_path).expect("open db to seed abort fixture");

    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES (?1, 0, 'active')",
        params![format!("tenant-{label}")],
    )
    .expect("insert tenant");
    let tenant_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO units (uuid, name, tenant_id, current_path, status)
         VALUES (?1, ?2, ?3, '/tmp/unit', 'active')",
        params![format!("uuid-{label}"), format!("unit-{label}"), tenant_id],
    )
    .expect("insert unit");
    let unit_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
         VALUES (?1, 1, 'staged', '/tmp/unit', 1, 32)",
        params![unit_id],
    )
    .expect("insert snapshot");
    let snapshot_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
        params![snapshot_id],
    )
    .expect("insert stage_set");
    let stage_set_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
        params![label],
    )
    .expect("insert volume");
    let volume_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
         VALUES (?1, ?2, ?3, 'planned')",
        params![stage_set_id, snapshot_id, volume_id],
    )
    .expect("insert writes row");
}

/// Issue #237's claim, checked against the real binary rather than only
/// against source: `tapectl --yes volume abort LABEL` — the GLOBAL `--yes`
/// given before the subcommand, no local `--yes` anywhere — was reported to
/// "fail closed" because `src/cli/volume.rs`'s `Abort` arm passes only the
/// subcommand-local flag (`write::volume_abort(conn, label, *yes)`),
/// dropping the separate global `yes: bool` that `run()` also receives.
///
/// It does not reproduce. `Abort`'s local `yes` field and the global
/// `Cli::yes` share clap's default arg id (the field name, "yes"), and
/// clap's global-value propagation unifies matches by id regardless of
/// where the flag was typed: giving `--yes` in EITHER position sets both
/// `Cli.yes` and `Abort.yes` together (confirmed independently via
/// `Cli::try_parse_from(["tapectl", "--yes", "volume", "abort", "L1"])`,
/// which yields `Abort { yes: true, .. }` with no local `--yes` token
/// anywhere). This test proves the same thing end-to-end against a fixture
/// whose `writes` row genuinely reaches the consent gate — see
/// `volume_abort_without_yes_refuses_non_interactively` below, which proves
/// that same fixture refuses when NO `--yes` is given at all, so a success
/// here cannot be explained by the gate never being reached.
///
/// This is a regression pin, not a fix verification. `src/cli/volume.rs`
/// was deliberately NOT changed for issue #237: the requested `*yes || yes`
/// would OR two values that are already always equal for the long flag — a
/// no-op — and applying it anyway despite the premise not reproducing would
/// have been "working around" a false claim rather than fixing a real one.
#[test]
fn volume_abort_proceeds_on_global_yes_alone() {
    let home = TempDir::new().expect("tempdir");
    let init = run_tapectl_noninteractive(home.path(), &["init"]);
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    seed_planned_write_session(home.path(), "ABRT-GLOBAL");

    let out = run_tapectl_noninteractive(home.path(), &["--yes", "volume", "abort", "ABRT-GLOBAL"]);
    assert!(
        out.status.success(),
        "global --yes alone should skip the prompt and proceed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The local `--yes` (given after the subcommand, no global flag anywhere)
/// must keep working — the regression this suite must not introduce.
#[test]
fn volume_abort_proceeds_on_local_yes_alone() {
    let home = TempDir::new().expect("tempdir");
    let init = run_tapectl_noninteractive(home.path(), &["init"]);
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    seed_planned_write_session(home.path(), "ABRT-LOCAL");

    let out = run_tapectl_noninteractive(home.path(), &["volume", "abort", "ABRT-LOCAL", "--yes"]);
    assert!(
        out.status.success(),
        "local --yes alone should skip the prompt and proceed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Control for the two tests above: with NEITHER flag, in a non-interactive
/// session, the same fixture must refuse rather than hang or silently
/// proceed — proving they actually exercised `cli::consent::confirm`'s
/// gate rather than some earlier, unrelated success path.
#[test]
fn volume_abort_without_yes_refuses_non_interactively() {
    let home = TempDir::new().expect("tempdir");
    let init = run_tapectl_noninteractive(home.path(), &["init"]);
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    seed_planned_write_session(home.path(), "ABRT-NEITHER");

    let out = run_tapectl_noninteractive(home.path(), &["volume", "abort", "ABRT-NEITHER"]);
    assert!(!out.status.success(), "must refuse without consent");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("refused: non-interactive"),
        "must refuse with the documented non-interactive message: {stderr}"
    );
}
