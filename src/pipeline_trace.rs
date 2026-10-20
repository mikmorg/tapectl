//! The write pipeline's trace and host sampler (issue #488; the write-shaping
//! note, `docs/research/2026-10-20-write-shaping.md` §9 items 1 and 2).
//!
//! **Why.** A contended production write ran at 105 MB/s and used 1.505
//! native bytes of tape per data byte (#426), and nothing recorded could say
//! which stage stalled: the session log has one line per 1 GiB file. This
//! module samples the pipeline and the host every [`SAMPLE_INTERVAL`] into
//! `<home>/logs/<session>.pipeline.tsv`, beside the session log
//! (`crate::progress` names that log), one tab-separated row per sample under
//! a header row ([`COLUMNS`]).
//!
//! **Off by default, in every build.** `TAPECTL_PIPELINE_TRACE=1` turns it on
//! ([`TRACE_ENV`], read once per process). Observing changes nothing the write
//! does: the write's own threads only bump atomics ([`Gauges`]), and one
//! sampler thread does every `/proc` and sysfs read, formats, and writes the
//! file. With the trace off nothing here runs at all (the pipeline holds no
//! [`Gauges`]). If the trace cannot start — no session log to sit beside, or
//! the file cannot be created — the write goes on without it and the session
//! log says why, once.
//!
//! **Its lifetime is the write pipeline's pool** (`pipeline::BufferPool`):
//! started by the first file the pool writes, stopped (with a last, partial
//! row) when the pool is dropped. The write session keeps one pool for its
//! whole execute loop, so one trace covers every file of a write without a
//! hook in the session; a resumed write in the same process appends to the
//! same file (the header is written only into an empty one).
//!
//! **What the columns mean** is [`COLUMNS`]' documentation. Two are not
//! literally what §9 names, and say so: the store's time per block is
//! measured as the gap between the store's successive reads of the queue
//! (the store reads a block, writes it with one `write(2)`, reads the next),
//! so it is `write(2)` plus a block copy; and the staging disks are found
//! from the path the pipeline was told it reads ([`Gauges::set_source`],
//! `BufferPool::trace_source`), which only a caller that knows the path can
//! give — until the write session calls it, the disk columns are empty.
//!
//! **Cost.** Per 512 KiB block on the tape thread: two `Instant::now()` and a
//! handful of relaxed atomic adds. Per sample on the sampler thread: one
//! read each of `/proc/stat`, `/proc/schedstat`, `/proc/diskstats`,
//! `/proc/loadavg`, `/proc/self/status`, and per traced thread its
//! `schedstat` and `stat`.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};

use crate::progress;

/// The environment variable that turns the trace on: exactly `1`.
pub const TRACE_ENV: &str = "TAPECTL_PIPELINE_TRACE";

/// One row per this much time.
pub const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

/// The trace file's name beside the session log `<session>.log`.
pub const TRACE_SUFFIX: &str = ".pipeline.tsv";

/// The header row, one name per column, in order. Times are milliseconds
/// within the row's interval unless named otherwise; byte counts are bytes.
///
/// - `t_s`: seconds since the process's first trace started (monotonic);
///   `utc`: the wall clock, to join with the session log.
/// - `file`: the file being written — the name of the path the pipeline was
///   told it reads, else `#N`, its place in this trace; `file_bytes`: its
///   declared length.
/// - `written_bytes`: bytes the store has taken, cumulative over the trace;
///   `write_mbps`: in this interval, decimal MB/s.
/// - `q_read_unhashed`, `q_hashed_unwritten`: the queue at the sample, split:
///   read from staging but not yet hashed, and hashed but not yet taken by
///   the store.
/// - `wr_n`, `wr_max_ms`, `wr_p99_ms`: blocks the store took in the interval
///   and its time per block (≈ `write(2)`): the longest, and the 99th
///   percentile as the upper edge of its quarter-octave bucket.
/// - `writer_wait_ms`: the store waiting for the next block (the drive
///   starved by the host); `writer_wait_max_ms`: the longest single such
///   wait that ended in the interval; `reader_wait_ms`: the staging reader
///   waiting for a free buffer (the queue full); `hasher_busy_ms`: the hasher
///   inside its updates.
/// - `paced_ms`, `paused_ms`, `pace_mbps`: the `research` build's shaping
///   (`crate::research`): the writer held back by the pace, held for a
///   refill, and the pace in force; 0, 0 and empty in every other build.
/// - `reader_*`, `hasher_*`, `writer_*`: each thread's `run_ms` on a CPU and
///   `delay_ms` runnable but waiting for one (`/proc/self/task/<tid>/schedstat`;
///   a per-file thread's last stretch is added when it exits), and `cpu`, the
///   CPU it last ran on (`stat` field 39; empty between files).
/// - `cpu_user_pct`, `cpu_sys_pct`, `cpu_iowait_pct`: `/proc/stat`, all
///   CPUs, share of the interval.
/// - `rq_delay_ms`: run delay summed over every CPU (`/proc/schedstat`);
///   `rq_delay_<role>_cpu_ms`: on the CPU that thread last ran on.
/// - `disk_util_max_pct`, `disk_await_max_ms`, `disks`: the staging
///   filesystem's member disks (`/proc/diskstats`, resolved through dm and md
///   `slaves/` in sysfs): the busiest, the slowest, and each one as
///   `name:util_pct:await_ms`. Empty when no path was given or it resolves to
///   no block device (tmpfs, a network filesystem).
/// - `load1`, `procs_running`: `/proc/loadavg`; `vmswap_kb`: this process's
///   swapped-out memory (`/proc/self/status`).
/// - `sample_ms`: the interval the row covers (250 but for the last row,
///   taken when the pool is dropped).
pub const COLUMNS: [&str; 41] = [
    "t_s",
    "utc",
    "file",
    "file_bytes",
    "written_bytes",
    "write_mbps",
    "q_read_unhashed",
    "q_hashed_unwritten",
    "wr_n",
    "wr_max_ms",
    "wr_p99_ms",
    "writer_wait_ms",
    "writer_wait_max_ms",
    "reader_wait_ms",
    "hasher_busy_ms",
    "paced_ms",
    "paused_ms",
    "pace_mbps",
    "reader_run_ms",
    "reader_delay_ms",
    "reader_cpu",
    "hasher_run_ms",
    "hasher_delay_ms",
    "hasher_cpu",
    "writer_run_ms",
    "writer_delay_ms",
    "writer_cpu",
    "cpu_user_pct",
    "cpu_sys_pct",
    "cpu_iowait_pct",
    "rq_delay_ms",
    "rq_delay_reader_cpu_ms",
    "rq_delay_hasher_cpu_ms",
    "rq_delay_writer_cpu_ms",
    "disk_util_max_pct",
    "disk_await_max_ms",
    "disks",
    "load1",
    "procs_running",
    "vmswap_kb",
    "sample_ms",
];

/// Whether the trace is asked for: [`TRACE_ENV`] is exactly `1`. Read once
/// per process.
pub fn requested() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var(TRACE_ENV).is_ok_and(|v| v.trim() == "1"))
}

// ── the clock ────────────────────────────────────────────────────────────

/// Nanoseconds since the process's trace origin: one monotonic clock for
/// every stage's atomics and every row's `t_s`.
pub(crate) fn now_ns() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    let origin = *ORIGIN.get_or_init(Instant::now);
    u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// Accumulated time spent waiting, readable mid-wait: a wait in progress
/// counts up to the moment it is read, so a 4 s stall shows across the rows
/// it spans, not all in the one where it ended. The waiting thread writes
/// three atomics per wait; the sampler reads a consistent pair through a
/// sequence counter (odd while a wait is being closed).
#[derive(Default)]
pub(crate) struct WaitClock {
    seq: AtomicU64,
    done_ns: AtomicU64,
    /// When the wait in progress began, `now_ns() + 1`; 0 = not waiting.
    since: AtomicU64,
}

impl WaitClock {
    pub(crate) fn begin(&self, at_ns: u64) {
        self.since.store(at_ns + 1, Ordering::SeqCst);
    }

    /// Close the wait begun with [`WaitClock::begin`]; how long it lasted.
    pub(crate) fn end(&self, at_ns: u64) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst);
        let since = self.since.load(Ordering::SeqCst);
        let took = if since == 0 {
            0
        } else {
            at_ns.saturating_sub(since - 1)
        };
        self.done_ns.fetch_add(took, Ordering::SeqCst);
        self.since.store(0, Ordering::SeqCst);
        self.seq.fetch_add(1, Ordering::SeqCst);
        took
    }

    /// Every wait so far, the one in progress up to `at_ns`.
    pub(crate) fn total(&self, at_ns: u64) -> u64 {
        loop {
            let s1 = self.seq.load(Ordering::SeqCst);
            if s1 % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let done = self.done_ns.load(Ordering::SeqCst);
            let since = self.since.load(Ordering::SeqCst);
            if self.seq.load(Ordering::SeqCst) == s1 {
                let ongoing = if since == 0 {
                    0
                } else {
                    at_ns.saturating_sub(since - 1)
                };
                return done + ongoing;
            }
        }
    }
}

/// A latency histogram of atomics: quarter-octave buckets over microseconds,
/// drained by the sampler each interval.
pub(crate) struct Histogram {
    buckets: Vec<AtomicU64>,
    max_us: AtomicU64,
}

const BUCKETS: usize = 256;

impl Default for Histogram {
    fn default() -> Self {
        Histogram {
            buckets: (0..BUCKETS).map(|_| AtomicU64::new(0)).collect(),
            max_us: AtomicU64::new(0),
        }
    }
}

/// The bucket for `us` microseconds: exact below 8, then four per octave.
fn bucket_of(us: u64) -> usize {
    if us < 4 {
        return us as usize;
    }
    let msb = 63 - us.leading_zeros() as usize; // ≥ 2
    let sub = ((us >> (msb - 2)) & 3) as usize;
    (4 * (msb - 1) + sub).min(BUCKETS - 1)
}

/// The largest value bucket `i` holds (its upper edge, inclusive).
fn bucket_upper(i: usize) -> u64 {
    if i < 4 {
        return i as u64;
    }
    let msb = i / 4 + 1;
    let sub = (i % 4) as u64;
    ((5 + sub) << (msb - 2)) - 1
}

impl Histogram {
    pub(crate) fn record(&self, took: Duration) {
        let us = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
        self.buckets[bucket_of(us)].fetch_add(1, Ordering::Relaxed);
        self.max_us.fetch_max(us, Ordering::Relaxed);
    }

    /// Everything recorded since the last drain: `(count, max µs, p99 µs)`.
    fn drain(&self) -> (u64, u64, u64) {
        let counts: Vec<u64> = self
            .buckets
            .iter()
            .map(|b| b.swap(0, Ordering::Relaxed))
            .collect();
        let max = self.max_us.swap(0, Ordering::Relaxed);
        let n: u64 = counts.iter().sum();
        (n, max, p99_of(&counts, n).min(max))
    }
}

/// The upper edge of the bucket holding the 99th percentile of `counts`.
fn p99_of(counts: &[u64], n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let want = (n * 99).div_ceil(100);
    let mut seen = 0;
    for (i, c) in counts.iter().enumerate() {
        seen += c;
        if seen >= want {
            return bucket_upper(i);
        }
    }
    bucket_upper(counts.len() - 1)
}

// ── what the stages share ────────────────────────────────────────────────

/// A thread the trace follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Reader,
    Hasher,
}

/// A per-file stage thread's slot: the live thread, and what the threads
/// that held the role before it ran and waited, added as each exits so no
/// stretch is lost between samples.
#[derive(Default)]
struct RoleSlot {
    tid: i32,
    retired_run_ns: u64,
    retired_delay_ns: u64,
}

/// Everything the pipeline's stages record for the sampler (and the
/// `research` build's shaper reads). Shared as `Arc<Gauges>`; present only
/// while the trace or the shaper is on.
#[derive(Default)]
pub struct Gauges {
    pub(crate) read_unhashed: AtomicU64,
    pub(crate) hashed_unwritten: AtomicU64,
    pub(crate) written: AtomicU64,
    pub(crate) writer_wait: WaitClock,
    pub(crate) writer_wait_max_ns: AtomicU64,
    pub(crate) reader_wait: WaitClock,
    pub(crate) hasher_busy_ns: AtomicU64,
    pub(crate) store_block: Histogram,
    pub(crate) paced_ns: AtomicU64,
    pub(crate) paused_ns: AtomicU64,
    /// The pace in force, `f64::to_bits` of MB/s; 0 = unpaced.
    pub(crate) pace_mbps_bits: AtomicU64,
    pub(crate) file_seq: AtomicU64,
    pub(crate) file_bytes: AtomicU64,
    writer_tid: AtomicI32,
    reader: Mutex<RoleSlot>,
    hasher: Mutex<RoleSlot>,
    source: Mutex<Option<PathBuf>>,
}

impl Gauges {
    /// A new file begins: its place and length, and the calling thread (the
    /// store's) noted as the writer.
    pub(crate) fn begin_file(&self, len: u64) {
        self.file_seq.fetch_add(1, Ordering::Relaxed);
        self.file_bytes.store(len, Ordering::Relaxed);
        self.writer_tid.store(gettid(), Ordering::Relaxed);
    }

    /// The file's stages are joined: whatever they left counted in the queue
    /// (a block the hasher withheld, a store that stopped early) went with
    /// them.
    pub(crate) fn end_file(&self) {
        self.read_unhashed.store(0, Ordering::Relaxed);
        self.hashed_unwritten.store(0, Ordering::Relaxed);
    }

    /// The path the pipeline reads next: its name for the `file` column, its
    /// filesystem for the disk columns.
    pub(crate) fn set_source(&self, path: &Path) {
        *self.source.lock().unwrap_or_else(|e| e.into_inner()) = Some(path.to_path_buf());
    }

    fn source(&self) -> Option<PathBuf> {
        self.source
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn slot(&self, role: Role) -> &Mutex<RoleSlot> {
        match role {
            Role::Reader => &self.reader,
            Role::Hasher => &self.hasher,
        }
    }

    /// The calling thread takes `role` until the guard drops.
    pub(crate) fn enter(self: &Arc<Self>, role: Role) -> RoleGuard {
        self.slot(role)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tid = gettid();
        RoleGuard {
            gauges: self.clone(),
            role,
        }
    }

    /// A role's run and delay so far (retired threads and the live one),
    /// and the CPU the live one last ran on.
    fn role_times(&self, role: Role) -> (u64, u64, Option<u32>) {
        let slot = self.slot(role).lock().unwrap_or_else(|e| e.into_inner());
        let (run, delay, cpu) = if slot.tid != 0 {
            let (r, d) = thread_schedstat(slot.tid).unwrap_or((0, 0));
            (r, d, thread_cpu(slot.tid))
        } else {
            (0, 0, None)
        };
        (
            slot.retired_run_ns + run,
            slot.retired_delay_ns + delay,
            cpu,
        )
    }

    fn writer_times(&self) -> (u64, u64, Option<u32>) {
        let tid = self.writer_tid.load(Ordering::Relaxed);
        if tid == 0 {
            return (0, 0, None);
        }
        let (r, d) = thread_schedstat(tid).unwrap_or((0, 0));
        (r, d, thread_cpu(tid))
    }
}

/// A stage thread's hold on its [`Role`]; on drop its own run and delay are
/// added to the role's account and the slot is freed.
pub(crate) struct RoleGuard {
    gauges: Arc<Gauges>,
    role: Role,
}

impl Drop for RoleGuard {
    fn drop(&mut self) {
        let own = std::fs::read_to_string("/proc/thread-self/schedstat")
            .ok()
            .and_then(|s| parse_schedstat_line(&s));
        let mut slot = self
            .gauges
            .slot(self.role)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((run, delay)) = own {
            slot.retired_run_ns += run;
            slot.retired_delay_ns += delay;
        }
        slot.tid = 0;
    }
}

fn gettid() -> i32 {
    thread_local! {
        static TID: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
    }
    TID.with(|t| {
        if t.get() == 0 {
            // SAFETY: gettid(2) takes no arguments and cannot fail.
            t.set(unsafe { nix::libc::syscall(nix::libc::SYS_gettid) } as i32);
        }
        t.get()
    })
}

// ── the trace ────────────────────────────────────────────────────────────

/// A running trace: the sampler thread and how to stop it. Dropping it takes
/// a last sample and joins the thread.
pub(crate) struct Trace {
    stop: Arc<(Mutex<bool>, Condvar)>,
    sampler: Option<JoinHandle<()>>,
}

impl Trace {
    /// Start sampling `gauges` into the file beside `session`'s log. `Err`
    /// says why it could not; the caller logs it once and writes on.
    pub(crate) fn start(
        session: &progress::Handle,
        gauges: Arc<Gauges>,
        queue_capacity: u64,
    ) -> Result<Trace, String> {
        let log = session
            .log_path()
            .ok_or("this command has no session log for the trace to sit beside")?;
        let path = trace_path(log);
        let file =
            open_beside(log, &path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        let mut out = BufWriter::new(file);
        if out.get_ref().metadata().map(|m| m.len()).unwrap_or(0) == 0 {
            writeln!(out, "{}", COLUMNS.join("\t"))
                .and_then(|()| out.flush())
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        }
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let stop_t = stop.clone();
        let sampler = std::thread::Builder::new()
            .name("tapectl-trace".into())
            .spawn(move || {
                Sampler::new(gauges, queue_capacity).run(&mut out, &stop_t);
            })
            .map_err(|e| format!("cannot start the sampler thread: {e}"))?;
        session.log(&format!(
            "pipeline trace: one row every {} ms into {} ({TRACE_ENV}=1)",
            SAMPLE_INTERVAL.as_millis(),
            path.display()
        ));
        Ok(Trace {
            stop,
            sampler: Some(sampler),
        })
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        {
            let (lock, cv) = &*self.stop;
            *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
            cv.notify_all();
        }
        if let Some(t) = self.sampler.take() {
            let _ = t.join();
        }
    }
}

/// `<session>.log` → `<session>.pipeline.tsv`, in the same directory.
pub fn trace_path(log: &Path) -> PathBuf {
    let stem = log
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.strip_suffix(".log").unwrap_or(n).to_string())
        .unwrap_or_else(|| "session".into());
    log.with_file_name(format!("{stem}{TRACE_SUFFIX}"))
}

/// Open `path` for appending with the session log's own mode, so a log
/// directory shared with the `[ops] group` shares the trace too.
fn open_beside(log: &Path, path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mode = std::fs::metadata(log)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0o600);
    let f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(mode)
        .open(path)?;
    f.set_permissions(std::fs::Permissions::from_mode(mode))?;
    Ok(f)
}

// ── the sampler ──────────────────────────────────────────────────────────

/// What one sample read, for the next one to difference against.
#[derive(Clone, Default)]
struct Reading {
    at_ns: u64,
    written: u64,
    writer_wait: u64,
    reader_wait: u64,
    hasher_busy: u64,
    paced: u64,
    paused: u64,
    roles: [(u64, u64); 3],
    cpu: Option<CpuTimes>,
    rq_delay: Vec<(u32, u64)>,
    disks: Vec<DiskStat>,
}

struct Sampler {
    gauges: Arc<Gauges>,
    capacity: u64,
    sysfs: PathBuf,
    /// The source's device and its member disks, resolved once per device.
    members: Option<(u64, Vec<String>)>,
}

impl Sampler {
    fn new(gauges: Arc<Gauges>, capacity: u64) -> Sampler {
        Sampler {
            gauges,
            capacity,
            sysfs: PathBuf::from("/sys"),
            members: None,
        }
    }

    fn run(mut self, out: &mut impl Write, stop: &(Mutex<bool>, Condvar)) {
        let mut prev = self.read();
        let started = Instant::now();
        let mut k = 1u32;
        loop {
            let stopping = {
                let (lock, cv) = stop;
                let mut stopped = lock.lock().unwrap_or_else(|e| e.into_inner());
                let due = SAMPLE_INTERVAL * k;
                while !*stopped {
                    let elapsed = started.elapsed();
                    if elapsed >= due {
                        break;
                    }
                    stopped = cv
                        .wait_timeout(stopped, due - elapsed)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
                *stopped
            };
            let now = self.read();
            let row = self.row(&prev, &now);
            if writeln!(out, "{row}").and_then(|()| out.flush()).is_err() {
                return;
            }
            prev = now;
            if stopping {
                return;
            }
            // Never fall behind into a burst of rows: the next due time is
            // the next multiple of the interval after now.
            k = (started.elapsed().as_nanos() / SAMPLE_INTERVAL.as_nanos()) as u32 + 1;
        }
    }

    fn read(&mut self) -> Reading {
        let disks = match self.member_disks() {
            Some(names) => read_diskstats(&names),
            None => Vec::new(),
        };
        let g = &self.gauges;
        let at_ns = now_ns();
        let roles = [
            g.role_times(Role::Reader),
            g.role_times(Role::Hasher),
            g.writer_times(),
        ];
        Reading {
            at_ns,
            written: g.written.load(Ordering::Relaxed),
            writer_wait: g.writer_wait.total(at_ns),
            reader_wait: g.reader_wait.total(at_ns),
            hasher_busy: g.hasher_busy_ns.load(Ordering::Relaxed),
            paced: g.paced_ns.load(Ordering::Relaxed),
            paused: g.paused_ns.load(Ordering::Relaxed),
            roles: roles.map(|(r, d, _)| (r, d)),
            cpu: std::fs::read_to_string("/proc/stat")
                .ok()
                .and_then(|s| parse_proc_stat(&s)),
            rq_delay: std::fs::read_to_string("/proc/schedstat")
                .map(|s| parse_proc_schedstat(&s))
                .unwrap_or_default(),
            disks,
        }
    }

    /// The source path's member disks, re-resolved only when its device
    /// changes. `None` when there is no path or it is on no block device.
    fn member_disks(&mut self) -> Option<Vec<String>> {
        use std::os::unix::fs::MetadataExt;
        let path = self.gauges.source()?;
        let dev = std::fs::metadata(&path).ok()?.dev();
        if self.members.as_ref().map(|(d, _)| *d) != Some(dev) {
            let (maj, min) = dev_major_minor(dev);
            self.members = Some((dev, member_disks(&self.sysfs, maj, min)));
        }
        let names = self.members.as_ref().map(|(_, m)| m.clone())?;
        (!names.is_empty()).then_some(names)
    }

    fn row(&self, prev: &Reading, now: &Reading) -> String {
        let g = &self.gauges;
        let dt_ns = now.at_ns.saturating_sub(prev.at_ns).max(1);
        let ms = |a: u64, b: u64| format!("{:.3}", a.saturating_sub(b) as f64 / 1e6);
        let (wr_n, wr_max, wr_p99) = g.store_block.drain();
        let wmax = g.writer_wait_max_ns.swap(0, Ordering::Relaxed);
        let pace = f64::from_bits(g.pace_mbps_bits.load(Ordering::Relaxed));
        let file = g
            .source()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .map(|n| n.replace(['\t', '\n'], " "))
            .unwrap_or_else(|| format!("#{}", g.file_seq.load(Ordering::Relaxed)));
        let roles = [
            g.role_times(Role::Reader).2,
            g.role_times(Role::Hasher).2,
            g.writer_times().2,
        ];
        let mut cols: Vec<String> = vec![
            format!("{:.3}", now.at_ns as f64 / 1e9),
            Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            file,
            g.file_bytes.load(Ordering::Relaxed).to_string(),
            now.written.to_string(),
            format!(
                "{:.1}",
                now.written.saturating_sub(prev.written) as f64 / 1e6 / (dt_ns as f64 / 1e9)
            ),
            g.read_unhashed
                .load(Ordering::Relaxed)
                .min(self.capacity)
                .to_string(),
            g.hashed_unwritten
                .load(Ordering::Relaxed)
                .min(self.capacity)
                .to_string(),
            wr_n.to_string(),
            format!("{:.3}", wr_max as f64 / 1e3),
            format!("{:.3}", wr_p99 as f64 / 1e3),
            ms(now.writer_wait, prev.writer_wait),
            format!("{:.3}", wmax as f64 / 1e6),
            ms(now.reader_wait, prev.reader_wait),
            ms(now.hasher_busy, prev.hasher_busy),
            ms(now.paced, prev.paced),
            ms(now.paused, prev.paused),
            if pace > 0.0 {
                format!("{pace:.1}")
            } else {
                String::new()
            },
        ];
        for (i, cpu) in roles.iter().enumerate() {
            cols.push(ms(now.roles[i].0, prev.roles[i].0));
            cols.push(ms(now.roles[i].1, prev.roles[i].1));
            cols.push(cpu.map(|c| c.to_string()).unwrap_or_default());
        }
        match (&prev.cpu, &now.cpu) {
            (Some(a), Some(b)) => {
                let (u, s, w) = b.shares_since(a);
                cols.extend([u, s, w].map(|x| format!("{x:.1}")));
            }
            _ => cols.extend([String::new(), String::new(), String::new()]),
        }
        let delay_on = |cpu: Option<u32>| -> String {
            let Some(cpu) = cpu else {
                return String::new();
            };
            let get = |r: &[(u32, u64)]| r.iter().find(|(c, _)| *c == cpu).map(|(_, d)| *d);
            match (get(&prev.rq_delay), get(&now.rq_delay)) {
                (Some(a), Some(b)) => ms(b, a),
                _ => String::new(),
            }
        };
        if now.rq_delay.is_empty() {
            cols.push(String::new());
        } else {
            let sum = |r: &[(u32, u64)]| r.iter().map(|(_, d)| d).sum::<u64>();
            cols.push(ms(sum(&now.rq_delay), sum(&prev.rq_delay)));
        }
        for cpu in roles {
            cols.push(delay_on(cpu));
        }
        let disks = disk_figures(&prev.disks, &now.disks, dt_ns);
        if disks.is_empty() {
            cols.extend([String::new(), String::new(), String::new()]);
        } else {
            let util = disks.iter().map(|d| d.1).fold(0.0, f64::max);
            let wait = disks.iter().map(|d| d.2).fold(0.0, f64::max);
            cols.push(format!("{util:.1}"));
            cols.push(format!("{wait:.2}"));
            cols.push(
                disks
                    .iter()
                    .map(|(n, u, w)| format!("{n}:{u:.1}:{w:.2}"))
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
        let (load1, running) = std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|s| parse_loadavg(&s))
            .map(|(l, r)| (format!("{l:.2}"), r.to_string()))
            .unwrap_or_default();
        cols.push(load1);
        cols.push(running);
        cols.push(
            std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|s| parse_vmswap_kb(&s))
                .map(|k| k.to_string())
                .unwrap_or_default(),
        );
        cols.push(format!("{:.1}", dt_ns as f64 / 1e6));
        debug_assert_eq!(cols.len(), COLUMNS.len());
        cols.join("\t")
    }
}

// ── /proc and sysfs, parsed ──────────────────────────────────────────────

/// `/proc/<pid>/task/<tid>/schedstat`'s first two fields: time on a CPU and
/// time runnable waiting for one, nanoseconds.
fn parse_schedstat_line(s: &str) -> Option<(u64, u64)> {
    let mut it = s.split_whitespace();
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

fn thread_schedstat(tid: i32) -> Option<(u64, u64)> {
    std::fs::read_to_string(format!("/proc/self/task/{tid}/schedstat"))
        .ok()
        .and_then(|s| parse_schedstat_line(&s))
}

/// Field 39 of a `stat` line, the CPU the task last ran on. The command
/// name (field 2) is in parentheses and may hold spaces and parentheses
/// itself, so the fields are counted from the LAST `)`.
fn parse_stat_cpu(s: &str) -> Option<u32> {
    let rest = &s[s.rfind(')')? + 1..];
    // After the name come fields 3 onward; 39 is the 37th of them.
    rest.split_whitespace().nth(36)?.parse().ok()
}

fn thread_cpu(tid: i32) -> Option<u32> {
    std::fs::read_to_string(format!("/proc/self/task/{tid}/stat"))
        .ok()
        .and_then(|s| parse_stat_cpu(&s))
}

/// The all-CPU line of `/proc/stat`, in USER_HZ ticks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CpuTimes {
    user: u64,
    system: u64,
    iowait: u64,
    total: u64,
}

impl CpuTimes {
    /// User, system and iowait as percentages of all CPU time since `prev`.
    fn shares_since(&self, prev: &CpuTimes) -> (f64, f64, f64) {
        let total = self.total.saturating_sub(prev.total).max(1) as f64;
        let pct = |a: u64, b: u64| a.saturating_sub(b) as f64 * 100.0 / total;
        (
            pct(self.user, prev.user),
            pct(self.system, prev.system),
            pct(self.iowait, prev.iowait),
        )
    }
}

fn parse_proc_stat(s: &str) -> Option<CpuTimes> {
    let line = s.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|f| f.parse().ok())
        .collect();
    // user nice system idle iowait irq softirq steal [guest guest_nice]
    // (guest time is already inside user, so it is not added again).
    if v.len() < 8 {
        return None;
    }
    Some(CpuTimes {
        user: v[0] + v[1],
        system: v[2] + v[5] + v[6],
        iowait: v[4],
        total: v[..8].iter().sum(),
    })
}

/// Each CPU's run delay from `/proc/schedstat` (`cpuN` lines, the eighth
/// number: time tasks spent runnable on it waiting to run, nanoseconds).
fn parse_proc_schedstat(s: &str) -> Vec<(u32, u64)> {
    s.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let cpu = it.next()?.strip_prefix("cpu")?.parse().ok()?;
            let delay = it.nth(7)?.parse().ok()?;
            Some((cpu, delay))
        })
        .collect()
}

/// `/proc/loadavg`: the one-minute load and the running task count.
fn parse_loadavg(s: &str) -> Option<(f64, u64)> {
    let mut it = s.split_whitespace();
    let load1 = it.next()?.parse().ok()?;
    let running = it.nth(2)?.split('/').next()?.parse().ok()?;
    Some((load1, running))
}

fn parse_vmswap_kb(s: &str) -> Option<u64> {
    s.lines()
        .find_map(|l| l.strip_prefix("VmSwap:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// One disk's `/proc/diskstats` counters the sampler differences.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DiskStat {
    name: String,
    ios: u64,
    io_ms: u64,
    busy_ms: u64,
}

fn parse_diskstats(s: &str, names: &[String]) -> Vec<DiskStat> {
    s.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let name = *f.get(2)?;
            if !names.iter().any(|n| n == name) {
                return None;
            }
            let n = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok());
            // 3 reads, 6 ms reading, 7 writes, 10 ms writing, 12 ms busy.
            Some(DiskStat {
                name: name.to_string(),
                ios: n(3)? + n(7)?,
                io_ms: n(6)? + n(10)?,
                busy_ms: n(12)?,
            })
        })
        .collect()
}

fn read_diskstats(names: &[String]) -> Vec<DiskStat> {
    std::fs::read_to_string("/proc/diskstats")
        .map(|s| parse_diskstats(&s, names))
        .unwrap_or_default()
}

/// Each disk in both readings: `(name, util %, await ms)` over `dt_ns`.
fn disk_figures(prev: &[DiskStat], now: &[DiskStat], dt_ns: u64) -> Vec<(String, f64, f64)> {
    now.iter()
        .filter_map(|b| {
            let a = prev.iter().find(|a| a.name == b.name)?;
            let ios = b.ios.saturating_sub(a.ios);
            let util = b.busy_ms.saturating_sub(a.busy_ms) as f64 * 1e6 * 100.0 / dt_ns as f64;
            let wait = if ios == 0 {
                0.0
            } else {
                b.io_ms.saturating_sub(a.io_ms) as f64 / ios as f64
            };
            Some((b.name.clone(), util.min(100.0), wait))
        })
        .collect()
}

/// `st_dev` split as the kernel's `MAJOR`/`MINOR` (glibc's encoding).
fn dev_major_minor(dev: u64) -> (u64, u64) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major, minor)
}

/// The whole disks under block device `major:minor`, through sysfs rooted at
/// `sysfs`: a device with `slaves/` (dm, md) is resolved through each slave,
/// a partition to the disk that holds it, and a disk is itself. Empty when
/// the device is not a block device sysfs knows (tmpfs, NFS, btrfs's
/// anonymous devices): the trace's disk columns stay empty, never an error.
pub(crate) fn member_disks(sysfs: &Path, major: u64, minor: u64) -> Vec<String> {
    let link = sysfs.join("dev/block").join(format!("{major}:{minor}"));
    let Ok(dir) = std::fs::canonicalize(link) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    resolve_disk(&dir, 0, &mut out);
    out.sort();
    out.dedup();
    out
}

fn resolve_disk(dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > 8 {
        return;
    }
    let slaves: Vec<PathBuf> = std::fs::read_dir(dir.join("slaves"))
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| std::fs::canonicalize(e.path()).ok())
                .collect()
        })
        .unwrap_or_default();
    if !slaves.is_empty() {
        for s in slaves {
            resolve_disk(&s, depth + 1, out);
        }
        return;
    }
    let disk = if dir.join("partition").exists() {
        dir.parent()
    } else {
        Some(dir)
    };
    if let Some(name) = disk.and_then(Path::file_name).and_then(|n| n.to_str()) {
        out.push(name.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_exact_low_and_quarter_octaves_above() {
        for us in 0..4 {
            assert_eq!(bucket_upper(bucket_of(us)), us);
        }
        // Every value sits at or under its bucket's upper edge, and above
        // the previous bucket's.
        for us in [
            4u64, 5, 7, 8, 9, 15, 16, 100, 999, 1000, 4096, 123_456, 9_999_999,
        ] {
            let b = bucket_of(us);
            assert!(us <= bucket_upper(b), "{us} in {b}");
            assert!(b == 0 || us > bucket_upper(b - 1), "{us} in {b}");
        }
        // A quarter octave: the edge is within 25% of the value.
        let b = bucket_of(10_000);
        assert!(bucket_upper(b) < 12_500, "{}", bucket_upper(b));
    }

    #[test]
    fn the_p99_is_the_bucket_of_the_ninety_ninth_value() {
        let h = Histogram::default();
        for _ in 0..99 {
            h.record(Duration::from_micros(100));
        }
        h.record(Duration::from_millis(50));
        let (n, max, p99) = h.drain();
        assert_eq!((n, max), (100, 50_000));
        assert!((100..=127).contains(&p99), "{p99}");
        // Drained: the next interval starts empty.
        assert_eq!(h.drain(), (0, 0, 0));
    }

    #[test]
    fn a_wait_in_progress_counts_up_to_the_sample() {
        let w = WaitClock::default();
        w.begin(1_000);
        assert_eq!(w.total(1_500), 500, "mid-wait");
        assert_eq!(w.end(2_000), 1_000);
        assert_eq!(w.total(9_000), 1_000, "closed");
        w.begin(10_000);
        assert_eq!(w.total(10_250), 1_250);
    }

    #[test]
    fn the_proc_files_parse() {
        assert_eq!(
            parse_schedstat_line("123456 7890 12\n"),
            Some((123_456, 7_890))
        );
        // A command name with a space and a parenthesis.
        let mut stat = String::from("4242 (tape (x) y) S");
        for i in 4..=52 {
            stat.push_str(&format!(" {}", if i == 39 { 17 } else { i }));
        }
        assert_eq!(parse_stat_cpu(&stat), Some(17));
        let cpu =
            parse_proc_stat("cpu  100 5 30 1000 20 3 2 0 7 0\ncpu0 1 2 3 4 5 6 7 8 9 10\nintr 1\n")
                .unwrap();
        assert_eq!(
            cpu,
            CpuTimes {
                user: 105,
                system: 35,
                iowait: 20,
                total: 1160
            }
        );
        let later = CpuTimes {
            user: 205,
            system: 35,
            iowait: 120,
            total: 1360,
        };
        let (u, s, w) = later.shares_since(&cpu);
        assert_eq!((u, s, w), (50.0, 0.0, 50.0));
        assert_eq!(
            parse_proc_schedstat(
                "version 15\ntimestamp 1\ncpu0 0 0 10 5 3 2 900 4444 77\n\
                 domain0 3 1 2 3\ncpu1 0 0 10 5 3 2 900 5555 77\n"
            ),
            vec![(0, 4444), (1, 5555)]
        );
        assert_eq!(
            parse_loadavg("1.12 0.80 0.50 3/812 4242\n"),
            Some((1.12, 3))
        );
        assert_eq!(
            parse_vmswap_kb("Name:\ttapectl\nVmSwap:\t    1536 kB\n"),
            Some(1536)
        );
        let names = vec!["sdc".to_string()];
        let d = parse_diskstats(
            "   8      32 sdc 100 0 800 50 10 0 80 30 0 70 90 0 0 0 0\n   8      33 sdc1 1 0 1 1 1 0 1 1 0 1 1\n",
            &names,
        );
        assert_eq!(
            d,
            vec![DiskStat {
                name: "sdc".into(),
                ios: 110,
                io_ms: 80,
                busy_ms: 70
            }]
        );
        let later = DiskStat {
            name: "sdc".into(),
            ios: 120,
            io_ms: 120,
            busy_ms: 195,
        };
        let f = disk_figures(&d, &[later], 250_000_000);
        assert_eq!(f, vec![("sdc".to_string(), 50.0, 4.0)]);
    }

    #[test]
    fn device_numbers_split_as_the_kernel_encodes_them() {
        assert_eq!(dev_major_minor(0x0811), (8, 17));
        // A minor past 255 (glibc's `makedev`: the minor's high bits sit
        // above the major's low twelve).
        let dev = (253u64 << 8) | 0x34 | (0x1200 << 12);
        assert_eq!(dev_major_minor(dev), (253, 0x1234));
    }

    /// The staging filesystem's member disks through a fake sysfs: LUKS
    /// (dm-0) over md1 (RAID0) over two partitions resolves to the two whole
    /// disks; a plain partition to its disk; an unknown device to nothing.
    #[test]
    fn member_disks_resolve_through_dm_md_and_partitions() {
        use std::os::unix::fs::symlink;
        let root = tempfile::TempDir::new().unwrap();
        let sys = root.path();
        let dev = |p: &str| {
            let d = sys.join("devices").join(p);
            std::fs::create_dir_all(&d).unwrap();
            d
        };
        let sdc = dev("pci/host0/block/sdc");
        let sdc1 = dev("pci/host0/block/sdc/sdc1");
        std::fs::write(sdc1.join("partition"), "1\n").unwrap();
        let sdd1 = dev("pci/host1/block/sdd/sdd1");
        std::fs::write(sdd1.join("partition"), "1\n").unwrap();
        let md1 = dev("virtual/block/md1");
        std::fs::create_dir_all(md1.join("slaves")).unwrap();
        symlink(&sdc1, md1.join("slaves/sdc1")).unwrap();
        symlink(&sdd1, md1.join("slaves/sdd1")).unwrap();
        let dm0 = dev("virtual/block/dm-0");
        std::fs::create_dir_all(dm0.join("slaves")).unwrap();
        symlink(&md1, dm0.join("slaves/md1")).unwrap();
        std::fs::create_dir_all(sys.join("dev/block")).unwrap();
        symlink(&dm0, sys.join("dev/block/253:0")).unwrap();
        symlink(&sdc1, sys.join("dev/block/8:33")).unwrap();
        symlink(&sdc, sys.join("dev/block/8:32")).unwrap();

        assert_eq!(member_disks(sys, 253, 0), ["sdc", "sdd"]);
        assert_eq!(member_disks(sys, 8, 33), ["sdc"]);
        assert_eq!(member_disks(sys, 8, 32), ["sdc"]);
        assert!(
            member_disks(sys, 0, 44).is_empty(),
            "tmpfs: no block device"
        );
    }

    #[test]
    fn the_trace_file_sits_beside_the_session_log() {
        assert_eq!(
            trace_path(Path::new(
                "/h/logs/20261020T120000Z-volume-write-L-1-42.log"
            )),
            Path::new("/h/logs/20261020T120000Z-volume-write-L-1-42.pipeline.tsv")
        );
    }

    #[test]
    fn the_header_names_every_column_once() {
        let mut seen = std::collections::BTreeSet::new();
        for c in COLUMNS {
            assert!(seen.insert(c), "{c} twice");
            assert!(!c.contains(['\t', ' ']), "{c}");
        }
    }
}
