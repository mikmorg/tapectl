//! The st driver's `MTIOCGET` status, kept whole (issue #344, the remainder
//! of #301; migration 035).
//!
//! `struct mtget` carries the drive type, `mt_resid` (st puts the partition
//! number there, not a residual count), the density and block-size
//! register, the generic status bits (`mt_gstat`: BOT, EOF, EOT, EOD, write
//! protect, online, door open, and `GMT_CLN` — the drive asking to be
//! cleaned), st's recovered-error count (`mt_erreg`) and st's position.
//! tapectl read it only for the position. Now the tape device reads it on
//! its own descriptor when it opens, after a tape command fails, and when it
//! closes ([`crate::tape::ioctl::TapeDevice`]), and the contact that device
//! works for journals every reading verbatim when it closes
//! ([`crate::tape::contact::ContactGuard::finish`]).
//!
//! # Why the readings wait in memory
//!
//! st refuses a second opener, so the contact guard cannot take this
//! reading — only the code holding the store's descriptor can — and the
//! store does not know its contact or hold the catalog. The readings are
//! therefore noted for the contact open on the calling thread and written
//! when it closes: [`begin`] at the contact's open, [`note`] from the
//! device, [`take`]/[`record`] at the close. The buffer is shared, and
//! `pipeline`'s stage threads carry it ([`handle`]/[`Handle::enter`]), so
//! the tape read `TapeStore::read_file` runs on its own thread
//! (`pipeline::read_through`) notes for the caller's contact. A reading
//! noted with no contact open, on its thread or carried to it, is not kept.
//!
//! **What that keeps today.** Every `failure` reading, on every path: a
//! failed positioning command, and a read that fails partway through a
//! slice during a confirm, verify, restore, read-slices or compact-read. The
//! write paths open their store inside the contact, so their `open` reading
//! lands too; the read paths (verify, restore, rebuild) open the store
//! before the contact and drop it after, so they keep no `open` or `close`
//! reading, and no path keeps a `close` reading while the store outlives the
//! guard. Every row that lands names the right contact; capturing more is a
//! change to those call sites.
//!
//! # `mt_erreg` is read-to-clear
//!
//! `MTIOCGET` is an ioctl answered by the st driver, not a log-page read —
//! but it is not free to read. st's handler ends
//! `STp->recover_reg = 0; /* Clear after read */` (`drivers/scsi/st.c`), so
//! each reading's soft-error count is what was recovered since the previous
//! `MTIOCGET` on that drive, by any process. tapectl issues one before
//! every tape read it positions for (`TapeStore`'s file cursor checks st's
//! own count), not only at the journalled points. So the device adds every
//! reading's count to a [`RecoveredTally`] through its one `MTIOCGET` call
//! site, and each journalled row carries the tally beside the verbatim
//! register (`recovered_since_open`). Not counted: an `MTIOCGET` from
//! another process (`mt status`), or tapectl's density and no-medium probes,
//! each on a descriptor of its own opened before any tape device. st
//! flushes any pending write-behind before answering, as it does for the
//! position reads.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection};
use tracing::warn;

/// `point`: the device has just opened and taken its block size.
pub const POINT_OPEN: &str = "open";
/// `point`: a tape command on the device has just failed.
pub const POINT_FAILURE: &str = "failure";
/// `point`: the device is closing.
pub const POINT_CLOSE: &str = "close";

/// `struct mtget` from `<linux/mtio.h>`, field for field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MtStatus {
    pub mt_type: i64,
    pub mt_resid: i64,
    pub mt_dsreg: i64,
    pub mt_gstat: i64,
    pub mt_erreg: i64,
    pub mt_fileno: i32,
    pub mt_blkno: i32,
}

/// The `mt_gstat` bits `<linux/mtio.h>` names (`GMT_*`), in bit order from
/// the top.
const GSTAT_BITS: &[(i64, &str)] = &[
    (0x8000_0000, "EOF"),
    (0x4000_0000, "BOT"),
    (0x2000_0000, "EOT"),
    (0x1000_0000, "SM"),
    (0x0800_0000, "EOD"),
    (0x0400_0000, "WR_PROT"),
    (0x0100_0000, "ONLINE"),
    (0x0080_0000, "D_6250"),
    (0x0040_0000, "D_1600"),
    (0x0020_0000, "D_800"),
    (0x0004_0000, "DR_OPEN"),
    (0x0001_0000, "IM_REP_EN"),
    (0x0000_8000, "CLN"),
];

/// `MT_ST_DENSITY_SHIFT`/`MT_ST_BLKSIZE_MASK` (`<linux/mtio.h>`): the st
/// driver packs the density code into `mt_dsreg`'s top byte and the block
/// size into its low 24 bits.
const DENSITY_SHIFT: i64 = 24;
const BLKSIZE_MASK: i64 = 0xff_ffff;
/// `MT_ST_SOFTERR_MASK`: `mt_erreg`'s low 16 bits are st's count of
/// recovered errors — since the previous `MTIOCGET` on the drive, by any
/// process, because st clears the register on every read of it.
const SOFTERR_MASK: i64 = 0xffff;

impl MtStatus {
    /// st's recovered-error count in this reading: what was recovered since
    /// the previous `MTIOCGET` on the drive (read-to-clear).
    pub fn soft_errors(&self) -> i64 {
        self.mt_erreg & SOFTERR_MASK
    }

    /// The `GMT_*` names of the bits set in `mt_gstat`.
    pub fn gstat_flags(&self) -> Vec<&'static str> {
        GSTAT_BITS
            .iter()
            .filter(|(bit, _)| self.mt_gstat & bit != 0)
            .map(|(_, name)| *name)
            .collect()
    }

    /// `GMT_CLN`: the drive asks to be cleaned.
    pub fn cleaning_requested(&self) -> bool {
        self.mt_gstat & 0x0000_8000 != 0
    }

    /// `GMT_WR_PROT`: the cartridge is write-protected.
    pub fn write_protected(&self) -> bool {
        self.mt_gstat & 0x0400_0000 != 0
    }

    /// What this build makes of the fields, for the journal's `decoded`.
    pub fn decoded(&self) -> serde_json::Value {
        serde_json::json!({
            "gstat": self.gstat_flags(),
            "cleaning_requested": self.cleaning_requested(),
            "write_protected": self.write_protected(),
            "density_code": (self.mt_dsreg >> DENSITY_SHIFT) & 0xff,
            "block_size": self.mt_dsreg & BLKSIZE_MASK,
            "soft_errors": self.soft_errors(),
            "partition": self.mt_resid,
        })
    }
}

/// A tape device's running count of st's recovered errors (issue #344).
///
/// st clears `mt_erreg` on every `MTIOCGET` (`drivers/scsi/st.c`, the end
/// of the MTIOCGET handler: `STp->recover_reg = 0; /* Clear after read */`),
/// and tapectl reads it for the position before every tape read, not only
/// at the journalled points. So each reading's count is added here or it is
/// lost: [`Self::observe`] every `MTIOCGET`, [`Self::restart`] after the
/// open's, whose count is what accumulated before this device opened.
#[derive(Debug, Default)]
pub struct RecoveredTally(std::cell::Cell<i64>);

impl RecoveredTally {
    pub fn observe(&self, status: &std::result::Result<MtStatus, String>) {
        if let Ok(s) = status {
            self.0.set(self.0.get() + s.soft_errors());
        }
    }

    pub fn restart(&self) {
        self.0.set(0);
    }

    pub fn total(&self) -> i64 {
        self.0.get()
    }
}

/// One reading, as the device noted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub captured_at: String,
    pub point: &'static str,
    pub device: String,
    /// The tape command that failed (`'failure'` only).
    pub command: Option<String>,
    /// Its errno, when the kernel gave one.
    pub errno: Option<i32>,
    /// The status, or why MTIOCGET itself failed.
    pub status: std::result::Result<MtStatus, String>,
    /// The device's [`RecoveredTally`] at this reading: every recovered
    /// error st reported to it after its open reading, this one included.
    /// `None` when no device kept one.
    pub recovered_since_open: Option<i64>,
}

/// One contact's readings, shared by every thread working for it. The inner
/// `None` is a contact that has closed: a reading noted after that is not
/// kept.
type Buffer = Arc<Mutex<Option<Vec<Reading>>>>;

thread_local! {
    /// The buffer of the contact this thread works for; `None` while no
    /// contact is open on this thread and none was carried to it
    /// ([`Handle::enter`]).
    static READINGS: RefCell<Option<Buffer>> = const { RefCell::new(None) };
}

/// A contact has opened on this thread: keep what devices note until it
/// closes. Readings left by a contact that never closed are dropped.
pub(crate) fn begin() {
    READINGS.with(|r| *r.borrow_mut() = Some(Arc::new(Mutex::new(Some(Vec::new())))));
}

/// Note one reading for the contact this thread works for, if any.
pub fn note(
    point: &'static str,
    device: &str,
    command: Option<&str>,
    errno: Option<i32>,
    status: std::result::Result<MtStatus, String>,
) {
    note_reading(point, device, command, errno, status, None);
}

/// [`note`], with the device's [`RecoveredTally`] at this reading.
pub fn note_reading(
    point: &'static str,
    device: &str,
    command: Option<&str>,
    errno: Option<i32>,
    status: std::result::Result<MtStatus, String>,
    recovered_since_open: Option<i64>,
) {
    let Some(buffer) = READINGS.with(|r| r.borrow().clone()) else {
        return;
    };
    let mut kept = buffer.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(v) = kept.as_mut() {
        v.push(Reading {
            captured_at: chrono::Utc::now()
                .format("%Y-%m-%d %H:%M:%S%.3f")
                .to_string(),
            point,
            device: device.to_string(),
            command: command.map(str::to_string),
            errno,
            status,
            recovered_since_open,
        });
    }
}

/// The contact is closing: take its readings and stop keeping more — on
/// this thread and on any worker it was carried to.
pub(crate) fn take() -> Vec<Reading> {
    let Some(buffer) = READINGS.with(|r| r.borrow_mut().take()) else {
        return Vec::new();
    };
    let mut kept = buffer.lock().unwrap_or_else(|p| p.into_inner());
    kept.take().unwrap_or_default()
}

/// The contact this thread works for, to carry to a worker thread — the
/// way `progress::handle` carries the session log. `pipeline`'s stage
/// threads enter it, so the tape read that runs on its own thread
/// (`pipeline::read_through`, under `TapeStore::read_file`) notes a failed
/// read for the caller's contact.
pub fn handle() -> Handle {
    Handle(READINGS.with(|r| r.borrow().clone()))
}

/// A contact's buffer, carried to another thread.
#[derive(Clone)]
pub struct Handle(Option<Buffer>);

impl Handle {
    /// Work for this contact on the calling thread until the returned guard
    /// drops. An empty handle leaves the thread with no contact.
    pub fn enter(&self) -> Entered {
        let previous = READINGS.with(|r| std::mem::replace(&mut *r.borrow_mut(), self.0.clone()));
        Entered { previous }
    }
}

/// A [`Handle`] entered on a thread; dropping it restores what the thread
/// had before.
#[must_use = "the contact is carried only while this guard lives"]
pub struct Entered {
    previous: Option<Buffer>,
}

impl Drop for Entered {
    fn drop(&mut self) {
        let previous = self.previous.take();
        READINGS.with(|r| *r.borrow_mut() = previous);
    }
}

/// Journal `readings` against `contact_id` (NULL when the contact's own row
/// was refused). Best-effort: a refused insert is a warning.
pub(crate) fn record(
    conn: &Connection,
    contact_id: Option<i64>,
    trigger: &str,
    readings: &[Reading],
) {
    for r in readings {
        let (ok, error, s) = match &r.status {
            Ok(s) => (1, None, Some(*s)),
            Err(e) => (0, Some(e.as_str()), None),
        };
        let inserted = conn.execute(
            "INSERT INTO mtget_journal
                 (captured_at, contact_id, point, trigger, device, command, errno, ok, error,
                  mt_type, mt_resid, mt_dsreg, mt_gstat, mt_erreg, mt_fileno, mt_blkno,
                  recovered_since_open, decoded, tapectl_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                     ?17, ?18, ?19)",
            params![
                r.captured_at,
                contact_id,
                r.point,
                trigger,
                r.device,
                r.command,
                r.errno,
                ok,
                error,
                s.map(|s| s.mt_type),
                s.map(|s| s.mt_resid),
                s.map(|s| s.mt_dsreg),
                s.map(|s| s.mt_gstat),
                s.map(|s| s.mt_erreg),
                s.map(|s| s.mt_fileno),
                s.map(|s| s.mt_blkno),
                r.recovered_since_open,
                s.map(|s| s.decoded().to_string()),
                crate::build_info::VERSION,
            ],
        );
        if let Err(e) = inserted {
            warn!(err = %e, point = r.point, "mtget_journal insert failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An LTO-6 at BOT, online, cleaning requested, 512 KiB blocks, density
    /// 0x5a, three recovered errors.
    fn sample() -> MtStatus {
        MtStatus {
            mt_type: 0x72,
            mt_resid: 0,
            mt_dsreg: (0x5a << 24) | 524_288,
            mt_gstat: 0x4000_0000 | 0x0100_0000 | 0x0000_8000,
            mt_erreg: 3,
            mt_fileno: 0,
            mt_blkno: 0,
        }
    }

    #[test]
    fn the_status_bits_and_registers_decode() {
        let s = sample();
        assert_eq!(s.gstat_flags(), vec!["BOT", "ONLINE", "CLN"]);
        assert!(s.cleaning_requested());
        assert!(!s.write_protected());
        let d = s.decoded();
        assert_eq!(d["density_code"], 0x5a);
        assert_eq!(d["block_size"], 524_288);
        assert_eq!(d["soft_errors"], 3);
        // st puts the partition number in `mt_resid` (st.c's MTIOCGET:
        // `mt_status.mt_resid = STp->partition`), not a residual count.
        assert_eq!(d["partition"], 0);
        let on_partition_1 = MtStatus {
            mt_resid: 1,
            ..sample()
        };
        assert_eq!(on_partition_1.decoded()["partition"], 1);
        assert!(d.get("residual").is_none());
        // The positive control: nothing set decodes to nothing.
        assert!(MtStatus::default().gstat_flags().is_empty());
        assert!(!MtStatus::default().cleaning_requested());
    }

    /// st clears its recovered-error register on every MTIOCGET (st.c:
    /// `STp->recover_reg = 0; /* Clear after read */`), so each reading
    /// counts only what happened since the one before. The device adds up
    /// every reading it takes after its open: what the open reading carried
    /// in is not this device's, and a failed MTIOCGET adds nothing.
    #[test]
    fn the_tally_adds_up_every_read_to_clear_reading_after_the_open() {
        let with = |n: i64| -> std::result::Result<MtStatus, String> {
            Ok(MtStatus {
                mt_erreg: n,
                ..MtStatus::default()
            })
        };
        let tally = RecoveredTally::default();
        tally.observe(&with(7));
        tally.restart();
        assert_eq!(tally.total(), 0, "the open's carry-in is not counted");
        for n in [1, 2, 0] {
            tally.observe(&with(n));
        }
        tally.observe(&Err("MTIOCGET: Input/output error".into()));
        assert_eq!(tally.total(), 3);
        // Only st's soft-error bits (MT_ST_SOFTERR_MASK) count.
        tally.observe(&with(0x1_0004));
        assert_eq!(tally.total(), 7);
    }

    /// A reading noted with no contact open on the thread is not kept; one
    /// noted while a contact is open is, and `take` ends the keeping.
    #[test]
    fn readings_are_kept_only_while_a_contact_is_open() {
        let _ = take();
        note(POINT_OPEN, "/dev/nst9", None, None, Ok(sample()));
        assert!(take().is_empty(), "no contact open: nothing kept");
        begin();
        note(POINT_OPEN, "/dev/nst9", None, None, Ok(sample()));
        note(
            POINT_FAILURE,
            "/dev/nst9",
            Some("write"),
            Some(5),
            Err("MTIOCGET: Input/output error".into()),
        );
        let kept = take();
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[1].command.as_deref(), Some("write"));
        note(POINT_CLOSE, "/dev/nst9", None, None, Ok(sample()));
        assert!(take().is_empty(), "the contact closed: nothing more kept");
    }

    /// The contact is the spine: what a device notes between a contact's
    /// open and its close lands in the journal under that contact's id.
    #[test]
    fn a_contact_journals_what_its_device_noted() {
        use crate::tape::contact::{ContactGuard, Medium, Operation};
        let conn = crate::db::open_memory().unwrap();
        let config = crate::config::Config::default();
        let guard = ContactGuard::open(
            &conn,
            &config,
            Operation::VolumeVerify,
            "/nonexistent/tapectl-mtget-test-nst",
            None,
            Medium::NoBackend,
        );
        let id = guard.id().expect("the contact row was written");
        note(
            POINT_FAILURE,
            "/nonexistent/tapectl-mtget-test-nst",
            Some("space forward 3 file(s)"),
            Some(5),
            Ok(sample()),
        );
        guard.finish(crate::tape::contact::OUTCOME_FAILED, Some("test"));
        let (contact, command, trigger): (Option<i64>, String, String) = conn
            .query_row(
                "SELECT contact_id, command, trigger FROM mtget_journal",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("one reading journalled");
        assert_eq!(contact, Some(id));
        assert_eq!(command, "space forward 3 file(s)");
        assert_eq!(trigger, Operation::VolumeVerify.as_str());
        assert!(take().is_empty(), "the close ended the keeping");
    }

    /// The finding behind this test: a confirm, verify, restore or
    /// read-slices reads each slice through `TapeStore::read_file`, which
    /// runs the tape read on its own thread (`pipeline::read_through`). A
    /// read that fails partway through a slice — the reading #344 exists
    /// for — must land under the contact open on the caller's thread. The
    /// fake tape notes a failed read as `TapeDevice` does.
    #[test]
    fn a_failed_streaming_read_lands_under_the_callers_contact() {
        use crate::store::{Store, TapeStore};
        use crate::tape::contact::{ContactGuard, Medium, Operation};
        let _ = take();
        let conn = crate::db::open_memory().unwrap();
        let config = crate::config::Config::default();
        let guard = ContactGuard::open(
            &conn,
            &config,
            Operation::VolumeVerify,
            "/nonexistent/tapectl-mtget-read-nst",
            None,
            Medium::NoBackend,
        );
        let id = guard.id().expect("the contact row was written");
        let tape = crate::tape::fake::FakeTape::with_files(vec![vec![9u8; 4 * 512]], 512);
        tape.state().fail_reads_at = vec![0];
        let mut store = TapeStore::from_ops(tape.boxed(), 1 << 20).unwrap();
        let mut sink = Vec::new();
        assert!(store.read_file(0, &mut sink).is_err(), "the read failed");
        guard.finish(crate::tape::contact::OUTCOME_FAILED, Some("test"));
        let rows: Vec<(Option<i64>, String, Option<String>)> = conn
            .prepare("SELECT contact_id, point, command FROM mtget_journal ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![(Some(id), "failure".to_string(), Some("read".to_string()))]
        );
    }

    /// Journalled verbatim: the seven fields as integers, the decode beside
    /// them, a failed MTIOCGET as `ok = 0` with its error and NULL fields.
    #[test]
    fn readings_are_journalled_verbatim_against_the_contact() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO cartridge_contacts (id, operation, device) VALUES (5, 'volume verify', '/dev/nst9')",
            [],
        )
        .unwrap();
        begin();
        note(POINT_OPEN, "/dev/nst9", None, None, Ok(sample()));
        note(
            POINT_FAILURE,
            "/dev/nst9",
            Some("read"),
            Some(5),
            Err("MTIOCGET: Input/output error".into()),
        );
        record(&conn, Some(5), "volume verify", &take());
        #[allow(clippy::type_complexity)]
        let rows: Vec<(
            i64,
            String,
            i64,
            Option<i64>,
            Option<i64>,
            Option<String>,
            Option<i64>,
        )> = conn
            .prepare(
                "SELECT contact_id, point, ok, mt_gstat, mt_dsreg, decoded, errno
                   FROM mtget_journal ORDER BY id",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, 5);
        assert_eq!(rows[0].1, "open");
        assert_eq!(rows[0].3, Some(0x4000_0000 | 0x0100_0000 | 0x0000_8000));
        assert_eq!(rows[0].4, Some((0x5a << 24) | 524_288));
        assert!(rows[0].5.as_deref().unwrap().contains("\"CLN\""));
        assert_eq!((rows[1].1.as_str(), rows[1].2), ("failure", 0));
        assert_eq!((rows[1].3, rows[1].6), (None, Some(5)));
    }

    /// The device's tally lands in its own column beside the verbatim
    /// register; a reading noted without one (no device behind it) is NULL,
    /// never 0.
    #[test]
    fn the_recovered_tally_is_journalled_beside_the_register() {
        let conn = crate::db::open_memory().unwrap();
        begin();
        note_reading(POINT_CLOSE, "/dev/nst9", None, None, Ok(sample()), Some(11));
        note(
            POINT_FAILURE,
            "/dev/nst9",
            Some("read"),
            Some(5),
            Ok(sample()),
        );
        record(&conn, None, "volume verify", &take());
        let rows: Vec<(i64, Option<i64>)> = conn
            .prepare("SELECT mt_erreg, recovered_since_open FROM mtget_journal ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(rows, vec![(3, Some(11)), (3, None)]);
    }
}
