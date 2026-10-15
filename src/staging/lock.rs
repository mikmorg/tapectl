//! Per-stage-set, per-volume and per-drive `flock`s (issues #98, #376,
//! #309 — the drive lock is described where it is defined, below).
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

/// Take volume `volume_id`'s session lock for a command that CHANGES the
/// volume's status or place (issue #461): `volume retire`, `cartridge
/// retire`, `cartridge mark-erased`, `volume compact-finish`, `volume move`.
/// Non-blocking, like [`acquire_volume`]; when a live session holds it, this
/// refuses with [`TapectlError::VolumeBusy`] — which exits 75, "nothing was
/// changed, run it again" — naming the holding process where the kernel
/// will say who it is ([`lock_holder_pids`]). `action` finishes the
/// sentence "refusing to …". Hold the result for as long as the command
/// reads the volume's state and writes the change, so the two see one volume.
pub fn acquire_volume_to_change(
    conn: &Connection,
    volume_id: i64,
    label: &str,
    action: &str,
) -> Result<VolumeLock> {
    match acquire_volume(conn, volume_id, label) {
        Err(TapectlError::VolumeSessionLive { .. }) => Err(TapectlError::VolumeBusy {
            label: label.to_string(),
            action: action.to_string(),
            holders: volume_holders(conn, volume_id),
        }),
        other => other,
    }
}

/// ` (held by pid N: `tapectl volume write L6-0001 …`)` for the process(es)
/// holding `volume_id`'s lock, or an empty string when the kernel does not
/// say (`/proc/locks` unreadable, a holder in another PID namespace). Best
/// effort: this only decorates a refusal.
fn volume_holders(conn: &Connection, volume_id: i64) -> String {
    let Some(db_file) = db_file_of(conn) else {
        return String::new();
    };
    let pids = lock_holder_pids(&volume_lock_path(&db_file, volume_id));
    if pids.is_empty() {
        return String::new();
    }
    let named: Vec<String> = pids
        .iter()
        .map(|pid| match process_command_line(*pid) {
            Some(cmd) => format!("pid {pid}: `{cmd}`"),
            None => format!("pid {pid}"),
        })
        .collect();
    format!(" (held by {})", named.join("; "))
}

/// `/proc/<pid>/cmdline` as one line, cut at 120 characters; `None` when the
/// process is gone or unreadable.
fn process_command_line(pid: u32) -> Option<String> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let line: String = String::from_utf8_lossy(&raw)
        .split('\0')
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if line.is_empty() {
        return None;
    }
    Some(if line.chars().count() > 120 {
        let cut: String = line.chars().take(120).collect();
        format!("{cut}…")
    } else {
        line
    })
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

// ── The drive lock (ADR-0012, 2026-10-07 item 29; issue #309) ──────────────
//
// `tapectl drive poll` reads the drive's log pages on a timer, and page 0x2E
// (TapeAlert) may be read-to-clear: a poll that read it in the middle of a
// command would take the alerts that command's own post-command sweep should
// have recorded against its contact. So a command that contacts the drive
// holds the drive's lock from the moment its contact opens until the process
// exits — through the post-command sweep, which on the read paths runs after
// the contact has closed — and the poll takes the same lock WITHOUT waiting
// and reads nothing when it cannot (exit 75).
//
// One lock per physical tape node, under the home's `locks/`, keyed by the
// node the configured path resolves to (`/dev/tape/by-id/…-nst` and the
// `/dev/nstN` it points at are one lock). Held in a process-wide registry
// rather than a guard on the stack: the contact that takes it and the sweep
// that must run under it live in different frames on several paths, and a
// process that contacts the drive twice (`volume compact`'s two steps,
// `collection run`'s copies) must not try to lock it against itself — two
// `flock`s on two open file descriptions conflict even inside one process.
// The kernel releases it when the process exits, however it exits.

/// How long a command waits for a drive lock another process holds before
/// it warns and carries on without it. A poll holds it for seconds.
const DRIVE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// The drive locks this process holds, by canonical lockfile path. Never
/// released before exit.
static HELD_DRIVES: std::sync::Mutex<Vec<(PathBuf, Flock<File>)>> =
    std::sync::Mutex::new(Vec::new());

/// `<db_parent>/locks/drive-<node>.lock`, `<node>` the basename of the tape
/// node `device_tape` resolves to (`nst0` for `/dev/nst0` or any by-id link
/// to it). Never reclaimed, for the volume lock's reason: unlinking a held
/// lockfile would let the next taker lock a fresh inode at the same path.
pub fn drive_lock_path(db_file: &Path, device_tape: &str) -> PathBuf {
    let dir = db_file.parent().unwrap_or_else(|| Path::new("."));
    let node = fs::canonicalize(device_tape)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .or_else(|| {
            Path::new(device_tape)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    let key: String = node
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir.join("locks").join(format!("drive-{key}.lock"))
}

/// Open (creating) `path` and return it with its canonical spelling, the
/// registry's key — so a catalog opened through a symlink and the same
/// catalog opened directly name one lock.
fn open_drive_lockfile(path: &Path) -> Result<(PathBuf, File)> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| {
            TapectlError::Other(format!(
                "could not open the drive lockfile {}: {e}",
                path.display()
            ))
        })?;
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    Ok((canonical, file))
}

/// Take `device_tape`'s drive lock for the rest of this process, WITHOUT
/// waiting. `Ok(true)`: this process holds it (now, or already did).
/// `Ok(false)`: another process holds it — a tapectl command has the drive.
pub fn try_hold_drive(db_file: &Path, device_tape: &str) -> Result<bool> {
    let (key, file) = open_drive_lockfile(&drive_lock_path(db_file, device_tape))?;
    let mut held = HELD_DRIVES.lock().unwrap_or_else(|p| p.into_inner());
    if held.iter().any(|(k, _)| *k == key) {
        return Ok(true);
    }
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(flock) => {
            held.push((key, flock));
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

/// A contact is opening on `device_tape` (`tape::contact`): hold its drive
/// lock for the rest of this process, waiting up to [`DRIVE_LOCK_WAIT`] for
/// a poll to finish. Infallible, like the contact itself — recording a
/// contact never refuses a tape command: a lock that cannot be had (another
/// command really has the drive, which the `st` driver will refuse on its
/// own; or an unwritable `locks/`) is a warning, and the command proceeds
/// as it did before the lock existed. An in-memory catalog takes no lock.
pub fn hold_drive_for_contact(conn: &Connection, device_tape: &str) {
    let Some(db_file) = db_file_of(conn) else {
        return;
    };
    let deadline = std::time::Instant::now() + DRIVE_LOCK_WAIT;
    let mut _wait = None;
    loop {
        match try_hold_drive(&db_file, device_tape) {
            Ok(true) => return,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(err = %e, device = device_tape, "drive lock not taken");
                return;
            }
        }
        if std::time::Instant::now() >= deadline || crate::signal::is_interrupted() {
            tracing::warn!(
                device = device_tape,
                lock = %drive_lock_path(&db_file, device_tape).display(),
                "another process has held this drive's lock for {}s; proceeding without it — \
                 a `tapectl drive poll` that starts now may read this command's log pages",
                DRIVE_LOCK_WAIT.as_secs()
            );
            return;
        }
        if _wait.is_none() {
            _wait = Some(crate::progress::wait(|| {
                format!("the drive lock for {device_tape} (a drive poll, or another command)")
            }));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// How `device_tape`'s drive lock stands, for a reader that must not take it
/// (`drive status`, issue #428).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveLockState {
    /// Nobody holds it: no tapectl command is in contact with the drive.
    Free,
    /// A tapectl command holds it (from its contact until its exit).
    Held,
    /// The lockfile could not be opened or tested, so nothing is known.
    Unknown(String),
}

/// Test `device_tape`'s drive lock WITHOUT keeping it: a non-blocking
/// `flock` taken and released at once, the probe [`is_crashed`] and
/// [`volume_session_live`] make for their locks. Unlike [`drive_lock_held`]
/// (which counts a lockfile it cannot open as held, the conservative answer
/// for a sweep) an unopenable lockfile is [`DriveLockState::Unknown`], so a
/// reader can say it does not know instead of reporting a hold it never saw.
///
/// A hold taken by THIS process's registry ([`try_hold_drive`]) conflicts
/// with the probe's own descriptor and reads as [`DriveLockState::Held`],
/// which is true.
pub fn drive_lock_state(db_file: &Path, device_tape: &str) -> DriveLockState {
    let path = drive_lock_path(db_file, device_tape);
    let (_, file) = match open_drive_lockfile(&path) {
        Ok(opened) => opened,
        Err(e) => return DriveLockState::Unknown(e.to_string()),
    };
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(flock) => {
            drop(flock);
            DriveLockState::Free
        }
        Err((_, nix::errno::Errno::EWOULDBLOCK)) => DriveLockState::Held,
        Err((_, errno)) => DriveLockState::Unknown(format!("flock {}: {errno}", path.display())),
    }
}

/// The ids of the volumes with a live session lock right now: every
/// `locks/volume-<id>.lock` beside the catalog `conn` is attached to that
/// another process holds ([`volume_session_live`]). Only lockfiles that
/// already exist are looked at, so asking creates none. An in-memory catalog
/// has no lockfiles and no other process.
pub fn live_volume_sessions(conn: &Connection) -> Vec<i64> {
    let Some(db_file) = db_file_of(conn) else {
        return Vec::new();
    };
    let dir = db_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("locks");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut ids: Vec<i64> = entries
        .filter_map(|e| {
            let name = e.ok()?.file_name().into_string().ok()?;
            name.strip_prefix("volume-")?
                .strip_suffix(".lock")?
                .parse::<i64>()
                .ok()
        })
        .filter(|id| volume_session_live(conn, *id))
        .collect();
    ids.sort_unstable();
    ids
}

/// The pids of the processes holding an `flock` on `lockfile`, from the
/// kernel's `/proc/locks`. An `flock` has no owner API, so this is the only
/// way to put a pid to "held"; it is best-effort and says so by returning
/// nothing: `/proc/locks` unreadable, the file gone, the holder in a PID
/// namespace this one cannot name (the kernel shows pid 0 then).
pub fn lock_holder_pids(lockfile: &Path) -> Vec<u32> {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = fs::metadata(lockfile) else {
        return Vec::new();
    };
    let Ok(text) = fs::read_to_string("/proc/locks") else {
        return Vec::new();
    };
    let dev = meta.dev();
    parse_proc_locks(
        &text,
        nix::sys::stat::major(dev),
        nix::sys::stat::minor(dev),
        meta.ino(),
    )
}

/// The pids of the `flock` holders of file `ino` on device `major:minor` in
/// `/proc/locks` text — one lock per line,
/// `1: FLOCK  ADVISORY  WRITE 1661 fd:10:2883617 0 EOF`, the device in hex and
/// the inode in decimal. A waiter's line (`1: -> FLOCK …`) is not a holder.
fn parse_proc_locks(text: &str, major: u64, minor: u64, ino: u64) -> Vec<u32> {
    let mut pids: Vec<u32> = text
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.contains(&"->") {
                return None;
            }
            let at = f.iter().position(|w| *w == "FLOCK")?;
            // FLOCK ADVISORY <READ|WRITE> <pid> <maj:min:ino> <start> <end>
            let pid: u32 = f.get(at + 3)?.parse().ok()?;
            let mut dev = f.get(at + 4)?.split(':');
            let lock_major = u64::from_str_radix(dev.next()?, 16).ok()?;
            let lock_minor = u64::from_str_radix(dev.next()?, 16).ok()?;
            let lock_ino: u64 = dev.next()?.parse().ok()?;
            (pid > 0 && (lock_major, lock_minor, lock_ino) == (major, minor, ino)).then_some(pid)
        })
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// Whether some process OTHER than this one's registry holds
/// `device_tape`'s drive lock — a probe that leaves nothing held. The test
/// seam for "a contact holds the drive"; production asks through
/// [`try_hold_drive`].
#[cfg(test)]
pub(crate) fn drive_lock_held(db_file: &Path, device_tape: &str) -> bool {
    let Ok((_, file)) = open_drive_lockfile(&drive_lock_path(db_file, device_tape)) else {
        return true;
    };
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(flock) => {
            drop(flock);
            false
        }
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_drive_lock_is_one_file_per_tape_node_under_locks() {
        let db_file = Path::new("/tmp/some/home/tapectl.db");
        assert_eq!(
            drive_lock_path(db_file, "/nonexistent/dev/nst7"),
            Path::new("/tmp/some/home/locks/drive-nst7.lock")
        );
        // A link to a node is the node's lock (canonicalised).
        let tmp = tempfile::TempDir::new().unwrap();
        let node = tmp.path().join("nst3");
        std::fs::write(&node, b"").unwrap();
        let link = tmp.path().join("scsi-XYZ-nst");
        std::os::unix::fs::symlink(&node, &link).unwrap();
        assert_eq!(
            drive_lock_path(db_file, link.to_str().unwrap()),
            drive_lock_path(db_file, node.to_str().unwrap()),
        );
        assert_eq!(
            drive_lock_path(db_file, "/dev/../weird name"),
            Path::new("/tmp/some/home/locks/drive-weird_name.lock"),
            "a name is reduced to a safe file name"
        );
    }

    #[test]
    fn try_hold_drive_is_refused_while_another_holder_has_it_and_reentrant_in_process() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_file = tmp.path().join("tapectl.db");
        // Another holder: its own open file description, as another
        // process would have.
        let (_, file) = open_drive_lockfile(&drive_lock_path(&db_file, "/x/nst9")).unwrap();
        let other = Flock::lock(file, FlockArg::LockExclusiveNonblock).unwrap();
        assert!(
            !try_hold_drive(&db_file, "/x/nst9").unwrap(),
            "held elsewhere"
        );
        drop(other);
        assert!(try_hold_drive(&db_file, "/x/nst9").unwrap(), "free: taken");
        assert!(
            try_hold_drive(&db_file, "/x/nst9").unwrap(),
            "a second take in this process is the same hold, not a self-conflict"
        );
        assert!(drive_lock_held(&db_file, "/x/nst9"), "and it stays held");
    }

    /// Issue #428: `drive status` reads the drive lock without keeping it.
    #[test]
    fn the_drive_lock_state_is_read_and_left_free() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_file = tmp.path().join("tapectl.db");
        assert_eq!(
            drive_lock_state(&db_file, "/x/nst4"),
            DriveLockState::Free,
            "positive control: nobody holds it"
        );
        // The probe kept nothing: another taker gets it.
        let (_, file) = open_drive_lockfile(&drive_lock_path(&db_file, "/x/nst4")).unwrap();
        let other = Flock::lock(file, FlockArg::LockExclusiveNonblock)
            .expect("the probe released its lock");
        assert_eq!(drive_lock_state(&db_file, "/x/nst4"), DriveLockState::Held);
        drop(other);
        assert_eq!(drive_lock_state(&db_file, "/x/nst4"), DriveLockState::Free);
    }

    #[test]
    fn a_lockfile_that_cannot_be_opened_is_unknown_not_held() {
        let tmp = tempfile::TempDir::new().unwrap();
        // `locks` is a FILE, so the lockfile's directory cannot be made.
        std::fs::write(tmp.path().join("locks"), b"").unwrap();
        let db_file = tmp.path().join("tapectl.db");
        assert!(matches!(
            drive_lock_state(&db_file, "/x/nst4"),
            DriveLockState::Unknown(_)
        ));
    }

    #[test]
    fn proc_locks_names_the_holder_of_one_inode_and_not_a_waiter() {
        let text = "\
1: POSIX  ADVISORY  READ 3638 fd:10:17566478 128 128
2: FLOCK  ADVISORY  WRITE 1661 fd:10:2883617 0 EOF
3: FLOCK  ADVISORY  WRITE 1662 fd:10:2883616 0 EOF
3: -> FLOCK  ADVISORY  WRITE 1700 fd:10:2883617 0 EOF
4: FLOCK  ADVISORY  WRITE 1661 fd:11:2883617 0 EOF
5: FLOCK  ADVISORY  WRITE 0 fd:10:2883617 0 EOF
";
        assert_eq!(parse_proc_locks(text, 0xfd, 0x10, 2883617), vec![1661]);
        assert_eq!(parse_proc_locks(text, 0xfd, 0x10, 2883616), vec![1662]);
        assert!(parse_proc_locks(text, 0xfd, 0x10, 99).is_empty());
        assert!(parse_proc_locks("", 0xfd, 0x10, 2883617).is_empty());
    }

    /// The real `/proc/locks`, against a lock this very process holds: its
    /// own pid. A pid-namespace sandbox that hides it would show pid 0 and
    /// fail here, which is the honest signal that the lookup is blind.
    #[test]
    fn the_holder_of_a_real_flock_is_found_in_proc_locks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_file = tmp.path().join("tapectl.db");
        let path = drive_lock_path(&db_file, "/x/nst6");
        assert!(lock_holder_pids(&path).is_empty(), "no lockfile, no holder");
        let (_, file) = open_drive_lockfile(&path).unwrap();
        assert!(lock_holder_pids(&path).is_empty(), "an unheld file");
        let _held = Flock::lock(file, FlockArg::LockExclusiveNonblock).unwrap();
        assert_eq!(lock_holder_pids(&path), vec![std::process::id()]);
    }

    #[test]
    fn live_volume_sessions_lists_only_held_volume_locks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_file = tmp.path().join("tapectl.db");
        let conn = Connection::open(&db_file).unwrap();
        assert!(live_volume_sessions(&conn).is_empty(), "no locks directory");
        // Volume 5's session is live; 6's lockfile is left over from a
        // finished one; a stage lock is not a volume's.
        let (_, held) = open_drive_lockfile(&volume_lock_path(&db_file, 5)).unwrap();
        let _live = Flock::lock(held, FlockArg::LockExclusiveNonblock).unwrap();
        open_drive_lockfile(&volume_lock_path(&db_file, 6)).unwrap();
        open_drive_lockfile(&lock_path(&db_file, 5)).unwrap();
        assert_eq!(live_volume_sessions(&conn), vec![5]);
    }

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
