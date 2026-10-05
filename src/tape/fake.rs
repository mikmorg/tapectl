//! An in-memory tape with the st driver's positioning semantics — the test
//! double under `TapeStore`'s file cursor (issue #389).
//!
//! `MemStore` is position-addressed: it reads any file without moving
//! anything, so it can prove what the chain walk *reads* but never how a
//! tape store *gets there*. This fakes the layer below `TapeStore` instead
//! ([`crate::store::TapeOps`]), keeps a head, and logs every motion, so a
//! test drives the real `TapeStore::read_file` and asserts on the rewinds
//! and spaces it actually issued.
//!
//! The semantics are the st driver's, as far as `TapeStore` relies on them:
//! a rewind puts the head at file 0; a forward space of `k` from anywhere in
//! file `n` lands at the start of `n + k`, and fails at end of data; a read
//! delivers the rest of the current file and leaves the head at the start
//! of the next one; a read at end of data returns nothing; a write replaces
//! everything from the head onward. A read can be made to fail partway
//! through a file, and the head moved behind the store's back.
//!
//! Opening it (issue #407) is the st driver's too: a read-write open of a
//! write-protected cartridge fails with EROFS, and a write through a
//! read-only open fails with EBADF. A fake that is never opened — a store
//! built with `TapeStore::from_ops` — writes freely, as before.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::error::{Result, TapectlError};
use crate::store::{OpenMode, TapeOps};
use crate::tape::ioctl::{ReadEnd, TapePosition};

/// One operation the store issued, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Rewind,
    /// Forward space this many filemarks.
    Space(u32),
    /// A whole-file read, starting in this file.
    Read(u32),
    /// A bounded head read, starting in this file.
    ReadHead(u32),
    /// A file written at this position.
    Write(u32),
}

#[derive(Debug, Default)]
pub(crate) struct State {
    /// Every file's on-tape (block-padded) bytes, position = index.
    pub files: Vec<Vec<u8>>,
    pub block_size: usize,
    /// `(file, blocks of it already read)`; `file == files.len()` is end of
    /// data.
    pub head: (usize, usize),
    pub ops: Vec<Op>,
    /// A read starting in one of these files delivers one block, then
    /// fails, leaving the head inside the file.
    pub fail_reads_at: Vec<u32>,
    /// The cartridge's write-protect tab is set.
    pub write_protected: bool,
    /// Every open, in order, as `TapeStore::open`/`open_read` asked for it.
    pub opens: Vec<OpenMode>,
    /// A path whose existence every read records in `watched` — how a test
    /// sees what is on disk WHILE the tape is being read (issue #406: where
    /// a restore's decrypted slices are).
    pub watch: Option<std::path::PathBuf>,
    /// `(file read, whether `watch` existed then)`, one per read.
    pub watched: Vec<(u32, bool)>,
}

/// A cloneable handle: the test keeps one, `TapeStore` owns the other.
#[derive(Clone, Default)]
pub(crate) struct FakeTape(Arc<Mutex<State>>);

impl FakeTape {
    /// A tape holding `files` (each already block-padded, as `MemStore`
    /// records them), head at BOT.
    pub(crate) fn with_files(files: Vec<Vec<u8>>, block_size: usize) -> Self {
        Self(Arc::new(Mutex::new(State {
            files,
            block_size,
            ..State::default()
        })))
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }

    pub(crate) fn ops(&self) -> Vec<Op> {
        self.state().ops.clone()
    }

    pub(crate) fn clear_ops(&self) {
        self.state().ops.clear();
    }

    pub(crate) fn rewinds(&self) -> usize {
        self.state()
            .ops
            .iter()
            .filter(|op| **op == Op::Rewind)
            .count()
    }

    pub(crate) fn spaces(&self) -> usize {
        self.state()
            .ops
            .iter()
            .filter(|op| matches!(op, Op::Space(_)))
            .count()
    }

    pub(crate) fn boxed(&self) -> Box<dyn TapeOps> {
        Box::new(self.clone())
    }

    /// Set the cartridge's write-protect tab.
    pub(crate) fn write_protect(&self) {
        self.state().write_protected = true;
    }

    /// Every open so far, in order.
    pub(crate) fn opens(&self) -> Vec<OpenMode> {
        self.state().opens.clone()
    }

    /// Record, at every read from now on, whether `path` exists.
    pub(crate) fn watch(&self, path: &std::path::Path) {
        self.state().watch = Some(path.to_path_buf());
    }

    /// What every read since [`Self::watch`] saw: `(file, path existed)`.
    pub(crate) fn watched(&self) -> Vec<(u32, bool)> {
        self.state().watched.clone()
    }

    fn note_read(s: &mut State, file: usize) {
        if let Some(path) = &s.watch {
            let seen = path.exists();
            s.watched.push((file as u32, seen));
        }
    }

    /// Open the fake as `mode` — what `TapeStore::open`/`open_read` do to a
    /// device. Linux `st.c`'s `check_tape`: a read-write open of a
    /// write-protected cartridge is refused with EROFS.
    pub(crate) fn open_as(&self, mode: OpenMode) -> Result<()> {
        let mut s = self.state();
        s.opens.push(mode);
        if mode == OpenMode::ReadWrite && s.write_protected {
            return Err(TapectlError::TapeIo(
                "open fake tape: Read-only file system (os error 30)".to_string(),
            ));
        }
        Ok(())
    }
}

fn io_error(what: &str) -> TapectlError {
    TapectlError::TapeIo(format!("{what}: Input/output error (os error 5)"))
}

impl TapeOps for FakeTape {
    fn rewind(&self) -> Result<()> {
        let mut s = self.state();
        s.ops.push(Op::Rewind);
        s.head = (0, 0);
        Ok(())
    }

    fn forward_space_file(&self, count: i32) -> Result<()> {
        let mut s = self.state();
        assert!(count > 0, "the store only ever spaces forward: {count}");
        s.ops.push(Op::Space(count as u32));
        let target = s.head.0 + count as usize;
        if target > s.files.len() {
            // st stops at end of data and reports the space failed.
            s.head = (s.files.len(), 0);
            return Err(io_error(&format!("ioctl op=1 count={count}")));
        }
        s.head = (target, 0);
        Ok(())
    }

    fn block_size(&self) -> usize {
        self.state().block_size
    }

    fn position(&self) -> Result<TapePosition> {
        let s = self.state();
        Ok(TapePosition {
            file_number: s.head.0 as i32,
            block_number: s.head.1 as i32,
        })
    }

    fn write_stream(&mut self, src: &mut dyn Read, len: u64, _sync: bool) -> Result<u64> {
        let mut s = self.state();
        if s.opens.last() == Some(&OpenMode::ReadOnly) {
            return Err(TapectlError::TapeIo(
                "write: Bad file descriptor (os error 9)".to_string(),
            ));
        }
        let (file, block) = s.head;
        assert_eq!(block, 0, "the fake only writes at a file boundary");
        s.ops.push(Op::Write(file as u32));
        let mut bytes = Vec::with_capacity(len as usize);
        src.take(len)
            .read_to_end(&mut bytes)
            .map_err(|e| TapectlError::TapeIo(format!("read source: {e}")))?;
        let bs = s.block_size.max(1);
        bytes.resize(bytes.len().div_ceil(bs) * bs, 0);
        let padded = bytes.len() as u64;
        s.files.truncate(file);
        s.files.push(bytes);
        s.head = (file + 1, 0);
        Ok(padded)
    }

    fn read_file_streaming(&mut self, sink: &mut dyn Write) -> Result<(u64, ReadEnd)> {
        let mut s = self.state();
        let (file, start) = s.head;
        s.ops.push(Op::Read(file as u32));
        FakeTape::note_read(&mut s, file);
        if file >= s.files.len() {
            return Ok((0, ReadEnd::Filemark)); // end of data: st returns 0
        }
        let bs = s.block_size.max(1);
        let blocks: Vec<Vec<u8>> = s.files[file].chunks(bs).map(<[u8]>::to_vec).collect();
        let fails = s.fail_reads_at.contains(&(file as u32));
        let mut total = 0u64;
        for (i, block) in blocks.iter().enumerate().skip(start) {
            if fails && i > start {
                s.head = (file, i);
                return Err(io_error("read"));
            }
            sink.write_all(block)
                .map_err(|e| TapectlError::TapeIo(format!("sink write: {e}")))?;
            total += block.len() as u64;
        }
        if fails {
            s.head = (file, blocks.len());
            return Err(io_error("read"));
        }
        s.head = (file + 1, 0);
        Ok((total, ReadEnd::Filemark))
    }

    fn read_file_head(&mut self, max_bytes: u64, sink: &mut dyn Write) -> Result<(u64, ReadEnd)> {
        let mut s = self.state();
        let (file, start) = s.head;
        s.ops.push(Op::ReadHead(file as u32));
        FakeTape::note_read(&mut s, file);
        if file >= s.files.len() {
            return Ok((0, ReadEnd::Filemark));
        }
        let bs = s.block_size.max(1);
        let blocks: Vec<Vec<u8>> = s.files[file].chunks(bs).map(<[u8]>::to_vec).collect();
        let mut total = 0u64;
        let mut next = start;
        // `TapeDevice::read_file_head`'s loop: read blocks while under
        // budget; the filemark is only seen by a read that finds no block.
        while total < max_bytes {
            let Some(block) = blocks.get(next) else {
                s.head = (file + 1, 0);
                return Ok((total, ReadEnd::Filemark));
            };
            let take = (block.len() as u64).min(max_bytes - total) as usize;
            sink.write_all(&block[..take])
                .map_err(|e| TapectlError::TapeIo(format!("sink write: {e}")))?;
            total += take as u64;
            next += 1;
        }
        s.head = (file, next);
        Ok((total, ReadEnd::Stopped))
    }
}
