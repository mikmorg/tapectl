use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;

use crate::error::{Result, TapectlError};

// Linux tape ioctl constants from <linux/mtio.h>
const MTIOCTOP: u64 = 0x40086d01;
/// `_IOR('m', 2, struct mtget)` from `<linux/mtio.h>`.
///
/// `pub(crate)` so `tape::media_detect`'s no-medium probe (issue #152) reads
/// the SAME kernel ABI this module does. It briefly had its own copy; two
/// declarations of an ioctl number and a `#[repr(C)]` layout that `unsafe`
/// code casts a raw pointer through is a drift hazard of a nastier kind than
/// most — a divergence would not fail to compile, it would read garbage.
pub(crate) const MTIOCGET: u64 = 0x80306d02;

// mtop operation codes
const MTREW: i16 = 6;
const MTWEOF: i16 = 5;
const MTWEOFI: i16 = 35;
const MTSETBLK: i16 = 20;
const MTFSF: i16 = 1;
const MTCOMP: i16 = 32; // MTCOMPRESSION

#[repr(C)]
struct MtOp {
    mt_op: i16,
    _pad: i16,
    mt_count: i32,
}

/// `struct mtget` from `<linux/mtio.h>`. `pub(crate)` for the same reason as
/// [`MTIOCGET`] — one declaration of the layout, not two.
#[repr(C)]
#[derive(Debug, Default)]
pub(crate) struct MtGet {
    pub(crate) mt_type: i64,
    pub(crate) mt_resid: i64,
    pub(crate) mt_dsreg: i64,
    pub(crate) mt_gstat: i64,
    pub(crate) mt_erreg: i64,
    pub(crate) mt_fileno: i32,
    pub(crate) mt_blkno: i32,
}

/// Tape position info: the st driver's own count (`MTIOCGET`'s `mt_fileno`
/// and `mt_blkno`), `-1` where st has lost track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapePosition {
    pub file_number: i32,
    pub block_number: i32,
    /// `GMT_EOD` in `mt_gstat`: the st driver last met END OF DATA — a
    /// space or read that ran into BLANK CHECK (`st.c`: `eof = ST_EOD`).
    /// After a failed forward space it says the space stopped at the end of
    /// what is recorded, rather than on a medium or transport error (issues
    /// #400, #403).
    pub at_eod: bool,
}

/// `GMT_EOD(x)` from `<linux/mtio.h>`: `(x) & 0x08000000`.
const GMT_EOD: i64 = 0x0800_0000;

/// How a read of one tape file ended: what `TapeStore`'s file cursor needs to
/// know about where the read left the head (issue #389).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadEnd {
    /// The read crossed the file's filemark (st returned 0), so the head is
    /// at the start of the next file.
    Filemark,
    /// The read stopped on `ENOSPC`, which these readers treat as the end of
    /// the file. Where that leaves the head relative to file boundaries is
    /// not known.
    EndOfMedium,
    /// The read stopped before the filemark because its byte budget ran out
    /// ([`TapeDevice::read_file_head`]): the head is inside the file.
    Stopped,
}

/// A tape write error: ENOSPC — the drive's early-warning point, a full
/// medium — is [`TapectlError::MediumFull`], the one store failure a write
/// session aborts on; anything else is [`TapectlError::TapeIo`] (issue #408).
fn write_error(what: &str, e: &io::Error) -> TapectlError {
    if e.raw_os_error() == Some(28) {
        TapectlError::MediumFull(format!("{what}: {e}"))
    } else {
        TapectlError::TapeIo(format!("{what}: {e}"))
    }
}

/// What an `MTIOCTOP` op does, for the session log's wait lines.
fn mt_op_name(op: i16, count: i32) -> String {
    match op {
        MTREW => "rewind".to_string(),
        MTWEOF => "filemark (synchronous)".to_string(),
        MTWEOFI => "filemark".to_string(),
        MTSETBLK => format!("set block size {count}"),
        MTFSF => format!("space forward {count} file(s)"),
        MTCOMP => "set compression".to_string(),
        other => format!("ioctl op {other} count {count}"),
    }
}

/// A wrapper around a tape device file descriptor.
///
/// It notes the st driver's whole `MTIOCGET` status when it opens, after a
/// tape command fails and when it closes (issue #344,
/// [`crate::tape::mtget_journal`]); the contact open on the same thread
/// journals those readings when it closes.
pub struct TapeDevice {
    file: File,
    block_size: usize,
    /// The path it was opened by, for the journal.
    path: String,
}

impl Drop for TapeDevice {
    fn drop(&mut self) {
        self.note(crate::tape::mtget_journal::POINT_CLOSE, None, None);
    }
}

impl TapeDevice {
    /// The st driver's whole `MTIOCGET` status (issue #344): an ioctl answered by the st
    /// driver, not a log-page read (st flushes pending write-behind first).
    pub fn status(&self) -> std::result::Result<crate::tape::mtget_journal::MtStatus, String> {
        let mut m = MtGet::default();
        let rc = unsafe { nix::libc::ioctl(self.raw_fd(), MTIOCGET, &mut m as *mut MtGet) };
        if rc != 0 {
            return Err(format!("MTIOCGET: {}", io::Error::last_os_error()));
        }
        Ok(crate::tape::mtget_journal::MtStatus {
            mt_type: m.mt_type,
            mt_resid: m.mt_resid,
            mt_dsreg: m.mt_dsreg,
            mt_gstat: m.mt_gstat,
            mt_erreg: m.mt_erreg,
            mt_fileno: m.mt_fileno,
            mt_blkno: m.mt_blkno,
        })
    }

    /// Note the status for the open contact (issue #344). `command` and
    /// `errno` name a failed tape command; the errno is taken by the caller
    /// before this, since MTIOCGET is itself a syscall.
    fn note(&self, point: &'static str, command: Option<&str>, errno: Option<i32>) {
        crate::tape::mtget_journal::note(point, &self.path, command, errno, self.status());
    }

    /// [`Self::note`] for a failed command, from its error.
    fn note_failure(&self, command: &str, e: &io::Error) {
        self.note(
            crate::tape::mtget_journal::POINT_FAILURE,
            Some(command),
            e.raw_os_error(),
        );
    }

    /// Open a tape device for read+write with the given block size.
    pub fn open(device_path: &str, block_size: usize) -> Result<Self> {
        // Issue #386: an open blocks while the drive loads and settles a
        // cartridge — a wait worth naming if it is long.
        let file = crate::progress::waited(
            || format!("opening tape device {device_path} for writing"),
            || OpenOptions::new().read(true).write(true).open(device_path),
        )
        .map_err(|e| TapectlError::TapeIo(format!("open {device_path}: {e}")))?;

        let mut dev = Self {
            file,
            block_size,
            path: device_path.to_string(),
        };
        dev.set_block_size(block_size)?;
        dev.note(crate::tape::mtget_journal::POINT_OPEN, None, None);
        Ok(dev)
    }

    /// Open read-only.
    pub fn open_read(device_path: &str, block_size: usize) -> Result<Self> {
        let file = crate::progress::waited(
            || format!("opening tape device {device_path} for reading"),
            || OpenOptions::new().read(true).open(device_path),
        )
        .map_err(|e| TapectlError::TapeIo(format!("open {device_path}: {e}")))?;

        let mut dev = Self {
            file,
            block_size,
            path: device_path.to_string(),
        };
        dev.set_block_size(block_size)?;
        dev.note(crate::tape::mtget_journal::POINT_OPEN, None, None);
        Ok(dev)
    }

    /// The block size reads and writes use (0: variable-block mode).
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    fn raw_fd(&self) -> i32 {
        self.file.as_raw_fd()
    }

    fn mt_ioctl(&self, op: i16, count: i32) -> Result<()> {
        let mtop = MtOp {
            mt_op: op,
            _pad: 0,
            mt_count: count,
        };
        // Issue #386: a rewind, a space or a synchronous filemark can hold
        // the process for minutes with no byte moving; it names itself in
        // the session log when it passes the wait threshold.
        let rc = crate::progress::waited(
            || format!("tape {}", mt_op_name(op, count)),
            || unsafe { nix::libc::ioctl(self.raw_fd(), MTIOCTOP, &mtop as *const MtOp) },
        );
        if rc != 0 {
            let e = io::Error::last_os_error();
            self.note_failure(&mt_op_name(op, count), &e);
            return Err(TapectlError::TapeIo(format!(
                "ioctl op={op} count={count}: {e}"
            )));
        }
        Ok(())
    }

    /// Set tape block size.
    pub fn set_block_size(&mut self, bs: usize) -> Result<()> {
        self.mt_ioctl(MTSETBLK, bs as i32)?;
        self.block_size = bs;
        Ok(())
    }

    /// Rewind tape to beginning.
    pub fn rewind(&self) -> Result<()> {
        self.mt_ioctl(MTREW, 0)
    }

    /// Disable the drive's hardware compression. Encrypted data is
    /// incompressible, so compression only wastes drive effort; the design
    /// (§2.8) requires it off. Best-effort — some drives/mhvtl reject the op.
    pub fn disable_compression(&self) -> Result<()> {
        self.mt_ioctl(MTCOMP, 0)
    }

    /// Write a file mark (immediate, no flush).
    pub fn write_filemark_immediate(&self) -> Result<()> {
        self.mt_ioctl(MTWEOFI, 1)
    }

    /// Write a file mark (synchronous flush).
    pub fn write_filemark_sync(&self) -> Result<()> {
        self.mt_ioctl(MTWEOF, 1)
    }

    /// Forward space N file marks.
    pub fn forward_space_file(&self, count: i32) -> Result<()> {
        self.mt_ioctl(MTFSF, count)
    }

    /// Get current tape position.
    pub fn get_position(&self) -> Result<TapePosition> {
        let mut mtget = MtGet::default();
        let rc = unsafe { nix::libc::ioctl(self.raw_fd(), MTIOCGET, &mut mtget as *mut MtGet) };
        if rc != 0 {
            return Err(TapectlError::TapeIo(format!(
                "MTIOCGET: {}",
                io::Error::last_os_error()
            )));
        }
        Ok(TapePosition {
            file_number: mtget.mt_fileno,
            block_number: mtget.mt_blkno,
            at_eod: mtget.mt_gstat & GMT_EOD != 0,
        })
    }

    /// Stream-write `len` bytes from `src` in `block_size` chunks, zero-padding
    /// the final partial block to the block boundary, followed by a file mark
    /// (synchronous if `sync`). Peak memory is one block, never `len` (the
    /// H9 streaming requirement; the whole-buffer v1 writers it replaced are
    /// gone, issue #417,
    /// `docs/design/volume-format-v2.md` §7 / layout-session.md's Store seam).
    /// Returns the number of bytes committed to the medium including padding
    /// (`layout_model::pad_to_blocks(len, block_size)`).
    pub fn write_stream(&mut self, src: &mut dyn Read, len: u64, sync: bool) -> Result<u64> {
        let bs = self.block_size.max(1);
        let mut buf = vec![0u8; bs];
        let mut remaining = len;
        let mut committed = 0u64;

        while remaining > 0 {
            let want = remaining.min(bs as u64) as usize;
            let mut got = 0usize;
            while got < want {
                // Issue #408: the SOURCE failing is the disk's problem, not
                // the tape's, and is named so.
                let n = src
                    .read(&mut buf[got..want])
                    .map_err(|e| TapectlError::SourceIo(format!("read source: {e}")))?;
                if n == 0 {
                    return Err(TapectlError::SourceIo(format!(
                        "source exhausted after {got} of {want} bytes wanted \
                         (declared length {len}, {remaining} remaining)"
                    )));
                }
                got += n;
            }
            if want < bs {
                for b in &mut buf[want..] {
                    *b = 0;
                }
            }
            let started = std::time::Instant::now();
            if let Err(e) = self.file.write_all(&buf[..bs]) {
                self.note_failure("write", &e);
                return Err(write_error("write", &e));
            }
            crate::progress::note_if_slow("one tape block write", started.elapsed());
            committed += bs as u64;
            remaining -= want as u64;
        }

        let marked = if sync {
            self.write_filemark_sync()
        } else {
            self.write_filemark_immediate()
        };
        // A filemark refused at the early-warning point is a full medium
        // too (issue #408).
        if let Err(e) = marked {
            let full = matches!(&e, TapectlError::TapeIo(m) if m.contains("(os error 28)"));
            return Err(if full {
                TapectlError::MediumFull(e.to_string())
            } else {
                e
            });
        }
        Ok(committed)
    }

    /// Stream-read one "file" from tape (all data until the next file mark),
    /// writing each block straight to `sink` as it arrives instead of
    /// accumulating in memory. Returns the total bytes read — the on-tape
    /// (padded) length; trimming to the true size is the caller's job, since
    /// only the front index knows it — and how the read ended. A read of 0
    /// is the filemark, and `ENOSPC` is treated as the end of the file; the
    /// [`ReadEnd`] says which of the two it was, because they leave the head
    /// in different places (issue #389).
    pub fn read_file_streaming(&mut self, sink: &mut dyn Write) -> Result<(u64, ReadEnd)> {
        let mut total = 0u64;
        let read_size = if self.block_size > 0 {
            self.block_size
        } else {
            1024 * 1024
        };
        let mut buf = vec![0u8; read_size];
        loop {
            let started = std::time::Instant::now();
            let got = self.file.read(&mut buf);
            crate::progress::note_if_slow("one tape block read", started.elapsed());
            match got {
                Ok(0) => return Ok((total, ReadEnd::Filemark)),
                Ok(n) => {
                    sink.write_all(&buf[..n])
                        .map_err(|e| TapectlError::TapeIo(format!("sink write: {e}")))?;
                    total += n as u64;
                }
                Err(e) if e.raw_os_error() == Some(28) => {
                    return Ok((total, ReadEnd::EndOfMedium));
                }
                Err(e) => {
                    self.note_failure("read", &e);
                    return Err(TapectlError::TapeIo(format!("read: {e}")));
                }
            }
        }
    }

    /// Read at most `max_bytes` from the start of the current file, then
    /// stop — without reading to the file mark.
    ///
    /// For attesting escrow coverage (#137): an age header is a few hundred
    /// bytes, and a data slice can be tens of gigabytes. Reading one block
    /// and stopping is the difference between "attest a shelf of tapes over
    /// lunch" and "over a week". Usually leaves the head mid-file, and the
    /// [`ReadEnd`] says whether it did: [`ReadEnd::Stopped`] is inside the
    /// file; [`ReadEnd::Filemark`] means the file was shorter than asked and
    /// the head is already at the next one. `TapeStore`'s file cursor needs
    /// the difference (issue #389): a forward space after a crossed filemark
    /// would skip a whole file.
    pub fn read_file_head(
        &mut self,
        max_bytes: u64,
        sink: &mut dyn Write,
    ) -> Result<(u64, ReadEnd)> {
        let mut total = 0u64;
        let read_size = if self.block_size > 0 {
            self.block_size
        } else {
            1024 * 1024
        };
        let mut buf = vec![0u8; read_size];
        while total < max_bytes {
            match self.file.read(&mut buf) {
                // File mark: the file is shorter than asked.
                Ok(0) => return Ok((total, ReadEnd::Filemark)),
                Ok(n) => {
                    let take = (n as u64).min(max_bytes - total) as usize;
                    sink.write_all(&buf[..take])
                        .map_err(|e| TapectlError::TapeIo(format!("sink write: {e}")))?;
                    total += take as u64;
                }
                Err(e) if e.raw_os_error() == Some(28) => {
                    return Ok((total, ReadEnd::EndOfMedium));
                }
                Err(e) => {
                    self.note_failure("read", &e);
                    return Err(TapectlError::TapeIo(format!("read: {e}")));
                }
            }
        }
        Ok((total, ReadEnd::Stopped))
    }
}

/// Read the drive's current density code via `MTIOCGET`'s `mt_dsreg` field
/// (ADR-0010's third, last-resort detection source).
///
/// Opens `device` read-only and issues *only* `MTIOCGET` — deliberately not
/// via [`TapeDevice::open_read`], whose construction calls `MTSETBLK`
/// (`set_block_size`) as a side effect; `MTIOCGET` alone causes no tape
/// motion and changes no drive state, unlike that. `None` means "not
/// reported" (an unloaded drive, or one whose driver leaves the field 0).
pub fn density_code(device: &str) -> Result<Option<u8>> {
    // A blocking open: on an empty drive it waits out the st driver's
    // no-medium timeout, so it is a named wait (issue #386).
    let file = crate::progress::waited(
        || format!("opening {device} to read its density register"),
        || OpenOptions::new().read(true).open(device),
    )
    .map_err(|e| TapectlError::TapeIo(format!("open {device}: {e}")))?;
    let mut mtget = MtGet::default();
    let rc = unsafe { nix::libc::ioctl(file.as_raw_fd(), MTIOCGET, &mut mtget as *mut MtGet) };
    if rc != 0 {
        return Err(TapectlError::TapeIo(format!(
            "MTIOCGET: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(density_from_dsreg(mtget.mt_dsreg))
}

/// Extract the density code byte from `MTIOCGET`'s `mt_dsreg` register: the
/// top byte, `(dsreg >> 24) & 0xff`. `0` means "not reported" and is
/// normalized to `None` rather than the misleading byte value `0x00`.
pub fn density_from_dsreg(dsreg: i64) -> Option<u8> {
    let code = ((dsreg >> 24) & 0xff) as u8;
    if code == 0 {
        None
    } else {
        Some(code)
    }
}

#[cfg(test)]
mod density_tests {
    use super::*;

    #[test]
    fn density_from_dsreg_extracts_the_top_byte() {
        // Real LTO-6 capture shape: density 0x5a in the top byte, other
        // bits carrying unrelated driver state.
        assert_eq!(density_from_dsreg(0x5a00_1234), Some(0x5a));
        assert_eq!(density_from_dsreg(0x5e00_0000), Some(0x5e));
    }

    #[test]
    fn density_from_dsreg_zero_top_byte_is_none() {
        assert_eq!(density_from_dsreg(0x0000_1234), None);
        assert_eq!(density_from_dsreg(0), None);
    }

    #[test]
    fn density_from_dsreg_negative_register_still_extracts_top_byte() {
        // mt_dsreg is signed (i64); a real register value can set high bits.
        assert_eq!(density_from_dsreg(-1i64), Some(0xff));
    }

    /// Issue #408: ENOSPC on a tape write is a full medium, the one store
    /// failure a write session aborts on; any other errno is a tape I/O
    /// error, which leaves the session resumable.
    #[test]
    fn a_write_enospc_is_a_full_medium_and_anything_else_a_tape_error() {
        let full = write_error("write", &io::Error::from_raw_os_error(28));
        assert!(matches!(full, TapectlError::MediumFull(_)), "{full:?}");
        let eio = write_error("write", &io::Error::from_raw_os_error(5));
        assert!(matches!(eio, TapectlError::TapeIo(_)), "{eio:?}");
    }

    #[test]
    fn density_code_on_a_nonexistent_device_errors_without_panicking() {
        let err = density_code("/nonexistent/tapectl-media-detect-test-device").unwrap_err();
        assert!(format!("{err}").contains("open"));
    }
}
