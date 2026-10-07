//! Live progress on stderr and the per-session log (issue #386).
//!
//! A production `volume write` of 1.2 TiB once sat for hours with the drive
//! idle and nothing on the terminal, and nothing afterwards could say which
//! phase it had been in or what it waited on. This module is the answer, in
//! three parts that share one recorder:
//!
//! - **Phases.** A long operation names each stage of its work with
//!   [`phase`] (`validate`, `dar`, `encrypt`, `write`, `confirm`, ...). A
//!   phase has a start, an end, a duration, optionally a byte total, and a
//!   current item (the slice or unit being worked on). Every phase that ends
//!   is kept as a [`PhaseTiming`], which the caller drains with [`drain`] to
//!   record in the catalog (`phase_timings`, migration 028) and the stage
//!   report.
//! - **Waits.** Anything that can block for a long time without moving a
//!   byte — opening the tape device, a rewind or a space, an `sg_*` command,
//!   a busy catalog — is wrapped in [`wait`]. A wait that lasts past
//!   [`WAIT_LOG_THRESHOLD`] is logged when it crosses the threshold (so a
//!   wait that never ends still names itself) and again when it ends, with
//!   its duration.
//! - **Display.** On a terminal a single status line is redrawn in place:
//!   phase, bytes done and total, rate, ETA, current item and any wait in
//!   progress. When stderr is not a terminal the redraw is replaced by one
//!   plain line every [`DEFAULT_INTERVAL`] (`TAPECTL_PROGRESS_INTERVAL`
//!   overrides it, in seconds). `--quiet` turns the display off; the session
//!   log is written either way. Nothing here ever writes to stdout, so
//!   `--json` output is untouched.
//!
//! The session log is `<home>/logs/<UTC start>-<command>-<pid>.log`, mode
//! 0600, one line per event, each line led by a UTC timestamp. `main` also
//! tees every `tracing` event at INFO and above (DEBUG under `--verbose`)
//! into it, whatever the stderr level is.
//!
//! The recorder is installed per THREAD ([`start_session`] installs it on the
//! calling thread; a background ticker thread holds a second handle to redraw
//! and to notice waits). Every function here is a cheap no-op on a thread
//! with no recorder, so library code calls them unconditionally and tests
//! that never start a session are unaffected — and tests that do start one
//! cannot see each other's phases under the multithreaded test runner.

use std::cell::RefCell;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};

/// A wait shorter than this is not logged at all; one that reaches it is
/// logged when it does and again when it ends.
pub const WAIT_LOG_THRESHOLD: Duration = Duration::from_secs(5);

/// How often a plain progress line is printed when stderr is not a
/// terminal, and how often a progress line is written to the session log.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);

/// A phase that counts bytes and has moved none for this long, with no
/// named wait in progress, is logged as stalled — the gap names itself even
/// when no call site thought to wrap it in a [`wait`].
pub const STALL_THRESHOLD: Duration = Duration::from_secs(60);

/// On a terminal, a phase at least this long leaves a one-line summary
/// behind when it ends; shorter ones only flash by on the status line.
const TTY_SUMMARY_MIN: Duration = Duration::from_secs(2);

/// The environment variable that overrides [`DEFAULT_INTERVAL`], in whole
/// seconds (at least 1).
pub const INTERVAL_ENV: &str = "TAPECTL_PROGRESS_INTERVAL";

/// How progress is shown on stderr.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Display {
    /// A status line redrawn in place.
    Tty,
    /// One plain line per interval, and a summary of each long phase.
    Lines,
    /// Nothing on stderr; the session log only.
    Off,
}

impl Display {
    /// The display for this invocation: off under `--quiet`, a redrawn line
    /// on a terminal that can take one, plain lines otherwise.
    pub fn choose(quiet: bool, stderr_is_terminal: bool, term: Option<&str>) -> Display {
        if quiet {
            Display::Off
        } else if stderr_is_terminal && term != Some("dumb") {
            Display::Tty
        } else {
            Display::Lines
        }
    }
}

/// One ended phase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhaseTiming {
    pub phase: String,
    pub started_at: DateTime<Utc>,
    pub duration: Duration,
    /// Bytes moved, when the phase counts bytes; `None` when it does not.
    pub bytes: Option<u64>,
    /// `false` when the phase ended because an error unwound past it.
    pub ok: bool,
}

impl PhaseTiming {
    /// `ok` or `failed` — `cartridge_contacts.outcome`'s vocabulary.
    pub fn outcome(&self) -> &'static str {
        if self.ok {
            "ok"
        } else {
            "failed"
        }
    }

    /// `started_at` as SQLite's `datetime('now')` spells it (UTC), so the
    /// catalog's timestamps compare as text.
    pub fn started_at_sql(&self) -> String {
        self.started_at.format("%Y-%m-%d %H:%M:%S").to_string()
    }

    /// `"write  2h 20m 1s  1.20 TiB  145.2 MiB/s"` — the one-line summary
    /// the stage report, `volume info` and the display share.
    pub fn summary(&self) -> String {
        let mut s = format!("{:<14} {:>10}", self.phase, format_duration(self.duration));
        if let Some(b) = self.bytes {
            s.push_str(&format!("  {:>10}", format_bytes(b)));
            if let Some(rate) = rate(b, self.duration) {
                s.push_str(&format!("  {rate}"));
            }
        }
        if !self.ok {
            s.push_str("  (failed)");
        }
        s
    }
}

struct ActivePhase {
    id: u64,
    name: String,
    started: Instant,
    started_utc: DateTime<Utc>,
    total: Option<u64>,
    bytes: u64,
    counts_bytes: bool,
    item: Option<String>,
    first_byte_at: Option<Instant>,
    last_byte_at: Instant,
    stall_logged: bool,
    poll: Option<Box<dyn FnMut() -> Option<u64> + Send>>,
    last_line_at: Instant,
    lines_emitted: u32,
}

struct ActiveWait {
    id: u64,
    what: String,
    started: Instant,
    announced: bool,
}

struct State {
    log: Option<File>,
    display: Display,
    interval: Duration,
    phases: Vec<ActivePhase>,
    finished: Vec<PhaseTiming>,
    waits: Vec<ActiveWait>,
    next_id: u64,
}

/// One session's shared state. See the module documentation.
pub struct Recorder {
    state: Mutex<State>,
    session_id: String,
    log_path: Option<PathBuf>,
    started: Instant,
    /// Where display output goes instead of stderr — a test's buffer.
    /// `None` in every real session.
    divert: Option<Arc<Mutex<Vec<String>>>>,
}

impl Recorder {
    fn println(&self, line: &str) {
        match &self.divert {
            Some(d) => d
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(line.to_string()),
            None => stderr_println(line),
        }
    }

    fn draw(&self, line: &str) {
        match &self.divert {
            Some(d) => d
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("\r{line}")),
            None => stderr_draw(line),
        }
    }

    fn clear(&self) {
        if self.divert.is_none() {
            stderr_clear();
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn next_id(st: &mut State) -> u64 {
        st.next_id += 1;
        st.next_id
    }

    fn start_phase(&self, name: &str, total: Option<u64>) -> u64 {
        let now = Instant::now();
        let mut st = self.lock();
        let id = Self::next_id(&mut st);
        let msg = match total {
            Some(t) => format!("phase start: {name} ({})", format_bytes(t)),
            None => format!("phase start: {name}"),
        };
        log_line(&mut st, &msg);
        st.phases.push(ActivePhase {
            id,
            name: name.to_string(),
            started: now,
            started_utc: Utc::now(),
            total,
            bytes: 0,
            counts_bytes: total.is_some(),
            item: None,
            first_byte_at: None,
            last_byte_at: now,
            stall_logged: false,
            poll: None,
            last_line_at: now,
            lines_emitted: 0,
        });
        id
    }

    fn end_phase(&self, id: u64, ok: bool) -> Option<Duration> {
        let now = Instant::now();
        let mut st = self.lock();
        let idx = st.phases.iter().position(|p| p.id == id)?;
        let mut p = st.phases.remove(idx);
        if let Some(poll) = p.poll.as_mut() {
            if let Some(b) = poll() {
                p.bytes = b;
            }
        }
        let duration = now.duration_since(p.started);
        let timing = PhaseTiming {
            phase: p.name.clone(),
            started_at: p.started_utc,
            duration,
            bytes: p.counts_bytes.then_some(p.bytes),
            ok,
        };
        log_line(
            &mut st,
            &format!("phase end: {}", timing.summary().trim_end()),
        );
        let display = st.display;
        let interval = st.interval;
        let none_left = st.phases.is_empty();
        st.finished.push(timing.clone());
        drop(st);

        if display == Display::Tty && none_left {
            self.clear();
        }
        let summary = format!(
            "{} {}",
            if ok { "done:" } else { "FAILED:" },
            timing.summary()
        );
        match display {
            Display::Tty if duration >= TTY_SUMMARY_MIN || !ok => self.println(&summary),
            Display::Lines if p.lines_emitted > 0 || duration >= interval => {
                self.println(&format!("progress: {summary}"))
            }
            _ => {}
        }
        Some(duration)
    }

    fn with_top(&self, f: impl FnOnce(&mut ActivePhase)) {
        let mut st = self.lock();
        if let Some(p) = st.phases.last_mut() {
            f(p);
        }
    }

    fn with_phase(&self, id: u64, f: impl FnOnce(&mut ActivePhase)) {
        let mut st = self.lock();
        if let Some(p) = st.phases.iter_mut().find(|p| p.id == id) {
            f(p);
        }
    }

    fn add_bytes(&self, n: u64) {
        let now = Instant::now();
        self.with_top(|p| {
            p.counts_bytes = true;
            p.bytes += n;
            p.first_byte_at.get_or_insert(now);
            p.last_byte_at = now;
            p.stall_logged = false;
        });
    }

    fn start_wait(&self, what: String) -> u64 {
        let mut st = self.lock();
        let id = Self::next_id(&mut st);
        st.waits.push(ActiveWait {
            id,
            what,
            started: Instant::now(),
            announced: false,
        });
        id
    }

    fn end_wait(&self, id: u64) {
        let mut st = self.lock();
        let Some(idx) = st.waits.iter().position(|w| w.id == id) else {
            return;
        };
        let w = st.waits.remove(idx);
        let took = w.started.elapsed();
        if took >= WAIT_LOG_THRESHOLD {
            log_line(
                &mut st,
                &format!("wait end: {} after {}", w.what, format_duration(took)),
            );
        }
        // The stall clock restarts: the time was spent on a named wait.
        let now = Instant::now();
        if let Some(p) = st.phases.last_mut() {
            p.last_byte_at = now;
        }
    }

    /// One tick of the background thread: announce waits that crossed the
    /// threshold, poll, detect a stall, then draw or print.
    fn tick(&self, now: Instant) {
        let mut st = self.lock();
        let mut notes = Vec::new();
        for w in st.waits.iter_mut() {
            if !w.announced && now.duration_since(w.started) >= WAIT_LOG_THRESHOLD {
                w.announced = true;
                notes.push(format!(
                    "wait start: {} ({} so far)",
                    w.what,
                    format_duration(now.duration_since(w.started))
                ));
            }
        }
        let waiting = !st.waits.is_empty();
        if let Some(p) = st.phases.last_mut() {
            if let Some(poll) = p.poll.as_mut() {
                if let Some(b) = poll() {
                    if b != p.bytes {
                        p.first_byte_at.get_or_insert(now);
                        p.last_byte_at = now;
                        p.stall_logged = false;
                    }
                    p.bytes = b;
                    p.counts_bytes = true;
                }
            }
            if p.counts_bytes
                && !waiting
                && !p.stall_logged
                && now.duration_since(p.last_byte_at) >= STALL_THRESHOLD
            {
                p.stall_logged = true;
                notes.push(format!(
                    "stall: phase {} has moved no bytes for {} and names no wait",
                    p.name,
                    format_duration(now.duration_since(p.last_byte_at))
                ));
            }
        }
        for n in &notes {
            log_line(&mut st, n);
        }

        let line = status_line(&st, now);
        let display = st.display;
        let interval = st.interval;
        let mut periodic = None;
        if let Some(p) = st.phases.last_mut() {
            if now.duration_since(p.last_line_at) >= interval {
                p.last_line_at = now;
                p.lines_emitted += 1;
                periodic = line.clone();
            }
        }
        if let Some(l) = &periodic {
            log_line(&mut st, &format!("progress: {l}"));
        }
        drop(st);

        match display {
            Display::Tty => match line {
                Some(l) => self.draw(&l),
                None => self.clear(),
            },
            Display::Lines => {
                for n in &notes {
                    self.println(&format!("progress: {n}"));
                }
                if let Some(l) = periodic {
                    self.println(&format!("progress: {l}"));
                }
            }
            Display::Off => {}
        }
    }
}

/// The status line for the innermost phase, or `None` when no phase runs.
fn status_line(st: &State, now: Instant) -> Option<String> {
    let p = st.phases.last()?;
    let mut s = p.name.clone();
    if p.counts_bytes {
        match p.total {
            Some(t) if t > 0 => s.push_str(&format!(
                " {} of {} ({:.1}%)",
                format_bytes(p.bytes),
                format_bytes(t),
                (p.bytes as f64 / t as f64 * 100.0).min(100.0)
            )),
            _ => s.push_str(&format!(" {}", format_bytes(p.bytes))),
        }
        if let Some(first) = p.first_byte_at {
            let moving = now.duration_since(first);
            if let Some(r) = rate(p.bytes, moving) {
                s.push_str(&format!(", {r}"));
                if let Some(t) = p.total {
                    if t > p.bytes && moving.as_secs_f64() > 0.0 && p.bytes > 0 {
                        let per_sec = p.bytes as f64 / moving.as_secs_f64();
                        let eta = Duration::from_secs_f64((t - p.bytes) as f64 / per_sec);
                        s.push_str(&format!(", ETA {}", format_duration(eta)));
                    }
                }
            }
        }
    }
    s.push_str(&format!(
        ", elapsed {}",
        format_duration(now.duration_since(p.started))
    ));
    if let Some(item) = &p.item {
        s.push_str(&format!(" — {item}"));
    }
    if let Some(w) = st.waits.last() {
        let waited = now.duration_since(w.started);
        if waited >= Duration::from_secs(1) {
            s.push_str(&format!(
                " — waiting on {} ({})",
                w.what,
                format_duration(waited)
            ));
        }
    }
    Some(s)
}

fn log_line(st: &mut State, msg: &str) {
    if let Some(f) = st.log.as_mut() {
        let _ = writeln!(
            f,
            "{} {msg}",
            Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
        );
    }
}

// --- stderr: one status line, shared with the tracing writer ------------

/// Whether a status line is currently drawn on stderr (TTY display only).
/// Process-global because stderr is.
static STATUS_DRAWN: Mutex<bool> = Mutex::new(false);

fn drawn() -> MutexGuard<'static, bool> {
    STATUS_DRAWN.lock().unwrap_or_else(|e| e.into_inner())
}

fn terminal_width() -> usize {
    // SAFETY: TIOCGWINSZ fills a plain-old-data struct we own.
    let mut ws: nix::libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { nix::libc::ioctl(2, nix::libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_col > 0 {
        ws.ws_col as usize
    } else {
        100
    }
}

fn stderr_draw(line: &str) {
    let width = terminal_width().saturating_sub(1).max(20);
    let shown: String = line.chars().take(width).collect();
    let mut d = drawn();
    let mut err = io::stderr().lock();
    let _ = write!(err, "\r\x1b[2K{shown}");
    let _ = err.flush();
    *d = true;
}

fn stderr_clear() {
    let mut d = drawn();
    if *d {
        let mut err = io::stderr().lock();
        let _ = write!(err, "\r\x1b[2K");
        let _ = err.flush();
        *d = false;
    }
}

/// Print one whole line on stderr, first clearing a drawn status line (the
/// next tick redraws it below).
pub fn stderr_println(line: &str) {
    let mut d = drawn();
    let mut err = io::stderr().lock();
    if *d {
        let _ = write!(err, "\r\x1b[2K");
        *d = false;
    }
    let _ = writeln!(err, "{line}");
}

/// A stderr writer that clears a drawn status line before it writes, so a
/// warning printed mid-phase starts on a clean line. `main` gives it to the
/// tracing subscriber.
pub struct ClearingStderr;

impl Write for ClearingStderr {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut d = drawn();
        let mut err = io::stderr().lock();
        if *d {
            err.write_all(b"\r\x1b[2K")?;
            *d = false;
        }
        err.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

// --- the session log as a tracing sink -----------------------------------

/// The recorder `main` opened, for the tracing tee: events can come from any
/// thread, the thread-local recorder is only on the main one.
static GLOBAL: Mutex<Option<Arc<Recorder>>> = Mutex::new(None);

/// A writer into the open session log, or nowhere when no session is open.
/// `main` gives it to the tracing subscriber's file layer.
pub struct SessionLogWriter;

impl Write for SessionLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let rec = GLOBAL.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(rec) = rec {
            let mut st = rec.lock();
            if let Some(f) = st.log.as_mut() {
                f.write_all(buf)?;
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Record, in the session `main` opened, that the process is about to exit
/// with `code` without unwinding (a `process::exit` skips the session's own
/// end line), and clear a drawn status line.
pub fn note_exit(code: i32) {
    let rec = GLOBAL.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(rec) = rec {
        let mut st = rec.lock();
        log_line(
            &mut st,
            &format!(
                "session exit with code {code} after {}",
                format_duration(rec.started.elapsed())
            ),
        );
        if let Some(f) = st.log.as_mut() {
            let _ = f.flush();
        }
    }
    stderr_clear();
}

// --- the thread's recorder -----------------------------------------------

thread_local! {
    static CURRENT: RefCell<Option<Arc<Recorder>>> = const { RefCell::new(None) };
}

fn current() -> Option<Arc<Recorder>> {
    CURRENT.with(|c| c.borrow().clone())
}

/// An open session: the recorder installed on this thread, the ticker, and
/// the log file. Dropping it ends the session.
pub struct SessionGuard {
    rec: Arc<Recorder>,
    previous: Option<Arc<Recorder>>,
    global: bool,
    stop: Arc<AtomicBool>,
    ticker: Option<JoinHandle<()>>,
}

impl SessionGuard {
    /// The session's id — its log file's name without `.log`.
    pub fn session_id(&self) -> &str {
        &self.rec.session_id
    }

    /// The session log's path, when one could be created.
    pub fn log_path(&self) -> Option<&Path> {
        self.rec.log_path.as_deref()
    }

    /// Record how the command ended — `ok`, or `failed — <error>` — just
    /// before the session's end line (issue #393), so `tapectl status` can
    /// tell a finished session's outcome from its log alone. Only the
    /// error's first line is kept.
    pub fn note_result(&self, result: std::result::Result<(), &str>) {
        let line = match result {
            Ok(()) => "session result: ok".to_string(),
            Err(e) => format!(
                "session result: failed — {}",
                e.lines().next().unwrap_or("").trim()
            ),
        };
        let mut st = self.rec.lock();
        log_line(&mut st, &line);
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.ticker.take() {
            let _ = t.join();
        }
        {
            let mut st = self.rec.lock();
            let open: Vec<String> = st.phases.iter().map(|p| p.name.clone()).collect();
            for name in open {
                log_line(&mut st, &format!("phase left open at session end: {name}"));
            }
            let total = self.rec.started.elapsed();
            log_line(
                &mut st,
                &format!("session end after {}", format_duration(total)),
            );
            if let Some(f) = st.log.as_mut() {
                let _ = f.flush();
            }
        }
        self.rec.clear();
        if self.global {
            let mut g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
            if g.as_ref().is_some_and(|r| Arc::ptr_eq(r, &self.rec)) {
                *g = None;
            }
        }
        let prev = self.previous.take();
        CURRENT.with(|c| *c.borrow_mut() = prev);
    }
}

/// The session id for `command` started at `at` by process `pid`:
/// `20260930T120000Z-volume-write-L6-0001-4242`.
pub fn session_id_for(command: &str, at: DateTime<Utc>, pid: u32) -> String {
    let slug: String = command
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("{}-{slug}-{pid}", at.format("%Y%m%dT%H%M%SZ"))
}

/// The interval for plain lines: [`INTERVAL_ENV`] when it is a whole number
/// of seconds of at least 1, else [`DEFAULT_INTERVAL`].
pub fn interval_from_env(value: Option<&str>) -> Duration {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s >= 1)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_INTERVAL)
}

/// Open a session for `command` on this thread: the log file under
/// `logs_dir` (none when `logs_dir` is `None` or the file cannot be
/// created — a warning, never a failure), the recorder, and the ticker.
/// `global` also makes it the tracing tee's target; only `main` passes
/// `true`.
pub fn start_session(
    logs_dir: Option<&Path>,
    command: &str,
    display: Display,
    global: bool,
) -> SessionGuard {
    start_session_diverted(logs_dir, command, display, global, None)
}

fn start_session_diverted(
    logs_dir: Option<&Path>,
    command: &str,
    display: Display,
    global: bool,
    divert: Option<Arc<Mutex<Vec<String>>>>,
) -> SessionGuard {
    let started_utc = Utc::now();
    let session_id = session_id_for(command, started_utc, std::process::id());
    let (log, log_path) = match logs_dir {
        Some(dir) => {
            let path = dir.join(format!("{session_id}.log"));
            match open_private_log(&path) {
                Ok(f) => (Some(f), Some(path)),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "cannot create the session log; continuing without one"
                    );
                    (None, None)
                }
            }
        }
        None => (None, None),
    };
    let interval = interval_from_env(std::env::var(INTERVAL_ENV).ok().as_deref());
    let rec = Arc::new(Recorder {
        state: Mutex::new(State {
            log,
            display,
            interval,
            phases: Vec::new(),
            finished: Vec::new(),
            waits: Vec::new(),
            next_id: 0,
        }),
        session_id,
        log_path,
        started: Instant::now(),
        divert,
    });
    {
        let mut st = rec.lock();
        log_line(
            &mut st,
            &format!(
                "session start: {command} (tapectl {}, pid {}, display {:?})",
                crate::build_info::VERSION,
                std::process::id(),
                display
            ),
        );
    }
    let previous = CURRENT.with(|c| c.borrow_mut().replace(rec.clone()));
    if global {
        *GLOBAL.lock().unwrap_or_else(|e| e.into_inner()) = Some(rec.clone());
    }
    let stop = Arc::new(AtomicBool::new(false));
    let has_log = rec.log_path.is_some();
    let ticker = if display != Display::Off || has_log {
        let rec_t = rec.clone();
        let stop_t = stop.clone();
        let tick = if display == Display::Tty {
            Duration::from_millis(250)
        } else {
            Duration::from_millis(500)
        };
        std::thread::Builder::new()
            .name("tapectl-progress".into())
            .spawn(move || {
                while !stop_t.load(Ordering::SeqCst) {
                    std::thread::sleep(tick);
                    if stop_t.load(Ordering::SeqCst) {
                        break;
                    }
                    rec_t.tick(Instant::now());
                }
            })
            .ok()
    } else {
        None
    };
    SessionGuard {
        rec,
        previous,
        global,
        stop,
        ticker,
    }
}

// The mode a new session log is created with: 0600, or 0640 once `main`
// has found an `[ops] group` (issue #393) — the logs directory is then
// setgid to that group, so the file takes it and its members can read it.
// Per thread, like the recorder itself: `main` sets it on the thread that
// then starts the session, and parallel tests cannot see each other's.
thread_local! {
    static LOG_MODE: std::cell::Cell<u32> = const { std::cell::Cell::new(0o600) };
}

/// Share the session logs this thread's sessions write with the logs
/// directory's group (0640) instead of keeping them to the user (0600).
/// Call before [`start_session`], on the same thread.
pub fn share_logs_with_group(share: bool) {
    LOG_MODE.with(|m| m.set(if share { 0o640 } else { 0o600 }));
}

fn open_private_log(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mode = LOG_MODE.with(|m| m.get());
    let f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(mode)
        .open(path)?;
    // The umask can take the group bit away at create; set it outright.
    if mode != 0o600 {
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    Ok(f)
}

/// A recorder with no log file, no display and no ticker, installed on this
/// thread — for tests that assert on the phases an operation records.
pub fn start_capture() -> SessionGuard {
    start_session(None, "capture", Display::Off, false)
}

// --- phases ------------------------------------------------------------------

/// An open phase. End it with [`Phase::done`]; dropped without that (an
/// error unwound past it) it ends as failed.
#[must_use = "a phase ends when this guard is dropped"]
pub struct Phase {
    rec: Option<Arc<Recorder>>,
    id: u64,
    ended: bool,
}

/// Begin a phase named `name`, with `total` bytes to move when that is
/// known. A no-op on a thread with no session.
pub fn phase(name: &str, total: Option<u64>) -> Phase {
    let rec = current();
    let id = rec
        .as_ref()
        .map(|r| r.start_phase(name, total))
        .unwrap_or(0);
    Phase {
        rec,
        id,
        ended: false,
    }
}

impl Phase {
    /// Set (or correct) the byte total.
    pub fn set_total(&self, total: u64) {
        if let Some(r) = &self.rec {
            r.with_phase(self.id, |p| {
                p.total = Some(total);
                p.counts_bytes = true;
            });
        }
    }

    /// Name what this phase is working on now (a slice, a unit, a file).
    pub fn item(&self, item: impl Into<String>) {
        if let Some(r) = &self.rec {
            let item = item.into();
            r.with_phase(self.id, |p| p.item = Some(item));
        }
    }

    /// Let the ticker learn this phase's byte count by asking `f` — for work
    /// done by a subprocess (dar), where no read passes through tapectl.
    pub fn poll(&self, f: impl FnMut() -> Option<u64> + Send + 'static) {
        if let Some(r) = &self.rec {
            r.with_phase(self.id, |p| {
                p.poll = Some(Box::new(f));
                p.counts_bytes = true;
            });
        }
    }

    /// End the phase as successful; its duration, when a session is open.
    pub fn done(mut self) -> Option<Duration> {
        self.ended = true;
        self.rec.as_ref().and_then(|r| r.end_phase(self.id, true))
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        if !self.ended {
            if let Some(r) = &self.rec {
                r.end_phase(self.id, false);
            }
        }
    }
}

/// Count `n` bytes moved by the innermost phase. A no-op with no session.
pub fn add_bytes(n: u64) {
    if let Some(r) = current() {
        r.add_bytes(n);
    }
}

/// Set the innermost phase's byte total, for code that learns it only after
/// the phase began.
pub fn set_total(total: u64) {
    if let Some(r) = current() {
        r.with_top(|p| {
            p.total = Some(total);
            p.counts_bytes = true;
        });
    }
}

/// Name what the innermost phase is working on now.
pub fn item(item: impl Into<String>) {
    if let Some(r) = current() {
        let item = item.into();
        r.with_top(|p| p.item = Some(item));
    }
}

/// Write one line to the session log (nowhere with no session).
pub fn log(msg: &str) {
    if let Some(r) = current() {
        let mut st = r.lock();
        log_line(&mut st, msg);
    }
}

/// Log a single blocking call that took at least [`WAIT_LOG_THRESHOLD`] —
/// for calls too frequent to wrap in a [`wait`] each (one tape block).
pub fn note_if_slow(what: &str, took: Duration) {
    if took >= WAIT_LOG_THRESHOLD {
        log(&format!("slow: {what} took {}", format_duration(took)));
    }
}

/// Every phase ended on this thread's session since the last call, oldest
/// first — for the caller to record. Empty with no session.
pub fn drain() -> Vec<PhaseTiming> {
    match current() {
        Some(r) => std::mem::take(&mut r.lock().finished),
        None => Vec::new(),
    }
}

/// This thread's session id, when a session is open.
pub fn session_id() -> Option<String> {
    current().map(|r| r.session_id.clone())
}

// --- worker threads ----------------------------------------------------------

/// This thread's session, to carry onto a worker thread (issue #390).
///
/// The recorder is per thread, so a thread the session's own thread spawns
/// starts with none, and everything it calls here — [`log`],
/// [`note_if_slow`], [`wait`] — would be a silent no-op. The overlapped I/O
/// pipeline (`crate::pipeline`) reads the staged file, hashes it and reads
/// the tape on worker threads; each takes one of these and [`Handle::enter`]s
/// it, so a slow disk read or tape block on a worker lands in the same
/// session log as it did when the work ran on the session's thread.
///
/// Byte counts are deliberately NOT routed this way: the pipeline counts
/// bytes once, on the session's thread, where they reach the store or the
/// sink — a worker running ahead by a queue's worth would make the phase
/// claim bytes the tape has not yet taken.
#[derive(Clone, Default)]
pub struct Handle(Option<Arc<Recorder>>);

/// The calling thread's session, as a [`Handle`] (empty with no session).
pub fn handle() -> Handle {
    Handle(current())
}

impl Handle {
    /// Install this session on the calling thread until the returned guard
    /// drops. A no-op for an empty handle.
    pub fn enter(&self) -> Entered {
        let previous = CURRENT.with(|c| std::mem::replace(&mut *c.borrow_mut(), self.0.clone()));
        Entered { previous }
    }
}

/// A [`Handle`] installed on a worker thread; dropping it restores what the
/// thread had before.
#[must_use = "the session is installed only while this guard lives"]
pub struct Entered {
    previous: Option<Arc<Recorder>>,
}

impl Drop for Entered {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CURRENT.with(|c| *c.borrow_mut() = previous);
    }
}

// --- waits -------------------------------------------------------------------

/// An open wait. See [`wait`].
#[must_use = "a wait ends when this guard is dropped"]
pub struct Wait {
    rec: Option<Arc<Recorder>>,
    id: u64,
}

/// Begin waiting on `what` (a rewind, an `sg_logs` run, a busy catalog).
/// Logged if it lasts [`WAIT_LOG_THRESHOLD`] or more — when it crosses it,
/// and when the returned guard drops. A no-op with no session.
pub fn wait(what: impl FnOnce() -> String) -> Wait {
    let rec = current();
    let id = rec.as_ref().map(|r| r.start_wait(what())).unwrap_or(0);
    Wait { rec, id }
}

/// Run `f` as a [`wait`] on `what`.
pub fn waited<T>(what: impl FnOnce() -> String, f: impl FnOnce() -> T) -> T {
    let _w = wait(what);
    f()
}

impl Drop for Wait {
    fn drop(&mut self) {
        if let Some(r) = &self.rec {
            r.end_wait(self.id);
        }
    }
}

// --- adapters ------------------------------------------------------------

/// A reader that counts every byte read through it with [`add_bytes`].
pub struct CountingReader<R>(pub R);

impl<R: io::Read> io::Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.0.read(buf)?;
        add_bytes(n as u64);
        Ok(n)
    }
}

/// A writer that counts every byte written through it with [`add_bytes`].
pub struct CountingWriter<W>(pub W);

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.0.write(buf)?;
        add_bytes(n as u64);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

// --- formatting --------------------------------------------------------------

/// Binary units, as every measured data size in tapectl is shown.
pub fn format_bytes(b: u64) -> String {
    crate::util::format_bytes_binary(b.min(i64::MAX as u64) as i64)
}

/// `"145.2 MiB/s"`, or `None` for a span too short to mean anything.
fn rate(bytes: u64, over: Duration) -> Option<String> {
    let secs = over.as_secs_f64();
    if secs < 0.5 || bytes == 0 {
        return None;
    }
    Some(format!("{}/s", format_bytes((bytes as f64 / secs) as u64)))
}

/// `"850 ms"`, `"42.0 s"`, `"3m 05s"`, `"2h 20m"`.
pub fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        return format!("{ms} ms");
    }
    let secs = d.as_secs();
    if secs < 60 {
        return format!("{:.1} s", d.as_secs_f64());
    }
    if secs < 3600 {
        return format!("{}m {:02}s", secs / 60, secs % 60);
    }
    format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session whose display output is collected instead of printed,
    /// with no ticker: the test drives `tick` itself.
    fn diverted(display: Display) -> (SessionGuard, Arc<Mutex<Vec<String>>>) {
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut s = start_session_diverted(None, "t", display, false, Some(out.clone()));
        s.stop.store(true, Ordering::SeqCst);
        if let Some(t) = s.ticker.take() {
            let _ = t.join();
        }
        (s, out)
    }

    #[test]
    fn plain_lines_come_once_per_interval_and_a_long_phase_leaves_a_summary() {
        let (_s, out) = diverted(Display::Lines);
        let p = phase("write", Some(1 << 30));
        p.item("file 5 of 14 (slice)");
        let rec = current().unwrap();
        let t0 = Instant::now();
        {
            let mut st = rec.lock();
            let ph = &mut st.phases[0];
            ph.started = t0 - Duration::from_secs(31);
            ph.last_line_at = t0 - Duration::from_secs(31);
            ph.first_byte_at = Some(t0 - Duration::from_secs(30));
            ph.bytes = 300 << 20;
        }
        rec.tick(t0);
        rec.tick(t0 + Duration::from_secs(1)); // inside the interval: silent
        p.done();
        let lines = out.lock().unwrap().clone();
        assert_eq!(lines.len(), 2, "{lines:#?}");
        assert_eq!(
            lines[0],
            "progress: write 300.0 MiB of 1.0 GiB (29.3%), 10.0 MiB/s, ETA 1m 12s, \
             elapsed 31.0 s — file 5 of 14 (slice)"
        );
        assert!(
            lines[1].starts_with("progress: done: write"),
            "{}",
            lines[1]
        );
        assert!(lines[1].contains("300.0 MiB"), "{}", lines[1]);
    }

    #[test]
    fn a_short_phase_prints_nothing_in_plain_mode() {
        let (_s, out) = diverted(Display::Lines);
        let p = phase("build", None);
        current().unwrap().tick(Instant::now());
        p.done();
        assert!(out.lock().unwrap().is_empty());
    }

    #[test]
    fn a_terminal_redraws_the_status_line_in_place() {
        let (_s, out) = diverted(Display::Tty);
        let p = phase("confirm", Some(100));
        add_bytes(50);
        current().unwrap().tick(Instant::now());
        p.done();
        let lines = out.lock().unwrap().clone();
        assert_eq!(
            lines.len(),
            1,
            "a sub-2-second phase leaves no summary: {lines:?}"
        );
        assert!(
            lines[0].starts_with("\rconfirm 50 B of 100 B (50.0%)"),
            "{lines:?}"
        );
    }

    #[test]
    fn quiet_prints_nothing() {
        let (_s, out) = diverted(Display::Off);
        let p = phase("write", Some(10));
        {
            let rec = current().unwrap();
            let mut st = rec.lock();
            st.phases[0].last_line_at = Instant::now() - Duration::from_secs(60);
        }
        current().unwrap().tick(Instant::now());
        p.done();
        assert!(out.lock().unwrap().is_empty());
    }

    #[test]
    fn display_is_off_under_quiet_redrawn_on_a_terminal_plain_otherwise() {
        assert_eq!(Display::choose(true, true, Some("xterm")), Display::Off);
        assert_eq!(Display::choose(false, true, Some("xterm")), Display::Tty);
        assert_eq!(Display::choose(false, true, None), Display::Tty);
        assert_eq!(Display::choose(false, true, Some("dumb")), Display::Lines);
        assert_eq!(Display::choose(false, false, Some("xterm")), Display::Lines);
    }

    #[test]
    fn without_a_session_everything_is_a_no_op() {
        let p = phase("nothing", Some(10));
        add_bytes(5);
        item("x");
        let _w = wait(|| "nothing".into());
        assert_eq!(p.done(), None);
        assert!(drain().is_empty());
        assert_eq!(session_id(), None);
    }

    #[test]
    fn phases_are_recorded_in_order_with_bytes_and_outcome() {
        let _s = start_capture();
        let a = phase("first", Some(100));
        add_bytes(60);
        add_bytes(40);
        a.done();
        {
            let _b = phase("second", None);
            // dropped without done(): an error unwound past it
        }
        let c = phase("third", None);
        c.done();
        let t = drain();
        let names: Vec<&str> = t.iter().map(|p| p.phase.as_str()).collect();
        assert_eq!(names, ["first", "second", "third"]);
        assert_eq!(t[0].bytes, Some(100));
        assert!(t[0].ok);
        assert_eq!(t[1].bytes, None);
        assert!(!t[1].ok);
        assert_eq!(t[1].outcome(), "failed");
        assert!(drain().is_empty(), "drain takes what it returns");
    }

    #[test]
    fn nested_phases_count_bytes_on_the_innermost() {
        let _s = start_capture();
        let outer = phase("outer", None);
        let inner = phase("inner", Some(8));
        add_bytes(8);
        inner.done();
        add_bytes(3);
        outer.done();
        let t = drain();
        assert_eq!(t[0].phase, "inner");
        assert_eq!(t[0].bytes, Some(8));
        assert_eq!(t[1].phase, "outer");
        assert_eq!(t[1].bytes, Some(3));
    }

    #[test]
    fn a_session_on_one_thread_is_invisible_to_another() {
        let _s = start_capture();
        std::thread::spawn(|| {
            assert_eq!(session_id(), None);
            phase("elsewhere", None).done();
        })
        .join()
        .unwrap();
        assert!(drain().is_empty());
    }

    #[test]
    fn the_log_names_phases_and_long_waits_with_utc_timestamps() {
        let dir = tempfile::TempDir::new().unwrap();
        let path;
        {
            let s = start_session(
                Some(dir.path()),
                "volume write L6-0001",
                Display::Off,
                false,
            );
            path = s.log_path().unwrap().to_path_buf();
            let p = phase("write", Some(4));
            add_bytes(4);
            // A wait past the threshold, without sleeping five seconds: end
            // it through the recorder with a back-dated start.
            let rec = current().unwrap();
            let id = rec.start_wait("tape rewind".into());
            rec.lock().waits[0].started = Instant::now() - Duration::from_secs(7);
            rec.tick(Instant::now());
            rec.end_wait(id);
            p.done();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.contains("-volume-write-L6-0001-"), "{name}");
        assert!(name.ends_with(".log"));
        for want in [
            "session start: volume write L6-0001",
            "phase start: write (4 B)",
            "wait start: tape rewind (7.0 s so far)",
            "wait end: tape rewind after 7.0 s",
            "phase end: write",
            "session end after",
        ] {
            assert!(text.contains(want), "missing {want:?} in:\n{text}");
        }
        for line in text.lines() {
            let ts = line.split(' ').next().unwrap();
            assert!(
                DateTime::parse_from_rfc3339(ts).is_ok() && ts.ends_with('Z'),
                "every line leads with a UTC timestamp: {line}"
            );
        }
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    /// Issue #393: with the logs shared (`[ops] group`) a session log is
    /// group-readable, it records how the command ended, and `tapectl
    /// status` reads that log back — the real writer, not a fixture.
    #[test]
    fn a_shared_log_is_group_readable_and_status_reads_its_outcome() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        share_logs_with_group(true);
        let path = {
            let session =
                start_session(Some(dir.path()), "stage create photos", Display::Off, false);
            let p = phase("archive", Some(10));
            add_bytes(10);
            p.done();
            session.note_result(Err("stopped by a signal: at slice 3\nmore"));
            session.log_path().unwrap().to_path_buf()
        };
        share_logs_with_group(false);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);

        let sessions = crate::cli::status::read_sessions(dir.path(), 5).unwrap();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.command.as_deref(), Some("stage create photos"));
        assert_eq!(
            s.state,
            crate::cli::status::State::Ended {
                outcome: "failed — stopped by a signal: at slice 3".into()
            }
        );
        assert_eq!(s.phases.len(), 1);
        assert!(s.phases[0].starts_with("archive"), "{:?}", s.phases);

        // And the default stays private.
        let private = start_session(Some(dir.path()), "volume verify L", Display::Off, false);
        let p = private.log_path().unwrap().to_path_buf();
        drop(private);
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    /// Issue #390: a worker that enters the session's handle logs into it,
    /// a plain spawned thread still does not, and leaving restores the
    /// worker's own (empty) state.
    #[test]
    fn a_worker_that_enters_the_handle_logs_into_the_session() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = start_session(Some(dir.path()), "t", Display::Off, false);
        let path = s.log_path().unwrap().to_path_buf();
        let h = handle();
        std::thread::spawn(move || {
            {
                let _in = h.enter();
                note_if_slow("one staged-file read", Duration::from_secs(6));
            }
            log("after leaving: not in the session");
            assert_eq!(session_id(), None);
        })
        .join()
        .unwrap();
        std::thread::spawn(|| log("never entered: not in the session"))
            .join()
            .unwrap();
        drop(s);
        let text = std::fs::read_to_string(path).unwrap();
        assert!(
            text.contains("slow: one staged-file read took 6.0 s"),
            "{text}"
        );
        assert!(!text.contains("not in the session"), "{text}");
    }

    #[test]
    fn a_short_wait_is_not_logged() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = start_session(Some(dir.path()), "t", Display::Off, false);
        let path = s.log_path().unwrap().to_path_buf();
        waited(|| "quick thing".into(), || ());
        drop(s);
        let text = std::fs::read_to_string(path).unwrap();
        assert!(!text.contains("quick thing"), "{text}");
    }

    #[test]
    fn a_stalled_byte_counting_phase_names_itself() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = start_session(Some(dir.path()), "t", Display::Off, false);
        let path = s.log_path().unwrap().to_path_buf();
        let p = phase("write", Some(10));
        let rec = current().unwrap();
        rec.lock().phases[0].last_byte_at = Instant::now() - STALL_THRESHOLD;
        rec.tick(Instant::now());
        p.done();
        drop(s);
        let text = std::fs::read_to_string(path).unwrap();
        assert!(
            text.contains("stall: phase write has moved no bytes"),
            "{text}"
        );
    }

    #[test]
    fn status_line_shows_bytes_rate_eta_item_and_wait() {
        let _s = start_capture();
        let p = phase("write", Some(1000));
        p.item("slice 3/40");
        let rec = current().unwrap();
        let now = Instant::now();
        {
            let mut st = rec.lock();
            let ph = &mut st.phases[0];
            ph.bytes = 250;
            ph.first_byte_at = Some(now - Duration::from_secs(10));
            ph.started = now - Duration::from_secs(12);
        }
        let _w = wait(|| "tape rewind".into());
        rec.lock().waits[0].started = now - Duration::from_secs(3);
        let line = status_line(&rec.lock(), now).unwrap();
        assert_eq!(
            line,
            "write 250 B of 1000 B (25.0%), 25 B/s, ETA 30.0 s, elapsed 12.0 s \
             — slice 3/40 — waiting on tape rewind (3.0 s)"
        );
        drop(_w);
        p.done();
    }

    #[test]
    fn a_polled_phase_takes_its_byte_count_from_the_poll() {
        let _s = start_capture();
        let p = phase("dar", Some(100));
        p.poll(|| Some(42));
        current().unwrap().tick(Instant::now());
        p.done();
        assert_eq!(drain()[0].bytes, Some(42));
    }

    #[test]
    fn interval_env_accepts_whole_seconds_only() {
        assert_eq!(interval_from_env(None), DEFAULT_INTERVAL);
        assert_eq!(interval_from_env(Some("2")), Duration::from_secs(2));
        assert_eq!(interval_from_env(Some("0")), DEFAULT_INTERVAL);
        assert_eq!(interval_from_env(Some("x")), DEFAULT_INTERVAL);
    }

    #[test]
    fn durations_read_at_every_scale() {
        assert_eq!(format_duration(Duration::from_millis(850)), "850 ms");
        assert_eq!(format_duration(Duration::from_secs(42)), "42.0 s");
        assert_eq!(format_duration(Duration::from_secs(185)), "3m 05s");
        assert_eq!(format_duration(Duration::from_secs(8420)), "2h 20m");
    }

    #[test]
    fn session_ids_are_utc_stamped_and_filesystem_safe() {
        let at = DateTime::parse_from_rfc3339("2026-09-30T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            session_id_for("volume write L6/0001", at, 42),
            "20260930T120000Z-volume-write-L6-0001-42"
        );
    }
}
