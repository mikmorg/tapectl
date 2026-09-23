//! The log-page sweep and its journal: every SCSI log page a health
//! collection reads, verbatim, in `log_page_journal` (migration 023, issue
//! #298).
//!
//! Until #298, `health::collect` read a hardcoded three pages (0x02, 0x03,
//! 0x2E) and never asked page 0x00 what the drive supports. This module
//! replaces that read with ONE sweep per contact:
//!
//! 1. read page 0x00 once, `sg_logs --maxlen=65532 --raw` — ONE LOG SENSE
//!    ([`READ_MAXLEN`], issue #328), the response bytes exactly;
//! 2. derive the page list from those bytes (4-byte header, then one page
//!    code per byte — [`parse_supported_pages`]);
//! 3. read every listed page once, `--raw` again;
//! 4. decode each page OFFLINE from its stored bytes (`sg_logs --in=- --raw
//!    --pdt=1`: no device, no second LOG SENSE). `--pdt=1` (sequential
//!    access) is required, or sg_logs guesses a disk and decodes 0x17 as
//!    "Non-volatile cache page".
//!
//! The existing 0x02/0x03/0x2E counter parsers ([`crate::tape::health`]) run
//! on step 4's text, so the health counters come out of the SAME reads the
//! journal records.
//!
//! # Each page at most once per contact
//!
//! ADR-0013, "Two hazards": TapeAlert (0x2E) is described by SSC-3 as
//! cleared when read. A second read inside one contact could return zeros
//! and the first read's evidence would be gone — so the rule is structural:
//! [`sweep`] is the only reader, it reads each page code at most once (page
//! 0x00 lists itself and is not read again; a duplicated list entry is read
//! once), and every consumer reads the [`Sweep`] or the journal, never the
//! drive. The process-spawning is behind [`LogSource`] so tests drive the
//! sweep with the recorded fixtures and COUNT reads per page.
//!
//! # When page 0x00 cannot be read
//!
//! The failed page-0x00 read is journalled (`ok = 0`) and the sweep falls
//! back to the three pages health collection read before #298 — still once
//! each. Reasons: that is exactly the pre-#298 read set, so a failed list
//! never costs less than the old code captured; the once-per-contact rule
//! holds either way (the hazard is a SECOND read, not a blind one); a
//! transient first-command failure (a pending unit attention is the classic
//! one) would otherwise silently cost the counters `report health` reads;
//! and a page the drive does not support just becomes one more `ok = 0`
//! row. Whether a read was listed or blind is a query, not a column: the
//! contact's page-0x00 row says whether a list existed.
//!
//! # The identity header
//!
//! `sg_logs` without `--raw` printed the drive's INQUIRY identity
//! (`    IBM       ULT3580-TD8       2160`) as its first line, and
//! `health_logs.raw_log` carried it — the third identity route in
//! [`crate::tape::drive_identity`] (#295). `--raw` prints no header, so
//! [`inquiry_header`] takes one standard INQUIRY (not a log page: no
//! read-to-clear hazard) and renders it exactly as sg_logs did
//! (`"    %.8s  %.16s  %.4s"` over the response's fixed fields), and
//! [`Sweep::health`] puts it first in `raw_log`. The route stays independent
//! of sysfs, and [`crate::tape::drive_identity::parse_sg_logs_identity_header`]
//! reads it unchanged.

use std::collections::BTreeSet;
use std::io::Write;
use std::process::{Command, Output, Stdio};

use rusqlite::types::Value;
use rusqlite::{params, Connection};
use tracing::warn;

use crate::error::Result;
use crate::tape::health::HealthCounters;

/// The tool every log-page read and decode spawns.
pub const LOG_TOOL: &str = "sg_logs";

/// The LOG SENSE allocation length every page read pins, so ONE `sg_logs`
/// invocation is ONE LOG SENSE command at the device (issue #328).
///
/// Without `--maxlen`, sg_logs 1.81 (`man sg_logs`, `-m, --maxlen=LEN`)
/// "first fetches the 4 byte response then does a second access with the
/// length indicated" — two commands per page. For a read-to-clear TapeAlert
/// page (0x2E) the header-only first fetch could clear the flags before the
/// second fetch, the one whose bytes reach stdout, was sent. ADR-0013's
/// once-per-contact rule is about commands at the drive, not processes, so
/// the read must be a single fetch.
///
/// 65532 (0xfffc) is sg_logs's own `MX_ALLOC_LEN`: exactly what its second
/// fetch asked for when no `--maxlen` was given, so the single fetch
/// requests no less than the old two-fetch read did and stdout keeps the
/// same shape (`page length + 4` bytes). It is even and a multiple of four
/// (sg_logs itself rounds odd lengths up because "some HBAs don't like odd
/// transfer lengths"), and inside both option spellings' bounds (`--maxlen`
/// accepts 2..=65535, the old-style `-m` 0..=0xfffc). A page longer than
/// this is possible in principle — a LOG SENSE page length is 16 bits — and
/// sg_logs then truncates its output and still exits 0, which
/// [`capture_from_output`] catches from the page header.
pub const READ_MAXLEN: u16 = 0xfffc;

/// LOG SENSE page 0x00: the drive's list of supported pages.
pub const SUPPORTED_PAGES: u8 = 0x00;

/// The pages health counters are parsed from, and the fallback read set when
/// page 0x00 is unavailable — what `health::collect` read before #298.
pub const HEALTH_PAGES: [u8; 3] = [0x02, 0x03, 0x2e];

// ── The seam ─────────────────────────────────────────────────────────────

/// Everything the sweep asks of the outside world. [`SgLogs`] is the real
/// one; tests supply a fixture-backed source that counts reads per page.
pub trait LogSource {
    /// The sg node reads are taken through (provenance for the journal).
    fn device_sg(&self) -> &str;
    /// ONE LOG SENSE of `page`, raw — one command at the device, not merely
    /// one process (the real source pins [`READ_MAXLEN`]). Returns the argv
    /// spawned and what the process gave.
    fn read_page(&mut self, page: u8) -> (Vec<String>, std::io::Result<Output>);
    /// Decode `raw` offline — no device. `Err` carries why it could not.
    fn decode(&mut self, raw: &[u8]) -> std::result::Result<String, String>;
    /// The tool's version string, for the journal's provenance.
    fn tool_version(&mut self) -> Option<String>;
}

/// The real source: `sg_logs` on one sg node.
pub struct SgLogs {
    device_sg: String,
}

impl SgLogs {
    pub fn new(device_sg: &str) -> Self {
        SgLogs {
            device_sg: device_sg.to_string(),
        }
    }

    /// The argv of one raw page read: a single LOG SENSE ([`READ_MAXLEN`]).
    /// The fixtures under `tests/fixtures/sg_logs/` were captured before
    /// issue #328 with this argv minus `--maxlen` (two LOG SENSE per page);
    /// sg_logs writes `page length + 4` bytes to stdout either way, so their
    /// bytes are the shape this argv produces.
    pub fn read_argv(device_sg: &str, page: u8) -> Vec<String> {
        vec![
            LOG_TOOL.to_string(),
            format!("--page=0x{page:02x}"),
            format!("--maxlen={READ_MAXLEN}"),
            "--raw".to_string(),
            device_sg.to_string(),
        ]
    }

    /// The argv of one offline decode: bytes on stdin, sequential-access
    /// device type forced.
    pub fn decode_argv() -> Vec<String> {
        vec![
            LOG_TOOL.to_string(),
            "--in=-".to_string(),
            "--raw".to_string(),
            "--pdt=1".to_string(),
        ]
    }
}

impl LogSource for SgLogs {
    fn device_sg(&self) -> &str {
        &self.device_sg
    }

    fn read_page(&mut self, page: u8) -> (Vec<String>, std::io::Result<Output>) {
        let argv = Self::read_argv(&self.device_sg, page);
        let output = Command::new(&argv[0]).args(&argv[1..]).output();
        (argv, output)
    }

    fn decode(&mut self, raw: &[u8]) -> std::result::Result<String, String> {
        decode_offline(raw)
    }

    fn tool_version(&mut self) -> Option<String> {
        tool_version()
    }
}

/// `sg_logs --in=- --raw --pdt=1`, bytes on stdin. Touches no device.
pub fn decode_offline(raw: &[u8]) -> std::result::Result<String, String> {
    let argv = SgLogs::decode_argv();
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{LOG_TOOL} decode spawn failed: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A page is at most 64 KiB and sg_logs reads all of `--in` before
        // decoding, so writing it whole and then closing cannot deadlock.
        stdin
            .write_all(raw)
            .map_err(|e| format!("{LOG_TOOL} decode stdin write failed: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("{LOG_TOOL} decode wait failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{LOG_TOOL} decode exit {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `sg_logs -V`, run at most ONCE per process and cached — the same shape
/// as `tape::mam`'s `sg_read_attr -V`. A failure is `None`, never an error.
fn tool_version() -> Option<String> {
    static VERSION: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    VERSION
        .get_or_init(|| {
            let out = Command::new(LOG_TOOL).arg("-V").output().ok()?;
            // sg3-utils prints the version on stderr; accept either stream.
            let text = if out.stdout.is_empty() {
                out.stderr
            } else {
                out.stdout
            };
            let text = String::from_utf8_lossy(&text).trim().to_string();
            (!text.is_empty()).then_some(text)
        })
        .clone()
}

/// The current UTC time in `datetime('now')`'s spelling.
fn now_sqlite() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

// ── One page read ────────────────────────────────────────────────────────

/// One log page read, before it becomes a journal row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageCapture {
    pub page_code: u8,
    /// Always 0: subpages are not enumerated (see migration 023).
    pub subpage_code: u8,
    /// When the read happened (UTC, `datetime('now')`'s spelling).
    pub captured_at: String,
    pub device_sg: String,
    /// The command actually spawned, program first.
    pub tool_argv: Vec<String>,
    pub tool_version: Option<String>,
    /// stdout byte for byte. `None` when the tool never ran; `Some` —
    /// possibly empty — when it ran, whatever its exit status.
    pub raw: Option<Vec<u8>>,
    /// `None` exactly when the read succeeded.
    pub error: Option<String>,
    /// The offline decode of `raw`, when the read succeeded and the decode
    /// ran. What THIS build's decoder said — never a substitute for `raw`.
    pub decoded: Option<String>,
}

impl PageCapture {
    /// The read succeeded: the tool ran, exited 0, and no error was recorded.
    pub fn ok(&self) -> bool {
        self.error.is_none() && self.raw.is_some()
    }
}

/// Build a capture from what the process gave — the pure half of a read.
pub fn capture_from_output(
    page_code: u8,
    device_sg: &str,
    captured_at: String,
    tool_argv: Vec<String>,
    tool_version: Option<String>,
    output: std::io::Result<Output>,
) -> PageCapture {
    let mut capture = PageCapture {
        page_code,
        subpage_code: 0,
        captured_at,
        device_sg: device_sg.to_string(),
        tool_argv,
        tool_version,
        raw: None,
        error: None,
        decoded: None,
    };
    match output {
        Err(e) => capture.error = Some(format!("{LOG_TOOL} spawn failed: {e}")),
        Ok(out) => {
            let short = truncated_page(&out.stdout);
            capture.raw = Some(out.stdout);
            if out.status.success() {
                // sg_logs truncates a page longer than `--maxlen` and still
                // exits 0 (its "Only fetched" note goes to stderr). The page
                // header says how long the page is, so check it: a partial
                // page is not a reading. The bytes are kept; `ok = 0` keeps
                // them out of the decode and the health counters.
                if let Some((declared, received)) = short {
                    capture.error = Some(format!(
                        "{LOG_TOOL} page 0x{page_code:02x} truncated: the page header declares \
                         {declared} bytes, {received} received (--maxlen={READ_MAXLEN})"
                    ));
                }
            } else {
                let stderr = String::from_utf8_lossy(&out.stderr);
                capture.error = Some(if stderr.trim().is_empty() {
                    format!("{LOG_TOOL} page 0x{page_code:02x} exit {}", out.status)
                } else {
                    format!(
                        "{LOG_TOOL} page 0x{page_code:02x} exit {}: {}",
                        out.status,
                        stderr.trim()
                    )
                });
            }
        }
    }
    capture
}

/// `Some((declared, received))` when a response's header declares more bytes
/// (page length + 4) than arrived — a truncated page. `None` for a complete
/// page, and for fewer than 4 bytes, which carry no length to check.
pub fn truncated_page(raw: &[u8]) -> Option<(usize, usize)> {
    if raw.len() < 4 {
        return None;
    }
    let declared = u16::from_be_bytes([raw[2], raw[3]]) as usize + 4;
    (declared > raw.len()).then_some((declared, raw.len()))
}

// ── Page 0x00 ────────────────────────────────────────────────────────────

/// The page codes a page-0x00 response lists, in the drive's order.
///
/// SPC: byte 0 is DS|SPF|page code, byte 1 the subpage, bytes 2-3 the page
/// length (big-endian); then one page code per byte. With SPF set the list
/// is (page, subpage) pairs instead — only a subpage-0xFF request returns
/// that, which the sweep never makes, but a drive that answers so anyway is
/// read as pairs and its subpage-0 entries kept rather than misread as
/// alternating page codes. A length running past the bytes received is
/// clamped to what arrived. `Err` for bytes that are not a page-0x00
/// response at all.
pub fn parse_supported_pages(raw: &[u8]) -> std::result::Result<Vec<u8>, String> {
    if raw.len() < 4 {
        return Err(format!(
            "page 0x00 response is {} bytes, shorter than its 4-byte header",
            raw.len()
        ));
    }
    let page = raw[0] & 0x3f;
    if page != SUPPORTED_PAGES {
        return Err(format!(
            "page 0x00 response names page 0x{page:02x}, not 0x00"
        ));
    }
    let spf = raw[0] & 0x40 != 0;
    let declared = u16::from_be_bytes([raw[2], raw[3]]) as usize;
    let body = &raw[4..(4 + declared).min(raw.len())];
    let pages = if spf {
        body.chunks_exact(2)
            .filter(|pair| pair[1] == 0)
            .map(|pair| pair[0] & 0x3f)
            .collect()
    } else {
        body.iter().map(|b| b & 0x3f).collect()
    };
    Ok(pages)
}

/// The health pages a listed set does NOT include — each is a counter the
/// health row will not have, and the reason is worth a warning by name.
pub fn missing_health_pages(listed: &[u8]) -> Vec<u8> {
    HEALTH_PAGES
        .iter()
        .copied()
        .filter(|p| !listed.contains(p))
        .collect()
}

// ── The sweep ────────────────────────────────────────────────────────────

/// One contact's log pages: page 0x00 and every page read after it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sweep {
    /// Every read, in the order taken; page 0x00 first.
    pub captures: Vec<PageCapture>,
    /// The page list page 0x00 gave; `None` when it could not be read or
    /// parsed and the sweep fell back to [`HEALTH_PAGES`].
    pub listed: Option<Vec<u8>>,
}

/// Read page 0x00, then every page it lists, **once each**, and decode each
/// successful read offline. Never fails: a failed read is a capture that
/// says so, and the sweep carries on.
pub fn sweep(source: &mut dyn LogSource) -> Sweep {
    let tool_version = source.tool_version();
    let mut read: BTreeSet<u8> = BTreeSet::new();
    let mut out = Sweep::default();

    let first = read_one(source, SUPPORTED_PAGES, &tool_version);
    read.insert(SUPPORTED_PAGES);
    let list = if first.ok() {
        parse_supported_pages(first.raw.as_deref().unwrap_or_default())
    } else {
        Err(first.error.clone().unwrap_or_default())
    };
    out.captures.push(first);

    let plan: Vec<u8> = match list {
        Ok(listed) => {
            let missing = missing_health_pages(&listed);
            if !missing.is_empty() {
                let names: Vec<String> = missing.iter().map(|p| format!("0x{p:02x}")).collect();
                warn!(
                    sg_device = source.device_sg(),
                    missing = %names.join(" "),
                    "the drive's page 0x00 does not list these health pages; their counters \
                     will be absent from this reading"
                );
            }
            out.listed = Some(listed.clone());
            listed
        }
        Err(reason) => {
            warn!(
                sg_device = source.device_sg(),
                reason = %reason,
                "log page 0x00 unavailable; reading the three health pages without a list"
            );
            HEALTH_PAGES.to_vec()
        }
    };

    for page in plan {
        // The structural once-per-contact rule: 0x00 lists itself, and a
        // list may repeat a code. Neither is ever read a second time.
        if !read.insert(page) {
            continue;
        }
        let capture = read_one(source, page, &tool_version);
        if let Some(err) = &capture.error {
            warn!(page = format!("0x{page:02x}"), err = %err, "log page read failed");
        }
        out.captures.push(capture);
    }
    out
}

fn read_one(source: &mut dyn LogSource, page: u8, tool_version: &Option<String>) -> PageCapture {
    let captured_at = now_sqlite();
    let (argv, output) = source.read_page(page);
    let mut capture = capture_from_output(
        page,
        source.device_sg(),
        captured_at,
        argv,
        tool_version.clone(),
        output,
    );
    if capture.ok() {
        match source.decode(capture.raw.as_deref().unwrap_or_default()) {
            Ok(text) => capture.decoded = Some(text),
            Err(e) => warn!(
                page = format!("0x{page:02x}"),
                err = %e,
                "offline decode failed; the raw bytes are journalled regardless"
            ),
        }
    }
    capture
}

impl Sweep {
    /// The capture of `page`, if the sweep read it.
    pub fn page(&self, page: u8) -> Option<&PageCapture> {
        self.captures.iter().find(|c| c.page_code == page)
    }

    /// The health reading this sweep yields: counters parsed from the
    /// decoded [`HEALTH_PAGES`], and the `health_logs.raw_log` text —
    /// `header` (the INQUIRY identity line, [`inquiry_header`]) first, then
    /// each health page's decode under its `=== page 0xNN ===` marker, the
    /// shape `HealthCounters::from_raw_log` and `report health` already read.
    ///
    /// `None` when no health page was read and decoded: there is no reading
    /// to record, and a row of zeros would claim the drive reported none.
    pub fn health(&self, header: Option<&str>) -> Option<(HealthCounters, String)> {
        let pages: Vec<(u8, &str)> = HEALTH_PAGES
            .iter()
            .filter_map(|p| {
                self.page(*p)
                    .and_then(|c| c.decoded.as_deref())
                    .map(|t| (*p, t))
            })
            .collect();
        if pages.is_empty() {
            return None;
        }
        let counters = HealthCounters::from_decoded_pages(pages.iter().copied());
        let mut raw_log = String::new();
        if let Some(h) = header {
            raw_log.push_str(h.trim_end_matches('\n'));
            raw_log.push('\n');
        }
        for (page, text) in &pages {
            raw_log.push_str(&format!("=== page 0x{page:02x} ===\n"));
            raw_log.push_str(text);
            raw_log.push('\n');
        }
        Some((counters, raw_log))
    }
}

// ── The identity header ──────────────────────────────────────────────────

/// One standard INQUIRY, 36 bytes, no VPD fetch — an INQUIRY is not a log
/// page, so it carries no read-to-clear hazard. `None` when it could not be
/// taken; the header is an addition to the record, never a precondition.
pub fn inquiry_header(device_sg: &str) -> Option<String> {
    let out = Command::new("sg_inq")
        .args(["--len=36", "--only", "--raw", device_sg])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    render_inquiry_header(&out.stdout)
}

/// Render a standard INQUIRY response as sg_logs printed its identity line:
/// `"    %.8s  %.16s  %.4s"` over the vendor (bytes 8-15), product (16-31)
/// and revision (32-35) fields, padding kept. `None` for a response too
/// short to hold them, or one with nothing printable in them.
pub fn render_inquiry_header(inquiry: &[u8]) -> Option<String> {
    if inquiry.len() < 36 {
        return None;
    }
    let field = |r: std::ops::Range<usize>| -> String {
        inquiry[r]
            .iter()
            .map(|&b| {
                if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else {
                    ' '
                }
            })
            .collect()
    };
    let (vendor, product, rev) = (field(8..16), field(16..32), field(32..36));
    if vendor.trim().is_empty() && product.trim().is_empty() && rev.trim().is_empty() {
        return None;
    }
    Some(format!("    {vendor}  {product}  {rev}"))
}

// ── The journal ──────────────────────────────────────────────────────────

/// One `log_page_journal` row, before it is written. Every column but `id`.
#[derive(Debug, Clone, PartialEq)]
pub struct JournalRow {
    pub captured_at: String,
    pub contact_id: Option<i64>,
    pub device_sg: String,
    pub device_tape: Option<String>,
    pub trigger: String,
    pub page_code: u8,
    pub subpage_code: u8,
    pub ok: bool,
    pub error: Option<String>,
    /// JSON array text, program first.
    pub tool_argv: String,
    pub tool_version: Option<String>,
    /// `Blob` of the exact bytes, `Null` when the tool never ran.
    pub raw: Value,
    pub decoded: Option<String>,
    pub tapectl_version: &'static str,
}

impl JournalRow {
    /// The row a capture becomes. `tapectl_version` is not a parameter —
    /// there is exactly one build writing (ADR-0013 §7).
    pub fn from_capture(
        contact_id: Option<i64>,
        trigger: &str,
        device_tape: Option<&str>,
        capture: &PageCapture,
    ) -> JournalRow {
        JournalRow {
            captured_at: capture.captured_at.clone(),
            contact_id,
            device_sg: capture.device_sg.clone(),
            device_tape: device_tape.map(str::to_string),
            trigger: trigger.to_string(),
            page_code: capture.page_code,
            subpage_code: capture.subpage_code,
            ok: capture.ok(),
            error: capture.error.clone(),
            tool_argv: serde_json::to_string(&capture.tool_argv)
                .unwrap_or_else(|_| "[]".to_string()),
            tool_version: capture.tool_version.clone(),
            raw: capture
                .raw
                .as_ref()
                .map_or(Value::Null, |b| Value::Blob(b.clone())),
            decoded: capture.decoded.clone(),
            tapectl_version: env!("CARGO_PKG_VERSION"),
        }
    }
}

/// Write one row. Fallible — [`record_sweep`] is the best-effort wrapper
/// production uses.
pub fn insert(conn: &Connection, row: &JournalRow) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO log_page_journal
             (captured_at, contact_id, device_sg, device_tape, trigger, page_code,
              subpage_code, ok, error, tool_argv, tool_version, raw, decoded,
              tapectl_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            row.captured_at,
            row.contact_id,
            row.device_sg,
            row.device_tape,
            row.trigger,
            row.page_code,
            row.subpage_code,
            row.ok,
            row.error,
            row.tool_argv,
            row.tool_version,
            row.raw,
            row.decoded,
            row.tapectl_version,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Journal every capture of a sweep, in the order read, best-effort: a
/// refused INSERT warns and is skipped. Bookkeeping must never become a new
/// way for a tape command to fail. Returns the ids written.
pub fn record_sweep(
    conn: &Connection,
    contact_id: Option<i64>,
    trigger: &str,
    device_tape: Option<&str>,
    sweep: &Sweep,
) -> Vec<i64> {
    let mut ids = Vec::with_capacity(sweep.captures.len());
    for capture in &sweep.captures {
        let row = JournalRow::from_capture(contact_id, trigger, device_tape, capture);
        match insert(conn, &row) {
            Ok(id) => ids.push(id),
            Err(e) => warn!(
                err = %e,
                page = format!("0x{:02x}", capture.page_code),
                trigger,
                "log_page_journal insert failed"
            ),
        }
    }
    ids
}

// ── Reading it back: parse it later ──────────────────────────────────────

/// One journalled read of a page, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalledPage {
    pub id: i64,
    pub captured_at: String,
    pub contact_id: Option<i64>,
    pub ok: bool,
    pub raw: Option<Vec<u8>>,
}

/// Every journalled read of `page_code`, oldest first — the route a parser
/// written after the fact takes over rows it never saw captured.
pub fn journalled(conn: &Connection, page_code: u8) -> Result<Vec<JournalledPage>> {
    let mut stmt = conn.prepare(
        "SELECT id, captured_at, contact_id, ok, raw FROM log_page_journal
          WHERE page_code = ?1 ORDER BY captured_at, id",
    )?;
    let rows = stmt
        .query_map([page_code], |r| {
            Ok(JournalledPage {
                id: r.get(0)?,
                captured_at: r.get(1)?,
                contact_id: r.get(2)?,
                ok: r.get(3)?,
                raw: r.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Log page 0x0D (Temperature), parameter 0x0000: the current temperature
/// in degrees Celsius, parsed from the RAW response bytes (so it needs no
/// decoder and reads rows captured before it was written). `None` for bytes
/// that are not page 0x0D, a missing parameter, or 0xFF ("not available").
///
/// Nothing parsed this page before #298 — it is here to prove historical
/// journal rows are re-parseable ([`temperature_history`]).
pub fn parse_temperature(raw: &[u8]) -> Option<u8> {
    if raw.len() < 4 || raw[0] & 0x3f != 0x0d {
        return None;
    }
    let end = (4 + u16::from_be_bytes([raw[2], raw[3]]) as usize).min(raw.len());
    let mut at = 4;
    while at + 4 <= end {
        let code = u16::from_be_bytes([raw[at], raw[at + 1]]);
        let len = raw[at + 3] as usize;
        let value = raw.get(at + 4..at + 4 + len)?;
        if code == 0x0000 {
            // Byte 0 reserved, byte 1 the temperature.
            return value.get(1).copied().filter(|t| *t != 0xff);
        }
        at += 4 + len;
    }
    None
}

/// One journalled temperature reading: `(journal id, contact_id, degrees C)`.
pub type TemperatureReading = (i64, Option<i64>, Option<u8>);

/// Every journalled temperature reading, oldest first. A failed read, or one
/// that carried no temperature, is `None` rather than skipped — it was still
/// a reading.
pub fn temperature_history(conn: &Connection) -> Result<Vec<TemperatureReading>> {
    Ok(journalled(conn, 0x0d)?
        .into_iter()
        .map(|p| {
            let t = if p.ok {
                p.raw.as_deref().and_then(parse_temperature)
            } else {
                None
            };
            (p.id, p.contact_id, t)
        })
        .collect())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    // ── The fixtures: one mhvtl drive, every page it lists ──

    macro_rules! page_fixture {
        ($hex:literal) => {
            (
                include_bytes!(concat!(
                    "../../tests/fixtures/sg_logs/mhvtl_td8_sg1/page_0x",
                    $hex,
                    ".bin"
                ))
                .as_slice(),
                include_str!(concat!(
                    "../../tests/fixtures/sg_logs/mhvtl_td8_sg1/page_0x",
                    $hex,
                    ".decoded.txt"
                )),
            )
        };
    }

    /// `(page, raw bytes, offline decode)` for every page the fixture's page
    /// 0x00 lists, in its order.
    pub(crate) fn fixture_pages() -> Vec<(u8, &'static [u8], &'static str)> {
        let all = [
            (0x00, page_fixture!("00")),
            (0x02, page_fixture!("02")),
            (0x03, page_fixture!("03")),
            (0x0c, page_fixture!("0c")),
            (0x0d, page_fixture!("0d")),
            (0x10, page_fixture!("10")),
            (0x11, page_fixture!("11")),
            (0x17, page_fixture!("17")),
            (0x2e, page_fixture!("2e")),
            (0x30, page_fixture!("30")),
            (0x31, page_fixture!("31")),
            (0x32, page_fixture!("32")),
            (0x37, page_fixture!("37")),
        ];
        all.into_iter().map(|(p, (b, t))| (p, b, t)).collect()
    }

    /// The live `sg_logs --page=0` output — its first line is the INQUIRY
    /// identity header sg_logs prints without `--raw`.
    const PAGE_00_LIVE: &str =
        include_str!("../../tests/fixtures/sg_logs/mhvtl_td8_sg1/page_0x00.live.txt");

    /// The page set the fixture's page 0x00 lists, spelled out BY NAME from
    /// the README rather than derived from the bytes under test.
    pub(crate) const LISTED: [u8; 13] = [
        0x00, 0x02, 0x03, 0x0c, 0x0d, 0x10, 0x11, 0x17, 0x2e, 0x30, 0x31, 0x32, 0x37,
    ];

    fn exit(code: i32, stdout: &[u8], stderr: &[u8]) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    /// A fixture-backed source that counts every read, per page.
    #[derive(Default)]
    pub(crate) struct FixtureSource {
        pub reads: BTreeMap<u8, usize>,
        pub order: Vec<u8>,
        pub decodes: usize,
        /// Pages whose read fails the way sg_logs fails (exit 5, stderr).
        pub fail: BTreeSet<u8>,
        /// Replacement bytes for a page (e.g. a synthetic page 0x00).
        pub bytes: BTreeMap<u8, Vec<u8>>,
        /// Replacement decode for a page.
        pub text: BTreeMap<u8, String>,
    }

    impl LogSource for FixtureSource {
        fn device_sg(&self) -> &str {
            "/dev/sg-fixture"
        }

        fn read_page(&mut self, page: u8) -> (Vec<String>, std::io::Result<Output>) {
            *self.reads.entry(page).or_default() += 1;
            self.order.push(page);
            let argv = SgLogs::read_argv(self.device_sg(), page);
            if self.fail.contains(&page) {
                return (
                    argv,
                    Ok(exit(
                        5,
                        b"",
                        b"LOG SENSE: Illegal request, invalid field in cdb",
                    )),
                );
            }
            let bytes = self.bytes.get(&page).cloned().or_else(|| {
                fixture_pages()
                    .into_iter()
                    .find(|(p, _, _)| *p == page)
                    .map(|(_, b, _)| b.to_vec())
            });
            match bytes {
                Some(b) => (argv, Ok(exit(0, &b, b""))),
                None => (
                    argv,
                    Ok(exit(
                        5,
                        b"",
                        b"LOG SENSE: Illegal request, invalid field in cdb",
                    )),
                ),
            }
        }

        fn decode(&mut self, raw: &[u8]) -> std::result::Result<String, String> {
            self.decodes += 1;
            let page = raw.first().ok_or("empty")? & 0x3f;
            if let Some(t) = self.text.get(&page) {
                return Ok(t.clone());
            }
            fixture_pages()
                .into_iter()
                .find(|(p, _, _)| *p == page)
                .map(|(_, _, t)| t.to_string())
                .ok_or_else(|| format!("no fixture decode for 0x{page:02x}"))
        }

        fn tool_version(&mut self) -> Option<String> {
            Some("Version string: 1.81 20200110".to_string())
        }
    }

    /// Assert every page was read exactly once. The helper the no-double-read
    /// tests share, so the positive control below exercises the SAME check.
    pub(crate) fn assert_each_page_read_once(reads: &BTreeMap<u8, usize>) {
        assert!(!reads.is_empty(), "positive control: the counter saw reads");
        let doubled: Vec<(u8, usize)> = reads
            .iter()
            .filter(|(_, n)| **n != 1)
            .map(|(p, n)| (*p, *n))
            .collect();
        assert!(
            doubled.is_empty(),
            "a log page was read more than once in one contact (ADR-0013's \
             read-to-clear hazard): {doubled:?}"
        );
    }

    /// Issue #320: the catalog holds exactly ONE health reading and ONE
    /// sweep's journal rows, and both belong to contact `cid` — the
    /// once-per-contact rule, asserted from the rows a read path left.
    ///
    /// `pages` is how many pages the fixture's 0x00 lists (0x00 included):
    /// one sweep journals exactly that many rows, and a second sweep would
    /// double it and repeat page 0x00. `volume_id` is the volume the reading
    /// must name — the same one its contact names. The reading's kind is
    /// `restore`, the one read-path kind (ADR-0013's 2026-09-23 amendment).
    pub(crate) fn assert_one_reading_for(
        conn: &Connection,
        cid: i64,
        trigger: &str,
        volume_id: Option<i64>,
        pages: usize,
    ) {
        let health: Vec<(Option<i64>, String, Option<i64>)> = conn
            .prepare("SELECT contact_id, operation, volume_id FROM health_logs ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            health,
            vec![(Some(cid), "restore".to_string(), volume_id)],
            "exactly one health_logs row, of kind 'restore', naming contact {cid}"
        );
        let journal: Vec<(Option<i64>, String, i64)> = conn
            .prepare("SELECT contact_id, trigger, page_code FROM log_page_journal ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            journal.len(),
            pages,
            "one sweep's journal rows, no more: {journal:?}"
        );
        assert_eq!(
            journal.iter().filter(|(_, _, p)| *p == 0).count(),
            1,
            "page 0x00 read once — one sweep"
        );
        let foreign: Vec<_> = journal
            .iter()
            .filter(|(c, t, _)| *c != Some(cid) || t != trigger)
            .collect();
        assert!(
            foreign.is_empty(),
            "every journal row names contact {cid} under trigger {trigger:?}: {foreign:?}"
        );
    }

    fn open_contact(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO cartridge_contacts (operation, device) VALUES ('volume write', '/dev/null')",
            [],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    // ── migration 023, to the #227/#264 standard ──

    #[test]
    fn migration_023_creates_the_journal_with_its_columns_and_indexes() {
        let conn = crate::db::open_memory().unwrap();
        let cols: Vec<(String, String, i64, Option<String>)> = {
            let mut stmt = conn.prepare("PRAGMA table_info(log_page_journal)").unwrap();
            let v = stmt
                .query_map([], |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                .unwrap()
                .map(|c| c.unwrap())
                .collect();
            v
        };
        let expect = |name: &str, ty: &str, notnull: i64, dflt: Option<&str>| {
            (
                name.to_string(),
                ty.to_string(),
                notnull,
                dflt.map(str::to_string),
            )
        };
        assert_eq!(
            cols,
            vec![
                expect("id", "INTEGER", 0, None),
                expect("captured_at", "TEXT", 1, Some("datetime('now')")),
                expect("contact_id", "INTEGER", 0, None),
                expect("device_sg", "TEXT", 1, None),
                expect("device_tape", "TEXT", 0, None),
                expect("trigger", "TEXT", 1, None),
                expect("page_code", "INTEGER", 1, None),
                expect("subpage_code", "INTEGER", 1, Some("0")),
                expect("ok", "INTEGER", 1, None),
                expect("error", "TEXT", 0, None),
                expect("tool_argv", "TEXT", 1, None),
                expect("tool_version", "TEXT", 0, None),
                expect("raw", "BLOB", 0, None),
                expect("decoded", "TEXT", 0, None),
                expect("tapectl_version", "TEXT", 1, None),
            ],
            "no drive column (ADR-0013 §1): the drive is reached through contact_id"
        );

        let indexes: Vec<(String, Vec<String>)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'index' \
                     AND tbl_name = 'log_page_journal' AND name NOT LIKE 'sqlite_%' ORDER BY name",
                )
                .unwrap();
            let names: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|n| n.unwrap())
                .collect();
            names
                .into_iter()
                .map(|n| {
                    let mut s = conn
                        .prepare(&format!("PRAGMA index_info(\"{n}\")"))
                        .unwrap();
                    let cols = s
                        .query_map([], |r| r.get::<_, String>(2))
                        .unwrap()
                        .map(|c| c.unwrap())
                        .collect();
                    (n, cols)
                })
                .collect()
        };
        assert_eq!(
            indexes,
            vec![
                (
                    "idx_log_page_journal_contact".to_string(),
                    vec!["contact_id".to_string()]
                ),
                (
                    "idx_log_page_journal_page".to_string(),
                    vec!["page_code".to_string(), "captured_at".to_string()]
                ),
            ]
        );

        let report = crate::cli::operations::db_fsck(&conn, false, false).unwrap();
        assert!(report.integrity_ok, "integrity_check after 023");
        assert!(report.issues.is_empty(), "{:?}", report.issues);
    }

    /// `PRAGMA table_info` reports neither foreign keys nor CHECKs (#227),
    /// so the FK is proved enforced by behaviour, with positive controls.
    #[test]
    fn the_contact_foreign_key_is_enforced_not_merely_declared() {
        let conn = crate::db::open_memory().unwrap();
        let fks: Vec<(String, String)> = {
            let mut stmt = conn
                .prepare("PRAGMA foreign_key_list(log_page_journal)")
                .unwrap();
            let v = stmt
                .query_map([], |r| Ok((r.get::<_, String>(2)?, r.get::<_, String>(3)?)))
                .unwrap()
                .map(|x| x.unwrap())
                .collect();
            v
        };
        assert_eq!(
            fks,
            vec![("cartridge_contacts".to_string(), "contact_id".to_string())],
            "one foreign key, to the contact — and no drive FK of its own"
        );

        let capture = sweep(&mut FixtureSource::default()).captures.remove(0);
        let bogus = JournalRow::from_capture(Some(99_999), "volume write", None, &capture);
        assert!(
            insert(&conn, &bogus).is_err(),
            "a contact_id naming no contact must be refused under foreign_keys=ON"
        );
        let contact = open_contact(&conn);
        let good = JournalRow::from_capture(Some(contact), "volume write", None, &capture);
        insert(&conn, &good).expect("a real contact_id is accepted");
        let none = JournalRow::from_capture(None, "volume write", None, &capture);
        insert(&conn, &none).expect("a NULL contact_id is the documented no-contact case");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM log_page_journal", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    /// The CHECKs are live, proved by behaviour; `trigger` has none
    /// (ADR-0013 §4) — an unknown command is accepted, beside the controls
    /// that the table does refuse what its CHECKs forbid.
    #[test]
    fn the_checks_are_live_and_trigger_is_free_text() {
        let conn = crate::db::open_memory().unwrap();
        let ins = |trigger: &str, page: i64, sub: i64, ok: i64| {
            conn.execute(
                "INSERT INTO log_page_journal
                     (device_sg, trigger, page_code, subpage_code, ok, tool_argv, tapectl_version)
                 VALUES ('/dev/sg0', ?1, ?2, ?3, ?4, '[]', 't')",
                params![trigger, page, sub, ok],
            )
        };
        ins("a command that does not exist yet", 0x2e, 0, 1)
            .expect("an unknown trigger must be accepted — no CHECK (ADR-0013 §4)");
        assert!(ins("volume write", 0x2e, 0, 7).is_err(), "CHECK on ok");
        assert!(
            ins("volume write", 256, 0, 1).is_err(),
            "CHECK on page_code"
        );
        assert!(ins("volume write", -1, 0, 1).is_err(), "CHECK on page_code");
        assert!(
            ins("volume write", 0x2e, 256, 1).is_err(),
            "CHECK on subpage_code"
        );
        ins("volume write", 0xff, 0xff, 0).expect("the boundary values are legal");
    }

    /// `db export` enumerates `sqlite_master`, so the new table is in it; the
    /// BLOB `raw` is rendered as hex (the exporter's documented BLOB form),
    /// never lossily as text.
    #[test]
    fn db_export_includes_the_journal_with_raw_as_hex() {
        let conn = crate::db::open_memory().unwrap();
        record_sweep(
            &conn,
            None,
            "volume write",
            None,
            &sweep(&mut FixtureSource::default()),
        );
        let mut buf = Vec::new();
        crate::db::export::export_json(&conn, &mut buf).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        let rows = v["tables"]["log_page_journal"]
            .as_array()
            .expect("log_page_journal is exported");
        assert_eq!(rows.len(), LISTED.len());
        let p0 = rows
            .iter()
            .find(|r| r["page_code"] == 0)
            .expect("the page-0x00 row");
        assert_eq!(p0["raw"], "0000000d0002030c0d1011172e30313237");
    }

    // ── page 0x00 ──

    #[test]
    fn page_00_parses_to_exactly_the_listed_pages() {
        let (_, raw, _) = fixture_pages()[0];
        assert_eq!(parse_supported_pages(raw).unwrap(), LISTED.to_vec());
    }

    #[test]
    fn page_00_parse_clamps_refuses_and_reads_spf_pairs() {
        // Length says 5 but only 2 codes arrived: clamp to what arrived.
        assert_eq!(
            parse_supported_pages(&[0x00, 0x00, 0x00, 0x05, 0x02, 0x03]).unwrap(),
            vec![0x02, 0x03]
        );
        // Not page 0x00 at all.
        assert!(parse_supported_pages(&[0x02, 0x00, 0x00, 0x00]).is_err());
        assert!(parse_supported_pages(&[0x00, 0x00]).is_err());
        // SPF set: (page, subpage) pairs; only subpage-0 entries are pages.
        assert_eq!(
            parse_supported_pages(&[0x40, 0xff, 0x00, 0x06, 0x02, 0x00, 0x0d, 0x01, 0x2e, 0x00])
                .unwrap(),
            vec![0x02, 0x2e]
        );
    }

    #[test]
    fn a_list_missing_a_health_page_is_named() {
        assert_eq!(missing_health_pages(&LISTED), Vec::<u8>::new());
        assert_eq!(missing_health_pages(&[0x00, 0x02, 0x03]), vec![0x2e]);
    }

    // ── the sweep ──

    /// The page set read is EXACTLY what page 0x00 lists — by name, in its
    /// order — and each is read once, 0x00 included (it lists itself).
    #[test]
    fn the_sweep_reads_exactly_the_listed_pages_each_once() {
        let mut src = FixtureSource::default();
        let s = sweep(&mut src);
        // The once-per-contact rule first, so a doubled read is reported
        // as exactly that rather than as an ordering difference.
        assert_each_page_read_once(&src.reads);
        assert_eq!(src.order, LISTED.to_vec(), "the pages read, in order");
        assert_eq!(s.listed.as_deref(), Some(&LISTED[..]));
        let captured: Vec<u8> = s.captures.iter().map(|c| c.page_code).collect();
        assert_eq!(captured, LISTED.to_vec());
        assert!(s.captures.iter().all(PageCapture::ok));
        assert_eq!(src.decodes, LISTED.len(), "every page decoded, offline");
    }

    /// A page 0x00 that repeats a code does not cause a second read.
    #[test]
    fn a_duplicated_list_entry_is_still_read_once() {
        let mut src = FixtureSource::default();
        src.bytes.insert(
            0x00,
            vec![0x00, 0x00, 0x00, 0x05, 0x00, 0x2e, 0x02, 0x2e, 0x00],
        );
        sweep(&mut src);
        assert_each_page_read_once(&src.reads);
        assert_eq!(src.order, vec![0x00, 0x2e, 0x02]);
    }

    /// The positive control for the no-double-read assertion: fed the count
    /// a doubled 0x2E read produces, the SAME helper fails. (Also shown once
    /// against `sweep` itself with a deliberately doubled read; see the
    /// issue #298 report.)
    #[test]
    #[should_panic(expected = "read more than once")]
    fn the_once_per_page_assertion_fails_on_a_doubled_read() {
        let mut src = FixtureSource::default();
        sweep(&mut src);
        // What a second, stray read of TapeAlert would leave behind.
        let _ = src.read_page(0x2e);
        assert_each_page_read_once(&src.reads);
    }

    /// A listed page that fails is an `ok = 0` capture with its error, the
    /// sweep carries on, and health still yields a reading. Positive
    /// control: the same page from a source that does not fail is `ok`.
    #[test]
    fn a_listed_page_that_fails_is_recorded_and_the_sweep_carries_on() {
        let good = sweep(&mut FixtureSource::default());
        assert!(good.page(0x17).unwrap().ok(), "positive control");

        let mut src = FixtureSource::default();
        src.fail.insert(0x17);
        let s = sweep(&mut src);
        let bad = s.page(0x17).unwrap();
        assert!(!bad.ok());
        assert!(bad.error.as_deref().unwrap().contains("Illegal request"));
        assert_eq!(bad.decoded, None, "a failed read is not decoded");
        assert_eq!(src.order, LISTED.to_vec(), "every other page still read");
        assert!(s.health(None).is_some(), "health still has its pages");
    }

    /// Page 0x00 fails: its row is kept, and the sweep reads the three
    /// health pages — and nothing else — once each.
    #[test]
    fn a_failed_page_00_falls_back_to_the_three_health_pages() {
        let mut src = FixtureSource::default();
        src.fail.insert(0x00);
        let s = sweep(&mut src);
        assert_eq!(src.order, vec![0x00, 0x02, 0x03, 0x2e]);
        assert_each_page_read_once(&src.reads);
        assert_eq!(s.listed, None);
        assert!(!s.page(0x00).unwrap().ok());
        assert!(s.health(None).is_some());
    }

    /// Bytes that are not a page list count as no list: the same fallback.
    #[test]
    fn an_unparseable_page_00_falls_back_too() {
        let mut src = FixtureSource::default();
        src.bytes.insert(0x00, vec![0x00, 0x00]);
        let s = sweep(&mut src);
        assert_eq!(src.order, vec![0x00, 0x02, 0x03, 0x2e]);
        assert!(s.page(0x00).unwrap().ok(), "the read itself succeeded");
        assert_eq!(s.listed, None);
    }

    /// No health page readable: no health reading at all, rather than a row
    /// of zeros claiming the drive reported none.
    #[test]
    fn no_health_page_means_no_health_reading() {
        let mut src = FixtureSource::default();
        src.fail.extend(HEALTH_PAGES);
        let s = sweep(&mut src);
        assert_eq!(s.health(None), None);
        assert_eq!(
            s.captures.len(),
            LISTED.len(),
            "the other pages are still captured"
        );
    }

    // ── issue #317: absent 0x2e is NULL, never "recorded, none" ──

    /// The `health_logs.tape_alerts` a sweep's health reading stores, read
    /// back through the production writer [`crate::tape::health::record`]
    /// as `Option` so NULL and 0 stay distinct.
    fn stored_tape_alerts(s: &Sweep) -> Option<i64> {
        let conn = crate::db::open_memory().unwrap();
        let (counters, raw_log) = s.health(None).expect("a health reading");
        crate::tape::health::record(
            &conn,
            None,
            None,
            None,
            crate::tape::health::Reading::Write,
            &counters,
            &raw_log,
        )
        .unwrap();
        conn.query_row("SELECT tape_alerts FROM health_logs", [], |r| r.get(0))
            .unwrap()
    }

    /// A drive whose page 0x00 lists 0x02 and 0x03 but NOT 0x2e: the
    /// reading is recorded (its counters exist) but its tape-alert count
    /// was never read, so the row says NULL — not 0, which would claim the
    /// drive reported no alerts.
    #[test]
    fn a_page_list_without_0x2e_stores_null_tape_alerts() {
        let mut src = FixtureSource::default();
        src.bytes
            .insert(0x00, vec![0x00, 0x00, 0x00, 0x03, 0x00, 0x02, 0x03]);
        let s = sweep(&mut src);
        assert_each_page_read_once(&src.reads);
        assert_eq!(src.order, vec![0x00, 0x02, 0x03], "0x2e is never read");
        assert!(s.page(0x2e).is_none());
        assert!(
            s.page(0x02).unwrap().ok() && s.page(0x03).unwrap().ok(),
            "positive control: the error-counter pages were read"
        );
        assert_eq!(stored_tape_alerts(&s), None);
    }

    /// 0x2e listed, but its read fails: the same NULL, and the failed page
    /// is not read a second time to try again.
    #[test]
    fn a_failed_0x2e_read_stores_null_tape_alerts() {
        let mut src = FixtureSource::default();
        src.fail.insert(0x2e);
        let s = sweep(&mut src);
        assert_each_page_read_once(&src.reads);
        assert_eq!(src.order, LISTED.to_vec(), "0x2e was listed and attempted");
        assert!(!s.page(0x2e).unwrap().ok());
        assert_eq!(stored_tape_alerts(&s), None);
    }

    /// Positive control: the mhvtl fixture reads 0x2e ok with every flag 0,
    /// and that IS a recording — 0, not NULL.
    #[test]
    fn a_clean_0x2e_read_stores_zero_tape_alerts() {
        let mut src = FixtureSource::default();
        let s = sweep(&mut src);
        let p2e = s.page(0x2e).expect("0x2e is listed in the mhvtl fixture");
        assert!(p2e.ok(), "positive control: 0x2e read ok");
        assert!(
            p2e.decoded.as_deref().unwrap().contains("Tape alert page"),
            "positive control: 0x2e decoded"
        );
        assert_eq!(stored_tape_alerts(&s), Some(0));
    }

    /// Positive control: two raised flags store 2.
    #[test]
    fn a_0x2e_read_with_two_flags_stores_two() {
        let mut src = FixtureSource::default();
        src.text.insert(
            0x2e,
            "Tape alert page (ssc-3) [0x2e]\n  Read warning: 1\n  Write warning: 0\n  \
             Hard error: 1\n"
                .to_string(),
        );
        let s = sweep(&mut src);
        assert_each_page_read_once(&src.reads);
        assert_eq!(stored_tape_alerts(&s), Some(2));
    }

    // ── the real HP LTO-6: every page it supports, no medium loaded ──

    macro_rules! hp_fixture {
        ($hex:literal) => {
            (
                include_bytes!(concat!(
                    "../../tests/fixtures/sg_logs/hp_lto6_sg0_nomedia/page_0x",
                    $hex,
                    ".bin"
                ))
                .as_slice(),
                include_str!(concat!(
                    "../../tests/fixtures/sg_logs/hp_lto6_sg0_nomedia/page_0x",
                    $hex,
                    ".decoded.txt"
                )),
            )
        };
    }

    /// The 22 pages the real HP LTO-6's page 0x00 lists, spelled out BY
    /// NAME from the fixture README rather than derived from the bytes.
    const HP_LISTED: [u8; 22] = [
        0x00, 0x02, 0x03, 0x0c, 0x0d, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x1b, 0x2e,
        0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x3e,
    ];

    /// `(page, raw bytes, offline decode)` for every HP LTO-6 page.
    fn hp_pages() -> Vec<(u8, &'static [u8], &'static str)> {
        let all = [
            (0x00, hp_fixture!("00")),
            (0x02, hp_fixture!("02")),
            (0x03, hp_fixture!("03")),
            (0x0c, hp_fixture!("0c")),
            (0x0d, hp_fixture!("0d")),
            (0x11, hp_fixture!("11")),
            (0x12, hp_fixture!("12")),
            (0x13, hp_fixture!("13")),
            (0x14, hp_fixture!("14")),
            (0x15, hp_fixture!("15")),
            (0x16, hp_fixture!("16")),
            (0x17, hp_fixture!("17")),
            (0x18, hp_fixture!("18")),
            (0x1b, hp_fixture!("1b")),
            (0x2e, hp_fixture!("2e")),
            (0x30, hp_fixture!("30")),
            (0x31, hp_fixture!("31")),
            (0x32, hp_fixture!("32")),
            (0x33, hp_fixture!("33")),
            (0x34, hp_fixture!("34")),
            (0x35, hp_fixture!("35")),
            (0x3e, hp_fixture!("3e")),
        ];
        all.into_iter().map(|(p, (b, t))| (p, b, t)).collect()
    }

    /// A [`FixtureSource`] answering every page from the HP LTO-6 set. Its
    /// overrides take precedence over the mhvtl defaults, and every page the
    /// HP list names is overridden, so no mhvtl byte can leak in.
    fn hp_source() -> FixtureSource {
        let mut src = FixtureSource::default();
        for (p, b, t) in hp_pages() {
            src.bytes.insert(p, b.to_vec());
            src.text.insert(p, t.to_string());
        }
        src
    }

    /// The first test against real-drive bytes: the HP LTO-6's page 0x00
    /// lists exactly the 22 README codes, the sweep reads each once, every
    /// read is ok and decoded, and the health counters parse to the values
    /// the offline decodes show.
    #[test]
    fn the_real_hp_lto6_fixture_set_sweeps_once_each_and_parses() {
        assert_eq!(hp_pages().len(), HP_LISTED.len());
        let (_, p00, _) = hp_pages()[0];
        assert_eq!(parse_supported_pages(p00).unwrap(), HP_LISTED.to_vec());

        let mut src = hp_source();
        let s = sweep(&mut src);
        assert_each_page_read_once(&src.reads);
        assert_eq!(src.reads.len(), 22, "22 distinct pages read");
        assert_eq!(src.order, HP_LISTED.to_vec(), "the pages read, in order");
        assert_eq!(s.listed.as_deref(), Some(&HP_LISTED[..]));
        assert!(s.captures.iter().all(PageCapture::ok), "every read ok");
        assert!(
            s.captures.iter().all(|c| c.decoded.is_some()),
            "every page decoded"
        );
        assert_eq!(src.decodes, 22);
        for (p, b, _) in hp_pages() {
            assert_eq!(
                s.page(p).unwrap().raw.as_deref(),
                Some(b),
                "0x{p:02x} captured verbatim"
            );
        }

        // Positive control: the inputs carry the lines the parser keys on.
        let text = |p: u8| hp_pages().into_iter().find(|(q, _, _)| *q == p).unwrap().2;
        assert!(text(0x02).contains("Total uncorrected errors = 0"));
        assert!(text(0x02).contains("Errors corrected without substantial delay = 920"));
        assert!(text(0x03).contains("Total times correction algorithm processed = 69"));

        let (c, raw_log) = s.health(None).expect("a health reading");
        assert_eq!(c.total_uncorrected, Some(0));
        assert_eq!(c.total_corrected, Some(0));
        assert_eq!(c.corrected_no_delay, Some(920 + 69));
        assert_eq!(c.corrected_with_delay, Some(0));
        assert_eq!(c.correction_algorithm_invocations, Some(306488 + 69));
        assert_eq!(c.total_bytes_processed, Some(12820), "the max, not the sum");
        assert_eq!(c.total_rewritten, Some(0));
        assert_eq!(c.total_retries, Some(0));
        assert_eq!(HealthCounters::from_raw_log(&raw_log), c);
    }

    /// The real HP LTO-6 lists 0x2e and every flag is 0: it stores 0.
    #[test]
    fn the_real_hp_lto6_stores_zero_tape_alerts() {
        let mut src = hp_source();
        let s = sweep(&mut src);
        let p2e = s.page(0x2e).expect("the HP LTO-6 lists 0x2e");
        assert!(p2e.ok(), "positive control: 0x2e read ok");
        assert!(p2e.decoded.as_deref().unwrap().contains("Hard error: 0"));
        assert_eq!(stored_tape_alerts(&s), Some(0));
    }

    // ── issue #322: every counter is NULL when a page it derives from was
    //    not read ok — never 0, never a partial sum (#317's rule, widened) ──

    /// Every counter column of the `health_logs` row a sweep's reading
    /// stores, read back through the production writer as `Option`.
    #[derive(Debug, PartialEq, Eq)]
    struct StoredCounters {
        total_bytes: Option<i64>,
        total_uncorrected: Option<i64>,
        total_corrected: Option<i64>,
        total_retries: Option<i64>,
        total_rewritten: Option<i64>,
        tape_alerts: Option<i64>,
    }

    fn stored_counters(s: &Sweep) -> StoredCounters {
        let conn = crate::db::open_memory().unwrap();
        let (counters, raw_log) = s.health(None).expect("a health reading");
        crate::tape::health::record(
            &conn,
            None,
            None,
            None,
            crate::tape::health::Reading::Write,
            &counters,
            &raw_log,
        )
        .unwrap();
        conn.query_row(
            "SELECT total_bytes, total_uncorrected, total_corrected, total_retries,
                    total_rewritten, tape_alerts FROM health_logs",
            [],
            |r| {
                Ok(StoredCounters {
                    total_bytes: r.get(0)?,
                    total_uncorrected: r.get(1)?,
                    total_corrected: r.get(2)?,
                    total_retries: r.get(3)?,
                    total_rewritten: r.get(4)?,
                    tape_alerts: r.get(5)?,
                })
            },
        )
        .unwrap()
    }

    /// Live (non-zero, pairwise distinct) texts for 0x02 and 0x03, so a
    /// stored value cannot be right by being 0 and a partial sum cannot pass
    /// for the whole: a partial `total_uncorrected` would be 1 or 4, the
    /// whole is 5.
    const LIVE_02: &str = "Write error counter page  [0x2]\n  Errors corrected without substantial delay = 875\n  Errors corrected with possible delays = 6\n  Total rewrites or rereads = 7\n  Total errors corrected = 8\n  Total times correction algorithm processed = 305674\n  Total bytes processed = 4096\n  Total uncorrected errors = 1\n";
    const LIVE_03: &str = "Read error counter page  [0x3]\n  Errors corrected without substantial delay = 2\n  Errors corrected with possible delays = 9\n  Total rewrites or rereads = 3\n  Total errors corrected = 10\n  Total times correction algorithm processed = 11\n  Total bytes processed = 8192\n  Total uncorrected errors = 4\n";

    fn live_source() -> FixtureSource {
        let mut src = FixtureSource::default();
        src.text.insert(0x02, LIVE_02.to_string());
        src.text.insert(0x03, LIVE_03.to_string());
        src
    }

    /// Positive control for every #322 case below: with BOTH pages read,
    /// every column is the whole figure — sums, the max, each page's own.
    #[test]
    fn both_error_counter_pages_read_store_every_counter() {
        let mut src = live_source();
        let s = sweep(&mut src);
        assert_each_page_read_once(&src.reads);
        assert_eq!(
            stored_counters(&s),
            StoredCounters {
                total_bytes: Some(8192),
                total_uncorrected: Some(5),
                total_corrected: Some(18),
                total_retries: Some(3),
                total_rewritten: Some(7),
                tape_alerts: Some(0),
            }
        );
    }

    /// Page 0x00 omits 0x02: `total_rewritten` (0x02 only) and every
    /// counter that sums or maxes both pages are NULL; `total_retries`
    /// (0x03 only) and `tape_alerts` (0x2e) are intact.
    #[test]
    fn a_page_list_without_0x02_stores_null_for_every_counter_0x02_feeds() {
        let mut src = live_source();
        src.bytes
            .insert(0x00, vec![0x00, 0x00, 0x00, 0x03, 0x00, 0x03, 0x2e]);
        let s = sweep(&mut src);
        assert_each_page_read_once(&src.reads);
        assert_eq!(src.order, vec![0x00, 0x03, 0x2e], "0x02 is never read");
        assert_eq!(
            stored_counters(&s),
            StoredCounters {
                total_bytes: None,
                total_uncorrected: None,
                total_corrected: None,
                total_retries: Some(3),
                total_rewritten: None,
                tape_alerts: Some(0),
            }
        );
    }

    /// Page 0x00 omits 0x03: the mirror image.
    #[test]
    fn a_page_list_without_0x03_stores_null_for_every_counter_0x03_feeds() {
        let mut src = live_source();
        src.bytes
            .insert(0x00, vec![0x00, 0x00, 0x00, 0x03, 0x00, 0x02, 0x2e]);
        let s = sweep(&mut src);
        assert_each_page_read_once(&src.reads);
        assert_eq!(src.order, vec![0x00, 0x02, 0x2e], "0x03 is never read");
        assert_eq!(
            stored_counters(&s),
            StoredCounters {
                total_bytes: None,
                total_uncorrected: None,
                total_corrected: None,
                total_retries: None,
                total_rewritten: Some(7),
                tape_alerts: Some(0),
            }
        );
    }

    /// 0x02 listed, its read fails: the same NULLs as unlisted.
    #[test]
    fn a_failed_0x02_read_stores_null_for_every_counter_0x02_feeds() {
        let mut src = live_source();
        src.fail.insert(0x02);
        let s = sweep(&mut src);
        assert_each_page_read_once(&src.reads);
        assert!(!s.page(0x02).unwrap().ok(), "0x02 was attempted and failed");
        assert!(s.page(0x03).unwrap().ok(), "positive control: 0x03 read");
        assert_eq!(
            stored_counters(&s),
            StoredCounters {
                total_bytes: None,
                total_uncorrected: None,
                total_corrected: None,
                total_retries: Some(3),
                total_rewritten: None,
                tape_alerts: Some(0),
            }
        );
    }

    /// 0x03 listed, its read fails: the mirror image.
    #[test]
    fn a_failed_0x03_read_stores_null_for_every_counter_0x03_feeds() {
        let mut src = live_source();
        src.fail.insert(0x03);
        let s = sweep(&mut src);
        assert_each_page_read_once(&src.reads);
        assert!(!s.page(0x03).unwrap().ok(), "0x03 was attempted and failed");
        assert!(s.page(0x02).unwrap().ok(), "positive control: 0x02 read");
        assert_eq!(
            stored_counters(&s),
            StoredCounters {
                total_bytes: None,
                total_uncorrected: None,
                total_corrected: None,
                total_retries: None,
                total_rewritten: Some(7),
                tape_alerts: Some(0),
            }
        );
    }

    /// Positive control: the mhvtl fixture set, every page read — today's
    /// values (all zero), and every one of them RECORDED.
    #[test]
    fn the_mhvtl_fixture_set_stores_every_counter_as_recorded() {
        let s = sweep(&mut FixtureSource::default());
        assert_eq!(
            stored_counters(&s),
            StoredCounters {
                total_bytes: Some(0),
                total_uncorrected: Some(0),
                total_corrected: Some(0),
                total_retries: Some(0),
                total_rewritten: Some(0),
                tape_alerts: Some(0),
            }
        );
    }

    /// Positive control: the real HP LTO-6 with no medium keeps today's
    /// values, every one recorded.
    #[test]
    fn the_real_hp_lto6_nomedia_set_stores_every_counter_as_recorded() {
        let s = sweep(&mut hp_source());
        assert_eq!(
            stored_counters(&s),
            StoredCounters {
                total_bytes: Some(12820),
                total_uncorrected: Some(0),
                total_corrected: Some(0),
                total_retries: Some(0),
                total_rewritten: Some(0),
                tape_alerts: Some(0),
            }
        );
    }

    /// Positive control: the real HP LTO-6 with the FUJIFILM cartridge
    /// loaded (`hp_lto6_sg0_fuji_ew7vwmvkf6`, issue #298), every page it
    /// lists answered from that set.
    #[test]
    fn the_real_hp_lto6_fuji_set_stores_every_counter_as_recorded() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/sg_logs/hp_lto6_sg0_fuji_ew7vwmvkf6");
        let p00 = std::fs::read(dir.join("page_0x00.bin")).unwrap();
        let listed = parse_supported_pages(&p00).unwrap();
        assert_eq!(listed.len(), 22, "positive control: the fixture's own 0x00");
        let mut src = FixtureSource::default();
        for p in &listed {
            src.bytes.insert(
                *p,
                std::fs::read(dir.join(format!("page_0x{p:02x}.bin"))).unwrap(),
            );
            src.text.insert(
                *p,
                std::fs::read_to_string(dir.join(format!("page_0x{p:02x}.decoded.txt"))).unwrap(),
            );
        }
        let s = sweep(&mut src);
        assert_each_page_read_once(&src.reads);
        assert_eq!(src.order, listed, "every page the fixture lists, in order");
        assert_eq!(
            stored_counters(&s),
            StoredCounters {
                total_bytes: Some(0),
                total_uncorrected: Some(0),
                total_corrected: Some(0),
                total_retries: Some(0),
                total_rewritten: Some(0),
                tape_alerts: Some(0),
            }
        );
    }

    // ── the counters: the sweep equals the old three-page path ──

    /// What the pre-#298 `collect` computed from three page texts:
    /// `parse_sg_logs_page` per page, merged — reproduced here through the
    /// unchanged `from_raw_log` over the old marker format.
    fn old_path(p02: &str, p03: &str, p2e: &str) -> HealthCounters {
        HealthCounters::from_raw_log(&format!(
            "=== page 0x02 ===\n{p02}\n=== page 0x03 ===\n{p03}\n=== page 0x2e ===\n{p2e}\n"
        ))
    }

    #[test]
    fn fixture_to_sweep_to_counters_equals_the_old_path() {
        let s = sweep(&mut FixtureSource::default());
        let (counters, _) = s.health(None).unwrap();
        let text = |p: u8| {
            fixture_pages()
                .into_iter()
                .find(|(q, _, _)| *q == p)
                .unwrap()
                .2
        };
        assert!(
            text(0x02).contains("Total uncorrected errors = 0"),
            "positive control: the old path is fed real page text"
        );
        assert_eq!(counters, old_path(text(0x02), text(0x03), text(0x2e)));
    }

    /// The same equality with LIVE (non-zero) inputs, so it cannot pass on
    /// all-zero counters alone: the real HP LTO-6 busy page 0x02 from the
    /// 2026-09-10 journal, a non-zero 0x03, and a TapeAlert page with flags.
    #[test]
    fn nonzero_pages_through_the_sweep_equal_the_old_path() {
        let p02 = "Write error counter page  [0x2]\n  Errors corrected without substantial delay = 875\n  Total errors corrected = 0\n  Total times correction algorithm processed = 305674\n  Total bytes processed = 4096\n  Total uncorrected errors = 1\n";
        let p03 = "Read error counter page  [0x3]\n  Errors corrected without substantial delay = 2\n  Total rewrites or rereads = 3\n  Total times correction algorithm processed = 2\n  Total uncorrected errors = 0\n";
        let p2e = "Tape alert page (ssc-3) [0x2e]\n  Read warning: 1\n  Write warning: 0\n  Hard error: 1\n";
        let expected = old_path(p02, p03, p2e);
        assert_ne!(
            expected,
            HealthCounters::default(),
            "positive control: the inputs are live"
        );
        assert_eq!(expected.corrected_no_delay, Some(877));
        assert_eq!(expected.tape_alerts, Some(2));

        let mut src = FixtureSource::default();
        src.text.insert(0x02, p02.to_string());
        src.text.insert(0x03, p03.to_string());
        src.text.insert(0x2e, p2e.to_string());
        let (counters, raw_log) = sweep(&mut src).health(None).unwrap();
        assert_eq!(counters, expected);
        assert_eq!(
            HealthCounters::from_raw_log(&raw_log),
            expected,
            "raw_log re-derives the same counters, as report health does"
        );
    }

    // ── the identity header ──

    /// A standard INQUIRY response for the fixture's drive: vendor, product
    /// and revision in their SPC fixed fields, space padded.
    fn inquiry_bytes(vendor: &str, product: &str, rev: &str) -> Vec<u8> {
        let mut b = vec![0x01, 0x80, 0x06, 0x02, 31, 0, 0, 0];
        b.extend(format!("{vendor:<8}").bytes());
        b.extend(format!("{product:<16}").bytes());
        b.extend(format!("{rev:<4}").bytes());
        assert_eq!(b.len(), 36);
        b
    }

    /// The rendered header is byte-identical to the line sg_logs itself
    /// printed for this drive (line 1 of the live page-0x00 capture).
    #[test]
    fn the_inquiry_header_renders_exactly_as_sg_logs_printed_it() {
        let live_header = PAGE_00_LIVE.lines().next().unwrap();
        assert!(
            live_header.contains("ULT3580-TD8"),
            "positive control: line 1 of the live capture is the header"
        );
        let rendered = render_inquiry_header(&inquiry_bytes("IBM", "ULT3580-TD8", "2160")).unwrap();
        assert_eq!(rendered, live_header);
        assert_eq!(render_inquiry_header(&[0u8; 35]), None, "too short");
    }

    /// The #295 route survives: `raw_log` built by the sweep, with the
    /// INQUIRY header, yields the drive identity through the UNCHANGED
    /// `drive_identity` parser and backfill — and without a header yields
    /// none, rather than inventing a drive from a page title.
    #[test]
    fn raw_log_still_yields_the_drive_identity_through_the_header_route() {
        use crate::tape::drive_identity::{parse_sg_logs_identity_header, DriveIdentity};
        let header = render_inquiry_header(&inquiry_bytes("IBM", "ULT3580-TD8", "2160"));
        let s = sweep(&mut FixtureSource::default());

        let (counters, raw_log) = s.health(header.as_deref()).unwrap();
        let id = parse_sg_logs_identity_header(&raw_log).expect("header route resolves");
        assert_eq!(id.vendor.as_deref(), Some("IBM"));
        assert_eq!(id.model.as_deref(), Some("ULT3580-TD8"));
        assert_eq!(id.firmware_rev.as_deref(), Some("2160"));
        let mut backfilled = DriveIdentity::default();
        backfilled.backfill_from_sg_logs_header(&raw_log);
        assert_eq!(backfilled.model.as_deref(), Some("ULT3580-TD8"));
        assert_eq!(
            HealthCounters::from_raw_log(&raw_log),
            counters,
            "the header before the first marker does not disturb the counters"
        );

        let (_, bare) = s.health(None).unwrap();
        assert!(
            bare.contains("Write error counter page"),
            "positive control"
        );
        assert_eq!(parse_sg_logs_identity_header(&bare), None);
    }

    // ── the journal: verbatim ──

    /// Every page's stored `raw` equals the fixture bytes exactly —
    /// including 0x37, which sg_logs cannot decode — and `decoded` is the
    /// offline decode, per contact, with the command verbatim as `trigger`.
    #[test]
    fn every_journalled_page_is_the_fixture_bytes_verbatim() {
        let conn = crate::db::open_memory().unwrap();
        let contact = open_contact(&conn);
        let ids = record_sweep(
            &conn,
            Some(contact),
            "volume write",
            Some("/dev/nst-fixture"),
            &sweep(&mut FixtureSource::default()),
        );
        assert_eq!(
            ids.len(),
            LISTED.len(),
            "one row per page read, 0x00 included"
        );

        let mut stmt = conn
            .prepare(
                "SELECT page_code, subpage_code, ok, raw, decoded, contact_id, trigger,
                        device_sg, device_tape, tool_argv, tool_version, tapectl_version, error
                   FROM log_page_journal ORDER BY id",
            )
            .unwrap();
        type Row = (
            u8,
            i64,
            i64,
            Vec<u8>,
            Option<String>,
            Option<i64>,
            String,
            String,
            Option<String>,
            String,
            Option<String>,
            String,
            Option<String>,
        );
        let rows: Vec<Row> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                    r.get(10)?,
                    r.get(11)?,
                    r.get(12)?,
                ))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let stored_pages: Vec<u8> = rows.iter().map(|r| r.0).collect();
        assert_eq!(stored_pages, LISTED.to_vec());

        for ((page, bytes, text), row) in fixture_pages().into_iter().zip(&rows) {
            assert_eq!(row.0, page);
            assert_eq!(row.1, 0, "subpage 0");
            assert_eq!(row.2, 1, "0x{page:02x} ok");
            assert_eq!(row.3, bytes, "0x{page:02x} raw is the response verbatim");
            assert_eq!(row.4.as_deref(), Some(text), "0x{page:02x} decoded");
            assert_eq!(row.5, Some(contact));
            assert_eq!(row.6, "volume write");
            assert_eq!(row.7, "/dev/sg-fixture");
            assert_eq!(row.8.as_deref(), Some("/dev/nst-fixture"));
            assert_eq!(
                row.9,
                format!(
                    r#"["sg_logs","--page=0x{page:02x}","--maxlen=65532","--raw","/dev/sg-fixture"]"#
                )
            );
            assert_eq!(row.10.as_deref(), Some("Version string: 1.81 20200110"));
            assert_eq!(row.11, env!("CARGO_PKG_VERSION"));
            assert_eq!(row.12, None);
        }
        let p37 = rows.iter().find(|r| r.0 == 0x37).unwrap();
        assert_eq!(
            p37.4.as_deref(),
            Some("Unable to decode Performance characteristics (lto-5) [pc_]\n"),
            "a page with no decoder keeps its bytes AND says it had none"
        );
        assert_eq!(
            p37.3,
            vec![0x37, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x01, 0x00]
        );
    }

    /// A failed listed page is an `ok = 0` row carrying its error and the
    /// (empty) stdout, beside `ok = 1` rows for the rest.
    #[test]
    fn a_failed_page_is_journalled_with_ok_zero() {
        let conn = crate::db::open_memory().unwrap();
        let mut src = FixtureSource::default();
        src.fail.insert(0x2e);
        record_sweep(&conn, None, "volume verify", None, &sweep(&mut src));
        type Row = (u8, i64, Option<String>, Option<Vec<u8>>, Option<String>);
        let rows: Vec<Row> = conn
            .prepare("SELECT page_code, ok, error, raw, decoded FROM log_page_journal ORDER BY id")
            .unwrap()
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows.len(), LISTED.len());
        let bad = rows.iter().find(|r| r.0 == 0x2e).unwrap();
        assert_eq!(bad.1, 0);
        assert!(bad.2.as_deref().unwrap().contains("exit"));
        assert_eq!(
            bad.3.as_deref(),
            Some(&[][..]),
            "the tool ran and printed nothing"
        );
        assert_eq!(bad.4, None);
        let good = rows.iter().find(|r| r.0 == 0x02).unwrap();
        assert_eq!((good.1, good.2.as_deref()), (1, None), "positive control");
    }

    /// Spawn failure: `raw` NULL, the error names it.
    #[test]
    fn a_spawn_failure_is_a_row_with_null_raw() {
        let c = capture_from_output(
            0x00,
            "/dev/sg0",
            "t".into(),
            SgLogs::read_argv("/dev/sg0", 0),
            None,
            Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        );
        assert!(!c.ok());
        assert_eq!(c.raw, None);
        assert!(c.error.unwrap().contains("spawn failed"));
    }

    /// A page longer than `--maxlen` comes back truncated with exit 0 (sg_logs
    /// only notes it on stderr). The header's declared length exposes it: the
    /// capture keeps the bytes but is not ok, so nothing decodes a partial
    /// page into counters. Positive control: a complete page of the same
    /// shape stays ok.
    #[test]
    fn a_truncated_page_is_kept_but_not_ok() {
        use std::os::unix::process::ExitStatusExt;
        let run = |stdout: Vec<u8>| {
            capture_from_output(
                0x2e,
                "/dev/sg0",
                "t".into(),
                SgLogs::read_argv("/dev/sg0", 0x2e),
                None,
                Ok(Output {
                    status: std::process::ExitStatus::from_raw(0),
                    stdout,
                    stderr: b"Only fetched 8 bytes of response".to_vec(),
                }),
            )
        };
        // Header declares 8 parameter bytes (12 total); 8 arrived.
        let short = vec![0x2e, 0, 0, 8, 0, 1, 3, 1];
        assert_eq!(truncated_page(&short), Some((12, 8)));
        let c = run(short.clone());
        assert!(!c.ok());
        assert_eq!(c.raw.as_deref(), Some(&short[..]), "the bytes are kept");
        let err = c.error.unwrap();
        assert!(err.contains("truncated"), "{err}");
        assert!(err.contains("declares 12 bytes, 8 received"), "{err}");

        let whole = vec![0x2e, 0, 0, 4, 0, 1, 3, 1];
        assert_eq!(truncated_page(&whole), None);
        let c = run(whole);
        assert!(c.ok(), "positive control: a complete page is ok");
        assert_eq!(c.error, None);

        // Fewer than 4 bytes carry no length: not called truncated here.
        assert_eq!(truncated_page(&[0x2e, 0]), None);
    }

    /// Every recorded fixture page is complete by its own header — the
    /// truncation check cannot misfire on what real drives returned.
    #[test]
    fn every_fixture_page_is_complete_by_its_header() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sg_logs");
        let mut n = 0;
        for sub in std::fs::read_dir(&dir).unwrap() {
            let sub = sub.unwrap().path();
            if !sub.is_dir() {
                continue;
            }
            for f in std::fs::read_dir(&sub).unwrap() {
                let f = f.unwrap().path();
                if f.extension().and_then(|e| e.to_str()) == Some("bin") {
                    let bytes = std::fs::read(&f).unwrap();
                    assert_eq!(truncated_page(&bytes), None, "{}", f.display());
                    n += 1;
                }
            }
        }
        assert!(n >= 50, "positive control: {n} fixture pages were checked");
    }

    /// A refused INSERT is best-effort. Positive control: a valid contact
    /// writes every row.
    #[test]
    fn a_refused_insert_is_best_effort() {
        let conn = crate::db::open_memory().unwrap();
        let s = sweep(&mut FixtureSource::default());
        assert!(record_sweep(&conn, Some(424_242), "volume write", None, &s).is_empty());
        let contact = open_contact(&conn);
        assert_eq!(
            record_sweep(&conn, Some(contact), "volume write", None, &s).len(),
            LISTED.len()
        );
    }

    // ── parse it later ──

    /// A parser nothing had before #298 — page 0x0D temperature — run over
    /// rows ALREADY in the journal, from their raw bytes: historical rows
    /// are re-parseable. Inputs are live: a synthetic 38 °C reading sits
    /// beside the fixture's 0 °C, and a failed read reads as `None`.
    #[test]
    fn a_later_parser_reads_historical_rows_from_the_journal() {
        let conn = crate::db::open_memory().unwrap();
        let c1 = open_contact(&conn);
        let c2 = open_contact(&conn);
        let c3 = open_contact(&conn);
        record_sweep(
            &conn,
            Some(c1),
            "volume write",
            None,
            &sweep(&mut FixtureSource::default()),
        );
        let mut warm = FixtureSource::default();
        warm.bytes.insert(
            0x0d,
            vec![0x0d, 0x00, 0x00, 0x06, 0x00, 0x00, 0x60, 0x02, 0x00, 38],
        );
        record_sweep(&conn, Some(c2), "volume write", None, &sweep(&mut warm));
        let mut failed = FixtureSource::default();
        failed.fail.insert(0x0d);
        record_sweep(&conn, Some(c3), "volume verify", None, &sweep(&mut failed));

        let history: Vec<(Option<i64>, Option<u8>)> = temperature_history(&conn)
            .unwrap()
            .into_iter()
            .map(|(_, c, t)| (c, t))
            .collect();
        assert_eq!(
            history,
            vec![(Some(c1), Some(0)), (Some(c2), Some(38)), (Some(c3), None)]
        );
    }

    #[test]
    fn temperature_parse_refuses_other_pages_and_not_available() {
        assert_eq!(
            parse_temperature(&[0x02, 0, 0, 6, 0, 0, 0x60, 2, 0, 38]),
            None
        );
        assert_eq!(
            parse_temperature(&[0x0d, 0, 0, 6, 0, 0, 0x60, 2, 0, 0xff]),
            None,
            "0xFF is 'not available'"
        );
        assert_eq!(
            parse_temperature(&[0x0d, 0, 0, 6, 0, 0, 0x60, 2, 0, 41]),
            Some(41)
        );
    }

    // ── the real tool, offline ──

    #[test]
    fn argv_shapes_are_the_fixture_capture_commands() {
        // `--maxlen` makes one invocation ONE LOG SENSE (issue #328): without
        // it sg_logs sends a 4-byte probe first, a second command at a page
        // that may clear when read. The fixtures were captured without it;
        // the stdout shape is the same (see `read_argv`).
        assert_eq!(
            SgLogs::read_argv("/dev/sg1", 0x2e),
            vec![
                "sg_logs",
                "--page=0x2e",
                "--maxlen=65532",
                "--raw",
                "/dev/sg1"
            ]
        );
        assert_eq!(READ_MAXLEN, 0xfffc, "sg_logs's own MX_ALLOC_LEN");
        assert_eq!(
            SgLogs::decode_argv(),
            vec!["sg_logs", "--in=-", "--raw", "--pdt=1"]
        );
    }

    /// The real offline decoder over every fixture's bytes reproduces the
    /// recorded decode exactly — proving the decode argv (`--pdt=1`
    /// especially) is the one the fixtures were made with. It touches no
    /// device (`--in=-`). sg3-utils is not a test dependency (only dar is,
    /// `tests/test_dependencies.rs`), so without `sg_logs` this says so and
    /// returns.
    #[test]
    fn the_real_offline_decoder_reproduces_every_fixture_decode() {
        if Command::new(LOG_TOOL).arg("-V").output().is_err() {
            eprintln!("SKIP: {LOG_TOOL} not on PATH; the offline decode was not exercised");
            return;
        }
        let mut n = 0;
        for (page, bytes, text) in fixture_pages() {
            let decoded =
                decode_offline(bytes).unwrap_or_else(|e| panic!("decode of 0x{page:02x}: {e}"));
            assert_eq!(decoded, text, "0x{page:02x}");
            n += 1;
        }
        assert_eq!(n, LISTED.len(), "positive control: every page was decoded");
    }
}
