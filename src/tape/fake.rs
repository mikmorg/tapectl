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
//! of the next one; a read at end of data returns nothing, except at BOT
//! of a blank tape, where st fails it with EIO; a write replaces
//! everything from the head onward. A read can be made to fail partway
//! through a file, and the head moved behind the store's back.
//!
//! Its writes are st's too, block by block (#420): each block goes through
//! the real `tape::ioctl::write_block_retrying`, against a medium with an
//! early warning past which st refuses about one block write in two, a hard
//! end past which it refuses every one, and the hazards a test sets
//! ([`crate::store::MediumEnd`]).
//!
//! Opening it (issue #407) is the st driver's too: a read-write open of a
//! write-protected cartridge fails with EROFS, and a write through a
//! read-only open fails with EBADF. A fake that is never opened — a store
//! built with `TapeStore::from_ops` — writes freely, as before.
//!
//! It also keeps a distance model (issue #416): every motion charges the
//! head's travel, in bytes of tape, to [`State::travel`]. A file's length in
//! the model is its stored bytes unless [`FakeTape::model_len`] gives that
//! position another one, so a test can hold a few kilobytes per slice and
//! still measure a tape of 150 ten-GiB slices — where mhvtl, which rewinds
//! instantly, measures nothing at all.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::error::{Result, TapectlError};
use crate::store::{LbpRefusalAt, MediumEnd, OpenMode, Protection, TapeOps};
use crate::tape::ioctl::{
    write_block_retrying, write_error, EarlyWarning, EarlyWarningState, ReadEnd, TapePosition,
};

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
    /// A long erase (`MTERASE`) from this position.
    Erase(u32),
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
    /// A read starting in one of these files fails before delivering a
    /// byte — a MEDIUM ERROR, or a block-size mismatch (ILI), on the first
    /// block, both of which st reports as EIO (issue #400).
    pub unreadable: Vec<u32>,
    /// Every byte any read has delivered — how a test sees that a bounded
    /// read stopped (issue #400).
    pub bytes_read: u64,
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
    /// `(file read, bytes of regular files directly inside `watch` then)`,
    /// one per read — how a test sees whether a restore SPOOLS decrypted
    /// slices to disk or streams them through named pipes (issue #411; a
    /// FIFO is not a regular file and holds nothing on disk).
    pub watched_bytes: Vec<(u32, u64)>,
    /// The distance model's length for the file at a position, where it is
    /// not the file's stored bytes (issue #416). Keyed by POSITION, not by
    /// the file there, so it can be set before a write and survives one.
    pub modeled: std::collections::HashMap<usize, u64>,
    /// The head's travel so far, in modeled bytes of tape: a rewind charges
    /// the distance back to BOT, a space the files it crosses, a read the
    /// bytes it passes over, a write the file it lays down.
    pub travel: u64,
    /// Rewinds that started anywhere but BOT — the ones that move tape. A
    /// rewind at BOT (every open's) costs nothing and is not counted.
    pub moving_rewinds: usize,
    /// `MTIOCGET` fails — what makes `TapeStore` distrust its cursor and
    /// reposition from BOT before every read, the access pattern of every
    /// release before 1.0.5 (#389). The positive control for the motion
    /// budgets (issue #416).
    pub position_fails: bool,
    /// The medium's HARD END: it holds this many stored bytes, then every
    /// block write is refused with ENOSPC — retried, and after more than
    /// 16 refusals of one block the write fails as a full medium: the
    /// blocks that fit are on the tape, no filemark follows them (issue
    /// #416 item 13 — the end-of-tape path under `write_stream`; since
    /// #420 the hard end past the early warning).
    pub capacity: Option<u64>,
    /// The drive's early warning and st's alternation past it, and the
    /// hazards a test sets (#420): see [`MediumEnd`].
    pub end: MediumEnd,
    /// A drive that reports every write and erase done and changes nothing
    /// on the medium — how a test proves `cartridge erase` checks its own
    /// work by reading it back, never by trusting the drive's word (ADR-0012
    /// 2026-10-12 item 51).
    pub writes_vanish: bool,
    /// A long erase that fails part-way, as st fails one on a medium error:
    /// the command returns EIO with the medium partly erased.
    pub erase_fails: bool,
    /// The opens, counted from 1 in the order they are asked for, that fail
    /// as a drive or transport error fails one (EIO) — how a test reaches
    /// the reopen after a write has already happened.
    pub fail_opens: Vec<usize>,
    /// What every open of the drive decides about Logical Block Protection
    /// (#388): `NotApplicable` unless a test sets it.
    pub protection: Protection,
    /// Refusals under LBP, each answered once by the write of its
    /// `surfaces_in` file (item 174): see [`LbpRefusalAt`].
    pub lbp_refusals: Vec<LbpRefusalAt>,
    /// Where the last refusal left the drive, until the head moves.
    pub refused_at: Option<u64>,
    /// `arm_read_check` was asked (item 175).
    pub read_check_armed: bool,
}

impl State {
    /// This position's length in the distance model.
    pub(crate) fn modeled_len(&self, file: usize) -> u64 {
        self.modeled
            .get(&file)
            .copied()
            .unwrap_or_else(|| self.files.get(file).map_or(0, |f| f.len() as u64))
    }

    /// Blocks and filemarks before tape file `file`: the drive's logical
    /// position at its start.
    pub(crate) fn objects_before(&self, file: usize) -> u64 {
        let bs = self.block_size.max(1);
        self.files[..file.min(self.files.len())]
            .iter()
            .map(|f| f.len().div_ceil(bs) as u64 + 1)
            .sum()
    }

    /// Where `head` is, in modeled bytes from BOT. Inside a file, the share
    /// of its blocks already read, scaled to its modeled length.
    fn offset(&self, (file, block): (usize, usize)) -> u64 {
        let before: u64 = (0..file.min(self.files.len()))
            .map(|i| self.modeled_len(i))
            .sum();
        let Some(bytes) = self.files.get(file) else {
            return before;
        };
        let blocks = bytes.len().div_ceil(self.block_size.max(1)) as u64;
        if blocks == 0 {
            return before;
        }
        before + self.modeled_len(file) * (block as u64).min(blocks) / blocks
    }
}

/// A cloneable handle: the test keeps one, `TapeStore` owns the other.
///
/// The medium and the drive's state are shared by every handle; the early
/// warning a write saw is the HANDLE's, as it is an open device's in
/// production (`TapeDevice`, #420). [`Self::boxed`] — what a store is built
/// over — is a new open, starting with no warning.
#[derive(Clone, Default)]
pub(crate) struct FakeTape {
    shared: Arc<Mutex<State>>,
    warning: EarlyWarningState,
}

impl FakeTape {
    /// A tape holding `files` (each already block-padded, as `MemStore`
    /// records them), head at BOT.
    pub(crate) fn with_files(files: Vec<Vec<u8>>, block_size: usize) -> Self {
        Self {
            shared: Arc::new(Mutex::new(State {
                files,
                block_size,
                ..State::default()
            })),
            warning: EarlyWarningState::default(),
        }
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, State> {
        self.shared.lock().unwrap()
    }

    /// The drive reports its early warning once the medium holds `bytes`
    /// stored bytes (#420): st's alternation from there, as [`MediumEnd`]
    /// models it. The hard end stays [`State::capacity`].
    pub(crate) fn early_warning_at(&self, bytes: u64) {
        self.state().end.early_warning_at = Some(bytes);
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

    /// A new open of the device over the same medium: what a store is built
    /// over. Its early warning starts at `None`.
    pub(crate) fn boxed(&self) -> Box<dyn TapeOps> {
        Box::new(Self {
            shared: self.shared.clone(),
            warning: EarlyWarningState::default(),
        })
    }

    /// Model the file at `position` as `len` bytes of tape (issue #416).
    pub(crate) fn model_len(&self, position: u32, len: u64) {
        self.state().modeled.insert(position as usize, len);
    }

    /// The head's travel since the last [`Self::load`], in modeled bytes.
    pub(crate) fn travel(&self) -> u64 {
        self.state().travel
    }

    /// The recorded tape's modeled length: BOT to end of data.
    pub(crate) fn tape_length(&self) -> u64 {
        let s = self.state();
        (0..s.files.len()).map(|i| s.modeled_len(i)).sum()
    }

    /// The cartridge as a new contact finds it: just loaded, head at BOT,
    /// nothing counted yet (ops, travel, bytes read, opens).
    pub(crate) fn load(&self) {
        let mut s = self.state();
        s.head = (0, 0);
        s.ops.clear();
        s.opens.clear();
        s.travel = 0;
        s.moving_rewinds = 0;
        s.bytes_read = 0;
        s.end.moved();
    }

    /// Rewinds since the last [`Self::load`] that moved tape.
    pub(crate) fn moving_rewinds(&self) -> usize {
        self.state().moving_rewinds
    }

    /// Run one `TapeOps` call on the state, charging the head's modeled
    /// travel from where it was to where the call left it.
    fn travelled<T>(&self, op: impl FnOnce(&mut State) -> T) -> T {
        let mut s = self.state();
        let before = s.offset(s.head);
        let out = op(&mut s);
        let after = s.offset(s.head);
        s.travel += before.abs_diff(after);
        out
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

    /// What every read since [`Self::watch`] saw on disk:
    /// `(file, bytes of regular files directly inside the watched path)`.
    pub(crate) fn watched_bytes(&self) -> Vec<(u32, u64)> {
        self.state().watched_bytes.clone()
    }

    /// The reads st fails with EIO before a byte arrives: a file marked
    /// [`State::unreadable`], and a read at BOT of a blank tape (`st.c`'s
    /// `read_tape`: a BLANK CHECK not just after a filemark is `-EIO`; only
    /// the first BLANK CHECK after crossing a filemark returns 0, which is
    /// what the end-of-data case below the call keeps).
    fn refuse_unreadable(s: &State, file: usize) -> Result<()> {
        if s.unreadable.contains(&(file as u32)) || (file == 0 && s.files.is_empty()) {
            return Err(read_failed());
        }
        Ok(())
    }

    /// One file's blocks, then its filemark, as st takes them on the medium
    /// [`State`] models after its last file: each block through the real
    /// [`write_block_retrying`] against [`MediumEnd`] and the hard end
    /// ([`State::capacity`]), into `bytes`; then one filemark, never retried
    /// (`volume-format-v2.md` §4.3 W3). On an error, `bytes` holds what
    /// landed.
    fn lay_down(
        s: &mut State,
        src: &mut dyn Read,
        len: u64,
        sync: bool,
        bytes: &mut Vec<u8>,
        warning: &mut EarlyWarningState,
    ) -> Result<()> {
        let bs = s.block_size.max(1);
        let before: u64 = s.files.iter().map(|f| f.len() as u64).sum();
        let mut buf = vec![0u8; bs];
        let mut read = 0u64;
        while read < len {
            let want = (len - read).min(bs as u64) as usize;
            // `TapeDevice::write_stream`: a short source is the source's
            // error, named so (issue #408).
            src.read_exact(&mut buf[..want]).map_err(|e| {
                TapectlError::SourceIo(if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    format!("source exhausted (declared length {len}, {read} read)")
                } else {
                    format!("read source: {e}")
                })
            })?;
            buf[want..].fill(0);
            read += want as u64;
            let (end, capacity) = (&mut s.end, s.capacity);
            write_block_retrying(
                |b| {
                    let stored = before + bytes.len() as u64;
                    end.write_block(stored, capacity, b, &mut |l| bytes.extend(l))
                },
                &buf,
                warning,
            )
            .map_err(|e| write_error("write", &e))?;
        }
        // `TapeDevice::write_stream`: a refused filemark is a full medium.
        s.end
            .write_filemark(before + bytes.len() as u64)
            .map_err(|e| {
                let op = if sync { 5 } else { 35 };
                TapectlError::MediumFull(
                    TapectlError::TapeIo(format!("ioctl op={op} count=1: {e}")).to_string(),
                )
            })
    }

    fn note_read(s: &mut State, file: usize) {
        if let Some(path) = &s.watch {
            let seen = path.exists();
            let bytes = std::fs::read_dir(path)
                .map(|entries| {
                    entries
                        .filter_map(|e| e.ok()?.metadata().ok())
                        .filter(|m| m.is_file())
                        .map(|m| m.len())
                        .sum()
                })
                .unwrap_or(0);
            s.watched.push((file as u32, seen));
            s.watched_bytes.push((file as u32, bytes));
        }
    }

    /// Open the fake as `mode` — what `TapeStore::open`/`open_read` do to a
    /// device. Linux `st.c`'s `check_tape`: a read-write open of a
    /// write-protected cartridge is refused with EROFS.
    pub(crate) fn open_as(&self, mode: OpenMode) -> Result<()> {
        let mut s = self.state();
        s.opens.push(mode);
        s.end.moved();
        if s.fail_opens.contains(&s.opens.len()) {
            return Err(io_error("open fake tape"));
        }
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

/// A read st fails with EIO — noted for the open contact as
/// `TapeDevice::read_file_streaming`/`read_file_head` note theirs (issue
/// #344), on whatever thread the read runs.
fn read_failed() -> TapectlError {
    crate::tape::mtget_journal::note(
        crate::tape::mtget_journal::POINT_FAILURE,
        "fake-tape",
        Some("read"),
        Some(5),
        Ok(crate::tape::mtget_journal::MtStatus::default()),
    );
    io_error("read")
}

impl TapeOps for FakeTape {
    fn rewind(&self) -> Result<()> {
        self.travelled(|s| {
            s.ops.push(Op::Rewind);
            if s.head != (0, 0) {
                s.moving_rewinds += 1;
            }
            s.head = (0, 0);
            s.end.moved();
            s.refused_at = None;
            Ok(())
        })
    }

    fn forward_space_file(&self, count: i32) -> Result<()> {
        self.travelled(|s| {
            assert!(count > 0, "the store only ever spaces forward: {count}");
            s.ops.push(Op::Space(count as u32));
            s.end.moved();
            s.refused_at = None;
            let target = s.head.0 + count as usize;
            if target > s.files.len() {
                // st stops at end of data and reports the space failed.
                s.head = (s.files.len(), 0);
                return Err(io_error(&format!("ioctl op=1 count={count}")));
            }
            s.head = (target, 0);
            Ok(())
        })
    }

    fn block_size(&self) -> usize {
        self.state().block_size
    }

    fn position(&self) -> Result<TapePosition> {
        let s = self.state();
        if s.position_fails {
            return Err(io_error("ioctl MTIOCGET"));
        }
        Ok(TapePosition {
            file_number: s.head.0 as i32,
            block_number: s.head.1 as i32,
            at_eod: s.head.0 >= s.files.len(),
        })
    }

    /// The st write, block by block: each block through the real
    /// [`write_block_retrying`] against the medium's [`MediumEnd`] and
    /// [`State::capacity`], then the filemark, never retried. A write that
    /// fails leaves the blocks that landed on the tape with no filemark
    /// after them, the head inside the file.
    fn write_stream(&mut self, src: &mut dyn Read, len: u64, sync: bool) -> Result<u64> {
        let mut warning = std::mem::take(&mut self.warning);
        let written = self.travelled(|s| {
            if s.opens.last() == Some(&OpenMode::ReadOnly) {
                return Err(TapectlError::TapeIo(
                    "write: Bad file descriptor (os error 9)".to_string(),
                ));
            }
            let (file, block) = s.head;
            assert_eq!(block, 0, "the fake only writes at a file boundary");
            s.ops.push(Op::Write(file as u32));
            // #388, item 174: a refusal set for this file's write. What came
            // before the refused block stays on the tape (the HP's way:
            // the block itself too, mhvtl's); everything after is gone.
            if let Some(k) = s
                .lbp_refusals
                .iter()
                .position(|r| r.surfaces_in == file as u32)
            {
                let at = s.lbp_refusals.remove(k);
                let bad = (at.file as usize).min(s.files.len());
                let kept = at.block as usize + usize::from(at.stored_before_refusing);
                let bs = s.block_size.max(1);
                s.files.truncate(bad + 1);
                if let Some(partial) = s.files.get_mut(bad) {
                    partial.truncate(kept * bs);
                }
                let objects = s.objects_before(bad);
                s.refused_at = Some(at.position(objects));
                s.head = (bad, kept);
                s.end.moved();
                return Err(at.error());
            }
            if s.writes_vanish {
                return Ok(len.div_ceil(s.block_size.max(1) as u64) * s.block_size.max(1) as u64);
            }
            let bs = s.block_size.max(1);
            s.files.truncate(file);
            let mut bytes = Vec::new();
            let laid = FakeTape::lay_down(s, src, len, sync, &mut bytes, &mut warning);
            let blocks = bytes.len() / bs;
            let padded = bytes.len() as u64;
            if let Err(e) = laid {
                // The blocks that landed are on the tape; no filemark follows.
                if blocks > 0 {
                    s.files.push(bytes);
                }
                s.head = (file, blocks);
                return Err(e);
            }
            s.files.push(bytes);
            s.head = (file + 1, 0);
            Ok(padded)
        });
        self.warning = warning;
        written
    }

    fn early_warning(&self) -> Option<EarlyWarning> {
        self.warning.warning()
    }

    fn protection(&self) -> Protection {
        self.state().protection.clone()
    }

    /// Where a refusal left the drive, or where the head is.
    fn logical_position(&self) -> Result<Option<u64>> {
        let s = self.state();
        Ok(Some(s.refused_at.unwrap_or_else(|| {
            s.objects_before(s.head.0) + s.head.1 as u64
        })))
    }

    fn arm_read_check(&mut self) -> Result<bool> {
        let mut s = self.state();
        s.read_check_armed = true;
        Ok(s.protection.writes_checked())
    }

    fn read_file_streaming(&mut self, sink: &mut dyn Write) -> Result<(u64, ReadEnd)> {
        self.travelled(|s| {
            let (file, start) = s.head;
            s.ops.push(Op::Read(file as u32));
            FakeTape::note_read(s, file);
            FakeTape::refuse_unreadable(s, file)?;
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
                    return Err(read_failed());
                }
                sink.write_all(block)
                    .map_err(|e| TapectlError::TapeIo(format!("sink write: {e}")))?;
                total += block.len() as u64;
                s.bytes_read += block.len() as u64;
            }
            if fails {
                s.head = (file, blocks.len());
                return Err(read_failed());
            }
            s.head = (file + 1, 0);
            Ok((total, ReadEnd::Filemark))
        })
    }

    /// st's long erase: everything from the head onward is gone, and the
    /// head is where it was (the caller rewinds). Refused through a
    /// read-only open, as a write is.
    fn erase_long(&mut self) -> Result<()> {
        self.travelled(|s| {
            if s.opens.last() == Some(&OpenMode::ReadOnly) {
                return Err(TapectlError::TapeIo(
                    "ioctl op=13 count=1: Bad file descriptor (os error 9)".to_string(),
                ));
            }
            let (file, _) = s.head;
            s.ops.push(Op::Erase(file as u32));
            if s.erase_fails {
                s.files.truncate(file + 1);
                return Err(io_error("ioctl op=13 count=1"));
            }
            if !s.writes_vanish {
                s.files.truncate(file);
                s.head = (file.min(s.files.len()), 0);
            }
            Ok(())
        })
    }

    fn read_file_head(&mut self, max_bytes: u64, sink: &mut dyn Write) -> Result<(u64, ReadEnd)> {
        self.travelled(|s| {
            let (file, start) = s.head;
            s.ops.push(Op::ReadHead(file as u32));
            FakeTape::note_read(s, file);
            FakeTape::refuse_unreadable(s, file)?;
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
                s.bytes_read += block.len() as u64;
                next += 1;
            }
            s.head = (file, next);
            Ok((total, ReadEnd::Stopped))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BS: usize = 4;

    /// Three one-block files, modeled as 100, 1 000 and 10 bytes of tape.
    fn modeled() -> FakeTape {
        let fake = FakeTape::with_files(vec![vec![0u8; BS]; 3], BS);
        fake.model_len(0, 100);
        fake.model_len(1, 1_000);
        fake.model_len(2, 10);
        fake
    }

    /// The distance model charges every motion by where it moves the head:
    /// a space the files it crosses, a read the file it passes over, a
    /// rewind the way back to BOT.
    #[test]
    fn every_motion_is_charged_the_distance_it_moves_the_head() {
        let mut fake = modeled();
        assert_eq!(fake.tape_length(), 1_110);
        fake.forward_space_file(1).unwrap();
        assert_eq!(fake.travel(), 100);
        fake.read_file_streaming(&mut std::io::sink()).unwrap();
        assert_eq!(fake.travel(), 1_100);
        fake.rewind().unwrap();
        assert_eq!(fake.travel(), 2_200, "back from 1 100 bytes in");
        fake.rewind().unwrap();
        assert_eq!(fake.travel(), 2_200, "a rewind at BOT moves nothing");
        assert_eq!((fake.rewinds(), fake.moving_rewinds()), (2, 1));
        fake.load();
        assert_eq!((fake.travel(), fake.ops()), (0, Vec::new()));
    }

    /// A bounded head read is charged for the share of the file it read; a
    /// failed space for the way to end of data.
    #[test]
    fn a_partial_read_and_a_failed_space_are_charged_where_they_stop() {
        let fake = FakeTape::with_files(vec![vec![0u8; 4 * BS]], BS);
        fake.model_len(0, 400);
        let mut f = fake.clone();
        f.read_file_head(BS as u64, &mut std::io::sink()).unwrap();
        assert_eq!(fake.travel(), 100, "one block of four");
        f.forward_space_file(5).unwrap_err();
        assert_eq!(fake.travel(), 400, "st stops at end of data");
    }

    /// A write lays down its file's modeled length, and the model, keyed by
    /// position, holds for a file written after it was set.
    #[test]
    fn a_write_is_charged_the_modeled_length_of_what_it_lays_down() {
        let mut fake = FakeTape::with_files(Vec::new(), BS);
        fake.model_len(1, 5_000);
        fake.write_stream(&mut &[1u8; BS][..], BS as u64, false)
            .unwrap();
        fake.write_stream(&mut &[2u8; BS][..], BS as u64, false)
            .unwrap();
        assert_eq!(fake.travel(), BS as u64 + 5_000);
        assert_eq!(fake.tape_length(), BS as u64 + 5_000);
    }

    /// A medium with `capacity` records the blocks that fit of the write
    /// that crosses it, writes no filemark, and fails with st's ENOSPC.
    #[test]
    fn a_write_past_the_capacity_records_what_fits_and_fails_enospc() {
        let mut fake = FakeTape::with_files(Vec::new(), BS);
        fake.state().capacity = Some(3 * BS as u64);
        fake.write_stream(&mut &[1u8; BS][..], BS as u64, false)
            .unwrap();
        let err = fake
            .write_stream(&mut &[2u8; 4 * BS][..], 4 * BS as u64, false)
            .unwrap_err();
        assert!(matches!(err, TapectlError::MediumFull(ref m) if m.contains("os error 28")));
        let s = fake.state();
        assert_eq!(s.files, vec![vec![1u8; BS], vec![2u8; 2 * BS]]);
        assert_eq!(s.head, (1, 2), "inside the file: no filemark was written");
    }

    // --- the early warning (#420, `volume-format-v2.md` §4.3) -----------

    fn block_of(b: u8, blocks: usize) -> Vec<u8> {
        vec![b; blocks * BS]
    }

    fn write(fake: &mut FakeTape, bytes: &[u8]) -> Result<u64> {
        fake.write_stream(&mut &bytes[..], bytes.len() as u64, false)
    }

    /// st past the warning, as measured: the block that reaches the warning
    /// is accepted and arms a refusal, the next is refused and retried, and
    /// so on; a filemark disarms, so the next file's first block is accepted
    /// at once. Every block lands once, and the handle that wrote keeps the
    /// warning: after the bytes it had accepted, with every refusal counted.
    #[test]
    fn a_write_across_the_early_warning_lands_every_block_once() {
        let mut fake = FakeTape::with_files(Vec::new(), BS);
        fake.early_warning_at(2 * BS as u64);
        write(&mut fake, &block_of(1, 1)).unwrap();
        assert_eq!(fake.early_warning(), None, "not reached yet");
        assert_eq!(write(&mut fake, &block_of(2, 4)).unwrap(), 4 * BS as u64);
        assert_eq!(
            fake.early_warning(),
            Some(EarlyWarning {
                bytes_at: 2 * BS as u64,
                refusals: 3
            }),
            "accepted, then three blocks refused once each"
        );
        // The filemark disarmed: one block is accepted at once; the next
        // file's second block is refused once.
        write(&mut fake, &block_of(3, 1)).unwrap();
        write(&mut fake, &block_of(4, 2)).unwrap();
        assert_eq!(fake.early_warning().unwrap().refusals, 4);
        assert_eq!(
            fake.state().files,
            vec![
                block_of(1, 1),
                block_of(2, 4),
                block_of(3, 1),
                block_of(4, 2)
            ],
            "no block lost, none doubled"
        );
        assert_eq!(fake.state().head, (4, 0));
    }

    /// Runs of refusals (up to 3 were measured) are retried the same way.
    #[test]
    fn runs_of_refusals_after_each_accepted_block_are_retried() {
        let mut fake = FakeTape::with_files(Vec::new(), BS);
        fake.early_warning_at(0);
        fake.state().end.refusal_run = 3;
        write(&mut fake, &block_of(5, 3)).unwrap();
        assert_eq!(fake.early_warning().unwrap().refusals, 6);
        assert_eq!(fake.state().files, vec![block_of(5, 3)]);
    }

    /// The hard end past the warning: every block refused, so after more
    /// than 16 refusals of one block the write fails as a full medium, the
    /// blocks that landed on the tape with no filemark after them.
    #[test]
    fn the_hard_end_past_the_warning_is_a_full_medium() {
        let mut fake = FakeTape::with_files(Vec::new(), BS);
        fake.early_warning_at(BS as u64);
        fake.state().capacity = Some(3 * BS as u64);
        let err = write(&mut fake, &block_of(6, 5)).unwrap_err();
        assert!(
            matches!(err, TapectlError::MediumFull(ref m) if m.contains("os error 28")),
            "{err:?}"
        );
        let refusals = fake.early_warning().unwrap().refusals;
        assert_eq!(refusals, 2 + 16, "two retried, then sixteen before the end");
        let s = fake.state();
        assert_eq!(s.files, vec![block_of(6, 3)]);
        assert_eq!(s.head, (0, 3), "inside the file: no filemark was written");
    }

    /// The hazard the readback past the warning is for (ADR-0012 item 114):
    /// a block st reported refused was in fact written, so the retry lays it
    /// down twice and the file is a block longer than what was sent.
    #[test]
    fn a_refused_block_that_was_written_is_doubled_by_the_retry() {
        let mut fake = FakeTape::with_files(Vec::new(), BS);
        fake.early_warning_at(BS as u64);
        fake.state().end.refused_blocks_land = true;
        let sent: Vec<u8> = (1..=3u8).flat_map(|b| vec![b; BS]).collect();
        write(&mut fake, &sent).unwrap();
        let expected: Vec<u8> = [1u8, 2, 2, 3, 3]
            .iter()
            .flat_map(|b| vec![*b; BS])
            .collect();
        assert_eq!(fake.state().files, vec![expected]);
        assert_eq!(fake.early_warning().unwrap().refusals, 2);
    }

    /// A refused filemark is never retried (W3): the write fails as a full
    /// medium at once, its blocks on the tape and no filemark after them.
    /// It is not a refused BLOCK: it counts no refusal.
    #[test]
    fn a_refused_filemark_is_a_full_medium_and_never_retried() {
        let mut fake = FakeTape::with_files(Vec::new(), BS);
        fake.state().end.filemarks_refused_from = Some(2 * BS as u64);
        write(&mut fake, &block_of(1, 1)).unwrap();
        let err = write(&mut fake, &block_of(2, 1)).unwrap_err();
        assert!(
            matches!(err, TapectlError::MediumFull(ref m) if m.contains("ioctl op=35")
                && m.contains("os error 28")),
            "{err:?}"
        );
        assert_eq!(fake.early_warning(), None);
        let s = fake.state();
        assert_eq!(s.files, vec![block_of(1, 1), block_of(2, 1)]);
        assert_eq!(s.head, (1, 1), "inside the file: its filemark was refused");
    }

    /// A store is built over a new open ([`FakeTape::boxed`]): it starts
    /// with no warning, though the medium is the same.
    #[test]
    fn a_new_open_starts_without_the_warning() {
        let mut fake = FakeTape::with_files(Vec::new(), BS);
        fake.early_warning_at(0);
        write(&mut fake, &block_of(1, 2)).unwrap();
        assert!(fake.early_warning().is_some());
        let reopened = fake.boxed();
        assert_eq!(reopened.early_warning(), None);
        assert_eq!(fake.state().files.len(), 1, "the same medium");
    }

    /// `position_fails` fails `MTIOCGET` and nothing else.
    #[test]
    fn position_fails_fails_only_the_position_query() {
        let mut fake = modeled();
        fake.state().position_fails = true;
        assert!(fake.position().is_err());
        fake.read_file_streaming(&mut std::io::sink()).unwrap();
        assert_eq!(fake.ops(), vec![Op::Read(0)]);
    }
}
