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
/// batch's units and the destination label.
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
        text.contains("L1"),
        "dry run does not name the destination label: {text}"
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

// ---------------------------------------------------------------------------
// Issue #241 — the class-wide sweep. Each test below proves one
// `Verdict::Honours` entry from `tests/dry_run_contract.rs`'s TABLE actually
// leaves every row untouched, the same DB-row discipline as the #230 tests
// above. `tests/dry_run_contract.rs` proves the CLASS (every leaf has a
// verdict, every `Refuses` leaf actually refuses); these prove the CONTENT
// of specific `Honours` verdicts, which a static contract test cannot see.
// ---------------------------------------------------------------------------

/// `cartridge register` (issue #241's own easiest-fix example): every
/// refusal (bad generation, duplicate barcode/serial) already ran before
/// the dry-run branch, so this only needs the happy path.
#[test]
fn cartridge_register_dry_run_inserts_nothing() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);

    let out = ok(
        home.path(),
        &[
            "cartridge",
            "register",
            "--barcode",
            "A100L6",
            "--generation",
            "LTO-6",
            "--dry-run",
        ],
    );

    let conn = db(home.path());
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM cartridges WHERE barcode = 'A100L6'"
        ),
        0,
        "--dry-run registered the cartridge: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("A100L6"),
        "dry-run output does not name the cartridge"
    );

    // A dry run must still refuse a barcode already taken.
    ok(
        home.path(),
        &[
            "cartridge",
            "register",
            "--barcode",
            "A100L6",
            "--generation",
            "LTO-6",
        ],
    );
    let dup = run_tapectl(
        home.path(),
        &[
            "cartridge",
            "register",
            "--barcode",
            "A100L6",
            "--generation",
            "LTO-6",
            "--dry-run",
        ],
    );
    assert!(
        !dup.status.success(),
        "a dry run must still refuse a barcode already taken"
    );
}

/// `tenant add`: the interesting side effects (two key files on disk, two
/// `encryption_keys` rows) must not happen under `--dry-run`.
#[test]
fn tenant_add_dry_run_creates_nothing() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);

    let out = ok(home.path(), &["tenant", "add", "acme", "--dry-run"]);

    let conn = db(home.path());
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM tenants WHERE name = 'acme'"),
        0,
        "--dry-run created the tenant: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM encryption_keys e JOIN tenants t ON t.id = e.tenant_id
             WHERE t.name = 'acme'"
        ),
        0,
        "--dry-run generated a keypair (the operator's own keys from `init` are excluded \
         by this query, on purpose)"
    );
    // `init` itself already populates `keys/` with the operator tenant's
    // own primary/backup files, so the directory is not expected to be
    // empty — only free of anything named for "acme".
    let acme_key_files: Vec<_> = std::fs::read_dir(home.path().join(".tapectl").join("keys"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("acme-"))
        .collect();
    assert!(
        acme_key_files.is_empty(),
        "--dry-run wrote a key file to disk: {acme_key_files:?}"
    );

    // A dry run must still refuse a name already taken.
    ok(home.path(), &["tenant", "add", "acme"]);
    let dup = run_tapectl(home.path(), &["tenant", "add", "acme", "--dry-run"]);
    assert!(
        !dup.status.success(),
        "a dry run must still refuse a tenant name already taken"
    );
}

/// `tenant delete`: the soft-delete UPDATE and its event must not run.
#[test]
fn tenant_delete_dry_run_leaves_the_tenant_active() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);
    ok(home.path(), &["tenant", "add", "acme"]);

    ok(home.path(), &["tenant", "delete", "acme", "--dry-run"]);

    let conn = db(home.path());
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM tenants WHERE name = 'acme' AND status = 'active'"
        ),
        1,
        "--dry-run deleted the tenant"
    );
}

/// `tenant reassign`: no unit's `tenant_id` may move.
#[test]
fn tenant_reassign_dry_run_moves_nothing() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);
    ok(home.path(), &["tenant", "add", "acme"]);
    ok(home.path(), &["tenant", "add", "other"]);

    let root = TempDir::new().unwrap();
    let unit_dir = root.path().join("u1");
    std::fs::create_dir_all(&unit_dir).unwrap();
    ok(
        home.path(),
        &[
            "unit",
            "init",
            unit_dir.to_str().unwrap(),
            "--tenant",
            "acme",
            "--name",
            "u1",
        ],
    );

    let out = ok(
        home.path(),
        &["tenant", "reassign", "acme", "--to", "other", "--dry-run"],
    );

    let conn = db(home.path());
    let tenant_name: String = conn
        .query_row(
            "SELECT t.name FROM units u JOIN tenants t ON t.id = u.tenant_id
             WHERE u.name = 'u1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tenant_name, "acme", "--dry-run reassigned the unit");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains('1'),
        "dry-run output does not name how many unit(s) would move"
    );
}

/// `archive-set create`: no row, no event.
#[test]
fn archive_set_create_dry_run_inserts_nothing() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);

    ok(
        home.path(),
        &[
            "archive-set",
            "create",
            "cold",
            "--min-copies",
            "2",
            "--dry-run",
        ],
    );

    let conn = db(home.path());
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM archive_sets WHERE name = 'cold'"
        ),
        0,
        "--dry-run created the archive set"
    );

    // A dry run must still refuse a name already taken.
    ok(home.path(), &["archive-set", "create", "cold"]);
    let dup = run_tapectl(home.path(), &["archive-set", "create", "cold", "--dry-run"]);
    assert!(
        !dup.status.success(),
        "a dry run must still refuse an archive-set name already taken"
    );
}

/// `archive-set edit`: no field may change.
#[test]
fn archive_set_edit_dry_run_changes_nothing() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);
    ok(
        home.path(),
        &["archive-set", "create", "cold", "--min-copies", "2"],
    );

    ok(
        home.path(),
        &[
            "archive-set",
            "edit",
            "cold",
            "--min-copies",
            "5",
            "--dry-run",
        ],
    );

    let conn = db(home.path());
    let min_copies: i64 = conn
        .query_row(
            "SELECT min_copies FROM archive_sets WHERE name = 'cold'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(min_copies, 2, "--dry-run changed min_copies");
}

/// `backend add`: `config.toml` must be byte-for-byte unchanged.
#[test]
fn backend_add_dry_run_does_not_touch_config_file() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);
    let cfg_path = home.path().join(".tapectl").join("config.toml");
    let before = std::fs::read_to_string(&cfg_path).unwrap();

    ok(
        home.path(),
        &[
            "backend",
            "add",
            "--name",
            "p1",
            "--device-tape",
            "/dev/tapectl-dry-run-contract-nonexistent",
            "--device-sg",
            "/dev/tapectl-dry-run-contract-nonexistent-sg",
            "--generation",
            "LTO-6",
            "--dry-run",
        ],
    );

    let after = std::fs::read_to_string(&cfg_path).unwrap();
    assert_eq!(before, after, "--dry-run modified config.toml");
}

/// `unit tag`: the tag set must not change.
#[test]
fn unit_tag_dry_run_changes_nothing() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);
    ok(home.path(), &["tenant", "add", "acme"]);
    let root = TempDir::new().unwrap();
    let unit_dir = root.path().join("u1");
    std::fs::create_dir_all(&unit_dir).unwrap();
    ok(
        home.path(),
        &[
            "unit",
            "init",
            unit_dir.to_str().unwrap(),
            "--tenant",
            "acme",
            "--name",
            "u1",
        ],
    );

    let out = ok(
        home.path(),
        &["unit", "tag", "u1", "--add", "hot", "--dry-run"],
    );

    let conn = db(home.path());
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM unit_tags t JOIN units u ON u.id = t.unit_id
             WHERE u.name = 'u1'"
        ),
        0,
        "--dry-run added a tag: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// `unit rename`: the old name must survive.
#[test]
fn unit_rename_dry_run_keeps_the_old_name() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);
    ok(home.path(), &["tenant", "add", "acme"]);
    let root = TempDir::new().unwrap();
    let unit_dir = root.path().join("u1");
    std::fs::create_dir_all(&unit_dir).unwrap();
    ok(
        home.path(),
        &[
            "unit",
            "init",
            unit_dir.to_str().unwrap(),
            "--tenant",
            "acme",
            "--name",
            "u1",
        ],
    );

    ok(home.path(), &["unit", "rename", "u1", "u2", "--dry-run"]);

    let conn = db(home.path());
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM units WHERE name = 'u1'"),
        1
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM units WHERE name = 'u2'"),
        0
    );

    // A dry run must still refuse a name already taken.
    std::fs::create_dir_all(root.path().join("u3")).unwrap();
    ok(
        home.path(),
        &[
            "unit",
            "init",
            root.path().join("u3").to_str().unwrap(),
            "--tenant",
            "acme",
            "--name",
            "u3",
        ],
    );
    let dup = run_tapectl(home.path(), &["unit", "rename", "u1", "u3", "--dry-run"]);
    assert!(
        !dup.status.success(),
        "a dry run must still refuse a unit name already taken"
    );
}

/// `snapshot delete`: the row must survive.
#[test]
fn snapshot_delete_dry_run_keeps_the_snapshot() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);
    ok(home.path(), &["tenant", "add", "acme"]);
    let root = TempDir::new().unwrap();
    let unit_dir = root.path().join("u1");
    std::fs::create_dir_all(&unit_dir).unwrap();
    std::fs::write(unit_dir.join("f.txt"), b"hello").unwrap();
    ok(
        home.path(),
        &[
            "unit",
            "init",
            unit_dir.to_str().unwrap(),
            "--tenant",
            "acme",
            "--name",
            "u1",
        ],
    );
    ok(home.path(), &["snapshot", "create", "u1"]);

    ok(
        home.path(),
        &["snapshot", "delete", "u1", "--version", "1", "--dry-run"],
    );

    let conn = db(home.path());
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM snapshots WHERE version = 1"),
        1,
        "--dry-run deleted the snapshot"
    );
}

/// `snapshot purge`: same shape as `delete` above, against a `reclaimable`
/// snapshot the real `purge` requires.
#[test]
fn snapshot_purge_dry_run_keeps_the_row() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);
    ok(home.path(), &["tenant", "add", "acme"]);
    let root = TempDir::new().unwrap();
    let unit_dir = root.path().join("u1");
    std::fs::create_dir_all(&unit_dir).unwrap();
    std::fs::write(unit_dir.join("f.txt"), b"hello").unwrap();
    ok(
        home.path(),
        &[
            "unit",
            "init",
            unit_dir.to_str().unwrap(),
            "--tenant",
            "acme",
            "--name",
            "u1",
        ],
    );
    ok(home.path(), &["snapshot", "create", "u1"]);
    {
        let conn = db(home.path());
        conn.execute(
            "UPDATE snapshots SET status = 'reclaimable' WHERE version = 1",
            [],
        )
        .unwrap();
    }

    ok(
        home.path(),
        &["snapshot", "purge", "u1", "--version", "1", "--dry-run"],
    );

    let conn = db(home.path());
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM snapshots WHERE version = 1 AND status = 'reclaimable'"
        ),
        1,
        "--dry-run purged the snapshot"
    );
}

/// `volume deposit add`/`remove`: neither may write or erase a row.
#[test]
fn volume_deposit_add_and_remove_dry_run_change_nothing() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);
    ok(
        home.path(),
        &["location", "add", "glacier", "--kind", "warehouse"],
    );
    let conn = db(home.path());
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                              capacity_bytes, status)
         VALUES ('L6-0001', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
        [],
    )
    .unwrap();
    drop(conn);

    ok(
        home.path(),
        &[
            "volume",
            "deposit",
            "add",
            "L6-0001",
            "--to",
            "glacier",
            "--dry-run",
        ],
    );
    let conn = db(home.path());
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM volume_deposits"),
        0,
        "--dry-run recorded a deposit"
    );
    drop(conn);

    ok(
        home.path(),
        &["volume", "deposit", "add", "L6-0001", "--to", "glacier"],
    );
    ok(
        home.path(),
        &[
            "volume",
            "deposit",
            "remove",
            "L6-0001",
            "--from",
            "glacier",
            "--dry-run",
        ],
    );
    let conn = db(home.path());
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM volume_deposits"),
        1,
        "--dry-run removed the deposit"
    );
}

/// `key import` (non-`--escrow`): no key row, no `.age.pub` file.
#[test]
fn key_import_dry_run_inserts_nothing() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);
    ok(home.path(), &["tenant", "add", "acme"]);
    let keyfile = home.path().join("imported.pub");
    std::fs::write(
        &keyfile,
        "age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq\n",
    )
    .unwrap();

    ok(
        home.path(),
        &[
            "key",
            "import",
            "--tenant",
            "acme",
            "--alias",
            "imported",
            keyfile.to_str().unwrap(),
            "--dry-run",
        ],
    );

    let conn = db(home.path());
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM encryption_keys WHERE alias = 'acme-imported'"
        ),
        0,
        "--dry-run imported the key"
    );
    assert!(
        !home
            .path()
            .join(".tapectl")
            .join("keys")
            .join("acme-imported.age.pub")
            .exists(),
        "--dry-run wrote the public key file"
    );
}

/// `restore unit`'s LOCAL `dry_run` field shares clap's arg id with the
/// GLOBAL `--dry-run` (both fields are literally named `dry_run`), so clap
/// unifies them by id and the global flag reaches it with no extra
/// plumbing in `cli::restore::run` — the same mechanism
/// `a_global_dry_run_before_collection_sync_registers_nothing` above pins
/// for `collection sync`. This is a characterisation, not a regression
/// guard for a bug that existed.
///
/// Fabricates the minimal write-position chain `restore_unit`'s query
/// joins across (`write_positions` -> `writes` -> `stage_slices` ->
/// `stage_sets` -> `snapshots` -> `volumes`) directly, the same way
/// `home_with_a_bound_cartridge` above fabricates `volumes`/
/// `cartridge_volumes` rather than running a real `volume write` — a real
/// write needs a tape this suite must never touch, and `restore_unit`'s
/// dry branch returns before opening one anyway, so nothing but the
/// catalog rows it reads is exercised.
#[test]
fn restore_unit_dry_run_reports_a_preview_via_the_shared_arg_id() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);
    ok(home.path(), &["tenant", "add", "acme"]);
    let root = TempDir::new().unwrap();
    let unit_dir = root.path().join("u1");
    std::fs::create_dir_all(&unit_dir).unwrap();
    std::fs::write(unit_dir.join("f.txt"), b"hello").unwrap();
    ok(
        home.path(),
        &[
            "unit",
            "init",
            unit_dir.to_str().unwrap(),
            "--tenant",
            "acme",
            "--name",
            "u1",
        ],
    );
    ok(home.path(), &["snapshot", "create", "u1"]);

    let conn = db(home.path());
    let unit_id: i64 = conn
        .query_row("SELECT id FROM units WHERE name = 'u1'", [], |r| r.get(0))
        .unwrap();
    let snapshot_id: i64 = conn
        .query_row(
            "SELECT id FROM snapshots WHERE unit_id = ?1 AND version = 1",
            rusqlite::params![unit_id],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                              capacity_bytes, status)
         VALUES ('L6-0001', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
        [],
    )
    .unwrap();
    let volume_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
        rusqlite::params![snapshot_id],
    )
    .unwrap();
    let stage_set_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                    sha256_plain, sha256_encrypted)
         VALUES (?1, 1, 100, 116, ?2, ?3)",
        rusqlite::params![stage_set_id, "a".repeat(64), "b".repeat(64)],
    )
    .unwrap();
    let stage_slice_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
         VALUES (?1, ?2, ?3, 'completed')",
        rusqlite::params![stage_set_id, snapshot_id, volume_id],
    )
    .unwrap();
    let write_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO write_positions (write_id, stage_slice_id, position, status)
         VALUES (?1, ?2, '0', 'written')",
        rusqlite::params![write_id, stage_slice_id],
    )
    .unwrap();
    drop(conn);

    // `--device` is required here for a reason unrelated to this test:
    // `restore` resolves its device LENIENTLY (`cli::read_device`, ADR-0005's
    // DR path) but still needs SOME value when no `[[backends.lto]]` is
    // configured, which this home never does (this suite must never touch
    // `/dev/nst*`). The value need not exist — `restore_unit`'s dry branch
    // returns before any device is opened.
    //
    // The GLOBAL flag in front position — the spelling clap accepts
    // because `--dry-run` is `global = true`, same as the `collection
    // sync` precedent — must still reach `restore unit`'s own local field.
    let out = ok(
        home.path(),
        &[
            "--dry-run",
            "restore",
            "unit",
            "--unit",
            "u1",
            "--from",
            "L6-0001",
            "--to",
            root.path().join("out").to_str().unwrap(),
            "--device",
            "/dev/tapectl-dry-run-contract-nonexistent",
        ],
    );

    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("would restore") && text.contains('1'),
        "dry-run output does not name the preview or the slice count: {text}"
    );
    assert!(
        !root.path().join("out").exists(),
        "--dry-run created the destination directory or restored into it"
    );
}

// ---------------------------------------------------------------------------
// Issue #247 — the ten leaves fenced off from #241 (`src/cli/operations.rs`
// and `src/cli/db.rs`, both assigned to the concurrently-running issue #233).
// Same DB-row/filesystem discipline as the #230/#241 tests above.
// ---------------------------------------------------------------------------

/// `db backup`: no destination file, and no `<dest>.keys` sidecar directory
/// either. `rusqlite::Connection::open` creates its target file the instant
/// it is called — even before any table is written — so the pre-fix defect
/// here is not "wrote a wrong backup", it is "created a zero-byte one".
#[test]
fn db_backup_dry_run_creates_no_file() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);

    let dest = home.path().join("backup.db");
    let out = ok(
        home.path(),
        &["db", "backup", "--to", dest.to_str().unwrap(), "--dry-run"],
    );
    assert!(
        !dest.exists(),
        "--dry-run created the backup file: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(dest.to_str().unwrap()),
        "dry-run output does not name the destination"
    );

    let dest_wk = home.path().join("backup-with-keys.db");
    ok(
        home.path(),
        &[
            "db",
            "backup",
            "--to",
            dest_wk.to_str().unwrap(),
            "--include-keys",
            "--dry-run",
        ],
    );
    assert!(
        !dest_wk.exists(),
        "--dry-run (--include-keys) created the backup file"
    );
    assert!(
        !dest_wk.with_extension("keys").exists(),
        "--dry-run (--include-keys) created the keys backup directory"
    );
}

/// `db fsck --repair --dry-run`: the violation report still names the
/// orphan, but the row must survive — this is the "genuinely useful
/// preview" issue #247 calls out, not a repair with the DELETE skipped
/// silently. `fsck`'s exit code is 1 (warning) whenever issues are found,
/// dry run or not (see `cli::db::fsck_exit_code`), so this uses
/// `run_tapectl` directly rather than `ok()`, which demands a 0.
#[test]
fn db_fsck_repair_dry_run_does_not_repair() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);

    // Plant one orphan the way issue #104's/#177's own fsck fixtures do:
    // FK enforcement OFF for the insert, back ON afterward (re-enabling the
    // pragma does not retroactively validate rows already there).
    {
        let conn = db(home.path());
        conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, source_path)
             VALUES (99999, 1, '/nonexistent')",
            [],
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
    }

    let out = run_tapectl(home.path(), &["db", "fsck", "--repair", "--dry-run"]);
    assert!(
        matches!(out.status.code(), Some(0) | Some(1)),
        "db fsck --repair --dry-run on a database with one warning-level finding must exit \
         0 or 1: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("snapshots") && text.contains("units"),
        "dry-run fsck did not report the planted orphan: {text}"
    );
    assert!(
        !text.contains("repaired=1"),
        "a dry run must not claim to have repaired anything: {text}"
    );

    let conn = db(home.path());
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM snapshots WHERE unit_id = 99999"
        ),
        1,
        "--dry-run deleted the orphan row"
    );
}

/// `db fsck --repair --dry-run` must still be REACHABLE on a database
/// ordinary `db::open` refuses (issue #233's `DatabaseNeedsRepair` gate in
/// `main.rs` matches on the command SHAPE `Fsck { repair: true }`, not on
/// `--dry-run`) — a preview that only works on an already-healthy database
/// is useless for the exact case the repair path exists to serve. Builds
/// the identical fixture `tests/cli_smoke.rs`'s
/// `issue_233_db_fsck_repair_can_fix_a_database_ordinary_commands_refuse_to_open`
/// uses: a database stopped two migrations short of head, carrying one
/// dangling `units.tenant_id`.
#[test]
fn db_fsck_repair_dry_run_is_reachable_on_a_database_ordinary_open_refuses() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);

    let db_path = home.path().join(".tapectl").join("tapectl.db");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db_path.display()));
    }

    {
        use rusqlite_migration::{Migrations, M};
        let mut conn = rusqlite::Connection::open(&db_path).expect("open fixture db");
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        Migrations::new(vec![
            M::up(include_str!("../src/db/migrations/001_initial.sql")),
            M::up(include_str!("../src/db/migrations/002_fts5_catalog.sql")),
        ])
        .to_latest(&mut conn)
        .expect("migrate fixture db to 002");

        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('u-orphan', 'orphan-unit', 99999, 'mtime_size', 1, 'active')",
            [],
        )
        .expect("plant the orphan row");
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    }

    let out = run_tapectl(home.path(), &["db", "fsck", "--repair", "--dry-run"]);
    assert!(
        matches!(out.status.code(), Some(0) | Some(1)),
        "a dry run must still be reachable on a database ordinary `db::open` refuses: \
         stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains("repaired=1"),
        "a dry run must not repair even on the orphan-blocked path: {text}"
    );

    // Bypass `tapectl::db::open` deliberately — this database is still two
    // migrations short of head, and re-opening it the ordinary way could
    // itself run `migrate()`, which is exactly what this fixture must NOT
    // have happened.
    let raw = rusqlite::Connection::open(&db_path).expect("reopen fixture db raw");
    let n: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM units WHERE uuid = 'u-orphan'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        n, 1,
        "a dry run repaired (deleted) the orphan row it should only have previewed"
    );
}

/// `db import`: this one was already correct before issue #247 —
/// `cli::db::run` forwarded `dry_run` to `operations::db_import`, which
/// reports a preview and returns before any consent prompt or
/// `Connection::open`. It was fenced alongside its siblings only because
/// `src/cli/db.rs` as a whole sat outside issue #241's file scope. This is
/// a characterisation test (green from the first run), not a regression
/// guard for a bug that existed — the same status
/// `a_global_dry_run_before_collection_sync_registers_nothing` above notes
/// for `collection sync`.
#[test]
fn db_import_dry_run_does_not_overwrite() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);
    ok(home.path(), &["tenant", "add", "acme"]);

    let export_path = home.path().join("exported.db");
    ok(
        home.path(),
        &["db", "backup", "--to", export_path.to_str().unwrap()],
    );

    // Added AFTER the export, so it exists only in the live database — the
    // one row that proves an import did or did not actually run.
    ok(home.path(), &["tenant", "add", "post-export"]);

    ok(
        home.path(),
        &["db", "import", export_path.to_str().unwrap(), "--dry-run"],
    );

    let conn = db(home.path());
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM tenants WHERE name = 'post-export'"
        ),
        1,
        "--dry-run overwrote the live database with the imported one"
    );
}

/// Top-level `export` (`main.rs`'s `Commands::Export` -> `operations::
/// export_unit`): no destination directory, no MANIFEST.toml/SHA256SUMS/
/// RECOVERY.md. The `stage_sets`/`stage_slices` chain is fabricated
/// directly, the same way `restore_unit_dry_run_reports_a_preview_via_the_
/// shared_arg_id` above fabricates its write-position chain — a real `dar`
/// stage is not needed because the dry branch returns before the encrypted
/// slice at `staging_path` is ever opened.
#[test]
fn export_dry_run_creates_no_destination_directory() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);
    ok(home.path(), &["tenant", "add", "acme"]);
    let root = TempDir::new().unwrap();
    let unit_dir = root.path().join("u1");
    std::fs::create_dir_all(&unit_dir).unwrap();
    ok(
        home.path(),
        &[
            "unit",
            "init",
            unit_dir.to_str().unwrap(),
            "--tenant",
            "acme",
            "--name",
            "u1",
        ],
    );

    let conn = db(home.path());
    let unit_id: i64 = conn
        .query_row("SELECT id FROM units WHERE name = 'u1'", [], |r| r.get(0))
        .unwrap();
    conn.execute(
        "INSERT INTO snapshots (unit_id, version, source_path) VALUES (?1, 1, ?2)",
        rusqlite::params![unit_id, unit_dir.to_str().unwrap()],
    )
    .unwrap();
    let snapshot_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
        rusqlite::params![snapshot_id],
    )
    .unwrap();
    let stage_set_id = conn.last_insert_rowid();
    // A REAL file, not a fake path: pre-fix (dry_run ignored), `export_unit`
    // reaches its `fs::copy` unconditionally, and a nonexistent source would
    // fail that copy with an unrelated I/O error instead of demonstrating
    // the actual defect (a real destination directory and its contents
    // getting written under `--dry-run`).
    let slice_path = root.path().join("slice.1.dar.age");
    std::fs::write(&slice_path, vec![0u8; 116]).unwrap();
    conn.execute(
        "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                    sha256_plain, sha256_encrypted, staging_path)
         VALUES (?1, 1, 100, 116, ?2, ?3, ?4)",
        rusqlite::params![
            stage_set_id,
            "a".repeat(64),
            "b".repeat(64),
            slice_path.to_str().unwrap()
        ],
    )
    .unwrap();
    drop(conn);

    let dest = root.path().join("export-out");
    let out = ok(
        home.path(),
        &[
            "export",
            "--unit",
            "u1",
            "--to",
            dest.to_str().unwrap(),
            "--dry-run",
        ],
    );

    assert!(
        !dest.exists(),
        "--dry-run created the export destination directory: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("u1"),
        "dry-run output does not name the unit: {text}"
    );
}

/// Top-level `import` (`main.rs`'s `Commands::Import` -> `operations::
/// volume_import`): no `volumes` row.
#[test]
fn import_dry_run_inserts_no_volume() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);

    let out = ok(
        home.path(),
        &[
            "import",
            "--label",
            "VOL-IMP",
            "--generation",
            "LTO-6",
            "--dry-run",
        ],
    );

    let conn = db(home.path());
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM volumes WHERE label = 'VOL-IMP'"
        ),
        0,
        "--dry-run imported the volume: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("VOL-IMP"),
        "dry-run output does not name the volume"
    );

    // A dry run must still refuse a label already taken.
    ok(
        home.path(),
        &["import", "--label", "VOL-IMP", "--generation", "LTO-6"],
    );
    let dup = run_tapectl(
        home.path(),
        &[
            "import",
            "--label",
            "VOL-IMP",
            "--generation",
            "LTO-6",
            "--dry-run",
        ],
    );
    assert!(
        !dup.status.success(),
        "a dry run must still refuse a volume label already taken"
    );
}

/// `init`: no `.tapectl` home at all — not a half-created one. Deliberately
/// does not call `db()` (which itself runs `tapectl::db::open` and would
/// CREATE the database), and checks stderr never carries the one-time
/// escrow secret banner, since a dry run must not mint an escrow identity
/// it then has to keep secret.
#[test]
fn init_dry_run_creates_nothing() {
    let home = TempDir::new().unwrap();

    let out = ok(home.path(), &["init", "--dry-run"]);

    assert!(
        !home.path().join(".tapectl").exists(),
        "--dry-run created the tapectl home: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("would"),
        "dry-run output does not read as a preview: {text}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("ESCROW IDENTITY GENERATED"),
        "a dry run must not print a one-time escrow secret it never generated: {stderr}"
    );

    // A dry run against an already-initialized home must still refuse.
    ok(home.path(), &["init"]);
    let dup = run_tapectl(home.path(), &["init", "--dry-run"]);
    assert!(
        !dup.status.success(),
        "a dry run must still refuse re-initializing an existing home"
    );
}

/// `quick-archive`: the worst of the ten (issue #247) — `operations::
/// quick_archive` took no `dry_run` parameter at all and ended in `volume
/// write`, so `--dry-run` staged a whole unit and sealed a real cartridge.
/// This is the exact negative control the issue names: a home with NO
/// `[[backends.lto]]` configured, so a refusal that fires after even one
/// lookup surfaces `config::no_lto_backend_error`'s text instead of the
/// `--dry-run` refusal — which is what the pre-fix run of this test showed.
#[test]
fn quick_archive_dry_run_refuses_before_resolving_a_backend() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);
    ok(home.path(), &["tenant", "add", "acme"]);
    let root = TempDir::new().unwrap();
    let unit_dir = root.path().join("u1");
    std::fs::create_dir_all(&unit_dir).unwrap();
    std::fs::write(unit_dir.join("f.txt"), b"hello").unwrap();

    let out = run_tapectl(
        home.path(),
        &[
            "quick-archive",
            unit_dir.to_str().unwrap(),
            "--tenant",
            "acme",
            "--volume",
            "NOSUCHVOL",
            "--dry-run",
        ],
    );

    assert!(
        !out.status.success(),
        "quick-archive --dry-run must refuse, not run: stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--dry-run"),
        "refusal must name --dry-run: {stderr}"
    );
    assert!(
        !stderr.contains("no LTO backend configured"),
        "the refusal must fire before backend resolution, not surface the backend error: \
         {stderr}"
    );

    let conn = db(home.path());
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM units"),
        0,
        "--dry-run created a unit"
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM stage_sets"),
        0,
        "--dry-run staged anything"
    );
}
