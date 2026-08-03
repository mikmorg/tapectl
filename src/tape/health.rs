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

/// Insert a row into the `health_logs` table.
pub fn record(
    conn: &Connection,
    volume_id: i64,
    operation: &str,
    counters: &HealthCounters,
    raw_log: &str,
) -> Result<()> {
    conn.execute(
        // `tape_alerts` added by migration 009 (issue #107). It had been
        // parsed on every collection since this module was written and then
        // dropped on the floor here, because there was no column for it.
        "INSERT INTO health_logs
            (volume_id, operation, total_bytes, total_uncorrected,
             total_corrected, total_retries, total_rewritten, tape_alerts, raw_log)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            volume_id,
            operation,
            counters.total_bytes_processed,
            counters.total_uncorrected,
            counters.total_corrected,
            counters.total_retries,
            counters.total_rewritten,
            counters.tape_alerts,
            raw_log,
        ],
    )?;
    Ok(())
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
        };
        record(&conn, vid, "write", &counters, "raw log contents").unwrap();

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
        };
        record(&conn, vid, "verify", &counters, "raw").unwrap();

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
            vid,
            "verify",
            &HealthCounters {
                total_bytes_processed: 1,
                total_uncorrected: 0,
                total_corrected: 0,
                total_retries: 0,
                total_rewritten: 0,
                tape_alerts: 0,
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
}
