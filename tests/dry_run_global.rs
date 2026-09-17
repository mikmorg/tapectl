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
