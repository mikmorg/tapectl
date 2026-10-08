//! Cost budgets for the catalog's read commands (issue #416, item 10).
//!
//! The production slowdowns passed every gate because the suites asserted
//! what a command returned, never what it cost, on catalogs a tenth of
//! production's size. Two costs are measured here, on a catalog
//! synthesized at two scales, through each command's real code:
//!
//! - **Statements.** Every statement SQLite runs is counted
//!   (`Connection::trace_v2`, rusqlite's `trace` feature, dev-only). A
//!   command that looks up a unit, a file or a volume runs the same number
//!   of statements at 200 units as at 40: a per-row statement (an N+1) is
//!   the defect this catches. A command that is per-unit by design
//!   (`audit`, `collection status`) is held to a pinned statements-per-unit
//!   slope instead.
//! - **Query plans.** Every SELECT a command ran is put through `EXPLAIN
//!   QUERY PLAN`, and the tables it reads in full (`SCAN t` with no index)
//!   are pinned per command. A new full scan of a big table — a dropped
//!   index, a predicate that stopped being sargable — fails the pin; so
//!   does a scan going away, which is a pin to update on purpose.
//!
//! The on-tape `catalog.db` build's commit budget (one commit, not one per
//! row: issue #399) is pinned in `db::ontape_catalog`'s own tests; a commit
//! is the fsync this suite cannot count directly.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rusqlite::trace::{TraceEvent, TraceEventCodes};
use rusqlite::{params, Connection};
use tapectl::cli::catalog::CatalogCommands;
use tapectl::config::{CollectionConfig, Config};
use tapectl::db::files::{FileEntry, FileKind};

/// Files in each unit's one version.
const FILES: usize = 60;
/// Slices in each unit's stage set.
const SLICES: i64 = 6;
/// Units on each volume (each unit is written to two volumes).
const UNITS_PER_VOLUME: usize = 20;
const SMALL: usize = 40;
const LARGE: usize = 200;

thread_local! {
    static RAN: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn record(event: TraceEvent<'_>) {
    if let TraceEvent::Stmt(_, sql) = event {
        // A trigger's sub-program is reported as `-- TRIGGER name`: part
        // of the statement that fired it, not a statement of its own.
        if !sql.trim_start().starts_with("--") {
            RAN.with(|r| r.borrow_mut().push(sql.to_string()));
        }
    }
}

/// Run `f` with every statement `conn` runs recorded; return them.
fn traced(conn: &Connection, f: impl FnOnce()) -> Vec<String> {
    RAN.with(|r| r.borrow_mut().clear());
    conn.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(record));
    f();
    conn.trace_v2(TraceEventCodes::empty(), None);
    RAN.with(|r| std::mem::take(&mut *r.borrow_mut()))
}

/// The tables `sql` reads in full, by `EXPLAIN QUERY PLAN`: a `SCAN` step
/// with no index. Parameters stay unbound (`raw_query` does not check the
/// count), which plans as for any value.
fn full_scans(conn: &Connection, sql: &str) -> BTreeSet<String> {
    let head = sql.trim_start().to_ascii_uppercase();
    if !(head.starts_with("SELECT") || head.starts_with("WITH")) {
        return BTreeSet::new();
    }
    let Ok(mut stmt) = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")) else {
        return BTreeSet::new(); // a temp table since dropped, say
    };
    let mut rows = stmt.raw_query();
    let mut scans = BTreeSet::new();
    while let Some(row) = rows.next().unwrap() {
        let detail: String = row.get(3).unwrap();
        // An AUTOMATIC index is one SQLite builds for this statement by
        // reading the whole table first: a full scan, every time it runs.
        if let Some(rest) = detail.strip_prefix("SEARCH ") {
            if detail.contains("USING AUTOMATIC") {
                scans.insert(
                    rest.split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .to_string(),
                );
            }
            continue;
        }
        let Some(rest) = detail.strip_prefix("SCAN ") else {
            continue;
        };
        // ` USING ` is an index (or the rowid); `VIRTUAL TABLE INDEX` is
        // FTS5's own; `(subquery-N)` and `CONSTANT ROW` are SQLite's
        // intermediate results, not tables.
        if detail.contains(" USING ")
            || detail.contains("VIRTUAL TABLE INDEX")
            || rest.starts_with('(')
            || rest.starts_with("CONSTANT ROW")
        {
            continue;
        }
        let table = rest.split_whitespace().next().unwrap_or_default();
        scans.insert(table.to_string());
    }
    scans
}

/// What one command cost.
struct Cost {
    statements: usize,
    /// Tables read in full by any statement, with the count of statements
    /// that did.
    scans: BTreeMap<String, usize>,
}

fn cost(conn: &Connection, f: impl FnOnce()) -> Cost {
    let ran = traced(conn, f);
    let mut scans = BTreeMap::new();
    for sql in &ran {
        for t in full_scans(conn, sql) {
            *scans.entry(t).or_insert(0) += 1;
        }
    }
    Cost {
        statements: ran.len(),
        scans,
    }
}

/// A catalog of `units` units, each one version of [`FILES`] files staged
/// as [`SLICES`] slices and written to two sealed volumes at two
/// locations, each unit's directory under `root`.
fn catalog(units: usize, dir: &Path) -> (Connection, Config) {
    let conn = tapectl::db::open(&dir.join("catalog.db")).unwrap();
    // Canonical, as `collection status` compares unit paths to its root.
    std::fs::create_dir_all(dir.join("root")).unwrap();
    let root = std::fs::canonicalize(dir.join("root")).unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    tx.execute_batch(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('operator', 1, 'active');
         INSERT INTO tenants (name, is_operator, status) VALUES ('alpha', 0, 'active');
         INSERT INTO locations (name) VALUES ('home');
         INSERT INTO locations (name) VALUES ('vault');",
    )
    .unwrap();
    let volumes = 2 * units.div_ceil(UNITS_PER_VOLUME);
    for v in 0..volumes {
        tx.execute(
            "INSERT INTO volumes (label, uuid, backend_type, backend_name, media_type,
                                  capacity_bytes, status, location_id, sealed_at)
             VALUES (?1, ?2, 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed', ?3,
                     datetime('now'))",
            params![format!("V{v:04}"), format!("uuid-v{v}"), 1 + (v % 2) as i64],
        )
        .unwrap();
    }
    for u in 0..units {
        let name = format!("unit-{u:04}");
        let path = root.join(&name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("f0000"), b"x").unwrap();
        tx.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES (?1, ?2, 2, ?3, 'active')",
            params![format!("uuid-{u}"), name, path.to_string_lossy()],
        )
        .unwrap();
        let unit_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'current', ?2, ?3, ?3)",
            params![unit_id, path.to_string_lossy(), FILES as i64],
        )
        .unwrap();
        let snapshot_id = tx.last_insert_rowid();
        tapectl::db::files::insert_version(
            &tx,
            snapshot_id,
            (0..FILES).map(|i| {
                Ok(FileEntry {
                    path: format!("dir{}/f{i:04}", i % 5),
                    kind: FileKind::Regular,
                    size_bytes: 1,
                    mtime_ns: None,
                    sha256: Some([0x11; 32]),
                    link_target: None,
                })
            }),
        )
        .unwrap();
        tx.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'cleaned', 1)",
            params![snapshot_id],
        )
        .unwrap();
        let stage_set_id = tx.last_insert_rowid();
        let mut slice_ids = Vec::new();
        for n in 1..=SLICES {
            tx.execute(
                "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes,
                                           encrypted_bytes, sha256_plain, sha256_encrypted)
                 VALUES (?1, ?2, 1, 1, 'p', ?3)",
                params![stage_set_id, n, format!("e-{u}-{n}")],
            )
            .unwrap();
            slice_ids.push(tx.last_insert_rowid());
        }
        let first = 2 * (u / UNITS_PER_VOLUME);
        for (copy, volume) in [first, first + 1].into_iter().enumerate() {
            tx.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status,
                                     started_at, completed_at)
                 VALUES (?1, ?2, ?3, 'completed', datetime('now'), datetime('now'))",
                params![stage_set_id, snapshot_id, volume as i64 + 1],
            )
            .unwrap();
            let write_id = tx.last_insert_rowid();
            for (i, slice_id) in slice_ids.iter().enumerate() {
                let position = 6 + (u % UNITS_PER_VOLUME) * SLICES as usize + i + copy;
                tx.execute(
                    "INSERT INTO write_positions (write_id, stage_slice_id, position, status)
                     VALUES (?1, ?2, ?3, 'written')",
                    params![write_id, slice_id, position.to_string()],
                )
                .unwrap();
            }
        }
    }
    tx.commit().unwrap();

    let mut config = Config::default();
    let collection: CollectionConfig = toml::from_str(&format!(
        "name = 'media'\nroot = '{}'\ntenant = 'alpha'\n",
        root.display()
    ))
    .unwrap();
    config.collections.push(collection);
    (conn, config)
}

/// The commands under budget, run once each against `conn`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Command {
    Ls,
    Search,
    Locate,
    Stats,
    RestoreSelect,
    Audit,
    CollectionStatus,
}

const COMMANDS: [Command; 7] = [
    Command::Ls,
    Command::Search,
    Command::Locate,
    Command::Stats,
    Command::RestoreSelect,
    Command::Audit,
    Command::CollectionStatus,
];

fn run(command: Command, conn: &Connection, config: &Config) {
    let catalog = |c: CatalogCommands| {
        tapectl::cli::catalog::run(conn, config, &c, true, false).unwrap();
    };
    match command {
        Command::Ls => catalog(CatalogCommands::Ls {
            unit: "unit-0007".to_string(),
            version: None,
        }),
        Command::Search => catalog(CatalogCommands::Search {
            pattern: "f0042".to_string(),
            limit: 50,
            all_versions: false,
        }),
        Command::Locate => catalog(CatalogCommands::Locate {
            unit: "unit-0007".to_string(),
        }),
        Command::Stats => catalog(CatalogCommands::Stats),
        Command::RestoreSelect => {
            tapectl::volume::restore::select_write_positions(conn, "unit-0007", "V0000", None)
                .unwrap();
        }
        Command::Audit => {
            tapectl::cli::audit::run(conn, config, None, false, true).unwrap();
        }
        Command::CollectionStatus => {
            let s = tapectl::collection::status::status_for_collection(
                conn,
                config,
                &config.collections[0],
            )
            .unwrap();
            // Positive control: the walk ran over every registered unit —
            // none is missing, and each one's directory (one file, where the
            // catalog recorded sixty) is seen as dirty.
            assert!(s.missing == 0 && s.dirty > 0, "{s:?}");
        }
    }
}

fn costs(units: usize) -> BTreeMap<Command, Cost> {
    let dir = tempfile::tempdir().unwrap();
    let (conn, config) = catalog(units, dir.path());
    COMMANDS
        .iter()
        .map(|c| (*c, cost(&conn, || run(*c, &conn, &config))))
        .collect()
}

/// Each command's budget: the most statements it may run at [`LARGE`]
/// units, the most MORE it may run per unit above [`SMALL`] (0 for a
/// lookup: it may not run a statement per row), and the tables it may
/// read in full. Measured 2026-10-07; a change in either direction is a
/// pin to move on purpose, with the reason in the commit.
fn budget(command: Command) -> (usize, usize, &'static [&'static str]) {
    match command {
        Command::Ls => (3, 0, &[]),
        // The FTS5 index answers the match (`VIRTUAL TABLE INDEX`).
        Command::Search => (1, 0, &[]),
        // `encryption_keys` is a handful of rows.
        Command::Locate => (4, 0, &["encryption_keys"]),
        // Totals over every version: one pass over `snapshots` is the job.
        Command::Stats => (4, 0, &["snapshots"]),
        Command::RestoreSelect => (3, 0, &[]),
        // Per unit by design today: six statements a unit (its checks run
        // unit by unit), plus one pass over `writes` (w) and `volumes` (v)
        // for the whole-catalog checks. The slope is pinned so a seventh
        // per-unit statement is a decision, not a drift.
        Command::Audit => (8 + 6 * LARGE, 6, &["encryption_keys", "v", "w"]),
        // Per unit by design today: the unit list, then for each unit
        // under the root its policy (`policy::resolve`) and its copy count
        // — three statements a unit.
        Command::CollectionStatus => (2 + 3 * LARGE, 3, &[]),
    }
}

/// Every command within its statement budget and its full-scan pin, at
/// both scales.
#[test]
fn every_catalog_command_stays_within_its_budget() {
    let small = costs(SMALL);
    let large = costs(LARGE);
    let mut over = Vec::new();
    for c in COMMANDS {
        let (most, per_unit, scans) = budget(c);
        let (s, l) = (&small[&c], &large[&c]);
        let grew = l.statements > s.statements + per_unit * (LARGE - SMALL);
        let pinned: BTreeSet<String> = scans.iter().map(|t| t.to_string()).collect();
        let scanned: BTreeSet<String> = l.scans.keys().cloned().collect();
        if l.statements > most || grew || scanned != pinned {
            over.push(format!(
                "{c:?}: {} -> {} statements (budget {most}, {per_unit} a unit), \
                 full scans {scanned:?} (pinned {pinned:?})",
                s.statements, l.statements
            ));
        }
    }
    assert!(over.is_empty(), "{over:#?}");
}

/// The statement counter's positive control: a command that IS per unit
/// is seen growing with the catalog — the counter is not blind to the very
/// thing it budgets.
#[test]
fn the_statement_counter_sees_a_per_unit_statement() {
    let small = costs(SMALL);
    let large = costs(LARGE);
    assert!(
        large[&Command::Audit].statements >= small[&Command::Audit].statements + (LARGE - SMALL),
        "audit's per-unit statements are counted"
    );
}

/// The full-scan pin's positive control: `write_positions` rebuilt the way
/// a careless table-rebuild migration would (`CREATE TABLE … AS SELECT`,
/// which carries no index and no constraint), and a restore's slice lookup
/// has no way into it but a full read — the same command breaks its pin.
#[test]
fn a_table_rebuilt_without_its_indexes_breaks_the_restore_lookups_pin() {
    let dir = tempfile::tempdir().unwrap();
    let (conn, config) = catalog(SMALL, dir.path());
    let pinned = |conn: &Connection| {
        cost(conn, || run(Command::RestoreSelect, conn, &config))
            .scans
            .is_empty()
    };
    assert!(pinned(&conn), "the pin holds with the schema's indexes");
    conn.execute_batch(
        "PRAGMA foreign_keys = OFF;
         CREATE TABLE write_positions_rebuilt AS SELECT * FROM write_positions;
         DROP TABLE write_positions;
         ALTER TABLE write_positions_rebuilt RENAME TO write_positions;",
    )
    .unwrap();
    assert!(!pinned(&conn), "and breaks without them");
}

/// The plan reader's positive control: a predicate on an unindexed column
/// is a full scan, and the same table by an indexed one is not.
#[test]
fn the_plan_reader_tells_a_full_scan_from_an_index() {
    let dir = tempfile::tempdir().unwrap();
    let (conn, _) = catalog(2, dir.path());
    assert_eq!(
        full_scans(&conn, "SELECT id FROM write_positions WHERE status = ?1"),
        BTreeSet::from(["write_positions".to_string()])
    );
    assert!(full_scans(&conn, "SELECT id FROM write_positions WHERE write_id = ?1").is_empty());
    assert!(full_scans(
        &conn,
        "SELECT rowid FROM paths_fts WHERE paths_fts MATCH ?1"
    )
    .is_empty());
}
