//! Per-stage-set and per-volume `flock`s (issues #98, #376).
//!
//! `stage_create` holds an exclusive, non-blocking `flock` on a lockfile for
//! the lifetime of the stage attempt; `volume init`, `volume write` and
//! `volume resume` (through confirm) and `volume verify` hold one per volume
//! for the lifetime of their session (issue #376). The kernel releases a
//! `flock` when the holding process dies, however it died — SIGKILL,
//! OOM-kill, or the box losing power and rebooting — so "can I get the
//! lock?" is a reliable crash/live test with no staleness heuristic and no
//! PID-reuse hazard.
//!
//! A live session is therefore a fact the kernel answers, not a status
//! column's inference. Before #376 the open-time sweep rewrote every
//! `in_progress` row on every `db::open`, so the rule "a row still
//! `in_progress` means another process is writing" could never be observed
//! by the command that asked it.
//!
//! Uses `nix::fcntl::Flock` (the guard type), NOT the free function
//! `nix::fcntl::flock`, which has been deprecated since nix 0.28 and is
//! rejected by this crate's `-D warnings` clippy gate.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};
use rusqlite::Connection;

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
    dir.join("locks").join(format!("stage-{stage_set_id}.lock"))
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
        .truncate(false)
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

/// The staging admission lock (issue #368), held while one `stage create`
/// checks staging space and records its stage set. Unlocks on drop.
#[allow(dead_code)]
pub struct AdmissionLock(Flock<File>);

/// `<db_parent>/locks/stage-admission.lock` (issue #368). Never reclaimed:
/// it is one fixed path every stage shares, and unlinking it under a holder
/// would let a second stage lock a fresh inode at the same path.
pub fn admission_lock_path(db_file: &Path) -> PathBuf {
    let dir = db_file.parent().unwrap_or_else(|| Path::new("."));
    dir.join("locks").join("stage-admission.lock")
}

/// Take the admission lock, waiting for it (issue #368). Two stages
/// admitted at once could each see the other's unit as not yet staging and
/// the other's space as not yet spoken for; under this lock the second
/// sees the first's stage set, committed and locked, before it decides.
/// It is held for milliseconds, except while a stage asks the operator
/// about staging space — then the next stage waits for the answer rather
/// than asking over it.
pub fn acquire_admission(db_file: &Path) -> Result<AdmissionLock> {
    let path = admission_lock_path(db_file);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| {
            TapectlError::Other(format!(
                "could not open the staging admission lockfile {}: {e}",
                path.display()
            ))
        })?;
    let mut file = file;
    let mut _wait = None;
    loop {
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(flock) => return Ok(AdmissionLock(flock)),
            Err((back, _)) => file = back,
        }
        if _wait.is_none() {
            _wait = Some(crate::progress::wait(|| {
                "another stage create checking staging space".into()
            }));
        }
        // Blocking would not notice a signal; a short sleep between tries
        // does, and the lock is held for milliseconds.
        crate::signal::check(|| "stopped while waiting to start staging".into())?;
        std::thread::sleep(std::time::Duration::from_millis(50));
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
    let file = match File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
    {
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

/// An exclusive lock held on a volume's session lockfile (issue #376).
/// Unlocks on drop, like [`StageLock`]. Bind it to a NAMED local
/// (`let _volume_lock = ...`): `let _ = ...` drops it on the spot. `None`
/// inside means an in-memory catalog, which takes no lock ([`db_file_of`]).
#[allow(dead_code)]
pub struct VolumeLock(Option<Flock<File>>);

/// `<db_parent>/locks/volume-<volume_id>.lock` (issue #376).
///
/// Never reclaimed (`staging::clean`'s lockfile reclaim removes only
/// `stage-*` files), and that is deliberate: every session on a volume
/// reuses its id, so unlinking the file while a holder has it open would let
/// the next session lock a fresh inode at the same path while the old holder
/// still believes it holds the lock — two "exclusive" holders. Stage-set ids
/// are never reused, which is why only their files may go.
pub fn volume_lock_path(db_file: &Path, volume_id: i64) -> PathBuf {
    let dir = db_file.parent().unwrap_or_else(|| Path::new("."));
    dir.join("locks").join(format!("volume-{volume_id}.lock"))
}

/// The database file `conn` is attached to, or `None` for an in-memory or
/// temporary database. No other process can open one of those, so there is
/// no other session to exclude and no lock to take. SQLite reports them as
/// `Some("")`, which must never become `./locks/...`.
pub fn db_file_of(conn: &Connection) -> Option<PathBuf> {
    conn.path().filter(|p| !p.is_empty()).map(PathBuf::from)
}

/// Acquire volume `volume_id`'s session lock for the catalog `conn` is
/// attached to (issue #376). Non-blocking: if another process holds it, a
/// session is live on this volume right now, and this refuses with
/// [`TapectlError::VolumeSessionLive`] — an ADR-0008 Tier-3 fact that
/// `--force` never crosses.
pub fn acquire_volume(conn: &Connection, volume_id: i64, label: &str) -> Result<VolumeLock> {
    let Some(db_file) = db_file_of(conn) else {
        return Ok(VolumeLock(None));
    };
    let path = volume_lock_path(&db_file, volume_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| {
            TapectlError::Other(format!(
                "could not open volume lockfile {}: {e}",
                path.display()
            ))
        })?;
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(flock) => Ok(VolumeLock(Some(flock))),
        Err((_file, _errno)) => Err(TapectlError::VolumeSessionLive {
            label: label.to_string(),
        }),
    }
}

/// Whether a session is live on `volume_id` right now: its lock is held by
/// another process, or by another open file description in this one (issue
/// #376). The same probe as [`is_crashed`], inverted, with the same
/// conservative rule: a lockfile that cannot be opened counts as LIVE, so
/// nothing is ever swept, aborted or released on a failed probe. An
/// in-memory catalog has no other process and is never live.
pub fn volume_session_live(conn: &Connection, volume_id: i64) -> bool {
    let Some(db_file) = db_file_of(conn) else {
        return false;
    };
    let path = volume_lock_path(&db_file, volume_id);
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return true;
        }
    }
    let file = match File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(_) => return true,
    };
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        // Free: the probe lock is released again before returning.
        Ok(flock) => {
            drop(flock);
            false
        }
        Err((_file, _errno)) => true,
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
