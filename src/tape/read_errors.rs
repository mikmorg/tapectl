//! Corrected read errors per verify, and their trend per cartridge (issue
//! #421, ADR-0012 2026-10-06 item 12: "No `scrub` command: `volume verify`
//! records the growth of corrected read errors and `audit` flags a
//! cartridge whose rate climbs").
//!
//! A tape whose drive ECC works harder each year still passes every sha256
//! until the day it does not. Page 0x03 (the read error counter page) is
//! where that work shows: errors corrected with and without delay, rereads,
//! uncorrected errors, and how much the drive read to produce them. One
//! verify's figures mean little alone; the same cartridge's figures verify
//! over verify are the signal.
//!
//! **What is recorded.** After each completed full verify, page 0x03's
//! counters across that verify, each per GiB the verify read, go into one
//! `events` row ([`EVENT_ACTION`]) beside the feed ratio's and for the same
//! reason (ADR-0013 §3: a derived figure is what tapectl said, recorded as
//! an event, never a column beside the facts it comes from; the facts stay
//! in `log_page_journal`).
//!
//! **The denominator is the verify's own bytes** (issue #495). Page 0x03's
//! parameter 5, which sg_logs labels "Total bytes processed", is on the HP
//! LTO-6 "Total data sets processed": a count of data sets read (HP-V3 p64;
//! the no-medium capture reads 296 of them beside 69 corrections). From
//! 1.1.0 (#421) to 1.7.0 every rate divided by it as if it were bytes,
//! inflating each figure by the data-set size, about 2.4 MB, and #467's
//! scope check (1.1.2) weighed it against the verify's bytes and could
//! never fire. Each rate now divides by
//! the bytes the verify's readback read: the volume's Layout as the tape's
//! own front index lists it, block-padded (`volume::write`'s verify, which
//! passes it in), as `feed_ratio`'s corrections divide by the write's own
//! bytes. Parameter 5 only proves the page counted a read: a verify whose
//! page counted no data set (mhvtl reports 0) has no rate.
//!
//! **The counts are the verify's own.** HP clears pages 0x02 and 0x03 when
//! the tape is unloaded (HP-V3 p63-64), so a reading taken after a verify
//! covers every read since the load: the verify's, and any earlier read in
//! the same load (a write's confirm, a restore). ADR-0013 reads each page
//! once per contact, after the command, so the verify's counts are its
//! reading less the previous contact's in the same load, from the journal
//! ([`crate::tape::same_load`], the one rule `feed_ratio` takes page 0x02
//! by), or less zero when the verify is the first contact of its load on an
//! HP drive. That scope is exact ([`Scope::SameLoad`]).
//!
//! **With no such baseline the scope is judged** (issue #467; ADR-0012
//! 2026-10-07 item 6). The whole reading is then the count, and it is this
//! verify's only if the page counted no more than [`SCOPE_FACTOR`] times
//! what the verify read. The page counts data sets, so its count times the
//! medium's data-set size ([`data_set_bytes`]) is weighed against the
//! verify's bytes: within, the point is [`Scope::Estimated`] and judged;
//! beyond, or with no honest data-set size to weigh by (another vendor's
//! drive, a generation not measured), it is [`Scope::Unknown`]: shown,
//! marked, and never judged ([`CartridgeTrend::rising`] skips it).
//!
//! **What is flagged.** No health formula (ADR-0012 items 11-12: thresholds
//! come from home2's data). One provisional, configurable factor
//! (`[health] read_error_rise_factor`, default [`DEFAULT_RISE_FACTOR`]): a
//! cartridge whose newest verify corrected more errors per GiB than the
//! factor times its previous verify's — and more than [`RISE_FLOOR_PER_GIB`]
//! per GiB the verify read (ADR-0012 2026-10-07 item 7) — is flagged by
//! `audit` and `report health`, with the remedy — copy it to a fresh
//! cartridge.
//!
//! **Points recorded before #495** carry their rates in the wrong unit. They
//! stay exactly as recorded (an event is what tapectl said then), are shown
//! marked, and are never compared ([`TrendPoint::recorded_before_fix`]):
//! recomputing them would need, for each, its contact's journalled page and
//! the verify's bytes, and a point from before 1.1.2 has no byte count at
//! all. Their raw pages stay in `log_page_journal` for anyone who wants to.

use rusqlite::{Connection, OptionalExtension};
use tracing::warn;

use crate::db::events;
use crate::error::Result;
use crate::policy::coverage;
use crate::store::Tier;
use crate::tape::health::parse_sg_logs_page;
use crate::tape::same_load::{self, LoadBaseline};

/// The `events.action` one verify's read-error figures are recorded under.
pub const EVENT_ACTION: &str = "verify_read_errors";
/// Its `events.field`: `new_value` is the corrected errors per GiB read.
pub const EVENT_FIELD: &str = "corrected_per_gib";

/// The `details` key a point recorded under #495's rule carries, with the
/// value [`RATE_DENOMINATOR`]: its rates divide by the verify's own bytes.
/// A point without it was recorded before the fix, its rates over page
/// 0x03's data-set count ([`TrendPoint::recorded_before_fix`]).
pub const RATE_DENOMINATOR_KEY: &str = "rate_denominator";
/// [`RATE_DENOMINATOR_KEY`]'s value: `verify_bytes`, the details key the
/// denominator is recorded under.
pub const RATE_DENOMINATOR: &str = "verify_bytes";

/// The provisional rise factor (issue #421): flag a cartridge whose
/// corrected-error rate more than doubled since its previous verify. A
/// starting point to be replaced from home2's recorded verifies, not a
/// measured threshold.
pub const DEFAULT_RISE_FACTOR: f64 = 2.0;

/// The absolute floor under a rise (ADR-0012 2026-10-07 item 7): a
/// cartridge is flagged as rising only when its newest rate is also above
/// this many corrected errors per GiB the verify read. Without it, any
/// first non-zero reading after a zero one is a rise past every factor;
/// with it, a cartridge correcting under one error per GiB is not an alarm
/// whatever its previous verify read. Fixed, not a config key: the factor
/// is the provisional, tunable half. Judged in real GiB since #495 (before
/// it, a GiB here was 2^30 data sets).
pub const RISE_FLOOR_PER_GIB: f64 = 1.0;

/// Cartridge statuses whose trend is no longer reported: the operator has
/// retired it, or its last live volume is gone and it waits to be erased.
/// Neither will be verified again, so a rise on it could never clear.
pub const OUT_OF_SERVICE_CARTRIDGE: &[&str] = &["retired_permanent", "pending_erase"];

const GIB: f64 = (1u64 << 30) as f64;

/// A page 0x03 whose data sets, at the medium's data-set size, come to more
/// than this many times the bytes the verify read is of unknown scope (issue
/// #467): it counted reads the verify did not make. Twice, because the
/// data-set figure is an estimate — a flush pads a partial data set, and
/// the drive reads ahead of the last file the verify asked for — so a page
/// that counted this verify alone stays inside it, and one that also counted
/// a readback as large (a full confirm earlier in the same load) does not.
pub const SCOPE_FACTOR: i64 = 2;

/// Bytes of data in one data set on an LTO-6 medium, as the HP LTO-6
/// counted them: page 0x17's "Total data sets written" rose by 8,698 and
/// 8,695 for 21,474,836,480 bytes in the 2026-09-23 runs 1 and 2, 2,468,940
/// and 2,469,791 bytes a data set (`docs/research/2026-10-20-write-shaping.md`
/// §2.2, OBSERVED on `HUJ808A5L4` and the cartridge `EW7VWMVKF6`). Run 1's
/// figure; run 2's differs by 0.03%. Measured on writes: no capture yet
/// reconciles page 0x03's count with a known read (the no-medium capture's
/// 296 against page 0x17's 18,050 MB read is about 61 MB a count), so on
/// the read side this weighs by HP-V3's text, not a measurement.
pub const LTO6_DATA_SET_BYTES: u64 = 2_468_940;

/// The data-set size a reading's parameter 5 is weighed by, when there is an
/// honest one: an HP drive (whose parameter 5 counts data sets, HP-V3 p64)
/// reading an LTO-6 medium (the one generation measured,
/// [`LTO6_DATA_SET_BYTES`]). `None` for every other drive or generation:
/// another vendor's parameter 5 has no statement of its unit behind it.
pub fn data_set_bytes(vendor: Option<&str>, medium_generation: Option<&str>) -> Option<u64> {
    let lto6 = medium_generation.and_then(crate::media::Generation::parse)
        == Some(crate::media::Generation::Lto6);
    (crate::tape::log_pages::is_hp_vendor(vendor) && lto6).then_some(LTO6_DATA_SET_BYTES)
}

/// Whether a page that counted `data_sets` counted far more than the verify
/// read (`verify_bytes`), at `data_set_bytes` a data set — or cannot be
/// told: with no verify byte count or no data-set size there is no honest
/// figure, and that is unknown too.
pub fn scope_unknown(
    data_sets: i64,
    verify_bytes: Option<u64>,
    data_set_bytes: Option<u64>,
) -> bool {
    match (verify_bytes, data_set_bytes) {
        (Some(v), Some(size)) => {
            data_sets.max(0) as f64 * size as f64 > SCOPE_FACTOR as f64 * v as f64
        }
        _ => true,
    }
}

/// Page 0x03's counters, as one sweep read them — or their difference
/// across a verify ([`ReadErrors::less`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReadErrors {
    pub corrected_no_delay: i64,
    pub corrected_with_delay: i64,
    /// `Total errors corrected` — 0 on the HP LTO-6 even while the two
    /// above count (issue #120), so [`ReadErrors::corrected`] takes the
    /// larger of it and their sum.
    pub total_corrected: i64,
    pub rereads: i64,
    pub uncorrected: i64,
    /// Parameter 5, sg_logs's "Total bytes processed"; HP's "Total data sets
    /// processed" (HP-V3 p64). A COUNT OF DATA SETS on the HP drive, not
    /// bytes (issue #495), so it is never a denominator: it only proves the
    /// page counted a read, and weighs the scope when there is no baseline.
    pub data_sets: i64,
}

impl ReadErrors {
    /// Parse a page 0x03 decode as sg_logs prints it.
    pub fn from_decoded_0x03(text: &str) -> ReadErrors {
        let p = parse_sg_logs_page(0x03, text);
        ReadErrors {
            corrected_no_delay: p.corrected_no_delay,
            corrected_with_delay: p.corrected_with_delay,
            total_corrected: p.total_corrected,
            rereads: p.total_retries,
            uncorrected: p.total_uncorrected,
            data_sets: p.total_bytes_processed,
        }
    }

    /// Errors the drive corrected: `Total errors corrected`, or the sum of
    /// the with- and without-delay counters where the drive leaves the
    /// total at 0.
    pub fn corrected(&self) -> i64 {
        self.total_corrected
            .max(self.corrected_no_delay + self.corrected_with_delay)
    }

    /// This reading less an earlier one in the same load. `None` when any
    /// counter went DOWN: the page was cleared between the two readings, so
    /// the difference is not what happened between them.
    pub fn less(&self, start: &ReadErrors) -> Option<ReadErrors> {
        let d = ReadErrors {
            corrected_no_delay: self.corrected_no_delay - start.corrected_no_delay,
            corrected_with_delay: self.corrected_with_delay - start.corrected_with_delay,
            total_corrected: self.total_corrected - start.total_corrected,
            rereads: self.rereads - start.rereads,
            uncorrected: self.uncorrected - start.uncorrected,
            data_sets: self.data_sets - start.data_sets,
        };
        [
            d.corrected_no_delay,
            d.corrected_with_delay,
            d.total_corrected,
            d.rereads,
            d.uncorrected,
            d.data_sets,
        ]
        .iter()
        .all(|n| *n >= 0)
        .then_some(d)
    }
}

/// How far a verify's counts are its own (issue #495; #467's question).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The reading less the previous contact's in the same load, or less
    /// zero as the first contact of its load on an HP drive
    /// (`baseline_contact_id` `None`): exact.
    SameLoad { baseline_contact_id: Option<i64> },
    /// No baseline; the whole reading, whose data sets come to no more than
    /// [`SCOPE_FACTOR`] times the verify's bytes. Judged.
    Estimated,
    /// No baseline, and the page counted more than that, or there is no
    /// honest figure to weigh it by. Shown, marked, never judged.
    Unknown,
}

impl Scope {
    /// The `details.scope` value.
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::SameLoad { .. } => "same_load",
            Scope::Estimated => "estimated",
            Scope::Unknown => "unknown",
        }
    }
}

/// One verify's read-error figures: page 0x03's counts across the verify,
/// the bytes it read, and how far the counts are its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyReadErrors {
    /// Across the verify ([`Scope::SameLoad`]), or the whole reading when
    /// there was no baseline.
    pub counts: ReadErrors,
    /// The bytes the verify's readback read, the rates' denominator; `None`
    /// when the caller does not know them (a readback continued from an
    /// interrupted one read only part of the tape in this contact).
    pub verify_bytes: Option<u64>,
    pub scope: Scope,
    /// The data-set size the scope was weighed by, when it was weighed.
    pub data_set_bytes: Option<u64>,
}

impl VerifyReadErrors {
    /// GiB the verify read.
    pub fn gib_read(&self) -> Option<f64> {
        self.verify_bytes.filter(|b| *b > 0).map(|b| b as f64 / GIB)
    }

    /// `count` per GiB the verify read; `None` when the bytes are not known,
    /// or when the page counted no data set across the verify (mhvtl reports
    /// 0, and a page that did not count the read has no rate to give).
    pub fn per_gib(&self, count: i64) -> Option<f64> {
        let gib = self.gib_read()?;
        (self.counts.data_sets > 0).then(|| count as f64 / gib)
    }

    /// Corrected errors per GiB read — the trended figure.
    pub fn corrected_per_gib(&self) -> Option<f64> {
        self.per_gib(self.counts.corrected())
    }

    /// Whether the point is left out of every comparison.
    pub fn scope_unknown(&self) -> bool {
        self.scope == Scope::Unknown
    }
}

/// The page 0x03 a contact's sweep journalled, parsed.
fn journalled_0x03(conn: &Connection, contact_id: i64) -> Result<Option<ReadErrors>> {
    Ok(same_load::journalled_decode(conn, contact_id, 0x03)?
        .as_deref()
        .map(ReadErrors::from_decoded_0x03))
}

/// The verify's figures from the journal: `contact_id`'s page 0x03 less its
/// same-load baseline ([`same_load::same_load_baseline`]), over
/// `verify_bytes`. With no baseline — none known, the previous contact in
/// the load journalled no page 0x03, or a counter went down between the two
/// — the whole reading, its scope weighed by the drive's vendor and the
/// volume's medium generation ([`data_set_bytes`], [`scope_unknown`]).
/// `None` when the contact's sweep journalled no page 0x03.
pub fn across_verify(
    conn: &Connection,
    contact_id: i64,
    volume_id: i64,
    verify_bytes: Option<u64>,
) -> Result<Option<VerifyReadErrors>> {
    let Some(end) = journalled_0x03(conn, contact_id)? else {
        return Ok(None);
    };
    let start = match same_load::same_load_baseline(conn, contact_id)? {
        LoadBaseline::LoadZero => Some((None, ReadErrors::default())),
        LoadBaseline::Contact(prev) => journalled_0x03(conn, prev)?.map(|r| (Some(prev), r)),
        LoadBaseline::Unknown => None,
    };
    if let Some((baseline_contact_id, counts)) =
        start.and_then(|(id, start)| end.less(&start).map(|d| (id, d)))
    {
        return Ok(Some(VerifyReadErrors {
            counts,
            verify_bytes,
            scope: Scope::SameLoad {
                baseline_contact_id,
            },
            data_set_bytes: None,
        }));
    }
    let (vendor, generation): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT d.vendor, (SELECT media_type FROM volumes WHERE id = ?2)
               FROM cartridge_contacts c LEFT JOIN drives d ON d.id = c.drive_id
              WHERE c.id = ?1",
            rusqlite::params![contact_id, volume_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .unwrap_or((None, None));
    let size = data_set_bytes(vendor.as_deref(), generation.as_deref());
    let scope = if scope_unknown(end.data_sets, verify_bytes, size) {
        Scope::Unknown
    } else {
        Scope::Estimated
    };
    Ok(Some(VerifyReadErrors {
        counts: end,
        verify_bytes,
        scope,
        data_set_bytes: size,
    }))
}

/// Record one completed verify's read-error figures ([`across_verify`]):
/// page 0x03's counts across it, each per GiB of `verify_bytes`, as an
/// `events` row naming the verification session, the contact, and the
/// cartridge and drive the contact names. Called by `volume verify` after
/// its post-command sweep, for a FULL verify that made a session: a
/// `--quick` verify reads a few MB, and a rate over that is no sample of the
/// medium (the review of #421). `tier` goes into the details by the name
/// `verification_sessions.verify_type` gives it, and [`trends`] reads only
/// full verifies whatever was recorded.
///
/// `verify_bytes` is what the verify's readback read (issue #495), from the
/// caller, which read the front index; `None` records the counts and no
/// rate.
///
/// `None` — nothing recorded — when the contact could not be named or its
/// sweep journalled no page 0x03 (a drive that does not list it, a failed
/// read, no backend configured). Best-effort: a refused INSERT is a log
/// warning, never a failed verify.
pub fn record_for_verify(
    conn: &Connection,
    contact_id: Option<i64>,
    session_id: i64,
    volume_id: i64,
    label: &str,
    tier: Tier,
    verify_bytes: Option<u64>,
) -> Option<VerifyReadErrors> {
    let contact_id = contact_id?;
    let figures = match across_verify(conn, contact_id, volume_id, verify_bytes) {
        Ok(f) => f?,
        Err(e) => {
            warn!(err = %e, contact_id, "log_page_journal read for page 0x03 failed");
            return None;
        }
    };
    let (cartridge_id, drive_serial): (Option<i64>, Option<String>) = conn
        .query_row(
            "SELECT c.cartridge_id, d.serial
               FROM cartridge_contacts c LEFT JOIN drives d ON d.id = c.drive_id
              WHERE c.id = ?1",
            [contact_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .ok()
        .flatten()
        .unwrap_or((None, None));
    let c = &figures.counts;
    let mut details = serde_json::json!({
        "session_id": session_id,
        "tier": match tier {
            Tier::Integrity => "full",
            Tier::Navigable => "quick",
        },
        "contact_id": contact_id,
        "cartridge_id": cartridge_id,
        "drive_serial": drive_serial,
        "verify_bytes": figures.verify_bytes,
        "scope": figures.scope.as_str(),
        "scope_unknown": figures.scope_unknown(),
        "data_sets": c.data_sets,
        "corrected": c.corrected(),
        "corrected_no_delay": c.corrected_no_delay,
        "corrected_with_delay": c.corrected_with_delay,
        "total_corrected": c.total_corrected,
        "rereads": c.rereads,
        "uncorrected": c.uncorrected,
        "corrected_per_gib": figures.corrected_per_gib(),
        "rereads_per_gib": figures.per_gib(c.rereads),
        "uncorrected_per_gib": figures.per_gib(c.uncorrected),
    });
    details[RATE_DENOMINATOR_KEY] = RATE_DENOMINATOR.into();
    if let Scope::SameLoad {
        baseline_contact_id,
    } = figures.scope
    {
        details["baseline_contact_id"] = baseline_contact_id.into();
    }
    if let Some(size) = figures.data_set_bytes {
        details["data_set_bytes"] = size.into();
    }
    if let Err(e) = events::log_event(
        conn,
        "volume",
        volume_id,
        Some(label),
        EVENT_ACTION,
        Some(EVENT_FIELD),
        None,
        figures
            .corrected_per_gib()
            .map(|r| format!("{r:.6}"))
            .as_deref(),
        Some(&details.to_string()),
        None,
    ) {
        warn!(err = %e, "events insert for the verify's read errors failed");
    }
    Some(figures)
}

/// One verify's recorded figures, for a trend.
#[derive(Debug, Clone, PartialEq)]
pub struct TrendPoint {
    /// When the verify's figures were recorded.
    pub at: String,
    pub volume: String,
    pub drive_serial: Option<String>,
    /// GiB the verify read (`details.verify_bytes`); `None` when the point
    /// recorded no byte count (a continued readback, or a point from before
    /// 1.1.2).
    pub gib_read: Option<f64>,
    pub corrected: i64,
    pub uncorrected: i64,
    pub corrected_per_gib: Option<f64>,
    /// `details.scope` as recorded: `same_load`, `estimated` or `unknown`
    /// ([`Scope`]); `None` on a point recorded before #495.
    pub scope: Option<String>,
    /// The page may have counted reads the verify did not make
    /// ([`Scope::Unknown`]): the rate is shown, marked, and never judged.
    pub scope_unknown: bool,
    /// Recorded before #495, its rate over page 0x03's data-set count as if
    /// it were bytes: shown marked, and never judged.
    pub recorded_before_fix: bool,
}

impl TrendPoint {
    /// Whether [`CartridgeTrend::rising`] may compare this point.
    fn judged(&self) -> bool {
        !self.scope_unknown && !self.recorded_before_fix
    }
}

/// One cartridge's verifies, oldest first.
#[derive(Debug, Clone, PartialEq)]
pub struct CartridgeTrend {
    /// The cartridge's barcode, or `volume:<label>` for a verify whose
    /// contact identified no cartridge.
    pub cartridge: String,
    pub points: Vec<TrendPoint>,
}

/// A trend's rise past the factor: the previous and newest rates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rise {
    pub previous: f64,
    pub newest: f64,
}

impl CartridgeTrend {
    /// The two newest verifies that have a rate, when the newer one's is
    /// more than `factor` times the older one's **and** above
    /// [`RISE_FLOOR_PER_GIB`] (ADR-0012 2026-10-07 item 7). A previous rate
    /// of 0 rising to anything above 0 is a rise past any factor, so the
    /// floor is what keeps a first non-zero reading after zero from being an
    /// alarm.
    ///
    /// Points of unknown scope ([`TrendPoint::scope_unknown`]) are skipped:
    /// their page may have counted reads the verify did not make, so a rate
    /// over them is no sample of this cartridge's verify. So are points
    /// recorded before #495 ([`TrendPoint::recorded_before_fix`]): their
    /// rates are in another unit.
    pub fn rising(&self, factor: f64) -> Option<Rise> {
        let mut rated = self
            .points
            .iter()
            .filter(|p| p.judged())
            .filter_map(|p| p.corrected_per_gib)
            .rev();
        let newest = rated.next()?;
        let previous = rated.next()?;
        (newest > previous * factor && newest > RISE_FLOOR_PER_GIB)
            .then_some(Rise { previous, newest })
    }

    /// `0.012 -> 0.030 -> 0.950 corrected/GiB over 3 verifies (uncorrected 0, 0, 1)`
    pub fn render(&self) -> String {
        let rates: Vec<String> = self
            .points
            .iter()
            .map(|p| match p.corrected_per_gib {
                _ if p.recorded_before_fix => "old".to_string(),
                Some(r) => format!("{r:.3}{}", if p.scope_unknown { "?" } else { "" }),
                None => "-".to_string(),
            })
            .collect();
        let uncorrected: Vec<String> = self
            .points
            .iter()
            .map(|p| p.uncorrected.to_string())
            .collect();
        let mut marks = String::new();
        if self
            .points
            .iter()
            .any(|p| p.scope_unknown && !p.recorded_before_fix)
        {
            marks.push_str(
                "; ? = the page may have counted reads this verify did not make, so it is not \
                 compared",
            );
        }
        if self.points.iter().any(|p| p.recorded_before_fix) {
            marks.push_str(
                "; old = recorded before 1.7.1 over a count of data sets, not bytes, so it is \
                 not compared",
            );
        }
        format!(
            "{} corrected/GiB over {} verif{} (uncorrected {}{marks})",
            rates.join(" -> "),
            self.points.len(),
            if self.points.len() == 1 { "y" } else { "ies" },
            uncorrected.join(", "),
        )
    }
}

/// Every cartridge's recorded verify figures, grouped and in time order.
///
/// Full verifies only: the point's `verification_sessions` row must say
/// `verify_type = 'full'`. `volume verify` records no other (the review of
/// #421), and this is the second check — read from the session the point
/// names rather than from a details field, so it holds for every row
/// whenever it was written.
///
/// Only media that will be verified again (the review of #421): a trend on
/// a cartridge the operator has already acted on would warn forever, its
/// last two points never changing. A cartridge's points drop out while it
/// is [`OUT_OF_SERVICE_CARTRIDGE`] (`cartridge retire`, or `volume retire`
/// /`compact-finish` freeing it for erasure); a `volume:<label>` trend's
/// while its volume no longer holds bytes to verify
/// ([`coverage::holds_bytes_to_verify`]: retired or erased — a quarantined
/// volume stays, since re-verifying it is when its trend matters).
///
/// A point whose details lack [`RATE_DENOMINATOR_KEY`] was recorded before
/// #495 and is marked so ([`TrendPoint::recorded_before_fix`]), from the
/// same single read: no statement per point.
pub fn trends(conn: &Connection) -> Result<Vec<CartridgeTrend>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT e.timestamp, COALESCE(e.entity_label, v.label, '?'), e.details, c.barcode
           FROM events e
           JOIN verification_sessions s
             ON s.id = json_extract(e.details, '$.session_id') AND s.verify_type = 'full'
           LEFT JOIN volumes v ON v.id = e.entity_id AND e.entity_type = 'volume'
           LEFT JOIN cartridges c ON c.id = json_extract(e.details, '$.cartridge_id')
          WHERE e.action = ?1
            AND CASE WHEN c.id IS NOT NULL
                     THEN c.status NOT IN ({out})
                     ELSE COALESCE({live}, 0) END
          ORDER BY e.id",
        out = OUT_OF_SERVICE_CARTRIDGE
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(","),
        live = coverage::holds_bytes_to_verify("v"),
    ))?;
    let rows = stmt
        .query_map([EVENT_ACTION], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut out: Vec<CartridgeTrend> = Vec::new();
    for (at, volume, details, barcode) in rows {
        let d: serde_json::Value = details
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        let recorded_before_fix = d.get(RATE_DENOMINATOR_KEY).is_none();
        let point = TrendPoint {
            at,
            drive_serial: d["drive_serial"].as_str().map(str::to_string),
            gib_read: d["verify_bytes"]
                .as_f64()
                .filter(|b| *b > 0.0)
                .map(|b| b / GIB),
            corrected: d["corrected"].as_i64().unwrap_or(0),
            uncorrected: d["uncorrected"].as_i64().unwrap_or(0),
            corrected_per_gib: d["corrected_per_gib"].as_f64(),
            scope: (!recorded_before_fix)
                .then(|| d["scope"].as_str().map(str::to_string))
                .flatten(),
            scope_unknown: d["scope_unknown"].as_bool().unwrap_or(false),
            recorded_before_fix,
            volume: volume.clone(),
        };
        let key = barcode.unwrap_or_else(|| format!("volume:{volume}"));
        match out.iter_mut().find(|t| t.cartridge == key) {
            Some(t) => t.points.push(point),
            None => out.push(CartridgeTrend {
                cartridge: key,
                points: vec![point],
            }),
        }
    }
    out.sort_by(|a, b| a.cartridge.cmp(&b.cartridge));
    Ok(out)
}

/// The sentence a flagged cartridge is reported with, in `audit` and
/// `report health` alike.
pub fn rise_message(trend: &CartridgeTrend, rise: Rise, factor: f64) -> String {
    format!(
        "corrected read errors per GiB rose from {:.3} to {:.3} between its last two verifies, \
         more than the {}x rise factor (`[health] read_error_rise_factor`, provisional) and \
         above the floor of {} per GiB: {}. The data still verified; the drive's error \
         correction is working harder. Copy it to a fresh cartridge while it reads",
        rise.previous,
        rise.newest,
        factor,
        RISE_FLOOR_PER_GIB,
        trend.render(),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rusqlite::params;

    /// One GiB of LTO-6 data, in the HP drive's data sets: 2^30 /
    /// [`LTO6_DATA_SET_BYTES`] is 434.9.
    pub(crate) const GIB_OF_DATA_SETS: i64 = 435;
    const GIB_BYTES: u64 = 1 << 30;

    /// The HP drive every seeded verify's contact names.
    fn seed_drive(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT OR IGNORE INTO drives (serial, vendor, model)
             VALUES ('SEED-HP', 'HP', 'Ultrium 6-SCSI')",
            [],
        )
        .unwrap();
        conn.query_row("SELECT id FROM drives WHERE serial = 'SEED-HP'", [], |r| {
            r.get(0)
        })
        .unwrap()
    }

    /// A registered cartridge `barcode` holding a sealed volume
    /// `V-<barcode>`, verified once per entry of `corrected` through the
    /// route `volume verify` takes: a contact on an HP drive, each verify the
    /// first of its own load, its journalled page 0x03 (that many errors
    /// corrected over a GiB of data sets), a passed full verification
    /// session, then [`record_for_verify`] with a 1 GiB readback. Each point
    /// is `n` corrected errors per GiB, of exact scope.
    pub(crate) fn seed_verifies(conn: &Connection, barcode: &str, corrected: &[i64]) {
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number)
             VALUES (?1, 'LTO-6', 2500000000000, ?1)",
            [barcode],
        )
        .unwrap();
        let cart = conn.last_insert_rowid();
        let vol = seed_volume(conn, &format!("V-{barcode}"));
        for &n in corrected {
            seed_one_verify(conn, Some(cart), vol, n, Tier::Integrity);
        }
    }

    /// A sealed volume `label` whose verifies' contacts identified no
    /// cartridge (the trend's `volume:<label>` key), verified once per
    /// entry of `corrected` as [`seed_verifies`] does. With no cartridge
    /// there is no same-load baseline, so each point's scope is weighed: a
    /// GiB of data sets against a 1 GiB readback is [`Scope::Estimated`].
    /// Returns its id.
    pub(crate) fn seed_unbound_verifies(conn: &Connection, label: &str, corrected: &[i64]) -> i64 {
        let vol = seed_volume(conn, label);
        for &n in corrected {
            seed_one_verify(conn, None, vol, n, Tier::Integrity);
        }
        vol
    }

    fn seed_volume(conn: &Connection, label: &str) -> i64 {
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                 capacity_bytes, status)
             VALUES (?1, 'lto', 'p', 'LTO-6', 2500000000000, 'sealed')",
            [label],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// A contact for `vol` on the seeded HP drive, naming `cart` at load
    /// count `loads` (each `None` for an unidentified one).
    fn seed_contact(
        conn: &Connection,
        operation: &str,
        cart: Option<i64>,
        loads: Option<i64>,
        vol: i64,
    ) -> i64 {
        let drive = seed_drive(conn);
        conn.execute(
            "INSERT INTO cartridge_contacts (cartridge_id, volume_id, drive_id, operation,
                                             device, chip_load_count)
             VALUES (?1, ?2, ?3, ?4, '/dev/nst0', ?5)",
            params![cart, vol, drive, operation, loads],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn journal_0x03(conn: &Connection, contact: i64, decoded: &str) {
        conn.execute(
            "INSERT INTO log_page_journal (contact_id, device_sg, trigger, page_code, ok,
                 tool_argv, decoded, tapectl_version)
             VALUES (?1, '/dev/sg0', 'volume verify', 3, 1, '[]', ?2, 't')",
            params![contact, decoded],
        )
        .unwrap();
    }

    /// A completed verification session of `vol` at `tier`.
    fn seed_session(conn: &Connection, vol: i64, tier: Tier) -> i64 {
        let verify_type = match tier {
            Tier::Integrity => "full",
            Tier::Navigable => "quick",
        };
        conn.execute(
            "INSERT INTO verification_sessions (volume_id, verify_type, completed_at, outcome)
             VALUES (?1, ?2, datetime('now'), 'passed')",
            params![vol, verify_type],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn label_of(conn: &Connection, vol: i64) -> String {
        conn.query_row("SELECT label FROM volumes WHERE id = ?1", [vol], |r| {
            r.get(0)
        })
        .unwrap()
    }

    /// One verify of `vol` at `tier`: its contact (on `cart`, the first of a
    /// new load, or none), the page 0x03 its sweep journalled (`n` corrected
    /// over a GiB of data sets), its session, and [`record_for_verify`] over
    /// a 1 GiB readback — called whatever the tier, so a test can put a
    /// quick verify's figures in front of [`trends`].
    fn seed_one_verify(conn: &Connection, cart: Option<i64>, vol: i64, n: i64, tier: Tier) {
        let loads: Option<i64> = cart.map(|c| {
            conn.query_row(
                "SELECT COUNT(*) + 1 FROM cartridge_contacts WHERE cartridge_id = ?1",
                [c],
                |r| r.get(0),
            )
            .unwrap()
        });
        let contact = seed_contact(conn, "volume verify", cart, loads, vol);
        journal_0x03(conn, contact, &page_0x03(n, 0, 0, GIB_OF_DATA_SETS));
        let session = seed_session(conn, vol, tier);
        let label = label_of(conn, vol);
        record_for_verify(
            conn,
            Some(contact),
            session,
            vol,
            &label,
            tier,
            Some(GIB_BYTES),
        )
        .expect("the journalled page 0x03 is recorded");
    }

    /// A page 0x03 decode with chosen counters, in sg_logs's own shape;
    /// `data_sets` is the line sg_logs calls "Total bytes processed".
    pub(crate) fn page_0x03(
        no_delay: i64,
        rereads: i64,
        uncorrected: i64,
        data_sets: i64,
    ) -> String {
        format!(
            "Read error counter page  [0x3]\n  Errors corrected without substantial delay = \
             {no_delay}\n  Errors corrected with possible delays = 0\n  Total rewrites or \
             rereads = {rereads}\n  Total errors corrected = 0\n  Total times correction \
             algorithm processed = {no_delay}\n  Total bytes processed = {data_sets}\n  Total \
             uncorrected errors = {uncorrected}\n"
        )
    }

    fn details(conn: &Connection) -> Vec<serde_json::Value> {
        conn.prepare("SELECT details FROM events WHERE action = ?1 ORDER BY id")
            .unwrap()
            .query_map([EVENT_ACTION], |r| {
                Ok(serde_json::from_str(&r.get::<_, String>(0)?).unwrap())
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn exact(counts: ReadErrors, verify_bytes: u64) -> VerifyReadErrors {
        VerifyReadErrors {
            counts,
            verify_bytes: Some(verify_bytes),
            scope: Scope::SameLoad {
                baseline_contact_id: None,
            },
            data_set_bytes: None,
        }
    }

    #[test]
    fn the_hp_shape_counts_corrected_errors_the_total_leaves_at_zero() {
        let e = ReadErrors::from_decoded_0x03(&page_0x03(40, 3, 0, 4 * GIB_OF_DATA_SETS));
        assert_eq!(e.total_corrected, 0, "the HP leaves the total at 0");
        assert_eq!(e.corrected(), 40);
        assert_eq!(e.data_sets, 4 * GIB_OF_DATA_SETS);
        let f = exact(e, 4 * GIB_BYTES);
        assert_eq!(f.corrected_per_gib(), Some(10.0));
        assert_eq!(f.per_gib(e.rereads), Some(0.75));
    }

    /// Issue #495's fact, on the real drive's capture. HP-V3 p64 calls page
    /// 0x03's parameter 5 "Total data sets processed"; sg_logs prints it as
    /// "Total bytes processed". The no-medium capture (the last tape's
    /// counts, still held) reads 296 beside 69 corrections. The same
    /// capture proves the unit on page 0x02, whose parameter is the same
    /// kind: page 0x17 says the last mount wrote 30,043 MB, and page 0x02
    /// counted 12,820 — 2.34 MB each, within 10% of the LTO-6 data set the
    /// drive phase measured, and nowhere near one byte each.
    ///
    /// The read side does NOT reconcile on this capture: page 0x17 says the
    /// last mount read 18,050 MB against page 0x03's 296, about 61 MB a
    /// count. Page 0x03's data-set semantics rest on HP-V3's text and page
    /// 0x02's analogy, not on a reconciled read figure; if parameter 5
    /// undercounts reads, the weighed scope check fires later than it should
    /// (a same-load difference is unaffected).
    #[test]
    fn the_hp_fixture_counts_data_sets_not_bytes() {
        const NOMEDIA: &str = "../../tests/fixtures/sg_logs/hp_lto6_sg0_nomedia";
        let p03 = ReadErrors::from_decoded_0x03(include_str!(concat!(
            "../../tests/fixtures/sg_logs/hp_lto6_sg0_nomedia",
            "/page_0x03.decoded.txt"
        )));
        assert_eq!((p03.corrected(), p03.data_sets), (69, 296));

        let p02 = crate::tape::feed_ratio::WriteCounters::from_decoded_0x02(include_str!(concat!(
            "../../tests/fixtures/sg_logs/hp_lto6_sg0_nomedia",
            "/page_0x02.decoded.txt"
        )));
        let p17 = include_str!(concat!(
            "../../tests/fixtures/sg_logs/hp_lto6_sg0_nomedia",
            "/page_0x17.decoded.txt"
        ));
        let mb_written: u64 = p17
            .lines()
            .find_map(|l| l.trim().strip_prefix("Last mount megabytes written: "))
            .expect("positive control: the capture carries the line")
            .trim()
            .parse()
            .unwrap();
        let per_count = (mb_written * 1_000_000) as f64 / p02.data_sets as f64;
        assert!(
            (per_count / LTO6_DATA_SET_BYTES as f64 - 1.0).abs() < 0.10,
            "{per_count:.0} bytes per count ({NOMEDIA})"
        );

        // The old rate divided 69 by 296 "bytes" — 2.5e8 per GiB. Over the
        // GiB a verify of that page would have read, it is 69.
        let old = p03.corrected() as f64 / (p03.data_sets as f64 / GIB);
        assert!(old > 2.0e8, "{old}");
        assert_eq!(exact(p03, GIB_BYTES).corrected_per_gib(), Some(69.0));
    }

    /// mhvtl reports 0 for every page 0x03 parameter, and the real drive's
    /// page reads all zeros on a cartridge just loaded: a page that counted
    /// no data set has no rate, whatever the verify read.
    #[test]
    fn a_page_that_counted_no_data_set_has_no_rate() {
        let fixture = include_str!("../../tests/fixtures/sg_logs/hp_lto6_page_0x03.txt");
        let e = ReadErrors::from_decoded_0x03(fixture);
        assert_eq!(e, ReadErrors::default(), "the real all-zero page");
        assert_eq!(exact(e, GIB_BYTES).corrected_per_gib(), None);
        // Positive control: one data set counted, and the same bytes rate.
        let one = ReadErrors { data_sets: 1, ..e };
        assert_eq!(exact(one, GIB_BYTES).corrected_per_gib(), Some(0.0));
        assert_eq!(
            VerifyReadErrors {
                verify_bytes: None,
                ..exact(one, GIB_BYTES)
            }
            .corrected_per_gib(),
            None,
            "no byte count, no rate"
        );
    }

    fn point(rate: Option<f64>) -> TrendPoint {
        TrendPoint {
            at: "t".into(),
            volume: "V".into(),
            drive_serial: None,
            gib_read: Some(1.0),
            corrected: 0,
            uncorrected: 0,
            corrected_per_gib: rate,
            scope: Some("same_load".into()),
            scope_unknown: false,
            recorded_before_fix: false,
        }
    }

    fn trend(rates: &[Option<f64>]) -> CartridgeTrend {
        CartridgeTrend {
            cartridge: "C1".into(),
            points: rates.iter().map(|r| point(*r)).collect(),
        }
    }

    #[test]
    fn rising_compares_the_two_newest_rated_verifies_against_the_factor() {
        // Doubling is not MORE than 2x; just past it is.
        assert_eq!(trend(&[Some(1.0), Some(2.0)]).rising(2.0), None);
        assert_eq!(
            trend(&[Some(1.0), Some(2.1)]).rising(2.0),
            Some(Rise {
                previous: 1.0,
                newest: 2.1
            })
        );
        // An unrated verify (no data set counted) is skipped, not read as 0.
        assert_eq!(
            trend(&[Some(1.0), None, Some(3.0)])
                .rising(2.0)
                .map(|r| r.previous),
            Some(1.0)
        );
        // Falling, or a single verify, is not a rise.
        assert_eq!(trend(&[Some(3.0), Some(1.0)]).rising(2.0), None);
        assert_eq!(trend(&[Some(3.0)]).rising(2.0), None);
        // ADR-0012 2026-10-07 item 7: from zero, a rise past any factor is
        // flagged only above the absolute floor of 1 corrected error per GiB,
        // so a first non-zero reading after zero is not an alarm.
        assert_eq!(trend(&[Some(0.0), Some(0.5)]).rising(2.0), None);
        assert_eq!(trend(&[Some(0.0), Some(0.01)]).rising(100.0), None);
        assert_eq!(
            trend(&[Some(0.0), Some(2.0)]).rising(2.0),
            Some(Rise {
                previous: 0.0,
                newest: 2.0
            })
        );
        // At the floor is not above it; past both the floor and the factor is.
        assert_eq!(
            trend(&[Some(0.2), Some(RISE_FLOOR_PER_GIB)]).rising(2.0),
            None
        );
        assert!(trend(&[Some(0.2), Some(1.01)]).rising(2.0).is_some());
        assert_eq!(
            trend(&[Some(1.0), Some(2.1), None]).render(),
            "1.000 -> 2.100 -> - corrected/GiB over 3 verifies (uncorrected 0, 0, 0)"
        );
    }

    /// Item 7's floor in real units (issue #495). One corrected error over a
    /// GiB read is 1.0 per GiB: at the floor, so 0 -> 1 is no alarm. The old
    /// unit divided the same one error by 435 data sets read as bytes —
    /// 2.5 million per "GiB", past every floor and factor (the negative
    /// control: the cartridge the old unit flagged). Three errors per real
    /// GiB from zero is flagged (the positive control).
    #[test]
    fn the_floor_is_judged_per_real_gib() {
        let one = exact(
            ReadErrors {
                corrected_no_delay: 1,
                data_sets: GIB_OF_DATA_SETS,
                ..ReadErrors::default()
            },
            GIB_BYTES,
        );
        assert_eq!(one.corrected_per_gib(), Some(1.0));
        assert_eq!(
            trend(&[Some(0.0), one.corrected_per_gib()]).rising(2.0),
            None
        );
        let old_unit = 1.0 / (GIB_OF_DATA_SETS as f64 / GIB);
        assert!(
            trend(&[Some(0.0), Some(old_unit)]).rising(2.0).is_some(),
            "the old unit flagged it: {old_unit}"
        );
        let three = exact(
            ReadErrors {
                corrected_no_delay: 3,
                ..one.counts
            },
            GIB_BYTES,
        );
        assert_eq!(
            trend(&[Some(0.0), three.corrected_per_gib()]).rising(2.0),
            Some(Rise {
                previous: 0.0,
                newest: 3.0
            })
        );
    }

    #[test]
    fn scope_is_unknown_past_twice_what_the_verify_read() {
        let size = Some(LTO6_DATA_SET_BYTES);
        // 869 data sets are 2,145,508,860 bytes, inside twice a GiB; 870
        // are 2,147,977,800, past it.
        assert!(!scope_unknown(869, Some(GIB_BYTES), size), "within twice");
        assert!(scope_unknown(870, Some(GIB_BYTES), size), "past twice");
        assert!(
            !scope_unknown(GIB_OF_DATA_SETS / 2, Some(GIB_BYTES), size),
            "less than the verify read: the page counted no more than it"
        );
        assert!(
            scope_unknown(GIB_OF_DATA_SETS, None, size),
            "no byte count: no honest figure"
        );
        assert!(
            scope_unknown(GIB_OF_DATA_SETS, Some(GIB_BYTES), None),
            "no data-set size: no honest figure"
        );
        assert_eq!(data_set_bytes(Some("HP"), Some("LTO-6")), size);
        assert_eq!(data_set_bytes(Some("IBM"), Some("LTO-6")), None);
        assert_eq!(data_set_bytes(Some("HP"), Some("LTO-5")), None);
        assert_eq!(data_set_bytes(None, Some("LTO-6")), None);
    }

    /// Rates from a pair of journalled page 0x03 readings in one load: the
    /// write's sweep after its full confirm (2 GiB of data sets read, 50
    /// corrected) and the verify's after its readback. The verify's counts
    /// are the difference, over the 1 GiB it read: 20 corrected per GiB, of
    /// exact scope, naming the write's contact. The whole reading over the
    /// verify's bytes would have said 70.
    #[test]
    fn the_rates_are_the_reading_less_the_previous_contact_in_the_same_load() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number)
             VALUES ('L6-0001', 'LTO-6', 2500000000000, 'L6-0001')",
            [],
        )
        .unwrap();
        let cart = conn.last_insert_rowid();
        let vol = seed_volume(&conn, "L6-0001");
        let write = seed_contact(&conn, "volume write", Some(cart), Some(7), vol);
        journal_0x03(&conn, write, &page_0x03(50, 2, 0, 2 * GIB_OF_DATA_SETS));
        let verify = seed_contact(&conn, "volume verify", Some(cart), Some(7), vol);
        journal_0x03(&conn, verify, &page_0x03(70, 3, 0, 3 * GIB_OF_DATA_SETS));
        let session = seed_session(&conn, vol, Tier::Integrity);
        let f = record_for_verify(
            &conn,
            Some(verify),
            session,
            vol,
            "L6-0001",
            Tier::Integrity,
            Some(GIB_BYTES),
        )
        .unwrap();
        assert_eq!(
            f.scope,
            Scope::SameLoad {
                baseline_contact_id: Some(write)
            }
        );
        assert_eq!(f.counts.corrected(), 20);
        assert_eq!(f.counts.data_sets, GIB_OF_DATA_SETS);
        assert_eq!(f.corrected_per_gib(), Some(20.0));
        assert_eq!(f.per_gib(f.counts.rereads), Some(1.0));

        let d = &details(&conn)[0];
        assert_eq!(d[RATE_DENOMINATOR_KEY], RATE_DENOMINATOR);
        assert_eq!(d["verify_bytes"], GIB_BYTES);
        assert_eq!(d["scope"], "same_load");
        assert_eq!(d["scope_unknown"], false);
        assert_eq!(d["baseline_contact_id"], write);
        assert_eq!(d["data_sets"], GIB_OF_DATA_SETS);
        assert_eq!(d["corrected"], 20);
        assert_eq!(d["corrected_per_gib"], 20.0);
        assert!(d.get("bytes_processed").is_none(), "{d}");
        assert!(d.get("data_set_bytes").is_none(), "not weighed: {d}");
        let t = trends(&conn).unwrap();
        assert_eq!(t[0].points[0].gib_read, Some(1.0));
        assert_eq!(t[0].points[0].scope.as_deref(), Some("same_load"));
    }

    /// HP-V3 p64: page 0x03 is cleared when the tape is unloaded, so a
    /// verify that is the first contact of its load counts from zero:
    /// nothing before it on the drive, another cartridge before it, or the
    /// same cartridge at an earlier load count. Exact scope, and the
    /// baseline is the load's zero (`baseline_contact_id` null).
    #[test]
    fn the_first_contact_of_a_load_on_an_hp_drive_counts_from_zero() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number)
             VALUES ('L6-0001', 'LTO-6', 2500000000000, 'S1'),
                    ('L6-0002', 'LTO-6', 2500000000000, 'S2')",
            [],
        )
        .unwrap();
        let vol = seed_volume(&conn, "L6-0001");
        let mut verifies = Vec::new();
        verifies.push(seed_contact(&conn, "volume verify", Some(1), Some(4), vol));
        let other = seed_contact(&conn, "volume write", Some(2), Some(9), vol);
        journal_0x03(&conn, other, &page_0x03(900, 9, 1, 9 * GIB_OF_DATA_SETS));
        verifies.push(seed_contact(&conn, "volume verify", Some(1), Some(5), vol));
        let earlier_load = seed_contact(&conn, "volume write", Some(1), Some(5), vol);
        journal_0x03(
            &conn,
            earlier_load,
            &page_0x03(900, 9, 1, 9 * GIB_OF_DATA_SETS),
        );
        verifies.push(seed_contact(&conn, "volume verify", Some(1), Some(6), vol));
        for (i, v) in verifies.iter().enumerate() {
            journal_0x03(&conn, *v, &page_0x03(4, 0, 0, GIB_OF_DATA_SETS));
            let session = seed_session(&conn, vol, Tier::Integrity);
            let f = record_for_verify(
                &conn,
                Some(*v),
                session,
                vol,
                "L6-0001",
                Tier::Integrity,
                Some(GIB_BYTES),
            )
            .unwrap();
            assert_eq!(
                f.scope,
                Scope::SameLoad {
                    baseline_contact_id: None
                },
                "verify {i}"
            );
            assert_eq!(f.corrected_per_gib(), Some(4.0), "verify {i}");
            let d = &details(&conn)[i];
            assert_eq!(d["baseline_contact_id"], serde_json::Value::Null, "{d}");
            assert_eq!(d["scope"], "same_load", "{d}");
        }
    }

    /// Issue #467's protection, able to fire since #495. A restore read 3
    /// GiB earlier in the verify's load, in a contact that took no sweep
    /// (it was killed), so there is no baseline and the whole reading is
    /// weighed: 4 GiB of data sets against a 1 GiB readback is past twice,
    /// so the point is recorded, shown with a `?`, and never judged — its
    /// 9/GiB over the earlier 1, 1 would be a rise past any factor. The same
    /// unswept restore having read nothing leaves a reading weighed in scope
    /// ([`Scope::Estimated`]), which IS a rise (the positive control); with
    /// the restore's contact swept, the difference is exact, and a rise too.
    #[test]
    fn scope_unknown_fires_for_an_earlier_read_in_the_load_with_no_baseline() {
        for (earlier_gib, swept, scope) in [
            (3, false, "unknown"),
            (0, false, "estimated"),
            (3, true, "same_load"),
        ] {
            let conn = crate::db::open_memory().unwrap();
            seed_verifies(&conn, "C-S", &[1, 1]);
            let (cart, vol): (i64, i64) = conn
                .query_row(
                    "SELECT c.id, v.id FROM cartridges c, volumes v
                      WHERE c.barcode = 'C-S' AND v.label = 'V-C-S'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            let restore = seed_contact(&conn, "restore unit", Some(cart), Some(3), vol);
            if swept {
                journal_0x03(
                    &conn,
                    restore,
                    &page_0x03(0, 0, 0, earlier_gib * GIB_OF_DATA_SETS),
                );
            }
            let verify = seed_contact(&conn, "volume verify", Some(cart), Some(3), vol);
            journal_0x03(
                &conn,
                verify,
                &page_0x03(9, 0, 0, (earlier_gib + 1) * GIB_OF_DATA_SETS),
            );
            let session = seed_session(&conn, vol, Tier::Integrity);
            let f = record_for_verify(
                &conn,
                Some(verify),
                session,
                vol,
                "V-C-S",
                Tier::Integrity,
                Some(GIB_BYTES),
            )
            .unwrap();
            let flagged = scope == "unknown";
            assert_eq!(f.scope.as_str(), scope, "{earlier_gib} GiB, swept {swept}");
            assert_eq!(f.scope_unknown(), flagged, "{scope}");
            let d = &details(&conn)[2];
            assert_eq!(d["scope"], scope, "{d}");
            assert_eq!(d["scope_unknown"], flagged, "{d}");
            assert_eq!(d["corrected_per_gib"], 9.0, "{d}");
            let t = trends(&conn).unwrap();
            assert_eq!(t[0].points[2].scope_unknown, flagged, "{scope}");
            if flagged {
                assert_eq!(d["data_set_bytes"], LTO6_DATA_SET_BYTES, "{d}");
                assert_eq!(
                    t[0].rising(DEFAULT_RISE_FACTOR),
                    None,
                    "the unknown-scope reading is skipped, leaving 1.0 -> 1.0"
                );
                let line = t[0].render();
                assert!(line.contains("9.000?"), "{line}");
                assert!(line.contains("not compared"), "{line}");
            } else {
                assert!(
                    t[0].rising(DEFAULT_RISE_FACTOR).is_some(),
                    "{scope}: the same reading in scope is a rise"
                );
                assert!(!t[0].render().contains('?'), "{scope}");
            }
        }
    }

    /// With no baseline and no honest figure to weigh by, the scope is
    /// unknown: another vendor's drive at the first contact of its load (no
    /// statement that its page clears, nor what its parameter 5 counts). A
    /// counter that went DOWN against the previous contact in the load is no
    /// baseline either (the page was cleared between them): the whole
    /// reading is weighed instead, and on the HP drive it is in scope.
    #[test]
    fn scope_unknown_without_an_honest_figure_and_a_cleared_page_is_weighed() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number)
             VALUES ('L6-0001', 'LTO-6', 2500000000000, 'S1')",
            [],
        )
        .unwrap();
        let vol = seed_volume(&conn, "L6-0001");
        conn.execute(
            "INSERT INTO drives (serial, vendor, model) VALUES ('IBM-1', 'IBM', 'ULT3580-TD6')",
            [],
        )
        .unwrap();
        let ibm = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO cartridge_contacts (cartridge_id, volume_id, drive_id, operation, device,
                                             chip_load_count)
             VALUES (1, ?1, ?2, 'volume verify', '/dev/nst1', 2)",
            params![vol, ibm],
        )
        .unwrap();
        let on_ibm = conn.last_insert_rowid();
        journal_0x03(&conn, on_ibm, &page_0x03(4, 0, 0, GIB_OF_DATA_SETS));
        let f = across_verify(&conn, on_ibm, vol, Some(GIB_BYTES))
            .unwrap()
            .unwrap();
        assert_eq!((f.scope, f.data_set_bytes), (Scope::Unknown, None));
        assert_eq!(f.corrected_per_gib(), Some(4.0), "shown, marked");

        let before = seed_contact(&conn, "volume write", Some(1), Some(8), vol);
        journal_0x03(&conn, before, &page_0x03(500, 0, 0, 5 * GIB_OF_DATA_SETS));
        let after = seed_contact(&conn, "volume verify", Some(1), Some(8), vol);
        journal_0x03(&conn, after, &page_0x03(4, 0, 0, GIB_OF_DATA_SETS));
        let f = across_verify(&conn, after, vol, Some(GIB_BYTES))
            .unwrap()
            .unwrap();
        assert_eq!(f.scope, Scope::Estimated, "{f:?}");
        assert_eq!(f.counts.corrected(), 4, "the whole reading");
        assert_eq!(f.data_set_bytes, Some(LTO6_DATA_SET_BYTES));
    }

    /// Issue #495: points recorded before the fix carry rates over page
    /// 0x03's data-set count. They stay as recorded, are shown as `old`,
    /// and are never compared — here an old 0.01 -> 2.0 that would be a rise
    /// past the factor and the floor. A new point beside them is one judged
    /// point, no trend; two new points that rise are flagged (the positive
    /// control).
    #[test]
    fn points_recorded_before_the_fix_are_marked_and_never_compared() {
        let conn = crate::db::open_memory().unwrap();
        seed_verifies(&conn, "C-OLD", &[]);
        let vol: i64 = conn
            .query_row("SELECT id FROM volumes WHERE label = 'V-C-OLD'", [], |r| {
                r.get(0)
            })
            .unwrap();
        // 1.1.2's shape (with `verify_bytes`), then 1.1.0's (without).
        for (rate, verify_bytes) in [("0.010000", "1073741824"), ("2.000000", "null")] {
            let session = seed_session(&conn, vol, Tier::Integrity);
            conn.execute(
                &format!(
                    "INSERT INTO events (entity_type, entity_id, entity_label, action, field,
                                         new_value, details)
                     VALUES ('volume', ?1, 'V-C-OLD', ?2, ?3, ?4,
                             json_object('session_id', ?5, 'cartridge_id', 1,
                                         'bytes_processed', 296, 'verify_bytes', {verify_bytes},
                                         'scope_unknown', json('false'), 'corrected', 2,
                                         'uncorrected', 0, 'corrected_per_gib', ?6))"
                ),
                params![
                    vol,
                    EVENT_ACTION,
                    EVENT_FIELD,
                    rate,
                    session,
                    rate.parse::<f64>().unwrap()
                ],
            )
            .unwrap();
        }
        let t = trends(&conn).unwrap();
        assert_eq!(t.len(), 1, "{t:?}");
        let old = &t[0].points;
        assert!(old.iter().all(|p| p.recorded_before_fix), "{old:?}");
        assert!(old.iter().all(|p| p.scope.is_none()), "{old:?}");
        assert_eq!(
            old[0].gib_read,
            Some(1.0),
            "1.1.2 recorded the verify's bytes"
        );
        assert_eq!(old[1].gib_read, None, "1.1.0 recorded none");
        assert_eq!(old[1].corrected_per_gib, Some(2.0), "kept as recorded");
        assert_eq!(t[0].rising(DEFAULT_RISE_FACTOR), None);
        let line = t[0].render();
        assert!(line.starts_with("old -> old corrected/GiB"), "{line}");
        assert!(line.contains("recorded before 1.7.1"), "{line}");
        let stored: Vec<String> = conn
            .prepare("SELECT new_value FROM events WHERE action = ?1 ORDER BY id")
            .unwrap()
            .query_map([EVENT_ACTION], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(stored, ["0.010000", "2.000000"], "no stored row rewritten");

        let cart: i64 = conn
            .query_row(
                "SELECT id FROM cartridges WHERE barcode = 'C-OLD'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        seed_one_verify(&conn, Some(cart), vol, 1, Tier::Integrity);
        assert_eq!(trends(&conn).unwrap()[0].rising(DEFAULT_RISE_FACTOR), None);
        seed_one_verify(&conn, Some(cart), vol, 5, Tier::Integrity);
        let t = trends(&conn).unwrap();
        assert_eq!(
            t[0].rising(DEFAULT_RISE_FACTOR),
            Some(Rise {
                previous: 1.0,
                newest: 5.0
            }),
            "positive control: two points of the new unit"
        );
        assert!(
            t[0].render()
                .starts_with("old -> old -> 1.000 -> 5.000 corrected/GiB over 4 verifies"),
            "{}",
            t[0].render()
        );
    }

    fn trend_keys(conn: &Connection) -> Vec<String> {
        trends(conn)
            .unwrap()
            .into_iter()
            .map(|t| t.cartridge)
            .collect()
    }

    /// The second check of the review of #421: whatever reaches `events`,
    /// [`trends`] reads only points whose verification session was full. A
    /// quick verify's 0.0 between two full ones would otherwise read as a
    /// rise past any factor. Seen failing without the session join: three
    /// points, rising from 0.000.
    #[test]
    fn trends_read_only_full_verifies() {
        let conn = crate::db::open_memory().unwrap();
        seed_verifies(&conn, "C-Q", &[2]);
        let (cart, vol): (i64, i64) = conn
            .query_row(
                "SELECT c.id, v.id FROM cartridges c, volumes v
                  WHERE c.barcode = 'C-Q' AND v.label = 'V-C-Q'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        seed_one_verify(&conn, Some(cart), vol, 0, Tier::Navigable);
        seed_one_verify(&conn, Some(cart), vol, 3, Tier::Integrity);
        let recorded: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE action = ?1",
                [EVENT_ACTION],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            recorded, 3,
            "positive control: the quick point is in events"
        );
        let t = trends(&conn).unwrap();
        assert_eq!(t.len(), 1, "{t:?}");
        let rates: Vec<_> = t[0].points.iter().map(|p| p.corrected_per_gib).collect();
        assert_eq!(rates, [Some(2.0), Some(3.0)], "the quick verify is skipped");
        assert_eq!(t[0].rising(DEFAULT_RISE_FACTOR), None);
    }

    /// The review of #421: a cartridge the operator has acted on — retired
    /// (`retired_permanent`) or freed for erasure (`pending_erase`) — is
    /// never verified again, so a rise on it would warn forever. It drops
    /// out of the trend; back in service, it returns (the positive control).
    #[test]
    fn a_retired_or_pending_erase_cartridge_leaves_the_trend() {
        let conn = crate::db::open_memory().unwrap();
        seed_verifies(&conn, "C-OLD", &[1, 5]);
        seed_verifies(&conn, "C-KEEP", &[1, 1]);
        assert_eq!(trend_keys(&conn), ["C-KEEP", "C-OLD"]);
        for status in ["retired_permanent", "pending_erase"] {
            conn.execute(
                "UPDATE cartridges SET status = ?1 WHERE barcode = 'C-OLD'",
                [status],
            )
            .unwrap();
            assert_eq!(trend_keys(&conn), ["C-KEEP"], "{status}");
        }
        conn.execute(
            "UPDATE cartridges SET status = 'in_use' WHERE barcode = 'C-OLD'",
            [],
        )
        .unwrap();
        assert_eq!(trend_keys(&conn), ["C-KEEP", "C-OLD"], "in service again");
    }

    /// The same for a trend keyed by volume (no cartridge identified): a
    /// retired or erased volume holds nothing a verify will read again, so
    /// it drops out (`policy::coverage::holds_bytes_to_verify`). A
    /// quarantined one stays: re-verifying it is when its trend matters.
    #[test]
    fn a_gone_volume_leaves_the_trend_and_a_quarantined_one_stays() {
        let conn = crate::db::open_memory().unwrap();
        let v = seed_unbound_verifies(&conn, "V-LOOSE", &[1, 5]);
        assert_eq!(trend_keys(&conn), ["volume:V-LOOSE"]);
        conn.execute(
            "UPDATE volumes SET observed_condition = 'quarantined' WHERE id = ?1",
            [v],
        )
        .unwrap();
        assert_eq!(trend_keys(&conn), ["volume:V-LOOSE"], "quarantined stays");
        for status in ["retired", "erased"] {
            conn.execute(
                "UPDATE volumes SET status = ?1 WHERE id = ?2",
                params![status, v],
            )
            .unwrap();
            assert!(trend_keys(&conn).is_empty(), "{status}");
        }
    }
}
