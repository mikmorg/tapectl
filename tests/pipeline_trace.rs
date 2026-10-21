//! Issue #488, the write-shaping note's §9 item 7: the dry run of the
//! pipeline trace. Files are written through the real write pipeline
//! (`pipeline::write_verified`, the stage read through `DropBehind` from a
//! real disk, the hash, and `MemStore` behind a throttle standing in for a
//! drive) with `TAPECTL_PIPELINE_TRACE=1`, and the trace file beside the
//! session log is checked: its header, monotonic times, and columns that
//! make sense against what the write did.
//!
//! **Its own test binary**, because the trace is switched on by an
//! environment variable read once per process: every test here sets it
//! before its first write, and no other test binary can see it. Ungated
//! (no tape, no mhvtl, no device) and about two seconds: the throttle makes
//! the write last several 250 ms samples. It drives the pipeline directly,
//! not a whole `volume write`, so it names each staged file to the trace
//! (`BufferPool::trace_source`) itself, where the write session's
//! `run_entries` does before each file.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::Digest;
use tapectl::pipeline::{write_verified, BufferPool, Verdict};
use tapectl::pipeline_trace::{trace_path, COLUMNS, TRACE_ENV};
use tapectl::progress;
use tapectl::store::{MemStore, Store};

const CHUNK: usize = 64 * 1024;
const BUFFERS: usize = 32;
// Each file takes about 0.6 s behind the throttle: not a multiple of the
// 250 ms sample, so the rows land mid-file as well as between files.
const FILES: usize = 4;
const FILE_BYTES: usize = 20 * 1024 * 1024 + 4097;

fn trace_on() {
    std::env::set_var(TRACE_ENV, "1");
}

/// A drive the host feeds: each read the store makes waits a little first,
/// as a `write(2)` to a drive whose buffer is full would.
struct Throttled<'a>(&'a mut dyn Read);

impl Read for Throttled<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        std::thread::sleep(Duration::from_micros(1500));
        let n = CHUNK.min(buf.len());
        self.0.read(&mut buf[..n])
    }
}

/// Staged files on a real disk: under cargo's per-target scratch directory,
/// which lives in the target directory, not on a tmpfs.
fn staged(dir: &Path) -> Vec<(PathBuf, String)> {
    (0..FILES)
        .map(|i| {
            let bytes: Vec<u8> = (0..FILE_BYTES).map(|j| (j * 13 + i * 7) as u8).collect();
            let path = dir.join(format!("slice-{i:03}.dar.age"));
            std::fs::write(&path, &bytes).unwrap();
            let sha = format!("{:x}", sha2::Sha256::digest(&bytes));
            (path, sha)
        })
        .collect()
}

/// The rows of a trace, each split into its columns.
fn rows(text: &str) -> (Vec<&str>, Vec<Vec<&str>>) {
    let mut lines = text.lines();
    let header = lines.next().expect("a header").split('\t').collect();
    (header, lines.map(|l| l.split('\t').collect()).collect())
}

fn col(header: &[&str], name: &str) -> usize {
    header
        .iter()
        .position(|h| *h == name)
        .unwrap_or_else(|| panic!("{name}"))
}

/// Whether sysfs knows the block device `path` is on (so the disk columns
/// must be filled): the positive control for "an unresolvable path gives
/// empty columns".
fn on_a_block_device(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let dev = std::fs::metadata(path).unwrap().dev();
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    Path::new("/sys/dev/block")
        .join(format!("{major}:{minor}"))
        .exists()
}

#[test]
fn a_traced_write_leaves_a_sane_tsv_beside_its_session_log() {
    trace_on();
    let logs = tempfile::TempDir::new().unwrap();
    let data = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let files = staged(data.path());
    let session = progress::start_session(
        Some(logs.path()),
        "volume write DRY-RUN",
        progress::Display::Off,
        false,
    );
    let log = session.log_path().unwrap().to_path_buf();

    let mut pool = BufferPool::new(CHUNK, BUFFERS);
    let mut mem = MemStore::new(CHUNK);
    for (path, sha) in &files {
        pool.trace_source(path);
        let src = tapectl::util::DropBehind::open(path).unwrap();
        let len = FILE_BYTES as u64;
        let streamed = write_verified(&mut pool, src, len, Some(sha), |src| {
            mem.execute(&mut Throttled(src), len, false)
        });
        assert!(
            matches!(streamed.verdict(Some(sha)), Verdict::Written(_)),
            "observing changes nothing the write does"
        );
    }
    // The pool's end is the trace's: a last row, then the sampler joins.
    drop(pool);
    drop(session);

    let tsv = std::fs::read_to_string(trace_path(&log)).expect("the trace file");
    // `--nocapture` shows it: what a trace looks like.
    eprintln!("the dry run's trace:\n{tsv}");
    let (header, rows) = rows(&tsv);
    assert_eq!(header, COLUMNS, "the header names every column");
    assert!(rows.len() >= 4, "{} rows:\n{tsv}", rows.len());
    for r in &rows {
        assert_eq!(r.len(), COLUMNS.len(), "{r:?}");
    }
    let num = |r: &Vec<&str>, name: &str| -> f64 {
        r[col(&header, name)]
            .parse()
            .unwrap_or_else(|e| panic!("{name} = {:?}: {e}", r[col(&header, name)]))
    };

    // Monotonic: time strictly, bytes written never back.
    for w in rows.windows(2) {
        assert!(num(&w[1], "t_s") > num(&w[0], "t_s"), "{tsv}");
        assert!(num(&w[1], "written_bytes") >= num(&w[0], "written_bytes"));
    }
    let total = (FILES * FILE_BYTES) as f64;
    assert_eq!(num(rows.last().unwrap(), "written_bytes"), total, "{tsv}");

    let capacity = (CHUNK * BUFFERS) as f64;
    let mut saw_blocks = false;
    for r in &rows {
        let q = num(r, "q_read_unhashed") + num(r, "q_hashed_unwritten");
        assert!(
            q <= capacity,
            "the queue never holds more than the pool: {r:?}"
        );
        assert!(num(r, "write_mbps") >= 0.0);
        if num(r, "wr_n") > 0.0 {
            saw_blocks = true;
            assert!(num(r, "wr_p99_ms") <= num(r, "wr_max_ms"), "{r:?}");
            assert!(num(r, "wr_max_ms") >= 1.0, "the throttle shows: {r:?}");
        }
        for name in ["cpu_user_pct", "cpu_sys_pct", "cpu_iowait_pct"] {
            let v = num(r, name);
            assert!((0.0..=100.0).contains(&v), "{name} {v}");
        }
        for name in ["reader_cpu", "hasher_cpu", "writer_cpu"] {
            let v = r[col(&header, name)];
            assert!(v.is_empty() || v.parse::<u32>().is_ok(), "{name} {v:?}");
        }
        assert!(num(r, "load1") >= 0.0);
        assert!(num(r, "paced_ms") == 0.0 && num(r, "paused_ms") == 0.0);
        assert!(r[col(&header, "file")].starts_with("slice-"), "{r:?}");
    }
    assert!(saw_blocks, "the store's blocks were timed:\n{tsv}");
    // Mid-file, the slow store leaves the queue holding hashed blocks, and
    // the per-file stage threads are live and placed on a CPU.
    assert!(
        rows.iter().any(|r| num(r, "q_hashed_unwritten") > 0.0),
        "the queue was seen holding data:\n{tsv}"
    );
    for name in ["reader_cpu", "hasher_cpu", "writer_cpu"] {
        assert!(
            rows.iter().any(|r| !r[col(&header, name)].is_empty()),
            "{name} was followed:\n{tsv}"
        );
    }
    let sum = |name: &str| rows.iter().map(|r| num(r, name)).sum::<f64>();
    assert!(sum("writer_run_ms") > 0.0, "the writer thread was followed");
    assert!(sum("hasher_busy_ms") > 0.0 && sum("hasher_run_ms") > 0.0);
    assert!(sum("reader_run_ms") > 0.0);
    // One row per 250 ms: never a row shorter than half the interval (the
    // sampler's rule when a row is taken late), and the typical row on time
    // (a loaded host may delay the odd one; the last is partial).
    let mut spans: Vec<f64> = rows[..rows.len() - 1]
        .iter()
        .map(|r| num(r, "sample_ms"))
        .collect();
    assert!(spans.iter().all(|s| *s >= 125.0), "{tsv}");
    spans.sort_by(f64::total_cmp);
    let median = spans[spans.len() / 2];
    assert!((245.0..=320.0).contains(&median), "median {median}:\n{tsv}");

    // Positive control for the disk columns: the staged files sit on a
    // block device sysfs knows, so its member disks were found.
    if on_a_block_device(&files[0].0) {
        assert!(
            rows.iter().any(|r| !r[col(&header, "disks")].is_empty()),
            "the staging disks resolved:\n{tsv}"
        );
    }

    let session_log = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        session_log
            .matches("pipeline trace: one row every 250 ms into")
            .count(),
        1,
        "{session_log}"
    );
}

/// A path that is on no block device (procfs here; tmpfs alike) gives empty
/// disk columns, never an error, and the write goes on.
#[test]
fn a_source_on_no_block_device_leaves_the_disk_columns_empty() {
    trace_on();
    let logs = tempfile::TempDir::new().unwrap();
    let session = progress::start_session(Some(logs.path()), "t", progress::Display::Off, false);
    let log = session.log_path().unwrap().to_path_buf();
    let mut pool = BufferPool::new(CHUNK, 4);
    let mut mem = MemStore::new(CHUNK);
    pool.trace_source(Path::new("/proc/version"));
    let bytes = vec![5u8; 300_000];
    let sha = format!("{:x}", sha2::Sha256::digest(&bytes));
    let streamed = write_verified(
        &mut pool,
        &bytes[..],
        bytes.len() as u64,
        Some(&sha),
        |src| mem.execute(&mut Throttled(src), bytes.len() as u64, false),
    );
    assert!(matches!(streamed.verdict(Some(&sha)), Verdict::Written(_)));
    std::thread::sleep(Duration::from_millis(300));
    drop(pool);
    drop(session);
    let tsv = std::fs::read_to_string(trace_path(&log)).unwrap();
    let (header, rows) = rows(&tsv);
    assert!(!rows.is_empty(), "{tsv}");
    for r in &rows {
        for name in ["disk_util_max_pct", "disk_await_max_ms", "disks"] {
            assert_eq!(r[col(&header, name)], "", "{name}: {r:?}");
        }
        assert_eq!(r[col(&header, "file")], "version");
    }
}

/// A trace that cannot start — here, something is already in the way of its
/// file — leaves the write untouched, and the session log says why once,
/// however many files follow.
#[test]
fn a_trace_that_cannot_start_says_why_once_and_the_write_goes_on() {
    trace_on();
    let logs = tempfile::TempDir::new().unwrap();
    let session = progress::start_session(Some(logs.path()), "t", progress::Display::Off, false);
    let log = session.log_path().unwrap().to_path_buf();
    std::fs::create_dir(trace_path(&log)).unwrap();
    let mut pool = BufferPool::new(CHUNK, 4);
    let mut mem = MemStore::new(CHUNK);
    for i in 0..3u8 {
        let bytes = vec![i; 200_000];
        let sha = format!("{:x}", sha2::Sha256::digest(&bytes));
        let streamed = write_verified(
            &mut pool,
            &bytes[..],
            bytes.len() as u64,
            Some(&sha),
            |src| mem.execute(src, bytes.len() as u64, false),
        );
        assert!(matches!(streamed.verdict(Some(&sha)), Verdict::Written(_)));
    }
    drop(pool);
    drop(session);
    let text = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        text.matches("pipeline trace: cannot start (cannot open")
            .count(),
        1,
        "{text}"
    );
    assert!(text.contains("the write goes on without it"), "{text}");
}
