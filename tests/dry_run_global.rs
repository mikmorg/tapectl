//! The global `--dry-run` flag, end to end through the real binary
//! (issue #230).
//!
//! `--dry-run` is declared `global = true` on `Cli` (`src/cli/mod.rs`) with
//! an unconditional promise — "Show what would be done without making
//! changes" — so clap accepts it on EVERY subcommand. That promise was only
//! kept by the commands `main.rs` happened to thread `cli.dry_run` into, and
//! of those, the `Move` arms never read it. Four mutating commands accepted
//! the flag and mutated anyway.
//!
//! Every test here spawns the compiled binary against a throwaway `HOME`
//! (never the operator's real `~/.tapectl`) and then asserts on the DATABASE
//! ROW, not on what was printed — "it printed the word dry-run" is not
//! evidence that nothing was written. Going through the process is
//! deliberate: two of the four bugs lived in `main.rs`'s dispatch, not in
//! the command bodies, so a library-level call could not have caught them.

use std::process::Command;
use tempfile::TempDir;

fn run_tapectl(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("failed to spawn tapectl binary")
}

fn ok(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    let out = run_tapectl(home, args);
    assert!(
        out.status.success(),
        "`tapectl {}` failed ({:?})\nstdout={}\nstderr={}",
        args.join(" "),
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out
}

fn db(home: &std::path::Path) -> rusqlite::Connection {
    tapectl::db::open(&home.join(".tapectl").join("tapectl.db")).expect("open catalog")
}

/// Name of the location a row currently points at, `None` when unset.
fn location_of(
    conn: &rusqlite::Connection,
    table: &str,
    key_col: &str,
    key: &str,
) -> Option<String> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        &format!(
            "SELECT l.name FROM {table} t JOIN locations l ON l.id = t.location_id
             WHERE t.{key_col} = ?1"
        ),
        rusqlite::params![key],
        |r| r.get(0),
    )
    .optional()
    .unwrap()
}

fn count(conn: &rusqlite::Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// `init` plus two shelves and one registered cartridge carrying two
/// volumes — the multi-volume shape that makes "the cartridge is the thing
/// that moves" visible on both sides of `move_together`.
///
/// The `volumes`/`cartridge_volumes` rows are inserted directly because
/// `volume init` needs a real tape drive, which these tests must never
/// touch. Everything else goes through the CLI.
fn home_with_a_bound_cartridge() -> TempDir {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);
    ok(home.path(), &["location", "add", "home-rack"]);
    ok(home.path(), &["location", "add", "bank"]);
    ok(
        home.path(),
        &[
            "cartridge",
            "register",
            "--barcode",
            "A001L6",
            "--generation",
            "LTO-6",
        ],
    );

    let conn = db(home.path());
    for label in ["L6-0001", "L6-0002"] {
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
            rusqlite::params![label],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cartridge_volumes (cartridge_id, volume_id)
             SELECT (SELECT id FROM cartridges WHERE barcode = 'A001L6'),
                    id FROM volumes WHERE label = ?1",
            rusqlite::params![label],
        )
        .unwrap();
    }
    drop(conn);
    home
}

/// Instance 2 of issue #230. `cartridge move` routes to
/// `cli::location::move_together`, which took no `dry_run` at all: under
/// `--dry-run` it opened its transaction and rewrote `cartridges.location_id`,
/// every mounted volume's `location_id`, a `volume_movements` row per volume
/// and a `moved` event apiece.
#[test]
fn cartridge_move_dry_run_leaves_every_row_untouched() {
    let home = home_with_a_bound_cartridge();

    let out = ok(
        home.path(),
        &[
            "cartridge",
            "move",
            "A001L6",
            "--to",
            "home-rack",
            "--dry-run",
        ],
    );

    let conn = db(home.path());
    assert_eq!(
        location_of(&conn, "cartridges", "barcode", "A001L6"),
        None,
        "--dry-run moved the cartridge: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    for label in ["L6-0001", "L6-0002"] {
        assert_eq!(
            location_of(&conn, "volumes", "label", label),
            None,
            "--dry-run moved volume {label} with the cartridge"
        );
    }
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM volume_movements"),
        0,
        "--dry-run wrote movement history"
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM events WHERE action = 'moved'"),
        0,
        "--dry-run logged a move event"
    );

    // Informative, not merely silent: a dry run must name what WOULD travel.
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("A001L6") && text.contains("home-rack"),
        "dry-run output names neither the cartridge nor the destination: {text}"
    );
    assert!(
        text.contains("L6-0001") && text.contains("L6-0002"),
        "dry-run output does not name the volumes that would travel: {text}"
    );
}

/// The `--json` half of the same arm: the marker must be present and plain
/// `true`, matching `cartridge relabel`'s established shape.
#[test]
fn cartridge_move_dry_run_json_carries_the_marker() {
    let home = home_with_a_bound_cartridge();

    let out = ok(
        home.path(),
        &[
            "--json",
            "cartridge",
            "move",
            "A001L6",
            "--to",
            "home-rack",
            "--dry-run",
        ],
    );
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "cartridge move --json --dry-run must emit one JSON object: {e}\nstdout={}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    assert_eq!(parsed["dry_run"], serde_json::json!(true), "{parsed}");
    assert_eq!(parsed["barcode"], serde_json::json!("A001L6"), "{parsed}");

    let conn = db(home.path());
    assert_eq!(location_of(&conn, "cartridges", "barcode", "A001L6"), None);
}

/// Instance 3 of issue #230 — the other end of the same shared mover.
/// `volume move` names a volume, but ADR-0011 makes the plastic travel, so
/// the identical rows change; under `--dry-run` none of them may.
#[test]
fn volume_move_dry_run_leaves_every_row_untouched() {
    let home = home_with_a_bound_cartridge();

    let out = ok(
        home.path(),
        &["volume", "move", "L6-0001", "--to", "bank", "--dry-run"],
    );

    let conn = db(home.path());
    assert_eq!(
        location_of(&conn, "volumes", "label", "L6-0001"),
        None,
        "--dry-run moved the named volume: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        location_of(&conn, "volumes", "label", "L6-0002"),
        None,
        "--dry-run moved the OTHER volume on that cartridge"
    );
    assert_eq!(
        location_of(&conn, "cartridges", "barcode", "A001L6"),
        None,
        "--dry-run moved the cartridge the volume is bound to"
    );
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM volume_movements"), 0);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM events WHERE action = 'moved'"),
        0
    );

    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("L6-0001") && text.contains("bank") && text.contains("A001L6"),
        "dry-run output does not name the volume, destination and cartridge: {text}"
    );
}

/// A dry run that hides a refusal is worse than no dry run: the operator
/// would drop `--dry-run` expecting the move to work. The location lookup
/// and ADR-0011's warehouse refusal both stay AHEAD of the dry-run return.
#[test]
fn a_dry_run_move_still_refuses_an_impossible_destination() {
    let home = home_with_a_bound_cartridge();
    ok(
        home.path(),
        &[
            "location",
            "add",
            "glacier",
            "--kind",
            "warehouse",
            "--description",
            "s3://bucket/prefix",
        ],
    );

    let unknown = run_tapectl(
        home.path(),
        &[
            "cartridge",
            "move",
            "A001L6",
            "--to",
            "nowhere",
            "--dry-run",
        ],
    );
    assert!(
        !unknown.status.success(),
        "a dry run must still refuse an unknown destination"
    );

    let warehouse = run_tapectl(
        home.path(),
        &[
            "cartridge",
            "move",
            "A001L6",
            "--to",
            "glacier",
            "--dry-run",
        ],
    );
    assert!(
        !warehouse.status.success(),
        "a dry run must still refuse a warehouse destination (ADR-0011)"
    );
}

/// Instance 4 of issue #230, first half: `main.rs` never handed
/// `cli::location::run` the flag at all, so `location add --dry-run`
/// inserted the row and logged a `created` event.
#[test]
fn location_add_dry_run_inserts_nothing() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);

    let out = ok(home.path(), &["location", "add", "shed", "--dry-run"]);

    let conn = db(home.path());
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM locations WHERE name = 'shed'"),
        0,
        "--dry-run inserted the location: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM events WHERE entity_type = 'location'"
        ),
        0,
        "--dry-run logged a location event"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("shed"),
        "dry-run output does not name the location"
    );
}

/// Instance 4, second half.
#[test]
fn location_rename_dry_run_keeps_the_old_name() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);
    ok(home.path(), &["location", "add", "shed"]);

    let out = ok(
        home.path(),
        &["location", "rename", "shed", "barn", "--dry-run"],
    );

    let conn = db(home.path());
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM locations WHERE name = 'shed'"),
        1,
        "--dry-run renamed the location away: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM locations WHERE name = 'barn'"),
        0,
        "--dry-run created the new name"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM events WHERE action = 'renamed'"
        ),
        0,
        "--dry-run logged a rename event"
    );

    // A rename of a location that does not exist must still fail under
    // --dry-run: the lookup stays ahead of the early return.
    let missing = run_tapectl(
        home.path(),
        &["location", "rename", "nope", "barn", "--dry-run"],
    );
    assert!(
        !missing.status.success(),
        "a dry-run rename must still refuse an unknown location"
    );
}

// ---------------------------------------------------------------------------
// Instance 1: `collection run`
// ---------------------------------------------------------------------------

/// A home with one two-unit collection, one configured drive, and two
/// `initialized` destination volumes — everything `collection run` needs to
/// get as far as `execute_batch` and no further.
///
/// The drive's `device_tape` is a path that is not a tape device, so if the
/// dry-run gate ever regresses the run fails at the tape open instead of
/// writing somewhere real. That failure is not what this test asserts on
/// (the `stage_sets` row is), it is only a backstop: these tests must never
/// touch `/dev/nst*`.
fn home_with_a_collection_ready_to_run() -> (TempDir, TempDir) {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);
    ok(home.path(), &["tenant", "add", "media"]);

    let root = TempDir::new().unwrap();
    for name in ["alpha", "beta"] {
        let dir = root.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.dat"), vec![0u8; 64 * 1024]).unwrap();
    }

    let cfg = home.path().join(".tapectl").join("config.toml");
    let written = std::fs::read_to_string(&cfg).unwrap();
    // `init` writes a root-level `collections = []`; the array-of-tables
    // form below would be a duplicate key on top of it.
    let mut text = written.replace("collections = []\n", "");
    assert_ne!(
        text, written,
        "fixture assumption broken — init no longer writes `collections = []`"
    );
    text.push_str(&format!(
        r#"
[[backends.lto]]
name = "p"
device_tape = "{dev}"
device_sg = "{dev}"
generation = "LTO-6"
capacity_override = "10M"
usable_capacity_factor = 1.0
enospc_buffer = "0"

[[collections]]
name = "microlib"
root = "{root}"
tenant = "media"
unit_depth = 1
"#,
        dev = home.path().join("not-a-tape").display(),
        root = root.path().display(),
    ));
    std::fs::write(&cfg, &text).unwrap();

    // Register the two units for real — the dry run under test is
    // `collection run`'s, not `collection sync`'s.
    ok(home.path(), &["collection", "sync"]);

    let conn = db(home.path());
    for label in ["L1", "L2"] {
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES (?1, 'lto', 'p', 'LTO-6', 10000000, 'initialized')",
            rusqlite::params![label],
        )
        .unwrap();
    }
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM units"),
        2,
        "fixture assumption broken — collection sync registered no units"
    );
    drop(conn);
    (home, root)
}

/// Instance 1 of issue #230, and the worst of the four: `main.rs` never
/// handed `cli::collection::run` the global flag, so `collection run
/// --dry-run` staged an entire batch (dar + age, hours and a tape's worth of
/// staging disk) and then WROTE A REAL TAPE. ADR-0003 makes a sealed volume
/// immutable, so the cartridge is consumed and nothing about it is
/// reversible.
///
/// WHAT THIS COVERS: that the run returns before `collection::batch::
/// execute_batch` — asserted as "no `stage_sets` row exists", `execute_batch`
/// staging being the first thing it does and the first row it writes. It
/// also pins that the dry run stays informative: the budget line, the chosen
/// batch's units and the destination labels.
///
/// WHAT IT DOES NOT COVER: the tape write itself. Reaching `Store::execute`
/// needs a real (or mhvtl) drive, which the ungated suite must never touch,
/// so the tape half of the promise is proved transitively — staging strictly
/// precedes it in `execute_batch`, so a run that never stages never writes.
#[test]
fn collection_run_dry_run_stages_nothing_and_writes_no_tape() {
    let (home, _root) = home_with_a_collection_ready_to_run();

    let out = ok(
        home.path(),
        &[
            "collection",
            "run",
            "--collection",
            "microlib",
            "--batch",
            "0",
            "--label",
            "L1",
            "--label",
            "L2",
            "--dry-run",
        ],
    );

    let conn = db(home.path());
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM stage_sets"),
        0,
        "--dry-run staged a batch: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM writes"),
        0,
        "--dry-run recorded a write"
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM snapshots"),
        0,
        "--dry-run snapshotted a unit"
    );

    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("budget"),
        "dry run dropped the budget line: {text}"
    );
    assert!(
        text.contains("microlib/alpha") && text.contains("microlib/beta"),
        "dry run does not name the units in the chosen batch: {text}"
    );
    assert!(
        text.contains("L1") && text.contains("L2"),
        "dry run does not name the destination labels: {text}"
    );
}

/// The `--json` half, and the one field a scripted caller needs to tell a
/// rehearsal from a real tape write.
#[test]
fn collection_run_dry_run_json_carries_the_marker() {
    let (home, _root) = home_with_a_collection_ready_to_run();

    let out = ok(
        home.path(),
        &[
            "--json",
            "collection",
            "run",
            "--collection",
            "microlib",
            "--label",
            "L1",
            "--dry-run",
        ],
    );
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "collection run --json --dry-run must emit one JSON object: {e}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        )
    });
    assert_eq!(parsed["dry_run"], serde_json::json!(true), "{parsed}");
    assert_eq!(
        parsed["collection"],
        serde_json::json!("microlib"),
        "{parsed}"
    );

    let conn = db(home.path());
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM stage_sets"), 0);
}

/// A dry run must still refuse what the real run would refuse — an unknown
/// `--label` here — rather than report a plan the operator cannot execute.
/// `plan_for_run` resolves the destination budget before planning anything,
/// so that refusal already sits ahead of the dry-run return.
#[test]
fn a_dry_run_collection_run_still_refuses_an_unknown_label() {
    let (home, _root) = home_with_a_collection_ready_to_run();

    let out = run_tapectl(
        home.path(),
        &[
            "collection",
            "run",
            "--collection",
            "microlib",
            "--label",
            "NOSUCH",
            "--dry-run",
        ],
    );
    assert!(
        !out.status.success(),
        "a dry run must still refuse an unknown destination label"
    );
}

/// `locations.name` is `NOT NULL UNIQUE` (001_initial.sql), so a real
/// `location add` of a name already taken fails on the constraint. The
/// dry-run branch must reach the same verdict, for the reason the move and
/// rename gates are placed where they are: a dry run that hides a refusal
/// is worse than none, because the operator drops the flag expecting it to
/// work.
#[test]
fn a_dry_run_location_add_still_refuses_a_name_already_taken() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);
    ok(home.path(), &["location", "add", "shed"]);

    let real = run_tapectl(home.path(), &["location", "add", "shed"]);
    assert!(
        !real.status.success(),
        "fixture assumption broken — a duplicate location name is no longer refused"
    );

    let dry = run_tapectl(home.path(), &["location", "add", "shed", "--dry-run"]);
    assert!(
        !dry.status.success(),
        "a dry run reported a name would be added that the real run refuses:\nstdout={}",
        String::from_utf8_lossy(&dry.stdout)
    );
}

/// `collection sync` declares its OWN `--dry-run` alongside the global one.
///
/// This test was written GREEN and is a characterisation, not a regression
/// guard for a bug that existed: both args carry the clap id `dry_run`, so
/// clap propagates the global value into the subcommand's own field, and
/// `tapectl --dry-run collection sync` was already a dry run before issue
/// #230 even though `main.rs` passed the flag nowhere. That was measured by
/// reverting the `*dry_run || global_dry_run` in `cli::collection::run` and
/// watching this test still pass. It is pinned because the behaviour rests
/// on two fields happening to share an id — rename either and the two
/// spellings would silently diverge, with no other test noticing.
#[test]
fn a_global_dry_run_before_collection_sync_registers_nothing() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);
    ok(home.path(), &["tenant", "add", "media"]);

    let root = TempDir::new().unwrap();
    std::fs::create_dir_all(root.path().join("alpha")).unwrap();
    std::fs::write(root.path().join("alpha").join("f.dat"), b"hello").unwrap();

    let cfg = home.path().join(".tapectl").join("config.toml");
    let written = std::fs::read_to_string(&cfg).unwrap();
    let mut text = written.replace("collections = []\n", "");
    assert_ne!(text, written, "fixture assumption broken");
    text.push_str(&format!(
        "\n[[collections]]\nname = \"microlib\"\nroot = \"{}\"\ntenant = \"media\"\nunit_depth = 1\n",
        root.path().display()
    ));
    std::fs::write(&cfg, &text).unwrap();

    // The flag in GLOBAL position, ahead of the subcommand — the spelling
    // clap accepts because `--dry-run` is `global = true`.
    let out = ok(home.path(), &["--dry-run", "collection", "sync"]);

    let conn = db(home.path());
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM units"),
        0,
        "a global --dry-run registered a unit anyway: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !root
            .path()
            .join("alpha")
            .join(".tapectl-unit.toml")
            .exists(),
        "a global --dry-run wrote a dotfile into the source tree"
    );
}
