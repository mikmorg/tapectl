//! The write-shaping experiment knobs (issue #488; the write-shaping note,
//! `docs/research/2026-10-20-write-shaping.md` §9 item 6, whose text records
//! these semantics as built).
//!
//! **A research build only.** The knobs act only in a build with the cargo
//! feature `research`, which is off by default and in no production build:
//! the shaping itself (§12 decisions 5-10) is NOT ruled, so no production
//! binary can turn it on. In any other build [`knobs`] is always empty, the
//! shaper type cannot be constructed (`Shaper` is uninhabited), and each
//! `TAPECTL_EXP_*` variable that is set draws one warning line saying this
//! build ignores it.
//!
//! **Read once, announced loudly.** The variables are read once per process,
//! the first time anything asks ([`knobs`]), following
//! `TAPECTL_TEST_PAUSE_AFTER_PLAN`'s pattern (environment variables, never
//! config keys). [`announce`] prints them on stderr once per process and
//! into the session log once per session, and the write pipeline calls it
//! when a write's pool starts, so every research write's log says what it
//! ran with. A research build announces itself even with no knob set, so a
//! research binary is never mistaken for a production one.
//!
//! **The knobs** (decimal MB, 10^6 bytes, as the note's arithmetic is):
//!
//! - `TAPECTL_EXP_QUEUE_BYTES=<size>`: the WRITE pipeline's queue bound
//!   instead of `pipeline::QUEUE_BYTES` (256 MiB). Binary suffixes `K`, `M`,
//!   `G`, `T` (`KiB`... also accepted), or plain bytes; at least two tape
//!   blocks. The read pipeline (confirm, verify, restore) keeps the
//!   constant. Without `TAPECTL_EXP_CROSSFILE` (not built) the queue still
//!   empties at every file boundary, so it can hold at most one file.
//! - `TAPECTL_EXP_PACE_MBPS=<MB/s>`: a fixed pace (§5.2). The store is handed
//!   at most this many bytes a second, a token bucket with a burst of four
//!   blocks on the writer's side of the queue; the time it holds the writer
//!   back is the trace's `paced_ms`. The pace carries across files.
//! - `TAPECTL_EXP_PACE_ADAPTIVE=<floor>,<ceiling>,<step %>`: an adaptive
//!   pace (§5.3) within [floor, ceiling] MB/s, starting at
//!   `TAPECTL_EXP_PACE_MBPS` if set, else the ceiling. Every
//!   [`ADAPT_WINDOW`] of writing it steps down by `step` % when the store
//!   was starved mid-file (it waited for a block after a file's first) or
//!   the queue's lowest fill in the window was under [`ADAPT_LOW`], and up
//!   by `step` % when the lowest fill stayed at or over [`ADAPT_HIGH`];
//!   otherwise it holds. One step per window at most, as §5.3 advises
//!   ("10% at most every 30 s"). The two numbers in R10 of §10 ("150/90")
//!   are this knob's ceiling and floor.
//! - `TAPECTL_EXP_REFILL=<low>,<high>`: pause and refill (§5.4), fractions
//!   of the queue. When the queue, mid-file, falls under `low` after having
//!   reached it in this file (or the store was starved mid-file), the writer
//!   stops handing blocks to the store until the queue is back at `high`, or
//!   [`REFILL_MAX_PAUSE`] passes (under the drive's 30 s write-delay flush,
//!   §2.1). The time held is the trace's `paused_ms`.
//! - `TAPECTL_EXP_HASH=ring|sha2`: which SHA-256 `pass_count::Sha256` runs
//!   (§5.9): `ring` (the default, what production runs) or `sha2`'s `asm`
//!   (what production ran before #488), for R7's baseline. It applies to
//!   every hash in the process, staging's included.
//! - `TAPECTL_EXP_CROSSFILE` (carry the queue across files): NOT built — it
//!   needs the write session (`volume::session`) to run one reader and one
//!   hasher for the whole Layout. Set, it is reported as ignored.
//!
//! **"The queue's fill"** for the adaptive pace and the refill is the bytes
//! hashed and waiting for the store over what the queue can hold for the
//! rest of THIS file: the pool's capacity, or the file's untaken remainder
//! if smaller, so a file's end does not read as the queue draining. A file's
//! first fill-up, before the queue first reaches `low`, is not judged.

use std::sync::{Mutex, Once, OnceLock};

pub const QUEUE_BYTES_ENV: &str = "TAPECTL_EXP_QUEUE_BYTES";
pub const PACE_MBPS_ENV: &str = "TAPECTL_EXP_PACE_MBPS";
pub const PACE_ADAPTIVE_ENV: &str = "TAPECTL_EXP_PACE_ADAPTIVE";
pub const REFILL_ENV: &str = "TAPECTL_EXP_REFILL";
pub const HASH_ENV: &str = "TAPECTL_EXP_HASH";
pub const CROSSFILE_ENV: &str = "TAPECTL_EXP_CROSSFILE";

/// Every knob's variable, in the order they are announced.
pub const ALL_ENV: [&str; 6] = [
    QUEUE_BYTES_ENV,
    PACE_MBPS_ENV,
    PACE_ADAPTIVE_ENV,
    REFILL_ENV,
    HASH_ENV,
    CROSSFILE_ENV,
];

/// Whether this build has the knobs: the cargo feature `research`.
pub const BUILT: bool = cfg!(feature = "research");

/// How often the adaptive pace may step.
pub const ADAPT_WINDOW: std::time::Duration = std::time::Duration::from_secs(30);
/// The adaptive pace steps down when the queue's lowest fill in a window
/// was under this.
pub const ADAPT_LOW: f64 = 0.25;
/// ... and up when it stayed at or over this.
pub const ADAPT_HIGH: f64 = 0.75;
/// The longest a refill pause holds the writer.
pub const REFILL_MAX_PAUSE: std::time::Duration = std::time::Duration::from_secs(25);
/// The pace's burst, in blocks.
pub const PACE_BURST_BLOCKS: u64 = 4;

/// Which SHA-256 does the hashing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HashImpl {
    #[default]
    Ring,
    Sha2,
}

/// `TAPECTL_EXP_PACE_ADAPTIVE`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Adaptive {
    pub floor_mbps: f64,
    pub ceiling_mbps: f64,
    /// The step as a fraction (10 % is 0.10).
    pub step: f64,
}

/// `TAPECTL_EXP_REFILL`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Refill {
    pub low: f64,
    pub high: f64,
}

/// The knobs in force, parsed.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Knobs {
    pub queue_bytes: Option<u64>,
    pub pace_mbps: Option<f64>,
    pub adaptive: Option<Adaptive>,
    pub refill: Option<Refill>,
    pub hash: HashImpl,
    /// Each variable that was set but is not in force, and why.
    pub ignored: Vec<String>,
}

impl Knobs {
    /// Parse the knobs from `get` (the environment, or a test's map). A
    /// value that does not parse, or is out of range, is not used and is
    /// named in [`Knobs::ignored`] with why.
    pub fn parse(get: impl Fn(&str) -> Option<String>) -> Knobs {
        let mut k = Knobs::default();
        let mut ignore = |var: &str, v: &str, why: &str| {
            k.ignored.push(format!("{var}={v}: {why}"));
        };
        let mut queue_bytes = None;
        let mut pace_mbps = None;
        let mut adaptive = None;
        let mut refill = None;
        let mut hash = HashImpl::Ring;
        if let Some(v) = get(QUEUE_BYTES_ENV) {
            match parse_size(&v) {
                Some(n) if n > 0 => queue_bytes = Some(n),
                _ => ignore(QUEUE_BYTES_ENV, &v, "not a size (e.g. 4G, 512M, 268435456)"),
            }
        }
        if let Some(v) = get(PACE_MBPS_ENV) {
            match v.trim().parse::<f64>() {
                Ok(r) if r.is_finite() && r > 0.0 => pace_mbps = Some(r),
                _ => ignore(PACE_MBPS_ENV, &v, "not a rate above 0 in MB/s"),
            }
        }
        if let Some(v) = get(PACE_ADAPTIVE_ENV) {
            match numbers(&v).as_deref() {
                Some(&[floor, ceiling, step])
                    if floor > 0.0 && ceiling >= floor && step > 0.0 && step < 100.0 =>
                {
                    adaptive = Some(Adaptive {
                        floor_mbps: floor,
                        ceiling_mbps: ceiling,
                        step: step / 100.0,
                    })
                }
                _ => ignore(
                    PACE_ADAPTIVE_ENV,
                    &v,
                    "not <floor>,<ceiling>,<step %> with 0 < floor <= ceiling and 0 < step < 100",
                ),
            }
        }
        if let Some(v) = get(REFILL_ENV) {
            match numbers(&v).as_deref() {
                Some(&[low, high]) if (0.0..1.0).contains(&low) && high > low && high <= 1.0 => {
                    refill = Some(Refill { low, high })
                }
                _ => ignore(
                    REFILL_ENV,
                    &v,
                    "not <low>,<high> fractions with 0 <= low < high <= 1",
                ),
            }
        }
        if let Some(v) = get(HASH_ENV) {
            match v.trim() {
                "ring" => hash = HashImpl::Ring,
                "sha2" => hash = HashImpl::Sha2,
                _ => ignore(HASH_ENV, &v, "not ring or sha2"),
            }
        }
        if let Some(v) = get(CROSSFILE_ENV) {
            ignore(
                CROSSFILE_ENV,
                &v,
                "not built: carrying the queue across files needs the write session",
            );
        }
        k.queue_bytes = queue_bytes;
        k.pace_mbps = pace_mbps;
        k.adaptive = adaptive;
        k.refill = refill;
        k.hash = hash;
        k
    }

    /// Whether any knob shapes the feed (a pace, an adaptive pace or a
    /// refill): the write pipeline then runs a shaper.
    pub fn shapes(&self) -> bool {
        self.pace_mbps.is_some() || self.adaptive.is_some() || self.refill.is_some()
    }

    /// The banner's lines: what is in force, then what was ignored.
    pub fn describe(&self) -> Vec<String> {
        let mut out = vec![
            "RESEARCH BUILD (cargo feature `research`): the write-shaping knobs are live; \
             never use this binary for a production write"
                .to_string(),
        ];
        let mut any = false;
        let mut line = |s: String| {
            any = true;
            out.push(format!("  {s}"));
        };
        if let Some(q) = self.queue_bytes {
            line(format!(
                "{QUEUE_BYTES_ENV}: the write queue holds {q} bytes ({:.1} MiB)",
                q as f64 / (1024.0 * 1024.0)
            ));
        }
        if let Some(r) = self.pace_mbps {
            line(format!("{PACE_MBPS_ENV}: paced at {r} MB/s"));
        }
        if let Some(a) = self.adaptive {
            line(format!(
                "{PACE_ADAPTIVE_ENV}: adaptive pace in [{}, {}] MB/s, {:.0} % steps, one per {} s",
                a.floor_mbps,
                a.ceiling_mbps,
                a.step * 100.0,
                ADAPT_WINDOW.as_secs()
            ));
        }
        if let Some(r) = self.refill {
            line(format!(
                "{REFILL_ENV}: refill pause under {} of the queue, until {} (at most {} s)",
                r.low,
                r.high,
                REFILL_MAX_PAUSE.as_secs()
            ));
        }
        if self.hash == HashImpl::Sha2 {
            line(format!("{HASH_ENV}: hashing with sha2 (asm), not ring"));
        }
        for i in &self.ignored {
            line(format!("ignored: {i}"));
        }
        if !any {
            out.push("  no TAPECTL_EXP_* knob is set: the write runs as production's".into());
        }
        out
    }
}

/// A size: plain bytes, or a number with a binary suffix.
fn parse_size(v: &str) -> Option<u64> {
    let v = v.trim();
    let digits = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    let (n, unit) = v.split_at(digits);
    let n: u64 = n.parse().ok()?;
    let shift = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 0,
        "K" | "KIB" => 10,
        "M" | "MIB" => 20,
        "G" | "GIB" => 30,
        "T" | "TIB" => 40,
        _ => return None,
    };
    n.checked_mul(1u64 << shift)
}

fn numbers(v: &str) -> Option<Vec<f64>> {
    v.split(',')
        .map(|p| p.trim().parse::<f64>().ok().filter(|x| x.is_finite()))
        .collect()
}

/// The knobs in force. In a `research` build, the environment parsed once;
/// in any other build, always none.
pub fn knobs() -> &'static Knobs {
    static KNOBS: OnceLock<Knobs> = OnceLock::new();
    KNOBS.get_or_init(|| {
        if BUILT {
            Knobs::parse(|k| std::env::var(k).ok())
        } else {
            Knobs::default()
        }
    })
}

/// What [`announce`] says: the research banner, or one warning per set
/// variable that this build ignores (nothing when none is set).
pub fn announcement(built: bool, knobs: &Knobs, set: &[&str]) -> Vec<String> {
    if built {
        knobs.describe()
    } else {
        set.iter()
            .map(|var| {
                format!(
                    "warning: {var} is set, but this build ignores it: the write-shaping \
                     knobs exist only in a build with the cargo feature `research`"
                )
            })
            .collect()
    }
}

/// Say what the knobs are: on stderr once per process, and into the calling
/// thread's session log once per session. The write pipeline calls it when a
/// write's pool starts.
pub fn announce() {
    static SET: OnceLock<Vec<&'static str>> = OnceLock::new();
    let set = SET.get_or_init(|| {
        ALL_ENV
            .iter()
            .copied()
            .filter(|v| std::env::var_os(v).is_some())
            .collect()
    });
    let lines = announcement(BUILT, knobs(), set);
    if lines.is_empty() {
        return;
    }
    static STDERR: Once = Once::new();
    STDERR.call_once(|| {
        for l in &lines {
            crate::progress::stderr_println(l);
        }
    });
    static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());
    if let Some(id) = crate::progress::session_id() {
        let mut logged = LOGGED.lock().unwrap_or_else(|e| e.into_inner());
        if !logged.contains(&id) {
            logged.push(id);
            for l in &lines {
                crate::progress::log(l);
            }
        }
    }
}

// ── the shaper ───────────────────────────────────────────────────────────

pub(crate) use shaper::Shaper;

#[cfg(feature = "research")]
mod shaper {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::{Adaptive, Knobs, Refill, ADAPT_HIGH, ADAPT_LOW, ADAPT_WINDOW, REFILL_MAX_PAUSE};
    use crate::pipeline_trace::Gauges;

    /// The write's shaping, between the queue and the store: a pace, an
    /// adaptive pace and a refill pause, as the knobs ask. It lives as long
    /// as the write's pool, so the pace and the adaptive window carry
    /// across files.
    pub(crate) struct Shaper {
        gauges: Arc<Gauges>,
        /// Bytes a second; `None` unpaced.
        rate: Option<f64>,
        burst_bytes: f64,
        /// The token bucket as a virtual clock: when the bytes handed so far
        /// are paid for.
        paid_until: Option<Instant>,
        adaptive: Option<Adaptive>,
        window_start: Instant,
        window_min_fill: Option<f64>,
        window_starved: bool,
        refill: Option<Refill>,
        file: FileState,
    }

    #[derive(Default)]
    struct FileState {
        len: u64,
        taken: u64,
        capacity: u64,
        /// The queue has reached the low mark in this file, or the store
        /// was starved mid-file: falling under it now means something.
        armed: bool,
    }

    impl Shaper {
        pub(crate) fn for_write(knobs: &Knobs, gauges: Arc<Gauges>, block: u64) -> Option<Shaper> {
            if !knobs.shapes() {
                return None;
            }
            let start = knobs.pace_mbps.or(knobs.adaptive.map(|a| a.ceiling_mbps));
            let s = Shaper {
                gauges,
                rate: start.map(|r| r * 1e6),
                burst_bytes: (super::PACE_BURST_BLOCKS * block.max(1)) as f64,
                paid_until: None,
                adaptive: knobs.adaptive,
                window_start: Instant::now(),
                window_min_fill: None,
                window_starved: false,
                refill: knobs.refill,
                file: FileState::default(),
            };
            s.publish_rate();
            Some(s)
        }

        fn publish_rate(&self) {
            let mbps = self.rate.map(|r| r / 1e6).unwrap_or(0.0);
            self.gauges
                .pace_mbps_bits
                .store(mbps.to_bits(), Ordering::Relaxed);
        }

        pub(crate) fn begin_file(&mut self, len: u64, capacity: u64) {
            self.file = FileState {
                len,
                taken: 0,
                capacity,
                armed: false,
            };
        }

        /// The queue's fill for the rest of this file (see the module doc).
        fn fill(&self) -> f64 {
            let room = self
                .file
                .capacity
                .min(self.file.len.saturating_sub(self.file.taken));
            if room == 0 {
                return 1.0;
            }
            self.gauges.hashed_unwritten.load(Ordering::Relaxed) as f64 / room as f64
        }

        /// The store was starved: it waited for a block after the file's
        /// first.
        pub(crate) fn note_starved(&mut self) {
            if self.file.taken > 0 {
                self.file.armed = true;
                self.window_starved = true;
            }
        }

        /// The writer is about to take the next block: judge the queue, step
        /// the adaptive pace at a window's end, and hold for a refill.
        pub(crate) fn before_take(&mut self) {
            if self.file.taken == 0 {
                return; // a file's first block: the queue is still filling
            }
            let fill = self.fill();
            let low = self.refill.map(|r| r.low).unwrap_or(ADAPT_LOW);
            if fill >= low {
                self.file.armed = true;
            }
            if self.file.armed {
                self.window_min_fill = Some(self.window_min_fill.map_or(fill, |m| m.min(fill)));
            }
            self.adapt();
            if let Some(r) = self.refill {
                if self.file.armed && fill < r.low {
                    self.hold_for_refill(r.high);
                }
            }
        }

        fn adapt(&mut self) {
            let Some(a) = self.adaptive else {
                return;
            };
            if self.window_start.elapsed() < ADAPT_WINDOW {
                return;
            }
            if let Some(rate) = self.rate {
                let mbps = rate / 1e6;
                let next = match self.window_min_fill {
                    _ if self.window_starved => mbps * (1.0 - a.step),
                    Some(m) if m < ADAPT_LOW => mbps * (1.0 - a.step),
                    Some(m) if m >= ADAPT_HIGH => mbps * (1.0 + a.step),
                    _ => mbps,
                };
                let next = next.clamp(a.floor_mbps, a.ceiling_mbps);
                if next != mbps {
                    self.rate = Some(next * 1e6);
                    self.publish_rate();
                    crate::progress::log(&format!(
                        "research: adaptive pace {mbps:.1} -> {next:.1} MB/s (window: lowest \
                         fill {}, starved {})",
                        self.window_min_fill
                            .map(|m| format!("{m:.2}"))
                            .unwrap_or_else(|| "-".into()),
                        self.window_starved
                    ));
                }
            }
            self.window_start = Instant::now();
            self.window_min_fill = None;
            self.window_starved = false;
        }

        fn hold_for_refill(&mut self, high: f64) {
            let started = Instant::now();
            while self.fill() < high && started.elapsed() < REFILL_MAX_PAUSE {
                std::thread::sleep(Duration::from_millis(2));
            }
            let held = started.elapsed();
            self.gauges.paused_ns.fetch_add(
                u64::try_from(held.as_nanos()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }

        /// `n` bytes are about to go to the store: hold the writer to the
        /// pace.
        pub(crate) fn pace(&mut self, n: usize) {
            self.file.taken += n as u64;
            let Some(rate) = self.rate else {
                return;
            };
            let now = Instant::now();
            let burst = Duration::from_secs_f64(self.burst_bytes / rate);
            let floor = now.checked_sub(burst).unwrap_or(now);
            let from = self.paid_until.map_or(floor, |p| p.max(floor));
            let until = from + Duration::from_secs_f64(n as f64 / rate);
            self.paid_until = Some(until);
            if until > now {
                let wait = until - now;
                std::thread::sleep(wait);
                self.gauges.paced_ns.fetch_add(
                    u64::try_from(now.elapsed().as_nanos()).unwrap_or(u64::MAX),
                    Ordering::Relaxed,
                );
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn adaptive_shaper() -> (Shaper, Arc<Gauges>) {
            let gauges = Arc::new(Gauges::default());
            let knobs = Knobs {
                adaptive: Some(Adaptive {
                    floor_mbps: 90.0,
                    ceiling_mbps: 150.0,
                    step: 0.10,
                }),
                ..Knobs::default()
            };
            let mut s = Shaper::for_write(&knobs, gauges.clone(), 512 * 1024).unwrap();
            // Mid-file in a long file: the first block is behind us.
            s.begin_file(100 << 30, 256 << 20);
            s.file.taken = 1;
            (s, gauges)
        }

        fn mbps(s: &Shaper) -> f64 {
            s.rate.unwrap() / 1e6
        }

        /// A window's end: lowest fill under ADAPT_LOW (or a starved store)
        /// steps down, a window that stayed over ADAPT_HIGH steps up, one in
        /// between holds — each within [floor, ceiling], one step a window.
        #[test]
        fn the_adaptive_pace_steps_on_the_windows_lowest_fill() {
            let (mut s, g) = adaptive_shaper();
            assert_eq!(mbps(&s), 150.0, "it starts at the ceiling");
            let end_window = |s: &mut Shaper, fill_bytes: u64| {
                g.hashed_unwritten.store(fill_bytes, Ordering::Relaxed);
                s.window_start = Instant::now() - ADAPT_WINDOW - Duration::from_secs(1);
                s.before_take();
            };
            let cap = 256u64 << 20;
            // A file's first fill-up is not judged: under the low mark
            // before the queue ever reached it, nothing moves.
            end_window(&mut s, cap / 10);
            assert_eq!(mbps(&s), 150.0, "not armed yet");
            end_window(&mut s, cap); // arms; already at the ceiling
            assert_eq!(mbps(&s), 150.0);
            end_window(&mut s, cap / 10); // 0.10 < ADAPT_LOW
            assert!((mbps(&s) - 135.0).abs() < 1e-9, "{}", mbps(&s));
            end_window(&mut s, cap / 2); // between the marks
            assert!((mbps(&s) - 135.0).abs() < 1e-9, "held");
            end_window(&mut s, cap); // over ADAPT_HIGH
            assert!((mbps(&s) - 148.5).abs() < 1e-9, "{}", mbps(&s));
            end_window(&mut s, cap);
            assert_eq!(mbps(&s), 150.0, "clamped to the ceiling");
            s.note_starved();
            end_window(&mut s, cap);
            assert!(
                (mbps(&s) - 135.0).abs() < 1e-9,
                "a starved store steps down"
            );
            for _ in 0..20 {
                end_window(&mut s, 0);
            }
            assert_eq!(mbps(&s), 90.0, "never under the floor");
            // Before a window ends nothing moves, whatever the fill.
            g.hashed_unwritten.store(0, Ordering::Relaxed);
            s.window_start = Instant::now();
            s.before_take();
            assert_eq!(mbps(&s), 90.0);
            assert_eq!(
                f64::from_bits(g.pace_mbps_bits.load(Ordering::Relaxed)),
                90.0,
                "the trace sees the pace in force"
            );
        }
    }
}

#[cfg(not(feature = "research"))]
mod shaper {
    use std::sync::Arc;

    use super::Knobs;
    use crate::pipeline_trace::Gauges;

    /// No shaping outside a `research` build: this type has no value, so a
    /// production build provably never shapes a write.
    pub(crate) enum Shaper {}

    impl Shaper {
        pub(crate) fn for_write(
            _knobs: &Knobs,
            _gauges: Arc<Gauges>,
            _block: u64,
        ) -> Option<Shaper> {
            None
        }

        pub(crate) fn begin_file(&mut self, _len: u64, _capacity: u64) {
            match *self {}
        }

        pub(crate) fn note_starved(&mut self) {
            match *self {}
        }

        pub(crate) fn before_take(&mut self) {
            match *self {}
        }

        pub(crate) fn pace(&mut self, _n: usize) {
            match *self {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn parse(vars: &[(&str, &str)]) -> Knobs {
        let m: BTreeMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Knobs::parse(|k| m.get(k).cloned())
    }

    #[test]
    fn every_knob_parses_as_documented() {
        let k = parse(&[
            (QUEUE_BYTES_ENV, "4G"),
            (PACE_MBPS_ENV, "140"),
            (PACE_ADAPTIVE_ENV, "90, 150, 10"),
            (REFILL_ENV, "0.10,0.90"),
            (HASH_ENV, "sha2"),
        ]);
        assert_eq!(k.queue_bytes, Some(4 << 30));
        assert_eq!(k.pace_mbps, Some(140.0));
        assert_eq!(
            k.adaptive,
            Some(Adaptive {
                floor_mbps: 90.0,
                ceiling_mbps: 150.0,
                step: 0.10
            })
        );
        assert_eq!(
            k.refill,
            Some(Refill {
                low: 0.10,
                high: 0.90
            })
        );
        assert_eq!(k.hash, HashImpl::Sha2);
        assert!(k.ignored.is_empty(), "{:?}", k.ignored);
        assert!(k.shapes());
        for (v, want) in [
            ("268435456", 268_435_456u64),
            ("512M", 512 << 20),
            ("512MiB", 512 << 20),
            ("1g", 1 << 30),
            ("64K", 64 << 10),
        ] {
            assert_eq!(parse_size(v), Some(want), "{v}");
        }
    }

    #[test]
    fn a_value_that_does_not_parse_is_named_and_not_used() {
        let k = parse(&[
            (QUEUE_BYTES_ENV, "lots"),
            (PACE_MBPS_ENV, "-5"),
            (PACE_ADAPTIVE_ENV, "150,90,10"),
            (REFILL_ENV, "0.9,0.1"),
            (HASH_ENV, "md5"),
            (CROSSFILE_ENV, "1"),
        ]);
        assert_eq!(
            k,
            Knobs {
                ignored: k.ignored.clone(),
                ..Knobs::default()
            }
        );
        assert_eq!(k.ignored.len(), 6, "{:?}", k.ignored);
        assert!(k.ignored[5].contains("not built"), "{:?}", k.ignored);
        assert!(!k.shapes());
        assert_eq!(
            parse(&[]),
            Knobs::default(),
            "nothing set, nothing in force"
        );
    }

    /// A research build says what it runs with, even with nothing set; any
    /// other build says only that a set variable is ignored.
    #[test]
    fn the_announcement_is_a_banner_in_research_and_a_warning_elsewhere() {
        let k = parse(&[(PACE_MBPS_ENV, "110")]);
        let banner = announcement(true, &k, &[PACE_MBPS_ENV]);
        assert!(banner[0].starts_with("RESEARCH BUILD"), "{banner:?}");
        assert!(banner.iter().any(|l| l.contains("paced at 110 MB/s")));
        let quiet = announcement(true, &Knobs::default(), &[]);
        assert!(quiet
            .iter()
            .any(|l| l.contains("no TAPECTL_EXP_* knob is set")));
        let warned = announcement(false, &Knobs::default(), &[PACE_MBPS_ENV, HASH_ENV]);
        assert_eq!(warned.len(), 2, "one line per set variable");
        assert!(warned[0]
            .starts_with("warning: TAPECTL_EXP_PACE_MBPS is set, but this build ignores it"));
        assert!(announcement(false, &Knobs::default(), &[]).is_empty());
    }

    /// Outside a `research` build nothing is in force, whatever is set.
    #[cfg(not(feature = "research"))]
    #[test]
    fn a_production_build_has_no_knobs() {
        const { assert!(!BUILT) };
        assert_eq!(knobs(), &Knobs::default());
    }
}
