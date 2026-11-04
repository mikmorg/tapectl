//! The tape-side commands' smoke tests (volume, cartridge, drive, restore, catalog),
//! split from tests/cli_smoke.rs for 1.9.0's Wave 6 (plan §3: 6B owns this file, 6C
//! cli_smoke.rs). A test that also pins a report, audit, collection, unit, snapshot,
//! stage or status output stays in cli_smoke.rs.

use rusqlite::params;
use std::process::Command;
use tempfile::TempDir;

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

/// Issue #391 / #299 (ADR-0012 2026-10-06 item 11): `cartridge info` shows
/// the cartridge's wear — the chip's lifetime attributes and the drive's
/// page 0x17 counters, from the journal — and a WARNING only for what the
/// hardware raised. Before any reading it says so rather than staying
/// silent; after a contact whose page 0x17 counts two unrecovered read
/// errors, the human output and `--json` both carry the warning.
#[test]
fn cartridge_info_shows_wear_and_warns_on_what_the_hardware_raised() {
    let home = TempDir::new().expect("tempdir");
    assert!(run_tapectl(home.path(), &["init"]).status.success());
    let reg = run_tapectl(
        home.path(),
        &[
            "cartridge",
            "register",
            "--barcode",
            "WEAR01",
            "--generation",
            "LTO-6",
        ],
    );
    assert!(
        reg.status.success(),
        "register: {}",
        String::from_utf8_lossy(&reg.stderr)
    );

    let before = run_tapectl(home.path(), &["cartridge", "info", "WEAR01"]);
    let before = String::from_utf8_lossy(&before.stdout).to_string();
    assert!(before.contains("Wear (figures only"), "{before}");
    assert!(
        before.contains("chip: no successful MAM reading recorded")
            && before.contains("page 0x17: no reading recorded for this cartridge"),
        "no reading is said, not left silent: {before}"
    );
    assert!(!before.contains("WARNING"), "{before}");

    let db_path = home.path().join(".tapectl").join("tapectl.db");
    let conn = tapectl::db::open(&db_path).expect("open the initialized db");
    conn.execute(
        "INSERT INTO cartridge_contacts (cartridge_id, operation, device)
         SELECT id, 'volume verify', '/dev/nst0' FROM cartridges WHERE barcode = 'WEAR01'",
        [],
    )
    .unwrap();
    let contact = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO log_page_journal (contact_id, device_sg, trigger, page_code, ok, tool_argv,
             decoded, tapectl_version)
         VALUES (?1, '/dev/sg0', 'volume verify', 23, 1, '[]', ?2, 't')",
        params![
            contact,
            "Volume statistics page (ssc-4), subpage=0\n  Total write retries: 12\n  \
             Total unrecovered write errors: 0\n  Total read retries: 3\n  \
             Total unrecovered read errors: 2\n  Beginning of medium passes: 40\n"
        ],
    )
    .unwrap();
    drop(conn);

    let after = run_tapectl(home.path(), &["cartridge", "info", "WEAR01"]);
    let after = String::from_utf8_lossy(&after.stdout).to_string();
    assert!(
        after.contains("write retries 12, read retries 3"),
        "the figures are shown: {after}"
    );
    assert!(
        after.contains(
            "WARNING: worn cartridge? the drive counts 0 unrecovered write and 2 unrecovered \
             read error(s) on this cartridge (page 0x17)"
        ),
        "{after}"
    );
    let json = run_tapectl(home.path(), &["--json", "cartridge", "info", "WEAR01"]);
    let v: serde_json::Value = serde_json::from_slice(&json.stdout).expect("cartridge info --json");
    assert_eq!(
        v["wear"]["warnings"].as_array().map(Vec::len),
        Some(1),
        "{v}"
    );
}

/// Issue #361: one meaning per word. "Receipt" is the recipient list a
/// stage set was encrypted to (CONTEXT.md); `volume info`'s list of this
/// volume's `writes` rows is headed "Writes:", not "Write receipts:". The
/// JSON key was already `writes`.
#[test]
fn volume_info_heads_its_write_history_writes_not_receipts() {
    let home = TempDir::new().expect("tempdir");
    assert!(run_tapectl(home.path(), &["init"]).status.success());
    let import = run_tapectl(
        home.path(),
        &["volume", "import", "INFO-1", "--generation", "LTO-6"],
    );
    assert!(
        import.status.success(),
        "import: {}",
        String::from_utf8_lossy(&import.stderr)
    );

    let out = run_tapectl(home.path(), &["volume", "info", "INFO-1"]);
    assert!(
        out.status.success(),
        "volume info: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.lines().any(|l| l == "Writes: none"),
        "the write history is headed \"Writes:\": {stdout}"
    );
    assert!(
        !stdout.to_lowercase().contains("receipt"),
        "\"receipt\" means the recipient list, not a write: {stdout}"
    );

    let json = run_tapectl(home.path(), &["--json", "volume", "info", "INFO-1"]);
    let v: serde_json::Value = serde_json::from_slice(&json.stdout).expect("volume info --json");
    assert!(v["writes"].is_array(), "the JSON key stays `writes`: {v}");
}

/// Issue #271: `volume plan --json` on a fresh home (nothing staged) used to
/// print a plain sentence ("no staged data to plan") and exit 0 -- not JSON
/// at all, so a `json.load(...)` consumer got a parse error rather than the
/// empty document issue #56's rule requires ("every `--json` mode's whole
/// stdout must parse"). This pins the empty-case document shape: the same
/// keys the populated case emits, zeroed, with an empty `units` array.
#[test]
fn volume_plan_json_parses_with_nothing_staged() {
    let home = TempDir::new().expect("tempdir");
    let init_out = run_tapectl(home.path(), &["init"]);
    assert!(
        init_out.status.success(),
        "tapectl init failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&init_out.stdout),
        String::from_utf8_lossy(&init_out.stderr)
    );

    let out = run_tapectl(home.path(), &["volume", "plan", "--json"]);
    assert!(
        out.status.success(),
        "tapectl volume plan --json failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("volume plan --json stdout did not parse as whole JSON: {e}\n{stdout}")
    });
    // Nothing staged: no copy floor to take, and nothing to plan.
    assert_eq!(parsed["copies"], serde_json::json!(1));
    assert_eq!(parsed["copy_floor_unit"], serde_json::Value::Null);
    assert_eq!(parsed["total_slices"], serde_json::json!(0));
    assert_eq!(parsed["total_bytes"], serde_json::json!(0));
    assert_eq!(parsed["units"], serde_json::json!([]));
    // Nothing staged, nothing to put in an index: no estimate to give.
    assert_eq!(parsed["index_bytes"], serde_json::Value::Null);
    assert_eq!(parsed["index_bytes_on_tape"], serde_json::Value::Null);
}

/// ADR-0010, "Read paths stay usable without a configured drive": the
/// machine that most needs `identify`/`verify`/`read-slices`/`restore`/
/// `catalog rebuild` is the rebuilt one that has keys and no `drive add`
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
/// "fail closed" because `src/cli/volume.rs`'s `Abort` arm passes only a
/// subcommand-local flag, dropping the separate global `yes: bool` that
/// `run()` also receives.
///
/// It did not reproduce, though not for the reason first suspected. At the
/// time, `Abort` still declared its own local `yes` field
/// (`#[arg(long)] yes: bool`), and it shared clap's default arg id with the
/// global (both named "yes"): clap's global-value propagation unifies
/// matches by id regardless of where the flag was typed, so giving `--yes`
/// in EITHER position set both `Cli.yes` and `Abort.yes` together
/// (confirmed independently via
/// `Cli::try_parse_from(["tapectl", "--yes", "volume", "abort", "L1"])`,
/// which yielded `Abort { yes: true, .. }` with no local `--yes` token
/// anywhere). This test proves the same thing end-to-end against a fixture
/// whose `writes` row genuinely reaches the consent gate — see
/// `volume_abort_without_yes_refuses_non_interactively` below, which proves
/// that same fixture refuses when NO `--yes` is given at all, so a success
/// here cannot be explained by the gate never being reached.
///
/// **Issue #258's correction: the local field described above no longer
/// exists.** Issue #240 found the ONE thing the shared-id field
/// could not share -- clap's `short`, so `-y` parsed before the subcommand
/// and failed after it -- and removed the local field outright rather than
/// also redeclaring `short = 'y'` on it (`src/cli/volume.rs`'s `Abort`
/// variant, and its match arm, now read only the global `yes` parameter;
/// see that arm's own comment). This test is still a valid regression pin
/// for the ORIGINAL #237 claim ("global `--yes` before the subcommand must
/// skip the prompt"), which remains true and is now trivially so — there is
/// no second declaration left to disagree with the global. The `*yes ||
/// yes` no-op this doc used to reason about was about code that has since
/// been deleted, not code that still exists.
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

/// `--yes` given AFTER the subcommand must keep working — the regression
/// this suite must not introduce. Since issue #240 there is no local field
/// at all; this exercises the global flag in the trailing position, which is
/// where an operator most naturally types it.
#[test]
fn volume_abort_proceeds_on_trailing_yes_alone() {
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

/// Issue #240: `-y` — the GLOBAL flag's short alias — must be accepted
/// AFTER the subcommand, not only before it.
///
/// It was not. `Abort` redeclared `yes` locally as `#[arg(long)]`, sharing
/// clap's arg id with the global but not its `short`, and the local
/// declaration wins for that subcommand's parser. Measured against the real
/// binary before the fix:
///
///     tapectl -y volume abort L        -> parsed
///     tapectl volume abort L --yes     -> parsed
///     tapectl volume abort L -y        -> error: unexpected argument '-y' found
///
/// The global's help promises unconditionally to skip Tier-2 prompts, so an
/// operator who has used `-y` elsewhere hit a bare parse error with no hint
/// that the long form would have worked — and in a non-interactive session
/// the command then refused at the very gate it was told to skip.
///
/// Process-level on purpose: this is about argv parsing, which a library
/// test cannot see.
#[test]
fn volume_abort_accepts_the_short_yes_after_the_subcommand() {
    let home = TempDir::new().expect("tempdir");
    let init = run_tapectl_noninteractive(home.path(), &["init"]);
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    seed_planned_write_session(home.path(), "ABRT-SHORT");

    let out = run_tapectl_noninteractive(home.path(), &["volume", "abort", "ABRT-SHORT", "-y"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("unexpected argument"),
        "-y after the subcommand must parse, not die on argv (issue #240): {stderr}"
    );
    assert!(
        out.status.success(),
        "-y after the subcommand should skip the prompt and proceed: stdout={}\nstderr={stderr}",
        String::from_utf8_lossy(&out.stdout),
    );
}

/// The same alias in the LEADING position, so the pair pins both and a
/// future change cannot fix one by breaking the other.
#[test]
fn volume_abort_accepts_the_short_yes_before_the_subcommand() {
    let home = TempDir::new().expect("tempdir");
    let init = run_tapectl_noninteractive(home.path(), &["init"]);
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    seed_planned_write_session(home.path(), "ABRT-SHORT-LEAD");

    let out =
        run_tapectl_noninteractive(home.path(), &["-y", "volume", "abort", "ABRT-SHORT-LEAD"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("unexpected argument"),
        "-y before the subcommand must parse (issue #240): {stderr}"
    );
    assert!(
        out.status.success(),
        "-y before the subcommand should skip the prompt and proceed: stdout={}\nstderr={stderr}",
        String::from_utf8_lossy(&out.stdout),
    );
}

/// Control for the tests above: with NEITHER flag, in a non-interactive
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

/// ADR-0012 item 210: `catalog rebuild --from-volume` shows as required in
/// its usage (clap's own check replaces the runtime one), and File 1's
/// spelling, frozen for good, parses unchanged.
#[test]
fn catalog_rebuild_shows_from_volume_as_required() {
    use clap::Parser;
    let home = TempDir::new().unwrap();
    let out = run_tapectl(home.path(), &["catalog", "rebuild", "--help"]);
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    let usage = help
        .lines()
        .find(|l| l.starts_with("Usage:"))
        .unwrap_or_else(|| panic!("no usage line: {help}"));
    assert!(
        usage.contains("[OPTIONS] --from-volume --key <FILE>"),
        "--from-volume is required in the usage: {usage}"
    );

    tapectl::cli::Cli::try_parse_from([
        "tapectl",
        "catalog",
        "rebuild",
        "--from-volume",
        "--device",
        "/dev/nst0",
        "--key",
        "YOUR_KEY.age.key",
    ])
    .expect("File 1's spelling parses");
    let err = tapectl::cli::Cli::try_parse_from(["tapectl", "catalog", "rebuild", "--key", "k"])
        .unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    assert!(err.to_string().contains("--from-volume"), "{err}");
}

/// The tape-side command groups 6B's help sweep covers (ADR-0012 item 211).
const TAPE_GROUPS: &[&str] = &["volume", "cartridge", "restore", "catalog"];

/// `(path, command)` for every visible command under `cmd`, itself included.
fn visible_commands<'a>(
    cmd: &'a clap::Command,
    path: &str,
    out: &mut Vec<(String, &'a clap::Command)>,
) {
    if cmd.is_hide_set() {
        return;
    }
    out.push((path.to_string(), cmd));
    for sub in cmd.get_subcommands() {
        visible_commands(sub, &format!("{path} {}", sub.get_name()), out);
    }
}

fn tape_commands(root: &clap::Command) -> Vec<(String, &clap::Command)> {
    let mut out = Vec::new();
    for g in TAPE_GROUPS {
        let group = root
            .find_subcommand(g)
            .unwrap_or_else(|| panic!("no `{g}` group"));
        visible_commands(group, &format!("tapectl {g}"), &mut out);
    }
    out
}

/// ADR-0012 item 211: `-h` gives one line per command and flag. The first
/// paragraph of every tape-side command's and argument's doc comment (what
/// `-h` prints) is one short line; the rest is `--help`'s. And help names no
/// code paths: no module, no source file, no design document.
#[test]
fn tape_side_short_help_is_one_line_and_names_no_code_paths() {
    use clap::CommandFactory;
    let root = tapectl::cli::Cli::command();
    let commands = tape_commands(&root);
    // Positive control: the walk reaches the commands it is about.
    let names: Vec<&str> = commands.iter().map(|(p, _)| p.as_str()).collect();
    for want in [
        "tapectl volume write",
        "tapectl cartridge erase",
        "tapectl restore file",
        "tapectl catalog rebuild",
        "tapectl volume deposit add",
    ] {
        assert!(names.contains(&want), "the walk missed {want}: {names:?}");
    }
    let code = |t: &str| {
        ["cli::", "src/", "docs/", "§", ".rs", "::"]
            .iter()
            .find(|n| t.contains(*n))
            .copied()
    };
    let mut bad = Vec::new();
    for (path, cmd) in &commands {
        let mut texts: Vec<(String, String, String)> = Vec::new();
        if let Some(about) = cmd.get_about() {
            let long = cmd
                .get_long_about()
                .map(|s| s.to_string())
                .unwrap_or_default();
            texts.push((path.clone(), about.to_string(), long));
        }
        for arg in cmd.get_arguments() {
            if arg.is_hide_set() || arg.is_global_set() {
                continue;
            }
            if let Some(h) = arg.get_help() {
                let long = arg
                    .get_long_help()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                texts.push((format!("{path} --{}", arg.get_id()), h.to_string(), long));
            }
        }
        for (at, short, long) in texts {
            if short.contains('\n') || short.chars().count() > 92 {
                bad.push(format!("{at}: -h is not one short line: {short:?}"));
            }
            for t in [&short, &long] {
                if let Some(n) = code(t) {
                    bad.push(format!("{at}: names a code path ({n}): {t:?}"));
                }
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// ADR-0012 item 212: each command's `--help` states its own exit codes,
/// and the most-used tape commands carry EXAMPLES (OQ16) whose command
/// lines are real: the one copy loop among them.
#[test]
fn tape_side_help_states_exit_codes_and_examples() {
    use clap::CommandFactory;
    let root = tapectl::cli::Cli::command();
    let mut missing = Vec::new();
    for (path, cmd) in tape_commands(&root) {
        if cmd.has_subcommands() {
            continue;
        }
        let long = cmd
            .get_long_about()
            .or(cmd.get_about())
            .map(|s| s.to_string())
            .unwrap_or_default();
        if !long.contains("Exit status") {
            missing.push(path);
        }
    }
    assert!(missing.is_empty(), "no exit status in: {missing:?}");

    let home = TempDir::new().unwrap();
    for (cmd, want) in [
        (
            &["volume", "write"][..],
            "tapectl volume write L6-0003 --init --full-confirm --move-to offsite",
        ),
        (&["volume", "verify"][..], "tapectl volume verify L6-0001"),
        (
            &["cartridge", "erase"][..],
            "tapectl cartridge erase --dry-run",
        ),
        (
            &["restore", "unit"][..],
            "tapectl restore unit photos --to /srv/restore/photos",
        ),
        (
            &["restore", "file"][..],
            "tapectl restore file photos 2024/beach.jpg --to /srv/restore",
        ),
    ] {
        let mut args = cmd.to_vec();
        args.push("--help");
        let out = run_tapectl(home.path(), &args);
        assert!(out.status.success(), "{args:?}");
        let help = String::from_utf8_lossy(&out.stdout);
        assert!(help.contains("Examples:"), "{args:?}: {help}");
        assert!(help.contains(want), "{args:?} lacks `{want}`: {help}");
        // `-h` keeps to one line per item: no examples there.
        let mut short = cmd.to_vec();
        short.push("-h");
        let out = run_tapectl(home.path(), &short);
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("Examples:"),
            "{short:?}"
        );
    }

    // Every example line parses as the command it shows.
    use clap::Parser;
    for (_, cmd) in tape_commands(&root) {
        let Some(after) = cmd.get_after_long_help() else {
            continue;
        };
        for line in after.to_string().lines() {
            let line = line.trim();
            if !line.starts_with("tapectl ") {
                continue;
            }
            let argv: Vec<&str> = line.split_whitespace().collect();
            if let Err(e) = tapectl::cli::Cli::try_parse_from(&argv) {
                panic!("example `{line}` does not parse: {e}");
            }
        }
    }
}

/// ADR-0012 item 212: `volume list` and `cartridge list` are borderless,
/// aligned tables in one header case, with capacities in decimal units;
/// `volume info` shows its times in UTC and says "drive"; and the one-
/// document JSON of each family (here `volume move --json`, `cartridge
/// register --json`, `catalog stats --json`) is pretty-printed, keys
/// unchanged, with times in RFC 3339.
#[test]
fn tape_side_tables_times_and_json_follow_the_output_rules() {
    let home = TempDir::new().unwrap();
    let h = home.path();
    let ok = |args: &[&str]| {
        let out = run_tapectl(h, args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    ok(&["init"]);
    ok(&["location", "add", "shelf"]);
    let reg = ok(&[
        "--json",
        "cartridge",
        "register",
        "--barcode",
        "TBL001",
        "--generation",
        "LTO-6",
    ]);
    assert!(reg.lines().count() > 1, "pretty JSON: {reg}");
    let v: serde_json::Value = serde_json::from_str(&reg).unwrap();
    assert_eq!(v["barcode"], "TBL001", "{reg}");
    {
        let conn = tapectl::db::open(&h.join(".tapectl").join("tapectl.db")).unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes,
                                  status, created_at, sealed_at)
             VALUES ('TBL-0001', 'lto', 'lto6', 'LTO-6', 2500000000000, 'sealed',
                     '2026-09-02 10:30:59', '2026-09-03 11:00:00')",
            [],
        )
        .unwrap();
    }

    let list = ok(&["volume", "list"]);
    let header = list.lines().next().unwrap_or_default();
    assert!(
        header.starts_with("Label") && header.contains("Min Copies"),
        "{list}"
    );
    assert!(!list.contains('|') && !list.contains("+-"), "{list}");
    let row = list
        .lines()
        .find(|l| l.starts_with("TBL-0001"))
        .expect(&list);
    assert!(row.contains("2.50 TB"), "decimal capacity: {list}");

    let carts = ok(&["cartridge", "list"]);
    assert!(carts.starts_with("Barcode"), "{carts}");
    assert!(!carts.contains('|') && !carts.contains("+-"), "{carts}");

    let info = ok(&["volume", "info", "TBL-0001"]);
    assert!(info.contains("Created:     2026-09-02 10:30 UTC"), "{info}");
    assert!(info.contains("Sealed:      2026-09-03 11:00 UTC"), "{info}");
    assert!(info.contains("  Drive:       lto6 (lto)"), "{info}");

    let moved = ok(&["--json", "volume", "move", "TBL-0001", "--to", "shelf"]);
    assert!(moved.lines().count() > 1, "pretty JSON: {moved}");
    let v: serde_json::Value = serde_json::from_str(&moved).unwrap();
    assert_eq!(v["label"], "TBL-0001");
    assert_eq!(v["location"], "shelf");
    let info: serde_json::Value =
        serde_json::from_str(&ok(&["--json", "volume", "info", "TBL-0001"])).unwrap();
    assert_eq!(info["location"], "shelf");
    assert_eq!(info["status"], "sealed");
    // RFC 3339 with `Z` in JSON; the stored value is unchanged.
    assert_eq!(info["created_at"], "2026-09-02T10:30:59Z");
    assert_eq!(info["sealed_at"], "2026-09-03T11:00:00Z");

    let stats = ok(&["--json", "catalog", "stats"]);
    assert!(stats.lines().count() > 1, "pretty JSON: {stats}");
    serde_json::from_str::<serde_json::Value>(&stats).unwrap();
}
