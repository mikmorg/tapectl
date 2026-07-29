pub mod events;
#[allow(dead_code)]
pub mod models;
pub mod queries;

use std::path::Path;

use rusqlite::Connection;
use rusqlite_migration::{Migrations, M};
use tracing::warn;

use crate::error::{Result, TapectlError};

/// Open (or create) the database and run migrations.
pub fn open(path: &Path) -> Result<Connection> {
    let mut conn = Connection::open(path)?;
    configure(&conn)?;
    migrate(&mut conn)?;
    recover_orphaned_sessions(&conn)?;
    Ok(conn)
}

/// Open an in-memory database for testing.
#[cfg(test)]
pub fn open_memory() -> Result<Connection> {
    let mut conn = Connection::open_in_memory()?;
    configure(&conn)?;
    migrate(&mut conn)?;
    Ok(conn)
}

/// Set WAL mode and other pragmas.
fn configure(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(())
}

fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(include_str!("migrations/001_initial.sql")),
        M::up(include_str!("migrations/002_fts5_catalog.sql")),
        // 003 rebuilds `volumes` (create/copy/drop/rename) to extend its status CHECK;
        // `.foreign_key_check()` runs `PRAGMA foreign_key_check` before commit (step 10 of
        // SQLite's 12-step schema-change procedure) so any FK violation aborts the migration
        // instead of landing silently. See migrate() below for why FK enforcement also has to
        // be toggled outside this migration's transaction.
        M::up(include_str!("migrations/003_v2_lifecycle.sql")).foreign_key_check(),
        // 004 adds volumes.uuid — a real, independent volume identifier (the v2 ID
        // thunk pairs it with label, and §2.1 seeds the envelope permutation from
        // it). See the migration for why deriving it from the label was rejected.
        M::up(include_str!("migrations/004_volume_uuid.sql")),
        // 005 adds files.file_type/link_target and manifest_entries.file_type/
        // link_target (issue #33/H7): the walk and the validator must agree on
        // link-following semantics, so the walk's classification is recorded and
        // the validator filters its content-validation set on it directly. See
        // the migration for the full defect history.
        M::up(include_str!("migrations/005_file_types.sql")),
    ])
}

fn migrate(conn: &mut Connection) -> Result<()> {
    // Migration 003 does DROP TABLE volumes while five tables (cartridge_volumes,
    // volume_movements, writes, verification_sessions, health_logs) hold rows with a
    // `REFERENCES volumes(id)` foreign key. `configure()` turns `foreign_keys` ON for this
    // connection, and SQLite refuses to drop a table that other rows still reference while
    // FK enforcement is on.
    //
    // Verified finding: rusqlite_migration 2.5.0 does NOT toggle `PRAGMA foreign_keys` around
    // migrations itself (confirmed by reading the vendored source: `goto_up`/`goto_down` in
    // lib.rs open exactly one transaction per to_latest()/to_version() call and never touch
    // that pragma; grep across the crate's source shows `foreign_keys` mentioned only in doc
    // comments). Those doc comments (`M::foreign_key_check`) explicitly instruct callers to
    // toggle the pragma on the Connection before/after calling `to_latest()`, and warn that
    // toggling it *inside* a migration's SQL is a no-op once a transaction is open -- which it
    // already is by the time that SQL runs. So this has to happen here, outside the crate's
    // transaction, per steps 1 and 12 of SQLite's documented 12-step "Making Other Kinds Of
    // Table Schema Changes" procedure (step 10, the pre-commit foreign_key_check, is covered by
    // `.foreign_key_check()` on the 003 migration above).
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let result = migrations()
        .to_latest(conn)
        .map_err(|e| TapectlError::Migration(e.to_string()));
    conn.pragma_update(None, "foreign_keys", "ON")?;
    result
}

/// On startup: detect write sessions orphaned by a crash and mark them
/// resumable, per `docs/design/layout-session.md`'s state table: "Interrupted
/// | SIGINT (clean mark) **or** startup sweep found orphaned `in_progress`
/// (crash). Resumable while the Layout revalidates." A crash is not data
/// loss — the tape may still be fully resumable per the two-case cursor rule
/// (`session::InterruptedSession::resume`) — so this sweep targets
/// `interrupted`, never `aborted` (CONTEXT.md: "Interruption is a Layout
/// transition, not an accident"). `Aborted` is reserved for an explicit
/// operator abandonment, an unrecoverable resume-revalidation failure, or a
/// real EOT — all decided later, inside `session.rs`, never here.
///
/// Only `in_progress` rows are matched (not also `interrupted`, as a
/// pre-T6 version of this sweep did): a row already `interrupted` — from a
/// clean SIGINT mark or a previous run of this same sweep — needs no further
/// action, and re-matching it on every `db::open()` would log a spurious
/// "recovered N sessions" event each time an unresolved interrupted session
/// simply sits there.
fn recover_orphaned_sessions(conn: &Connection) -> Result<()> {
    let updated = conn.execute(
        "UPDATE writes SET status = 'interrupted'
         WHERE status = 'in_progress'",
        [],
    )?;
    if updated > 0 {
        warn!(
            count = updated,
            "recovered orphaned write sessions — marked as interrupted (resumable)"
        );
        events::log_event(
            conn,
            "system",
            0,
            None,
            "crash_recovery",
            Some("writes.status"),
            None,
            Some("interrupted"),
            Some(&format!("{updated} sessions")),
            None,
        )?;
    }

    let updated = conn.execute(
        "UPDATE stage_sets SET status = 'failed'
         WHERE status = 'staging'",
        [],
    )?;
    if updated > 0 {
        warn!(
            count = updated,
            "recovered orphaned staging sessions — marked as failed"
        );
        events::log_event(
            conn,
            "system",
            0,
            None,
            "crash_recovery",
            Some("stage_sets.status"),
            None,
            Some("failed"),
            Some(&format!("{updated} sessions")),
            None,
        )?;
    }

    let updated = conn.execute(
        "UPDATE verification_sessions SET outcome = 'aborted'
         WHERE outcome = 'in_progress'",
        [],
    )?;
    if updated > 0 {
        warn!(
            count = updated,
            "recovered orphaned verification sessions — marked as aborted"
        );
        events::log_event(
            conn,
            "system",
            0,
            None,
            "crash_recovery",
            Some("verification_sessions.outcome"),
            None,
            Some("aborted"),
            Some(&format!("{updated} sessions")),
            None,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_memory() {
        let conn = open_memory().unwrap();
        // Verify tables exist
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='tenants'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_wal_mode() {
        let conn = open_memory().unwrap();
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        // In-memory databases use "memory" journal mode, but WAL was requested
        assert!(mode == "wal" || mode == "memory");
    }

    // --- Migration 003 (v2 lifecycle: sealed/quarantined + escrow) ---
    // Decision-sheet §3.6 (docs/design/v2-open-questions.md).

    /// Build a connection migrated to exactly the 002 schema (pre-003), the way a real
    /// database created before this migration existed would look. Uses the same `configure`
    /// the production `open()`/`open_memory()` path uses, so `PRAGMA foreign_keys` is really
    /// ON here too -- this is what makes the FK check in
    /// `test_migrate_002_populated_db_to_003_preserves_data_and_fk` a real discriminator
    /// rather than a vacuous pass.
    fn open_memory_at_002() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        Migrations::new(vec![
            M::up(include_str!("migrations/001_initial.sql")),
            M::up(include_str!("migrations/002_fts5_catalog.sql")),
        ])
        .to_latest(&mut conn)
        .unwrap();
        conn
    }

    /// Exactly the 003 schema — the reference point for "003 did not change
    /// columns". Must NOT be `open_memory()` (that is *latest*, which includes
    /// 004's deliberate `volumes.uuid` addition and every migration after).
    fn open_memory_at_003() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        Migrations::new(vec![
            M::up(include_str!("migrations/001_initial.sql")),
            M::up(include_str!("migrations/002_fts5_catalog.sql")),
            M::up(include_str!("migrations/003_v2_lifecycle.sql")).foreign_key_check(),
        ])
        .to_latest(&mut conn)
        .unwrap();
        conn
    }

    /// (name, type, notnull, dflt_value, pk) for every column, in declaration order.
    fn table_info(
        conn: &Connection,
        table: &str,
    ) -> Vec<(String, String, i64, Option<String>, i64)> {
        conn.prepare(&format!("PRAGMA table_info({table})"))
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn index_names(conn: &Connection, table: &str) -> Vec<String> {
        let mut names: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name=?1")
            .unwrap()
            .query_map([table], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        names.sort();
        names
    }

    /// (a) A fresh DB migrates cleanly to latest; the extended CHECK carries every legacy
    /// status plus the two new ones, and encryption_keys gained is_escrow (default 0).
    #[test]
    fn test_migration_003_fresh_db_reaches_latest() {
        let conn = open_memory().unwrap();

        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='volumes'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        for status in [
            "'blank'",
            "'initialized'",
            "'active'",
            "'full'",
            "'retired'",
            "'missing'",
            "'erased'",
            "'sealed'",
            "'quarantined'",
        ] {
            assert!(
                sql.contains(status),
                "volumes CHECK missing {status}: {sql}"
            );
        }

        let cols = table_info(&conn, "encryption_keys");
        let is_escrow = cols
            .iter()
            .find(|(name, ..)| name == "is_escrow")
            .unwrap_or_else(|| panic!("encryption_keys.is_escrow column missing: {cols:?}"));
        assert_eq!(is_escrow.2, 1, "is_escrow must be NOT NULL");
        assert_eq!(
            is_escrow.3.as_deref(),
            Some("0"),
            "is_escrow must default to 0"
        );
    }

    /// Bulletproof self-check: the rebuilt `volumes` table is column-for-column,
    /// default-for-default, index-for-index identical to the 002 schema except the two
    /// added CHECK values (which PRAGMA table_info can't see, since CHECK isn't part of a
    /// column's structural identity -- exercised separately by the insert-based tests below).
    #[test]
    fn test_migration_003_volumes_columns_and_indexes_unchanged() {
        let conn_002 = open_memory_at_002();
        let cols_002 = table_info(&conn_002, "volumes");
        let idx_002 = index_names(&conn_002, "volumes");

        // Compare against 003 specifically, not `open_memory()` (= latest):
        // migration 004 deliberately ADDS `volumes.uuid`, so latest is the
        // wrong reference point for a "003 changed nothing" assertion.
        let conn_003 = open_memory_at_003();
        let cols_003 = table_info(&conn_003, "volumes");
        let idx_003 = index_names(&conn_003, "volumes");

        assert_eq!(
            cols_002, cols_003,
            "volumes columns/defaults/notnull/pk changed by migration 003"
        );
        assert_eq!(idx_002, idx_003);
        // Two explicit indexes plus the implicit UNIQUE(label) autoindex.
        assert_eq!(
            idx_002,
            vec![
                "idx_volumes_location",
                "idx_volumes_status",
                "sqlite_autoindex_volumes_1",
            ]
        );
    }

    /// (b) + (d): a DB populated at 002-level -- with a legacy 'full' volume row and a row in
    /// every table that FK-references volumes(id) (cartridge_volumes, volume_movements,
    /// writes, verification_sessions, health_logs; five in total per the §3.6 recon) --
    /// migrates cleanly through the real `migrate()` (exercising the actual FK on/off
    /// wrapping), `PRAGMA foreign_key_check` comes back empty, every row is intact, the
    /// legacy 'full' status is still readable, and `db_fsck` is clean.
    #[test]
    fn test_migrate_002_populated_db_to_003_preserves_data_and_fk() {
        let mut conn = open_memory_at_002();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('V-LEGACY', 'lto', 'lto0', 'LTO-6', 2500000000000, 'full')",
            [],
        )
        .unwrap();
        let vol_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO tenants (name, description, is_operator, status) VALUES ('t1', '', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (tenant_id, uuid, name, current_path, status)
             VALUES (?1, 'uuid-u1', 'u1', '/tmp/u1', 'active')",
            [tenant_id],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'created', '/tmp/u1', 0, 0)",
            [unit_id],
        )
        .unwrap();
        let snap_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
            [snap_id],
        )
        .unwrap();
        let stage_set_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'completed')",
            [stage_set_id, snap_id, vol_id],
        )
        .unwrap();
        let write_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO verification_sessions (volume_id, verify_type, outcome)
             VALUES (?1, 'full', 'passed')",
            [vol_id],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status)
             VALUES ('BC-1', 'LTO-6', 2500000000000, 'in_use')",
            [],
        )
        .unwrap();
        let cart_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
            [cart_id, vol_id],
        )
        .unwrap();

        conn.execute("INSERT INTO locations (name) VALUES ('loc1')", [])
            .unwrap();
        let loc_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO volume_movements (volume_id, to_location) VALUES (?1, ?2)",
            [vol_id, loc_id],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO health_logs (volume_id, operation) VALUES (?1, 'write')",
            [vol_id],
        )
        .unwrap();

        // Exercise the real production migrate() -- the actual FK off/on wrapping this
        // migration depends on -- not a hand-rolled call to Migrations::to_latest().
        migrate(&mut conn).unwrap();

        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            fk_violations, 0,
            "PRAGMA foreign_key_check found violations"
        );

        let (label, status): (String, String) = conn
            .query_row(
                "SELECT label, status FROM volumes WHERE id = ?1",
                [vol_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(label, "V-LEGACY");
        assert_eq!(
            status, "full",
            "(d) legacy 'full' row must still be readable"
        );

        let writes_vol: i64 = conn
            .query_row(
                "SELECT volume_id FROM writes WHERE id = ?1",
                [write_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(writes_vol, vol_id);

        let vs_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM verification_sessions WHERE volume_id = ?1",
                [vol_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(vs_count, 1);

        let cv_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM cartridge_volumes WHERE volume_id = ?1",
                [vol_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cv_count, 1);

        let vm_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM volume_movements WHERE volume_id = ?1",
                [vol_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(vm_count, 1);

        let hl_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM health_logs WHERE volume_id = ?1",
                [vol_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hl_count, 1);

        let report = crate::cli::operations::db_fsck(&conn, false).unwrap();
        assert!(report.integrity_ok, "db fsck integrity check failed");
        assert!(
            report.issues.is_empty(),
            "db fsck found issues: {:?}",
            report.issues
        );
    }

    /// (c) The two new statuses are insertable after migrating.
    #[test]
    fn test_migration_003_new_statuses_insertable() {
        let conn = open_memory().unwrap();
        for status in ["sealed", "quarantined"] {
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
                 VALUES (?1, 'lto', 'lto0', 2500000000000, ?2)",
                rusqlite::params![format!("V-{status}"), status],
            )
            .unwrap_or_else(|e| panic!("status '{status}' should be insertable: {e}"));
        }
    }

    /// The CHECK constraint still rejects unknown values -- proof it wasn't dropped or
    /// widened into a no-op while extending it.
    #[test]
    fn test_migration_003_invalid_status_still_rejected() {
        let conn = open_memory().unwrap();
        let err = conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
             VALUES ('V-bad', 'lto', 'lto0', 2500000000000, 'not_a_real_status')",
            [],
        );
        assert!(
            err.is_err(),
            "CHECK constraint should reject unknown status values"
        );
    }
}
