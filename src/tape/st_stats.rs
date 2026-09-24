//! The kernel's own tape counters, read at each contact's open and close
//! (issue #301; ADR-0012 2026-09-24 amendment item 2; ADR-0013).
//!
//! The st driver keeps cumulative per-device I/O statistics in sysfs,
//! `/sys/class/scsi_tape/<node>/stats/` — bytes, command counts and
//! NANOSECOND time for reads, writes and everything else, plus a residual
//! counter and the number of commands in flight. They cost no SCSI command and
//! no device open, and they are the only drive evidence left when the drive
//! will not answer SCSI at all. Until this module nothing read them.
//!
//! **Captured verbatim, differenced by query.** Every regular file in
//! `stats/` is read as text, trailing newline and all, and journalled as one
//! JSON object per reading (`st_stats_journal.stats_json`, migration 025).
//! The set of files is whatever the running kernel publishes — nothing here
//! names a counter, so a kernel that adds one has it recorded with no code
//! change. No arithmetic is applied: what a contact did is the difference
//! between its `open` and `close` readings, which is a query (ADR-0013 §3),
//! and the counters' reset scope is not yet measured.
//!
//! **Where the readings are taken.** [`crate::tape::contact::ContactGuard`]
//! takes the `open` reading right after inserting its contact row and the
//! `close` reading right before setting `closed_at`, so every closed contact
//! has both and a contact that never closed has only the first. The pair
//! brackets the CONTACT, not the file descriptor — see migration 025.
//!
//! **Best-effort, and absence is no row.** A device with no `stats/` (a
//! MemStore, a non-st node, a kernel without st statistics) yields no
//! reading; a journal write that fails warns. Neither ever refuses or fails
//! a tape command.
//!
//! **Sysfs only — no ioctl.** The guard never holds the store's descriptor
//! and must not open the node (the st driver refuses a second concurrent
//! open), so `MTIOCGET`'s `mtget` is out of reach from here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use tracing::warn;

use crate::tape::drive_identity;

/// Which end of a contact a reading was taken at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Point {
    /// Right after the contact row was inserted.
    Open,
    /// Right before the contact's `closed_at` was set.
    Close,
}

impl Point {
    pub fn as_str(self) -> &'static str {
        match self {
            Point::Open => "open",
            Point::Close => "close",
        }
    }
}

/// The `stats/` directory for the tape node `device` names, under `root`
/// (the kernel's `/sys/class/scsi_tape` in production, a temp tree in
/// tests). Resolved the way the drive-identity read resolves its node: a
/// by-id symlink canonicalises to the `nstN` sysfs is keyed by. `None` only
/// when the path has no usable basename; whether the directory EXISTS is
/// [`read`]'s question.
pub fn stats_dir(root: &Path, device: &str) -> Option<PathBuf> {
    Some(drive_identity::sysfs_node_dir(root, device)?.join("stats"))
}

/// One reading of a `stats/` directory, before it becomes a journal row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    /// UTC, millisecond precision (`YYYY-MM-DD HH:MM:SS.SSS`, SQLite's own
    /// `%f` form, so `julianday()` reads it).
    pub captured_at: String,
    /// The directory read.
    pub dir: PathBuf,
    /// File name → the file's text exactly as read.
    pub files: BTreeMap<String, String>,
    /// File name → why it is not in `files` (unreadable, or not UTF-8 text
    /// — whose bytes are kept here as hex rather than lossily converted).
    pub errors: BTreeMap<String, String>,
}

/// Read every regular file in `dir`, verbatim. `None` when the directory
/// cannot be listed — the recorded absence is no row.
pub fn read(dir: &Path) -> Option<Reading> {
    let entries = std::fs::read_dir(dir).ok()?;
    let captured_at = chrono::Utc::now()
        .format("%Y-%m-%d %H:%M:%S%.3f")
        .to_string();
    let mut files = BTreeMap::new();
    let mut errors = BTreeMap::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                errors.insert(format!("<entry {}>", errors.len()), e.to_string());
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        // `metadata` follows symlinks: a directory (or anything that is not a
        // file) is not a counter.
        match std::fs::metadata(entry.path()) {
            Ok(m) if m.is_file() => {}
            Ok(_) => continue,
            Err(e) => {
                errors.insert(name, e.to_string());
                continue;
            }
        }
        match std::fs::read(entry.path()) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => {
                    files.insert(name, text);
                }
                Err(e) => {
                    let hex: String = e.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
                    errors.insert(name, format!("not UTF-8 text; bytes (hex): {hex}"));
                }
            },
            Err(e) => {
                errors.insert(name, e.to_string());
            }
        }
    }
    Some(Reading {
        captured_at,
        dir: dir.to_path_buf(),
        files,
        errors,
    })
}

/// Write one reading. Fallible — [`capture`] is the best-effort wrapper the
/// contact guard uses.
pub fn insert(
    conn: &Connection,
    contact_id: Option<i64>,
    point: Point,
    trigger: &str,
    device: &str,
    reading: &Reading,
) -> rusqlite::Result<i64> {
    let stats_json = serde_json::to_string(&reading.files).unwrap_or_else(|_| "{}".to_string());
    let errors_json = if reading.errors.is_empty() {
        None
    } else {
        serde_json::to_string(&reading.errors).ok()
    };
    conn.execute(
        "INSERT INTO st_stats_journal
             (captured_at, contact_id, point, trigger, device, sysfs_dir,
              stats_json, errors_json, tapectl_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            reading.captured_at,
            contact_id,
            point.as_str(),
            trigger,
            device,
            reading.dir.to_string_lossy(),
            stats_json,
            errors_json,
            // The BUILD identity (commit + day), not the package version:
            // ADR-0012, 2026-09-24 amendment, item 1.
            crate::build_info::VERSION,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Read `dir` (if there is one) and journal it. Best-effort: no directory
/// is no row, and a failed write warns. Never fails.
pub fn capture(
    conn: &Connection,
    contact_id: Option<i64>,
    point: Point,
    trigger: &str,
    device: &str,
    dir: Option<&Path>,
) {
    let Some(reading) = dir.and_then(read) else {
        return;
    };
    if let Err(e) = insert(conn, contact_id, point, trigger, device, &reading) {
        warn!(err = %e, point = point.as_str(), "st_stats_journal insert failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake `stats/` tree under a temp root, shaped as the kernel lays it
    /// out: `<root>/<node>/stats/<file>`.
    fn fake_node(root: &Path, node: &str, files: &[(&str, &[u8])]) -> PathBuf {
        let dir = root.join(node).join("stats");
        std::fs::create_dir_all(&dir).unwrap();
        for (name, bytes) in files {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
        dir
    }

    #[test]
    fn stats_dir_is_the_nodes_stats_under_the_root() {
        assert_eq!(
            stats_dir(Path::new("/fake"), "/dev/nst-no-such-node"),
            Some(PathBuf::from("/fake/nst-no-such-node/stats"))
        );
    }

    /// Verbatim means verbatim: trailing newlines kept, a value that is not
    /// a number kept as it is (nothing here parses), a subdirectory skipped,
    /// non-UTF-8 bytes named with their hex rather than lossily converted.
    #[test]
    fn read_keeps_every_file_verbatim_and_names_what_it_could_not_keep() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = fake_node(
            tmp.path(),
            "nst7",
            &[
                ("write_byte_cnt", b"26129465344\n"),
                ("in_flight", b"0\n"),
                ("odd_counter", b"not a number \n"),
                ("binary", &[0xff, 0x00]),
            ],
        );
        std::fs::create_dir(dir.join("subdir")).unwrap();

        let r = read(&dir).expect("the directory exists");
        assert_eq!(r.dir, dir);
        let want: BTreeMap<String, String> = [
            ("in_flight", "0\n"),
            ("odd_counter", "not a number \n"),
            ("write_byte_cnt", "26129465344\n"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(r.files, want);
        assert_eq!(
            r.errors.get("binary").map(String::as_str),
            Some("not UTF-8 text; bytes (hex): ff00")
        );
        assert_eq!(r.errors.len(), 1, "the subdirectory is not an error");
    }

    /// Absence: no directory is no reading. Positive control: the same
    /// path, once created, IS read.
    #[test]
    fn a_missing_directory_is_no_reading() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nst7").join("stats");
        assert_eq!(read(&dir), None);
        fake_node(tmp.path(), "nst7", &[("io_ns", b"1\n")]);
        assert!(read(&dir).is_some(), "positive control");
    }
}
