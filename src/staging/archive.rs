//! The archive pass of `stage create` on GNU tar (`volume-format-v3.md` §3;
//! ADR-0012 items 253, 254, 265, 270, 272, 274, 277, 283 to 287; the plan's
//! T4.5, D3, D12, D15, D22; invariants 5, 7, 11, 13).
//!
//! - [`scan`]: the snapshot's own entries, in bytewise path order (never a
//!   new walk), each looked at on disk before tar runs: its kind, its
//!   device, inode, size, mtime and ctime, and whether it has an ACL
//!   (step 3).
//! - [`StageSink`]: the stream reader's sink. Each member goes to the
//!   cutter (`staging::cut`), which places it in a slice; each slice goes
//!   through `files::SliceWriter` (packed alone when the staging is
//!   compressed, then age-encrypted, both in memory) and is recorded as it
//!   closes. Each checked member's row is kept for the file table.
//! - [`recheck`]: after tar, each entry against what [`scan`] saw; a file
//!   inside the racy window is hashed from disk and compared with the
//!   stream's hash (step 5).

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;

use rusqlite::{params, Connection};

use super::cut::{Cutter, Slices};
use super::files::{packed_bound, SliceWriter, StagingDir};
use super::pack::Packing;
use super::validate::{changed_while_staging, hash_source_file, now_ns, Seen, RACY_WINDOW_NS};
use crate::db::busy::BusyPolicy;
use crate::db::files::FileKind;
use crate::error::{Result, TapectlError};
use crate::tar::{AclOnDisk, Listed, Sink};
use crate::volume::filetable::{Kind, Row};

/// One entry of the list tar is given, and what it was on disk before tar
/// ran.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) listed: Listed,
    pub(crate) seen: Seen,
}

/// What [`scan`] saw.
#[derive(Debug)]
pub(crate) struct Scan {
    pub(crate) entries: Vec<Entry>,
    /// When the scan began, in ns since the epoch, on the clock file times
    /// are stamped with: the racy window is measured from it.
    pub(crate) started_ns: i128,
}

impl Scan {
    /// The paths, in the list's order, for `tar::write_list`.
    pub(crate) fn paths(&self) -> Vec<&str> {
        self.entries
            .iter()
            .map(|e| e.listed.path.as_str())
            .collect()
    }
}

/// Whether `path` has the extended attribute `name` (never following a
/// symbolic link: tar archives the link itself).
fn has_attribute(path: &CString, name: &str) -> bool {
    let name = CString::new(name).expect("no NUL in an attribute name");
    // SAFETY: NUL-terminated strings; a null buffer of length 0 asks only
    // for the value's size.
    let rc = unsafe { nix::libc::lgetxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
    rc >= 0
}

fn kind_of(ft: &std::fs::FileType) -> Option<Kind> {
    Some(if ft.is_dir() {
        Kind::Folder
    } else if ft.is_file() {
        Kind::File
    } else if ft.is_symlink() {
        Kind::Symlink
    } else if ft.is_fifo() {
        Kind::Fifo
    } else if ft.is_char_device() {
        Kind::CharDevice
    } else if ft.is_block_device() {
        Kind::BlockDevice
    } else {
        return None;
    })
}

/// Whether the snapshot's coarser kind admits what is on disk now.
fn kind_agrees(recorded: FileKind, now: Kind) -> bool {
    match recorded {
        FileKind::Dir => now == Kind::Folder,
        FileKind::Regular => now == Kind::File,
        FileKind::Symlink => now == Kind::Symlink,
        FileKind::Special => matches!(now, Kind::Fifo | Kind::CharDevice | Kind::BlockDevice),
    }
}

/// Step 3: the snapshot's entries, sorted bytewise by path, each looked at
/// on disk under `root`. The unit's dotfile is listed when it is on disk,
/// at the size it has now (it is not content, ADR-0012 2026-10-07 item 24),
/// and left out when it is not. Any other entry gone, or of another kind
/// than the snapshot recorded, is refused: the snapshot no longer describes
/// the unit. A socket is never listed (item 270; the snapshot leaves it
/// out).
pub(crate) fn scan(conn: &Connection, snapshot_id: i64, root: &Path, unit: &str) -> Result<Scan> {
    let started_ns = now_ns();
    let mut stmt = conn.prepare(&format!(
        "SELECT p.path, fv.kind FROM {} WHERE fv.snapshot_id = ?1",
        crate::db::files::VERSION_FILES
    ))?;
    let mut recorded: Vec<(String, FileKind)> = stmt
        .query_map(params![snapshot_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, FileKind>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let dotfile = crate::unit::dotfile::UNIT_DOTFILE;
    if !recorded.iter().any(|(p, _)| p == dotfile) {
        recorded.push((dotfile.to_string(), FileKind::Regular));
    }
    recorded.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    recorded.dedup_by(|a, b| a.0 == b.0);

    let mut entries = Vec::with_capacity(recorded.len());
    for (i, (path, kind)) in recorded.into_iter().enumerate() {
        if i % 4096 == 0 {
            crate::signal::check(|| format!("stopped while looking at unit \"{unit}\"'s files"))?;
        }
        let is_dotfile = crate::unit::dotfile::is_unit_dotfile(&path);
        let full = root.join(&path);
        let meta = match std::fs::symlink_metadata(&full) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && is_dotfile => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(TapectlError::Other(format!(
                    "source entry missing: {path} — unit \"{unit}\"'s snapshot lists it and it \
                     is gone from {}. Take a new snapshot (`tapectl snapshot create {unit}`) \
                     and stage that.",
                    root.display()
                )))
            }
            Err(e) => {
                return Err(TapectlError::Other(format!(
                    "cannot stat source entry: {path} ({e})"
                )))
            }
        };
        let now = kind_of(&meta.file_type());
        let Some(now) = now.filter(|k| (is_dotfile && *k == Kind::File) || kind_agrees(kind, *k))
        else {
            return Err(TapectlError::Other(format!(
                "DIRTY: source entry {path} of unit \"{unit}\" is no longer the kind of entry \
                 its snapshot recorded ({}). Take a new snapshot (`tapectl snapshot create \
                 {unit}`) and stage that.",
                kind.as_str()
            )));
        };
        let c = CString::new(full.as_os_str().as_bytes())
            .map_err(|_| TapectlError::Other(format!("a source path holds a NUL byte: {path}")))?;
        let acl = AclOnDisk {
            access: has_attribute(&c, "system.posix_acl_access"),
            default: has_attribute(&c, "system.posix_acl_default"),
        };
        entries.push(Entry {
            listed: Listed {
                size: if now == Kind::File { meta.len() } else { 0 },
                linked: now == Kind::File && std::os::unix::fs::MetadataExt::nlink(&meta) > 1,
                path,
                kind: now,
                acl,
            },
            seen: Seen::of(&meta),
        });
    }
    Ok(Scan {
        entries,
        started_ns,
    })
}

/// Step 5: each entry against what [`scan`] saw before tar ran. A file,
/// link or node must have the device, inode, size, mtime and ctime it had;
/// a folder, its device and inode (its mtime moves when a name in it
/// changes, which is not a change to what was archived). A regular file
/// whose ctime lay within [`RACY_WINDOW_NS`] of the scan is hashed from disk
/// and must hash as the stream did. Whatever tar said, a file changed under
/// it refuses the staging (invariant 7).
pub(crate) fn recheck(
    root: &Path,
    scan: &Scan,
    stream_sha: &std::collections::HashMap<&str, [u8; 32]>,
) -> Result<()> {
    let total = scan.entries.len();
    for (done, entry) in scan.entries.iter().enumerate() {
        if done % 4096 == 0 {
            crate::signal::check(|| {
                format!("stopped while re-checking the source ({done} of {total} entries)")
            })?;
        }
        let path = entry.listed.path.as_str();
        let meta = std::fs::symlink_metadata(root.join(path)).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                changed_while_staging(path, "it was removed while tar read the unit")
            } else {
                TapectlError::Other(format!("cannot stat source entry: {path} ({e})"))
            }
        })?;
        let now = Seen::of(&meta);
        let before = entry.seen;
        let same = if entry.listed.kind == Kind::Folder {
            now.dev == before.dev && now.ino == before.ino
        } else {
            now == before
        };
        if !same {
            return Err(changed_while_staging(path, "it changed while tar read it"));
        }
        if entry.listed.kind == Kind::File && before.ctime_ns >= scan.started_ns - RACY_WINDOW_NS {
            let (again, _) = hash_source_file(&root.join(path), path)?;
            let streamed = stream_sha
                .get(path)
                .map(|h| crate::db::files::sha256_to_hex(h));
            if streamed.as_deref() != Some(again.as_str()) {
                return Err(changed_while_staging(
                    path,
                    "it hashed differently on disk than in the stream",
                ));
            }
        }
    }
    Ok(())
}

/// What a staging's slices came to.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Totals {
    pub(crate) slices: u64,
    pub(crate) plain: i64,
    pub(crate) encrypted: i64,
}

/// The cutter's slices, as staged files: each one created through the
/// staging directory, recorded in `stage_slices` as it closes.
pub(crate) struct StagedSlices<'a> {
    pub(crate) conn: &'a Connection,
    pub(crate) staging: &'a StagingDir,
    pub(crate) stage_set_id: i64,
    pub(crate) base: &'a str,
    pub(crate) recipients: &'a [String],
    pub(crate) packing: Option<Packing>,
    pub(crate) slice_size: u64,
    pub(crate) unit: &'a str,
    pub(crate) on_slice: &'a dyn Fn(u64),
    pub(crate) open: Option<(u64, bool, SliceWriter)>,
    pub(crate) totals: Totals,
}

impl Slices for StagedSlices<'_> {
    fn begin(&mut self, n: u64, continues: bool) -> Result<()> {
        (self.on_slice)(n);
        let w = self
            .staging
            .create_slice(self.base, n, self.recipients, self.packing)?;
        self.open = Some((n, continues, w));
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        let (_, _, w) = self.open.as_mut().expect("a slice is open");
        w.write_all(bytes)?;
        Ok(())
    }

    fn end(&mut self, n: u64) -> Result<()> {
        let (open_n, continues, w) = self.open.take().expect("a slice is open");
        debug_assert_eq!(open_n, n);
        let path = w.path().to_path_buf();
        let info = w.finish()?;
        if let (Some(p), Some(packed)) = (self.packing, info.packed_size) {
            let bound = packed_bound(self.slice_size);
            #[cfg(test)]
            let bound = bound_hook::get().unwrap_or(bound);
            if packed > bound {
                let _ = std::fs::remove_file(&path);
                return Err(TapectlError::Other(format!(
                    "slice {n} of unit \"{}\" packed with {} to {packed} bytes, past the bound \
                     the early warning's rules hold a slice to: slice_size ({} bytes) plus {}'s \
                     stated worst growth, {bound} bytes in all (volume-format-v3.md §3.4). A \
                     staging has one algorithm, so the slice is not stored unpacked in its \
                     place; nothing was staged.",
                    self.unit,
                    p.algorithm.name(),
                    self.slice_size,
                    p.algorithm.name(),
                )));
            }
        }
        super::record_encrypted_slice(
            self.conn,
            BusyPolicy::DEFAULT,
            self.stage_set_id,
            n as i64,
            continues,
            &path,
            &info,
        )?;
        self.totals.slices = n;
        self.totals.plain += info.plain_size;
        self.totals.encrypted += info.encrypted_size;
        tracing::info!(
            slice = n,
            plain_mb = info.plain_size / (1024 * 1024),
            encrypted_mb = info.encrypted_size / (1024 * 1024),
            continues,
            "staged slice"
        );
        Ok(())
    }
}

/// The stream reader's sink: the cutter, the rows, and progress.
pub(crate) struct StageSink<S> {
    pub(crate) cutter: Cutter<S>,
    pub(crate) rows: Vec<Row>,
}

impl<S: Slices> Sink for StageSink<S> {
    fn member(&mut self, start: u64, len: u64) -> Result<u64> {
        self.cutter.member(start, len)
    }

    fn bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.cutter.bytes(bytes)?;
        crate::progress::add_bytes(bytes.len() as u64);
        Ok(())
    }

    fn row(&mut self, row: Row) -> Result<()> {
        #[cfg(test)]
        row_hook::fire(&row.path);
        self.rows.push(row);
        Ok(())
    }

    fn tail(&mut self, start: u64, len: u64) -> Result<()> {
        self.cutter.tail(start, len).map(|_| ())
    }
}

/// Test-only: hold the packed-slice bound to a figure a test chooses, on
/// this thread, so its refusal can be reached (the format's worst growth is
/// far above what any real frame adds).
#[cfg(test)]
pub(crate) mod bound_hook {
    use std::cell::Cell;

    thread_local! {
        static BOUND: Cell<Option<u64>> = const { Cell::new(None) };
    }

    pub(crate) fn set(bound: Option<u64>) {
        BOUND.with(|b| b.set(bound));
    }

    pub(super) fn get() -> Option<u64> {
        BOUND.with(|b| b.get())
    }
}

/// Test-only: run a closure once the stream reader has checked the member
/// of a given path, on this thread: how a test changes the source while tar
/// reads it, at a known place and not by a race.
#[cfg(test)]
pub(crate) mod row_hook {
    use std::cell::RefCell;

    type Hook = (String, Box<dyn FnOnce()>);

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// Run `hook` once the member of `path` has passed.
    pub(crate) fn on(path: &str, hook: impl FnOnce() + 'static) {
        HOOK.with(|h| *h.borrow_mut() = Some((path.to_string(), Box::new(hook))));
    }

    pub(super) fn fire(path: &str) {
        let hook = HOOK.with(|h| {
            let mut h = h.borrow_mut();
            match h.take() {
                Some((p, f)) if p == path => Some(f),
                other => {
                    *h = other;
                    None
                }
            }
        });
        if let Some(hook) = hook {
            hook();
        }
    }
}
