//! The ungated layer of the proof that staging writes no plaintext to the
//! staging device (ADR-0012, 2026-10-06 amendment item 4; issue #370;
//! docs/research/2026-10-06-plaintext-free-staging.md §8, layer 2).
//!
//! [`StagingAudit`] watches the staging directory with inotify while a stage
//! runs. inotify reports every file created there, however briefly it
//! lived, so the audit sees:
//!
//! - every name: each must be a `.dar.age` slice of the stage or the
//!   writability probe — anything else (a `.dar`, a dar temporary, a
//!   directory) is a violation;
//! - every file's content: the audit opens each file the moment it appears
//!   and keeps it open, so it can read the bytes at the end even if the
//!   file was deleted meanwhile. A `.dar.age` must be age ciphertext from
//!   its first byte, and no file may hold a planted marker.
//!
//! The raw-device layer (a deleted block still on the disk) is
//! `scripts/plaintext-scan.sh`, which needs a loop mount.

use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};

/// What the audit saw.
#[derive(Debug, Default)]
struct Seen {
    /// Every name created in (or moved into) the directory, in order.
    created: Vec<String>,
    /// Files opened as they appeared, read at the end.
    held: Vec<(String, File)>,
    /// Names created and gone before they could be opened.
    vanished: Vec<String>,
    /// Directories created.
    dirs: Vec<String>,
    overflowed: bool,
}

pub(super) struct StagingAudit {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Seen>>,
}

/// What the audit found: the names it saw, and every violation.
#[derive(Debug)]
pub(super) struct AuditReport {
    pub(super) created: Vec<String>,
    pub(super) violations: Vec<String>,
}

/// The names `stage create` may create under staging: its slices,
/// `{uuid12}_v{V}_s{id}.{N}.dar.age`, and its writability probe.
fn allowed_name(name: &str) -> bool {
    if let Some(pid) = name.strip_prefix(".tapectl-stage-probe-") {
        return !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit());
    }
    let Some(stem) = name.strip_suffix(".dar.age") else {
        return false;
    };
    let Some((base, n)) = stem.rsplit_once('.') else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let parts: Vec<&str> = base.split('_').collect();
    digits(n)
        && parts.len() == 3
        && parts[0].len() == 12
        && parts[0].bytes().all(|b| b.is_ascii_hexdigit())
        && parts[1].strip_prefix('v').is_some_and(digits)
        && parts[2].strip_prefix('s').is_some_and(digits)
}

impl StagingAudit {
    /// Start watching `dir` (which must exist).
    pub(super) fn start(dir: &Path) -> Self {
        let inotify = Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC).unwrap();
        inotify
            .add_watch(
                dir,
                AddWatchFlags::IN_CREATE
                    | AddWatchFlags::IN_MOVED_TO
                    | AddWatchFlags::IN_CLOSE_WRITE,
            )
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread_dir = dir.to_path_buf();
        let handle = std::thread::spawn(move || {
            let mut seen = Seen::default();
            loop {
                match inotify.read_events() {
                    Ok(events) => {
                        for event in events {
                            if event.mask.contains(AddWatchFlags::IN_Q_OVERFLOW) {
                                seen.overflowed = true;
                            }
                            let Some(name) = event.name.map(OsString::into_string) else {
                                continue;
                            };
                            let name = name.unwrap_or_else(|n| n.to_string_lossy().into_owned());
                            if !event
                                .mask
                                .intersects(AddWatchFlags::IN_CREATE | AddWatchFlags::IN_MOVED_TO)
                            {
                                continue;
                            }
                            seen.created.push(name.clone());
                            if event.mask.contains(AddWatchFlags::IN_ISDIR) {
                                seen.dirs.push(name);
                                continue;
                            }
                            match File::open(thread_dir.join(&name)) {
                                Ok(f) => seen.held.push((name, f)),
                                Err(_) => seen.vanished.push(name),
                            }
                        }
                    }
                    Err(Errno::EAGAIN) => {
                        if thread_stop.load(Ordering::Acquire) {
                            return seen;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => panic!("inotify read failed: {e}"),
                }
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// Stop watching, read every file it held, and judge: names, kinds,
    /// age framing, and the absence of every `marker`.
    pub(super) fn finish(mut self, markers: &[&[u8]]) -> AuditReport {
        // Let the last events arrive.
        std::thread::sleep(Duration::from_millis(50));
        self.stop.store(true, Ordering::Release);
        let seen = self.handle.take().unwrap().join().unwrap();
        let mut violations = Vec::new();
        if seen.overflowed {
            violations.push("inotify queue overflowed: events were lost".to_string());
        }
        for name in &seen.created {
            if !allowed_name(name) {
                violations.push(format!("a file staging must never create: {name}"));
            }
        }
        for name in &seen.dirs {
            violations.push(format!("a directory under staging: {name}"));
        }
        for (name, mut file) in seen.held {
            let mut bytes = Vec::new();
            file.seek(SeekFrom::Start(0)).unwrap();
            file.read_to_end(&mut bytes).unwrap();
            if name.ends_with(".dar.age")
                && !bytes.is_empty()
                && !bytes.starts_with(b"age-encryption.org/v1\n")
            {
                violations.push(format!("{name} is not age ciphertext"));
            }
            for marker in markers {
                if bytes.windows(marker.len()).any(|w| w == *marker) {
                    violations.push(format!(
                        "{name} holds plaintext: marker {:?}",
                        String::from_utf8_lossy(marker)
                    ));
                }
            }
        }
        for name in seen.vanished {
            if !name.starts_with(".tapectl-stage-probe-") {
                violations.push(format!(
                    "{name} was deleted before its content could be checked"
                ));
            }
        }
        AuditReport {
            created: seen.created,
            violations,
        }
    }
}

impl Drop for StagingAudit {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

mod tests {
    use super::*;
    use crate::config::Config;
    use crate::staging::tests::setup_unit_with_excludes;
    use crate::staging::{snapshot_create, stage_create};
    use std::fs;

    fn token() -> String {
        let mut b = [0u8; 12];
        rand::Rng::fill(&mut rand::rng(), &mut b[..]);
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn the_allowed_names_are_slices_and_the_probe() {
        for ok in [
            "0123456789ab_v1_s7.1.dar.age",
            "0123456789ab_v12_s300.10.dar.age",
            ".tapectl-stage-probe-4242",
        ] {
            assert!(allowed_name(ok), "{ok}");
        }
        for bad in [
            "0123456789ab_v1_s7.1.dar",
            "0123456789ab_v1_s7.1.dar.sha512",
            "0123456789ab_v1_s7.dar.age",
            "catalog_snapshot.db",
            "secret-unit.1.dar.age",
            ".tapectl-stage-probe-",
        ] {
            assert!(!allowed_name(bad), "{bad}");
        }
    }

    /// Issue #370, layer 2: a stage of a tree full of a planted marker —
    /// in file contents (a small file, a multi-slice file), file and
    /// directory names, a symlink target — creates nothing under staging but
    /// its `.age` slices and the probe, every slice is age ciphertext, and
    /// no file ever held the marker, however briefly.
    #[test]
    fn staging_never_holds_plaintext_audited_by_inotify() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.defaults.slice_size = "64K".to_string();
        let marker = format!("plaintext-marker-{}", token());
        let dir = src.join(format!("dir-{marker}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            src.join(format!("small-{marker}.txt")),
            format!("{marker}\n").repeat(50),
        )
        .unwrap();
        // Incompressible, with the marker every 16 KiB: several slices.
        let mut big = vec![0u8; 300 * 1024];
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        for b in big.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        for at in (0..big.len() - marker.len()).step_by(16 * 1024) {
            big[at..at + marker.len()].copy_from_slice(marker.as_bytes());
        }
        fs::write(dir.join("big.bin"), &big).unwrap();
        std::os::unix::fs::symlink(format!("target-{marker}"), dir.join("link")).unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        let staging = Path::new(&config.staging.directory).to_path_buf();
        let audit = StagingAudit::start(&staging);
        let id = stage_create(&conn, &paths, &config, snap_id, false).unwrap();
        let report = audit.finish(&[marker.as_bytes()]);

        let slices: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM stage_slices WHERE stage_set_id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(slices >= 2, "positive control: a multi-slice stage");
        let seen_slices = report
            .created
            .iter()
            .filter(|n| n.ends_with(".dar.age"))
            .count();
        assert_eq!(
            seen_slices as i64, slices,
            "positive control: the audit saw every slice created: {:?}",
            report.created
        );
        assert!(
            report.violations.is_empty(),
            "plaintext reached staging: {:#?}",
            report.violations
        );
    }

    /// The positive control the audit owes: the same audit, around
    /// plaintext planted in staging by hand — a `.dar` slice deleted at
    /// once, and plaintext under an allowed `.dar.age` name — reports both.
    #[test]
    fn the_audit_catches_plaintext_planted_in_staging() {
        let tmp = tempfile::TempDir::new().unwrap();
        let staging = tmp.path().join("staging");
        fs::create_dir_all(&staging).unwrap();
        let marker = format!("plaintext-marker-{}", token());

        let audit = StagingAudit::start(&staging);
        let leaked = staging.join("0123456789ab_v1_s1.1.dar");
        fs::write(&leaked, format!("dar slice {marker}")).unwrap();
        // Long enough for the audit to open it, then gone, as the old
        // pipeline's plaintext slices were.
        std::thread::sleep(Duration::from_millis(20));
        fs::remove_file(&leaked).unwrap();
        fs::write(
            staging.join("0123456789ab_v1_s1.2.dar.age"),
            format!("not ciphertext {marker}"),
        )
        .unwrap();
        let report = audit.finish(&[marker.as_bytes()]);

        let has = |needle: &str| report.violations.iter().any(|v| v.contains(needle));
        assert!(
            has("a file staging must never create: 0123456789ab_v1_s1.1.dar"),
            "{:#?}",
            report.violations
        );
        assert!(
            has("0123456789ab_v1_s1.1.dar holds plaintext"),
            "the deleted file's content is still checked: {:#?}",
            report.violations
        );
        assert!(
            has("0123456789ab_v1_s1.2.dar.age is not age ciphertext")
                && has("0123456789ab_v1_s1.2.dar.age holds plaintext"),
            "{:#?}",
            report.violations
        );
    }
}
