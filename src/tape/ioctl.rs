use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;

use crate::error::{Result, TapectlError};
use crate::tape::lbp::{self, Protection};

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
/// `_IOR('m', 3, struct mtpos)` from `<linux/mtio.h>`: st sends READ
/// POSITION and answers the drive's logical object position — blocks and
/// filemarks from the beginning of the tape (#388, ADR-0012 item 174: where
/// a refused write left the drive). 64-bit `long`, as [`MTIOCGET`]'s layout
/// assumes.
const MTIOCPOS: nix::libc::Ioctl = 0x80086d03u32 as nix::libc::Ioctl;

/// `struct mtpos` from `<linux/mtio.h>`.
#[repr(C)]
#[derive(Debug, Default)]
struct MtPos {
    mt_blkno: nix::libc::c_long,
}

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
    /// The bytes its block writes have accepted since it opened, and the
    /// early warning once st refused one (#420). A new open starts afresh:
    /// past the warning, the drive refuses the first write there again.
    warning: EarlyWarningState,
    /// `TAPECTL_TEST_EARLY_WARNING_AT` (#420): st's answers past an early
    /// warning, given in front of this device's block writes from the bytes
    /// set ([`Self::fake_early_warning_at`]). `None` on every open but a
    /// write store's with the hook set.
    fake_warning: Option<crate::store::MediumEnd>,
    /// The drive's Control Data Protection page while this open has it
    /// armed (#388, ADR-0012 items 163-164): set by the open, cleared by
    /// `Drop`.
    lbp: Option<ArmedPage>,
    /// What this open decided about Logical Block Protection: what a write
    /// session records (migration 041).
    protection: Protection,
    /// The block size st holds now: [`Self::block_size`], or four bytes more
    /// while a check puts the CRC on the wire.
    st_block: usize,
    /// The last st operation was a block write: the close writes the
    /// filemark st's own close would, before it clears the page (§4.3).
    pending_data: Cell<bool>,
    /// `TAPECTL_TEST_LBP_CORRUPT_BLOCK` (#388, F5): the block writes,
    /// counted from 1 since the open (re-sent ones too), whose CRC gets one
    /// bit flipped. Empty unless the hook is set.
    corrupt: lbp::CorruptBlocks,
    /// A test's record of every `MTIOCTOP` this device issues, in order,
    /// beside the page commands of a recording page control (§4.3's close
    /// order). `None` on every open: only a test over a plain file sets it.
    /// Not compiled out of production builds, because this file's source
    /// pins read everything before its test module as the device.
    op_log: Option<std::sync::Arc<std::sync::Mutex<Vec<String>>>>,
}

/// The page an open armed, and the control that clears it.
struct ArmedPage {
    control: Box<dyn lbp::PageControl + Send>,
    armed: lbp::Armed,
}

/// The close sequence (§4.3). Unarmed, the close reading alone, as before
/// #388. Armed: the close reading (its `MTIOCGET` flushes st's write-behind
/// buffer under the armed page); a synchronous filemark when the last st
/// operation was a block write, which is the filemark st's own close would
/// write and flushes the DRIVE's buffer while the page that wrote it is still
/// armed (risk R6); the page put back as found and read back; the block size
/// back to the format's. Nothing here panics: a failure is one loud line.
impl Drop for TapeDevice {
    fn drop(&mut self) {
        self.note(crate::tape::mtget_journal::POINT_CLOSE, None, None);
        if let Some(page) = self.lbp.take() {
            if self.pending_data.get() {
                let _ = self.mt_ioctl_raw(MTWEOF, 1);
            }
            if let Err(reason) = lbp::disarm(&*page.control, &page.armed) {
                crate::progress::say(&lbp::clear_failed_line(&self.path, &reason));
            }
            let _ = self.mt_ioctl_raw(MTSETBLK, self.block_size as i32);
        }
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
        Self::open_with(file, device_path, block_size, true)
    }

    /// Open read-only.
    pub fn open_read(device_path: &str, block_size: usize) -> Result<Self> {
        let file = crate::progress::waited(
            || format!("opening tape device {device_path} for reading"),
            || OpenOptions::new().read(true).open(device_path),
        )
        .map_err(|e| TapectlError::TapeIo(format!("open {device_path}: {e}")))?;
        Self::open_with(file, device_path, block_size, false)
    }

    /// Both opens, once st's exclusive open has succeeded (§2.2): the block
    /// size, then Logical Block Protection (item 164: armed only inside st's
    /// exclusive open, so no other process does data I/O on the drive while
    /// it is), then the open reading — which therefore shows st at the wire
    /// block size while armed. An open that arms rewinds first, for the
    /// probe ([`Self::arm_protection`]); a rewind that fails fails the open.
    fn open_with(
        file: File,
        device_path: &str,
        block_size: usize,
        write_open: bool,
    ) -> Result<Self> {
        let mut dev = Self {
            file,
            block_size,
            path: device_path.to_string(),
            recovered: Default::default(),
            warning: EarlyWarningState::default(),
            fake_warning: None,
            lbp: None,
            protection: Protection::NotApplicable,
            st_block: block_size,
            pending_data: Cell::new(false),
            corrupt: lbp::CorruptBlocks::default(),
            op_log: None,
        };
        dev.set_block_size(block_size)?;
        let control =
            lbp::control_for(device_path).map(|c| Box::new(c) as Box<dyn lbp::PageControl + Send>);
        dev.arm_protection(write_open, lbp::LBP_R_POLICY, control)?;
        dev.note(crate::tape::mtget_journal::POINT_OPEN, None, None);
        Ok(dev)
    }

    /// Logical Block Protection for this open (items 162-165; F2: every
    /// `TapeStore` open comes here). `policy` says what an open of this kind
    /// arms ([`lbp::LbpReadPolicy::open_bits`]); variable-block mode arms
    /// nothing. LBP never fails the open: a drive, an sg node or an st that
    /// cannot take it is [`Protection::Unavailable`], warned once on a write
    /// open (item 165) and logged on a read (F4). An open that arms nothing
    /// still clears a page a crash left armed (§4.4). The one error is the
    /// rewind an arming open makes before anything else, for the probe: it
    /// fails the open before the page is touched, as the store's own rewind
    /// after the open would have.
    fn arm_protection(
        &mut self,
        write_open: bool,
        policy: lbp::LbpReadPolicy,
        control: Option<Box<dyn lbp::PageControl + Send>>,
    ) -> Result<()> {
        let bits = if self.block_size == 0 {
            None
        } else {
            policy.open_bits(write_open)
        };
        let Some(control) = control else {
            if bits.is_some() {
                let reason = lbp::reason_no_sg_node(&self.path);
                lbp::note_unavailable(&self.path, &reason, write_open);
                self.protection = Protection::Unavailable(reason);
            }
            return Ok(());
        };
        let Some(bits) = bits else {
            if let Ok(true) = lbp::clear_leftover(&*control) {
                self.say_leftover(control.node(), true);
            }
            return Ok(());
        };
        // 388c's finding (#388): after a read that failed or ran off the
        // data, st answers the next read itself, from its own end-of-data or
        // error state, without sending it to the drive. The probe below then
        // sees no EBUSY on a path that cannot carry the wire block, the open
        // arms, and its first transfer fails EBUSY (`volume identify` after
        // a failed `dd` on mhvtl; after `mt rewind` it fell back as it
        // should). A rewind clears that state, so the probe is a READ the
        // drive answers, at BOT. Every store rewinds after its open anyway,
        // which also takes the head back over the block the probe read.
        self.rewind()?;
        let payload = self.block_size;
        let armed = {
            let this = &*self;
            let mut set_block_size =
                |n: usize| this.mt_ioctl(MTSETBLK, n as i32).map_err(|e| e.to_string());
            // F12: one read of a wire block at BOT (the rewind above), before
            // the store's own rewind — st answers EBUSY/EOVERFLOW without
            // sending it when the path cannot carry it.
            let mut probe = |n: usize| {
                let mut buf = vec![0u8; n];
                (&this.file).read(&mut buf)
            };
            lbp::arm(&*control, bits, payload, &mut set_block_size, &mut probe)
        };
        match armed {
            Ok(armed) => {
                if armed.leftover {
                    self.say_leftover(control.node(), false);
                }
                self.st_block = payload + lbp::CRC_LEN;
                self.protection = Protection::Lbp {
                    reads: bits & lbp::LBP_R != 0,
                };
                let line = format!(
                    "Logical Block Protection armed on {} ({}): the drive checks tapectl's CRC \
                     on every block written{}",
                    self.path,
                    control.node(),
                    if bits & lbp::LBP_R != 0 {
                        ", and sends its own with every block read"
                    } else {
                        ""
                    }
                );
                crate::progress::log(&line);
                self.lbp = Some(ArmedPage { control, armed });
            }
            Err(not_armed) => {
                if not_armed.leftover {
                    self.say_leftover(control.node(), true);
                }
                self.st_block = payload;
                lbp::note_unavailable(&self.path, &not_armed.reason, write_open);
                self.protection = Protection::Unavailable(not_armed.reason);
            }
        }
        Ok(())
    }

    /// The leftover note (§4.4), read or write: once on stderr and once in
    /// the session log (`progress::say`, never also a tracing event).
    fn say_leftover(&self, node: &str, cleared_now: bool) {
        crate::progress::say(&lbp::leftover_note(&self.path, node, cleared_now));
    }

    /// What this open decided about Logical Block Protection.
    pub fn protection(&self) -> Protection {
        self.protection.clone()
    }

    /// The flag bits armed now (0 when not armed).
    fn armed_bits(&self) -> u8 {
        self.lbp.as_ref().map_or(0, |p| p.armed.bits)
    }

    /// The drive checks what this device writes: every block carries a CRC.
    fn writes_checked(&self) -> bool {
        self.armed_bits() & lbp::LBP_W != 0
    }

    /// The drive sends its CRC with every block this device reads.
    fn reads_checked(&self) -> bool {
        self.armed_bits() & lbp::LBP_R != 0
    }

    /// One block on the wire when writing: the payload, and the CRC when
    /// the drive checks it.
    fn write_wire(&self) -> usize {
        if self.writes_checked() {
            self.block_size + lbp::CRC_LEN
        } else {
            self.block_size
        }
    }

    /// Bytes `write(2)` accepted, in payload bytes: what every count above
    /// the device is in (the early warning's `bytes_at`, the test hook's
    /// figure). Fixed-mode st accepts whole blocks or nothing.
    fn payload_bytes(&self, wire_bytes: u64) -> u64 {
        let wire = self.write_wire().max(1) as u64;
        if wire == self.block_size.max(1) as u64 {
            return wire_bytes;
        }
        // Whole wire blocks scale exactly; a part of one (a short write st
        // should never give in fixed mode) counts its bytes up to the
        // payload's end. Never an assertion: home2's acceptance runs debug
        // builds, and a counter must not stop a tape write.
        let whole = wire_bytes / wire * self.block_size as u64;
        whole + (wire_bytes % wire).min(self.block_size as u64)
    }

    /// st's block size set to `n` if it is not already: only under item
    /// 175's fallback policy, where a write-armed page reads at the payload
    /// size until the confirm arms the read check, does it ever change after
    /// the open.
    fn ensure_st_block(&mut self, n: usize) -> Result<()> {
        if self.st_block != n {
            self.mt_ioctl(MTSETBLK, n as i32)?;
            self.st_block = n;
        }
        Ok(())
    }

    /// Item 175: add LBP_R to a page this open armed for writes, for the
    /// confirm of a session that wrote with LBP. `Ok(true)` when reads are
    /// checked from here on (already, under [`lbp::LbpReadPolicy::AllReads`]);
    /// `Ok(false)` when this open has no page armed, or the drive would not
    /// take it, which the session log says — the confirm then reads as
    /// before #388.
    pub fn arm_read_check(&mut self) -> Result<bool> {
        let Some(page) = self.lbp.as_mut() else {
            return Ok(false);
        };
        if page.armed.bits & lbp::LBP_R != 0 {
            return Ok(true);
        }
        let bits = page.armed.bits | lbp::LBP_R;
        match lbp::rearm(&*page.control, &mut page.armed, bits) {
            Ok(()) => {
                self.ensure_st_block(self.block_size + lbp::CRC_LEN)?;
                self.protection = Protection::Lbp { reads: true };
                crate::progress::log(&format!(
                    "Logical Block Protection: read check armed on {} for the confirm",
                    self.path
                ));
                Ok(true)
            }
            Err(reason) => {
                crate::progress::say(&format!(
                    "Logical Block Protection: the read check could not be armed on {} for the \
                     confirm ({reason}); the confirm reads without it",
                    self.path
                ));
                Ok(false)
            }
        }
    }

    /// The drive's logical object position: READ POSITION through st
    /// (`MTIOCPOS`), blocks and filemarks from the beginning of the tape.
    /// Asked by a write session after a refused write (item 174) to find the
    /// first block the drive did not accept. Moves no tape.
    pub fn logical_position(&self) -> Result<u64> {
        let mut pos = MtPos::default();
        let rc = unsafe { nix::libc::ioctl(self.raw_fd(), MTIOCPOS, &mut pos as *mut MtPos) };
        if rc != 0 {
            let e = io::Error::last_os_error();
            self.note_failure("read position", &e);
            return Err(TapectlError::TapeIo(format!("MTIOCPOS: {e}")));
        }
        u64::try_from(pos.mt_blkno)
            .map_err(|_| TapectlError::TapeIo(format!("MTIOCPOS: position {}", pos.mt_blkno)))
    }

    /// `TAPECTL_TEST_LBP_CORRUPT_BLOCK` (#388, F5; read and announced by
    /// `store::TapeStore::open`, which refuses it on an open that is not
    /// write-armed): each listed block write since this open, counted from 1
    /// with every re-sent block counted too ([`lbp::CorruptBlocks`]), goes
    /// to the drive with one bit of its CRC flipped, so the drive refuses it
    /// as it refuses a block damaged on the way.
    pub fn fake_lbp_corrupt_blocks(&mut self, blocks: &[u64]) {
        self.corrupt = lbp::CorruptBlocks::new(blocks);
    }

    /// The block size reads and writes use (0: variable-block mode).
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    fn raw_fd(&self) -> i32 {
        self.file.as_raw_fd()
    }

    fn mt_ioctl(&self, op: i16, count: i32) -> Result<()> {
        self.mt_ioctl_raw(op, count)
            .map_err(|e| TapectlError::TapeIo(format!("ioctl op={op} count={count}: {e}")))
    }

    /// [`Self::mt_ioctl`] with the kernel's error itself, for the callers
    /// that tell errnos apart (a filemark's ENOSPC and, armed, its EIO). A
    /// filemark, a rewind, a space or an erase that succeeded ends a run of
    /// block writes, so the close owes no filemark.
    fn mt_ioctl_raw(&self, op: i16, count: i32) -> io::Result<()> {
        let mtop = MtOp {
            mt_op: op,
            _pad: 0,
            mt_count: count,
        };
        if let Some(log) = &self.op_log {
            log.lock()
                .unwrap()
                .push(format!("ioctl {}", mt_op_name(op, count)));
        }
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
            return Err(e);
        }
        if matches!(op, MTWEOF | MTWEOFI | MTREW | MTFSF | MTERASE) {
            self.pending_data.set(false);
        }
        Ok(())
    }

    /// Set tape block size.
    pub fn set_block_size(&mut self, bs: usize) -> Result<()> {
        self.mt_ioctl(MTSETBLK, bs as i32)?;
        self.block_size = bs;
        self.st_block = bs;
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

    /// Write a file mark (immediate, no flush). The kernel's error itself:
    /// [`Self::write_stream`] tells a full medium, and an armed refusal,
    /// from the rest by errno.
    pub fn write_filemark_immediate(&self) -> io::Result<()> {
        self.mt_ioctl_raw(MTWEOFI, 1)
    }

    /// Write a file mark (synchronous flush). The kernel's error itself, as
    /// [`Self::write_filemark_immediate`].
    pub fn write_filemark_sync(&self) -> io::Result<()> {
        self.mt_ioctl_raw(MTWEOF, 1)
    }

    /// A failed filemark, classified (issue #408, #420 W3, #388): ENOSPC is
    /// a full medium (never retried); EIO while the page is armed is
    /// [`TapectlError::LbpRefusal`] — the WRITE FILEMARKS carries the
    /// drive's deferred refusal of a block written before it (item 174);
    /// anything else a tape I/O error.
    fn filemark_error(&self, op: i16, e: &io::Error) -> TapectlError {
        let what = format!("ioctl op={op} count=1: {e}");
        if e.raw_os_error() == Some(nix::libc::ENOSPC) {
            TapectlError::MediumFull(TapectlError::TapeIo(what).to_string())
        } else if self.writes_checked() && e.raw_os_error() == Some(nix::libc::EIO) {
            TapectlError::LbpRefusal(lbp::refusal_text(
                &format!("the {} after this file ({what})", mt_op_name(op, 1)),
                e,
            ))
        } else {
            TapectlError::TapeIo(what)
        }
    }

    /// A failed block write, classified: [`write_error`]'s, except EIO while
    /// the page is armed, which is [`TapectlError::LbpRefusal`] (item 174;
    /// st cannot tell a refusal from another EIO, so every one is).
    fn block_write_error(&self, what: &str, e: &io::Error) -> TapectlError {
        if self.writes_checked() && e.raw_os_error() == Some(nix::libc::EIO) {
            TapectlError::LbpRefusal(lbp::refusal_text(what, e))
        } else {
            write_error(what, e)
        }
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
    ///
    /// Every block goes through [`write_block_retrying`] (#420): past the
    /// early warning a refused block is written again, and only the hard end
    /// fails the write. The filemark is one ioctl, never retried
    /// (`volume-format-v2.md` §4.3 W3): refused, it is a full medium.
    pub fn write_stream(&mut self, src: &mut dyn Read, len: u64, sync: bool) -> Result<u64> {
        if self.writes_checked() {
            self.ensure_st_block(self.write_wire())?;
        }
        let mut warning = std::mem::take(&mut self.warning);
        let mut fake = self.fake_warning.take();
        let refusals_before = warning.refusals();
        let blocks = self.write_blocks(src, len, &mut warning, &mut fake);
        self.warning = warning;
        self.summarize_refusals(refusals_before);
        let committed = match blocks {
            Ok(n) => n,
            Err(e) => {
                self.fake_warning = fake;
                return Err(e);
            }
        };

        let marked = if sync {
            self.write_filemark_sync()
        } else {
            self.write_filemark_immediate()
        };
        // A filemark the drive took ends the test hook's alternation, as it
        // ends st's (`store::MediumEnd`).
        if let (Ok(()), Some(end)) = (&marked, fake.as_mut()) {
            end.filemark_landed();
        }
        self.fake_warning = fake;
        // A refused filemark is a full medium too (issue #408), and is not
        // retried (#420 W3); armed, an EIO is a refusal (#388).
        if let Err(e) = marked {
            return Err(self.filemark_error(if sync { MTWEOF } else { MTWEOFI }, &e));
        }
        Ok(committed)
    }

    /// The early warning, once st refused a block write on this device
    /// (#420): sticky for the device's life, which is its store's. Its
    /// `bytes_at` is in payload bytes, the CRC of an armed page not counted.
    pub fn early_warning(&self) -> Option<EarlyWarning> {
        self.warning.warning().map(|w| EarlyWarning {
            bytes_at: self.payload_bytes(w.bytes_at),
            ..w
        })
    }

    /// The test hook `TAPECTL_TEST_EARLY_WARNING_AT` (#420; read and
    /// announced by `store::TapeStore::open`, which says why it is safe):
    /// from `at` bytes accepted since this device opened, its block writes
    /// are answered as st answers them past a drive's early warning
    /// (`store::MediumEnd`): a refused block is answered with ENOSPC before
    /// `write(2)`, so it never reaches the drive, and the device's one retry
    /// sends the same bytes again. Nothing the drive is sent changes.
    pub fn fake_early_warning_at(&mut self, at: u64) {
        self.fake_warning = Some(crate::store::MediumEnd::early_warning_only(at));
    }

    /// [`Self::write_stream`]'s blocks, each through
    /// [`write_block_retrying`]. The device's journal gets the FIRST refusal
    /// of its life — one `MTIOCGET`, while st still shows the refusal — and
    /// the hard end, never a reading per refusal; the rest is counted in
    /// `warning` and summed up by [`Self::summarize_refusals`].
    fn write_blocks(
        &self,
        src: &mut dyn Read,
        len: u64,
        warning: &mut EarlyWarningState,
        fake: &mut Option<crate::store::MediumEnd>,
    ) -> Result<u64> {
        let bs = self.block_size.max(1);
        // #388: the wire block. Armed, the payload's CRC follows it in the
        // same `write(2)`; `committed` and every count above the device stay
        // in payload bytes, and `pad_to_blocks` and the trim do not change.
        let wire = self.write_wire().max(1);
        let protect = self.writes_checked();
        let mut buf = vec![0u8; wire];
        let mut remaining = len;
        let mut committed = 0u64;
        let mut first_refusal = warning.warning().is_none();

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
            if protect {
                lbp::protect(&mut buf);
                if self.corrupt.next_block() {
                    // The test hook: one CRC bit flipped (F5).
                    buf[wire - 1] ^= 0x01;
                }
            }
            self.pending_data.set(true);
            let started = std::time::Instant::now();
            let accepted_before = self.payload_bytes(warning.accepted());
            let mut landed = 0u64;
            let written = write_block_retrying(
                |block| {
                    // The test hook answers first: a refusal never reaches
                    // the drive (`Self::fake_early_warning_at`).
                    let faked = fake.as_mut().is_some_and(|end| end.refuses_block());
                    let attempt = if faked {
                        Err(io::Error::from_raw_os_error(nix::libc::ENOSPC))
                    } else {
                        (&self.file).write(block)
                    };
                    if let (Ok(n), Some(end)) = (&attempt, fake.as_mut()) {
                        let n = self.payload_bytes(*n as u64);
                        end.block_landed(accepted_before + landed, n);
                        landed += n;
                    }
                    if let Err(e) = &attempt {
                        if first_refusal && e.raw_os_error() == Some(nix::libc::ENOSPC) {
                            first_refusal = false;
                            let (refuser, by) = if faked {
                                (
                                    "the test hook TAPECTL_TEST_EARLY_WARNING_AT, not st,",
                                    ", by the test hook TAPECTL_TEST_EARLY_WARNING_AT",
                                )
                            } else {
                                ("st", "")
                            };
                            self.note_failure(
                                &format!("write (early warning: first refused block{by})"),
                                e,
                            );
                            let line = format!(
                                "early warning: {refuser} refused a block write (ENOSPC) after \
                                 {accepted_before} bytes written since the drive opened; \
                                 retrying it"
                            );
                            tracing::warn!("{line}");
                            crate::progress::log(&line);
                        }
                    }
                    attempt
                },
                &buf[..wire],
                warning,
            );
            if let Err(e) = written {
                let what = if e.raw_os_error() == Some(nix::libc::ENOSPC) {
                    format!(
                        "write (refused more than {MAX_REFUSALS_IN_A_ROW} times in a row: \
                         the medium's hard end)"
                    )
                } else {
                    "write".to_string()
                };
                self.note_failure(&what, &e);
                return Err(self.block_write_error(&what, &e));
            }
            crate::progress::note_if_slow("one tape block write", started.elapsed());
            committed += bs as u64;
            remaining -= want as u64;
        }
        Ok(committed)
    }

    /// The session log's summary of one file's refusals past the early
    /// warning, when it had any (#420): how many, and the device's total.
    fn summarize_refusals(&self, before: u64) {
        let Some(w) = self.early_warning() else {
            return;
        };
        let here = w.refusals - before;
        if here == 0 {
            return;
        }
        let line = format!(
            "early warning: {here} refused block write(s) retried in this file; {} since \
             the first, which came after {} bytes written since the drive opened",
            w.refusals, w.bytes_at
        );
        tracing::info!("{line}");
        crate::progress::log(&line);
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
        let (read_size, checked) = self.read_shape()?;
        let mut buf = vec![0u8; read_size];
        let mut block = 0u64;
        loop {
            let started = std::time::Instant::now();
            let got = self.file.read(&mut buf);
            crate::progress::note_if_slow("one tape block read", started.elapsed());
            match got {
                Ok(0) => return Ok((total, ReadEnd::Filemark)),
                Ok(n) => {
                    let payload = self.read_payload(&buf, n, checked, block)?;
                    sink.write_all(payload)
                        .map_err(|e| TapectlError::TapeIo(format!("sink write: {e}")))?;
                    total += payload.len() as u64;
                    block += 1;
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
        let (read_size, checked) = self.read_shape()?;
        let mut buf = vec![0u8; read_size];
        let mut block = 0u64;
        while total < max_bytes {
            match self.file.read(&mut buf) {
                // File mark: the file is shorter than asked.
                Ok(0) => return Ok((total, ReadEnd::Filemark)),
                Ok(n) => {
                    let payload = self.read_payload(&buf, n, checked, block)?;
                    let take = (payload.len() as u64).min(max_bytes - total) as usize;
                    sink.write_all(&payload[..take])
                        .map_err(|e| TapectlError::TapeIo(format!("sink write: {e}")))?;
                    total += take as u64;
                    block += 1;
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

    /// How a read takes its blocks: the size of one `read(2)` and whether
    /// each carries the drive's CRC (LBP_R armed), st's block size set to
    /// match (§2.4).
    fn read_shape(&mut self) -> Result<(usize, bool)> {
        if self.block_size == 0 {
            return Ok((1024 * 1024, false));
        }
        let checked = self.reads_checked();
        let size = if checked {
            self.block_size + lbp::CRC_LEN
        } else {
            self.block_size
        };
        if self.lbp.is_some() {
            self.ensure_st_block(size)?;
        }
        Ok((size, checked))
    }

    /// The payload of one block read: `buf[..n]` as it came, or, with LBP_R
    /// armed, the block checked against the CRC the drive sent and stripped
    /// of it. A short block or a mismatch is a read error — never a verdict
    /// about the medium (`MismatchKind::ContentUnreadable`): the bytes
    /// changed between the drive and tapectl.
    fn read_payload<'b>(
        &self,
        buf: &'b [u8],
        n: usize,
        checked: bool,
        block: u64,
    ) -> Result<&'b [u8]> {
        if !checked {
            return Ok(&buf[..n]);
        }
        if n != buf.len() {
            return Err(TapectlError::TapeIo(format!(
                "Logical Block Protection read: st returned a {n}-byte block, expected {}",
                buf.len()
            )));
        }
        lbp::check(buf).map_err(|m| {
            TapectlError::TapeIo(format!(
                "Logical Block Protection check failed on block {block} of this file: the drive \
                 sent CRC {:08X}, the block tapectl received computes to {:08X}; the bytes \
                 changed between the drive and tapectl, not on the medium",
                m.got, m.want
            ))
        })?;
        Ok(&buf[..n - lbp::CRC_LEN])
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
        assert!(matches!(
            write_error("write", &err),
            TapectlError::TapeIo(_)
        ));
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

    /// A device over a plain file (no tape: its ioctls fail, its block
    /// writes land in the file), for the test hook's tests.
    fn device_over(file: File, block_size: usize) -> TapeDevice {
        TapeDevice {
            file,
            block_size,
            path: "test-hook-file".to_string(),
            recovered: Default::default(),
            warning: EarlyWarningState::default(),
            fake_warning: None,
            lbp: None,
            protection: Protection::NotApplicable,
            st_block: block_size,
            pending_data: Cell::new(false),
            corrupt: lbp::CorruptBlocks::default(),
            op_log: None,
        }
    }

    /// [`device_over`] with the page armed (`bits`) over a recorded page
    /// control, as a successful open leaves it: st at the wire size, and one
    /// log for the device's ioctls and the page's commands.
    fn device_over_armed(
        file: File,
        block_size: usize,
        bits: u8,
    ) -> (
        TapeDevice,
        crate::tape::lbp::fake::FakePage,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        use crate::tape::lbp::fake::{FakePage, HP_PAGE};
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut page = FakePage::new(&HP_PAGE);
        let found = lbp::ProtectionPage::from_page_bytes(&HP_PAGE).unwrap();
        page.log = Some(log.clone());
        let armed_page = found.with_protection(lbp::METHOD_RS, lbp::RS_INFO_LEN, bits);
        lbp::PageControl::select(&page, &armed_page).unwrap();
        log.lock().unwrap().clear();
        let mut dev = device_over(file, block_size);
        dev.lbp = Some(ArmedPage {
            control: Box::new(page.clone()),
            armed: lbp::Armed {
                found,
                leftover: false,
                bits,
            },
        });
        dev.protection = Protection::Lbp {
            reads: bits & lbp::LBP_R != 0,
        };
        dev.st_block = block_size + lbp::CRC_LEN;
        dev.op_log = Some(log.clone());
        (dev, page, log)
    }

    /// The protected blocks a write of `payload` puts on the wire: each
    /// `bs` payload bytes (the last zero-padded), then their CRC big-endian.
    fn protected(payload: &[u8], bs: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for chunk in payload.chunks(bs) {
            let mut block = chunk.to_vec();
            block.resize(bs, 0);
            let crc = lbp::rs_crc(&block, 0);
            out.extend_from_slice(&block);
            out.extend_from_slice(&crc.to_be_bytes());
        }
        out
    }

    /// #388 §2.3: armed, every block goes to the drive as its padded
    /// payload and its CRC, big-endian, in one `write(2)`; the write still
    /// commits payload bytes (`pad_to_blocks`, the trim, unchanged).
    #[test]
    fn armed_writes_put_a_big_endian_crc_after_every_padded_block() {
        let bs = 4096usize;
        let source: Vec<u8> = (0..(2 * bs + bs / 2)).map(|i| (i % 251) as u8).collect();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let (dev, _page, _log) =
            device_over_armed(tmp.reopen().unwrap(), bs, lbp::LBP_W | lbp::LBP_R);
        let mut warning = EarlyWarningState::default();
        let committed = dev
            .write_blocks(
                &mut io::Cursor::new(&source),
                source.len() as u64,
                &mut warning,
                &mut None,
            )
            .unwrap();
        assert_eq!(committed, 3 * bs as u64, "payload bytes, padded");
        let on_wire = std::fs::read(tmp.path()).unwrap();
        assert_eq!(on_wire.len(), 3 * (bs + 4));
        assert_eq!(on_wire, protected(&source, bs));
        for record in on_wire.chunks(bs + 4) {
            assert!(lbp::check(record).is_ok());
        }
        assert!(
            dev.pending_data.get(),
            "the last st operation was a block write"
        );
        assert_eq!(
            warning.accepted(),
            3 * (bs as u64 + 4),
            "st counted wire bytes"
        );
    }

    /// §2.4: armed with LBP_R, each wire block is checked against the CRC
    /// the drive sent and delivered as payload; a flipped CRC bit is a read
    /// error naming the block, and so is a short record.
    #[test]
    fn armed_reads_strip_and_check() {
        let bs = 4096usize;
        let payload: Vec<u8> = (0..3 * bs).map(|i| (i % 253) as u8).collect();
        let wire = protected(&payload, bs);
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &wire).unwrap();
        let (mut dev, _page, log) =
            device_over_armed(tmp.reopen().unwrap(), bs, lbp::LBP_W | lbp::LBP_R);
        let mut out = Vec::new();
        let (n, end) = dev.read_file_streaming(&mut out).unwrap();
        assert_eq!((n, end), (3 * bs as u64, ReadEnd::Filemark));
        assert_eq!(out, payload);
        assert!(
            log.lock().unwrap().is_empty(),
            "st already at the wire size: no block size set"
        );

        // A bounded head read strips too.
        let (mut dev, _page, _log) =
            device_over_armed(tmp.reopen().unwrap(), bs, lbp::LBP_W | lbp::LBP_R);
        let mut head = Vec::new();
        let (n, end) = dev.read_file_head(10, &mut head).unwrap();
        assert_eq!(
            (n, end, head.as_slice()),
            (10, ReadEnd::Stopped, &payload[..10])
        );

        let mut bad = wire.clone();
        bad[2 * (bs + 4) - 1] ^= 0x01;
        std::fs::write(tmp.path(), &bad).unwrap();
        let (mut dev, _page, _log) =
            device_over_armed(tmp.reopen().unwrap(), bs, lbp::LBP_W | lbp::LBP_R);
        let err = dev
            .read_file_streaming(&mut Vec::new())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("check failed on block 1 of this file")
                && err.contains("not on the medium"),
            "{err}"
        );

        std::fs::write(tmp.path(), &wire[..wire.len() - 10]).unwrap();
        let (mut dev, _page, _log) =
            device_over_armed(tmp.reopen().unwrap(), bs, lbp::LBP_W | lbp::LBP_R);
        let err = dev
            .read_file_streaming(&mut Vec::new())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("st returned a 4090-byte block, expected 4100"),
            "{err}"
        );

        // Unarmed, the same bytes come back as they are, CRC and all.
        let tmp2 = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp2.path(), &payload).unwrap();
        let mut plain = device_over(tmp2.reopen().unwrap(), bs);
        let mut out = Vec::new();
        plain.read_file_streaming(&mut out).unwrap();
        assert_eq!(out, payload);
    }

    /// §2.3: the early warning, the test hook's figure and the log lines
    /// count payload bytes, though st accepts wire bytes.
    #[test]
    fn armed_early_warning_counts_payload_bytes() {
        let bs = 8usize;
        let source: Vec<u8> = (0..6 * bs as u8).collect();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let (mut dev, _page, _log) = device_over_armed(tmp.reopen().unwrap(), bs, lbp::LBP_W);
        dev.fake_early_warning_at(20);
        let mut warning = EarlyWarningState::default();
        let mut fake = dev.fake_warning.take();
        dev.write_blocks(
            &mut io::Cursor::new(&source),
            source.len() as u64,
            &mut warning,
            &mut fake,
        )
        .unwrap();
        dev.warning = warning;
        assert_eq!(
            dev.early_warning(),
            Some(EarlyWarning {
                bytes_at: 24,
                refusals: 3
            }),
            "the unarmed figures of the_test_hook_refuses_like_st_and_every_block_lands_once"
        );
        assert_eq!(
            dev.warning.warning().unwrap().bytes_at,
            36,
            "st's own: 3 x 12"
        );
        assert_eq!(std::fs::read(tmp.path()).unwrap(), protected(&source, bs));
    }

    /// F5: the hook flips one CRC bit of the nth block write, and only that
    /// one; the payload is untouched.
    #[test]
    fn the_corrupt_block_hook_flips_one_crc_bit_of_one_block() {
        let bs = 64usize;
        let source = vec![7u8; 3 * bs];
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let (mut dev, _page, _log) = device_over_armed(tmp.reopen().unwrap(), bs, lbp::LBP_W);
        dev.fake_lbp_corrupt_blocks(&[2]);
        dev.write_blocks(
            &mut io::Cursor::new(&source),
            source.len() as u64,
            &mut EarlyWarningState::default(),
            &mut None,
        )
        .unwrap();
        let on_wire = std::fs::read(tmp.path()).unwrap();
        let mut want = protected(&source, bs);
        want[2 * (bs + 4) - 1] ^= 0x01;
        assert_eq!(on_wire, want);
        let checks: Vec<bool> = on_wire
            .chunks(bs + 4)
            .map(|r| lbp::check(r).is_ok())
            .collect();
        assert_eq!(checks, [true, false, true]);
    }

    /// The coordinator's 388b addition 2: the hook takes a list, and counts
    /// every block the open sends, re-sent ones too. Blocks 2 and 5 of an
    /// open that writes three blocks, then sends the last two again (as
    /// item 174's retry sends its file again on the same open): the second
    /// number lands on a re-sent block.
    #[test]
    fn the_corrupt_block_hook_counts_re_sent_blocks() {
        let bs = 64usize;
        let source: Vec<u8> = (0..3 * bs).map(|i| (i / bs) as u8 + 1).collect();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let (mut dev, _page, _log) = device_over_armed(tmp.reopen().unwrap(), bs, lbp::LBP_W);
        dev.fake_lbp_corrupt_blocks(&[2, 5]);
        for part in [&source[..], &source[bs..]] {
            dev.write_blocks(
                &mut io::Cursor::new(part),
                part.len() as u64,
                &mut EarlyWarningState::default(),
                &mut None,
            )
            .unwrap();
        }
        let on_wire = std::fs::read(tmp.path()).unwrap();
        let checks: Vec<bool> = on_wire
            .chunks(bs + 4)
            .map(|r| lbp::check(r).is_ok())
            .collect();
        assert_eq!(checks, [true, false, true, true, false]);
        let payloads: Vec<u8> = on_wire.chunks(bs + 4).map(|r| r[0]).collect();
        assert_eq!(payloads, [1, 2, 3, 2, 3], "the data itself is untouched");
    }

    /// §4.3: the close of an armed device after a block write: the
    /// synchronous filemark first, then the page put back and read back,
    /// then the format's block size. Without a pending write there is no
    /// filemark; unarmed, the close touches neither.
    #[test]
    fn drop_clears_the_page_after_the_closing_filemark() {
        let bs = 4096usize;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let (dev, page, log) =
            device_over_armed(tmp.reopen().unwrap(), bs, lbp::LBP_W | lbp::LBP_R);
        dev.write_blocks(
            &mut io::Cursor::new(vec![1u8; 10]),
            10,
            &mut EarlyWarningState::default(),
            &mut None,
        )
        .unwrap();
        drop(dev);
        assert_eq!(
            *log.lock().unwrap(),
            [
                "ioctl filemark (synchronous)",
                "select 4a,f0,00,04,00,00,00,00",
                "sense",
                "ioctl set block size 4096"
            ]
        );
        assert_eq!(page.current(), crate::tape::lbp::fake::HP_PAGE);

        let (dev, page, log) = device_over_armed(tmp.reopen().unwrap(), bs, lbp::LBP_W);
        drop(dev);
        assert_eq!(
            *log.lock().unwrap(),
            [
                "select 4a,f0,00,04,00,00,00,00",
                "sense",
                "ioctl set block size 4096"
            ],
            "no write, no filemark"
        );
        assert_eq!(page.current(), crate::tape::lbp::fake::HP_PAGE);

        let mut plain = device_over(tmp.reopen().unwrap(), bs);
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        plain.op_log = Some(log.clone());
        plain.pending_data.set(true);
        drop(plain);
        assert!(
            log.lock().unwrap().is_empty(),
            "unarmed: the close as before"
        );
    }

    /// Items 166/174: armed, a write's EIO — a block's or a filemark's — is
    /// an LBP refusal; ENOSPC stays a full medium; unarmed, nothing changes.
    #[test]
    fn an_armed_write_eio_is_a_refusal_and_nothing_else_changes() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let (armed, _page, _log) = device_over_armed(tmp.reopen().unwrap(), 4096, lbp::LBP_W);
        let plain = device_over(tmp.reopen().unwrap(), 4096);
        let eio = io::Error::from_raw_os_error(nix::libc::EIO);
        let full = io::Error::from_raw_os_error(nix::libc::ENOSPC);

        let e = armed.block_write_error("write", &eio);
        assert!(
            matches!(&e, TapectlError::LbpRefusal(m) if m.starts_with("write: ")),
            "{e}"
        );
        assert!(
            e.to_string().contains("logical block guard check failed"),
            "{e}"
        );
        let e = armed.filemark_error(MTWEOFI, &eio);
        assert!(
            matches!(&e, TapectlError::LbpRefusal(m) if m.starts_with("the filemark after this file")),
            "{e}"
        );
        assert!(matches!(
            armed.block_write_error("write", &full),
            TapectlError::MediumFull(_)
        ));
        let e = armed.filemark_error(MTWEOF, &full);
        assert_eq!(
            e.to_string(),
            "tape full: tape I/O error: ioctl op=5 count=1: No space left on device (os error 28)"
        );
        assert!(matches!(
            plain.block_write_error("write", &eio),
            TapectlError::TapeIo(_)
        ));
        assert!(matches!(
            plain.filemark_error(MTWEOFI, &eio),
            TapectlError::TapeIo(_)
        ));
    }

    /// Item 175's fallback policy on a write-armed device: reads before the
    /// confirm are unprotected at the payload size (st set back to it);
    /// arming the read check for the confirm sets the page and the wire size.
    #[test]
    fn under_the_fallback_policy_reads_follow_the_read_check() {
        let bs = 4096usize;
        let payload = vec![9u8; bs];
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &payload).unwrap();
        let (mut dev, page, log) = device_over_armed(tmp.reopen().unwrap(), bs, lbp::LBP_W);
        let mut out = Vec::new();
        // On a plain file the MTSETBLK fails: the read reports it.
        let err = dev.read_file_streaming(&mut out).unwrap_err().to_string();
        assert!(err.contains("ioctl op=20 count=4096"), "{err}");
        assert_eq!(*log.lock().unwrap(), ["ioctl set block size 4096"]);
        log.lock().unwrap().clear();

        // The set failed, so st is still at the wire size the open left.
        std::fs::write(tmp.path(), protected(&payload, bs)).unwrap();
        assert!(dev.arm_read_check().unwrap());
        assert_eq!(page.current()[6], 0xc0, "LBP_R added");
        assert_eq!(dev.armed_bits(), lbp::LBP_W | lbp::LBP_R);
        assert_eq!(dev.protection(), Protection::Lbp { reads: true });
        let mut out = Vec::new();
        dev.read_file_streaming(&mut out).unwrap();
        assert_eq!(out, payload, "checked and stripped");
        assert!(dev.arm_read_check().unwrap(), "already armed: nothing sent");
        assert_eq!(page.selects(), 2, "the open's arm and the one rearm");

        // An unarmed device has no read check to arm.
        let mut plain = device_over(tmp.reopen().unwrap(), bs);
        assert!(!plain.arm_read_check().unwrap());
    }

    /// #388: the page is armed in one place (the open's
    /// `arm_protection`), re-armed in one (`arm_read_check`, item 175) and
    /// cleared in one (`Drop`); `sg_wr_mode` is lbp.rs's alone.
    #[test]
    fn the_page_is_set_in_one_place() {
        let src = include_str!("ioctl.rs");
        let body = src.split("#[cfg(test)]\nmod density_tests").next().unwrap();
        for (call, inside, ends) in [
            ("lbp::arm(", "    fn arm_protection(", "\n    }\n"),
            ("lbp::rearm(", "    pub fn arm_read_check(", "\n    }\n"),
            ("lbp::disarm(", "impl Drop for TapeDevice", "\n}\n"),
            (
                "lbp::clear_leftover(",
                "    fn arm_protection(",
                "\n    }\n",
            ),
        ] {
            assert_eq!(body.matches(call).count(), 1, "{call}");
            let start = body.find(inside).expect(inside);
            let end = start + body[start..].find(ends).unwrap();
            assert!(body[start..end].contains(call), "{call} inside {inside}");
        }
        assert!(!body.contains("sg_wr_mode\""), "no sg tool named here");
        assert!(
            !body.contains("Command::new"),
            "the device starts no program"
        );
    }

    /// 388c's finding (#388): after a read that failed or ran off the data,
    /// st answers the next read without sending it, which would let the
    /// probe arm a path that cannot carry the wire block. So an open that
    /// arms rewinds before it touches the page, and the probe (the last step
    /// of `lbp::arm`) reads at BOT. Over a plain file the rewind is issued
    /// first and fails, and nothing else is: no page command, no block size,
    /// the page as found, nothing armed, and the open fails as the store's
    /// own rewind would have. Positive control: an open that arms nothing
    /// (the fallback policy's read) sends no rewind and still reads the page
    /// for a leftover.
    #[test]
    fn an_open_that_arms_rewinds_before_the_page_and_the_probe() {
        use crate::tape::lbp::fake::{FakePage, HP_PAGE};
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut page = FakePage::new(&HP_PAGE);
        page.log = Some(log.clone());

        let mut dev = device_over(tmp.reopen().unwrap(), 4096);
        dev.op_log = Some(log.clone());
        let err = dev
            .arm_protection(
                true,
                lbp::LbpReadPolicy::AllReads,
                Some(Box::new(page.clone())),
            )
            .expect_err("a plain file does not rewind");
        assert!(matches!(err, TapectlError::TapeIo(_)), "{err:?}");
        assert_eq!(
            *log.lock().unwrap(),
            ["ioctl rewind"],
            "the rewind first, and nothing after it failed"
        );
        assert_eq!(page.current(), HP_PAGE, "the page untouched");
        assert!(dev.lbp.is_none());
        assert_eq!(dev.protection(), Protection::NotApplicable);
        drop(dev);

        log.lock().unwrap().clear();
        let mut dev = device_over(tmp.reopen().unwrap(), 4096);
        dev.op_log = Some(log.clone());
        dev.arm_protection(
            false,
            lbp::LbpReadPolicy::OnlyConfirmOfLbpWrite,
            Some(Box::new(page.clone())),
        )
        .unwrap();
        assert_eq!(*log.lock().unwrap(), ["sense"], "no probe, so no rewind");
        drop(dev);

        // In the source, the one rewind comes before `lbp::arm(`, whose
        // last step is the probe.
        let src = include_str!("ioctl.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        let start = body.find("    fn arm_protection(").unwrap();
        let end = start + body[start..].find("\n    }\n").unwrap();
        let f = &body[start..end];
        assert_eq!(f.matches("self.rewind()").count(), 1, "{f}");
        assert!(
            f.find("self.rewind()?").unwrap() < f.find("lbp::arm(").unwrap(),
            "the rewind precedes the arming and its probe"
        );
    }

    /// #388: each note an open or a close gives is one line on stderr and one
    /// in the session log, counted over a run as `main` sets it up
    /// (`progress::heard`): a leftover the open clears, a write open's
    /// item-165 warning, a read open's note (the log alone, F4), and a close
    /// that cannot clear the page. They were two and two, the second of each
    /// through a `tracing` event main prints and tees as well.
    #[test]
    fn each_lbp_note_is_one_print_and_one_log_line() {
        use crate::progress::heard;
        use crate::tape::lbp::fake::{FakePage, HP_PAGE};
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let armed_page = lbp::ProtectionPage::from_page_bytes(&HP_PAGE)
            .unwrap()
            .with_protection(lbp::METHOD_RS, lbp::RS_INFO_LEN, lbp::LBP_W | lbp::LBP_R);

        // A leftover, cleared by an open that arms nothing.
        let leftover = FakePage::new(armed_page.bytes());
        let mut dev = device_over(tmp.reopen().unwrap(), 4096);
        dev.path = "/dev/nst-heard-leftover".into();
        let (armed, heard) = heard::during(|| {
            dev.arm_protection(
                false,
                lbp::LbpReadPolicy::OnlyConfirmOfLbpWrite,
                Some(Box::new(leftover.clone())),
            )
        });
        armed.unwrap();
        assert_eq!(leftover.current(), HP_PAGE, "positive control: cleared");
        assert_eq!(
            heard.count("had its Logical Block Protection page armed"),
            (1, 1),
            "{:?}",
            heard.stderr
        );

        // No sg node: the write open's warning, then a read open's note.
        let mut dev = device_over(tmp.reopen().unwrap(), 4096);
        dev.path = "/dev/nst-heard-no-sg".into();
        let ((), heard) = heard::during(|| {
            dev.arm_protection(true, lbp::LbpReadPolicy::AllReads, None)
                .unwrap();
            dev.arm_protection(false, lbp::LbpReadPolicy::AllReads, None)
                .unwrap();
        });
        assert_eq!(
            heard.count("warning: Logical Block Protection is not available on"),
            (1, 1),
            "{:?}",
            heard.stderr
        );
        assert_eq!(
            heard.count("is not available on /dev/nst-heard-no-sg for this read"),
            (0, 1),
            "{:?}",
            heard.log
        );

        // The close of an armed device whose page will not clear.
        let (dev, page, _log) = device_over_armed(tmp.reopen().unwrap(), 4096, lbp::LBP_W);
        page.fail_select(lbp::PageError::Other("sg_wr_mode exited 99".into()));
        let ((), heard) = heard::during(|| drop(dev));
        assert_eq!(
            heard.count("could not clear the Logical Block Protection page"),
            (1, 1),
            "{:?}",
            heard.stderr
        );
    }

    /// `TAPECTL_TEST_EARLY_WARNING_AT` (#420) on the device's real write
    /// path: from the bytes set, a block is refused before `write(2)` after
    /// each one accepted (st past an early warning, `store::MediumEnd`), the
    /// device's one retry sends it again, and the medium receives every
    /// block exactly once, in order: the bytes are what a write without the
    /// hook would have put there. The warning is recorded at the bytes
    /// accepted before the first refusal, as a real one is.
    #[test]
    fn the_test_hook_refuses_like_st_and_every_block_lands_once() {
        let bs = 8usize;
        let source: Vec<u8> = (0..6 * bs as u8).collect();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut dev = device_over(tmp.reopen().unwrap(), bs);
        // Reached inside the third block: it lands and arms the refusal.
        dev.fake_early_warning_at(20);
        let mut warning = EarlyWarningState::default();
        let mut fake = dev.fake_warning.take();
        let committed = dev
            .write_blocks(
                &mut io::Cursor::new(&source),
                source.len() as u64,
                &mut warning,
                &mut fake,
            )
            .unwrap();
        assert_eq!(committed, source.len() as u64);
        assert_eq!(
            std::fs::read(tmp.path()).unwrap(),
            source,
            "every block once, in order: a refused block never reached the file"
        );
        assert_eq!(
            warning.warning(),
            Some(EarlyWarning {
                bytes_at: 24,
                refusals: 3
            }),
            "blocks 4, 5 and 6 refused once each, after the 24 bytes of blocks 1-3"
        );

        // Control: the same write with no hook set refuses nothing.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let dev = device_over(tmp.reopen().unwrap(), bs);
        let mut warning = EarlyWarningState::default();
        dev.write_blocks(
            &mut io::Cursor::new(&source),
            source.len() as u64,
            &mut warning,
            &mut None,
        )
        .unwrap();
        assert_eq!(std::fs::read(tmp.path()).unwrap(), source);
        assert_eq!(warning.warning(), None);
    }

    /// The hook's alternation is `store::MediumEnd`'s, block by block: below
    /// the figure nothing is refused; a block reaching it lands and arms one
    /// refusal; a filemark disarms, so the next file's first block lands (and
    /// arms again, being past the figure).
    #[test]
    fn the_test_hooks_model_is_the_doubles_model() {
        let mut end = crate::store::MediumEnd::early_warning_only(100);
        assert!(!end.refuses_block());
        end.block_landed(0, 50);
        assert!(!end.refuses_block(), "below the figure");
        end.block_landed(50, 50);
        assert!(end.refuses_block(), "the block that reached it armed one");
        assert!(!end.refuses_block(), "one refusal, then the retry lands");
        end.block_landed(100, 50);
        end.filemark_landed();
        assert!(!end.refuses_block(), "a filemark disarms");
        end.block_landed(150, 50);
        assert!(
            end.refuses_block(),
            "past the figure, every landed block arms"
        );
    }

    /// The device's write path, by its source (no tape here): every block
    /// goes through [`write_block_retrying`], nothing writes the descriptor
    /// past it, and the filemark is one ioctl with no retry around it (W3).
    /// The positive control is the scan finding the one call each must.
    #[test]
    fn the_device_retries_its_blocks_and_never_a_filemark() {
        let src = include_str!("ioctl.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        let start = body.find("    pub fn write_stream(").unwrap();
        let end = start + body[start..].find("    /// Stream-read one").unwrap();
        let writer = &body[start..end];
        assert_eq!(writer.matches("write_block_retrying(").count(), 1);
        assert_eq!(writer.matches("(&self.file).write(block)").count(), 1);
        assert!(
            !writer.contains("write_all("),
            "a block bypassing the retry"
        );
        for filemark in [
            "self.write_filemark_sync()",
            "self.write_filemark_immediate()",
        ] {
            assert_eq!(writer.matches(filemark).count(), 1, "{filemark}");
        }
        let loops: Vec<&str> = writer
            .lines()
            .map(str::trim_start)
            .filter(|l| l.starts_with("while ") || l.starts_with("loop ") || l.starts_with("for "))
            .collect();
        assert_eq!(
            loops,
            vec![
                "while remaining > 0 {",
                "while got < want {",
                "for b in &mut buf[want..] {"
            ],
            "the only loops are the block loop, its source read and its padding"
        );
        assert_eq!(
            body.matches("self.file.write").count() + body.matches("(&self.file).write").count(),
            1,
            "no other tape write in the device"
        );
    }
}
