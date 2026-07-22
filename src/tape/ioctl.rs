use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;

use crate::error::{Result, TapectlError};

// Linux tape ioctl constants from <linux/mtio.h>
const MTIOCTOP: u64 = 0x40086d01;
const MTIOCGET: u64 = 0x80306d02;

// mtop operation codes
const MTREW: i16 = 6;
const MTWEOF: i16 = 5;
const MTWEOFI: i16 = 35;
const MTSETBLK: i16 = 20;
const MTFSF: i16 = 1;
const MTEOM: i16 = 12;
const MTCOMP: i16 = 32; // MTCOMPRESSION

#[repr(C)]
struct MtOp {
    mt_op: i16,
    _pad: i16,
    mt_count: i32,
}

#[repr(C)]
#[derive(Debug, Default)]
struct MtGet {
    mt_type: i64,
    mt_resid: i64,
    mt_dsreg: i64,
    mt_gstat: i64,
    mt_erreg: i64,
    mt_fileno: i32,
    mt_blkno: i32,
}

/// Tape position info.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct TapePosition {
    pub file_number: i32,
    pub block_number: i32,
}

/// A wrapper around a tape device file descriptor.
pub struct TapeDevice {
    file: File,
    block_size: usize,
}

impl TapeDevice {
    /// Open a tape device for read+write with the given block size.
    pub fn open(device_path: &str, block_size: usize) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(device_path)
            .map_err(|e| TapectlError::TapeIo(format!("open {device_path}: {e}")))?;

        let mut dev = Self { file, block_size };
        dev.set_block_size(block_size)?;
        Ok(dev)
    }

    /// Open read-only.
    pub fn open_read(device_path: &str, block_size: usize) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .open(device_path)
            .map_err(|e| TapectlError::TapeIo(format!("open {device_path}: {e}")))?;

        let mut dev = Self { file, block_size };
        dev.set_block_size(block_size)?;
        Ok(dev)
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
        let rc = unsafe { nix::libc::ioctl(self.raw_fd(), MTIOCTOP, &mtop as *const MtOp) };
        if rc != 0 {
            return Err(TapectlError::TapeIo(format!(
                "ioctl op={op} count={count}: {}",
                io::Error::last_os_error()
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

    /// Seek to end of media (after last file mark).
    pub fn seek_eom(&self) -> Result<()> {
        self.mt_ioctl(MTEOM, 0)
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
        })
    }

    /// Write data to tape, padding the last block to block_size if needed.
    /// Returns the number of bytes written (including padding).
    pub fn write_data(&mut self, data: &[u8]) -> Result<usize> {
        if self.block_size == 0 {
            // Variable block mode — write in 512KB chunks
            let chunk = 512 * 1024;
            let mut offset = 0;
            while offset < data.len() {
                let end = (offset + chunk).min(data.len());
                self.file
                    .write_all(&data[offset..end])
                    .map_err(|e| TapectlError::TapeIo(format!("write: {e}")))?;
                offset = end;
            }
            Ok(data.len())
        } else {
            // Fixed block mode — pad last block
            let bs = self.block_size;
            let padded_len = data.len().div_ceil(bs) * bs;
            let mut buf = data.to_vec();
            buf.resize(padded_len, 0);
            self.file
                .write_all(&buf)
                .map_err(|e| TapectlError::TapeIo(format!("write: {e}")))?;
            Ok(padded_len)
        }
    }

    /// Read one "file" from tape (all data until the next file mark).
    /// In fixed block mode, reads blocks until a file mark is hit (read returns 0).
    pub fn read_file(&mut self) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        let read_size = if self.block_size > 0 {
            self.block_size
        } else {
            1024 * 1024
        };
        let mut buf = vec![0u8; read_size];
        loop {
            match self.file.read(&mut buf) {
                Ok(0) => break, // file mark
                Ok(n) => data.extend_from_slice(&buf[..n]),
                Err(e) if e.raw_os_error() == Some(28) => break, // ENOSPC
                Err(e) => return Err(TapectlError::TapeIo(format!("read: {e}"))),
            }
        }
        Ok(data)
    }

    /// Write data + file mark (immediate).
    pub fn write_file_with_mark(&mut self, data: &[u8]) -> Result<usize> {
        let written = self.write_data(data)?;
        self.write_filemark_immediate()?;
        Ok(written)
    }

    /// Write data + synchronous file mark (for final files).
    pub fn write_file_with_sync_mark(&mut self, data: &[u8]) -> Result<usize> {
        let written = self.write_data(data)?;
        self.write_filemark_sync()?;
        Ok(written)
    }

    /// Stream-write `len` bytes from `src` in `block_size` chunks, zero-padding
    /// the final partial block to the block boundary, followed by a file mark
    /// (synchronous if `sync`). Unlike `write_data`/`write_file_with_mark`
    /// (whole-buffer, kept intact for the v1 read/write paths), peak memory
    /// here is one block, never `len` (the H9 streaming requirement,
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
                let n = src
                    .read(&mut buf[got..want])
                    .map_err(|e| TapectlError::TapeIo(format!("read source: {e}")))?;
                if n == 0 {
                    return Err(TapectlError::TapeIo(format!(
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
            self.file
                .write_all(&buf[..bs])
                .map_err(|e| TapectlError::TapeIo(format!("write: {e}")))?;
            committed += bs as u64;
            remaining -= want as u64;
        }

        if sync {
            self.write_filemark_sync()?;
        } else {
            self.write_filemark_immediate()?;
        }
        Ok(committed)
    }

    /// Stream-read one "file" from tape (all data until the next file mark),
    /// writing each block straight to `sink` as it arrives instead of
    /// accumulating in memory (unlike `read_file`, kept intact for the v1
    /// paths). Returns the total bytes read — the on-tape (padded) length;
    /// trimming to the true size is the caller's job, since only the front
    /// index knows it. Same block-mode / ENOSPC-as-filemark reading
    /// convention as `read_file`.
    pub fn read_file_streaming(&mut self, sink: &mut dyn Write) -> Result<u64> {
        let mut total = 0u64;
        let read_size = if self.block_size > 0 {
            self.block_size
        } else {
            1024 * 1024
        };
        let mut buf = vec![0u8; read_size];
        loop {
            match self.file.read(&mut buf) {
                Ok(0) => break, // file mark
                Ok(n) => {
                    sink.write_all(&buf[..n])
                        .map_err(|e| TapectlError::TapeIo(format!("sink write: {e}")))?;
                    total += n as u64;
                }
                Err(e) if e.raw_os_error() == Some(28) => break, // ENOSPC
                Err(e) => return Err(TapectlError::TapeIo(format!("read: {e}"))),
            }
        }
        Ok(total)
    }
}
