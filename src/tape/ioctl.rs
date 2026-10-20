use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;

use crate::error::{Result, TapectlError};

// Linux tape ioctl constants from <linux/mtio.h>
const MTIOCTOP: nix::libc::Ioctl = 0x40086d01;
/// `_IOR('m', 2, struct mtget)` from `<linux/mtio.h>`.
///
/// `pub(crate)` so `tape::media_detect`'s no-medium probe (issue #152) reads
/// the SAME kernel ABI this module does. It briefly had its own copy; two
/// declarations of an ioctl number and a `#[repr(C)]` layout that `unsafe`
/// code casts a raw pointer through is a drift hazard of a nastier kind than
/// most — a divergence would not fail to compile, it would read garbage.
pub(crate) const MTIOCGET: nix::libc::Ioctl = 0x80306d02u32 as nix::libc::Ioctl;

// mtop operation codes
const MTREW: i16 = 6;
const MTWEOF: i16 = 5;
const MTWEOFI: i16 = 35;
const MTSETBLK: i16 = 20;
const MTFSF: i16 = 1;
const MTCOMP: i16 = 32; // MTCOMPRESSION
/// `MTERASE` from <linux/mtio.h>. st sends ERASE with the LONG bit set when
/// the count is non-zero (`st.c`: `cmd[1] = (arg ? 1 : 0)`), which is what
/// `mt erase` does: the whole medium, hours on real LTO.
const MTERASE: i16 = 13;
/// `MTOFFL` from <linux/mtio.h>: rewind and unload the cartridge (`mt
/// offline`). st sends START STOP UNIT with the load bit clear; the command
/// completes when the drive has put the cartridge out, so a full tape's
/// rewind is part of the wait.
const MTOFFL: i16 = 7;

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

/// A tape write error: ENOSPC — a full medium — is
/// [`TapectlError::MediumFull`], the one store failure a write session aborts
/// on; anything else is [`TapectlError::TapeIo`] (issue #408). Since 1.6.0
/// (#420) a block write's ENOSPC reaches here only at the hard end: the early
/// warning's refusals are retried by [`write_block_retrying`] first.
pub(crate) fn write_error(what: &str, e: &io::Error) -> TapectlError {
    if e.raw_os_error() == Some(nix::libc::ENOSPC) {
        TapectlError::MediumFull(format!("{what}: {e}"))
    } else {
        TapectlError::TapeIo(format!("{what}: {e}"))
    }
}

/// More than this many refusals of one block in a row is the hard end
/// (`volume-format-v2.md` §4.3 W2): the medium has no room left, and the
/// write fails as [`TapectlError::MediumFull`]. The measured runs past the
/// early warning were at most 3 (`docs/runs/2026-10-20-lto6-profile-and-lbp.md`).
pub const MAX_REFUSALS_IN_A_ROW: u32 = 16;

/// The drive's early warning, as a store saw it (`volume-format-v2.md` §4.3
/// W1; `layout-session.md`, the Store seam): the first block write st
/// refused with ENOSPC. `None` until then, and from then on `Some` for the
/// rest of the store's life — sticky, because the warning is a fact about
/// where the medium is, and the drive keeps refusing about one block write in
/// two past it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EarlyWarning {
    /// The bytes the store had accepted since it was OPENED, before its
    /// first refused block write — block-padded bytes of data, filemarks
    /// adding nothing. Not counted from the beginning of the tape: a store
    /// opened to resume a session starts at the cursor, and the session adds
    /// the Layout's padded sizes of every file before the first entry this
    /// process wrote (`layout-session.md`, "The `write_early_warning`
    /// event").
    pub bytes_at: u64,
    /// Every refused block write the store has retried, its whole life. A
    /// refusal that made the hard end was not retried and is not counted.
    pub refusals: u64,
}

/// A device's record of its own block writes, for [`write_block_retrying`]:
/// the bytes accepted since it opened, and the [`EarlyWarning`] once it
/// came. One per open device (`TapeDevice`), and one per test double.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EarlyWarningState {
    accepted: u64,
    warning: Option<EarlyWarning>,
}

impl EarlyWarningState {
    /// The warning, once the first block write was refused.
    pub(crate) fn warning(&self) -> Option<EarlyWarning> {
        self.warning
    }

    /// Every byte a block write has accepted since the device opened.
    pub(crate) fn accepted(&self) -> u64 {
        self.accepted
    }

    /// The refusals retried so far: 0 before the warning.
    pub(crate) fn refusals(&self) -> u64 {
        self.warning.map_or(0, |w| w.refusals)
    }
}

/// Write one tape block whole through `write` — one `write(2)` per call —
/// retrying a block st refuses past the early warning (`volume-format-v2.md`
/// §4.3 W1, W2). The one implementation: `TapeDevice` runs its block writes
/// through it, and so do the test doubles (`MemStore`, the in-memory
/// `FakeTape`), so a test of either runs the real retry.
///
/// - **ENOSPC** is a refusal: st refused the block without sending it, and
///   nothing of it was written. The first of the device's life records the
///   [`EarlyWarning`] in `state`, at the bytes accepted before it. Every
///   refusal is counted and the same bytes are written again. More than
///   [`MAX_REFUSALS_IN_A_ROW`] refusals of this block in a row is the hard
///   end: that last ENOSPC is returned, which [`write_error`] makes
///   [`TapectlError::MediumFull`].
/// - **A short write** keeps `write_all`'s handling, as before: the rest is
///   written by the next call, a write of 0 bytes is a `WriteZero` error,
///   and EINTR is retried.
/// - **Any other error** is returned unchanged, and records nothing.
///
/// Block writes only. A filemark is never retried (W3): a retried filemark
/// that had in fact been written would add an empty file and move every
/// later position, so a refused filemark stays the medium-full error.
pub(crate) fn write_block_retrying(
    mut write: impl FnMut(&[u8]) -> io::Result<usize>,
    buf: &[u8],
    state: &mut EarlyWarningState,
) -> io::Result<()> {
    let mut rest = buf;
    let mut in_a_row = 0u32;
    while !rest.is_empty() {
        match write(rest) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ))
            }
            Ok(n) => {
                state.accepted += n as u64;
                rest = &rest[n..];
                in_a_row = 0;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.raw_os_error() == Some(nix::libc::ENOSPC) => {
                in_a_row += 1;
                if in_a_row > MAX_REFUSALS_IN_A_ROW {
                    return Err(e);
                }
                let bytes_at = state.accepted;
                state
                    .warning
                    .get_or_insert(EarlyWarning {
                        bytes_at,
                        refusals: 0,
                    })
                    .refusals += 1;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
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
        MTERASE => "erase (long)".to_string(),
        other => format!("ioctl op {other} count {count}"),
    }
}

/// A wrapper around a tape device file descriptor.
///
/// It notes the st driver's whole `MTIOCGET` status when it opens, after a
/// tape command fails and when it closes (issue #344,
/// [`crate::tape::mtget_journal`]); the contact it works for journals those
/// readings when it closes. Every `MTIOCGET` it issues — the position reads
/// too — goes through [`Self::read_status`], which adds st's read-to-clear
/// recovered-error count to the device's tally.
pub struct TapeDevice {
    file: File,
    block_size: usize,
    /// The path it was opened by, for the journal.
    path: String,
    /// st's recovered errors, added up across every `MTIOCGET` since the
    /// open reading (issue #344).
    recovered: crate::tape::mtget_journal::RecoveredTally,
}

impl Drop for TapeDevice {
    fn drop(&mut self) {
        self.note(crate::tape::mtget_journal::POINT_CLOSE, None, None);
    }
}

impl TapeDevice {
    /// The st driver's whole `MTIOCGET` status (issue #344): an ioctl
    /// answered by the st driver, not a log-page read (st flushes pending
    /// write-behind first). It is NOT free to read: st clears its
    /// recovered-error register (`mt_erreg`) on every `MTIOCGET`, so this is
    /// the device's one `MTIOCGET` call site, and it adds each reading's
    /// count to [`Self::recovered`] before anything else sees it.
    fn read_status(&self) -> std::result::Result<crate::tape::mtget_journal::MtStatus, String> {
        let mut m = MtGet::default();
        let rc = unsafe { nix::libc::ioctl(self.raw_fd(), MTIOCGET, &mut m as *mut MtGet) };
        let status = if rc != 0 {
            Err(format!("MTIOCGET: {}", io::Error::last_os_error()))
        } else {
            Ok(crate::tape::mtget_journal::MtStatus {
                mt_type: m.mt_type,
                mt_resid: m.mt_resid,
                mt_dsreg: m.mt_dsreg,
                mt_gstat: m.mt_gstat,
                mt_erreg: m.mt_erreg,
                mt_fileno: m.mt_fileno,
                mt_blkno: m.mt_blkno,
            })
        };
        self.recovered.observe(&status);
        status
    }

    /// Note the status for the contact this thread works for (issue #344).
    /// `command` and `errno` name a failed tape command; the errno is taken
    /// by the caller before this, since MTIOCGET is itself a syscall. The
    /// open reading restarts the tally: what it carried in accumulated
    /// before this device opened.
    fn note(&self, point: &'static str, command: Option<&str>, errno: Option<i32>) {
        let status = self.read_status();
        if point == crate::tape::mtget_journal::POINT_OPEN {
            self.recovered.restart();
        }
        crate::tape::mtget_journal::note_reading(
            point,
            &self.path,
            command,
            errno,
            status,
            Some(self.recovered.total()),
        );
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
            recovered: Default::default(),
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
            recovered: Default::default(),
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

    /// Erase the whole medium from the head onward — `mt erase`, the LONG
    /// erase (ADR-0012 2026-10-12 item 52: `cartridge erase --long`). Hours
    /// on real LTO; instant on mhvtl. A named wait, as every `mt_ioctl` is.
    pub fn erase_long(&self) -> Result<()> {
        self.mt_ioctl(MTERASE, 1)
    }

    /// Forward space N file marks.
    pub fn forward_space_file(&self, count: i32) -> Result<()> {
        self.mt_ioctl(MTFSF, count)
    }

    /// Get current tape position.
    pub fn get_position(&self) -> Result<TapePosition> {
        let mtget = self.read_status().map_err(TapectlError::TapeIo)?;
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

/// `GMT_*` bits of `mt_gstat` that [`TapeNodeStatus`] names, from
/// `<linux/mtio.h>`. `mtget_journal::GSTAT_BITS` lists the same bits by
/// name for the journal; these are the ones `drive status` branches on.
const GMT_BOT: i64 = 0x4000_0000;
const GMT_EOT: i64 = 0x2000_0000;
const GMT_WR_PROT: i64 = 0x0400_0000;
const GMT_ONLINE: i64 = 0x0100_0000;
const GMT_DR_OPEN: i64 = 0x0004_0000;
const GMT_CLN: i64 = 0x0000_8000;

/// What the st driver says is in the drive (`MTIOCGET`, read by
/// [`status_nonblocking`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CartridgeState {
    /// `GMT_DR_OPEN`: "the drive does not have a tape in place". Checked
    /// first because mhvtl (and some real drives) report `DR_OPEN` together
    /// with `ONLINE` when empty.
    Empty,
    /// `GMT_ONLINE` without `DR_OPEN`: a cartridge is loaded and ready.
    Loaded,
    /// Neither: a cartridge is present and the drive is not ready (still
    /// loading or calibrating, or faulted).
    NotReady,
}

/// The st driver's `MTIOCGET` answer, read through a descriptor that does
/// nothing else ([`status_nonblocking`]), decoded for `drive status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapeNodeStatus {
    pub raw: crate::tape::mtget_journal::MtStatus,
}

impl TapeNodeStatus {
    pub fn cartridge(&self) -> CartridgeState {
        if self.raw.mt_gstat & GMT_DR_OPEN != 0 {
            CartridgeState::Empty
        } else if self.raw.mt_gstat & GMT_ONLINE != 0 {
            CartridgeState::Loaded
        } else {
            CartridgeState::NotReady
        }
    }

    /// The cartridge's write-protect tab, or `None` when no ready cartridge
    /// makes the bit mean anything (st leaves it stale on an empty drive).
    pub fn write_protected(&self) -> Option<bool> {
        (self.cartridge() == CartridgeState::Loaded).then_some(self.raw.mt_gstat & GMT_WR_PROT != 0)
    }

    /// st's own file count, `None` where it has lost track (`-1`) or there
    /// is no ready cartridge to be positioned on.
    pub fn file_number(&self) -> Option<i32> {
        (self.cartridge() == CartridgeState::Loaded && self.raw.mt_fileno >= 0)
            .then_some(self.raw.mt_fileno)
    }

    /// st's own block count within the file, with [`Self::file_number`]'s
    /// caveats.
    pub fn block_number(&self) -> Option<i32> {
        (self.cartridge() == CartridgeState::Loaded && self.raw.mt_blkno >= 0)
            .then_some(self.raw.mt_blkno)
    }

    /// `GMT_BOT`: st believes the head is at the beginning of tape.
    pub fn at_bot(&self) -> bool {
        self.cartridge() == CartridgeState::Loaded && self.raw.mt_gstat & GMT_BOT != 0
    }

    pub fn at_eod(&self) -> bool {
        self.cartridge() == CartridgeState::Loaded && self.raw.mt_gstat & GMT_EOD != 0
    }

    pub fn at_eot(&self) -> bool {
        self.cartridge() == CartridgeState::Loaded && self.raw.mt_gstat & GMT_EOT != 0
    }

    /// `GMT_CLN`: the drive has asked to be cleaned.
    pub fn cleaning_requested(&self) -> bool {
        self.raw.mt_gstat & GMT_CLN != 0
    }

    /// The density code st last saw, from `mt_dsreg`'s top byte.
    pub fn density_code(&self) -> Option<u8> {
        density_from_dsreg(self.raw.mt_dsreg)
    }

    /// The block size st holds (`mt_dsreg`'s low 24 bits; 0 is variable).
    pub fn block_size(&self) -> i64 {
        self.raw.mt_dsreg & 0xff_ffff
    }
}

/// Read what is in the drive: open `device` read-only with `O_NONBLOCK`,
/// issue `MTIOCGET`, close (`drive status`, issue #428).
///
/// The same open `mt status` makes, and the same one
/// `media_detect::probe_no_medium` makes: `O_NONBLOCK` skips the st driver's
/// wait for the drive to become ready, so an empty drive answers at once
/// instead of after `ST_BLOCK_SECONDS`. Deliberately NOT via
/// [`TapeDevice::open_read`], whose construction sends `MTSETBLK`; and no
/// rewind, no read, no write: nothing here moves tape or changes a mode.
///
/// Errors are the open's or the ioctl's own errno, unwrapped, because the
/// caller tells them apart: `ENOENT`/`ENODEV`/`ENXIO` (no such drive),
/// `EBUSY` (st allows one opener: another process has the drive open) and
/// `EACCES` (not in the tape group) are different answers for the operator.
///
/// Like every `MTIOCGET`, it clears st's recovered-error register; the count
/// it held is in the answer ([`TapeNodeStatus::raw`]'s `mt_erreg`).
pub fn status_nonblocking(device: &str) -> io::Result<TapeNodeStatus> {
    use std::os::unix::fs::OpenOptionsExt;
    if node_rewinds_on_close(device) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{device} is a rewinding tape node (stN, not nstN): the st driver rewinds the \
                 tape when it is closed, so status will not open it; name the non-rewinding \
                 node (/dev/nstN, or the by-id link ending -nst)"
            ),
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(device)?;
    let mut m = MtGet::default();
    let rc = unsafe { nix::libc::ioctl(file.as_raw_fd(), MTIOCGET, &mut m as *mut MtGet) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(TapeNodeStatus {
        raw: crate::tape::mtget_journal::MtStatus {
            mt_type: m.mt_type,
            mt_resid: m.mt_resid,
            mt_dsreg: m.mt_dsreg,
            mt_gstat: m.mt_gstat,
            mt_erreg: m.mt_erreg,
            mt_fileno: m.mt_fileno,
            mt_blkno: m.mt_blkno,
        },
    })
}

/// Rewind and unload the cartridge in `device`: `MTOFFL`, the operation `mt
/// offline` performs (`drive eject`, ADR-0012 2026-10-15 item 61).
///
/// STANDALONE, deliberately: not a [`TapeDevice`] method and not in
/// [`crate::store::TapeOps`]. A `TapeDevice` sends `MTSETBLK` when it opens,
/// and unloading a cartridge is not an operation a [`crate::store::Store`]
/// has any business offering — a session that held one could unload the tape
/// it is writing. The test `the_device_issues_mtoffl_in_one_place` pins the
/// one call site.
///
/// The caller has already read the node's status ([`status_nonblocking`])
/// and found a ready cartridge, so this open is the blocking one `mt` makes
/// (the st driver's `check_tape` runs at open) and cannot wait out the
/// no-medium timeout. A rewinding node (`stN`) is refused unopened, as
/// [`status_nonblocking`] refuses it, for the same reason: this command
/// names its drive by the non-rewinding node.
///
/// The unload is a named wait ([`crate::progress::waited`]): a full LTO
/// cartridge rewinds for a minute or more before it comes out.
pub fn offline(device: &str) -> io::Result<()> {
    if node_rewinds_on_close(device) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{device} is a rewinding tape node (stN, not nstN); name the non-rewinding \
                 node (/dev/nstN, or the by-id link ending -nst)"
            ),
        ));
    }
    let file = crate::progress::waited(
        || format!("opening {device} to unload its cartridge"),
        || OpenOptions::new().read(true).open(device),
    )?;
    let mtop = MtOp {
        mt_op: MTOFFL,
        _pad: 0,
        mt_count: 0,
    };
    let rc = crate::progress::waited(
        || format!("unloading the cartridge from {device} (rewind, then eject)"),
        || unsafe { nix::libc::ioctl(file.as_raw_fd(), MTIOCTOP, &mtop as *const MtOp) },
    );
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `SCSI_TAPE_MAJOR` (`<linux/major.h>`): the st driver's block of device
/// numbers.
const SCSI_TAPE_MAJOR: u64 = 9;

/// Whether `device` is an st node that rewinds the tape when it is closed
/// (`/dev/stN`, as opposed to `/dev/nstN`). Read from the device number the
/// kernel assigned, so a by-id link and a renamed node are judged by what
/// they are: bit 7 of an st minor is "no rewind" (`TAPE_REWIND` in `st.c`).
/// Anything that is not an st character device — a path that does not exist,
/// a test's regular file — answers `false`.
fn node_rewinds_on_close(device: &str) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Ok(meta) = std::fs::metadata(device) else {
        return false;
    };
    meta.file_type().is_char_device()
        && st_minor_rewinds(
            nix::sys::stat::major(meta.rdev()),
            nix::sys::stat::minor(meta.rdev()),
        )
}

/// [`node_rewinds_on_close`] for a device number.
fn st_minor_rewinds(major: u64, minor: u64) -> bool {
    major == SCSI_TAPE_MAJOR && minor & 0x80 == 0
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

    /// Issue #344: st clears its recovered-error register on every
    /// `MTIOCGET`, so a `TapeDevice` that read it anywhere but through the
    /// one method that adds each reading to its tally would lose counts. The
    /// rule is held structurally: the device's descriptor is passed to
    /// `MTIOCGET` in exactly one place. (`density_code` opens its own
    /// descriptor before any `TapeDevice` exists — st refuses a second
    /// opener — and is the one other call, counted here so a third is seen.)
    #[test]
    fn the_device_issues_mtiocget_in_one_place() {
        let src = include_str!("ioctl.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        assert_eq!(
            body.matches("ioctl(self.raw_fd(), MTIOCGET").count(),
            1,
            "TapeDevice issues MTIOCGET only through read_status"
        );
        // Issue #428: `status_nonblocking` is the third (`drive status`),
        // on its own descriptor like `density_code` — never on a
        // `TapeDevice`'s, whose tally it would bypass.
        assert_eq!(
            body.matches(", MTIOCGET,").count(),
            3,
            "density_code and status_nonblocking, each on a descriptor of its own"
        );
    }

    /// ADR-0012 2026-10-15 item 61: unloading the cartridge is one standalone
    /// function. The `MTOFFL` op is issued in exactly one place (`offline`),
    /// no `TapeDevice` method or `TapeOps` impl offers it, and neither does
    /// the `Store` seam: `src/store.rs` does not so much as name it. The
    /// positive control is the same scan finding the one call it must.
    #[test]
    fn the_device_issues_mtoffl_in_one_place() {
        let src = include_str!("ioctl.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        assert_eq!(
            body.matches("mt_op: MTOFFL").count(),
            1,
            "MTOFFL is issued by `offline` alone"
        );
        let offline_at = body.find("pub fn offline(").expect("the one call site");
        let call_at = body.find("mt_op: MTOFFL").unwrap();
        assert!(
            call_at > offline_at,
            "the MTOFFL op is built inside `offline`, not in a TapeDevice method"
        );
        assert!(
            !body.contains("mt_ioctl(MTOFFL"),
            "TapeDevice must not offer an unload"
        );
        let store = include_str!("../store.rs");
        let store_body = store.split("#[cfg(test)]").next().unwrap();
        for word in ["MTOFFL", "ioctl::offline", "unload"] {
            assert!(
                !store_body.contains(word),
                "the Store seam must not offer an unload ({word} found in src/store.rs)"
            );
        }
    }

    #[test]
    fn offline_of_a_nonexistent_node_is_its_errno_and_a_non_tape_is_refused_by_the_kernel() {
        let err = offline("/nonexistent/tapectl-drive-eject-test/nst0").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        // /dev/null opens, and the ioctl refuses it (ENOTTY): the function
        // reports the kernel's answer, it does not pretend.
        let err = offline("/dev/null").unwrap_err();
        assert_eq!(err.raw_os_error(), Some(nix::libc::ENOTTY), "{err}");
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
    fn node(gstat: i64, fileno: i32, blkno: i32) -> TapeNodeStatus {
        TapeNodeStatus {
            raw: crate::tape::mtget_journal::MtStatus {
                mt_gstat: gstat,
                mt_dsreg: 0x5a00_0000 | 524_288,
                mt_fileno: fileno,
                mt_blkno: blkno,
                ..Default::default()
            },
        }
    }

    /// Issue #428: the states `drive status` reports, from the `mt_gstat`
    /// shapes `media_detect`'s tests record. `DR_OPEN` wins over `ONLINE`
    /// (an empty mhvtl drive carries both).
    #[test]
    fn the_cartridge_state_is_read_from_the_gstat_bits() {
        let empty = node(GMT_DR_OPEN | GMT_ONLINE, 0, 0);
        assert_eq!(empty.cartridge(), CartridgeState::Empty);
        assert_eq!(empty.write_protected(), None, "no cartridge, no tab");
        assert_eq!(empty.file_number(), None, "no cartridge, no position");
        assert!(!empty.at_bot());

        let loaded = node(GMT_BOT | GMT_ONLINE, 0, 0);
        assert_eq!(loaded.cartridge(), CartridgeState::Loaded);
        assert!(loaded.at_bot());
        assert_eq!(loaded.write_protected(), Some(false));
        assert_eq!(loaded.file_number(), Some(0));
        assert_eq!(loaded.block_size(), 524_288);
        assert_eq!(loaded.density_code(), Some(0x5a));

        let tab = node(GMT_BOT | GMT_ONLINE | GMT_WR_PROT, 0, 0);
        assert_eq!(tab.write_protected(), Some(true));

        let loading = node(0, -1, -1);
        assert_eq!(loading.cartridge(), CartridgeState::NotReady);
        assert_eq!(loading.write_protected(), None);
    }

    #[test]
    fn a_position_st_has_lost_is_not_reported_as_a_number() {
        let lost = node(GMT_ONLINE, -1, -1);
        assert_eq!(lost.file_number(), None);
        assert_eq!(lost.block_number(), None);
        let mid = node(GMT_ONLINE | GMT_EOD, 4, 12);
        assert_eq!((mid.file_number(), mid.block_number()), (Some(4), Some(12)));
        assert!(mid.at_eod() && !mid.at_eot() && !mid.at_bot());
    }

    #[test]
    fn cleaning_is_requested_by_the_cln_bit_alone() {
        assert!(node(GMT_ONLINE | GMT_CLN, 0, 0).cleaning_requested());
        assert!(!node(GMT_ONLINE, 0, 0).cleaning_requested());
    }

    /// The gstat constants here are the ones the journal names, so a status
    /// reading and a journalled one can never disagree about a bit.
    #[test]
    fn the_status_bits_agree_with_the_journals() {
        let all = crate::tape::mtget_journal::MtStatus {
            mt_gstat: GMT_BOT | GMT_EOT | GMT_WR_PROT | GMT_ONLINE | GMT_DR_OPEN | GMT_CLN,
            ..Default::default()
        };
        assert_eq!(
            all.gstat_flags(),
            ["BOT", "EOT", "WR_PROT", "ONLINE", "DR_OPEN", "CLN"]
        );
    }

    /// A rewinding node is refused before it is opened: closing it would
    /// rewind, and a status never moves tape.
    #[test]
    fn a_rewinding_st_node_is_recognised_by_its_device_number() {
        assert!(st_minor_rewinds(9, 0), "/dev/st0");
        assert!(st_minor_rewinds(9, 32), "/dev/st0l");
        assert!(st_minor_rewinds(9, 3), "/dev/st3");
        assert!(!st_minor_rewinds(9, 128), "/dev/nst0");
        assert!(!st_minor_rewinds(9, 160), "/dev/nst0l");
        assert!(!st_minor_rewinds(8, 0), "a disk is not a tape");
        assert!(!node_rewinds_on_close("/nonexistent/tapectl/st0"));
        assert!(!node_rewinds_on_close("/dev/null"), "not an st node");
    }

    #[test]
    fn status_of_a_nonexistent_node_is_its_errno_not_a_panic() {
        let err = status_nonblocking("/nonexistent/tapectl-drive-status-test/nst0").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    // --- the early warning's retry (#420, `volume-format-v2.md` §4.3) -----

    fn enospc() -> io::Error {
        io::Error::from_raw_os_error(nix::libc::ENOSPC)
    }

    /// A scripted st: each block `write(2)` takes the next answer — `true`
    /// accepts the whole buffer, `false` refuses it with ENOSPC — and every
    /// accepted buffer is recorded. Past the script, every write is accepted.
    struct Scripted {
        answers: std::collections::VecDeque<bool>,
        attempts: usize,
        written: Vec<Vec<u8>>,
    }

    impl Scripted {
        fn new(answers: &[bool]) -> Self {
            Self {
                answers: answers.iter().copied().collect(),
                attempts: 0,
                written: Vec::new(),
            }
        }

        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.attempts += 1;
            if self.answers.pop_front().unwrap_or(true) {
                self.written.push(buf.to_vec());
                Ok(buf.len())
            } else {
                Err(enospc())
            }
        }
    }

    /// W1/W2: st alternates past the early warning. The block that crosses
    /// it is accepted; the next is refused at once, retried with the same
    /// bytes and accepted; and so on. Every block lands once, the warning is
    /// recorded at the bytes accepted before the first refusal, and it
    /// stays, counting every refusal after it.
    #[test]
    fn a_refused_block_is_written_again_and_the_warning_is_recorded_once() {
        let mut st = Scripted::new(&[true, true, false, true, false, true, false, true]);
        let mut state = EarlyWarningState::default();
        for i in 0..2u8 {
            write_block_retrying(|b| st.write(b), &[i; 4], &mut state).unwrap();
        }
        assert_eq!(state.warning(), None, "no refusal yet");
        assert_eq!(state.accepted(), 8);
        for i in 2..5u8 {
            write_block_retrying(|b| st.write(b), &[i; 4], &mut state).unwrap();
        }
        assert_eq!(
            st.written,
            (0..5u8).map(|i| vec![i; 4]).collect::<Vec<_>>(),
            "every block once, in order: a refused block wrote nothing"
        );
        assert_eq!(st.attempts, 8, "three blocks retried once each");
        assert_eq!(
            state.warning(),
            Some(EarlyWarning {
                bytes_at: 8,
                refusals: 3
            })
        );
        assert_eq!((state.accepted(), state.refusals()), (20, 3));

        // Sticky: blocks accepted at once after it leave it as it was, and
        // `bytes_at` never moves.
        write_block_retrying(|b| st.write(b), &[9; 4], &mut state).unwrap();
        assert_eq!(state.warning().unwrap().bytes_at, 8);
        assert_eq!(state.refusals(), 3);
    }

    /// The measured runs were up to 3 refusals of one block in a row; a run
    /// of any length up to [`MAX_REFUSALS_IN_A_ROW`] is retried, and the
    /// run counter starts again with each block.
    #[test]
    fn runs_of_refusals_up_to_the_limit_are_retried() {
        let mut answers = vec![false, false, true, false, false, false, true];
        answers.extend(std::iter::repeat_n(false, MAX_REFUSALS_IN_A_ROW as usize));
        answers.push(true);
        let mut st = Scripted::new(&answers);
        let mut state = EarlyWarningState::default();
        for i in 0..3u8 {
            write_block_retrying(|b| st.write(b), &[i; 2], &mut state).unwrap();
        }
        assert_eq!(st.written.len(), 3);
        assert_eq!(
            state.warning(),
            Some(EarlyWarning {
                bytes_at: 0,
                refusals: 2 + 3 + u64::from(MAX_REFUSALS_IN_A_ROW)
            })
        );
    }

    /// More than [`MAX_REFUSALS_IN_A_ROW`] refusals of one block in a row is
    /// the hard end: the last ENOSPC comes back, a full medium; the refusal
    /// that made it was not retried and is not counted.
    #[test]
    fn more_than_sixteen_refusals_of_one_block_is_the_hard_end() {
        let refusals = MAX_REFUSALS_IN_A_ROW as usize + 1;
        let mut st = Scripted::new(&[true]);
        st.answers.extend(std::iter::repeat_n(false, refusals));
        let mut state = EarlyWarningState::default();
        write_block_retrying(|b| st.write(b), &[1; 4], &mut state).unwrap();
        let err = write_block_retrying(|b| st.write(b), &[2; 4], &mut state).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(nix::libc::ENOSPC));
        assert_eq!(st.attempts, 1 + refusals, "17 attempts, then no more");
        assert_eq!(st.written, vec![vec![1; 4]]);
        assert_eq!(
            state.warning(),
            Some(EarlyWarning {
                bytes_at: 4,
                refusals: u64::from(MAX_REFUSALS_IN_A_ROW)
            })
        );
        assert!(matches!(
            write_error("write", &err),
            TapectlError::MediumFull(_)
        ));
    }

    /// Any error but ENOSPC (and EINTR) comes back at once, unchanged, and
    /// records no warning: a medium error is not the early warning.
    #[test]
    fn any_other_error_passes_through_and_records_nothing() {
        let mut state = EarlyWarningState::default();
        let mut attempts = 0;
        let err = write_block_retrying(
            |_| {
                attempts += 1;
                Err(io::Error::from_raw_os_error(nix::libc::EIO))
            },
            &[1; 4],
            &mut state,
        )
        .unwrap_err();
        assert_eq!(err.raw_os_error(), Some(nix::libc::EIO));
        assert_eq!(attempts, 1);
        assert_eq!(state, EarlyWarningState::default());
        assert!(matches!(write_error("write", &err), TapectlError::TapeIo(_)));
    }

    /// A short write keeps `write_all`'s handling: the rest goes in the next
    /// call, EINTR is retried, and a write of nothing is `WriteZero` — none
    /// of them a refusal.
    #[test]
    fn a_short_write_keeps_write_alls_handling() {
        let mut state = EarlyWarningState::default();
        let mut got = Vec::new();
        let mut calls = 0;
        write_block_retrying(
            |b| {
                calls += 1;
                match calls {
                    1 => Err(io::Error::from(io::ErrorKind::Interrupted)),
                    2 => {
                        got.extend_from_slice(&b[..3]);
                        Ok(3)
                    }
                    _ => {
                        got.extend_from_slice(b);
                        Ok(b.len())
                    }
                }
            },
            &[7; 8],
            &mut state,
        )
        .unwrap();
        assert_eq!((got, calls), (vec![7; 8], 3));
        assert_eq!((state.accepted(), state.warning()), (8, None));

        let err = write_block_retrying(|_| Ok(0), &[7; 8], &mut state).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WriteZero);
        assert_eq!(state.warning(), None);
    }
}
