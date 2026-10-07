//! The st driver's `MTIOCGET` status, kept whole (issue #344, the remainder
//! of #301; migration 033).
//!
//! `struct mtget` carries the drive type, the residual count, the density
//! and block-size register, the generic status bits (`mt_gstat`: BOT, EOF,
//! EOT, EOD, write protect, online, door open, and `GMT_CLN` — the drive
//! asking to be cleaned), the soft error register and st's position.
//! tapectl read it only for the position. Now the tape device reads it on
//! its own descriptor when it opens, after a tape command fails, and when it
//! closes ([`crate::tape::ioctl::TapeDevice`]), and the contact that device
//! was open under journals every reading verbatim when it closes
//! ([`crate::tape::contact::ContactGuard::finish`]).
//!
//! # Why the readings wait in memory
//!
//! st refuses a second opener, so the contact guard cannot take this
//! reading — only the code holding the store's descriptor can — and the
//! store does not know its contact or hold the catalog. The readings are
//! therefore noted on the thread that holds the contact (the write path
//! calls the store on its own thread, `pipeline::write_verified`) and
//! written when the contact closes: [`begin`] at the contact's open,
//! [`note`] from the device, [`take`]/[`record`] at the close. A reading
//! noted with no contact open on its thread is not kept.
//!
//! **What that keeps today.** The write paths open their store inside the
//! contact, so their `open` reading lands; the read paths (verify, restore,
//! rebuild) open the store before the contact and drop it after, so for
//! them only `failure` readings land, and no path keeps a `close` reading
//! while the store outlives the guard. Every row that lands names the right
//! contact; capturing more is a change to those call sites.
//!
//! `MTIOCGET` is an ioctl answered by the st driver, not a log-page read, so
//! it cannot disturb a read-to-clear counter (ADR-0013's hazard). st flushes
//! any pending write-behind first, as it does for the position reads
//! tapectl already made.

use std::cell::RefCell;

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
/// recovered errors.
const SOFTERR_MASK: i64 = 0xffff;

impl MtStatus {
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
            "soft_errors": self.mt_erreg & SOFTERR_MASK,
        })
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
}

thread_local! {
    /// The open contact's readings; `None` while no contact is open on this
    /// thread.
    static READINGS: RefCell<Option<Vec<Reading>>> = const { RefCell::new(None) };
}

/// A contact has opened on this thread: keep what devices note until it
/// closes. Readings left by a contact that never closed are dropped.
pub(crate) fn begin() {
    READINGS.with(|r| *r.borrow_mut() = Some(Vec::new()));
}

/// Note one reading for the contact open on this thread, if any.
pub fn note(
    point: &'static str,
    device: &str,
    command: Option<&str>,
    errno: Option<i32>,
    status: std::result::Result<MtStatus, String>,
) {
    READINGS.with(|r| {
        if let Some(v) = r.borrow_mut().as_mut() {
            v.push(Reading {
                captured_at: chrono::Utc::now()
                    .format("%Y-%m-%d %H:%M:%S%.3f")
                    .to_string(),
                point,
                device: device.to_string(),
                command: command.map(str::to_string),
                errno,
                status,
            });
        }
    });
}

/// The contact is closing: take this thread's readings, stop keeping more.
pub(crate) fn take() -> Vec<Reading> {
    READINGS.with(|r| r.borrow_mut().take().unwrap_or_default())
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
                  decoded, tapectl_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                     ?17, ?18)",
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
        // The positive control: nothing set decodes to nothing.
        assert!(MtStatus::default().gstat_flags().is_empty());
        assert!(!MtStatus::default().cleaning_requested());
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
}
