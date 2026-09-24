//! Native tape consumed per byte of data a `volume write` sent (issue #338,
//! ruled 2026-09-23, grilling Q1; the guard #323 asked for).
//!
//! An LTO drive that cannot get data fast enough — or gets it in bursts —
//! stops, backs up and restarts, and every stop-start burns tape. On the
//! real HP LTO-6 a bursty `openssl` pipe used **1.48** bytes of native
//! capacity per byte of data; tapectl's own steady write loop used
//! **1.000** (`docs/runs/2026-09-23-lto6-capacity-measurement.md`, runs 1
//! and 3) and a steady 155 MB/s fill used **1.0047**
//! (`docs/runs/2026-09-23-real-drive-rehearsal.md`, "The end-of-tape
//! fill"). The ratio is therefore a direct, after-the-fact reading of
//! whether the host fed the drive evenly — and it is computable from what
//! tapectl already journals, with no extra read of the drive.
//!
//! **Numerator: page 0x0c "Native capacity from BOP to EOD", not page
//! 0x17.** Page 0x17's "Total used native capacity [MB]" agrees with it on a
//! rewound tape, but is PARTIAL when read at EOD before a rewind — and the
//! write's post-command sweep runs wherever the store left the head, which
//! after a sealed write is at EOD. 0x0c's BOP→EOD is position-independent:
//! it describes the recorded extent of the tape, not where the head is.
//! Decimal MB (10^6), as sg_logs prints it (ADR-0012: cartridge capacities
//! are decimal).
//!
//! **Denominator: the write's whole Layout, block-padded**
//! ([`crate::volume::layout_model::Layout::on_tape_bytes`]) — every file
//! from the ID thunk to the seal marker, each rounded up to a whole block,
//! which is exactly what the fixed-block driver sends. Run 3 measured
//! 21,488,398,724 bytes as the process's `wchar` over all 14 volume files
//! against 21,487 MB of native capacity, and that pair computes to 1.000
//! here ([`tests::the_run_3_measurement_computes_to_one`]). NOT
//! `volumes.bytes_written`, which is slices only and would inflate the
//! ratio.
//!
//! **It warns; it never refuses and never gates.** The write is already
//! complete and sealed when this runs, and a ratio above the threshold
//! changes nothing about the volume — it tells the operator the host was
//! not quiet (`docs/operator-guide.md`, "A quiet host while the tape
//! runs") and that the next tape will hold less than planned if that
//! continues. The exit code is untouched.
//!
//! **Recorded as an `events` row, not a `health_logs` column.** ADR-0013 §3
//! rules that a derived figure does not become a column beside the facts
//! it derives from ("how the two disagree later"). Both facts are already
//! durable and queryable — the 0x0c decode in `log_page_journal` against
//! the write's contact, the layout on the write — and the ratio is what
//! tapectl SAID about them at the time, which is the audit trail's job.
//! The row is written whenever the ratio is computable, not only when it
//! warns, so run 3's 1.000 is as queryable as a 1.48, and it carries the
//! threshold it was judged against, so a later change to the constant does
//! not rewrite history. `report events` lists it with no change: the
//! action, field and value columns already show it.
//!
//! **Every consumer reads the journal** (migration 023): the 0x0c text is
//! read back from `log_page_journal` by contact id, never from the drive
//! and never from a second sweep. A drive that does not list page 0x0c, or
//! a decode without the line (the no-medium capture has none), makes the
//! ratio `None` — nothing recorded, nothing warned.
//!
//! **Recording and warning are split** ([`Suppression`]). mhvtl DOES list
//! page 0x0c and reports a BOP→EOD of 500 MB beside "0 GB written" — a
//! figure with no relation to any write, which against a 30 MB gate write
//! is a 16x ratio. On a drive with a `capacity_override` (ADR-0010: virtual
//! drives and the microcosm harnesses, nothing else — a drive that lies
//! about capacity cannot be read for a capacity ratio) the row is still
//! RECORDED, so the gate exercises the whole path and a reader can see the
//! 16x, but the warning is suppressed and the row's `details` says why
//! (`"suppressed": "capacity_override"`). A real drive has no override and
//! warns.

use rusqlite::{Connection, OptionalExtension};
use tracing::warn;

use crate::db::events;
use crate::error::Result;

/// Warn when the drive used more than this many bytes of native capacity
/// per byte of data sent.
///
/// The measurements it sits between, all on the HP LTO-6 `HUJ808A5L4`:
/// - bursty `openssl enc` pipe, 94.8 MB/s: **1.48**
///   (`docs/runs/2026-09-23-lto6-capacity-measurement.md`, run 1);
/// - `openssl` at 151 MB/s: **1.006** (same, run 2);
/// - tapectl `volume write`, steady 56 MB/s: **1.000** (same, run 3);
/// - steady 155.3 MB/s fill to ENOSPC: **1.0047**
///   (`docs/runs/2026-09-23-real-drive-rehearsal.md`, "The end-of-tape
///   fill").
///
/// 1.05 clears every steady feed by ten times its excess and is a third of
/// the way to nothing towards the one bursty feed measured. It is a
/// planning-margin figure, not a physical constant: 5% of a 2.5 TB tape is
/// 125 GB the plan assumed it had.
pub const FEED_RATIO_WARN_THRESHOLD: f64 = 1.05;

/// sg_logs prints page 0x0c's capacities in decimal megabytes (ADR-0012:
/// cartridge capacities are decimal, data sizes binary).
pub const NATIVE_MB_BYTES: u64 = 1_000_000;

/// The `events.action` the ratio is recorded under.
pub const EVENT_ACTION: &str = "write_feed_ratio";
/// The `events.field` — what `new_value` is a measurement of.
pub const EVENT_FIELD: &str = "native_per_data_byte";

/// The page 0x0c line this module reads, as sg_logs decodes it.
const BOP_TO_EOD_LINE: &str = "Native capacity from BOP to EOD:";

/// Parse "Native capacity from BOP to EOD: N MB" out of a page 0x0c decode.
///
/// `None` when the line is absent — a decode taken with no medium loaded
/// has no capacity lines at all (`hp_lto6_sg0_nomedia/page_0x0c`) — or when
/// the value is not in MB. The unit is checked, not assumed: a decoder that
/// printed GB here would otherwise be read as a thousand times too little
/// tape and never warn.
pub fn parse_native_bop_to_eod_mb(decoded_0x0c: &str) -> Option<u64> {
    decoded_0x0c.lines().find_map(|line| {
        let rest = line.trim().strip_prefix(BOP_TO_EOD_LINE)?;
        rest.trim().strip_suffix(" MB")?.trim().parse().ok()
    })
}

/// One write's tape-consumed-per-data-byte reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedRatio {
    /// Page 0x0c "Native capacity from BOP to EOD" after the write, decimal MB.
    pub native_bop_to_eod_mb: u64,
    /// The write's whole Layout, block-padded — every byte sent to the drive.
    pub data_bytes: u64,
}

impl FeedRatio {
    /// `None` when `data_bytes` is 0: there is no ratio to a write that sent
    /// nothing, and a division by zero is not a measurement.
    pub fn new(native_bop_to_eod_mb: u64, data_bytes: u64) -> Option<FeedRatio> {
        (data_bytes > 0).then_some(FeedRatio {
            native_bop_to_eod_mb,
            data_bytes,
        })
    }

    /// The numerator in bytes.
    pub fn native_bytes(&self) -> u64 {
        self.native_bop_to_eod_mb * NATIVE_MB_BYTES
    }

    /// Native bytes of tape per byte of data sent.
    pub fn ratio(&self) -> f64 {
        self.native_bytes() as f64 / self.data_bytes as f64
    }

    /// Above [`FEED_RATIO_WARN_THRESHOLD`].
    pub fn exceeds_threshold(&self) -> bool {
        self.ratio() > FEED_RATIO_WARN_THRESHOLD
    }

    /// The operator warning, one paragraph on stderr. Names both numbers,
    /// the ratio, the threshold and what it means; says the volume is fine.
    pub fn warning_text(&self, label: &str) -> String {
        format!(
            "warning: volume \"{label}\": the drive used {native} bytes of native tape \
             ({mb} MB, page 0x0c BOP to EOD) for {data} bytes of data sent -- {ratio:.3} \
             native bytes per data byte, above the {threshold:.2} threshold. The drive spent \
             tape repositioning behind an irregular feed; see issue #323 and the operator \
             guide's quiet-host rule. The volume is complete and correct; this is a warning \
             about the host, and the next tape will hold less than planned if it continues.",
            native = self.native_bytes(),
            mb = self.native_bop_to_eod_mb,
            data = self.data_bytes,
            ratio = self.ratio(),
            threshold = FEED_RATIO_WARN_THRESHOLD,
        )
    }
}

/// Why a ratio above the threshold draws no warning. The row is recorded
/// either way; this only gates the stderr line, and is written into the
/// row's `details` so a reader knows why a 16x drew nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suppression {
    /// The drive has a `capacity_override` — virtual drives (mhvtl) and the
    /// microcosm harnesses, and nothing else (ADR-0010). mhvtl's page 0x0c
    /// reports a static BOP→EOD unrelated to any write; a drive that lies
    /// about capacity cannot be read for a capacity ratio.
    CapacityOverride,
}

impl Suppression {
    /// The `details.suppressed` value.
    pub fn as_str(self) -> &'static str {
        match self {
            Suppression::CapacityOverride => "capacity_override",
        }
    }
}

/// One write's ratio together with the verdict on it: recorded always,
/// warned only when above the threshold AND not suppressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assessment {
    pub ratio: FeedRatio,
    pub suppressed: Option<Suppression>,
}

impl Assessment {
    /// Whether the operator warning is printed — and what `details.warned`
    /// records, so the row never claims a warning that did not appear.
    pub fn warns(&self) -> bool {
        self.ratio.exceeds_threshold() && self.suppressed.is_none()
    }

    /// The `events.details` JSON: both facts, the verdict, the threshold it
    /// was judged against, and — only when set — why a warning was
    /// suppressed. The key is ABSENT on an unsuppressed row, not null, so a
    /// query for `json_extract(details, '$.suppressed')` finds exactly the
    /// suppressed ones.
    pub fn details_json(&self, contact_id: i64) -> String {
        let mut details = serde_json::json!({
            "source_page": "0x0c",
            "native_bop_to_eod_mb": self.ratio.native_bop_to_eod_mb,
            "data_bytes": self.ratio.data_bytes,
            "ratio": self.ratio.ratio(),
            "threshold": FEED_RATIO_WARN_THRESHOLD,
            "warned": self.warns(),
            "contact_id": contact_id,
        });
        if let Some(s) = self.suppressed {
            details["suppressed"] = serde_json::Value::String(s.as_str().to_string());
        }
        details.to_string()
    }
}

/// The ratio from a page 0x0c decode (or its absence) and the write's bytes.
/// `None` when there is no decode, no BOP→EOD line in it, or nothing was
/// sent.
pub fn from_decoded_0x0c(decoded_0x0c: Option<&str>, data_bytes: u64) -> Option<FeedRatio> {
    let mb = parse_native_bop_to_eod_mb(decoded_0x0c?)?;
    FeedRatio::new(mb, data_bytes)
}

/// The decoded page 0x0c THIS contact's sweep journalled, if it read one.
///
/// One sweep per contact (migration 023) means at most one such row; the
/// newest is taken should a future change ever leave two. A failed read or
/// a failed decode is `None`: the row exists, but says nothing about
/// capacity.
pub fn journalled_0x0c_decode(conn: &Connection, contact_id: i64) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT decoded FROM log_page_journal
              WHERE contact_id = ?1 AND page_code = 0x0c AND subpage_code = 0
                AND ok = 1 AND decoded IS NOT NULL
              ORDER BY id DESC LIMIT 1",
            [contact_id],
            |r| r.get(0),
        )
        .optional()?)
}

/// Compute the write's feed ratio from the journal and record it as an
/// `events` row — the one production entry point, called by `volume write`
/// after its post-command sweep, only for a write that completed (an
/// aborted or interrupted write sent fewer bytes than its Layout, so the
/// denominator would be wrong).
///
/// `None` — nothing recorded, nothing to warn about — when the contact
/// could not be named (the sweep's rows carry a NULL contact id and cannot
/// be attributed), when the sweep journalled no usable page 0x0c, when the
/// decode has no BOP→EOD line, or when nothing was sent. `suppressed` does
/// NOT stop the recording — it is written into the row and gates only the
/// caller's warning ([`Assessment::warns`]). Best-effort throughout: a
/// refused INSERT warns in the log and the assessment is still returned,
/// so the operator warning does not depend on the bookkeeping.
pub fn assess_and_record(
    conn: &Connection,
    contact_id: Option<i64>,
    volume_id: i64,
    label: &str,
    data_bytes: u64,
    suppressed: Option<Suppression>,
) -> Option<Assessment> {
    let contact_id = contact_id?;
    let decoded = match journalled_0x0c_decode(conn, contact_id) {
        Ok(d) => d,
        Err(e) => {
            warn!(err = %e, contact_id, "log_page_journal read for page 0x0c failed");
            return None;
        }
    };
    let ratio = from_decoded_0x0c(decoded.as_deref(), data_bytes)?;
    let assessment = Assessment { ratio, suppressed };
    if let Err(e) = events::log_event(
        conn,
        "volume",
        volume_id,
        Some(label),
        EVENT_ACTION,
        Some(EVENT_FIELD),
        None,
        Some(&format!("{:.4}", ratio.ratio())),
        Some(&assessment.details_json(contact_id)),
        None,
    ) {
        warn!(err = %e, "events insert for the feed ratio failed");
    }
    Some(assessment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape::log_pages::{self, JournalRow};

    /// The real HP LTO-6 with the FUJIFILM cartridge loaded (2026-09-23):
    /// 23 MB recorded from BOP to EOD.
    const HP_LOADED_0X0C: &str = include_str!(
        "../../tests/fixtures/sg_logs/hp_lto6_sg0_fuji_ew7vwmvkf6/page_0x0c.decoded.txt"
    );
    /// The same drive with no medium: the page decodes, but carries no
    /// capacity lines at all.
    const HP_NOMEDIA_0X0C: &str =
        include_str!("../../tests/fixtures/sg_logs/hp_lto6_sg0_nomedia/page_0x0c.decoded.txt");
    /// The mhvtl drive: it DOES list 0x0c, and prints a BOP→EOD of 500 MB
    /// beside "0 GB written" — a figure with no relation to any write.
    const MHVTL_0X0C: &str =
        include_str!("../../tests/fixtures/sg_logs/mhvtl_td8_sg1/page_0x0c.decoded.txt");

    /// Run 3 of the capacity measurement
    /// (`docs/runs/2026-09-23-lto6-capacity-measurement.md`): `volume
    /// write`, steady 56 MB/s.
    const RUN_3_NATIVE_MB: u64 = 21_487;
    const RUN_3_DATA_BYTES: u64 = 21_488_398_724;

    /// A synthetic page 0x0c decode in the real drive's shape with the
    /// BOP→EOD figure substituted.
    fn synthetic_0x0c(bop_to_eod_mb: u64) -> String {
        HP_LOADED_0X0C
            .lines()
            .map(|l| {
                if l.trim().starts_with(BOP_TO_EOD_LINE) {
                    format!("  {BOP_TO_EOD_LINE} {bop_to_eod_mb} MB")
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ── The parser, against the real captures ──

    #[test]
    fn parses_bop_to_eod_from_the_real_drive_capture() {
        // Negative control: change the fixture's "23 MB" line and this fails.
        assert_eq!(parse_native_bop_to_eod_mb(HP_LOADED_0X0C), Some(23));
        assert!(
            HP_LOADED_0X0C.contains("Native capacity from BOP to EOD: 23 MB"),
            "positive control: the fixture carries the line being parsed"
        );
    }

    #[test]
    fn a_no_medium_decode_has_no_bop_to_eod_line() {
        // Negative control: a parser that defaulted a missing line to 0
        // would return Some(0) here and later compute a ratio of 0.
        assert!(
            !HP_NOMEDIA_0X0C.contains(BOP_TO_EOD_LINE),
            "positive control: the no-medium fixture really lacks the line"
        );
        assert_eq!(parse_native_bop_to_eod_mb(HP_NOMEDIA_0X0C), None);
    }

    #[test]
    fn the_mhvtl_capture_lists_0x0c_with_a_figure_unrelated_to_any_write() {
        // The brief for #338 assumed mhvtl has no page 0x0c. It does, and
        // its BOP→EOD is 500 MB beside "0 GB" written. This is WHY
        // `volume write` SUPPRESSES the warning on a drive with a
        // `capacity_override` (virtual drives only, ADR-0010) while still
        // recording the row: mhvtl's 500 MB against a 30 MB gate write is
        // 16x on every run — see `a_capacity_override_drive_is_recorded_
        // but_not_warned`. Negative control: if mhvtl ever reports a real
        // BOP→EOD, this assertion is what says the suppression can be
        // revisited.
        assert_eq!(parse_native_bop_to_eod_mb(MHVTL_0X0C), Some(500));
        assert!(MHVTL_0X0C.contains("Data bytes written to media by WRITE commands: 0 GB"));
    }

    #[test]
    fn a_value_not_in_megabytes_is_not_read() {
        // Negative control: drop the unit check and "23 GB" parses as 23 MB
        // — a thousandfold under-read that would never warn.
        assert_eq!(
            parse_native_bop_to_eod_mb("  Native capacity from BOP to EOD: 23 GB\n"),
            None
        );
        assert_eq!(
            parse_native_bop_to_eod_mb("  Native capacity from BOP to EOD: 23\n"),
            None
        );
        assert_eq!(
            parse_native_bop_to_eod_mb("Native capacity from BOP to EOD: 23 MB"),
            Some(23),
            "positive control: an unindented line still parses"
        );
    }

    // ── The ratio ──

    #[test]
    fn a_bursty_feed_at_1_48x_warns() {
        // Run 1's ratio, applied to a synthetic write: 1,000,000,000 data
        // bytes against 1,480 MB of tape. Negative control: raise
        // FEED_RATIO_WARN_THRESHOLD above 1.48 and this fails.
        let data_bytes = 1_000_000_000;
        let decoded = synthetic_0x0c(1_480);
        let r = from_decoded_0x0c(Some(&decoded), data_bytes).expect("computable");
        assert!((r.ratio() - 1.48).abs() < 1e-9, "ratio {}", r.ratio());
        assert!(r.exceeds_threshold(), "1.48 is above 1.05");
    }

    #[test]
    fn a_steady_feed_at_1_00x_does_not_warn() {
        // Negative control: lower FEED_RATIO_WARN_THRESHOLD to 1.0 or below
        // and this fails.
        let data_bytes = 1_000_000_000;
        let decoded = synthetic_0x0c(1_000);
        let r = from_decoded_0x0c(Some(&decoded), data_bytes).expect("computable");
        assert!((r.ratio() - 1.0).abs() < 1e-9);
        assert!(!r.exceeds_threshold(), "1.000 is not above 1.05");
    }

    #[test]
    fn the_rehearsal_fill_at_1_0047x_does_not_warn() {
        // The fourth #323 data point: 2,513,648 MB for 2,501,995,134,976
        // bytes (`docs/runs/2026-09-23-real-drive-rehearsal.md`). A steady
        // real fill must not warn, or the threshold is mis-set.
        let r = FeedRatio::new(2_513_648, 2_501_995_134_976).unwrap();
        assert_eq!(format!("{:.4}", r.ratio()), "1.0047");
        assert!(!r.exceeds_threshold());
    }

    #[test]
    fn page_absent_computes_nothing() {
        // A drive whose sweep read no 0x0c (or read it and it failed):
        // nothing to compute, so nothing to warn. Negative control: a
        // `from_decoded_0x0c` that defaulted the numerator would return
        // Some here.
        assert_eq!(from_decoded_0x0c(None, 1_000_000_000), None);
        // And a page that decoded without the line (no medium).
        assert_eq!(
            from_decoded_0x0c(Some(HP_NOMEDIA_0X0C), 1_000_000_000),
            None
        );
    }

    #[test]
    fn a_write_that_sent_nothing_has_no_ratio() {
        // Negative control: remove the `data_bytes > 0` guard and this is a
        // division by zero (inf), which `exceeds_threshold` would warn on.
        assert_eq!(FeedRatio::new(23, 0), None);
        assert_eq!(from_decoded_0x0c(Some(HP_LOADED_0X0C), 0), None);
    }

    #[test]
    fn the_run_3_measurement_computes_to_one() {
        // The acceptance criterion on issue #338: run 3's journalled pair
        // — 21,487 MB native for 21,488,398,724 bytes sent — computes to
        // 1.000. The exact value is 0.99993; the record prints it to three
        // places, so that is what is pinned. Negative control: change
        // NATIVE_MB_BYTES to 2^20 and this reads 1.049.
        let r = FeedRatio::new(RUN_3_NATIVE_MB, RUN_3_DATA_BYTES).unwrap();
        assert_eq!(format!("{:.3}", r.ratio()), "1.000");
        assert!(!r.exceeds_threshold());
        // And through the parser, from a decode in the drive's own shape.
        let decoded = synthetic_0x0c(RUN_3_NATIVE_MB);
        let via_parser = from_decoded_0x0c(Some(&decoded), RUN_3_DATA_BYTES).unwrap();
        assert_eq!(via_parser, r);
    }

    #[test]
    fn the_warning_names_both_numbers_the_ratio_and_the_cause() {
        let r = FeedRatio::new(1_480, 1_000_000_000).unwrap();
        let text = r.warning_text("V1");
        // Pinned verbatim: this is the operator-facing text. Negative
        // control: any rewording fails here, deliberately.
        assert_eq!(
            text,
            "warning: volume \"V1\": the drive used 1480000000 bytes of native tape (1480 MB, \
             page 0x0c BOP to EOD) for 1000000000 bytes of data sent -- 1.480 native bytes \
             per data byte, above the 1.05 threshold. The drive spent tape repositioning \
             behind an irregular feed; see issue #323 and the operator guide's quiet-host \
             rule. The volume is complete and correct; this is a warning about the host, and \
             the next tape will hold less than planned if it continues."
        );
        assert!(
            text.starts_with("warning: "),
            "it is a warning, not an error"
        );
    }

    // ── The journal read and the events row ──

    fn contact(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO cartridge_contacts (operation, device) VALUES ('volume write', '/dev/null')",
            [],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn volume(conn: &Connection, label: &str) -> i64 {
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
            [label],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn journal_0x0c(conn: &Connection, contact_id: i64, decoded: Option<&str>, ok: bool) {
        let row = JournalRow {
            captured_at: "2026-09-23 14:50:00".into(),
            contact_id: Some(contact_id),
            device_sg: "/dev/sg0".into(),
            device_tape: Some("/dev/nst0".into()),
            trigger: "volume write".into(),
            page_code: 0x0c,
            subpage_code: 0,
            ok,
            error: None,
            tool_argv: "[\"sg_logs\"]".into(),
            tool_version: None,
            raw: rusqlite::types::Value::Blob(vec![0x0c, 0, 0, 0]),
            decoded: decoded.map(str::to_string),
            tapectl_version: crate::build_info::VERSION,
        };
        log_pages::insert(conn, &row).unwrap();
    }

    fn events_for(
        conn: &Connection,
        volume_id: i64,
    ) -> Vec<(String, Option<String>, Option<String>)> {
        let mut stmt = conn
            .prepare(
                "SELECT field, new_value, details FROM events
                  WHERE entity_type = 'volume' AND entity_id = ?1 AND action = ?2",
            )
            .unwrap();
        stmt.query_map(rusqlite::params![volume_id, EVENT_ACTION], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
    }

    #[test]
    fn assess_reads_this_contacts_0x0c_from_the_journal_and_records_an_event() {
        let conn = crate::db::open_memory().unwrap();
        let vid = volume(&conn, "V1");
        let cid = contact(&conn);
        // ANOTHER contact's 0x0c at 1.00x, journalled first: the read must
        // be by contact id, not "the latest 0x0c row". Negative control:
        // drop `contact_id = ?1` from the query and, with ORDER BY id DESC,
        // this test still passes — so the other contact's row is inserted
        // SECOND below as well.
        let other = contact(&conn);
        journal_0x0c(&conn, other, Some(&synthetic_0x0c(1_000)), true);
        journal_0x0c(&conn, cid, Some(&synthetic_0x0c(1_480)), true);
        journal_0x0c(&conn, other, Some(&synthetic_0x0c(1_000)), true);

        let a = assess_and_record(&conn, Some(cid), vid, "V1", 1_000_000_000, None)
            .expect("a journalled 0x0c with the line is computable");
        assert_eq!(a.ratio.native_bop_to_eod_mb, 1_480);
        assert!(a.ratio.exceeds_threshold());
        assert!(a.warns(), "a real drive (no suppression) at 1.48 warns");

        let rows = events_for(&conn, vid);
        assert_eq!(rows.len(), 1, "one events row per assessment");
        let (field, new_value, details) = &rows[0];
        assert_eq!(field.as_str(), EVENT_FIELD);
        assert_eq!(new_value.as_deref(), Some("1.4800"));
        let details: serde_json::Value = serde_json::from_str(details.as_deref().unwrap()).unwrap();
        assert_eq!(details["source_page"], "0x0c");
        assert_eq!(details["native_bop_to_eod_mb"], 1_480);
        assert_eq!(details["data_bytes"], 1_000_000_000u64);
        assert_eq!(details["threshold"], FEED_RATIO_WARN_THRESHOLD);
        assert_eq!(details["warned"], true);
        assert_eq!(details["contact_id"], cid);
        assert!(
            details.get("suppressed").is_none(),
            "an unsuppressed row has NO `suppressed` key, not a null one"
        );
    }

    #[test]
    fn a_steady_write_is_recorded_too_and_not_flagged() {
        // The row is written whenever computable, so run 3's 1.000 is
        // queryable. Negative control: an `assess_and_record` that only
        // wrote on a warning leaves `events` empty here.
        let conn = crate::db::open_memory().unwrap();
        let vid = volume(&conn, "M323A1");
        let cid = contact(&conn);
        journal_0x0c(&conn, cid, Some(&synthetic_0x0c(RUN_3_NATIVE_MB)), true);

        let a = assess_and_record(&conn, Some(cid), vid, "M323A1", RUN_3_DATA_BYTES, None).unwrap();
        assert!(!a.ratio.exceeds_threshold());
        assert!(!a.warns());
        let rows = events_for(&conn, vid);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].1.as_deref(),
            Some("0.9999"),
            "four places, unrounded"
        );
        let details: serde_json::Value =
            serde_json::from_str(rows[0].2.as_deref().unwrap()).unwrap();
        assert_eq!(details["warned"], false);
        assert!(details.get("suppressed").is_none());
    }

    #[test]
    fn a_capacity_override_drive_is_recorded_but_not_warned() {
        // The mhvtl shape, end to end: its static 500 MB against a 30 MB
        // gate write is 16.67x. On a drive with a `capacity_override` the
        // row IS recorded — so the gate exercises the whole path and the
        // 16x is visible to a reader — but no warning is drawn, and the row
        // says why. Negative controls: fold the suppression back into a
        // skip and `events` is empty here; drop `suppressed` from
        // `warns()` and `warned` reads true; write `"suppressed": null`
        // instead of omitting the key and the unsuppressed tests fail.
        let conn = crate::db::open_memory().unwrap();
        let vid = volume(&conn, "GATE1");
        let cid = contact(&conn);
        journal_0x0c(&conn, cid, Some(MHVTL_0X0C), true);

        let data_bytes = 30_000_000;
        let a = assess_and_record(
            &conn,
            Some(cid),
            vid,
            "GATE1",
            data_bytes,
            Some(Suppression::CapacityOverride),
        )
        .expect("recorded even though suppressed");
        assert_eq!(a.ratio.native_bop_to_eod_mb, 500);
        assert!(
            a.ratio.exceeds_threshold(),
            "positive control: 16x IS above 1.05"
        );
        assert!(!a.warns(), "but a suppressed assessment does not warn");
        assert_eq!(a.suppressed, Some(Suppression::CapacityOverride));

        let rows = events_for(&conn, vid);
        assert_eq!(rows.len(), 1, "recorded, not skipped");
        assert_eq!(rows[0].1.as_deref(), Some("16.6667"));
        let details: serde_json::Value =
            serde_json::from_str(rows[0].2.as_deref().unwrap()).unwrap();
        assert_eq!(details["source_page"], "0x0c");
        assert_eq!(details["native_bop_to_eod_mb"], 500);
        assert_eq!(details["data_bytes"], data_bytes);
        assert_eq!(
            details["warned"], false,
            "the row never claims a warning that did not appear"
        );
        assert_eq!(details["suppressed"], "capacity_override");
    }

    #[test]
    fn a_real_drive_shaped_write_at_1_48x_warns_and_records_it() {
        // The other half of the split: no override (a real drive), 1.48x
        // → warns, and the row says warned=true with no `suppressed`.
        // Negative control: suppress unconditionally and `warns()` is
        // false here.
        let conn = crate::db::open_memory().unwrap();
        let vid = volume(&conn, "REAL1");
        let cid = contact(&conn);
        journal_0x0c(&conn, cid, Some(&synthetic_0x0c(1_480)), true);
        let a = assess_and_record(&conn, Some(cid), vid, "REAL1", 1_000_000_000, None).unwrap();
        assert!(a.warns());
        assert_eq!(a.suppressed, None);
        assert!(a
            .ratio
            .warning_text("REAL1")
            .starts_with("warning: volume \"REAL1\""));
        let details: serde_json::Value =
            serde_json::from_str(events_for(&conn, vid)[0].2.as_deref().unwrap()).unwrap();
        assert_eq!(details["warned"], true);
        assert!(details.get("suppressed").is_none());
    }

    #[test]
    fn no_journalled_0x0c_records_nothing() {
        // The mhvtl-shaped case the brief describes (a drive that lists no
        // 0x0c) and the failed-read case: no row, no ratio, no event.
        // Negative control: a query without `ok = 1 AND decoded IS NOT
        // NULL` would compute from the failed read's NULL and panic, or a
        // parser defaulting the numerator would record a 0.
        let conn = crate::db::open_memory().unwrap();
        let vid = volume(&conn, "V1");
        let cid = contact(&conn);
        assert_eq!(
            assess_and_record(&conn, Some(cid), vid, "V1", 1_000_000_000, None),
            None
        );
        assert!(
            events_for(&conn, vid).is_empty(),
            "nothing recorded without a page"
        );

        // A 0x0c row whose read failed.
        journal_0x0c(&conn, cid, None, false);
        assert_eq!(
            assess_and_record(&conn, Some(cid), vid, "V1", 1_000_000_000, None),
            None
        );
        assert!(events_for(&conn, vid).is_empty());

        // A 0x0c that decoded without the line (no medium).
        journal_0x0c(&conn, cid, Some(HP_NOMEDIA_0X0C), true);
        assert_eq!(
            assess_and_record(&conn, Some(cid), vid, "V1", 1_000_000_000, None),
            None
        );
        assert!(events_for(&conn, vid).is_empty());

        // Positive control: the same contact with a real line IS computed.
        journal_0x0c(&conn, cid, Some(HP_LOADED_0X0C), true);
        assert!(assess_and_record(&conn, Some(cid), vid, "V1", 23_000_000, None).is_some());
        assert_eq!(events_for(&conn, vid).len(), 1);
    }

    #[test]
    fn an_unnamed_contact_cannot_be_assessed() {
        // The sweep's rows carry a NULL contact id when the contact INSERT
        // failed; they cannot be attributed to this write, so nothing is
        // computed. Negative control: an `assess_and_record` that fell back
        // to "the latest 0x0c row" would find this one.
        let conn = crate::db::open_memory().unwrap();
        let vid = volume(&conn, "V1");
        let row = JournalRow {
            captured_at: "2026-09-23 14:50:00".into(),
            contact_id: None,
            device_sg: "/dev/sg0".into(),
            device_tape: None,
            trigger: "volume write".into(),
            page_code: 0x0c,
            subpage_code: 0,
            ok: true,
            error: None,
            tool_argv: "[\"sg_logs\"]".into(),
            tool_version: None,
            raw: rusqlite::types::Value::Blob(vec![0x0c, 0, 0, 0]),
            decoded: Some(synthetic_0x0c(1_480)),
            tapectl_version: crate::build_info::VERSION,
        };
        log_pages::insert(&conn, &row).unwrap();
        assert_eq!(
            assess_and_record(&conn, None, vid, "V1", 1_000_000_000, None),
            None
        );
        assert!(events_for(&conn, vid).is_empty());
    }
}
