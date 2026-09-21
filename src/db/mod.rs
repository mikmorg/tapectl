pub mod catalog_snapshot;
pub mod events;
pub mod export;
#[allow(dead_code)]
pub mod models;
pub mod ontape_catalog;
pub mod queries;

use std::path::Path;

use rusqlite::Connection;
use rusqlite_migration::{Error as MigrationError, Migrations, M};
use tracing::warn;

use crate::error::{Result, TapectlError};

/// Open (or create) the database and run migrations.
///
/// Issue #41: `tapectl.db` holds every filename, path, size, mtime, sha256,
/// and tenant/unit name tapectl has recorded — precisely the plaintext
/// content-metadata index the on-tape format works hard to keep out of
/// plaintext. `Connection::open` sets no mode of its own, so this used to
/// land at whatever the process umask handed out. Tightened here (not just
/// on creation) since this function runs on *every* command invocation —
/// unlike `TapectlPaths::ensure_dirs` (init-only until this same change
/// wired it into `main.rs`'s general dispatch), this is the one fix that
/// reaches an already-initialized `~/.tapectl` on its own. `secure_path`
/// warns rather than fails, so a database this process doesn't own doesn't
/// break every command that touches it.
pub fn open(path: &Path) -> Result<Connection> {
    let mut conn = Connection::open(path)?;
    configure(&conn)?;
    migrate(&mut conn)?;
    recover_orphaned_sessions(&conn, path)?;
    crate::config::secure_path(path, 0o600);
    // WAL mode (set in `configure`) writes pending pages to `<path>-wal`
    // (and its `-shm` index) — the exact same content as the main db file,
    // just not checkpointed into it yet. Tighten them too, if SQLite has
    // created them by this point; best-effort, since whether they exist
    // yet is a SQLite-internal timing detail this function doesn't control.
    for suffix in ["-wal", "-shm"] {
        let sidecar = sidecar_path(path, suffix);
        if sidecar.exists() {
            crate::config::secure_path(&sidecar, 0o600);
        }
    }
    Ok(conn)
}

/// Build `<path>-wal` / `<path>-shm` the way SQLite itself names them —
/// appended directly to the full filename, not swapping the extension.
fn sidecar_path(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(suffix);
    std::path::PathBuf::from(os)
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
        // 006 adds writes.session_dir (issue #25): the pointer to the frozen
        // staging directory a restarted process needs to REHYDRATE an
        // interrupted session's Layout. It cannot be regenerated — see the
        // migration for the `created_at`/`mam_loads` proof.
        M::up(include_str!("migrations/006_write_session_dir.sql")),
        // 007 adds locations.kind, archive_sets.warehouse_copies and the
        // volume_deposits table (issue #73, ADR-0006): a warehouse copy is a
        // RECORDED deposit of an already-sealed volume, not a second volume and
        // not a change of the cartridge's location. See the migration header for
        // why it is not a volume_locations join table.
        M::up(include_str!("migrations/007_warehouse_locations.sql")),
        // 008 drops volumes.storage_class (issue #101): dead since 001, and
        // after 007 it collides by name with volume_deposits.storage_class,
        // which means something different. A cartridge has a media_type, not a
        // storage class. See the migration header.
        M::up(include_str!("migrations/008_drop_volume_storage_class.sql")),
        // 009 adds health_logs.tape_alerts (issue #107): the value has always
        // been parsed from log page 0x2e and then discarded for want of a
        // column. Nullable with no default — NULL means "not recorded", 0
        // means "recorded, none raised". See the migration header.
        M::up(include_str!("migrations/009_health_tape_alerts.sql")),
        // 010 adds stage_sets.origin ('staged' | 'rebuilt') (issue #137): a
        // stage set rebuilt from a tape's envelope has no recorded recipient
        // list, and the escrow predicate must tell "never written down" from
        // "could not have been written down". See the migration header.
        M::up(include_str!("migrations/010_stage_set_origin.sql")),
        // 011 adds a PARTIAL unique index on cartridges.serial_number
        // (ADR-0010): `volume init` binds the cartridge it is writing by
        // matching the loaded medium's MAM serial to that column, so a
        // repeated serial would bind to an arbitrary row. See the migration
        // header for why it is partial.
        M::up(include_str!("migrations/011_cartridge_serial_index.sql")),
        // 012 rebuilds `cartridges` (create/copy/drop/rename) to drop 'offsite'
        // from its status CHECK and to index `location_id` (ADR-0011, issue
        // #148): a cartridge's PLACE is a location, and its status is only its
        // fitness to hold data. `.foreign_key_check()` for the same reason as
        // 003 — `cartridge_volumes` holds a `REFERENCES cartridges(id)` FK, and
        // a rebuild that renumbered rows would orphan it silently. See the
        // migration header for the two traps in the rebuild order.
        M::up(include_str!("migrations/012_cartridge_lifecycle.sql")).foreign_key_check(),
        // 013 rebuilds `manifest_entries` (create/copy/drop/rename) without
        // `has_xattrs`/`has_acls` (issue #149): both were written as literal 0
        // under a comment claiming they were populated on stage, and read by
        // nothing — a column that is always zero misleads more than an absent
        // one. dar owns xattr/ACL handling. `.foreign_key_check()` even though
        // nothing holds a `REFERENCES manifest_entries(id)` FK: the table does
        // hold one OUT (to `manifests`), and the check is cheap insurance that
        // the rebuild carried every row's parent intact. See the migration
        // header.
        M::up(include_str!("migrations/013_drop_manifest_entry_flags.sql")).foreign_key_check(),
        // 014 adds the nullable `cartridge_volumes.identity_source` ('mam' |
        // 'operator'), the provenance of the identity a binding was
        // established under (ADR-0012, ADR-0010's 2026-09-14 amendment, issue
        // #192). It is what File 0's `[media].cartridge_identity_source` is
        // read back from at write time, so the tape and the catalog cannot
        // disagree about which string identifies the cartridge. A plain ADD
        // COLUMN — no rebuild, so no `.foreign_key_check()`; legacy rows stay
        // NULL, which means unknown and never 'mam'. See the migration header
        // for the three sources that were rejected.
        M::up(include_str!(
            "migrations/014_cartridge_binding_identity_source.sql"
        )),
        // Issue #184: data-only backfill. Every stored 0 predates any writer
        // of this column, so each one is "never observed" rather than "zero
        // loads"; NULL is how the new code and the display say that. No
        // `.foreign_key_check()` — this touches no key, only a nullable
        // counter on rows that already exist.
        M::up(include_str!(
            "migrations/015_cartridge_load_count_unknown.sql"
        )),
        // 016 adds `cartridges.operator_serial` (ADR-0012's 2026-09-16
        // amendment, issue #197): `serial_number` is the chip-read identity
        // and after this migration is written ONLY from a MAM read;
        // `operator_serial` is the operator's typed claim, written only by
        // `cartridge register --serial` and `cartridge edit --serial`.
        // `lookup_cartridge` falls back to it only while `serial_number IS
        // NULL`. Plain ADD COLUMN, no rebuild, no backfill -- see the
        // migration header for why existing `serial_number` values are left
        // exactly where they are.
        M::up(include_str!("migrations/016_cartridge_operator_serial.sql")),
        // 017 rebuilds `volumes` (create/copy/drop/rename) to add
        // `observed_condition` and to drop 'quarantined' from the `status`
        // CHECK (ADR-0012's 2026-09-17 amendment "the status column is the
        // operator's; a medium's condition is its own fact", issue #242):
        // four writers set `volumes.status = 'quarantined'` unconditionally,
        // overwriting a terminal operator status like `retired`, and
        // `policy::coverage::eligible` silently stopped counting a copy by
        // moving `status` OFF `sealed` rather than by any dedicated
        // predicate. `.foreign_key_check()` for the same reason as 003 and
        // 012 -- five tables hold a `REFERENCES volumes(id)` FK, and a
        // rebuild that renumbered rows would orphan every one of them
        // silently. See the migration header for the full rationale and the
        // deliberate narrowing of this ADR's own "restore to `sealed`"
        // fallback for rows a write-path (never-sealed) quarantine produced.
        M::up(include_str!("migrations/017_volume_observed_condition.sql")).foreign_key_check(),
    ])
}

fn migrate(conn: &mut Connection) -> Result<()> {
    // Migration 003 does DROP TABLE volumes while five tables (cartridge_volumes,
    // volume_movements, writes, verification_sessions, health_logs) hold rows with a
    // `REFERENCES volumes(id)` foreign key. Migration 012 does the same to `cartridges`,
    // which `cartridge_volumes` references. `configure()` turns `foreign_keys` ON for this
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
    // `.foreign_key_check()` on the 003 and 012 migrations above).
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let result = migrations().to_latest(conn).map_err(|e| {
        // Issue #233: the message is computed before matching on `e` by
        // value below (matching moves it), and the match is on the typed
        // `rusqlite_migration::Error::ForeignKeyCheck` variant, never on
        // this string — a corrupt schema must not get repair advice that
        // does not apply to it.
        let msg = e.to_string();
        match e {
            MigrationError::ForeignKeyCheck(_) => TapectlError::DatabaseNeedsRepair(msg),
            _ => TapectlError::Migration(msg),
        }
    });
    conn.pragma_update(None, "foreign_keys", "ON")?;
    result
}

/// Open the database WITHOUT running migrations — for `db fsck --repair`
/// only (issue #233).
///
/// `.foreign_key_check()` (migrations 003/012/013/017) runs an explicit
/// `PRAGMA foreign_key_check` inside `migrate()`'s transaction, unaffected
/// by the `foreign_keys` enforcement pragma — so there is no way to
/// disable the check and still call `migrate()`. The only way in is to
/// skip `migrate()` entirely and repair against whatever schema is
/// actually on disk. That is safe: `pragma_foreign_key_check` and the
/// repair it drives (`cli::operations::repair_foreign_key_violations`) are
/// schema-agnostic — both read exactly what SQLite itself reports, never a
/// hardcoded table list — so they work identically whether the database
/// is at the latest migration or several behind it (proven in this
/// module's `issue_233_*` tests). The very next ordinary `db::open()` call
/// then migrates the now-clean data forward; `schema_is_current` is how a
/// caller tells whether that still needs to happen.
///
/// Deliberately narrow: no `configure()` (no WAL, no `foreign_keys = ON`
/// — repair wants enforcement OFF, which is `Connection::open`'s own
/// default, and does its own `defer_foreign_keys` inside one transaction
/// regardless), no `recover_orphaned_sessions` (those tables may not exist
/// yet on a pre-migration schema), no permission tightening (the next
/// ordinary open already does that). This connection exists to repair and
/// exit — never hand it to any other command body.
pub fn open_for_repair(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(conn)
}

/// Whether `conn`'s schema is already at the latest migration (issue
/// #233). `open_for_repair`'s connection never calls `migrate()`, so a
/// database repaired through it stays wherever it started — behind head,
/// possibly several migrations short — until the very next ordinary
/// `db::open()` call completes the migration. `cli::db::run`'s `Fsck` arm
/// uses this to tell the operator that explicitly, rather than let
/// "repaired N rows" read as fully done when a migration is still
/// pending.
pub fn schema_is_current(conn: &Connection) -> Result<bool> {
    let pending = migrations()
        .pending_migrations(conn)
        .map_err(|e| TapectlError::Migration(e.to_string()))?;
    Ok(pending <= 0)
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
/// Issue #98 asymmetry, deliberate: the `writes` sweep just above stays
/// exactly as it was — NOT made lock-aware. `docs/design/layout-session.md`
/// (~line 157) makes "still `in_progress` at open ⇒ a live writer in
/// another process" load-bearing for `InterruptedSession::rehydrate`, and
/// changing that inference to a flock check would invalidate a documented
/// contract this function does not own. It also degrades safely as-is:
/// `interrupted` is resumable and revalidation still runs on resume, so a
/// live writer wrongly marked `interrupted` by this sweep loses nothing.
/// The `stage_sets` sweep below has no such safety net — marking a live
/// `staging` row `failed` while something else could later target `failed`
/// rows for deletion would be actively destructive — which is exactly why
/// it, and only it, gets the flock treatment.
fn recover_orphaned_sessions(conn: &Connection, db_path: &Path) -> Result<()> {
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

    // Issue #98: lock-aware. A `status = 'staging'` row alone cannot tell a
    // crashed stage from one running right now in another process — this
    // sweep runs on *every* `db::open()`, including read-only commands, so
    // a plain status-only UPDATE (the pre-fix version of this sweep) would
    // mark a live invocation's row `'failed'` out from under it. Each
    // candidate row is instead probed via its per-stage-set flock
    // (`staging::lock::is_crashed`): free ⇒ no live holder ⇒ crashed ⇒ mark
    // `'failed'`; held ⇒ a live process owns it ⇒ left untouched.
    //
    // Marking ONLY — this never touches the filesystem beyond the lockfiles
    // `is_crashed` itself opens/probes (and releases immediately). Deleting
    // any staging content here would mean opening the DB for a read (e.g.
    // `report copies`, `catalog ls`) could destroy staging data; actual
    // file cleanup for a `'failed'` set happens later, and only via
    // `staging::clean::clean_staging`.
    let staging_ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM stage_sets WHERE status = 'staging'")?;
        let ids = stmt
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids
    };

    let mut crashed = 0u64;
    for stage_set_id in staging_ids {
        if crate::staging::lock::is_crashed(db_path, stage_set_id) {
            conn.execute(
                "UPDATE stage_sets SET status = 'failed'
                 WHERE id = ?1 AND status = 'staging'",
                [stage_set_id],
            )?;
            crashed += 1;
        }
    }
    if crashed > 0 {
        warn!(
            count = crashed,
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
            Some(&format!("{crashed} sessions")),
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

    /// Issue #41: `db::open` used to `Connection::open(path)` with no mode
    /// of its own, leaving `tapectl.db` — which holds every filename, path,
    /// size, mtime, sha256, and tenant/unit name tapectl has recorded — at
    /// whatever the process umask handed out (0644 on a stock box). This is
    /// the one fix that reaches an *already-initialized* `~/.tapectl` too:
    /// `db::open` runs on every command invocation, unlike `ensure_dirs`
    /// (which was init-only until this same change wired it into the
    /// general dispatch path in `main.rs`).
    #[test]
    fn open_sets_db_file_mode_to_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("tapectl.db");

        let conn = open(&db_path).unwrap();
        drop(conn);

        let mode = std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "tapectl.db should be 0600, was {mode:o}");
    }

    /// Umask-independent companion to the above (see the analogous
    /// `config::tests::ensure_dirs_tightens_a_pre_existing_loose_directory`
    /// doc comment for why this shape is needed): a *re-opened*,
    /// pre-existing db file at a loose mode must be tightened too, not just
    /// a freshly-created one — `db::open` is called on every invocation
    /// against a file that (pre-fix) already exists from a prior run.
    #[test]
    fn open_tightens_a_pre_existing_loose_db_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("tapectl.db");

        // Create it once, then loosen it, simulating a pre-fix database
        // left over from before this change.
        drop(open(&db_path).unwrap());
        std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777,
            0o644,
            "fixture must start loose"
        );

        let conn = open(&db_path).unwrap();
        drop(conn);

        let mode = std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "re-opening must tighten a loose db file to 0600"
        );
    }

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

    /// (referenced table, from-column, to-column) for every outbound foreign
    /// key, sorted. `PRAGMA table_info` reports neither FK nor CHECK
    /// constraints, which is why this exists separately — see
    /// `test_migration_012_changes_no_cartridge_column`'s own note.
    fn foreign_keys_of(conn: &Connection, table: &str) -> Vec<(String, String, String)> {
        let mut fks: Vec<(String, String, String)> = conn
            .prepare(&format!("PRAGMA foreign_key_list({table})"))
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        fks.sort();
        fks
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

        let report = crate::cli::operations::db_fsck(&conn, false, false).unwrap();
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
        // 'sealed' is still legal against the LIVE schema. 'quarantined' is
        // deliberately excluded from the insertable set here (issue #242,
        // migration 017): it left `status`'s CHECK entirely and is legal
        // now only as a value of `observed_condition` — see
        // `test_migration_017_observed_condition_is_closed_and_defaults_ok`.
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
             VALUES ('V-sealed', 'lto', 'lto0', 2500000000000, 'sealed')",
            [],
        )
        .unwrap_or_else(|e| panic!("status 'sealed' should be insertable: {e}"));

        let err = conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
             VALUES ('V-quarantined', 'lto', 'lto0', 2500000000000, 'quarantined')",
            [],
        );
        assert!(
            err.is_err(),
            "'quarantined' is a condition now (issue #242), not a status -- the live \
             schema must reject it as a status value"
        );
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

    // --- Migration 012 (ADR-0011: the four-state cartridge lifecycle) ---

    /// A connection migrated to exactly the 011 schema — the last point at
    /// which `cartridges.status = 'offsite'` is still a legal value, so a row
    /// carrying it can actually be seeded and then watched through the
    /// rebuild. Mirrors `open_memory_at_002`, including `configure()`, so
    /// `PRAGMA foreign_keys` is genuinely ON for the seeding below and the FK
    /// assertions are real discriminators.
    fn open_memory_at_011() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        let mut ms = vec![
            M::up(include_str!("migrations/001_initial.sql")),
            M::up(include_str!("migrations/002_fts5_catalog.sql")),
            M::up(include_str!("migrations/003_v2_lifecycle.sql")).foreign_key_check(),
            M::up(include_str!("migrations/004_volume_uuid.sql")),
            M::up(include_str!("migrations/005_file_types.sql")),
            M::up(include_str!("migrations/006_write_session_dir.sql")),
            M::up(include_str!("migrations/007_warehouse_locations.sql")),
            M::up(include_str!("migrations/008_drop_volume_storage_class.sql")),
            M::up(include_str!("migrations/009_health_tape_alerts.sql")),
            M::up(include_str!("migrations/010_stage_set_origin.sql")),
            M::up(include_str!("migrations/011_cartridge_serial_index.sql")),
        ];
        // 003 drops `volumes` while five tables reference it; same reason as
        // production `migrate()`.
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        Migrations::new(std::mem::take(&mut ms))
            .to_latest(&mut conn)
            .unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn
    }

    /// THE test for this migration, and the one the 12-step procedure exists
    /// to make pass: a populated 011 database — including a cartridge bound to
    /// a volume through `cartridge_volumes`, the one table holding a
    /// `REFERENCES cartridges(id)` foreign key — survives the rebuild with its
    /// row IDS INTACT.
    ///
    /// The join assertion is the real discriminator. An `INSERT INTO
    /// cartridges_new SELECT ...` that omitted `id` would renumber every
    /// cartridge, and `PRAGMA foreign_key_check` alone would NOT necessarily
    /// catch it — with one cartridge, rowid 1 is handed straight back, and the
    /// FK still resolves while pointing at what is, in general, a different
    /// cartridge. So three rows are seeded and the join is checked by barcode.
    #[test]
    fn test_migrate_011_populated_db_to_012_preserves_ids_and_fk() {
        let mut conn = open_memory_at_011();

        conn.execute("INSERT INTO locations (name) VALUES ('home-rack')", [])
            .unwrap();
        let loc_id = conn.last_insert_rowid();

        // Three cartridges so a renumbering cannot coincidentally land every
        // row back on its own id, and one of them carries the doomed
        // 'offsite' status — legal at 011, gone at 012.
        for (bc, status) in [
            ("BC-1", "in_use"),
            ("BC-2", "offsite"),
            ("BC-3", "pending_erase"),
        ] {
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status, location_id)
                 VALUES (?1, 'LTO-6', 2500000000000, ?2, ?3)",
                rusqlite::params![bc, status, loc_id],
            )
            .unwrap();
        }
        let bc3_id: i64 = conn
            .query_row(
                "SELECT id FROM cartridges WHERE barcode = 'BC-3'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        // Bind the LAST cartridge, so an off-by-one renumbering shows up.
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('L6-0001', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
            [],
        )
        .unwrap();
        let vol_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
            [bc3_id, vol_id],
        )
        .unwrap();

        // The real production migrate(), with its real FK off/on wrapping.
        migrate(&mut conn).unwrap();

        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            fk_violations, 0,
            "PRAGMA foreign_key_check found violations after the cartridges rebuild"
        );

        // The join still names the SAME cartridge — proof the ids survived,
        // not merely that they still resolve to something.
        let joined: String = conn
            .query_row(
                "SELECT c.barcode FROM cartridge_volumes cv
                 JOIN cartridges c ON c.id = cv.cartridge_id
                 WHERE cv.volume_id = ?1",
                [vol_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            joined, "BC-3",
            "the rebuild renumbered cartridges and silently re-pointed the join"
        );

        // Every row survived, statuses mapped, location preserved.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM cartridges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
        let statuses: Vec<(String, String, Option<i64>)> = conn
            .prepare("SELECT barcode, status, location_id FROM cartridges ORDER BY barcode")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            statuses,
            vec![
                ("BC-1".to_string(), "in_use".to_string(), Some(loc_id)),
                // ADR-0011: 'offsite' is not a status; it maps to 'available'
                // and the place is recorded by location_id, which is retained.
                ("BC-2".to_string(), "available".to_string(), Some(loc_id)),
                (
                    "BC-3".to_string(),
                    "pending_erase".to_string(),
                    Some(loc_id)
                ),
            ]
        );

        let report = crate::cli::operations::db_fsck(&conn, false, false).unwrap();
        assert!(report.integrity_ok, "db fsck integrity check failed");
    }

    // --- Migration 013 (issue #149: the two always-zero manifest flags) ---

    /// A connection migrated to exactly the 012 schema — the last point at
    /// which `manifest_entries.has_xattrs` / `.has_acls` still exist, so rows
    /// carrying them can be seeded and watched through the rebuild.
    fn open_memory_at_012() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        let mut ms = vec![
            M::up(include_str!("migrations/001_initial.sql")),
            M::up(include_str!("migrations/002_fts5_catalog.sql")),
            M::up(include_str!("migrations/003_v2_lifecycle.sql")).foreign_key_check(),
            M::up(include_str!("migrations/004_volume_uuid.sql")),
            M::up(include_str!("migrations/005_file_types.sql")),
            M::up(include_str!("migrations/006_write_session_dir.sql")),
            M::up(include_str!("migrations/007_warehouse_locations.sql")),
            M::up(include_str!("migrations/008_drop_volume_storage_class.sql")),
            M::up(include_str!("migrations/009_health_tape_alerts.sql")),
            M::up(include_str!("migrations/010_stage_set_origin.sql")),
            M::up(include_str!("migrations/011_cartridge_serial_index.sql")),
            M::up(include_str!("migrations/012_cartridge_lifecycle.sql")).foreign_key_check(),
        ];
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        Migrations::new(std::mem::take(&mut ms))
            .to_latest(&mut conn)
            .unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn
    }

    /// THE test for 013: a populated 012 database survives the rebuild with
    /// its row IDs and its `manifest_id` parentage intact, and every column
    /// 005 appended (`file_type`, `link_target`) still carries its value.
    ///
    /// Three entries are seeded, and the assertions read the LAST one, so an
    /// off-by-one renumbering cannot coincidentally pass. `file_type` is the
    /// discriminator for the column-ORDER trap specific to this migration:
    /// 005 added it with `ALTER TABLE ... ADD COLUMN`, so it sits AFTER
    /// `has_acls` in the live table. A rebuild that listed the new columns in
    /// 001's order and copied with a bare positional SELECT would shift
    /// `file_type` into `groupname` and lose it, and no row count or FK check
    /// would notice.
    #[test]
    fn test_migrate_012_populated_db_to_013_preserves_ids_and_columns() {
        let mut conn = open_memory_at_012();

        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES ('u-uuid', 'u', ?1, '/tmp/u', 'active')",
            [tenant_id],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, source_path, file_count, total_size)
             VALUES (?1, 1, '/tmp/u', 3, 30)",
            [unit_id],
        )
        .unwrap();
        let snapshot_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO manifests (snapshot_id) VALUES (?1)",
            [snapshot_id],
        )
        .unwrap();
        let manifest_id = conn.last_insert_rowid();

        for (path, ft, target) in [
            ("a.txt", "regular", None::<&str>),
            ("b", "dir", None),
            ("c.lnk", "symlink", Some("../a.txt")),
        ] {
            conn.execute(
                "INSERT INTO manifest_entries
                    (manifest_id, path, size_bytes, mtime, is_directory, mode, uid, gid,
                     username, groupname, has_xattrs, has_acls, file_type, link_target)
                 VALUES (?1, ?2, 10, '2026-01-01T00:00:00Z', 0, 420, 1000, 1000,
                         'mike', 'mike', 0, 0, ?3, ?4)",
                rusqlite::params![manifest_id, path, ft, target],
            )
            .unwrap();
        }
        let last_id: i64 = conn
            .query_row(
                "SELECT id FROM manifest_entries WHERE path = 'c.lnk'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        migrate(&mut conn).unwrap();

        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk_violations, 0);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM manifest_entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3, "the rebuild lost rows");

        let (id, parent, path, ft, target, groupname): (
            i64,
            i64,
            String,
            String,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT id, manifest_id, path, file_type, link_target, groupname
                 FROM manifest_entries WHERE path = 'c.lnk'",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(id, last_id, "the rebuild renumbered manifest_entries");
        assert_eq!(parent, manifest_id);
        assert_eq!(path, "c.lnk");
        assert_eq!(ft, "symlink", "005's appended columns shifted in the copy");
        assert_eq!(target.as_deref(), Some("../a.txt"));
        assert_eq!(groupname.as_deref(), Some("mike"));

        let report = crate::cli::operations::db_fsck(&conn, false, false).unwrap();
        assert!(report.integrity_ok, "db fsck integrity check failed");
    }

    /// The columns are actually GONE, not merely unwritten — the whole point
    /// of the migration. A `SELECT` naming either must now fail.
    #[test]
    fn test_migration_013_removes_the_two_flag_columns() {
        let conn = open_memory().unwrap();
        let names: Vec<String> = table_info(&conn, "manifest_entries")
            .into_iter()
            .map(|c| c.0)
            .collect();
        assert!(!names.iter().any(|n| n == "has_xattrs"), "{names:?}");
        assert!(!names.iter().any(|n| n == "has_acls"), "{names:?}");
        assert!(
            conn.query_row("SELECT has_xattrs FROM manifest_entries", [], |r| r
                .get::<_, i64>(0))
                .is_err(),
            "has_xattrs still resolves"
        );
    }

    /// The only index the table carried is back. A rebuild that forgot it
    /// would turn every manifest lookup into a full scan of the largest
    /// table in the schema, and nothing else in the suite would notice.
    #[test]
    fn test_migration_013_recreates_the_manifest_index() {
        let conn = open_memory().unwrap();
        assert_eq!(
            index_names(&conn, "manifest_entries"),
            vec!["idx_manifest_entries_manifest"]
        );
    }

    /// 012 changes the status CHECK and nothing else: every column, type,
    /// default, notnull and pk is identical either side of it.
    ///
    /// Note what this does NOT prove: `PRAGMA table_info` does not report
    /// CHECK constraints, so this test would pass even if the rebuild had
    /// dropped the CHECK entirely. That half is
    /// `test_migration_012_offsite_rejected_four_states_accepted`'s job,
    /// and it is written with a negative assertion for exactly this reason.
    #[test]
    fn test_migration_012_changes_no_cartridge_column() {
        let before = open_memory_at_011();
        let cols_011 = table_info(&before, "cartridges");

        // Exactly the 012 schema, NOT `open_memory()` (latest): migration
        // 016 (ADR-0012 amendment, 2026-09-16; issue #197) adds
        // `cartridges.operator_serial`, a legitimate later change this test
        // must not see -- it exists to prove 012's REBUILD didn't alter any
        // column that already existed in 011, nothing about the schema's
        // current, total shape.
        let after = open_memory_at_012();
        let cols_012 = table_info(&after, "cartridges");

        assert_eq!(
            cols_011, cols_012,
            "cartridges columns/defaults/notnull/pk changed by migration 012"
        );
    }

    /// Every index the old table carried is back, plus the new location one.
    /// A rebuild that forgot 011's partial unique index would silently
    /// re-admit the duplicate medium serials it exists to prevent, and no
    /// column-shape assertion would notice.
    #[test]
    fn test_migration_012_recreates_every_index_and_adds_location() {
        let conn = open_memory().unwrap();
        assert_eq!(
            index_names(&conn, "cartridges"),
            vec![
                "idx_cartridges_barcode",
                "idx_cartridges_location",
                "idx_cartridges_serial_number",
                "idx_cartridges_status",
                // the implicit UNIQUE(barcode) autoindex
                "sqlite_autoindex_cartridges_1",
            ]
        );

        // And 011's index still ENFORCES, not merely exists.
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number)
             VALUES ('BC-1', 'LTO-6', 2500000000000, 'SERIAL1')",
            [],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number)
             VALUES ('BC-2', 'LTO-6', 2500000000000, 'SERIAL1')",
            [],
        );
        assert!(
            dup.is_err(),
            "the partial unique index on serial_number must survive the rebuild"
        );
        // ...and stay PARTIAL: two NULL serials are still fine.
        for bc in ["BC-3", "BC-4"] {
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity)
                 VALUES (?1, 'LTO-6', 2500000000000)",
                rusqlite::params![bc],
            )
            .unwrap();
        }
    }

    /// 'offsite' is gone from the CHECK and the other four still work. The
    /// negative half matters most: a rebuild that dropped the CHECK entirely
    /// would pass every other assertion in this file.
    #[test]
    fn test_migration_012_offsite_rejected_four_states_accepted() {
        let conn = open_memory().unwrap();
        for status in ["available", "in_use", "pending_erase", "retired_permanent"] {
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status)
                 VALUES (?1, 'LTO-6', 2500000000000, ?2)",
                rusqlite::params![format!("BC-{status}"), status],
            )
            .unwrap_or_else(|e| panic!("status '{status}' should be insertable: {e}"));
        }
        let err = conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status)
             VALUES ('BC-offsite', 'LTO-6', 2500000000000, 'offsite')",
            [],
        );
        assert!(
            err.is_err(),
            "'offsite' is a location, not a status (ADR-0011) — the CHECK must reject it"
        );
    }

    /// The last unpinned edge of 012's rebuild (issue #227).
    ///
    /// A create/copy/drop/rename rebuild silently drops whatever the new
    /// table's DDL forgets to restate, and `PRAGMA table_info` reports
    /// neither FK nor CHECK constraints — so
    /// `test_migration_012_changes_no_cartridge_column` would pass just as
    /// happily if the rebuild had dropped `location_id REFERENCES
    /// locations(id)`. The CHECK half of that blind spot is covered by
    /// `test_migration_012_offsite_rejected_four_states_accepted`; this is
    /// the FK half.
    ///
    /// It matters under ADR-0011 specifically: a cartridge's PLACE is a
    /// location, so `location_id` is the column that ADR made load-bearing,
    /// and an unenforced reference is how a cartridge ends up pointing at a
    /// location that no longer exists.
    ///
    /// Two assertions, and the second is the one that would actually catch a
    /// dropped constraint: the enumeration proves the edge is declared, the
    /// insert proves it is ENFORCED.
    #[test]
    fn test_migration_012_preserves_the_cartridge_location_foreign_key() {
        let before = open_memory_at_011();
        let after = open_memory().unwrap();

        let expected = vec![(
            "locations".to_string(),
            "location_id".to_string(),
            "id".to_string(),
        )];
        assert_eq!(
            foreign_keys_of(&before, "cartridges"),
            expected,
            "precondition: 011 declares exactly the one outbound FK"
        );
        assert_eq!(
            foreign_keys_of(&after, "cartridges"),
            expected,
            "012's rebuild must restate `location_id REFERENCES locations(id)` — \
             a rebuild drops any constraint its new DDL omits"
        );

        let err = after.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status, location_id)
             VALUES ('BC-dangling', 'LTO-6', 2500000000000, 'available', 99999)",
            [],
        );
        assert!(
            err.is_err(),
            "the FK must be ENFORCED after the rebuild, not merely declared: \
             a cartridge cannot sit at a location that does not exist (ADR-0011)"
        );
    }

    // --- Migration 017 (ADR-0012's 2026-09-17 amendment: `observed_condition`) ---

    /// A connection migrated to exactly the 016 schema — the last point at
    /// which `volumes.status = 'quarantined'` is still legal and
    /// `observed_condition` does not exist yet, so rows carrying the old
    /// shape can be seeded and watched through the rebuild.
    fn open_memory_at_016() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        let mut ms = vec![
            M::up(include_str!("migrations/001_initial.sql")),
            M::up(include_str!("migrations/002_fts5_catalog.sql")),
            M::up(include_str!("migrations/003_v2_lifecycle.sql")).foreign_key_check(),
            M::up(include_str!("migrations/004_volume_uuid.sql")),
            M::up(include_str!("migrations/005_file_types.sql")),
            M::up(include_str!("migrations/006_write_session_dir.sql")),
            M::up(include_str!("migrations/007_warehouse_locations.sql")),
            M::up(include_str!("migrations/008_drop_volume_storage_class.sql")),
            M::up(include_str!("migrations/009_health_tape_alerts.sql")),
            M::up(include_str!("migrations/010_stage_set_origin.sql")),
            M::up(include_str!("migrations/011_cartridge_serial_index.sql")),
            M::up(include_str!("migrations/012_cartridge_lifecycle.sql")).foreign_key_check(),
            M::up(include_str!("migrations/013_drop_manifest_entry_flags.sql")).foreign_key_check(),
            M::up(include_str!(
                "migrations/014_cartridge_binding_identity_source.sql"
            )),
            M::up(include_str!(
                "migrations/015_cartridge_load_count_unknown.sql"
            )),
            M::up(include_str!("migrations/016_cartridge_operator_serial.sql")),
        ];
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        Migrations::new(std::mem::take(&mut ms))
            .to_latest(&mut conn)
            .unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn
    }

    /// THE test for this migration. Three volumes, each pinning one of the
    /// three data-migration paths issue #242 specifies:
    ///
    /// - `Q-EVENT`: `status = 'quarantined'` with an `events` row recording
    ///   the transition into quarantine (`field = 'status'`, `new_value =
    ///   'quarantined'`, `old_value = 'sealed'`) — the verify path's shape
    ///   (`quarantine_on_medium_evidence`). Must come out `status =
    ///   'sealed'`, `observed_condition = 'quarantined'`.
    /// - `Q-NOEVENT`: `status = 'quarantined'` with NO such event — the
    ///   write-path writers' shape (`session.rs`, which never sealed). Must
    ///   come out `status = 'initialized'` (the deliberate narrowing of this
    ///   ADR's own "restore to sealed" fallback — see the migration
    ///   header), `observed_condition = 'quarantined'`.
    /// - `Q-SEALED`: an ordinary `sealed` volume, untouched by any of this.
    ///   Must come out unchanged, `observed_condition = 'ok'`.
    ///
    /// `Q-SEALED` also carries a `writes` row and a `cartridge_volumes` row
    /// — the one table holding a `REFERENCES volumes(id)` FK the rebuild
    /// must not renumber (mirrors 011->012's own id-preservation
    /// discriminator).
    #[test]
    fn test_migrate_016_populated_db_to_017_migrates_quarantine_data_and_preserves_ids_and_fk() {
        let mut conn = open_memory_at_016();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('Q-EVENT', 'lto', 'lto0', 'LTO-6', 2500000000000, 'quarantined')",
            [],
        )
        .unwrap();
        let q_event_id = conn.last_insert_rowid();
        // An older, irrelevant event first, so the "most recent" ordering is
        // a real discriminator and not vacuously the only row.
        conn.execute(
            "INSERT INTO events (entity_type, entity_id, entity_label, action, field, old_value, new_value)
             VALUES ('volume', ?1, 'Q-EVENT', 'created', NULL, NULL, NULL)",
            rusqlite::params![q_event_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events (entity_type, entity_id, entity_label, action, field, old_value, new_value)
             VALUES ('volume', ?1, 'Q-EVENT', 'verify_quarantined', 'status', 'sealed', 'quarantined')",
            rusqlite::params![q_event_id],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('Q-NOEVENT', 'lto', 'lto0', 'LTO-6', 2500000000000, 'quarantined')",
            [],
        )
        .unwrap();
        let q_noevent_id = conn.last_insert_rowid();

        conn.execute("INSERT INTO locations (name) VALUES ('home-rack')", [])
            .unwrap();
        let loc_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status, location_id)
             VALUES ('Q-SEALED', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed', ?1)",
            rusqlite::params![loc_id],
        )
        .unwrap();
        let q_sealed_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status, location_id)
             VALUES ('BC-Q', 'LTO-6', 2500000000000, 'in_use', ?1)",
            rusqlite::params![loc_id],
        )
        .unwrap();
        let cart_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
            rusqlite::params![cart_id, q_sealed_id],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('u-q', 'q-unit', ?1, 'mtime_size', 1, 'active')",
            rusqlite::params![tid],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
             VALUES (?1, 1, 'full', 'current', '/src')",
            rusqlite::params![unit_id],
        )
        .unwrap();
        let snap_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
            rusqlite::params![snap_id],
        )
        .unwrap();
        let ss_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'completed')",
            rusqlite::params![ss_id, snap_id, q_sealed_id],
        )
        .unwrap();

        // The real production migrate(), with its real FK off/on wrapping.
        migrate(&mut conn).unwrap();

        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            fk_violations, 0,
            "PRAGMA foreign_key_check found violations after the volumes rebuild"
        );

        let read = |id: i64| -> (String, String) {
            conn.query_row(
                "SELECT status, observed_condition FROM volumes WHERE id = ?1",
                rusqlite::params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(
            read(q_event_id),
            ("sealed".to_string(), "quarantined".to_string()),
            "the events row's old_value must restore status; observed_condition carries the fact forward"
        );
        assert_eq!(
            read(q_noevent_id),
            ("initialized".to_string(), "quarantined".to_string()),
            "no events row -> the write-path fallback is 'initialized', never 'sealed' \
             (this ADR's point 4, deliberately narrowed -- see the migration header)"
        );
        assert_eq!(
            read(q_sealed_id),
            ("sealed".to_string(), "ok".to_string()),
            "an ordinary sealed volume must be untouched by the migration"
        );

        // The join still names the SAME volume -- proof the ids survived,
        // not merely that they still resolve to something.
        let joined: String = conn
            .query_row(
                "SELECT v.label FROM cartridge_volumes cv
                 JOIN volumes v ON v.id = cv.volume_id
                 WHERE cv.cartridge_id = ?1",
                rusqlite::params![cart_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            joined, "Q-SEALED",
            "the rebuild renumbered volumes and silently re-pointed the join"
        );

        let write_volume_label: String = conn
            .query_row(
                "SELECT v.label FROM writes w JOIN volumes v ON v.id = w.volume_id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(write_volume_label, "Q-SEALED");

        assert_eq!(
            index_names(&conn, "volumes"),
            vec![
                "idx_volumes_location",
                "idx_volumes_status",
                "idx_volumes_uuid",
                // the implicit UNIQUE(label) autoindex
                "sqlite_autoindex_volumes_1",
            ]
        );

        let err = conn.execute(
            "UPDATE volumes SET status = 'quarantined' WHERE id = ?1",
            rusqlite::params![q_sealed_id],
        );
        assert!(
            err.is_err(),
            "'quarantined' must be rejected as a status value after the rebuild (issue #242)"
        );

        let report = crate::cli::operations::db_fsck(&conn, false, false).unwrap();
        assert!(report.integrity_ok, "db fsck integrity check failed");
    }

    /// **The migration's own CHECK must survive a volume quarantined TWICE.**
    ///
    /// Pre-017 `quarantine_on_medium_evidence` had no guard: it read whatever
    /// `status` held, wrote `'quarantined'` over it, and logged `old_value =
    /// previous_status`. Re-verifying an already-quarantined volume is a
    /// supported path — `volume verify` has always been runnable twice, and
    /// `re_verifying_an_already_quarantined_volume_reports_no_condition_change`
    /// exists precisely because it is — so a pre-017 database can legally hold
    /// `field = 'status'`, `new_value = 'quarantined'`, `old_value =
    /// 'quarantined'` as the MOST RECENT such event.
    ///
    /// A restore query that took simply the most recent event would then write
    /// `'quarantined'` back into `status`, which this very migration has just
    /// made illegal. The INSERT fails the new CHECK, the migration aborts,
    /// `db::open` fails — and every command fails with it, including the
    /// `db fsck --repair` that is supposed to be the way out. That is issue
    /// #233's bricked-database shape, manufactured by the fix for #242, on a
    /// database whose only sin was having a tape verified twice.
    ///
    /// So the restore takes the most recent transition into quarantine **from
    /// a legal status**, never merely the most recent transition into
    /// quarantine.
    #[test]
    fn test_migration_017_restores_a_twice_quarantined_volume_not_to_quarantined() {
        let mut conn = open_memory_at_016();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('Q-TWICE', 'lto', 'lto0', 'LTO-6', 2500000000000, 'quarantined')",
            [],
        )
        .unwrap();
        let q_twice_id = conn.last_insert_rowid();

        // First quarantine: sealed -> quarantined, the ordinary verify shape.
        conn.execute(
            "INSERT INTO events (entity_type, entity_id, entity_label, action, field, old_value, new_value)
             VALUES ('volume', ?1, 'Q-TWICE', 'verify_quarantined', 'status', 'sealed', 'quarantined')",
            rusqlite::params![q_twice_id],
        )
        .unwrap();
        // Second verify of the same already-quarantined tape. The pre-017
        // writer recorded the no-op transition verbatim, so `old_value` is
        // itself 'quarantined' and this row has the higher `events.id`.
        conn.execute(
            "INSERT INTO events (entity_type, entity_id, entity_label, action, field, old_value, new_value)
             VALUES ('volume', ?1, 'Q-TWICE', 'verify_quarantined', 'status', 'quarantined', 'quarantined')",
            rusqlite::params![q_twice_id],
        )
        .unwrap();

        migrate(&mut conn).expect(
                "migration 017 must not restore a status its own CHECK forbids: a volume \
                 quarantined twice carries 'quarantined' as the most recent old_value, and \
                 writing that back aborts the migration and bricks the database (issues #242, #233)",
            );

        let (status, condition): (String, String) = conn
            .query_row(
                "SELECT status, observed_condition FROM volumes WHERE id = ?1",
                rusqlite::params![q_twice_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (status.as_str(), condition.as_str()),
            ("sealed", "quarantined"),
            "the restore must reach past the no-op re-quarantine event to the real transition"
        );
    }

    /// `observed_condition` itself is a closed set of exactly two values,
    /// defaulting to `'ok'` for a row that never mentions it — the negative
    /// half matters most, matching the discipline every other CHECK test in
    /// this file follows (`test_migration_012_offsite_rejected_...`).
    #[test]
    fn test_migration_017_observed_condition_is_closed_and_defaults_ok() {
        let conn = open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes)
             VALUES ('L6-DEFAULT', 'lto', 'lto0', 'LTO-6', 2500000000000)",
            [],
        )
        .unwrap();
        let default_condition: String = conn
            .query_row(
                "SELECT observed_condition FROM volumes WHERE label = 'L6-DEFAULT'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(default_condition, "ok");

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, observed_condition)
             VALUES ('L6-QUAR', 'lto', 'lto0', 'LTO-6', 2500000000000, 'quarantined')",
            [],
        )
        .unwrap_or_else(|e| panic!("'quarantined' should be a legal observed_condition: {e}"));

        let err = conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, observed_condition)
             VALUES ('L6-BAD', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sketchy')",
            [],
        );
        assert!(
            err.is_err(),
            "an unknown observed_condition must be rejected"
        );
    }

    /// 012's own standard (issue #227), applied to 017: every column that
    /// existed before the rebuild survives with its name, type, notnull,
    /// default and pk unchanged. `observed_condition` is 017's whole point,
    /// so it cannot be folded into a blanket "nothing changed" comparison
    /// the way 012's analogous test does for `cartridges` — it is pulled out
    /// and its own shape pinned explicitly, then the rest is compared as a
    /// literal before/after equality.
    ///
    /// Note what this does NOT prove: `PRAGMA table_info` does not report
    /// CHECK constraints, so this test would pass even if the rebuild had
    /// dropped the `status` or `observed_condition` CHECK entirely. That half
    /// is `test_migration_017_observed_condition_is_closed_and_defaults_ok`
    /// (and the twice-quarantined / restores-quarantine-data tests for
    /// `status`).
    #[test]
    fn test_migration_017_changes_no_volume_column() {
        let before = open_memory_at_016();
        let cols_016 = table_info(&before, "volumes");

        let after = open_memory().unwrap();
        let mut cols_017 = table_info(&after, "volumes");

        let observed_condition_pos = cols_017
            .iter()
            .position(|c| c.0 == "observed_condition")
            .expect("017 must add observed_condition");
        let observed_condition = cols_017.remove(observed_condition_pos);
        assert_eq!(
            observed_condition,
            (
                "observed_condition".to_string(),
                "TEXT".to_string(),
                1,
                Some("'ok'".to_string()),
                0,
            ),
            "observed_condition must be NOT NULL DEFAULT 'ok'"
        );

        assert_eq!(
            cols_016, cols_017,
            "017's rebuild changed the name/type/notnull/default/pk of a \
             column that already existed in 016"
        );
    }

    /// 012's index standard (issue #227), applied to 017: every index the
    /// pre-rebuild table carried is back, and `idx_volumes_uuid` still
    /// ENFORCES uniqueness, not merely exists. This is the one most worth
    /// pinning explicitly — a duplicate volume uuid breaks
    /// `check_tape_contact`'s File 0 identity match
    /// (`docs/design/layout-session.md`), and a create/copy/drop/rename
    /// rebuild silently drops whatever its new DDL forgets to restate.
    #[test]
    fn test_migration_017_recreates_every_index_and_pins_uuid_uniqueness() {
        let conn = open_memory().unwrap();
        assert_eq!(
            index_names(&conn, "volumes"),
            vec![
                "idx_volumes_location",
                "idx_volumes_status",
                "idx_volumes_uuid",
                // the implicit UNIQUE(label) autoindex
                "sqlite_autoindex_volumes_1",
            ]
        );

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, uuid)
             VALUES ('V-1', 'lto', 'lto0', 'LTO-6', 2500000000000, '11111111-1111-1111-1111-111111111111')",
            [],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, uuid)
             VALUES ('V-2', 'lto', 'lto0', 'LTO-6', 2500000000000, '11111111-1111-1111-1111-111111111111')",
            [],
        );
        assert!(
            dup.is_err(),
            "idx_volumes_uuid must remain UNIQUE after the rebuild — a duplicate \
             uuid would break the resume/contact divergence check"
        );
    }

    /// 012's FK standard (issue #227), applied to 017's one OUTBOUND edge:
    /// `volumes.location_id REFERENCES locations(id)`. `PRAGMA table_info`
    /// reports neither FK nor CHECK constraints, so
    /// `test_migration_017_changes_no_volume_column` would pass just as
    /// happily if the rebuild had dropped this reference. Two assertions,
    /// and the second is the one that would actually catch a dropped
    /// constraint: the enumeration proves the edge is declared, the insert
    /// proves it is ENFORCED.
    #[test]
    fn test_migration_017_preserves_the_volumes_location_foreign_key() {
        let before = open_memory_at_016();
        let after = open_memory().unwrap();

        let expected = vec![(
            "locations".to_string(),
            "location_id".to_string(),
            "id".to_string(),
        )];
        assert_eq!(
            foreign_keys_of(&before, "volumes"),
            expected,
            "precondition: 016 declares exactly the one outbound FK"
        );
        assert_eq!(
            foreign_keys_of(&after, "volumes"),
            expected,
            "017's rebuild must restate `location_id REFERENCES locations(id)` — \
             a rebuild drops any constraint its new DDL omits"
        );

        let err = after.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, location_id)
             VALUES ('V-dangling', 'lto', 'lto0', 'LTO-6', 2500000000000, 99999)",
            [],
        );
        assert!(
            err.is_err(),
            "the FK must be ENFORCED after the rebuild, not merely declared"
        );
    }

    /// Makes explicit what
    /// `test_migrate_016_populated_db_to_017_migrates_quarantine_data_and_preserves_ids_and_fk`
    /// only shows indirectly (via a `cartridge_volumes` join that still
    /// resolves): the rebuild's `INSERT ... SELECT v.id, ...` copies `id`
    /// verbatim rather than letting SQLite assign fresh rowids. An explicit,
    /// out-of-sequence id (500, not 1) makes this a real discriminator — a
    /// rebuild that dropped `id` from the copy would renumber the sole row
    /// to 1, not merely leave it unchanged by coincidence. Six tables hold a
    /// `REFERENCES volumes(id)` foreign key (see the corrected migration
    /// header); every one of them depends on this.
    #[test]
    fn test_migration_017_preserves_volume_ids_across_rebuild() {
        let mut conn = open_memory_at_016();
        conn.execute(
            "INSERT INTO volumes (id, label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES (500, 'ID-PIN', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
            [],
        )
        .unwrap();

        migrate(&mut conn).unwrap();

        let after_id: i64 = conn
            .query_row("SELECT id FROM volumes WHERE label = 'ID-PIN'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            after_id, 500,
            "the rebuild must preserve the row's original id, not let SQLite \
             assign a fresh one"
        );
    }

    // --- Issue #233: an orphan blocks ordinary open; repair must still run ---

    /// Build a FILE-backed (not `:memory:`) database at exactly the 002
    /// schema — one migration short of 003, the first
    /// `.foreign_key_check()`-decorated migration — carrying one orphan
    /// row planted the way #104's fsck tests do: FK enforcement OFF for
    /// the insert, back ON afterward (re-enabling the pragma does not
    /// retroactively validate rows already there). File-backed because the
    /// whole point of these tests is reopening it as a fresh connection,
    /// the way a real process invocation does — `open`/`open_for_repair`
    /// both take a `&Path`.
    fn write_orphaned_pre_003_db(db_path: &std::path::Path) {
        let mut conn = Connection::open(db_path).unwrap();
        configure(&conn).unwrap();
        Migrations::new(vec![
            M::up(include_str!("migrations/001_initial.sql")),
            M::up(include_str!("migrations/002_fts5_catalog.sql")),
        ])
        .to_latest(&mut conn)
        .unwrap();

        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('u-orphan', 'orphan-unit', 99999, 'mtime_size', 1, 'active')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    }

    /// THE shape this whole fix depends on, verified directly per issue
    /// #233's own instruction ("verify this shape works before building on
    /// it"): a database that never migrated past 002 and already carries a
    /// dangling `units.tenant_id` makes ordinary `open()` fail outright —
    /// migration 003's whole-database FK check finds it the instant it
    /// runs — and `open()` must name this specific, repairable condition
    /// (`DatabaseNeedsRepair`) rather than the generic `Migration` variant.
    /// `open_for_repair` must nonetheless be able to open that same file,
    /// and `cli::operations::db_fsck(..., true)` must repair it — proving
    /// the repair mechanics (`pragma_foreign_key_check` /
    /// `defer_foreign_keys`) are schema-agnostic and do not depend on being
    /// at the latest migration. The very next ordinary `open()` must then
    /// migrate the now-clean database all the way to head.
    #[test]
    fn issue_233_repair_can_open_and_fix_a_database_ordinary_open_refuses() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("orphaned.db");
        write_orphaned_pre_003_db(&db_path);

        let open_err = open(&db_path).expect_err(
            "a pre-existing orphan must block ordinary open — this is the defect's premise, \
             not the part under test",
        );
        assert!(
            matches!(open_err, TapectlError::DatabaseNeedsRepair(_)),
            "open() must name this specific, repairable condition rather than a generic \
             migration failure: got {open_err:?}"
        );

        let repair_conn = open_for_repair(&db_path).expect(
            "db fsck --repair must be able to open a database ordinary open refuses, or \
             repair can never run",
        );
        assert!(
            !schema_is_current(&repair_conn).unwrap(),
            "the repair connection must still read as behind head — it never migrated"
        );
        let report = crate::cli::operations::db_fsck(&repair_conn, true, false)
            .expect("repair must succeed against the unmigrated (002) schema");
        assert_eq!(
            report.repaired, 1,
            "exactly the one planted orphan row should have been deleted"
        );
        drop(repair_conn);

        // After repair, ordinary open must migrate the now-clean database
        // all the way to head.
        let conn = open(&db_path)
            .expect("after repair, ordinary open must migrate the now-clean database to head");
        assert!(schema_is_current(&conn).unwrap());
        assert!(
            table_info(&conn, "volumes")
                .iter()
                .any(|(name, ..)| name == "observed_condition"),
            "migration must have run all the way through 017"
        );
    }

    /// `db::open`'s error for a genuinely corrupt schema (as opposed to a
    /// repairable orphan) must stay the generic `Migration` variant, never
    /// `DatabaseNeedsRepair` — issue #233's own trap: matching every
    /// migration failure the same way would hand an operator with a broken
    /// schema misleading advice about orphan rows.
    #[test]
    fn issue_233_a_non_fk_migration_failure_is_not_reported_as_repairable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("corrupt.db");

        // A schema-version claim of 1 with nothing actually migrated: the
        // next `to_latest()` tries to run migration 002's SQL against a
        // database that never ran 001, which fails on a missing table --
        // a real migration-definition problem, not a foreign key.
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }

        let err = open(&db_path).expect_err("a corrupt/inconsistent schema must fail to open");
        assert!(
            matches!(err, TapectlError::Migration(_)),
            "a non-FK migration failure must stay the generic variant, not \
             DatabaseNeedsRepair: got {err:?}"
        );
    }
}
