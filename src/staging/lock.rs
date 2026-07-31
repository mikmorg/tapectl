//! Per-stage-set `flock` (issue #98).
//!
//! `stage_create` holds an exclusive, non-blocking `flock` on a lockfile for
//! the lifetime of the stage attempt. The kernel releases a `flock` when the
//! holding process dies, however it died — SIGKILL, OOM-kill, or the box
//! losing power and rebooting — so "can I get the lock?" is a reliable
//! crash/live test with no staleness heuristic and no PID-reuse hazard.
//!
//! Uses `nix::fcntl::Flock` (the guard type), NOT the free function
//! `nix::fcntl::flock`, which has been deprecated since nix 0.28 and is
//! rejected by this crate's `-D warnings` clippy gate.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};

use crate::error::{Result, TapectlError};

/// An exclusive lock held on a stage set's lockfile. Unlocks automatically
/// on drop (via `nix::fcntl::Flock`'s own `Drop` impl) — releasing it is
/// just letting this value go out of scope.
#[allow(dead_code)]
pub struct StageLock(Flock<File>);

/// `<db_parent>/locks/stage-<stage_set_id>.lock` — derivable from the DB
/// path alone, because `db::open(path: &Path)` has no `Config` and thus no
/// other source of truth for "where does this installation keep its
/// files." Does not require the directory to exist.
pub fn lock_path(db_file: &Path, stage_set_id: i64) -> PathBuf {
    let dir = db_file.parent().unwrap_or_else(|| Path::new("."));
    dir.join("locks")
        .join(format!("stage-{stage_set_id}.lock"))
}

/// Acquire the exclusive, non-blocking lock for `stage_set_id`, creating
/// `locks/` and the lockfile itself if needed. Fails (rather than blocking)
/// if another live process already holds it — that should never happen for
/// a freshly-inserted stage_set_id, since ids are unique, but a failure
/// here must not silently proceed.
pub fn acquire(db_file: &Path, stage_set_id: i64) -> Result<StageLock> {
    let path = lock_path(db_file, stage_set_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::options()
        .create(true)
        .write(true)
        .open(&path)
        .map_err(|e| {
            TapectlError::Other(format!(
                "could not open staging lockfile {}: {e}",
                path.display()
            ))
        })?;

    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(flock) => Ok(StageLock(flock)),
        Err((_file, errno)) => Err(TapectlError::Other(format!(
            "could not acquire staging lock for stage_set {stage_set_id} \
             (held by another process?): {errno}"
        ))),
    }
}

/// Probe whether `stage_set_id`'s lock is currently free — i.e. no live
/// process holds it — WITHOUT leaving any lock held afterward.
///
/// Used by the startup sweep (`db::recover_orphaned_sessions`) to decide
/// whether a `status = 'staging'` row is a live in-flight stage (lock held,
/// returns `false`, row left untouched) or one orphaned by a crash (lock
/// free, returns `true`, row eligible to be marked `'failed'`). Any probe
/// lock this function acquires to test the free/held state is released
/// again before returning — it never holds a lock past this call.
///
/// A lockfile that can't even be opened (e.g. permissions) is treated
/// conservatively as "still live" (`false`) — this function only ever
/// classifies a row as crashed on positive, successful evidence.
pub fn is_crashed(db_file: &Path, stage_set_id: i64) -> bool {
    let path = lock_path(db_file, stage_set_id);
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    let file = match File::options().create(true).write(true).open(&path) {
        Ok(f) => f,
        Err(_) => return false,
    };

    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        // Lock was free — probe lock acquired, then immediately dropped
        // (releasing it) before returning `true`.
        Ok(flock) => {
            drop(flock);
            true
        }
        // Lock is held by a live process.
        Err((_file, _errno)) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_path_is_under_locks_dir_next_to_db() {
        let db_file = Path::new("/tmp/some/home/tapectl.db");
        let p = lock_path(db_file, 42);
        assert_eq!(p, Path::new("/tmp/some/home/locks/stage-42.lock"));
    }

    #[test]
    fn acquire_then_is_crashed_is_false_while_held() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_file = tmp.path().join("tapectl.db");
        let _lock = acquire(&db_file, 7).unwrap();
        assert!(
            !is_crashed(&db_file, 7),
            "a held lock must not be classified as crashed"
        );
    }

    #[test]
    fn is_crashed_is_true_once_the_holder_drops() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_file = tmp.path().join("tapectl.db");
        {
            let _lock = acquire(&db_file, 8).unwrap();
        } // dropped here, lock released
        assert!(
            is_crashed(&db_file, 8),
            "a lock with no holder must be classified as crashed"
        );
    }

    #[test]
    fn is_crashed_leaves_no_probe_lock_held() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_file = tmp.path().join("tapectl.db");
        {
            let _lock = acquire(&db_file, 9).unwrap();
        }
        assert!(is_crashed(&db_file, 9));
        // If the probe above leaked its lock, this second acquire would fail.
        let second = acquire(&db_file, 9);
        assert!(
            second.is_ok(),
            "is_crashed must release its probe lock before returning"
        );
    }

    #[test]
    fn is_crashed_on_a_lockfile_that_never_existed_is_true() {
        // No prior `acquire` call at all for this id — the lockfile doesn't
        // exist yet. Treated the same as "free": create it and find it
        // uncontended.
        let tmp = tempfile::TempDir::new().unwrap();
        let db_file = tmp.path().join("tapectl.db");
        assert!(is_crashed(&db_file, 999));
    }
}
