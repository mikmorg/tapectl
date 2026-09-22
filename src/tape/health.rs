//! sg_logs health collection for tape drives.
//!
//! Shells out to `sg_logs` (sg3-utils) and parses the human-readable output
//! for log pages 0x02 (write errors), 0x03 (read errors), and 0x2e (tape alert).
//! Results are persisted to the `health_logs` table for trending.
//!
//! sg_logs output is human-oriented and varies across sg3-utils versions.
//! The parser is deliberately forgiving: it greps known key phrases and
//! ignores anything it does not understand. Unknown format = zeroed counters,
//! not a crash.
//!
//! Different vendors populate different parameters on pages 0x02/0x03
//! (issue #120). `Total errors corrected` is the parameter this module has
//! always read, but a real HP LTO-6 leaves it at 0 and reports its ECC
//! activity under `Errors corrected without substantial delay` and
//! `Total times correction algorithm processed` instead — the two
//! parameters that actually trend upward as a drive or medium degrades.
//! `total_corrected` stays exactly what it was (the persisted `health_logs`
//! column, faithful to `Total errors corrected`, and what older rows already
//! depend on); the trending pair is parsed alongside it into
//! `HealthCounters` and — since neither is a stored column — can also be
//! re-derived from a row's `raw_log` on demand via `from_raw_log`. `report
//! health` surfaces and labels both so a summary never claims a cleaner
//! picture than the drive is reporting.

use std::process::Command;

use rusqlite::{params, Connection};
use tracing::warn;

use crate::error::{Result, TapectlError};

/// Aggregated error counters across all parsed pages.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HealthCounters {
    pub total_bytes_processed: i64,
    pub total_uncorrected: i64,
    pub total_corrected: i64,
    pub total_retries: i64,
    pub total_rewritten: i64,
    pub tape_alerts: i64,
    /// "Errors corrected without substantial delay" (0x02/0x03). On drives
    /// that leave `total_corrected` (`Total errors corrected`) at 0, this is
    /// where ECC activity actually shows up — see the module doc.
    pub corrected_no_delay: i64,
    /// "Errors corrected with possible delays" (0x02/0x03) — sibling of
    /// `corrected_no_delay`, the costlier ECC bucket.
    pub corrected_with_delay: i64,
    /// "Total times correction algorithm processed" (0x02/0x03) — the other
    /// parameter that trends upward as a drive or medium degrades.
    pub correction_algorithm_invocations: i64,
}

/// Parse a single sg_logs page output into partial counters.
///
/// Merges into whatever fields that page actually reports.
/// For 0x02/0x03 (write/read error counter): extracts total-error fields.
/// For 0x2e (tape alert ssc-3): sums any non-zero flag as `tape_alerts`.
pub fn parse_sg_logs_page(page: u8, raw: &str) -> HealthCounters {
    let mut c = HealthCounters::default();

    match page {
        0x02 | 0x03 => {
            for line in raw.lines() {
                let line = line.trim();
                if let Some(v) = extract_counter(line, "Total uncorrected errors") {
                    c.total_uncorrected += v;
                } else if let Some(v) = extract_counter(line, "Total errors corrected") {
                    c.total_corrected += v;
                } else if let Some(v) =
                    extract_counter(line, "Errors corrected without substantial delay")
                {
                    c.corrected_no_delay += v;
                } else if let Some(v) =
                    extract_counter(line, "Errors corrected with possible delays")
                {
                    c.corrected_with_delay += v;
                } else if let Some(v) =
                    extract_counter(line, "Total times correction algorithm processed")
                {
                    c.correction_algorithm_invocations += v;
                } else if let Some(v) = extract_counter(line, "Total rewrites or rereads") {
                    // Page 0x02 calls this "rewrites", page 0x03 "rereads". Same line text.
                    if page == 0x02 {
                        c.total_rewritten += v;
                    } else {
                        c.total_retries += v;
                    }
                } else if let Some(v) = extract_counter(line, "Total bytes processed") {
                    // Last-writer-wins: write/read pages both report this; we want the max.
                    if v > c.total_bytes_processed {
                        c.total_bytes_processed = v;
                    }
                }
            }
        }
        0x2e => {
            // Every line of the form "  <flag name>: <0|1>"; sum the ones.
            for line in raw.lines() {
                let line = line.trim();
                if let Some(idx) = line.rfind(": ") {
                    let val = line[idx + 2..].trim();
                    if val == "1" {
                        c.tape_alerts += 1;
                    }
                }
            }
        }
        _ => {}
    }

    c
}

impl HealthCounters {
    /// Re-derive `HealthCounters` from a stored `raw_log` blob — the
    /// concatenated per-page text `collect()` writes, delimited by
    /// `=== page 0xNN ===` markers. Lets a caller recompute counters,
    /// including the two "trending" fields that are not persisted as their
    /// own columns (see the module doc), from an already-stored row without
    /// a second sg_logs collection.
    ///
    /// As forgiving as `parse_sg_logs_page`: text before the first marker is
    /// ignored, an empty or markerless string yields `HealthCounters::default()`,
    /// and an unrecognized page number contributes nothing (same as
    /// `parse_sg_logs_page`'s `_ => {}` arm).
    pub fn from_raw_log(raw: &str) -> HealthCounters {
        let mut totals = HealthCounters::default();
        let mut current_page: Option<u8> = None;
        let mut current_text = String::new();

        for line in raw.lines() {
            if let Some(page) = parse_page_marker(line) {
                if let Some(p) = current_page {
                    merge(&mut totals, parse_sg_logs_page(p, &current_text));
                }
                current_page = Some(page);
                current_text.clear();
            } else {
                current_text.push_str(line);
                current_text.push('\n');
            }
        }
        if let Some(p) = current_page {
            merge(&mut totals, parse_sg_logs_page(p, &current_text));
        }

        totals
    }
}

/// Parse a `=== page 0xNN ===` separator line (the format `collect()`
/// writes) into the page number. Returns `None` for anything else, so
/// ordinary log text never gets mistaken for a marker.
fn parse_page_marker(line: &str) -> Option<u8> {
    let line = line.trim();
    let hex = line.strip_prefix("=== page 0x")?.strip_suffix(" ===")?;
    u8::from_str_radix(hex, 16).ok()
}

/// Shell out to sg_logs and collect counters from all three pages.
///
/// Returns aggregated counters and the concatenated raw output (for
/// `health_logs.raw_log`). Errors from individual pages are logged as
/// warnings and do not fail the collection — a partial result is better
/// than none.
pub fn collect(sg_device: &str) -> Result<(HealthCounters, String)> {
    let mut totals = HealthCounters::default();
    let mut combined_raw = String::new();

    for page in [0x02u8, 0x03, 0x2e] {
        match run_sg_logs(sg_device, page) {
            Ok(raw) => {
                let c = parse_sg_logs_page(page, &raw);
                merge(&mut totals, c);
                combined_raw.push_str(&format!("=== page 0x{page:02x} ===\n"));
                combined_raw.push_str(&raw);
                combined_raw.push('\n');
            }
            Err(e) => {
                warn!(page = format!("0x{page:02x}"), err = %e, "sg_logs page collection failed");
            }
        }
    }

    Ok((totals, combined_raw))
}

/// What KIND of reading a `health_logs` row is — the `operation` vocabulary.
///
/// Free TEXT in the schema from migration 021 (ADR-0013 §4), which is why
/// this type exists: the CHECK that used to close the column was
/// `IN ('write','read','verify','clean')`, and `read` and `clean` have
/// **never had a writer**. A closed vocabulary already wrong in half its
/// values protects nothing, and it made the one value the code actually
/// needed — `resume` — impossible to write. The replacement is this enum plus
/// [`tests::the_reading_vocabulary_is_what_code_actually_writes`], which gives
/// the same typo protection at no migration cost and can additionally catch
/// the failure a CHECK never could: a permitted value nothing writes.
///
/// A **different vocabulary** from [`crate::tape::contact::Operation`], which
/// is the command verbatim (`volume init`, `volume verify`, …). Migration
/// 020's header says so explicitly and neither list may stand in for the
/// other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    /// A fresh `volume write`.
    Write,
    /// A `volume resume` continuing an interrupted write. Recorded as
    /// `'write'` until migration 021, because the CHECK forbade the honest
    /// word and a refused INSERT would have silently dropped the row.
    Resume,
    /// A `volume verify`.
    Verify,
}

impl Reading {
    /// Every value, for the pinning test. Kept in one place so the test
    /// cannot drift from the type.
    pub const ALL: &'static [Reading] = &[Reading::Write, Reading::Resume, Reading::Verify];

    pub fn as_str(self) -> &'static str {
        match self {
            Reading::Write => "write",
            Reading::Resume => "resume",
            Reading::Verify => "verify",
        }
    }
}

/// Insert a row into the `health_logs` table.
///
/// `volume_id` is `Option` from migration 021: a drive-only reading has no
/// volume (ADR-0013 §§2-3), and `NOT NULL` forbade recording one at all. No
/// production path passes `None` today — all three writers have a volume in
/// hand — so this is the schema no longer forbidding the reading, not a claim
/// that one already exists.
///
/// `contact_id` is the `cartridge_contacts` row this reading was taken
/// during (ADR-0013 §2, migration 021). It is what makes the reading
/// differenceable against its own pair, and the only route from a reading to
/// the DRIVE that produced it — without which "is it the drive or the tape?",
/// the central question in tape diagnostics, is unanswerable from this table.
///
/// `session_id` is the `verification_sessions` row this reading belongs to,
/// and **nothing else** (ADR-0013 §3: the column keeps exactly the meaning
/// its foreign key has always declared, and nothing may overload it). It has
/// existed since `001_initial.sql` with zero writers; issue #295 gives it
/// one, at the only call site that has the value — `volume_verify`, whose
/// session row is created by `volume_verify_with_store` and carried out on
/// `VerifyReport::session_id`. `None` on the write path, which runs no
/// verification session: a column with no writer reads as a promise
/// (issue #107's lesson), and a fabricated session id would be worse than
/// the NULL.
///
/// `tapectl_version` is **not a parameter**: there is exactly one build doing
/// the writing, so there is nothing for a caller to get wrong, and ADR-0013
/// §7 requires every row to record its observer — "parse it later" needs to
/// know which build wrote the parsed columns beside the raw text, or a parser
/// bug fixed in a later version is indistinguishable from a hardware change.
pub fn record(
    conn: &Connection,
    volume_id: Option<i64>,
    contact_id: Option<i64>,
    session_id: Option<i64>,
    operation: Reading,
    counters: &HealthCounters,
    raw_log: &str,
) -> Result<()> {
    conn.execute(
        // `tape_alerts` added by migration 009 (issue #107). It had been
        // parsed on every collection since this module was written and then
        // dropped on the floor here, because there was no column for it.
        // `contact_id` and `tapectl_version` added by migration 021.
        "INSERT INTO health_logs
            (volume_id, contact_id, session_id, operation, total_bytes, total_uncorrected,
             total_corrected, total_retries, total_rewritten, tape_alerts, raw_log,
             tapectl_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            volume_id,
            contact_id,
            session_id,
            operation.as_str(),
            counters.total_bytes_processed,
            counters.total_uncorrected,
            counters.total_corrected,
            counters.total_retries,
            counters.total_rewritten,
            counters.tape_alerts,
            raw_log,
            env!("CARGO_PKG_VERSION"),
        ],
    )?;
    Ok(())
}

// ── Is it the drive or the tape? ──────────────────────────────────────────

/// One drive's health readings, across every cartridge it read them through.
///
/// The FIRST of ADR-0013's two routes: a problem that follows the MACHINE
/// shows up here as one drive with bad readings across several cartridges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveReadings {
    pub drive_id: i64,
    pub serial: String,
    /// Readings taken during a contact with this drive.
    pub readings: i64,
    /// Distinct identified cartridges those readings came through. A
    /// reading whose contact never identified its cartridge counts in
    /// `readings` but not here.
    pub cartridges: i64,
    /// The worst `total_uncorrected` any one reading reported — deliberately
    /// not a SUM: whether these counters are per-load or lifetime is not yet
    /// established (ADR-0013 leaves trend analysis to the non-gating work),
    /// and summing lifetime counters would multiply-count one error.
    pub max_uncorrected: Option<i64>,
    /// Readings whose `tape_alerts` was recorded AND non-zero. A NULL (not
    /// recorded, 009) is never counted as either raised or clean.
    pub readings_with_alerts: i64,
}

/// One cartridge's health readings, across every drive that read it.
///
/// The SECOND route: a problem that follows the MEDIUM shows up here as one
/// cartridge with bad readings across several drives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CartridgeReadings {
    pub cartridge_id: i64,
    pub barcode: String,
    pub readings: i64,
    /// Distinct identified drives that took those readings. A reading whose
    /// contact recorded no drive counts in `readings` but not here.
    pub drives: i64,
    /// See [`DriveReadings::max_uncorrected`].
    pub max_uncorrected: Option<i64>,
    pub readings_with_alerts: i64,
}

/// Group health readings BY DRIVE across cartridges (ADR-0013 §§1-2, issue
/// #296's acceptance query).
///
/// A reading reaches its drive only through its contact
/// (`health_logs.contact_id -> cartridge_contacts.drive_id`); the table grows
/// no drive column of its own (§1). Readings that cannot be attributed — a
/// pre-021 row with no contact, or a contact whose drive published no serial
/// — are therefore absent from this grouping, not assigned a guessed drive.
pub fn readings_by_drive(conn: &Connection) -> Result<Vec<DriveReadings>> {
    let mut stmt = conn.prepare(
        "SELECT d.id, d.serial, COUNT(h.id), COUNT(DISTINCT cc.cartridge_id),
                MAX(h.total_uncorrected),
                SUM(CASE WHEN h.tape_alerts > 0 THEN 1 ELSE 0 END)
           FROM health_logs h
           JOIN cartridge_contacts cc ON cc.id = h.contact_id
           JOIN drives d ON d.id = cc.drive_id
          GROUP BY d.id
          ORDER BY d.serial",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(DriveReadings {
                drive_id: r.get(0)?,
                serial: r.get(1)?,
                readings: r.get(2)?,
                cartridges: r.get(3)?,
                max_uncorrected: r.get(4)?,
                readings_with_alerts: r.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Group health readings BY CARTRIDGE across drives — the other route; see
/// [`readings_by_drive`]. A reading reaches its cartridge through its
/// contact (`cartridge_contacts.cartridge_id`), never through `volume_id`:
/// a volume is a logical thing a cartridge can be re-initialised away from
/// (ADR-0010/0011), and a blank or foreign tape has no volume at all.
pub fn readings_by_cartridge(conn: &Connection) -> Result<Vec<CartridgeReadings>> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.barcode, COUNT(h.id), COUNT(DISTINCT cc.drive_id),
                MAX(h.total_uncorrected),
                SUM(CASE WHEN h.tape_alerts > 0 THEN 1 ELSE 0 END)
           FROM health_logs h
           JOIN cartridge_contacts cc ON cc.id = h.contact_id
           JOIN cartridges c ON c.id = cc.cartridge_id
          GROUP BY c.id
          ORDER BY c.barcode",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(CartridgeReadings {
                cartridge_id: r.get(0)?,
                barcode: r.get(1)?,
                readings: r.get(2)?,
                drives: r.get(3)?,
                max_uncorrected: r.get(4)?,
                readings_with_alerts: r.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn run_sg_logs(sg_device: &str, page: u8) -> Result<String> {
    let output = Command::new("sg_logs")
        .arg(format!("--page=0x{page:02x}"))
        .arg(sg_device)
        .output()
        .map_err(|e| TapectlError::Other(format!("sg_logs spawn failed: {e}")))?;
    if !output.status.success() {
        return Err(TapectlError::Other(format!(
            "sg_logs page 0x{page:02x} exit {}",
            output.status
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn merge(into: &mut HealthCounters, from: HealthCounters) {
    into.total_uncorrected += from.total_uncorrected;
    into.total_corrected += from.total_corrected;
    into.total_retries += from.total_retries;
    into.total_rewritten += from.total_rewritten;
    into.tape_alerts += from.tape_alerts;
    into.corrected_no_delay += from.corrected_no_delay;
    into.corrected_with_delay += from.corrected_with_delay;
    into.correction_algorithm_invocations += from.correction_algorithm_invocations;
    if from.total_bytes_processed > into.total_bytes_processed {
        into.total_bytes_processed = from.total_bytes_processed;
    }
}

/// Extract the integer value from `"<name> = <num>"`, returning None on miss.
fn extract_counter(line: &str, name: &str) -> Option<i64> {
    let idx = line.find(name)?;
    let rest = &line[idx + name.len()..];
    let eq = rest.find('=')?;
    rest[eq + 1..].trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixtures captured from mhvtl 1.8 + sg3-utils 1.46 on 2026-04-11
    // against /dev/sg1 (an IBM ULT3580-TD8 emulation). See
    // tests/fixtures/sg_logs/ for the raw files.
    const PAGE_02: &str = include_str!("../../tests/fixtures/sg_logs/page_0x02.txt");
    const PAGE_03: &str = include_str!("../../tests/fixtures/sg_logs/page_0x03.txt");
    const PAGE_2E: &str = include_str!("../../tests/fixtures/sg_logs/page_0x2e.txt");

    #[test]
    fn parse_page_02_clean_tape() {
        let c = parse_sg_logs_page(0x02, PAGE_02);
        assert_eq!(c.total_uncorrected, 0);
        assert_eq!(c.total_corrected, 0);
        assert_eq!(c.total_rewritten, 0);
        assert_eq!(c.tape_alerts, 0);
    }

    #[test]
    fn parse_page_03_clean_tape() {
        let c = parse_sg_logs_page(0x03, PAGE_03);
        assert_eq!(c.total_uncorrected, 0);
        assert_eq!(c.total_retries, 0);
    }

    #[test]
    fn parse_page_2e_no_alerts() {
        let c = parse_sg_logs_page(0x2e, PAGE_2E);
        assert_eq!(c.tape_alerts, 0);
    }

    #[test]
    fn parse_page_02_with_error_counts() {
        let raw = "\
    IBM       ULT3580-TD8       2160
Write error counter page  [0x2]
  Errors corrected without substantial delay = 5
  Errors corrected with possible delays = 2
  Total rewrites or rereads = 7
  Total errors corrected = 3
  Total times correction algorithm processed = 0
  Total bytes processed = 1048576
  Total uncorrected errors = 1
";
        let c = parse_sg_logs_page(0x02, raw);
        assert_eq!(c.total_uncorrected, 1);
        assert_eq!(c.total_corrected, 3);
        assert_eq!(c.total_rewritten, 7);
        assert_eq!(c.total_bytes_processed, 1048576);
    }

    #[test]
    fn parse_page_2e_with_alerts() {
        let raw = "\
Tape alert page (ssc-3) [0x2e]
  Read warning: 1
  Write warning: 0
  Hard error: 1
  Media: 0
  Media life: 1
";
        let c = parse_sg_logs_page(0x2e, raw);
        assert_eq!(c.tape_alerts, 3);
    }

    #[test]
    fn parse_unknown_page_yields_zeros() {
        let c = parse_sg_logs_page(0x99, "anything at all = 12345\n");
        assert_eq!(c, HealthCounters::default());
    }

    #[test]
    fn parse_malformed_lines_are_skipped() {
        let raw = "\
Write error counter page [0x2]
  Total uncorrected errors = not-a-number
  garbage line with no delimiter
  Total errors corrected = 42
";
        let c = parse_sg_logs_page(0x02, raw);
        assert_eq!(c.total_uncorrected, 0); // malformed, skipped
        assert_eq!(c.total_corrected, 42);
    }

    // ── Issue #120: this drive's ECC activity trends under different
    // parameters than the one the summary reads ──────────────────────────
    //
    // Measured on a real HP LTO-6 during the 2026-09-10 session
    // (`docs/lto6-session-journal-2026-09-10.md`): `Total errors corrected`
    // sits at 0 on this drive while `Errors corrected without substantial
    // delay` (875) and `Total times correction algorithm processed`
    // (305,674) carry its actual ECC activity.

    // Real HP LTO-6 (Ultrium 6-SCSI) sg_logs output, pages 0x02/0x03,
    // captured verbatim during that session (recordings under
    // /scratch/tapectl-lto6-session/recordings/measure/run-20260910-012953/)
    // — the pre-write/idle state, every counter at 0. This proves the parser
    // extracts fields correctly against the REAL device's banner and field
    // text, not only mhvtl's IBM emulation shape used by PAGE_02/PAGE_03
    // above.
    const HP_LTO6_PAGE_02: &str =
        include_str!("../../tests/fixtures/sg_logs/hp_lto6_page_0x02.txt");
    const HP_LTO6_PAGE_03: &str =
        include_str!("../../tests/fixtures/sg_logs/hp_lto6_page_0x03.txt");

    #[test]
    fn parse_real_hp_lto6_page_02_zero_state() {
        let c = parse_sg_logs_page(0x02, HP_LTO6_PAGE_02);
        assert_eq!(c.total_uncorrected, 0);
        assert_eq!(c.total_corrected, 0);
        assert_eq!(c.corrected_no_delay, 0);
        assert_eq!(c.corrected_with_delay, 0);
        assert_eq!(c.correction_algorithm_invocations, 0);
    }

    #[test]
    fn parse_real_hp_lto6_page_03_zero_state() {
        let c = parse_sg_logs_page(0x03, HP_LTO6_PAGE_03);
        assert_eq!(c.total_uncorrected, 0);
        assert_eq!(c.corrected_no_delay, 0);
        assert_eq!(c.correction_algorithm_invocations, 0);
    }

    /// Quoted VERBATIM from the journal's "⚠ `report health` reports
    /// `corrected=0` while the drive reports 875" subsection (issue #120) —
    /// the busy-page excerpt the bug report is actually about. This is a
    /// DIFFERENT capture than `HP_LTO6_PAGE_02` above: that file is the
    /// pre-write preflight/measure snapshot (all zero) still on disk; this
    /// text is what the same drive reported after the write. The post-write
    /// raw sg_logs output was never itself saved to a file anywhere in this
    /// session's recordings (confirmed by grepping the whole
    /// `/scratch/tapectl-lto6-session` tree for "875" and "305674" — no
    /// hits outside the journal) — only this quoted excerpt survives, so it
    /// is reproduced here exactly rather than treated as a full page dump.
    const HP_LTO6_BUSY_PAGE_02_JOURNAL_EXCERPT: &str = "\
Write error counter page [0x2]
  Errors corrected without substantial delay   = 875
  Total errors corrected                       = 0
  Total times correction algorithm processed   = 305674
  Total uncorrected errors                     = 0
";

    #[test]
    fn parse_real_hp_lto6_busy_page_02_from_journal() {
        let c = parse_sg_logs_page(0x02, HP_LTO6_BUSY_PAGE_02_JOURNAL_EXCERPT);
        assert_eq!(c.total_corrected, 0, "faithful to Total errors corrected");
        assert_eq!(c.total_uncorrected, 0);
        assert_eq!(c.corrected_no_delay, 875);
        assert_eq!(c.correction_algorithm_invocations, 305_674);
    }

    /// No real page-0x03 capture at a non-zero state exists anywhere in the
    /// session recordings, and the journal doesn't quote one either — only
    /// page 0x02 is discussed there. This page is therefore SYNTHETIC: the
    /// real HP field shape (banner + field order taken from `HP_LTO6_PAGE_03`
    /// above) with small values substituted, purely to prove `from_raw_log`
    /// sums page 0x02 *and* 0x03 rather than only reading one of them. It is
    /// not presented as a real capture anywhere outside this comment.
    const SYNTHETIC_HP_LTO6_PAGE_03: &str = "\
    HP        Ultrium 6-SCSI    35GD
Read error counter page  [0x3]
  Errors corrected without substantial delay = 2
  Errors corrected with possible delays = 0
  Total rewrites or rereads = 0
  Total errors corrected = 0
  Total times correction algorithm processed = 2
  Total bytes processed = 0
  Total uncorrected errors = 0
";

    #[test]
    fn parse_synthetic_hp_lto6_page_03() {
        let c = parse_sg_logs_page(0x03, SYNTHETIC_HP_LTO6_PAGE_03);
        assert_eq!(c.corrected_no_delay, 2);
        assert_eq!(c.correction_algorithm_invocations, 2);
    }

    #[test]
    fn from_raw_log_sums_the_trending_fields_across_pages_02_and_03() {
        let raw = format!(
            "=== page 0x02 ===\n{HP_LTO6_BUSY_PAGE_02_JOURNAL_EXCERPT}\n=== page 0x03 ===\n{SYNTHETIC_HP_LTO6_PAGE_03}\n"
        );
        let c = HealthCounters::from_raw_log(&raw);
        assert_eq!(c.corrected_no_delay, 875 + 2);
        assert_eq!(c.correction_algorithm_invocations, 305_674 + 2);
        assert_eq!(c.total_corrected, 0);
        assert_eq!(c.total_uncorrected, 0);
    }

    /// `from_raw_log` must carry `tape_alerts` through the re-derive too —
    /// not just the two new trending fields — so a three-page concatenation
    /// (0x02 + 0x03 + 0x2e) is exercised here, not only the two pages issue
    /// #120 is about.
    #[test]
    fn from_raw_log_three_pages_includes_tape_alerts() {
        let raw = format!(
            "=== page 0x02 ===\n{HP_LTO6_BUSY_PAGE_02_JOURNAL_EXCERPT}\n=== page 0x03 ===\n{SYNTHETIC_HP_LTO6_PAGE_03}\n=== page 0x2e ===\n{PAGE_2E}\n"
        );
        let c = HealthCounters::from_raw_log(&raw);
        assert_eq!(c.corrected_no_delay, 875 + 2);
        assert_eq!(c.tape_alerts, 0); // PAGE_2E fixture has no raised flags
    }

    /// Text before the first `=== page 0xNN ===` marker (a stray blank line,
    /// or output the parser doesn't recognize) must be ignored rather than
    /// mis-attributed to page 0 — consistent with the "deliberately
    /// forgiving" parser design (module doc): unknown input yields zeroed
    /// counters, never an error.
    #[test]
    fn from_raw_log_ignores_text_before_first_marker() {
        let raw = format!(
            "some stray preamble\nnot a page marker\n=== page 0x02 ===\n{HP_LTO6_BUSY_PAGE_02_JOURNAL_EXCERPT}\n"
        );
        let c = HealthCounters::from_raw_log(&raw);
        assert_eq!(c.corrected_no_delay, 875);
    }

    #[test]
    fn from_raw_log_of_empty_string_is_zero() {
        assert_eq!(HealthCounters::from_raw_log(""), HealthCounters::default());
    }

    #[test]
    fn record_inserts_row() {
        // Full ordered migration chain (issue #44) — was a hand-applied
        // 001-only snapshot.
        let conn = crate::db::open_memory().unwrap();

        // Minimal volume row
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('V1', 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
            [],
        )
        .unwrap();
        let vid = conn.last_insert_rowid();

        let counters = HealthCounters {
            total_bytes_processed: 1024,
            total_uncorrected: 0,
            total_corrected: 2,
            total_retries: 1,
            total_rewritten: 3,
            tape_alerts: 0,
            corrected_no_delay: 0,
            corrected_with_delay: 0,
            correction_algorithm_invocations: 0,
        };
        record(
            &conn,
            Some(vid),
            None,
            None,
            Reading::Write,
            &counters,
            "raw log contents",
        )
        .unwrap();

        let (bytes, uncorrected, corrected, raw): (i64, i64, i64, String) = conn
            .query_row(
                "SELECT total_bytes, total_uncorrected, total_corrected, raw_log
                 FROM health_logs WHERE volume_id = ?1",
                params![vid],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(bytes, 1024);
        assert_eq!(uncorrected, 0);
        assert_eq!(corrected, 2);
        assert_eq!(raw, "raw log contents");
    }

    /// Issue #107: `tape_alerts` was parsed on every collection and then
    /// discarded, because the INSERT wrote eight columns and this was not
    /// one of them. Migration 009 adds the column; this proves the value
    /// actually survives the round trip rather than being computed into a
    /// local and dropped, which is exactly what it did before.
    ///
    /// A raised tape alert is the drive reporting that the medium or the
    /// head is degrading — the most directly actionable signal a tape
    /// system produces, and it was the one number being thrown away.
    #[test]
    fn record_persists_the_tape_alert_count() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('V-ALERT', 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
            [],
        )
        .unwrap();
        let vid = conn.last_insert_rowid();

        let counters = HealthCounters {
            total_bytes_processed: 1,
            total_uncorrected: 0,
            total_corrected: 0,
            total_retries: 0,
            total_rewritten: 0,
            tape_alerts: 3,
            corrected_no_delay: 0,
            corrected_with_delay: 0,
            correction_algorithm_invocations: 0,
        };
        record(
            &conn,
            Some(vid),
            None,
            None,
            Reading::Verify,
            &counters,
            "raw",
        )
        .unwrap();

        let stored: Option<i64> = conn
            .query_row(
                "SELECT tape_alerts FROM health_logs WHERE volume_id = ?1",
                params![vid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stored,
            Some(3),
            "the parsed alert count must reach the database, not a local variable"
        );
    }

    /// Zero alerts must store as 0, never NULL. The distinction is the whole
    /// reason the column is nullable: NULL means "not recorded" (a row
    /// written before migration 009), 0 means "recorded, and the drive
    /// raised none". A reader that conflates them would report a clean drive
    /// for a collection that never happened.
    #[test]
    fn zero_alerts_is_stored_as_zero_not_null() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('V-CLEAN', 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
            [],
        )
        .unwrap();
        let vid = conn.last_insert_rowid();

        record(
            &conn,
            Some(vid),
            None,
            None,
            Reading::Verify,
            &HealthCounters {
                total_bytes_processed: 1,
                total_uncorrected: 0,
                total_corrected: 0,
                total_retries: 0,
                total_rewritten: 0,
                tape_alerts: 0,
                corrected_no_delay: 0,
                corrected_with_delay: 0,
                correction_algorithm_invocations: 0,
            },
            "raw",
        )
        .unwrap();

        let stored: Option<i64> = conn
            .query_row(
                "SELECT tape_alerts FROM health_logs WHERE volume_id = ?1",
                params![vid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stored,
            Some(0),
            "recorded-and-clean must not read as unknown"
        );
    }

    /// `health_logs.session_id` has existed since `001_initial.sql` with a
    /// foreign key to `verification_sessions(id)` and ZERO writers. Issue
    /// #295 gives it one; ADR-0013 §3 narrows its meaning to exactly what
    /// that foreign key declares and forbids overloading it. This proves the
    /// value survives the round trip and that the join the column was
    /// always for actually resolves — `report verify-status` and `report
    /// health` become joinable on it.
    #[test]
    fn record_populates_session_id_and_the_join_resolves() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('V-SESSION', 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
            [],
        )
        .unwrap();
        let vid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO verification_sessions
                (volume_id, verify_type, outcome, completed_at, slices_checked, slices_passed, slices_failed)
             VALUES (?1, 'full', 'passed', datetime('now'), 3, 3, 0)",
            params![vid],
        )
        .unwrap();
        let session_id = conn.last_insert_rowid();

        record(
            &conn,
            Some(vid),
            None,
            Some(session_id),
            Reading::Verify,
            &HealthCounters::default(),
            "raw",
        )
        .unwrap();

        // Read the health row back THROUGH the join, not by reading the
        // column and trusting it points somewhere.
        let (stored_session, outcome): (i64, String) = conn
            .query_row(
                "SELECT vs.id, vs.outcome
                   FROM health_logs h
                   JOIN verification_sessions vs ON vs.id = h.session_id
                  WHERE h.volume_id = ?1",
                params![vid],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored_session, session_id);
        assert_eq!(outcome, "passed");
    }

    /// The write path runs no verification session, so its rows must carry
    /// NULL rather than a fabricated id — the paired negative for the join
    /// test above, and the reason `session_id` is `Option<i64>`.
    #[test]
    fn a_write_path_reading_records_a_null_session_id() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('V-NOSESSION', 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
            [],
        )
        .unwrap();
        let vid = conn.last_insert_rowid();
        record(
            &conn,
            Some(vid),
            None,
            None,
            Reading::Write,
            &HealthCounters::default(),
            "raw",
        )
        .unwrap();

        let stored: Option<i64> = conn
            .query_row(
                "SELECT session_id FROM health_logs WHERE volume_id = ?1",
                params![vid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, None);
    }

    /// The tripwire from issue #295, **fired and rewritten** — not deleted.
    ///
    /// Its first life asserted the opposite: that `record(…, "resume", …)`
    /// FAILED with `CHECK constraint failed`, because
    /// `001_initial.sql`'s `CHECK(operation IN ('write','read','verify','clean'))`
    /// was still in force and `collect_health_best_effort` only *warns* on an
    /// insert failure — so writing the honest word would have silently
    /// dropped the health row on every resume, strictly worse than the
    /// mislabel it fixed. `volume resume` therefore recorded `'write'`, and
    /// the test pinned why. It was written to go RED the moment migration 021
    /// dropped the CHECK: a reminder wired into the gate rather than a
    /// sentence in an issue.
    ///
    /// 021 has landed, so this is the other side of it. Two halves, because
    /// "the schema now permits it" is only half the fix — the other half is
    /// that `volume resume` actually says it:
    ///
    /// 1. `Reading::Resume` round-trips and stores the string `resume`.
    /// 2. `volume::write::volume_resume` **passes it**. Without this, 021
    ///    would have landed leaving the vocabulary naming a value nothing
    ///    writes — which is precisely the `read`/`clean` defect ADR-0013 §4
    ///    was filed against, reintroduced by its own fix.
    #[test]
    fn resume_is_recorded_as_resume_now_that_migration_021_dropped_the_check() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('V-RESUME', 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
            [],
        )
        .unwrap();
        let vid = conn.last_insert_rowid();

        record(
            &conn,
            Some(vid),
            None,
            None,
            Reading::Resume,
            &HealthCounters::default(),
            "raw",
        )
        .expect("migration 021 drops the CHECK that used to refuse 'resume'");

        let stored: String = conn
            .query_row(
                "SELECT operation FROM health_logs WHERE volume_id = ?1",
                params![vid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stored, "resume",
            "a resume must record the word it means, not 'write'"
        );

        // Half two: the production writer. `volume_resume` is the only caller
        // that may pass this, and it reaches `record` through
        // `collect_health_best_effort`'s `operation` parameter — threaded
        // through by #295 for exactly this flip.
        let resume_fn = source_of("volume/write.rs")
            .split("fn volume_resume_contacted")
            .nth(1)
            .expect(
                "positive control: volume_resume_contacted must exist in \
                 src/volume/write.rs — the scan below is vacuous without it",
            )
            .to_string();
        assert!(
            resume_fn.contains("health::Reading::Resume") || resume_fn.contains("Reading::Resume"),
            "volume_resume_contacted must pass Reading::Resume — a vocabulary \
             value with no writer is the `read`/`clean` defect ADR-0013 §4 names"
        );
    }

    /// The pinning test ADR-0013 §4 requires in place of the dropped CHECK,
    /// and the half a CHECK could never do — the sibling of
    /// `tape::contact`'s `the_operation_vocabulary_is_what_code_actually_writes`.
    ///
    /// **These are two different vocabularies and neither list may stand in
    /// for the other.** `health_logs.operation` is what KIND of reading a row
    /// is; `cartridge_contacts.operation` is the command verbatim
    /// (`volume init`, `volume verify`, …). Migration 020's header says so
    /// explicitly.
    ///
    /// Two halves, because either alone is the defect the ADR describes:
    ///
    /// 1. The strings are exactly these — the typo protection the CHECK gave.
    /// 2. **Every one of them has a production writer.** The expected set is
    ///    deliberately NOT seeded from the CHECK's four values: `read` and
    ///    `clean` never had a writer, so a list asserted only against itself
    ///    would enshrine exactly the lie the CHECK was told to stop telling —
    ///    in the one artefact that is supposed to be the evidence.
    #[test]
    fn the_reading_vocabulary_is_what_code_actually_writes() {
        let names: Vec<&str> = Reading::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(
            names,
            vec!["write", "resume", "verify"],
            "health_logs.operation is what KIND of reading a row is — a \
             different vocabulary from cartridge_contacts.operation, which is \
             the command verbatim"
        );

        let corpus = source_of("volume/write.rs");
        // Positive control on the scan: a corpus that read nothing would make
        // every assertion below vacuous (issues #282/#284/#285).
        assert!(
            corpus.contains("collect_health_best_effort"),
            "positive control: the source scan must actually have read the call sites"
        );
        for reading in Reading::ALL {
            let variant = format!("Reading::{reading:?}");
            assert!(
                corpus.contains(&variant),
                "{variant} has no writer in src/volume/write.rs — a vocabulary \
                 value with no writer is exactly the `read`/`clean` defect \
                 ADR-0013 §4 names, and it is a FINDING, not a row to add"
            );
        }
    }

    /// ADR-0013 §7: every row records the observer. "Parse it later" requires
    /// knowing which build wrote the parsed columns beside the raw text, or a
    /// parser bug fixed in a later version is indistinguishable from a
    /// hardware change.
    ///
    /// It is not a parameter — there is one build doing the writing — so what
    /// this proves is that `record` stamps it at all. The paired negative is
    /// in `db::tests::test_migrate_020_populated_db_to_021_preserves_every_recorded_fact`:
    /// a pre-021 row keeps NULL, because it genuinely does not know.
    #[test]
    fn record_stamps_the_build_that_wrote_the_row() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('V-VERSION', 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
            [],
        )
        .unwrap();
        let vid = conn.last_insert_rowid();
        record(
            &conn,
            Some(vid),
            None,
            None,
            Reading::Write,
            &HealthCounters::default(),
            "raw",
        )
        .unwrap();

        let stored: Option<String> = conn
            .query_row(
                "SELECT tapectl_version FROM health_logs WHERE volume_id = ?1",
                params![vid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    }

    /// Issue #296's acceptance, both routes: 2 drives x 2 cartridges, every
    /// pairing read once, so each grouping is non-trivial — every drive
    /// spans two cartridges and every cartridge two drives.
    ///
    /// `SER-BAD` is the bad drive: its readings are bad on BOTH cartridges,
    /// while each cartridge is bad on only one drive. So the by-drive route
    /// shows the fault following the machine, and the by-cartridge route
    /// shows each medium clean on the good drive — the two answers to "is
    /// it the drive or the tape?" read off one fixture.
    ///
    /// Plus the two readings neither route may attribute: a pre-021 row
    /// with no contact (in neither grouping) and a drive-only reading with no
    /// identified cartridge (counts for its drive, not for any cartridge).
    fn seed_two_by_two(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO cartridges (id, barcode, media_type, nominal_capacity)
                VALUES (10, 'C1', 'LTO-6', 2500000000000),
                       (11, 'C2', 'LTO-6', 2500000000000);
             INSERT INTO drives (id, serial) VALUES (20, 'SER-BAD'), (21, 'SER-GOOD');
             INSERT INTO volumes (id, label, backend_type, backend_name, media_type, capacity_bytes, status)
                VALUES (1, 'V-OLD', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed');
             INSERT INTO cartridge_contacts (id, cartridge_id, drive_id, operation, device)
                VALUES (30, 10, 20, 'volume verify', '/dev/nst0'),
                       (31, 11, 20, 'volume verify', '/dev/nst0'),
                       (32, 10, 21, 'volume verify', '/dev/nst1'),
                       (33, 11, 21, 'volume verify', '/dev/nst1'),
                       (34, NULL, 21, 'volume identify', '/dev/nst1');
             INSERT INTO health_logs (volume_id, contact_id, operation, total_uncorrected, tape_alerts)
                VALUES (NULL, 30, 'verify', 7, 2),
                       (NULL, 31, 'verify', 9, 1),
                       (NULL, 32, 'verify', 0, 0),
                       (NULL, 33, 'verify', 0, NULL),
                       (NULL, 34, 'verify', 0, 0),
                       (1, NULL, 'write', 99, 5);",
        )
        .unwrap();
    }

    #[test]
    fn readings_group_by_drive_across_cartridges() {
        let conn = crate::db::open_memory().unwrap();
        seed_two_by_two(&conn);
        let by_drive = readings_by_drive(&conn).unwrap();
        assert!(
            !by_drive.is_empty(),
            "positive control: the grouping saw rows"
        );
        assert_eq!(
            by_drive,
            vec![
                DriveReadings {
                    drive_id: 20,
                    serial: "SER-BAD".into(),
                    readings: 2,
                    cartridges: 2,
                    max_uncorrected: Some(9),
                    readings_with_alerts: 2,
                },
                DriveReadings {
                    drive_id: 21,
                    serial: "SER-GOOD".into(),
                    // Three: two cartridges plus the drive-only reading,
                    // which has a drive but no identified cartridge.
                    readings: 3,
                    cartridges: 2,
                    max_uncorrected: Some(0),
                    readings_with_alerts: 0,
                },
            ],
            "the contact-less pre-021 row (uncorrected 99) must be in NO group"
        );
    }

    #[test]
    fn readings_group_by_cartridge_across_drives() {
        let conn = crate::db::open_memory().unwrap();
        seed_two_by_two(&conn);
        let by_cartridge = readings_by_cartridge(&conn).unwrap();
        assert!(
            !by_cartridge.is_empty(),
            "positive control: the grouping saw rows"
        );
        assert_eq!(
            by_cartridge,
            vec![
                CartridgeReadings {
                    cartridge_id: 10,
                    barcode: "C1".into(),
                    readings: 2,
                    drives: 2,
                    max_uncorrected: Some(7),
                    readings_with_alerts: 1,
                },
                CartridgeReadings {
                    cartridge_id: 11,
                    barcode: "C2".into(),
                    readings: 2,
                    drives: 2,
                    max_uncorrected: Some(9),
                    // C2 on SER-GOOD recorded NULL alerts: not counted as
                    // raised, and not as clean either.
                    readings_with_alerts: 1,
                },
            ],
            "neither the drive-only reading nor the pre-021 row names a cartridge"
        );
    }

    /// Read one source file under `src/`, for the writer halves above.
    fn source_of(relative: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join(relative);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"))
    }
}
