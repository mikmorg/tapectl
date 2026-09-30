//! The busy policy: what tapectl does when another process holds the
//! catalog's write lock (issue #377).
//!
//! SQLite in WAL mode lets any number of readers run beside one writer, but
//! only one writer at a time. Every connection waits `busy_timeout` (5 s,
//! `db::configure`) for that lock and then fails with `SQLITE_BUSY`. Before
//! #377 that failure was fatal wherever it landed, including after hours of
//! finished staging work. The rules:
//!
//! 1. **Reading never takes the write lock.** Opening the catalog runs the
//!    crash sweep, and the sweep SELECTs its candidates first and writes
//!    only when there is something to recover (`recover_orphaned_sessions`).
//!    A read-only command (`report`, `audit`, `db backup`) therefore opens
//!    and runs while another command writes.
//! 2. **A transaction that reads and then writes starts IMMEDIATE**
//!    ([`immediate_tx`]). A DEFERRED one takes a read snapshot at its first
//!    read and upgrades at its first write; if another connection committed
//!    in between, the upgrade fails at once with `SQLITE_BUSY_SNAPSHOT`, and
//!    `busy_timeout` does not apply to that case. IMMEDIATE takes the write
//!    lock at `BEGIN`, where `busy_timeout` does apply.
//! 3. **Long writers retry** ([`retry`]). A write that records work already
//!    done — a stage set's slices and its finalization, a tape session's
//!    cursor rows, its seal and its confirm — retries `SQLITE_BUSY` and
//!    `SQLITE_LOCKED` with backoff for [`BusyPolicy::DEFAULT`]'s budget
//!    (ten minutes) rather than throwing that work away. Past the budget it
//!    fails with [`TapectlError::CatalogBusy`].
//! 4. **"Catalog busy" is not a verdict.** It has its own error
//!    ([`TapectlError::CatalogBusy`], or a busy `rusqlite` error anywhere in
//!    the chain, [`is_catalog_busy`]) and its own exit code
//!    ([`crate::error::EXIT_CATALOG_BUSY`], 75), which the `contrib/` timer
//!    wrappers report as "retry later", never as a violation.
//! 5. **A lock error never discards staged work.** `stage create` does not
//!    run its failed-stage cleanup for a busy error (`staging::mod`), and a
//!    slice's `stage_slices` row is recorded before its plaintext is deleted.
//!
//! Short interactive writes (`unit tag`, `location add`) keep the plain
//! 5-second wait: failing fast with a clear message is right for a command
//! a person just typed.

use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode, Transaction, TransactionBehavior};

use crate::error::{Result, TapectlError};

/// How long a long writer keeps retrying a busy catalog, and how it backs
/// off. A parameter, never a global, so a test can shrink it without
/// touching other tests running in the same process.
#[derive(Debug, Clone, Copy)]
pub struct BusyPolicy {
    /// Total time to keep retrying, measured from the first failure.
    pub budget: Duration,
    /// The first pause; doubled after each failure up to `max_pause`.
    pub first_pause: Duration,
    pub max_pause: Duration,
}

impl BusyPolicy {
    /// Ten minutes: longer than any lock holder measured in production (a
    /// 154-second stage finalization, #372), short enough that a wedged
    /// holder is reported the same hour. Each attempt also waits the
    /// connection's own `busy_timeout` before failing.
    pub const DEFAULT: BusyPolicy = BusyPolicy {
        budget: Duration::from_secs(600),
        first_pause: Duration::from_millis(100),
        max_pause: Duration::from_secs(5),
    };
}

/// Whether `e` is SQLite saying another connection holds a lock this one
/// needs: `SQLITE_BUSY` (including its `BUSY_SNAPSHOT` and `BUSY_RECOVERY`
/// extended codes) or `SQLITE_LOCKED`.
pub fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy) | Some(ErrorCode::DatabaseLocked)
    )
}

/// [`is_busy`] for a crate error — the shape [`retry`] classifies.
pub fn is_busy_error(e: &TapectlError) -> bool {
    match e {
        TapectlError::Database(inner) => is_busy(inner),
        TapectlError::CatalogBusy(_) => true,
        _ => false,
    }
}

/// Whether any error in `err`'s chain is a busy catalog — what `main` maps
/// to [`crate::error::EXIT_CATALOG_BUSY`]. Walks the chain because a busy
/// open reaches `main` wrapped in context ("failed to open database").
pub fn is_catalog_busy(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        if let Some(e) = cause.downcast_ref::<TapectlError>() {
            is_busy_error(e)
        } else if let Some(e) = cause.downcast_ref::<rusqlite::Error>() {
            is_busy(e)
        } else {
            false
        }
    })
}

/// Run `op` until it succeeds, fails with something other than a busy
/// catalog, or `policy.budget` runs out — then [`TapectlError::CatalogBusy`]
/// naming `what`. `op` must be safe to run again after a busy failure: a
/// single statement, or a whole transaction (a transaction dropped on error
/// rolls back, so the retry starts clean).
pub fn retry<T>(policy: BusyPolicy, what: &str, mut op: impl FnMut() -> Result<T>) -> Result<T> {
    let started = Instant::now();
    let mut pause = policy.first_pause;
    // Issue #386: from the first busy answer until the operation lands (or
    // gives up), this is a named wait in the session log.
    let mut waiting: Option<crate::progress::Wait> = None;
    loop {
        match op() {
            Err(e) if is_busy_error(&e) => {
                if waiting.is_none() {
                    waiting = Some(crate::progress::wait(|| {
                        format!("a busy catalog, to record {what}")
                    }));
                }
                if started.elapsed() >= policy.budget {
                    return Err(TapectlError::CatalogBusy(format!(
                        "could not record {what} within {}s ({e})",
                        policy.budget.as_secs()
                    )));
                }
                tracing::warn!(what, error = %e, "catalog busy — retrying");
                std::thread::sleep(pause);
                pause = (pause * 2).min(policy.max_pause);
            }
            other => return other,
        }
    }
}

/// Begin an IMMEDIATE transaction on `conn` (rule 2 above). The `&Connection`
/// form of `transaction_with_behavior`, like `unchecked_transaction` is of
/// `transaction`: it fails at runtime, rather than at compile time, if a
/// transaction is already open on `conn`.
pub fn immediate_tx(conn: &Connection) -> Result<Transaction<'_>> {
    Ok(Transaction::new_unchecked(
        conn,
        TransactionBehavior::Immediate,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUICK: BusyPolicy = BusyPolicy {
        budget: Duration::from_millis(300),
        first_pause: Duration::from_millis(10),
        max_pause: Duration::from_millis(50),
    };

    /// Two connections to one file-backed WAL catalog; `b` waits only
    /// 50 ms per attempt so a held lock is seen quickly.
    fn two_connections(tmp: &tempfile::TempDir) -> (Connection, Connection) {
        let path = tmp.path().join("tapectl.db");
        let a = crate::db::open(&path).unwrap();
        let b = crate::db::open(&path).unwrap();
        b.pragma_update(None, "busy_timeout", 50).unwrap();
        (a, b)
    }

    #[test]
    fn retry_rides_out_a_lock_held_past_busy_timeout() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("tapectl.db");
        let (_a, b) = two_connections(&tmp);

        // The holder: another connection in another thread holds the write
        // lock for 400 ms — eight times `b`'s busy_timeout.
        let (tx_ready, rx_ready) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let c = Connection::open(&path).unwrap();
            c.execute_batch("BEGIN IMMEDIATE").unwrap();
            tx_ready.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(400));
            c.execute_batch("COMMIT").unwrap();
        });
        rx_ready.recv().unwrap();

        // Positive control: without retry the same write fails busy.
        let bare = b.execute("INSERT INTO tenants (name) VALUES ('bare')", []);
        assert!(
            matches!(&bare, Err(e) if is_busy(e)),
            "the holder must make an unretried write fail busy: {bare:?}"
        );

        let policy = BusyPolicy {
            budget: Duration::from_secs(10),
            ..QUICK
        };
        retry(policy, "a tenant", || {
            b.execute("INSERT INTO tenants (name) VALUES ('retried')", [])
                .map_err(Into::into)
        })
        .expect("retry must outlast the holder");
        holder.join().unwrap();
    }

    #[test]
    fn retry_gives_up_as_catalog_busy_after_its_budget() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (a, b) = two_connections(&tmp);
        a.execute_batch("BEGIN IMMEDIATE").unwrap();
        let err = retry(QUICK, "a tenant", || {
            b.execute("INSERT INTO tenants (name) VALUES ('x')", [])
                .map_err(Into::into)
        })
        .unwrap_err();
        assert!(matches!(err, TapectlError::CatalogBusy(_)), "{err:?}");
        assert!(is_catalog_busy(&anyhow::Error::from(err)));
        a.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn retry_does_not_retry_other_errors() {
        let mut calls = 0;
        let err = retry(QUICK, "x", || -> Result<()> {
            calls += 1;
            Err(TapectlError::Other("not busy".into()))
        })
        .unwrap_err();
        assert_eq!(calls, 1);
        assert!(!is_catalog_busy(&anyhow::Error::from(err)));
    }

    /// Rule 2's proof. A DEFERRED read-then-write whose snapshot went stale
    /// fails at once with SQLITE_BUSY_SNAPSHOT (the negative control); the
    /// same work under [`immediate_tx`] succeeds.
    #[test]
    fn immediate_tx_avoids_busy_snapshot_under_a_concurrent_writer() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (a, b) = two_connections(&tmp);
        b.pragma_update(None, "busy_timeout", 2000).unwrap();

        // DEFERRED: read (snapshot taken), another connection commits, write.
        let tx = b.unchecked_transaction().unwrap();
        let _: i64 = tx
            .query_row("SELECT COUNT(*) FROM tenants", [], |r| r.get(0))
            .unwrap();
        a.execute("INSERT INTO tenants (name) VALUES ('other')", [])
            .unwrap();
        let stale = tx.execute("INSERT INTO tenants (name) VALUES ('mine')", []);
        match &stale {
            Err(rusqlite::Error::SqliteFailure(f, _)) => assert_eq!(
                f.extended_code,
                rusqlite::ffi::SQLITE_BUSY_SNAPSHOT,
                "negative control: expected BUSY_SNAPSHOT, got {stale:?}"
            ),
            other => panic!("negative control: expected BUSY_SNAPSHOT, got {other:?}"),
        }
        drop(tx);

        // IMMEDIATE: the write lock is taken at BEGIN, so the read cannot go
        // stale; the other writer's commit lands before or after, never
        // in between.
        let tx = immediate_tx(&b).unwrap();
        let _: i64 = tx
            .query_row("SELECT COUNT(*) FROM tenants", [], |r| r.get(0))
            .unwrap();
        tx.execute("INSERT INTO tenants (name) VALUES ('mine')", [])
            .unwrap();
        tx.commit().unwrap();
    }
}
