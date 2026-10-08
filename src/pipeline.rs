//! Overlapped block I/O (issue #390): `volume write`'s staged-file read, its
//! inline SHA-256 (tri-layer L2) and its tape write run on three threads
//! joined by a bounded queue, and a tape read runs on its own thread ahead of
//! whatever consumes it (confirm's hash, a restore's ciphertext file).
//!
//! **Why.** Until 1.0.6 one thread read a 512 KiB block of the staged file,
//! hashed it, wrote it to tape, and only then read the next block, so the
//! drive waited out every disk read and every hash. On a CPU without SHA-NI
//! the hash alone runs at about 191 MB/s (asm, one core) against the
//! 160 MB/s an LTO-6 needs to stream: L6-0001's 1.32 TB write took 5 h 22 m,
//! of which only about 49 minutes were spent inside tape write calls. With
//! each stage on its own thread the slowest stage sets the rate, not the sum
//! of all three.
//!
//! **The bound.** Every byte in flight lives in one of a [`BufferPool`]'s
//! buffers, and a pool never holds more than [`BufferPool::max_buffers`] of
//! them, each [`BufferPool::chunk`] bytes — the tape block size, so a buffer
//! is exactly one block for the store to take. The channels between stages
//! carry only those buffers (plus a closing message), so they need no bound
//! of their own: a producer that has used every buffer waits for its
//! consumer to hand one back. Memory therefore tracks the pool's constant
//! capacity ([`QUEUE_BYTES`]), never the size of the file (ADR-0006/0007:
//! RAM tracks the block size, not the slice size — now a fixed number of
//! blocks). Buffers are allocated as the queue first fills and then reused,
//! across files too: a pool lives as long as the write session's execute
//! loop, or the `TapeStore`.
//!
//! **Threads.** Scoped (`std::thread::scope`): every worker is joined before
//! the function that spawned it returns — on success, a store error, a hash
//! mismatch or a panic (a worker's panic is re-raised on the calling thread
//! after the join, as it would have unwound there before). Every stage stops
//! at the first failed send or receive, and the calling thread drops its ends
//! of the channels before it joins, so a consumer that stops early (a full
//! medium, a sink that fails) unblocks everything upstream of it. There is no
//! mid-file cancel, by design: Ctrl-C is still checked between files by the
//! write session, and a mid-file kill is still a crash for the startup sweep.
//!
//! **Progress.** Bytes are counted once, on the calling thread, where the
//! store takes them or the sink receives them — where they were counted
//! before, so a phase never claims bytes the tape has not taken. The workers
//! enter the caller's session ([`progress::Handle`]), so a slow staged-file
//! read or tape block on a worker still names itself in the session log.

use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{Scope, ScopedJoinHandle};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::error::{Result, TapectlError};
use crate::progress;

/// How many bytes a pipeline may hold in flight: 256 MiB, 512 of the
/// default 512 KiB tape blocks.
///
/// A constant, not a config key. It is about 1.6 s of an LTO-6 streaming at
/// 160 MB/s — enough to ride out the hash thread being descheduled by a
/// co-hosted VM or a staging-disk hiccup without the drive stopping, which
/// is all the queue is for; a deeper one only buys the same smoothing with
/// more memory. It is an eighth of the 2 GiB of available memory the quiet-host
/// check already demands before a write (`[host_check] min_available_mb`),
/// and one pipeline runs at a time (the write's, then the confirm's). No
/// operator decision depends on it, and a config key would be one more
/// surface for the unknown-key rule (ADR-0012); if a real-drive measurement
/// (#326) shows another size is needed, it is a one-line patch release.
pub const QUEUE_BYTES: u64 = 256 * 1024 * 1024;

/// The number of `chunk`-byte buffers a queue of `queue_bytes` holds — at
/// least two, one being filled while one is drained.
pub fn buffers_for(queue_bytes: u64, chunk: usize) -> usize {
    let n = queue_bytes / chunk.max(1) as u64;
    usize::try_from(n).unwrap_or(usize::MAX).max(2)
}

/// The reusable block buffers one pipeline at a time streams through. See
/// the module documentation: the pool's capacity is the pipeline's memory
/// bound.
pub struct BufferPool {
    chunk: usize,
    max_buffers: usize,
    /// Buffers that exist now: in `free`, or out in a running pipeline.
    allocated: usize,
    free: Vec<Vec<u8>>,
}

impl BufferPool {
    /// A pool of at most `max_buffers` (at least two) buffers of `chunk`
    /// bytes each. Nothing is allocated until a pipeline needs it.
    pub fn new(chunk: usize, max_buffers: usize) -> Self {
        Self {
            chunk: chunk.max(1),
            max_buffers: max_buffers.max(2),
            allocated: 0,
            free: Vec::new(),
        }
    }

    /// A pool of `chunk`-byte buffers holding at most `queue_bytes` —
    /// [`QUEUE_BYTES`] everywhere but the tests.
    pub fn with_queue_bytes(chunk: usize, queue_bytes: u64) -> Self {
        Self::new(chunk, buffers_for(queue_bytes, chunk))
    }

    /// The size of one buffer: one tape block.
    pub fn chunk(&self) -> usize {
        self.chunk
    }

    /// The most buffers this pool will ever hold at once.
    pub fn max_buffers(&self) -> usize {
        self.max_buffers
    }

    /// The bound itself: the most bytes of buffer this pool will ever hold.
    pub fn capacity_bytes(&self) -> u64 {
        self.chunk as u64 * self.max_buffers as u64
    }

    /// How many buffers exist right now.
    pub fn allocated(&self) -> usize {
        self.allocated
    }

    /// Take back what a finished pipeline returned. Its stages are joined and
    /// its channels dropped, so every buffer still alive is in `returned` or
    /// already in `free`; any other went with a stage that stopped.
    fn settle(&mut self, returned: Receiver<Vec<u8>>) {
        self.free.extend(returned.try_iter());
        self.allocated = self.free.len();
    }
}

/// What travels between stages.
enum Msg {
    /// A buffer holding this many bytes of the stream, in order.
    Data(Vec<u8>, usize),
    /// The stream ended (cleanly, or short: the consumer counts).
    End,
    /// The source failed; its own error, passed through untouched.
    Fail(io::Error),
    /// The hasher withheld the file's last block: its bytes do not match the
    /// recorded sha256 (tri-layer L2).
    Withheld,
}

/// The upstream end of a pipeline: the stage that fills buffers.
struct Producer<'p> {
    pool: &'p mut BufferPool,
    /// Buffers the consumer has finished with.
    returned: &'p mut Receiver<Vec<u8>>,
    tx: Sender<Msg>,
    /// Time spent waiting for a free buffer: the queue full.
    waited: Duration,
}

impl Producer<'_> {
    /// A buffer to fill: a reused one, a new one while the pool has room, or
    /// — the bound — the next one the consumer hands back. `None` when the
    /// consumer has gone, which is this stage's cue to stop.
    fn take(&mut self) -> Option<Vec<u8>> {
        if let Some(buf) = self.pool.free.pop() {
            return Some(buf);
        }
        match self.returned.try_recv() {
            Ok(buf) => return Some(buf),
            Err(TryRecvError::Disconnected) => return None,
            Err(TryRecvError::Empty) => {}
        }
        if self.pool.allocated < self.pool.max_buffers {
            self.pool.allocated += 1;
            return Some(vec![0u8; self.pool.chunk]);
        }
        let started = Instant::now();
        let buf = self.returned.recv().ok();
        self.waited += started.elapsed();
        buf
    }

    /// Pass `msg` downstream; `false` once there is no one to pass it to.
    fn send(&self, msg: Msg) -> bool {
        self.tx.send(msg).is_ok()
    }
}

/// Where the time went in one streamed file — what the session log's
/// per-file line reports, so a slow write says which side was slow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// The consumer (the tape writer; on a read, the hash or file behind the
    /// sink) waiting for the next block: the host behind the drive on a
    /// write, the drive behind the host on a read.
    pub consumer_waited: Duration,
    /// The producer waiting for a free buffer: the queue full, so something
    /// downstream of it — on a write, the hash OR the tape — is the slower.
    /// Only with `consumer_waited` does it name which: on a write, both high
    /// means the hash, `consumer_waited` near zero the tape.
    pub producer_waited: Duration,
}

// ── write: staged file → hash → store ──

/// One file streamed by [`write_verified`].
pub struct Streamed {
    /// What the store's `execute` returned.
    pub store: Result<u64>,
    /// The sha256 of exactly the declared length of source bytes; `None`
    /// when the source ended short or failed, or the store stopped before
    /// the hasher got to the end.
    pub sha256: Option<String>,
    /// The hasher found `sha256` disagreeing with the recorded hash and
    /// withheld the file's last block: the store met an error where that
    /// block should have been, so it wrote neither the block nor the
    /// filemark after it.
    pub withheld: bool,
    pub stats: Stats,
}

/// What one file's write came to, in the precedence the write session has
/// always applied (`session::run_entries`).
#[derive(Debug)]
pub enum Verdict {
    /// The store took the whole file and its bytes hash to the recorded
    /// sha256 (carried here).
    Written(String),
    /// The bytes hash to this instead of the recorded sha256: tri-layer L2's
    /// clean abort.
    Mismatch(String),
    /// The store failed for its own reason — a full medium, a drive error, a
    /// source that failed or ended short — before any mismatch reached it.
    Failed(TapectlError),
}

impl Streamed {
    /// The verdict against the recorded sha256 `expected`.
    ///
    /// A withheld block is a mismatch whatever the store made of it (the
    /// store's error is only the withheld block's echo). Otherwise a store
    /// error wins, exactly as before #390: a full medium mid-file is
    /// "execute failed", not a mismatch, even if the hasher, running ahead,
    /// had already finished. Only a store that took the whole file is judged
    /// on its hash — which is also how an empty file (no block to withhold)
    /// is judged.
    pub fn verdict(self, expected: Option<&str>) -> Verdict {
        if self.withheld {
            return Verdict::Mismatch(
                self.sha256
                    .expect("the hasher withholds a block only after finishing the hash"),
            );
        }
        if let Err(e) = self.store {
            return Verdict::Failed(e);
        }
        match self.sha256 {
            Some(actual) if expected == Some(actual.as_str()) => Verdict::Written(actual),
            Some(actual) => Verdict::Mismatch(actual),
            None => Verdict::Failed(TapectlError::Other(
                "write pipeline: the store took the whole file but the inline hash \
                 did not finish"
                    .into(),
            )),
        }
    }
}

/// Stream exactly `len` bytes of `src` into `execute` — the store's write of
/// one file — through the three-stage pipeline: a reader thread fills
/// buffers from `src`, a hasher thread hashes them in order and passes them
/// on, and `execute` runs on the calling thread reading them back as one
/// ordinary `Read`.
///
/// The hasher holds the file's **last** block until the hash is finished.
/// If it matches `expected_sha256` the block goes on, followed by the end of
/// the stream; if not, the block is dropped and the store's next read fails
/// instead, so the store never writes the last block or the filemark after
/// it ([`Streamed::withheld`]). An empty file has no block to hold back and
/// is judged after `execute` returns, as every file was before #390.
///
/// `execute` sees the bytes it would have read from `src` directly: the same
/// bytes in the same order, `src`'s own read error at the same offset, and
/// end of file where `src` ended (a short source is the store's own "source
/// exhausted" error, never a mismatch).
pub fn write_verified<R: Read + Send>(
    pool: &mut BufferPool,
    src: R,
    len: u64,
    expected_sha256: Option<&str>,
    execute: impl FnOnce(&mut dyn Read) -> Result<u64>,
) -> Streamed {
    let (free_tx, mut free_rx) = mpsc::channel::<Vec<u8>>();
    let (read_tx, read_rx) = mpsc::channel::<Msg>();
    let (hash_tx, hash_rx) = mpsc::channel::<Msg>();
    let expected = expected_sha256.map(str::to_owned);
    let session = progress::handle();

    let streamed = std::thread::scope(|s| {
        let reader = {
            let session = session.clone();
            let mut producer = Producer {
                pool: &mut *pool,
                returned: &mut free_rx,
                tx: read_tx,
                waited: Duration::ZERO,
            };
            spawn(s, "tapectl-stage-read", move || {
                let _in = session.enter();
                read_stage(&mut producer, src, len);
                producer.waited
            })
        };
        let hasher = spawn(s, "tapectl-hash", move || {
            let _in = session.enter();
            hash_stage(read_rx, hash_tx, len, expected.as_deref())
        });

        let mut pipe = PipeReader {
            rx: hash_rx,
            free: free_tx,
            current: None,
            ended: false,
            failed: None,
            withheld: false,
            waited: Duration::ZERO,
        };
        let store = execute(&mut pipe);
        let (withheld, consumer_waited) = (pipe.withheld, pipe.waited);
        // Drop our ends before joining: a stage still waiting on us — the
        // store stopped early — sees them go and stops.
        drop(pipe);
        let producer_waited = join(reader);
        let sha256 = join(hasher);
        Streamed {
            store,
            sha256,
            withheld,
            stats: Stats {
                consumer_waited,
                producer_waited,
            },
        }
    });
    pool.settle(free_rx);
    streamed
}

/// The reader stage: `len` bytes of `src`, a buffer at a time, then `End` —
/// or `src`'s own error, after whatever it delivered first.
fn read_stage<R: Read>(producer: &mut Producer<'_>, mut src: R, len: u64) {
    let chunk = producer.pool.chunk as u64;
    let mut remaining = len;
    while remaining > 0 {
        let Some(mut buf) = producer.take() else {
            return;
        };
        let want = remaining.min(chunk) as usize;
        let mut got = 0usize;
        while got < want {
            let started = Instant::now();
            let read = src.read(&mut buf[got..want]);
            progress::note_if_slow("one staged-file read", started.elapsed());
            match read {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    if got > 0 && !producer.send(Msg::Data(buf, got)) {
                        return;
                    }
                    producer.send(Msg::Fail(e));
                    return;
                }
            }
        }
        if got == 0 {
            // End of file on a block boundary, short of `len`.
            producer.pool.free.push(buf);
            break;
        }
        if !producer.send(Msg::Data(buf, got)) {
            return;
        }
        remaining -= got as u64;
        if got < want {
            break; // the source ended short
        }
    }
    producer.send(Msg::End);
}

/// The hasher stage. Returns the hash of exactly `len` bytes, or `None` if
/// the stream ended or failed short of them, or the store went away first.
fn hash_stage(
    rx: Receiver<Msg>,
    tx: Sender<Msg>,
    len: u64,
    expected: Option<&str>,
) -> Option<String> {
    let mut hasher = Sha256::new();
    let mut hashed = 0u64;
    loop {
        // A reader that vanished without an `End` panicked; the join re-raises it.
        let msg = rx.recv().ok()?;
        match msg {
            Msg::Data(buf, n) => {
                hasher.update(&buf[..n]);
                hashed += n as u64;
                if hashed == len {
                    // The last block: finish the hash before it goes on, so
                    // a mismatched file never reaches its filemark.
                    let actual = format!("{:x}", hasher.finalize());
                    if expected == Some(actual.as_str()) {
                        if tx.send(Msg::Data(buf, n)).is_ok() {
                            let _ = tx.send(Msg::End);
                        }
                    } else {
                        drop(buf);
                        let _ = tx.send(Msg::Withheld);
                    }
                    return Some(actual);
                }
                if tx.send(Msg::Data(buf, n)).is_err() {
                    return None;
                }
            }
            Msg::End => {
                let _ = tx.send(Msg::End);
                // Complete only for an empty file; anything else ended short.
                return (hashed == len).then(|| format!("{:x}", hasher.finalize()));
            }
            other @ (Msg::Fail(_) | Msg::Withheld) => {
                let _ = tx.send(other);
                return None;
            }
        }
    }
}

/// The calling thread's end of the write pipeline: the queue, read back as
/// one ordinary `Read` for the store.
struct PipeReader {
    rx: Receiver<Msg>,
    /// Where a drained buffer goes back to the reader stage.
    free: Sender<Vec<u8>>,
    /// The buffer being drained: (buffer, bytes in it, bytes drained).
    current: Option<(Vec<u8>, usize, usize)>,
    ended: bool,
    /// A failure already reported, repeated to any read after it.
    failed: Option<(io::ErrorKind, String)>,
    withheld: bool,
    waited: Duration,
}

impl PipeReader {
    fn next(&mut self) -> Msg {
        match self.rx.try_recv() {
            Ok(msg) => msg,
            Err(TryRecvError::Disconnected) => Msg::Fail(stopped()),
            Err(TryRecvError::Empty) => {
                let started = Instant::now();
                let msg = self.rx.recv().unwrap_or_else(|_| Msg::Fail(stopped()));
                self.waited += started.elapsed();
                msg
            }
        }
    }
}

impl Read for PipeReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if let Some((buf, len, pos)) = self.current.as_mut() {
                let n = (*len - *pos).min(out.len());
                out[..n].copy_from_slice(&buf[*pos..*pos + n]);
                *pos += n;
                if *pos == *len {
                    let (buf, _, _) = self.current.take().expect("just drained");
                    let _ = self.free.send(buf);
                }
                return Ok(n);
            }
            if let Some((kind, msg)) = &self.failed {
                return Err(io::Error::new(*kind, msg.clone()));
            }
            if self.ended {
                return Ok(0);
            }
            match self.next() {
                Msg::Data(buf, n) if n > 0 => self.current = Some((buf, n, 0)),
                Msg::Data(buf, _) => {
                    let _ = self.free.send(buf);
                }
                Msg::End => self.ended = true,
                Msg::Fail(e) => {
                    self.failed = Some((e.kind(), e.to_string()));
                    return Err(e);
                }
                Msg::Withheld => {
                    self.withheld = true;
                    let e = withheld();
                    self.failed = Some((e.kind(), e.to_string()));
                    return Err(e);
                }
            }
        }
    }
}

fn stopped() -> io::Error {
    io::Error::other("the write pipeline stopped before the end of the file")
}

fn withheld() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "the file's last block was withheld: its bytes do not match the recorded \
         sha256 (tri-layer L2)",
    )
}

// ── read: tape → sink ──

/// One file read by [`read_through`].
pub struct Delivered<T> {
    /// What the producer — the tape read — returned. It says how the read
    /// ended, whatever the sink did.
    pub produced: Result<T>,
    /// The sink's own error, if writing to it failed; the producer was then
    /// stopped at its next block.
    pub sink: io::Result<()>,
    pub stats: Stats,
}

/// Run `produce` — one tape file's read, writing its blocks to the `Write`
/// it is given — on a worker thread, and deliver everything it writes to
/// `sink` on the calling thread, through the pool's bounded queue. The read
/// runs ahead of the sink by up to the pool's capacity, so the drive keeps
/// reading while the sink hashes or writes to disk.
///
/// If the sink fails, the calling thread stops draining and drops its ends;
/// the producer's next write fails ("the reader of this tape file stopped")
/// and it returns that error. Either way the producer's own result comes
/// back in [`Delivered::produced`], because it — not the sink — knows how far
/// the tape moved (issue #389's cursor).
pub fn read_through<T: Send>(
    pool: &mut BufferPool,
    produce: impl FnOnce(&mut dyn Write) -> Result<T> + Send,
    sink: &mut dyn Write,
) -> Delivered<T> {
    let (free_tx, mut free_rx) = mpsc::channel::<Vec<u8>>();
    let (tx, rx) = mpsc::channel::<Msg>();
    let session = progress::handle();

    let delivered = std::thread::scope(|s| {
        let mut producer = Producer {
            pool: &mut *pool,
            returned: &mut free_rx,
            tx,
            waited: Duration::ZERO,
        };
        let worker = spawn(s, "tapectl-tape-read", move || {
            let _in = session.enter();
            let mut pipe = PipeWriter {
                producer: &mut producer,
                current: None,
            };
            let produced = produce(&mut pipe);
            pipe.finish();
            (produced, producer.waited)
        });

        let mut sink_result = Ok(());
        let mut consumer_waited = Duration::ZERO;
        loop {
            let msg = match rx.try_recv() {
                Ok(msg) => msg,
                Err(TryRecvError::Disconnected) => break,
                Err(TryRecvError::Empty) => {
                    let started = Instant::now();
                    let msg = rx.recv();
                    consumer_waited += started.elapsed();
                    match msg {
                        Ok(msg) => msg,
                        Err(_) => break,
                    }
                }
            };
            match msg {
                Msg::Data(buf, n) => {
                    if let Err(e) = sink.write_all(&buf[..n]) {
                        sink_result = Err(e);
                        break;
                    }
                    let _ = free_tx.send(buf);
                }
                Msg::End | Msg::Fail(_) | Msg::Withheld => break,
            }
        }
        // Drop our ends before joining: a producer waiting on us stops.
        drop(rx);
        drop(free_tx);
        let (produced, producer_waited) = join(worker);
        Delivered {
            produced,
            sink: sink_result,
            stats: Stats {
                consumer_waited,
                producer_waited,
            },
        }
    });
    pool.settle(free_rx);
    delivered
}

/// The producer's end of the read pipeline: a `Write` that packs what it is
/// given into the pool's buffers and queues each one as it fills.
struct PipeWriter<'a, 'p> {
    producer: &'a mut Producer<'p>,
    /// The buffer being filled, and how many bytes it holds.
    current: Option<(Vec<u8>, usize)>,
}

impl PipeWriter<'_, '_> {
    /// Queue the last, partly filled buffer and close the stream.
    fn finish(mut self) {
        if let Some((buf, fill)) = self.current.take() {
            if fill > 0 && !self.producer.send(Msg::Data(buf, fill)) {
                return;
            }
        }
        self.producer.send(Msg::End);
    }
}

impl Write for PipeWriter<'_, '_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let mut done = 0;
        while done < data.len() {
            if self.current.is_none() {
                let buf = self.producer.take().ok_or_else(consumer_gone)?;
                self.current = Some((buf, 0));
            }
            let (buf, fill) = self.current.as_mut().expect("just filled");
            let n = (buf.len() - *fill).min(data.len() - done);
            buf[*fill..*fill + n].copy_from_slice(&data[done..done + n]);
            *fill += n;
            done += n;
            if *fill == buf.len() {
                let (buf, fill) = self.current.take().expect("just filled");
                if !self.producer.send(Msg::Data(buf, fill)) {
                    return Err(consumer_gone());
                }
            }
        }
        Ok(done)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn consumer_gone() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "the reader of this tape file stopped",
    )
}

// ── threads ──

fn spawn<'scope, 'env, T: Send + 'scope>(
    s: &'scope Scope<'scope, 'env>,
    name: &str,
    f: impl FnOnce() -> T + Send + 'scope,
) -> ScopedJoinHandle<'scope, T> {
    // Issue #344: a stage works for the caller's contact, so what a tape
    // device notes on it — a failed read's MTIOCGET status, on the tape-read
    // thread — is that contact's.
    let contact = crate::tape::mtget_journal::handle();
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn_scoped(s, move || {
            let _for = contact.enter();
            f()
        })
        .unwrap_or_else(|e| panic!("cannot start the {name} thread: {e}"))
}

/// Join a stage; a stage that panicked panics the caller, as the same code
/// would have when it ran on the caller's thread.
fn join<T>(handle: ScopedJoinHandle<'_, T>) -> T {
    match handle.join() {
        Ok(value) => value,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{MemStore, Store};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex};

    const CHUNK: usize = 16;

    fn sha(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    /// `len` bytes that differ block to block, so a reordered, dropped or
    /// duplicated block cannot compare equal.
    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    fn padded(bytes: &[u8], block: usize) -> Vec<u8> {
        let mut v = bytes.to_vec();
        v.resize(bytes.len().div_ceil(block) * block, 0);
        v
    }

    fn pad(len: u64, bs: usize) -> u64 {
        len.div_ceil(bs as u64) * bs as u64
    }

    /// A source that counts what has been read from it (with a condvar to
    /// wait on), can fail at an offset like a bad disk, and says when it is
    /// dropped — which, the reader thread owning it, means that thread has
    /// finished.
    struct Source {
        bytes: Vec<u8>,
        at: usize,
        fail_at: Option<usize>,
        progress: Arc<(Mutex<usize>, Condvar)>,
        dropped: Arc<AtomicBool>,
    }

    impl Source {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                bytes,
                at: 0,
                fail_at: None,
                progress: Arc::new((Mutex::new(0), Condvar::new())),
                dropped: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    impl Read for Source {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.fail_at.is_some_and(|f| self.at >= f) {
                return Err(io::Error::from_raw_os_error(5));
            }
            let end = self
                .bytes
                .len()
                .min(self.at + buf.len())
                .min(self.fail_at.unwrap_or(usize::MAX));
            let n = end - self.at;
            buf[..n].copy_from_slice(&self.bytes[self.at..end]);
            self.at = end;
            let (count, cv) = &*self.progress;
            *count.lock().unwrap() = self.at;
            cv.notify_all();
            Ok(n)
        }
    }

    impl Drop for Source {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    /// The store's side of a tape write, done the way
    /// `TapeDevice::write_stream` does it: `want` bytes per block (however
    /// many reads that takes), the last zero-padded, each block written —
    /// then the filemark. Each block and the filemark are recorded as they
    /// would reach the tape.
    #[derive(Default)]
    struct TapeLike {
        blocks: Vec<Vec<u8>>,
        filemark: bool,
    }

    impl TapeLike {
        fn execute(&mut self, src: &mut dyn Read, len: u64, bs: usize) -> Result<u64> {
            let mut remaining = len;
            while remaining > 0 {
                let want = remaining.min(bs as u64) as usize;
                let mut buf = vec![0u8; bs];
                let mut got = 0;
                while got < want {
                    let n = src
                        .read(&mut buf[got..want])
                        .map_err(|e| TapectlError::TapeIo(format!("read source: {e}")))?;
                    if n == 0 {
                        return Err(TapectlError::TapeIo(format!(
                            "source exhausted after {got} of {want} bytes wanted"
                        )));
                    }
                    got += n;
                }
                self.blocks.push(buf);
                remaining -= want as u64;
            }
            self.filemark = true;
            Ok(pad(len, bs))
        }
    }

    // ── the write pipeline ──

    /// What the store receives through the pipeline is exactly the source,
    /// at every size around the block boundaries, through a queue of only
    /// two buffers — and the hash is of exactly those bytes.
    #[test]
    fn the_store_receives_the_source_unchanged_at_every_size() {
        let mut pool = BufferPool::new(CHUNK, 2);
        for len in [0usize, 1, 15, 16, 17, 48, 100, 1000] {
            let source = pattern(len);
            let want = sha(&source);

            let mut mem = MemStore::new(CHUNK);
            let streamed = write_verified(
                &mut pool,
                Source::new(source.clone()),
                len as u64,
                Some(&want),
                |src| mem.execute(src, len as u64, false),
            );
            assert_eq!(streamed.store.as_ref().ok(), Some(&pad(len as u64, CHUNK)));
            assert!(!streamed.withheld);
            match streamed.verdict(Some(&want)) {
                Verdict::Written(actual) => assert_eq!(actual, want, "len {len}"),
                other => panic!("len {len}: {other:?}"),
            }
            assert_eq!(mem.files, vec![padded(&source, CHUNK)], "len {len}");

            // The same through a `write_stream`-shaped consumer: block for
            // block what it builds reading the source directly.
            let mut direct = TapeLike::default();
            direct.execute(&mut &source[..], len as u64, CHUNK).unwrap();
            let mut piped = TapeLike::default();
            let streamed = write_verified(
                &mut pool,
                Source::new(source.clone()),
                len as u64,
                Some(&want),
                |src| piped.execute(src, len as u64, CHUNK),
            );
            assert!(streamed.store.is_ok(), "len {len}");
            assert_eq!(piped.blocks, direct.blocks, "len {len}");
            assert!(piped.filemark);
        }
        assert!(pool.allocated() <= 2, "{}", pool.allocated());
    }

    /// Tri-layer L2, the #390 ordering: a file whose bytes do not match
    /// reaches the store whole but for its last block, and the store's
    /// write ends in an error where that block should be — so neither it nor
    /// the filemark after it is ever written.
    #[test]
    fn a_mismatch_withholds_the_last_block_and_the_filemark() {
        let mut pool = BufferPool::new(CHUNK, 3);
        let source = pattern(100); // 7 blocks, the last partial
        let mut tape = TapeLike::default();
        let streamed = write_verified(
            &mut pool,
            Source::new(source.clone()),
            100,
            Some("not-the-hash"),
            |src| tape.execute(src, 100, CHUNK),
        );
        assert!(streamed.withheld);
        let err = streamed.store.as_ref().unwrap_err().to_string();
        assert!(err.contains("withheld"), "{err}");
        assert_eq!(tape.blocks.len(), 6, "every block but the last");
        assert_eq!(tape.blocks.concat(), source[..96].to_vec(), "in order");
        assert!(!tape.filemark, "no filemark after a mismatched file");
        match streamed.verdict(Some("not-the-hash")) {
            Verdict::Mismatch(actual) => assert_eq!(actual, sha(&source)),
            other => panic!("{other:?}"),
        }
    }

    /// A one-block file that does not match reaches the store not at all.
    #[test]
    fn a_mismatched_one_block_file_reaches_the_store_not_at_all() {
        let mut pool = BufferPool::new(CHUNK, 2);
        let mut mem = MemStore::new(CHUNK);
        let streamed = write_verified(&mut pool, Source::new(pattern(10)), 10, None, |src| {
            mem.execute(src, 10, false)
        });
        assert!(streamed.withheld);
        assert!(mem.files.is_empty(), "MemStore recorded nothing");
        assert!(matches!(streamed.verdict(None), Verdict::Mismatch(_)));
    }

    /// An empty file has no block to hold back: the store writes its empty
    /// file and the verdict comes after, exactly as before #390.
    #[test]
    fn an_empty_file_is_judged_after_the_store_as_before() {
        let mut pool = BufferPool::new(CHUNK, 2);
        let mut mem = MemStore::new(CHUNK);
        let streamed = write_verified(
            &mut pool,
            Source::new(Vec::new()),
            0,
            Some("not-the-hash"),
            |src| mem.execute(src, 0, false),
        );
        assert!(!streamed.withheld);
        assert!(streamed.store.is_ok());
        match streamed.verdict(Some("not-the-hash")) {
            Verdict::Mismatch(actual) => assert_eq!(actual, sha(b"")),
            other => panic!("{other:?}"),
        }
    }

    /// A source shorter than its declared length is the store's own "source
    /// exhausted" error — never a hash mismatch, as before #390.
    #[test]
    fn a_short_source_is_the_stores_error_not_a_mismatch() {
        let mut pool = BufferPool::new(CHUNK, 2);
        let mut mem = MemStore::new(CHUNK);
        let source = Source::new(pattern(60));
        let dropped = source.dropped.clone();
        let streamed = write_verified(&mut pool, source, 100, Some("x"), |src| {
            mem.execute(src, 100, false)
        });
        assert!(!streamed.withheld);
        assert_eq!(streamed.sha256, None);
        match streamed.verdict(Some("x")) {
            Verdict::Failed(e) => assert!(
                e.to_string().contains("source exhausted after 60 of 100"),
                "{e}"
            ),
            other => panic!("{other:?}"),
        }
        assert!(dropped.load(Ordering::SeqCst));
    }

    /// A disk read error reaches the store as itself, at the offset it
    /// happened, so the store's message is the one it always gave — and the
    /// reader thread has finished by the time the call returns.
    #[test]
    fn a_source_read_error_reaches_the_store_as_itself_and_every_thread_joins() {
        let mut pool = BufferPool::new(CHUNK, 4);
        let mut source = Source::new(pattern(1000));
        source.fail_at = Some(40);
        let dropped = source.dropped.clone();
        let mut tape = TapeLike::default();
        let streamed = write_verified(&mut pool, source, 1000, Some("x"), |src| {
            tape.execute(src, 1000, CHUNK)
        });
        assert!(dropped.load(Ordering::SeqCst), "the reader thread finished");
        assert_eq!(tape.blocks.len(), 2, "the whole blocks before the error");
        assert!(!tape.filemark);
        match streamed.verdict(Some("x")) {
            Verdict::Failed(e) => assert_eq!(
                e.to_string(),
                TapectlError::TapeIo(format!("read source: {}", io::Error::from_raw_os_error(5)))
                    .to_string()
            ),
            other => panic!("{other:?}"),
        }
    }

    /// A store that stops mid-file (a full medium) stops the whole pipeline:
    /// its error is the verdict, the reader stops within a queue of where
    /// the store stopped, and every thread is joined.
    #[test]
    fn a_store_that_stops_mid_file_stops_the_pipeline() {
        const BUFFERS: usize = 4;
        let mut pool = BufferPool::new(CHUNK, BUFFERS);
        let source = Source::new(pattern(CHUNK * 1000));
        let (progress, dropped) = (source.progress.clone(), source.dropped.clone());
        let streamed = write_verified(&mut pool, source, (CHUNK * 1000) as u64, Some("x"), |src| {
            let mut block = [0u8; CHUNK];
            src.read_exact(&mut block).unwrap();
            Err(TapectlError::TapeIo(
                "write: No space left on device (os error 28)".into(),
            ))
        });
        assert!(dropped.load(Ordering::SeqCst));
        let read = *progress.0.lock().unwrap();
        assert!(
            read <= (1 + BUFFERS) * CHUNK,
            "the reader got {read} bytes ahead of a store that took {CHUNK}"
        );
        match streamed.verdict(Some("x")) {
            Verdict::Failed(e) => assert!(e.to_string().contains("No space left"), "{e}"),
            other => panic!("{other:?}"),
        }
        assert!(pool.allocated() <= BUFFERS);
    }

    /// THE bound: while the store has taken nothing, the reader fills
    /// exactly the pool's buffers and then waits; the whole file then
    /// passes through those same buffers. Sixty-four blocks, four buffers.
    #[test]
    fn the_queue_never_holds_more_than_its_buffers() {
        const BUFFERS: usize = 4;
        const BLOCKS: usize = 64;
        let mut pool = BufferPool::new(CHUNK, BUFFERS);
        let bytes = pattern(CHUNK * BLOCKS);
        let source = Source::new(bytes.clone());
        let progress = source.progress.clone();
        let mut mem = MemStore::new(CHUNK);
        let streamed = write_verified(
            &mut pool,
            source,
            bytes.len() as u64,
            Some(&sha(&bytes)),
            |src| {
                let (count, cv) = &*progress;
                let full = BUFFERS * CHUNK;
                let guard = cv
                    .wait_timeout_while(count.lock().unwrap(), Duration::from_secs(20), |n| {
                        *n < full
                    })
                    .unwrap()
                    .0;
                assert_eq!(*guard, full, "the reader fills the whole queue");
                // ...and goes no further while nothing is taken from it.
                let (guard, waited) = cv
                    .wait_timeout_while(guard, Duration::from_millis(200), |n| *n == full)
                    .unwrap();
                assert!(waited.timed_out(), "read {} past a full queue", *guard);
                drop(guard);
                mem.execute(src, (CHUNK * BLOCKS) as u64, false)
            },
        );
        assert!(matches!(
            streamed.verdict(Some(&sha(&bytes))),
            Verdict::Written(_)
        ));
        assert_eq!(mem.files, vec![bytes]);
        assert_eq!(pool.allocated(), BUFFERS, "reused, never more");
    }

    /// Buffers outlive one file: a pool reused across files never holds
    /// more than its bound.
    #[test]
    fn buffers_are_reused_across_files() {
        let mut pool = BufferPool::new(CHUNK, 3);
        for len in [500usize, 40, 900, 0, 17] {
            let source = pattern(len);
            let want = sha(&source);
            let mut mem = MemStore::new(CHUNK);
            let streamed = write_verified(
                &mut pool,
                Source::new(source.clone()),
                len as u64,
                Some(&want),
                |src| mem.execute(src, len as u64, false),
            );
            assert!(matches!(streamed.verdict(Some(&want)), Verdict::Written(_)));
            assert!(pool.allocated() <= 3, "{}", pool.allocated());
        }
    }

    /// The production bound: 256 MiB of 512 KiB tape blocks is 512
    /// buffers, nothing is allocated before a pipeline needs it, and no
    /// pool has fewer than two buffers.
    #[test]
    fn the_default_queue_is_256_mib_of_tape_blocks() {
        assert_eq!(QUEUE_BYTES, 256 * 1024 * 1024);
        let pool = BufferPool::with_queue_bytes(512 * 1024, QUEUE_BYTES);
        assert_eq!(pool.max_buffers(), 512);
        assert_eq!(pool.chunk(), 512 * 1024);
        assert_eq!(pool.capacity_bytes(), QUEUE_BYTES);
        assert_eq!(pool.allocated(), 0);
        assert_eq!(buffers_for(QUEUE_BYTES, 1024 * 1024 * 1024), 2);
    }

    /// A worker's slow staged-file read reaches the caller's session log.
    #[test]
    fn a_slow_read_on_the_reader_thread_reaches_the_session_log() {
        struct Slow(Vec<u8>, usize);
        impl Read for Slow {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                // No five-second sleep: a direct note past the threshold
                // proves the routing, which is what is under test.
                if self.1 == 0 {
                    progress::note_if_slow("a staged read in the test", Duration::from_secs(9));
                }
                let n = (self.0.len() - self.1).min(buf.len());
                buf[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
                self.1 += n;
                Ok(n)
            }
        }
        let dir = tempfile::TempDir::new().unwrap();
        let session = progress::start_session(Some(dir.path()), "t", progress::Display::Off, false);
        let path = session.log_path().unwrap().to_path_buf();
        let mut pool = BufferPool::new(CHUNK, 2);
        let source = pattern(40);
        let mut mem = MemStore::new(CHUNK);
        let streamed = write_verified(
            &mut pool,
            Slow(source.clone(), 0),
            40,
            Some(&sha(&source)),
            |src| mem.execute(src, 40, false),
        );
        assert!(streamed.store.is_ok());
        drop(session);
        let text = std::fs::read_to_string(path).unwrap();
        assert!(
            text.contains("slow: a staged read in the test took 9.0 s"),
            "{text}"
        );
    }

    // ── the read pipeline ──

    /// Everything the producer writes reaches the sink, in order, whatever
    /// sizes it writes in, and the producer's own result comes back.
    #[test]
    fn a_read_delivers_every_byte_and_the_producers_result() {
        let mut pool = BufferPool::new(CHUNK, 2);
        let source = pattern(1000);
        let mut sink = Vec::new();
        let delivered = read_through(
            &mut pool,
            |pipe| {
                for piece in source.chunks(37) {
                    pipe.write_all(piece).unwrap();
                }
                Ok(("done", source.len()))
            },
            &mut sink,
        );
        assert_eq!(delivered.produced.unwrap(), ("done", 1000));
        assert!(delivered.sink.is_ok());
        assert_eq!(sink, source);
        assert!(pool.allocated() <= 2);
    }

    /// A producer error comes back as itself, after the sink got what came
    /// before it.
    #[test]
    fn a_producer_error_comes_back_after_what_it_delivered() {
        let mut pool = BufferPool::new(CHUNK, 2);
        let mut sink = Vec::new();
        let delivered: Delivered<()> = read_through(
            &mut pool,
            |pipe| {
                pipe.write_all(&[7u8; CHUNK * 2]).unwrap();
                Err(TapectlError::TapeIo(
                    "read: Input/output error (os error 5)".into(),
                ))
            },
            &mut sink,
        );
        assert_eq!(
            delivered.produced.unwrap_err().to_string(),
            TapectlError::TapeIo("read: Input/output error (os error 5)".into()).to_string()
        );
        assert!(delivered.sink.is_ok());
        assert_eq!(sink, vec![7u8; CHUNK * 2]);
    }

    /// Issue #344: the tape read runs on the `tapectl-tape-read` worker, and
    /// the MTIOCGET reading a failed read notes there belongs to the contact
    /// open on the CALLING thread. The worker carries that contact's buffer,
    /// so the reading is kept, not noted into a thread with no contact.
    #[test]
    fn a_reading_noted_on_the_tape_read_thread_reaches_the_callers_contact() {
        use crate::tape::mtget_journal;
        let _ = mtget_journal::take();
        mtget_journal::begin();
        let mut pool = BufferPool::new(CHUNK, 2);
        let mut sink = Vec::new();
        let delivered: Delivered<()> = read_through(
            &mut pool,
            |_pipe| {
                mtget_journal::note(
                    mtget_journal::POINT_FAILURE,
                    "/dev/nst9",
                    Some("read"),
                    Some(5),
                    Ok(mtget_journal::MtStatus::default()),
                );
                Err(TapectlError::TapeIo("read: EIO".into()))
            },
            &mut sink,
        );
        assert!(delivered.produced.is_err());
        let kept = mtget_journal::take();
        assert_eq!(kept.len(), 1, "the worker's reading reached the contact");
        assert_eq!(kept[0].command.as_deref(), Some("read"));
    }

    /// A sink that fails stops the producer at its next block — it does not
    /// read the rest of the file into a queue nobody drains — and both
    /// errors come back: the sink's, and the producer's own account.
    #[test]
    fn a_failing_sink_stops_the_producer() {
        struct FailsAfter(usize);
        impl Write for FailsAfter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if self.0 == 0 {
                    return Err(io::Error::from_raw_os_error(28));
                }
                self.0 -= 1;
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        const BUFFERS: usize = 3;
        let mut pool = BufferPool::new(CHUNK, BUFFERS);
        let mut written = 0usize;
        let delivered = read_through(
            &mut pool,
            |pipe| {
                for _ in 0..10_000 {
                    pipe.write_all(&[1u8; CHUNK])
                        .map_err(|e| TapectlError::TapeIo(format!("sink write: {e}")))?;
                    written += 1;
                }
                Ok(())
            },
            &mut FailsAfter(2),
        );
        assert_eq!(delivered.sink.unwrap_err().raw_os_error(), Some(28));
        let err = delivered.produced.unwrap_err().to_string();
        assert!(
            err.contains("sink write: the reader of this tape file stopped"),
            "{err}"
        );
        assert!(
            written <= 2 + BUFFERS,
            "the producer wrote {written} blocks for a sink that took 2"
        );
    }

    /// The read side's bound: a producer whose sink is still busy with the
    /// first block gets the pool's buffers ahead and no further.
    #[test]
    fn a_read_runs_ahead_by_the_queue_and_no_further() {
        const BUFFERS: usize = 4;
        struct Gate(Arc<(Mutex<usize>, Condvar)>, bool);
        impl Write for Gate {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if !self.1 {
                    self.1 = true;
                    let (count, cv) = &*self.0;
                    let guard = cv
                        .wait_timeout_while(count.lock().unwrap(), Duration::from_secs(20), |n| {
                            *n < BUFFERS
                        })
                        .unwrap()
                        .0;
                    assert_eq!(*guard, BUFFERS, "the producer fills the queue");
                    let (guard, waited) = cv
                        .wait_timeout_while(guard, Duration::from_millis(200), |n| *n == BUFFERS)
                        .unwrap();
                    assert!(waited.timed_out(), "wrote {} past a full queue", *guard);
                }
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut pool = BufferPool::new(CHUNK, BUFFERS);
        let progress = Arc::new((Mutex::new(0usize), Condvar::new()));
        let counter = progress.clone();
        let delivered = read_through(
            &mut pool,
            move |pipe| {
                for _ in 0..50 {
                    pipe.write_all(&[3u8; CHUNK]).unwrap();
                    let (count, cv) = &*counter;
                    *count.lock().unwrap() += 1;
                    cv.notify_all();
                }
                Ok(())
            },
            &mut Gate(progress, false),
        );
        assert!(delivered.produced.is_ok() && delivered.sink.is_ok());
        assert_eq!(pool.allocated(), BUFFERS);
    }

    // ── the overlap budget (issue #416 item 6) ──
    //
    // #390's whole point is that the stages run at once: the slowest one
    // sets the rate, not the sum of all of them. Each test below gives two
    // stages the same per-block delay and holds the wall time to at most
    // 0.7 of the time the stages spent busy, summed — overlapped, it is
    // about half; serial, it is all of it. Busy time is measured around
    // each sleep, so a loaded host that oversleeps inflates both sides.

    const OVERLAP_BLOCKS: usize = 30;
    const OVERLAP_DELAY: Duration = Duration::from_millis(8);
    const OVERLAP_BUDGET: f64 = 0.7;

    /// Sleep one block's delay, adding what it took to `busy`.
    fn busy_block(busy: &Mutex<Duration>) {
        let started = Instant::now();
        std::thread::sleep(OVERLAP_DELAY);
        *busy.lock().unwrap() += started.elapsed();
    }

    /// A source that takes [`OVERLAP_DELAY`] to deliver each block.
    struct SlowSource {
        left: usize,
        busy: Arc<Mutex<Duration>>,
    }

    impl Read for SlowSource {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.left == 0 {
                return Ok(0);
            }
            busy_block(&self.busy);
            let n = buf.len().min(CHUNK);
            buf[..n].fill(7);
            self.left -= 1;
            Ok(n)
        }
    }

    /// A sink that takes [`OVERLAP_DELAY`] to take each block.
    struct SlowSink(Arc<Mutex<Duration>>);

    impl Write for SlowSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            busy_block(&self.0);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn overlap(wall: Duration, a: &Mutex<Duration>, b: &Mutex<Duration>) -> f64 {
        wall.as_secs_f64() / (*a.lock().unwrap() + *b.lock().unwrap()).as_secs_f64()
    }

    /// The write pipeline: a staged file that is slow to read and a store
    /// that is slow to write overlap.
    #[test]
    fn a_slow_source_and_a_slow_store_overlap() {
        let len = (OVERLAP_BLOCKS * CHUNK) as u64;
        let source_busy = Arc::new(Mutex::new(Duration::ZERO));
        let store_busy = Arc::new(Mutex::new(Duration::ZERO));
        let want = sha(&vec![7u8; len as usize]);
        let mut pool = BufferPool::new(CHUNK, 4);
        let started = Instant::now();
        let streamed = write_verified(
            &mut pool,
            SlowSource {
                left: OVERLAP_BLOCKS,
                busy: source_busy.clone(),
            },
            len,
            Some(&want),
            |src| {
                let mut block = [0u8; CHUNK];
                for _ in 0..OVERLAP_BLOCKS {
                    src.read_exact(&mut block).unwrap();
                    busy_block(&store_busy);
                }
                Ok(len)
            },
        );
        let wall = started.elapsed();
        assert!(streamed.store.is_ok());
        let ratio = overlap(wall, &source_busy, &store_busy);
        assert!(
            ratio <= OVERLAP_BUDGET,
            "the source and the store took turns: wall {wall:?} is {ratio:.2} of their busy time"
        );
    }

    /// The read pipeline: a tape read that is slow to deliver and a sink
    /// that is slow to take overlap.
    #[test]
    fn a_slow_tape_read_and_a_slow_sink_overlap() {
        let tape_busy = Arc::new(Mutex::new(Duration::ZERO));
        let sink_busy = Arc::new(Mutex::new(Duration::ZERO));
        let mut pool = BufferPool::new(CHUNK, 4);
        let busy = tape_busy.clone();
        let started = Instant::now();
        let delivered = read_through(
            &mut pool,
            move |pipe| {
                for _ in 0..OVERLAP_BLOCKS {
                    busy_block(&busy);
                    pipe.write_all(&[5u8; CHUNK]).unwrap();
                }
                Ok(())
            },
            &mut SlowSink(sink_busy.clone()),
        );
        let wall = started.elapsed();
        assert!(delivered.produced.is_ok() && delivered.sink.is_ok());
        let ratio = overlap(wall, &tape_busy, &sink_busy);
        assert!(
            ratio <= OVERLAP_BUDGET,
            "the tape read and the sink took turns: wall {wall:?} is {ratio:.2} of their busy time"
        );
    }

    /// The overlap measure's positive control: the same two stages run in
    /// turn on one thread, as every release before 1.0.6 ran them, are
    /// over the budget — the measure sees serial work as serial.
    #[test]
    fn the_same_stages_in_turn_are_over_the_overlap_budget() {
        let a = Mutex::new(Duration::ZERO);
        let b = Mutex::new(Duration::ZERO);
        let started = Instant::now();
        for _ in 0..OVERLAP_BLOCKS {
            busy_block(&a);
            busy_block(&b);
        }
        let ratio = overlap(started.elapsed(), &a, &b);
        assert!(ratio > OVERLAP_BUDGET, "serial ratio {ratio:.2}");
    }
}
