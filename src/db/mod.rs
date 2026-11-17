pub mod busy;
pub mod catalog_snapshot;
pub mod events;
pub mod export;
pub mod files;
#[allow(dead_code)]
pub mod models;
pub mod ontape_catalog;
pub mod phase_timings;
pub mod queries;
pub mod stats;

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
    refuse_another_catalog(&conn, path)?;
    configure(&conn)?;
    migrate(&mut conn)?;
    recover_orphaned_sessions(&conn, path)?;
    optimize(&conn);
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

/// The mark of a 2.x catalog: SQLite's `application_id`, set by the
/// baseline schema (the plan's D9), 0x54504354, the bytes "TPCT". tapectl
/// 1.x set none, so its catalogs read 0.
pub const APPLICATION_ID: i64 = 0x5450_4354;

/// Why a catalog of tapectl 1.x is not opened (ADR-0012 item 258): there is
/// no upgrade from 1.x, and the release that reads it is named.
const MADE_BY_1X: &str = "made by tapectl 1.x; tapectl 2.0.0 does not upgrade it \
                          (ADR-0012 item 258): start a new catalog, or use tapectl 1.9.2 \
                          for this one";

/// Refuse a database this tapectl did not make, before anything writes to
/// it (`configure()` would set WAL mode, a migration would create tables):
/// one that has tables and does not carry [`APPLICATION_ID`] is a 1.x
/// catalog (or no catalog of tapectl's) and is named, never migrated. A file
/// with no table is a new catalog. Two plain reads, so an open still never
/// needs the write lock (issue #377).
fn refuse_another_catalog(conn: &Connection, path: &Path) -> Result<()> {
    let application_id: i64 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    if application_id == APPLICATION_ID || !has_tables(conn)? {
        return Ok(());
    }
    Err(TapectlError::Migration(if application_id == 0 {
        format!("the catalog at {} was {MADE_BY_1X}", path.display())
    } else {
        format!(
            "{} is not a catalog of tapectl's (its SQLite application_id is {application_id:#x}, \
             a 2.x catalog's is {APPLICATION_ID:#x})",
            path.display()
        )
    }))
}

/// Whether the database holds any table of its own (SQLite's `sqlite_*`
/// tables aside).
fn has_tables(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master
                         WHERE type = 'table' AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\')",
        [],
        |r| r.get(0),
    )?)
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

/// The values `table.status`'s CHECK admits in the LIVE schema (every
/// migration applied), sorted -- for pinning a hand-copied closed set (a
/// `--status` filter's list, a classifier's cases) to the schema instead of
/// to a migration file that a later migration supersedes (issue #362: the
/// `is_write_target` pin read 017's text and kept passing after the CHECK
/// moved on).
#[cfg(test)]
pub(crate) fn live_status_check(table: &str) -> Vec<String> {
    let conn = open_memory().unwrap();
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |r| r.get(0),
        )
        .unwrap_or_else(|e| panic!("no table {table:?} in the live schema: {e}"));
    let flat = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let needle = "CHECK(status IN (";
    let start = flat
        .find(needle)
        .unwrap_or_else(|| panic!("no `{needle}` on {table}: {flat}"))
        + needle.len();
    let end = flat[start..]
        .find(')')
        .expect("the status CHECK closes its IN (...) list")
        + start;
    let mut set: Vec<String> = flat[start..end]
        .split(',')
        .map(|t| t.trim().trim_matches('\'').to_string())
        .filter(|t| !t.is_empty())
        .collect();
    set.sort();
    set
}

/// Give the query planner statistics (issue #373): `PRAGMA optimize=0x10002`
/// at open, as SQLite recommends for a connection opened per command. It
/// runs ANALYZE, under SQLite's default analysis limit, only on a table the
/// planner would profit from, and records the result in `sqlite_stat1`.
/// Without statistics SQLite rated `status = ?` as selective as
/// `stage_set_id = ?` and drove the coverage queries quadratic; migration
/// 027 dropped the indexes that made that plan possible, and this is the
/// backstop for the next index that does the same.
///
/// Best-effort and never waiting. ANALYZE writes, so it needs the write
/// lock, and `db::busy`'s rule 1 is that a command which only reads never
/// waits for it. The busy timeout is 0 for this one statement: if another
/// process is writing, the statistics wait for the next open, and a failure
/// of any kind only logs.
fn optimize(conn: &Connection) {
    let run = || -> rusqlite::Result<()> {
        conn.pragma_update(None, "busy_timeout", 0)?;
        let r = conn.execute_batch("PRAGMA optimize=0x10002");
        conn.pragma_update(None, "busy_timeout", BUSY_TIMEOUT_MS)?;
        r
    };
    if let Err(e) = run() {
        tracing::debug!(error = %e, "PRAGMA optimize skipped");
        let _ = conn.pragma_update(None, "busy_timeout", BUSY_TIMEOUT_MS);
    }
}

/// The wait for another connection's lock before `SQLITE_BUSY` (`db::busy`).
const BUSY_TIMEOUT_MS: i64 = 5000;

/// Set WAL mode and other pragmas.
fn configure(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", BUSY_TIMEOUT_MS)?;
    Ok(())
}

fn migrations() -> Migrations<'static> {
    let mut ms = vec![
        // The baseline schema of tapectl 2.x (ADR-0012 item 258(b), the
        // plan's D9). It replaces the 41 migrations of 1.x; a 1.x catalog is
        // refused by name, never migrated. Until `v2.0.0` is tagged it is
        // edited in place; after the tag a change is a numbered migration
        // again, registered after it.
        M::up(include_str!("migrations/001_baseline.sql")),
    ];
    ms.extend(pending_test_migration());
    Migrations::new(ms)
}

/// The environment variable that gives a DEBUG build of the binary one more,
/// empty, migration carrying `.foreign_key_check()` (T3.1 step 6). With one
/// baseline there is no migration left for a catalog to be behind on, so
/// issue #233's repair door (an orphan blocks `db::open`, `db fsck --repair`
/// opens it through [`open_for_repair`] and says the schema "has not
/// finished migrating", the next command migrates) could not otherwise be
/// shown through the real binary until a 2.x migration exists. A release
/// build never reads it: the migration bumps `user_version`, and a catalog
/// once opened with it would be "too far ahead" for the same binary without.
pub const PENDING_TEST_MIGRATION_ENV: &str = "TAPECTL_TEST_PENDING_MIGRATION";

#[cfg(test)]
thread_local! {
    /// The same seam for the library's own tests, per thread so that no
    /// other test sees the extra migration.
    static PENDING_TEST_MIGRATION: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// The test-only migration [`PENDING_TEST_MIGRATION_ENV`] asks for: no SQL,
/// and the whole-database foreign-key check every rebuilding migration
/// carries.
fn pending_test_migration() -> Option<M<'static>> {
    #[cfg(test)]
    let in_test = PENDING_TEST_MIGRATION.with(std::cell::Cell::get);
    #[cfg(not(test))]
    let in_test = false;
    let wanted = in_test
        || (cfg!(debug_assertions) && std::env::var_os(PENDING_TEST_MIGRATION_ENV).is_some());
    wanted.then(|| M::up("").foreign_key_check())
}

fn migrate(conn: &mut Connection) -> Result<()> {
    migrate_to(conn, None)
}

/// Run the production migration list up to `target` (`None` = the latest),
/// with the foreign-key handling and error mapping every caller needs.
/// `migrate` is the production entry; a test of one migration re-targets it
/// with `Some(version)` so a later migration cannot move its claim.
fn migrate_to(conn: &mut Connection, target: Option<usize>) -> Result<()> {
    // A migration that rebuilds a table (create, copy, drop, rename) drops a
    // table other rows reference, which SQLite refuses while
    // `foreign_keys` is ON, as `configure()` leaves it. rusqlite_migration
    // 2.5.0 does not toggle the pragma itself (`goto_up` in the vendored
    // source opens one transaction per call and never touches it), and
    // toggling it inside a migration's SQL is a no-op once that transaction
    // is open. So it is turned off here, outside the transaction, per steps 1
    // and 12 of SQLite's 12-step "Making Other Kinds Of Table Schema Changes"
    // procedure; step 10, the pre-commit foreign-key check, is
    // `.foreign_key_check()` on such a migration.
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let ms = migrations();
    let run = match target {
        None => ms.to_latest(conn),
        Some(v) => ms.to_version(conn, v),
    };
    let result = run.map_err(|e| {
        // Issue #233: the message is computed before matching on `e` by
        // value below (matching moves it), and the match is on the typed
        // `rusqlite_migration::Error::ForeignKeyCheck` variant, never on
        // this string — a corrupt schema must not get repair advice that
        // does not apply to it.
        let msg = e.to_string();
        match e {
            MigrationError::ForeignKeyCheck(_) => TapectlError::DatabaseNeedsRepair(msg),
            // Issue #362: a migration that refuses on purpose does it with
            // `RAISE(ABORT, <message>)`, which SQLite reports as
            // SQLITE_CONSTRAINT_TRIGGER. That message is written for the
            // operator, so it is shown as it is -- `rusqlite_migration`'s
            // Display would put the whole migration script in front of it.
            // Typed on the extended code, never on the text; a CHECK failure
            // (SQLITE_CONSTRAINT_CHECK) and everything else keep the full
            // `msg` below.
            MigrationError::RusqliteError {
                err: rusqlite::Error::SqliteFailure(failure, Some(raised)),
                ..
            } if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER => {
                TapectlError::Migration(raised)
            }
            _ => TapectlError::Migration(msg),
        }
    });
    conn.pragma_update(None, "foreign_keys", "ON")?;
    result
}

/// Open the database WITHOUT running migrations — for `db fsck --repair`
/// only (issue #233).
///
/// `.foreign_key_check()` (on any migration that rebuilds a table; the
/// baseline has none to carry) runs an explicit
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
    refuse_another_catalog(&conn, path)?;
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

/// The schema version this binary migrates a catalog to: the number of
/// migrations it carries, which is what `PRAGMA user_version` reads once
/// every one has run.
pub fn latest_schema_version() -> Result<i64> {
    let conn = Connection::open_in_memory()?;
    let pending = migrations()
        .pending_migrations(&conn)
        .map_err(|e| TapectlError::Migration(e.to_string()))?;
    Ok(i64::from(pending))
}

/// What `db import` found when it checked the file it is about to put in
/// the live catalog's place (issue #530).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSource {
    /// The source's `PRAGMA user_version`: how many of this binary's
    /// migrations it has had. Below [`ImportSource::latest`] the next
    /// ordinary open migrates it forward.
    pub schema_version: i64,
    /// This binary's [`latest_schema_version`].
    pub latest: i64,
}

/// The tables every catalog of tapectl's has had since migration 001. A file
/// without them is not a catalog of tapectl's, whatever its `user_version` says.
const CATALOG_TABLES: [&str; 5] = ["tenants", "units", "snapshots", "stage_sets", "volumes"];

/// Run `f` on `path` opened read only, then remove the `-wal` and `-shm`
/// SQLite made beside it to read it (issue #530). A read-only connection to
/// a WAL-mode file (a `db backup` copy is one) creates both, and cannot
/// remove them when it closes; `db import`'s check and copy of a backup, and
/// its dry run, must leave the backup's directory as they found it. Only a
/// sidecar that was not there before is removed: one that was is the
/// source's own.
pub fn with_read_only<T>(path: &Path, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    use rusqlite::OpenFlags;
    let made_here: Vec<std::path::PathBuf> = ["-wal", "-shm"]
        .iter()
        .map(|suffix| sidecar_path(path, suffix))
        .filter(|sidecar| sidecar.symlink_metadata().is_err())
        .collect();
    let result = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(TapectlError::from)
    .and_then(|conn| f(&conn));
    for sidecar in made_here {
        let _ = std::fs::remove_file(sidecar);
    }
    result
}

/// Check a file before `db import` puts it where the live catalog is (issue
/// #530): it opens as an SQLite database, read only; it passes `PRAGMA
/// integrity_check`; it is a catalog of tapectl's (the core tables, and a schema
/// version above zero); and this binary can open it (a schema version no
/// newer than [`latest_schema_version`]). Each failure is refused by name,
/// before anything is replaced. `db import` used to copy any file page for
/// page, so a damaged backup became the live catalog, and a backup from a
/// newer binary left every later command failing with `DatabaseTooFarAhead`,
/// `db import` included.
///
/// Read only (`SQLITE_OPEN_READ_ONLY`, [`with_read_only`]): the check never
/// writes to the source, a file that is not a database is never turned into
/// one, and the `-wal`/`-shm` SQLite makes to read it are removed again.
pub fn check_import_source(path: &Path) -> Result<ImportSource> {
    let refuse = |why: String| {
        TapectlError::Other(format!(
            "db import refuses {}: {why}; nothing was replaced",
            path.display()
        ))
    };
    with_read_only(path, |conn| check_import_conn(conn, &refuse)).map_err(|e| match e {
        TapectlError::Database(e) => {
            refuse(format!("it does not open as an SQLite database ({e})"))
        }
        other => other,
    })
}

/// [`check_import_source`]'s checks, on the source opened read only.
fn check_import_conn(
    conn: &Connection,
    refuse: &dyn Fn(String) -> TapectlError,
) -> Result<ImportSource> {
    // The first statement is where SQLite reads the header, so a file that
    // is not a database fails here rather than at the open.
    let integrity: Vec<String> = conn
        .prepare("PRAGMA integrity_check")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(|e| refuse(format!("it does not read as an SQLite database ({e})")))?;
    if !(integrity.len() == 1 && integrity[0] == "ok") {
        let shown: Vec<&str> = integrity.iter().take(3).map(String::as_str).collect();
        let more = if integrity.len() > shown.len() {
            format!(" (and {} more)", integrity.len() - shown.len())
        } else {
            String::new()
        };
        return Err(refuse(format!(
            "it fails SQLite's integrity check: {}{more}. Import an older backup",
            shown.join("; ")
        )));
    }
    let mut missing = Vec::new();
    for table in CATALOG_TABLES {
        let present: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |r| r.get(0),
        )?;
        if !present {
            missing.push(table);
        }
    }
    let schema_version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if !missing.is_empty() || schema_version <= 0 {
        return Err(refuse(format!(
            "it is not a catalog of tapectl's (schema version {schema_version}{})",
            if missing.is_empty() {
                String::new()
            } else {
                format!("; no {} table", missing.join(", no "))
            }
        )));
    }
    // Before the version check, which would call a 1.x catalog (version 41)
    // a newer tapectl's.
    let application_id: i64 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    if application_id != APPLICATION_ID {
        return Err(refuse(if application_id == 0 {
            format!("it is a catalog {MADE_BY_1X}")
        } else {
            format!(
                "it is not a catalog of tapectl's (its SQLite application_id is \
                 {application_id:#x}, a 2.x catalog's is {APPLICATION_ID:#x})"
            )
        }));
    }
    let latest = latest_schema_version()?;
    if schema_version > latest {
        return Err(refuse(format!(
            "it is a catalog of schema version {schema_version}, newer than this tapectl's \
             {latest}: a newer tapectl wrote it, and this one cannot open it. Import it with \
             that tapectl (`tapectl --version` names this one)"
        )));
    }
    Ok(ImportSource {
        schema_version,
        latest,
    })
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
///
/// **Lock-aware, all three tables (issues #98, #376).** A row's status alone
/// cannot tell a crashed session from one running right now in another
/// process, and this sweep runs on every `db::open()`, including every
/// read-only command and timer. Until #376 the `writes` and
/// `verification_sessions` arms rewrote every `in_progress` row regardless,
/// so the rule "a row still `in_progress` at open means a live writer"
/// (`docs/design/layout-session.md`'s rehydrate contract, and the `volume
/// abort`/`volume resume` refusals built on it) could never be observed by
/// the command asking. Now each candidate is probed through its lock: a
/// write or verify session through its volume's lock
/// (`staging::lock::volume_session_live`), a stage set through its own
/// (`staging::lock::is_crashed`). Held ⇒ a live process owns it ⇒ left
/// untouched. Free ⇒ crashed ⇒ swept.
///
/// **Reads first, writes only on candidates (issue #377).** Every arm
/// SELECTs, and UPDATEs only when a crashed row exists, so an open with
/// nothing to recover never needs SQLite's write lock — a read-only command
/// opens and runs while another command holds it.
fn recover_orphaned_sessions(conn: &Connection, db_path: &Path) -> Result<()> {
    let mut updated = 0usize;
    for volume_id in in_progress_volumes(conn, "writes", "status")? {
        if !crate::staging::lock::volume_session_live(conn, volume_id) {
            updated += conn.execute(
                "UPDATE writes SET status = 'interrupted'
                 WHERE volume_id = ?1 AND status = 'in_progress'",
                [volume_id],
            )?;
        }
    }
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
    // `report copies`, `catalog list`) could destroy staging data; actual
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

    let mut updated = 0usize;
    for volume_id in in_progress_volumes(conn, "verification_sessions", "outcome")? {
        if !crate::staging::lock::volume_session_live(conn, volume_id) {
            updated += conn.execute(
                "UPDATE verification_sessions SET outcome = 'aborted'
                 WHERE volume_id = ?1 AND outcome = 'in_progress'",
                [volume_id],
            )?;
        }
    }
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

/// The volumes with at least one `in_progress` row in `table` (`writes` by
/// `status`, `verification_sessions` by `outcome`) — the sweep's candidates.
/// A plain SELECT, so finding none needs no write lock (issue #377).
fn in_progress_volumes(conn: &Connection, table: &str, column: &str) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT DISTINCT volume_id FROM {table} WHERE {column} = 'in_progress'"
    ))?;
    let ids = stmt
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<Vec<i64>, _>>()?;
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file-backed catalog holding one volume with one `in_progress`
    /// write session and one `in_progress` verification session — the
    /// shape a live `volume write` is in during its confirm (issue #376).
    /// Returns `(db_path, volume_id)`; the connection is closed.
    fn live_session_catalog(tmp: &tempfile::TempDir) -> (std::path::PathBuf, i64) {
        let db_path = tmp.path().join("tapectl.db");
        let conn = open(&db_path).unwrap();
        conn.execute_batch(
            "INSERT INTO tenants (name) VALUES ('alpha');
             INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                 VALUES ('u-1', 'photos', 1, 'mtime_size', 1, 'active');
             INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
                 VALUES (1, 1, 'staged', '/tmp/photos', 1, 10);
             INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (1, 'staged', 524288);
             INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
                 VALUES ('L6-0001', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized');",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (1, 1, 1, 'in_progress')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO verification_sessions (volume_id, verify_type, outcome)
             VALUES (1, 'full', 'in_progress')",
            [],
        )
        .unwrap();
        (db_path, 1)
    }

    fn session_states(conn: &Connection) -> (String, String) {
        (
            conn.query_row("SELECT status FROM writes", [], |r| r.get(0))
                .unwrap(),
            conn.query_row("SELECT outcome FROM verification_sessions", [], |r| {
                r.get(0)
            })
            .unwrap(),
        )
    }

    /// Issue #376: with the volume's lock held (another open file
    /// description — the kernel treats it exactly as another process's),
    /// opening the catalog leaves the live session's rows alone. Released —
    /// the positive control — the same open sweeps both.
    #[test]
    fn open_sweeps_a_session_only_once_its_volume_lock_is_free() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (db_path, volume_id) = live_session_catalog(&tmp);

        let holder_conn = Connection::open(&db_path).unwrap();
        let holder =
            crate::staging::lock::acquire_volume(&holder_conn, volume_id, "L6-0001").unwrap();
        let conn = open(&db_path).unwrap();
        assert_eq!(
            session_states(&conn),
            ("in_progress".to_string(), "in_progress".to_string()),
            "a live session must survive another command's open"
        );
        drop(conn);

        drop(holder);
        let conn = open(&db_path).unwrap();
        assert_eq!(
            session_states(&conn),
            ("interrupted".to_string(), "aborted".to_string()),
            "once the lock is free, the open sweeps the crashed session"
        );
    }

    /// Issue #377: opening the catalog needs no write lock when there is
    /// nothing to recover, so a read-only command opens and reads while
    /// another connection holds a write transaction. The holder's lock is
    /// real (the negative control: a write from the second connection
    /// fails busy), and the open finishes well inside its 5-second wait.
    #[test]
    fn open_succeeds_while_another_connection_holds_the_write_lock() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("tapectl.db");
        drop(open(&db_path).unwrap());

        let holder = Connection::open(&db_path).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = std::time::Instant::now();
        let conn = open(&db_path).expect("a read-only open must not need the write lock");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "open waited {:?} — something at open wanted the write lock",
            started.elapsed()
        );
        let _: i64 = conn
            .query_row("SELECT COUNT(*) FROM volumes", [], |r| r.get(0))
            .unwrap();

        conn.pragma_update(None, "busy_timeout", 50).unwrap();
        let write = conn.execute("INSERT INTO tenants (name) VALUES ('x')", []);
        assert!(
            matches!(&write, Err(e) if busy::is_busy(e)),
            "negative control: the holder must really hold the write lock: {write:?}"
        );
        holder.execute_batch("ROLLBACK").unwrap();
    }

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

    fn user_version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    // --- Issue #233: an orphan blocks ordinary open; repair must still run ---

    /// Runs `f` with this thread's [`migrations`] carrying the test-only
    /// pending migration ([`PENDING_TEST_MIGRATION_ENV`]'s, through the
    /// thread-local seam), and clears it again however `f` ends.
    fn with_pending_test_migration<T>(f: impl FnOnce() -> T) -> T {
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                PENDING_TEST_MIGRATION.with(|p| p.set(false));
            }
        }
        PENDING_TEST_MIGRATION.with(|p| p.set(true));
        let _clear = Clear;
        f()
    }

    /// A file-backed catalog at the baseline carrying one orphan row
    /// (`units.tenant_id` naming no tenant), planted the way #104's fsck
    /// tests do: FK enforcement OFF for the insert, back ON afterward
    /// (re-enabling the pragma does not validate rows already there).
    /// File-backed because these tests reopen it as a fresh connection, the
    /// way a real process invocation does.
    fn write_orphaned_baseline_db(db_path: &std::path::Path) {
        let conn = open(db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('u-orphan', 'orphan-unit', 99999, 'mtime_size', 1, 'active')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    }

    /// THE shape issue #233's fix depends on: a catalog one migration
    /// behind head, where that migration carries `.foreign_key_check()`,
    /// and that already holds a dangling `units.tenant_id`. Ordinary
    /// `open()` fails and must name this specific, repairable condition
    /// (`DatabaseNeedsRepair`) rather than the generic `Migration` variant;
    /// `open_for_repair` must still open the same file, read as behind
    /// head, and `cli::operations::db_fsck(..., true)` must repair it; the
    /// next ordinary `open()` then migrates it to head. The migration behind
    /// is the test-only one (T3.1 step 6): with one baseline, no real one is
    /// left to be behind on.
    #[test]
    fn issue_233_repair_can_open_and_fix_a_database_ordinary_open_refuses() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("orphaned.db");
        write_orphaned_baseline_db(&db_path);

        with_pending_test_migration(|| {
            let open_err = open(&db_path).expect_err(
                "a pre-existing orphan must block ordinary open — this is the defect's \
                 premise, not the part under test",
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
                .expect("repair must succeed against the unmigrated schema");
            assert_eq!(
                report.repaired, 1,
                "exactly the one planted orphan row should have been deleted"
            );
            drop(repair_conn);

            let conn = open(&db_path)
                .expect("after repair, ordinary open must migrate the now-clean database to head");
            assert!(schema_is_current(&conn).unwrap());
            assert_eq!(user_version(&conn), 2, "the pending migration ran");
        });
    }

    /// `db::open`'s error for a genuinely broken schema (as opposed to a
    /// repairable orphan) must stay the generic `Migration` variant, never
    /// `DatabaseNeedsRepair` — issue #233's own trap: matching every
    /// migration failure the same way would hand an operator with a broken
    /// schema misleading advice about orphan rows.
    #[test]
    fn issue_233_a_non_fk_migration_failure_is_not_reported_as_repairable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("corrupt.db");

        // A catalog that claims no migration yet but already holds one of
        // the baseline's tables: the baseline's CREATE TABLE fails on it --
        // a schema problem, not a foreign key.
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
                .unwrap();
        }

        let err = open(&db_path).expect_err("a corrupt/inconsistent schema must fail to open");
        assert!(
            matches!(err, TapectlError::Migration(_)),
            "a non-FK migration failure must stay the generic variant, not \
             DatabaseNeedsRepair: got {err:?}"
        );
    }

    /// The seam is per thread and off by default: an ordinary open has the
    /// one baseline, and nothing pending.
    #[test]
    fn the_pending_test_migration_is_off_unless_asked_for() {
        let conn = open_memory().unwrap();
        assert_eq!(user_version(&conn), 1);
        assert!(schema_is_current(&conn).unwrap());
        assert_eq!(latest_schema_version().unwrap(), 1);
        with_pending_test_migration(|| {
            assert!(!schema_is_current(&conn).unwrap());
            assert_eq!(latest_schema_version().unwrap(), 2);
        });
        assert!(schema_is_current(&conn).unwrap());
    }

    // --- A 1.x catalog is refused by name (ADR-0012 item 258, the plan's D9) ---

    /// The 2.x catalog's mark: SQLite's `application_id`, "TPCT".
    const TAPECTL_2X_APPLICATION_ID: i64 = 0x5450_4354;

    fn application_id(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA application_id", [], |r| r.get(0))
            .unwrap()
    }

    /// A file shaped as 1.x left a catalog: tables, `user_version` 41, no
    /// `application_id` (1.x set none), WAL mode.
    fn write_1x_catalog(db_path: &std::path::Path) {
        let conn = Connection::open(db_path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta (key, value) VALUES ('schema_version', '1');
             CREATE TABLE tenants (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
             INSERT INTO tenants (name) VALUES ('operator');
             PRAGMA user_version = 41;",
        )
        .unwrap();
    }

    fn assert_names_1x(text: &str) {
        for needle in [
            "made by tapectl 1.x",
            "tapectl 2.0.0 does not upgrade it (ADR-0012 item 258)",
            "start a new catalog, or use tapectl 1.9.2 for this one",
        ] {
            assert!(text.contains(needle), "{needle:?} not in: {text}");
        }
    }

    #[test]
    fn a_new_catalog_carries_the_2x_application_id() {
        assert_eq!(
            application_id(&open_memory().unwrap()),
            TAPECTL_2X_APPLICATION_ID
        );
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("tapectl.db");
        drop(open(&db_path).unwrap());
        let conn = open(&db_path).expect("a 2.x catalog opens again");
        assert_eq!(application_id(&conn), TAPECTL_2X_APPLICATION_ID);
        // An empty file (no tables) is a new catalog, not a 1.x one.
        let empty = tmp.path().join("empty.db");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(
            application_id(&open(&empty).unwrap()),
            TAPECTL_2X_APPLICATION_ID
        );
    }

    /// A 1.x catalog is refused by name on open, as a `Migration` error (the
    /// exit code of every other catalog that cannot be opened), and the file
    /// is left exactly as it was: no migration, no `application_id`, no row.
    #[test]
    fn a_1x_catalog_is_refused_by_name_and_left_as_it_was() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("tapectl.db");
        write_1x_catalog(&db_path);

        let err = open(&db_path).expect_err("a 1.x catalog must not open");
        assert!(matches!(err, TapectlError::Migration(_)), "{err:?}");
        assert_names_1x(&err.to_string());
        assert!(
            err.to_string().contains(&db_path.display().to_string()),
            "{err}"
        );

        let err = open_for_repair(&db_path).expect_err("nor open for repair");
        assert_names_1x(&err.to_string());

        let raw = Connection::open(&db_path).unwrap();
        assert_eq!(user_version(&raw), 41);
        assert_eq!(application_id(&raw), 0);
        let tables: i64 = raw
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 2, "nothing was created in it");
    }

    /// `db import` of a 1.x backup is refused the same way, before the
    /// newer-schema check could call it a newer tapectl's.
    #[test]
    fn db_import_refuses_a_1x_catalog_by_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("backup.db");
        write_1x_catalog(&src);
        // The core tables, so it reads as a catalog of tapectl's.
        {
            let conn = Connection::open(&src).unwrap();
            conn.execute_batch(
                "CREATE TABLE units (id INTEGER PRIMARY KEY);
                 CREATE TABLE snapshots (id INTEGER PRIMARY KEY);
                 CREATE TABLE stage_sets (id INTEGER PRIMARY KEY);
                 CREATE TABLE volumes (id INTEGER PRIMARY KEY);",
            )
            .unwrap();
        }
        let err = check_import_source(&src).expect_err("a 1.x backup must not import");
        let text = err.to_string();
        assert_names_1x(&text);
        assert!(text.contains("nothing was replaced"), "{text}");
        assert!(!text.contains("newer"), "{text}");

        // Positive control: a 2.x catalog's file passes the same check.
        let ok = tmp.path().join("ok.db");
        drop(open(&ok).unwrap());
        check_import_source(&ok).expect("a 2.x catalog imports");
    }

    // --- The schema's own rules ---

    /// `readback_checkpoints` (issue #410) refuses a second row for one
    /// file of one readback and a negative position, and its rows go with
    /// their verification session.
    #[test]
    fn readback_checkpoints_are_one_per_file_and_go_with_their_session() {
        let conn = open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('CK', 'lto', 'lto0', 'LTO-6', 1, 'initialized')",
            [],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO verification_sessions (volume_id, verify_type) VALUES (?1, 'full')",
            [volume_id],
        )
        .unwrap();
        let session_id = conn.last_insert_rowid();
        let insert = |position: i64| {
            conn.execute(
                "INSERT INTO readback_checkpoints (session_id, position, sha256, front_index_sha256)
                 VALUES (?1, ?2, 'aa', 'bb')",
                rusqlite::params![session_id, position],
            )
        };
        insert(4).unwrap();
        assert!(insert(4).is_err(), "one row per file per readback");
        assert!(insert(-1).is_err(), "a position is never negative");
        conn.execute(
            "DELETE FROM verification_sessions WHERE id = ?1",
            [session_id],
        )
        .unwrap();
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM readback_checkpoints", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(left, 0);
    }

    /// Issue #417: `audit`'s Heir Kit check — the newest event of one action
    /// — is one seek on the index on `(action, timestamp)`. The control
    /// drops that index on the same catalog and sees the plan lose it.
    #[test]
    fn the_newest_event_of_an_action_is_an_index_seek() {
        const Q: &str = "EXPLAIN QUERY PLAN \
            SELECT MAX(timestamp) FROM events WHERE action = 'escrow_kit_generated'";
        let plan = |conn: &Connection| -> String {
            let mut stmt = conn.prepare(Q).unwrap();
            let rows: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            rows.join("; ")
        };
        let conn = open_memory().unwrap();
        let with = plan(&conn);
        assert!(
            with.contains("USING COVERING INDEX idx_events_action_timestamp"),
            "{with}"
        );
        conn.execute_batch("DROP INDEX idx_events_action_timestamp")
            .unwrap();
        let without = plan(&conn);
        assert!(
            !without.contains("idx_events_action_timestamp"),
            "positive control, without the index: {without}"
        );
    }

    /// ADR-0012 item 295: `writes` carries no column of Layout 2's rejected
    /// end-of-tape salvage. The control reads a column it does carry.
    #[test]
    fn writes_carries_no_end_of_tape_salvage_column() {
        let conn = open_memory().unwrap();
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('writes')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(columns.iter().any(|c| c == "write_verified"), "{columns:?}");
        for gone in ["eot_recovery", "sacrificed_slice_id"] {
            assert!(!columns.iter().any(|c| c == gone), "{gone}: {columns:?}");
        }
    }

    /// The live `writes.status` vocabulary, pinned to the schema rather than
    /// to a schema file's text (`live_status_check`'s reason).
    #[test]
    fn the_live_write_status_check_is_the_eight_states() {
        assert_eq!(
            live_status_check("writes"),
            [
                "aborted",
                "completed",
                "cut",
                "failed",
                "in_progress",
                "interrupted",
                "planned",
                "range"
            ]
        );
    }

    /// `rusqlite_migration` numbers a migration by its position in
    /// [`migrations`], not by its file name, so a file whose prefix is not
    /// its position applies under another number than the one its header,
    /// the docs and every later branch use. Two batches that each took "the
    /// next number" would land that way; this refuses it, and a gap or a
    /// file nothing includes.
    #[test]
    fn every_migration_file_is_named_for_its_position() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/db");
        let source = std::fs::read_to_string(dir.join("mod.rs")).unwrap();
        let body = &source[source.find("fn migrations()").unwrap()..];
        let body = &body[..body.find("\n}\n").unwrap()];
        // Some entries wrap `include_str!(` and its path onto two lines.
        let included: Vec<String> = body
            .match_indices("\"migrations/")
            .map(|(i, m)| {
                let rest = &body[i + m.len()..];
                rest[..rest.find('"').unwrap()].to_string()
            })
            .collect();
        for (i, name) in included.iter().enumerate() {
            assert_eq!(
                name[..3].parse::<usize>().unwrap(),
                i + 1,
                "{name} is migration {} in migrations()",
                i + 1
            );
        }
        let mut on_disk: Vec<String> = std::fs::read_dir(dir.join("migrations"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        on_disk.sort();
        assert_eq!(
            on_disk, included,
            "every file, and only those, included in order"
        );
        assert_eq!(
            user_version(&open_memory().unwrap()),
            included.len() as i64,
            "one migration per file"
        );
    }
}
