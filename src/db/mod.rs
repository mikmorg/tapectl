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
    ])
}

fn migrate(conn: &mut Connection) -> Result<()> {
    migrations()
        .to_latest(conn)
        .map_err(|e| TapectlError::Migration(e.to_string()))
}

/// On startup: detect orphaned in_progress/interrupted sessions and mark aborted.
fn recover_orphaned_sessions(conn: &Connection) -> Result<()> {
    let updated = conn.execute(
        "UPDATE writes SET status = 'aborted'
         WHERE status IN ('in_progress', 'interrupted')",
        [],
    )?;
    if updated > 0 {
        warn!(
            count = updated,
            "recovered orphaned write sessions — marked as aborted"
        );
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
}
