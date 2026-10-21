//! `TAPECTL_EXP_CROSSFILE=1`: the write queue carried across files (issue
//! #488; the write-shaping note, `docs/research/2026-10-20-write-shaping.md`
//! §9 item 6 and §9.1). In a `research` build only: no other build compiles
//! this module, and with it absent nothing in the per-file pipeline
//! ([`super::write_verified`]) changes.
//!
//! **Why.** The per-file pipeline is built, drained and joined for every file
//! of the Layout, so at each file boundary its queue empties and the store
//! waits for the next file's first block to be read and hashed. A queue
//! larger than one file (`TAPECTL_EXP_QUEUE_BYTES=4G` against 1 GiB slices)
//! can then never fill, which the note's R7-R9 need it to.
//!
//! **What it is.** Before its execute loop the write session hands the files
//! it is about to write to [`BufferPool::carry_across`]. ONE reader thread
//! then reads them in order, each through [`read_stage`], the per-file
//! pipeline's own reader, and ONE hasher thread hashes them; the boundaries
//! travel as messages. Each `write_verified` call takes its file's blocks
//! from that one queue, so the reader and the hasher run on into the next
//! files while the store writes this one, up to the pool's bound.
//!
//! **What does not change, per file:**
//! - the hasher withholds the file's last block until the file's hash is
//!   finished, and drops it on a mismatch, so the store's read where it
//!   should be fails and neither that block nor the filemark after it is
//!   written ([`Streamed::withheld`]);
//! - a staged file that fails or ends short is the store's own error at the
//!   same offset, and the verdict keeps its precedence
//!   ([`Streamed::verdict`]);
//! - Ctrl-C is checked between files by the write session, on the writer's
//!   side: nothing read ahead reaches the store before its own file's turn;
//! - the memory bound: the carry takes the pool's buffers over and never
//!   holds more than its `max_buffers` of them.
//!
//! **Its end.** The first file that is not written whole and matching ends
//! the carry, as it ends the session; any later call is refused. Dropping
//! the pool drops the carry: the writer's ends of the channels go first,
//! then both threads are joined.
//!
//! **The trace and the shaper see one run.** The gauges' queue counters are
//! not reset between files (they hold the next files' blocks), and the
//! shaper judges the queue's fill against what is left of the whole run.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sha2::Digest;

use super::{read_stage, stopped, withheld, BufferPool, Msg, Producer, Stats, Streamed};
use crate::error::{Result, TapectlError};
use crate::pass_count::Sha256;
use crate::pipeline_trace::{self, Gauges, Role};
use crate::progress;
use crate::research::Shaper;

/// What the hasher passes to the writer: each file's blocks, then exactly
/// one message saying how the file ended.
enum Carried {
    /// A buffer holding this many bytes of the current file, in order.
    Data(Vec<u8>, usize),
    /// The file's stream ended: its sha256 when every declared byte was
    /// hashed (its last block went on before this), `None` when the source
    /// ended short.
    End(Option<String>),
    /// The source failed; its own error, passed through untouched.
    Fail(io::Error),
    /// The file's last block was withheld: its bytes hash to this, not the
    /// recorded sha256 (tri-layer L2).
    Withheld(String),
}

/// How the writer saw one file's stream end.
enum Terminal {
    Ended(Option<String>),
    Withheld(String),
    Failed,
}

/// The carried pipeline: its two threads, the writer's ends of their
/// channels, and the files still to come.
pub(super) struct Carry {
    /// The hasher's output, every file's in turn.
    rx: Receiver<Carried>,
    /// Where a drained buffer goes back to the reader.
    free: Sender<Vec<u8>>,
    /// Each file still to come, in write order: its declared length and
    /// recorded sha256, checked against the call that writes it.
    plan: VecDeque<(u64, Option<String>)>,
    /// The trace's gauges, or the carry's own: the reader's waits on them
    /// are each file's `queue full`.
    gauges: Arc<Gauges>,
    shaper: Option<Shaper>,
    /// Bytes handed to the store over the whole run.
    taken: u64,
    /// Tells the reader to start no further file.
    stop: Arc<AtomicBool>,
    /// A file was not written whole and matching: the carry writes nothing
    /// more.
    ended: bool,
    reader: Option<JoinHandle<()>>,
    hasher: Option<JoinHandle<()>>,
}

impl BufferPool {
    /// Carry the write queue across `files` when `TAPECTL_EXP_CROSSFILE=1`
    /// asked for it: the files the write session is about to write, in write
    /// order, each as its staged path, declared length and recorded sha256.
    /// Every later [`super::write_verified`] through this pool then takes its
    /// file from the one carried pipeline, in that order. With the knob
    /// unset, or `None` (an entry with no staged path or size, which the
    /// execute loop refuses by itself), the per-file pipeline stays.
    pub fn carry_across(&mut self, files: Option<Vec<(PathBuf, u64, Option<String>)>>) {
        if !crate::research::knobs().crossfile {
            return;
        }
        if let Some(files) = files {
            self.start_carry(files);
        }
    }

    /// Start the carried pipeline over `files`; the knob is already decided.
    fn start_carry(&mut self, files: Vec<(PathBuf, u64, Option<String>)>) {
        // The knobs, the queue size and the trace, decided as the first
        // file's write would decide them.
        self.decide();
        let gauges = self.observe.gauges.clone().unwrap_or_default();
        let mut shaper = self.observe.shaper.take();
        if let Some(sh) = shaper.as_mut() {
            let run: u64 = files.iter().map(|f| f.1).sum();
            sh.begin_file(run, self.capacity_bytes());
        }
        // The reader's own pool: this pool's buffers, and its bound.
        let mut own = BufferPool::new(self.chunk, self.max_buffers);
        own.free = std::mem::take(&mut self.free);
        own.allocated = own.free.len();
        self.allocated = 0;

        let (free_tx, free_rx) = mpsc::channel::<Vec<u8>>();
        let (read_tx, read_rx) = mpsc::channel::<Msg>();
        let (hash_tx, hash_rx) = mpsc::channel::<Carried>();
        let stop = Arc::new(AtomicBool::new(false));
        let reads: Vec<(PathBuf, u64)> = files
            .iter()
            .map(|(path, len, _)| (path.clone(), *len))
            .collect();
        let plan: VecDeque<(u64, Option<String>)> =
            files.into_iter().map(|(_, len, sha)| (len, sha)).collect();
        let hashes: Vec<(u64, Option<String>)> = plan.iter().cloned().collect();
        let count = plan.len();

        let reader = {
            let (gauges, stop) = (gauges.clone(), stop.clone());
            spawn_carried("tapectl-stage-read", move || {
                reader_stage(reads, own, free_rx, read_tx, gauges, stop)
            })
        };
        let hasher = {
            let gauges = gauges.clone();
            spawn_carried("tapectl-hash", move || {
                hasher_stage(hashes, read_rx, hash_tx, gauges)
            })
        };
        progress::log(&format!(
            "research: {}: one reader and one hasher carry the write queue across the {count} \
             files of this write ({} bytes of queue)",
            crate::research::CROSSFILE_ENV,
            self.capacity_bytes()
        ));
        self.observe.carry = Some(Carry {
            rx: hash_rx,
            free: free_tx,
            plan,
            gauges,
            shaper,
            taken: 0,
            stop,
            ended: false,
            reader: Some(reader),
            hasher: Some(hasher),
        });
    }
}

impl Carry {
    /// Write the next file of the run: what [`super::write_verified`] does
    /// for one file, its blocks taken from the carried queue.
    pub(super) fn write_next(
        &mut self,
        len: u64,
        expected_sha256: Option<&str>,
        execute: impl FnOnce(&mut dyn Read) -> Result<u64>,
    ) -> Streamed {
        let planned = self.plan.pop_front();
        let in_turn = planned
            .as_ref()
            .is_some_and(|(l, sha)| *l == len && sha.as_deref() == expected_sha256);
        if self.ended || !in_turn {
            let why = if self.ended {
                "the write queue carried across files stopped at an earlier file"
            } else {
                "this file is not the one the write queue carried across files holds next"
            };
            self.stop_carry();
            return Streamed {
                store: Err(TapectlError::Other(format!(
                    "write pipeline ({}): {why}",
                    crate::research::CROSSFILE_ENV
                ))),
                sha256: None,
                withheld: false,
                stats: Stats::default(),
            };
        }
        self.gauges.begin_file(len);
        let reader_waits = self.gauges.reader_wait.total(pipeline_trace::now_ns());
        let mut pipe = CarryReader {
            rx: &self.rx,
            free: &self.free,
            current: None,
            end: None,
            failed: None,
            waited: Duration::ZERO,
            gauges: &self.gauges,
            shaper: self.shaper.as_mut(),
            taken: &mut self.taken,
            last_return: None,
            first_block: None,
        };
        let store = execute(&mut pipe);
        // A store that took the whole file stopped short of reading its end:
        // read it now, for the file's hash and so the next file starts in
        // turn.
        if store.is_ok() && pipe.end.is_none() {
            pipe.drain();
        }
        let (end, consumer_waited, first_block_at) =
            (pipe.end.take(), pipe.waited, pipe.first_block);
        drop(pipe);
        let producer_waited = Duration::from_nanos(
            self.gauges
                .reader_wait
                .total(pipeline_trace::now_ns())
                .saturating_sub(reader_waits),
        );
        let (sha256, held_back) = match end {
            Some(Terminal::Ended(sha)) => (sha, false),
            Some(Terminal::Withheld(actual)) => (Some(actual), true),
            Some(Terminal::Failed) | None => (None, false),
        };
        // Only a file written whole and matching lets the next one follow:
        // anything else ends the session, and the carry with it.
        let whole =
            store.is_ok() && !held_back && sha256.is_some() && sha256.as_deref() == expected_sha256;
        if !whole {
            self.stop_carry();
        }
        Streamed {
            store,
            sha256,
            withheld: held_back,
            stats: Stats {
                consumer_waited,
                producer_waited,
                first_block_at,
            },
        }
    }

    /// Write nothing more, and tell the reader to start no further file.
    fn stop_carry(&mut self) {
        self.ended = true;
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Carry {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // The writer's ends first, as the per-file pipeline drops them before
        // it joins: a stage waiting on them sees them go and stops.
        drop(std::mem::replace(&mut self.rx, mpsc::channel().1));
        drop(std::mem::replace(&mut self.free, mpsc::channel().0));
        for stage in [self.reader.take(), self.hasher.take()]
            .into_iter()
            .flatten()
        {
            // A stage that panicked has said so on stderr; the write it
            // served is over either way.
            let _ = stage.join();
        }
        self.gauges.end_file();
    }
}

/// Start a stage thread that outlives the call starting it, carrying the
/// caller's session and tape contact as the per-file stages do.
fn spawn_carried(name: &str, f: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
    let session = progress::handle();
    let contact = crate::tape::mtget_journal::handle();
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let _in = session.enter();
            let _for = contact.enter();
            f()
        })
        .unwrap_or_else(|e| panic!("cannot start the {name} thread: {e}"))
}

/// The reader: every file in turn through [`read_stage`], the per-file
/// pipeline's own reader, so each file's stream is what that reader sends
/// (its blocks, then `End`, or the source's own `Fail`).
fn reader_stage(
    files: Vec<(PathBuf, u64)>,
    mut own: BufferPool,
    mut returned: Receiver<Vec<u8>>,
    tx: Sender<Msg>,
    gauges: Arc<Gauges>,
    stop: Arc<AtomicBool>,
) {
    let _role = gauges.enter(Role::Reader);
    let mut producer = Producer {
        pool: &mut own,
        returned: &mut returned,
        tx,
        waited: Duration::ZERO,
        gauges: Some(gauges.clone()),
    };
    for (path, len) in files {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match crate::util::DropBehind::open(&path) {
            Ok(src) => read_stage(&mut producer, src, len),
            // The store meets the open's error where the file's first byte
            // would be, as it meets a read error.
            Err(e) => {
                if !producer.send(Msg::Fail(e)) {
                    return;
                }
            }
        }
    }
}

/// The hasher: each file in turn by the per-file hasher's rules, its result
/// sent on as the file's last message.
fn hasher_stage(
    files: Vec<(u64, Option<String>)>,
    rx: Receiver<Msg>,
    tx: Sender<Carried>,
    gauges: Arc<Gauges>,
) {
    let _role = gauges.enter(Role::Hasher);
    for (len, expected) in files {
        if !hash_one(&rx, &tx, len, expected.as_deref(), &gauges) {
            return;
        }
    }
}

/// One file's hash, as the per-file hasher makes it: the last block held
/// until the hash is finished, and dropped on a mismatch. `false` once a
/// neighbour has gone (the reader stopped, or the writer dropped its end):
/// the cue to stop.
fn hash_one(
    rx: &Receiver<Msg>,
    tx: &Sender<Carried>,
    len: u64,
    expected: Option<&str>,
    gauges: &Gauges,
) -> bool {
    let mut hasher = Sha256::new();
    let mut hashed = 0u64;
    let busy = |started: Instant| {
        let ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        gauges.hasher_busy_ns.fetch_add(ns, Ordering::Relaxed);
    };
    loop {
        let Ok(msg) = rx.recv() else {
            return false;
        };
        match msg {
            Msg::Data(buf, n) => {
                let started = Instant::now();
                hasher.update(&buf[..n]);
                hashed += n as u64;
                gauges.read_unhashed.fetch_sub(n as u64, Ordering::Relaxed);
                if hashed == len {
                    // The last block: finish the hash before it goes on, so
                    // a mismatched file never reaches its filemark.
                    let actual = format!("{:x}", hasher.finalize());
                    busy(started);
                    let sent = if expected == Some(actual.as_str()) {
                        gauges
                            .hashed_unwritten
                            .fetch_add(n as u64, Ordering::Relaxed);
                        tx.send(Carried::Data(buf, n)).is_ok()
                            && tx.send(Carried::End(Some(actual))).is_ok()
                    } else {
                        drop(buf);
                        tx.send(Carried::Withheld(actual)).is_ok()
                    };
                    // The reader's `End` for this file follows its last block.
                    return sent && matches!(rx.recv(), Ok(Msg::End));
                }
                busy(started);
                gauges
                    .hashed_unwritten
                    .fetch_add(n as u64, Ordering::Relaxed);
                if tx.send(Carried::Data(buf, n)).is_err() {
                    return false;
                }
            }
            Msg::End => {
                // Complete only for an empty file; anything else ended short.
                let sha = (hashed == len).then(|| format!("{:x}", hasher.finalize()));
                return tx.send(Carried::End(sha)).is_ok();
            }
            Msg::Fail(e) => return tx.send(Carried::Fail(e)).is_ok(),
            // Only a hasher withholds; no reader sends it.
            Msg::Withheld => return false,
        }
    }
}

/// The writer's end of the carried queue for one file, read back as one
/// ordinary `Read` for the store: the per-file `PipeReader`'s rules, over a
/// queue that goes on into the next file.
struct CarryReader<'c> {
    rx: &'c Receiver<Carried>,
    free: &'c Sender<Vec<u8>>,
    /// The buffer being drained: (buffer, bytes in it, bytes drained).
    current: Option<(Vec<u8>, usize, usize)>,
    /// How this file's stream ended, once its last message was read.
    end: Option<Terminal>,
    /// A failure already reported, repeated to any read after it.
    failed: Option<(io::ErrorKind, String)>,
    waited: Duration,
    gauges: &'c Gauges,
    shaper: Option<&'c mut Shaper>,
    /// Bytes handed to the store over the whole run.
    taken: &'c mut u64,
    /// When the last read that returned data returned (the store's time
    /// per block, as the per-file reader measures it).
    last_return: Option<Instant>,
    /// When this file's first block reached the store.
    first_block: Option<Instant>,
}

impl CarryReader<'_> {
    fn next(&mut self) -> Carried {
        match self.rx.try_recv() {
            Ok(msg) => msg,
            Err(TryRecvError::Disconnected) => Carried::Fail(stopped()),
            Err(TryRecvError::Empty) => {
                let started = Instant::now();
                self.gauges.writer_wait.begin(pipeline_trace::now_ns());
                let msg = self.rx.recv().unwrap_or_else(|_| Carried::Fail(stopped()));
                self.waited += started.elapsed();
                let took = self.gauges.writer_wait.end(pipeline_trace::now_ns());
                self.gauges
                    .writer_wait_max_ns
                    .fetch_max(took, Ordering::Relaxed);
                if *self.taken > 0 {
                    if let Some(sh) = self.shaper.as_deref_mut() {
                        sh.note_starved();
                    }
                }
                msg
            }
        }
    }

    /// Take one message of this file's stream: a block to drain, or how the
    /// stream ended, with the error the store meets when it ended in one.
    fn receive(&mut self, msg: Carried) -> Option<io::Error> {
        match msg {
            Carried::Data(buf, n) if n > 0 => {
                if self.first_block.is_none() {
                    self.first_block = Some(Instant::now());
                }
                self.current = Some((buf, n, 0));
                None
            }
            Carried::Data(buf, _) => {
                let _ = self.free.send(buf);
                None
            }
            Carried::End(sha) => {
                self.end = Some(Terminal::Ended(sha));
                None
            }
            Carried::Fail(e) => {
                self.end = Some(Terminal::Failed);
                self.failed = Some((e.kind(), e.to_string()));
                Some(e)
            }
            Carried::Withheld(actual) => {
                self.end = Some(Terminal::Withheld(actual));
                let e = withheld();
                self.failed = Some((e.kind(), e.to_string()));
                Some(e)
            }
        }
    }

    /// What is left of this file's stream, up to its last message, each
    /// buffer handed back.
    fn drain(&mut self) {
        if let Some((buf, len, _)) = self.current.take() {
            self.gauges
                .hashed_unwritten
                .fetch_sub(len as u64, Ordering::Relaxed);
            let _ = self.free.send(buf);
        }
        while self.end.is_none() {
            match self.next() {
                Carried::Data(buf, n) => {
                    self.gauges
                        .hashed_unwritten
                        .fetch_sub(n as u64, Ordering::Relaxed);
                    let _ = self.free.send(buf);
                }
                other => {
                    let _ = self.receive(other);
                }
            }
        }
    }
}

impl Read for CarryReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if let Some(t) = self.last_return.take() {
            self.gauges.store_block.record(t.elapsed());
        }
        loop {
            if let Some((buf, len, pos)) = self.current.as_mut() {
                let n = (*len - *pos).min(out.len());
                out[..n].copy_from_slice(&buf[*pos..*pos + n]);
                *pos += n;
                if *pos == *len {
                    let (buf, len, _) = self.current.take().expect("just drained");
                    self.gauges
                        .hashed_unwritten
                        .fetch_sub(len as u64, Ordering::Relaxed);
                    let _ = self.free.send(buf);
                }
                if let Some(sh) = self.shaper.as_deref_mut() {
                    sh.pace(n);
                }
                *self.taken += n as u64;
                self.gauges.written.fetch_add(n as u64, Ordering::Relaxed);
                self.last_return = Some(Instant::now());
                return Ok(n);
            }
            if let Some((kind, msg)) = &self.failed {
                return Err(io::Error::new(*kind, msg.clone()));
            }
            if self.end.is_some() {
                return Ok(0);
            }
            if let Some(sh) = self.shaper.as_deref_mut() {
                sh.before_take();
            }
            let msg = self.next();
            if let Some(e) = self.receive(msg) {
                return Err(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::{write_verified, Observe, Verdict};
    use crate::store::{MemStore, Store};
    use std::path::Path;

    const CHUNK: usize = 16 * 1024;

    fn sha(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    /// Staged files on disk, each its own pattern, with the length and the
    /// recorded sha256 a Layout entry would carry for it.
    fn staged(dir: &Path, sizes: &[usize]) -> Vec<(PathBuf, u64, Option<String>)> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, &n)| {
                let bytes: Vec<u8> = (0..n).map(|j| (j * 31 + i * 7 + j / 253) as u8).collect();
                let path = dir.join(format!("slice-{i:03}.dar.age"));
                std::fs::write(&path, &bytes).unwrap();
                (path, n as u64, Some(sha(&bytes)))
            })
            .collect()
    }

    /// What the write session's execute loop does per entry: open the staged
    /// file, stream it through `write_verified` into the store, and stop at
    /// the first file that is not `Written`.
    fn write_in_turn(
        pool: &mut BufferPool,
        files: &[(PathBuf, u64, Option<String>)],
    ) -> (MemStore, Vec<Verdict>) {
        let mut mem = MemStore::new(CHUNK);
        let mut verdicts = Vec::new();
        for (path, len, expected) in files {
            let len = *len;
            let src = crate::util::DropBehind::open(path).unwrap();
            let streamed = write_verified(pool, src, len, expected.as_deref(), |s| {
                mem.execute(s, len, false)
            });
            let verdict = streamed.verdict(expected.as_deref());
            let written = matches!(verdict, Verdict::Written(_));
            verdicts.push(verdict);
            if !written {
                break;
            }
        }
        (mem, verdicts)
    }

    /// A pool carrying `files` across, as a research write with the knob set
    /// would; `gauged` gives it the trace's gauges too. Decided as a write
    /// with neither the trace nor another knob on, whatever this test
    /// process's environment holds.
    fn carrying(
        files: &[(PathBuf, u64, Option<String>)],
        buffers: usize,
        gauged: bool,
    ) -> BufferPool {
        let mut pool = BufferPool::new(CHUNK, buffers);
        pool.observe = Observe {
            decided: true,
            gauges: gauged.then(|| Arc::new(Gauges::default())),
            ..Observe::default()
        };
        pool.start_carry(files.to_vec());
        assert!(pool.observe.carry.is_some());
        pool
    }

    /// The carry writes, byte for byte and hash for hash, what the per-file
    /// pipeline writes: around block boundaries, an empty file, a one-byte
    /// one, through a queue of four buffers every file passes through, with
    /// and without the trace's gauges.
    #[test]
    fn the_carry_writes_what_the_per_file_pipeline_writes() {
        let dir = tempfile::TempDir::new().unwrap();
        let files = staged(
            dir.path(),
            &[3 * CHUNK + 17, 0, CHUNK, 9 * CHUNK + 1, 1, 2 * CHUNK],
        );
        let (per_file, per_file_verdicts) = write_in_turn(&mut BufferPool::new(CHUNK, 4), &files);
        assert_eq!(
            per_file.files.len(),
            files.len(),
            "positive control: the per-file pipeline wrote every file"
        );
        for gauged in [false, true] {
            let mut pool = carrying(&files, 4, gauged);
            let (carried, verdicts) = write_in_turn(&mut pool, &files);
            assert_eq!(carried.files, per_file.files, "gauged {gauged}");
            assert_eq!(verdicts.len(), files.len(), "gauged {gauged}");
            for ((v, p), (_, _, expected)) in verdicts.iter().zip(&per_file_verdicts).zip(&files) {
                match (v, p) {
                    (Verdict::Written(a), Verdict::Written(b)) => {
                        assert_eq!(a, b);
                        assert_eq!(Some(a), expected.as_ref());
                    }
                    other => panic!("gauged {gauged}: {other:?}"),
                }
            }
            let carry = pool.observe.carry.as_ref().unwrap();
            assert!(!carry.ended && carry.plan.is_empty());
            drop(pool); // both threads are joined
        }
    }

    /// Tri-layer L2 in the carry: a file whose bytes do not match its
    /// recorded hash is refused exactly as the per-file pipeline refuses it,
    /// the files before it stand, and nothing after it is written.
    #[test]
    fn a_mismatch_refuses_that_file_and_the_files_before_it_stand() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut files = staged(
            dir.path(),
            &[2 * CHUNK + 5, 4 * CHUNK, 3 * CHUNK + 1, CHUNK],
        );
        let actual = files[2].2.clone().unwrap();
        files[2].2 = Some("not-the-hash".into());
        let (per_file, per_file_verdicts) = write_in_turn(&mut BufferPool::new(CHUNK, 4), &files);
        let mut pool = carrying(&files, 4, true);
        let (carried, carried_verdicts) = write_in_turn(&mut pool, &files);
        for (name, mem, verdicts) in [
            ("per-file", &per_file, &per_file_verdicts),
            ("carried", &carried, &carried_verdicts),
        ] {
            assert_eq!(verdicts.len(), 3, "{name}: written, written, refused");
            assert!(
                matches!(verdicts[0], Verdict::Written(_))
                    && matches!(verdicts[1], Verdict::Written(_)),
                "{name}: {verdicts:?}"
            );
            match &verdicts[2] {
                Verdict::Mismatch(got) => assert_eq!(got, &actual, "{name}"),
                other => panic!("{name}: {other:?}"),
            }
            assert_eq!(
                mem.files.len(),
                2,
                "{name}: the refused file is not recorded"
            );
        }
        assert_eq!(carried.files, per_file.files);
        assert!(pool.observe.carry.as_ref().unwrap().ended);
        // The carry has ended: a further file is refused, never written.
        let (path, len, expected) = &files[3];
        let mut mem = MemStore::new(CHUNK);
        let streamed = write_verified(
            &mut pool,
            crate::util::DropBehind::open(path).unwrap(),
            *len,
            expected.as_deref(),
            |s| mem.execute(s, *len, false),
        );
        assert!(matches!(
            streamed.verdict(expected.as_deref()),
            Verdict::Failed(_)
        ));
        assert!(mem.files.is_empty());
        drop(pool);
    }

    /// A store that stops mid-file (a full medium) ends the carry: its error
    /// is the verdict, and dropping the pool joins both threads although the
    /// reader was ahead in later files with the queue full.
    #[test]
    fn a_store_that_stops_mid_file_ends_the_carry_and_every_thread_joins() {
        let dir = tempfile::TempDir::new().unwrap();
        let files = staged(dir.path(), &[5 * CHUNK, 40 * CHUNK, 40 * CHUNK, 40 * CHUNK]);
        let mut pool = carrying(&files, 4, false);
        let (mem, verdicts) = write_in_turn(&mut pool, &files[..1]);
        assert_eq!(verdicts.len(), 1);
        assert!(matches!(verdicts[0], Verdict::Written(_)));
        assert_eq!(mem.files.len(), 1);
        let (path, len, expected) = &files[1];
        let streamed = write_verified(
            &mut pool,
            crate::util::DropBehind::open(path).unwrap(),
            *len,
            expected.as_deref(),
            |src| {
                let mut block = [0u8; CHUNK];
                src.read_exact(&mut block)
                    .map_err(|e| TapectlError::TapeIo(e.to_string()))?;
                Err(TapectlError::TapeIo(
                    "write: No space left on device (os error 28)".into(),
                ))
            },
        );
        match streamed.verdict(expected.as_deref()) {
            Verdict::Failed(e) => assert!(e.to_string().contains("No space left"), "{e}"),
            other => panic!("{other:?}"),
        }
        assert!(pool.observe.carry.as_ref().unwrap().ended);
        drop(pool);
    }

    /// The carry's point: while the store has not yet asked for a file, its
    /// blocks are read and hashed into the queue.
    #[test]
    fn the_next_file_is_queued_before_its_write_begins() {
        let dir = tempfile::TempDir::new().unwrap();
        let files = staged(dir.path(), &[2 * CHUNK, 6 * CHUNK]);
        let mut pool = carrying(&files, 16, true);
        let gauges = pool.observe.carry.as_ref().unwrap().gauges.clone();
        let (first, verdicts) = write_in_turn(&mut pool, &files[..1]);
        assert!(matches!(verdicts[0], Verdict::Written(_)));
        let deadline = Instant::now() + Duration::from_secs(20);
        while gauges.hashed_unwritten.load(Ordering::Relaxed) < (6 * CHUNK) as u64 {
            assert!(
                Instant::now() < deadline,
                "file 1 was not read ahead: {} bytes queued",
                gauges.hashed_unwritten.load(Ordering::Relaxed)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let (rest, verdicts) = write_in_turn(&mut pool, &files[1..]);
        assert!(matches!(verdicts[0], Verdict::Written(_)));
        assert_eq!(first.files.len() + rest.files.len(), 2);
    }
}
