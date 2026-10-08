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
    /// The medium holds this many stored bytes, then a write fails as st
    /// fails one at the early-warning point: the blocks that fit are on
    /// the tape, no filemark follows them, and the error is ENOSPC (issue
    /// #416 item 13 — the end-of-tape path under `write_stream`, which no
    /// real medium has run).
    pub capacity: Option<u64>,
}

impl State {
    /// This position's length in the distance model.
    pub(crate) fn modeled_len(&self, file: usize) -> u64 {
        self.modeled
            .get(&file)
            .copied()
            .unwrap_or_else(|| self.files.get(file).map_or(0, |f| f.len() as u64))
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
            Ok(())
        })
    }

    fn forward_space_file(&self, count: i32) -> Result<()> {
        self.travelled(|s| {
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

    fn write_stream(&mut self, src: &mut dyn Read, len: u64, _sync: bool) -> Result<u64> {
        self.travelled(|s| {
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
                .map_err(|e| TapectlError::SourceIo(format!("read source: {e}")))?;
            let bs = s.block_size.max(1);
            bytes.resize(bytes.len().div_ceil(bs) * bs, 0);
            let padded = bytes.len() as u64;
            s.files.truncate(file);
            if let Some(capacity) = s.capacity {
                let used: u64 = s.files.iter().map(|f| f.len() as u64).sum();
                let room = capacity.saturating_sub(used) / bs as u64 * bs as u64;
                if padded > room {
                    // `TapeDevice::write_stream`'s `write_error`: ENOSPC is
                    // a full medium. The blocks before it are recorded.
                    bytes.truncate(room as usize);
                    let blocks = bytes.len() / bs;
                    if blocks > 0 {
                        s.files.push(bytes);
                    }
                    s.head = (file, blocks);
                    return Err(TapectlError::MediumFull(
                        "write: No space left on device (os error 28)".to_string(),
                    ));
                }
            }
            s.files.push(bytes);
            s.head = (file + 1, 0);
            Ok(padded)
        })
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
