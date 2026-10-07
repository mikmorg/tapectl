//! Corrected read errors per verify, and their trend per cartridge (issue
//! #421, ADR-0012 2026-10-06 item 12: "No `scrub` command: `volume verify`
//! records the growth of corrected read errors and `audit` flags a
//! cartridge whose rate climbs").
//!
//! A tape whose drive ECC works harder each year still passes every sha256
//! until the day it does not. Page 0x03 (the read error counter page) is
//! where that work shows: errors corrected with and without delay, rereads,
//! uncorrected errors, and the bytes the drive read to produce them. One
//! verify's figures mean little alone; the same cartridge's figures verify
//! over verify are the signal.
//!
//! **What is recorded.** After each completed verify, its contact's
//! journalled page 0x03 (the one post-command sweep, ADR-0013: read once,
//! journalled verbatim) is parsed and its counters, normalised per GiB of
//! the bytes page 0x03 itself says were read, go into one `events` row
//! ([`EVENT_ACTION`]) beside the feed ratio's and for the same reason
//! (ADR-0013 §3: a derived figure is what tapectl said, recorded as an
//! event, never a column beside the facts it comes from; the facts stay in
//! `log_page_journal`).
//!
//! **Scope of the counters.** A log page has no "since" field. On the HP
//! LTO-6 the drive clears pages 0x02/0x03 when a cartridge is loaded (its
//! 2026-09-23 capture, `tests/fixtures/sg_logs/hp_lto6_sg0_fuji_ew7vwmvkf6`:
//! page 0x02 reads 0 bytes processed on a cartridge with 32 GB written in
//! its life, page 0x17's lifetime figure). So the reading taken after a
//! verify covers the reads since that load — the verify's readback, plus
//! any earlier read in the same load. The rate divides each counter by the
//! same page's own byte count, so both sides always share one scope,
//! whatever that scope turns out to be on another drive. ADR-0013 allows
//! one read of a page per contact, so there is no before-the-readback
//! reading to subtract.
//!
//! **What is flagged.** No health formula (ADR-0012 items 11-12: thresholds
//! come from home2's data). One provisional, configurable factor
//! (`[health] read_error_rise_factor`, default [`DEFAULT_RISE_FACTOR`]): a
//! cartridge whose newest verify corrected more errors per GiB than the
//! factor times its previous verify's — and more than [`RISE_FLOOR_PER_GIB`]
//! in all (ADR-0012 2026-10-07 item 7) — is flagged by `audit` and `report
//! health`, with the remedy — copy it to a fresh cartridge.

use rusqlite::{Connection, OptionalExtension};
use tracing::warn;

use crate::db::events;
use crate::error::Result;
use crate::policy::coverage;
use crate::store::Tier;
use crate::tape::health::parse_sg_logs_page;

/// The `events.action` one verify's read-error figures are recorded under.
pub const EVENT_ACTION: &str = "verify_read_errors";
/// Its `events.field`: `new_value` is the corrected errors per GiB read.
pub const EVENT_FIELD: &str = "corrected_per_gib";

/// The provisional rise factor (issue #421): flag a cartridge whose
/// corrected-error rate more than doubled since its previous verify. A
/// starting point to be replaced from home2's recorded verifies, not a
/// measured threshold.
pub const DEFAULT_RISE_FACTOR: f64 = 2.0;

/// The absolute floor under a rise (ADR-0012 2026-10-07 item 7): a
/// cartridge is flagged as rising only when its newest rate is also above
/// this many corrected errors per GiB read. Without it, any first non-zero
/// reading after a zero one is a rise past every factor; with it, a
/// cartridge correcting under one error per GiB is not an alarm whatever
/// its previous verify read. Fixed, not a config key: the factor is the
/// provisional, tunable half.
pub const RISE_FLOOR_PER_GIB: f64 = 1.0;

/// Cartridge statuses whose trend is no longer reported: the operator has
/// retired it, or its last live volume is gone and it waits to be erased.
/// Neither will be verified again, so a rise on it could never clear.
pub const OUT_OF_SERVICE_CARTRIDGE: &[&str] = &["retired_permanent", "pending_erase"];

const GIB: f64 = (1u64 << 30) as f64;

/// Page 0x03's counters, as one verify's sweep read them.
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
    pub bytes_processed: i64,
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
            bytes_processed: p.total_bytes_processed,
        }
    }

    /// Errors the drive corrected: `Total errors corrected`, or the sum of
    /// the with- and without-delay counters where the drive leaves the
    /// total at 0.
    pub fn corrected(&self) -> i64 {
        self.total_corrected
            .max(self.corrected_no_delay + self.corrected_with_delay)
    }

    /// GiB the page says were read.
    pub fn gib_read(&self) -> f64 {
        self.bytes_processed.max(0) as f64 / GIB
    }

    /// `count` per GiB read; `None` when the page counted no bytes (mhvtl
    /// reports 0, and a rate over nothing is not a rate).
    pub fn per_gib(&self, count: i64) -> Option<f64> {
        (self.bytes_processed > 0).then(|| count as f64 / self.gib_read())
    }

    /// Corrected errors per GiB read — the trended figure.
    pub fn corrected_per_gib(&self) -> Option<f64> {
        self.per_gib(self.corrected())
    }
}

/// The newest successful page 0x03 decode journalled through `contact_id`.
fn journalled_0x03(conn: &Connection, contact_id: i64) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT decoded FROM log_page_journal
              WHERE contact_id = ?1 AND page_code = 0x03 AND subpage_code = 0
                AND ok = 1 AND decoded IS NOT NULL
              ORDER BY id DESC LIMIT 1",
            [contact_id],
            |r| r.get(0),
        )
        .optional()?)
}

/// Record one completed verify's read-error figures: the counters its
/// contact's sweep journalled on page 0x03, each per GiB read, as an
/// `events` row naming the verification session, the contact, and the
/// cartridge and drive the contact names. Called by `volume verify` after
/// its post-command sweep, for a FULL verify that made a session: a
/// `--quick` verify reads a few MB, and a rate over that is no sample of the
/// medium (the review of #421). `tier` goes into the details by the name
/// `verification_sessions.verify_type` gives it, and [`trends`] reads only
/// full verifies whatever was recorded.
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
) -> Option<ReadErrors> {
    let contact_id = contact_id?;
    let decoded = match journalled_0x03(conn, contact_id) {
        Ok(d) => d?,
        Err(e) => {
            warn!(err = %e, contact_id, "log_page_journal read for page 0x03 failed");
            return None;
        }
    };
    let errors = ReadErrors::from_decoded_0x03(&decoded);
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
    let details = serde_json::json!({
        "session_id": session_id,
        "tier": match tier {
            Tier::Integrity => "full",
            Tier::Navigable => "quick",
        },
        "contact_id": contact_id,
        "cartridge_id": cartridge_id,
        "drive_serial": drive_serial,
        "bytes_processed": errors.bytes_processed,
        "corrected": errors.corrected(),
        "corrected_no_delay": errors.corrected_no_delay,
        "corrected_with_delay": errors.corrected_with_delay,
        "total_corrected": errors.total_corrected,
        "rereads": errors.rereads,
        "uncorrected": errors.uncorrected,
        "corrected_per_gib": errors.corrected_per_gib(),
        "rereads_per_gib": errors.per_gib(errors.rereads),
        "uncorrected_per_gib": errors.per_gib(errors.uncorrected),
        "scope": "page 0x03 since the drive last cleared it (the HP LTO-6 clears it at load)",
    });
    if let Err(e) = events::log_event(
        conn,
        "volume",
        volume_id,
        Some(label),
        EVENT_ACTION,
        Some(EVENT_FIELD),
        None,
        errors
            .corrected_per_gib()
            .map(|r| format!("{r:.6}"))
            .as_deref(),
        Some(&details.to_string()),
        None,
    ) {
        warn!(err = %e, "events insert for the verify's read errors failed");
    }
    Some(errors)
}

/// One verify's recorded figures, for a trend.
#[derive(Debug, Clone, PartialEq)]
pub struct TrendPoint {
    /// When the verify's figures were recorded.
    pub at: String,
    pub volume: String,
    pub drive_serial: Option<String>,
    pub gib_read: f64,
    pub corrected: i64,
    pub uncorrected: i64,
    pub corrected_per_gib: Option<f64>,
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
    pub fn rising(&self, factor: f64) -> Option<Rise> {
        let mut rated = self.points.iter().filter_map(|p| p.corrected_per_gib).rev();
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
            .map(|p| {
                p.corrected_per_gib
                    .map(|r| format!("{r:.3}"))
                    .unwrap_or_else(|| "-".into())
            })
            .collect();
        let uncorrected: Vec<String> = self
            .points
            .iter()
            .map(|p| p.uncorrected.to_string())
            .collect();
        format!(
            "{} corrected/GiB over {} verif{} (uncorrected {})",
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
        let point = TrendPoint {
            at,
            drive_serial: d["drive_serial"].as_str().map(str::to_string),
            gib_read: d["bytes_processed"].as_i64().unwrap_or(0).max(0) as f64 / GIB,
            corrected: d["corrected"].as_i64().unwrap_or(0),
            uncorrected: d["uncorrected"].as_i64().unwrap_or(0),
            corrected_per_gib: d["corrected_per_gib"].as_f64(),
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

    /// A registered cartridge `barcode` holding a sealed volume
    /// `V-<barcode>`, verified once per entry of `corrected` through the
    /// route `volume verify` takes: a contact, its journalled page 0x03
    /// (that many errors corrected over 1 GiB read), a passed full
    /// verification session, then [`record_for_verify`].
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
    /// entry of `corrected` as [`seed_verifies`] does. Returns its id.
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

    /// One verify of `vol` at `tier`: its contact (on `cart`, or none), the
    /// page 0x03 its sweep journalled (`n` corrected over 1 GiB), its
    /// session, and [`record_for_verify`] — called whatever the tier, so a
    /// test can put a quick verify's figures in front of [`trends`].
    fn seed_one_verify(conn: &Connection, cart: Option<i64>, vol: i64, n: i64, tier: Tier) {
        let label: String = conn
            .query_row("SELECT label FROM volumes WHERE id = ?1", [vol], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute(
            "INSERT INTO cartridge_contacts (cartridge_id, volume_id, operation, device)
             VALUES (?1, ?2, 'volume verify', '/dev/nst0')",
            params![cart, vol],
        )
        .unwrap();
        let contact = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO log_page_journal (contact_id, device_sg, trigger, page_code, ok,
                 tool_argv, decoded, tapectl_version)
             VALUES (?1, '/dev/sg0', 'volume verify', 3, 1, '[]', ?2, 't')",
            params![contact, page_0x03(n, 0, 0, 1 << 30)],
        )
        .unwrap();
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
        let session = conn.last_insert_rowid();
        record_for_verify(conn, Some(contact), session, vol, &label, tier)
            .expect("the journalled page 0x03 is recorded");
    }

    /// A page 0x03 decode with chosen counters, in sg_logs's own shape.
    pub(crate) fn page_0x03(no_delay: i64, rereads: i64, uncorrected: i64, bytes: i64) -> String {
        format!(
            "Read error counter page  [0x3]\n  Errors corrected without substantial delay = \
             {no_delay}\n  Errors corrected with possible delays = 0\n  Total rewrites or \
             rereads = {rereads}\n  Total errors corrected = 0\n  Total times correction \
             algorithm processed = {no_delay}\n  Total bytes processed = {bytes}\n  Total \
             uncorrected errors = {uncorrected}\n"
        )
    }

    #[test]
    fn the_hp_shape_counts_corrected_errors_the_total_leaves_at_zero() {
        let e = ReadErrors::from_decoded_0x03(&page_0x03(40, 3, 0, 4 << 30));
        assert_eq!(e.total_corrected, 0, "the HP leaves the total at 0");
        assert_eq!(e.corrected(), 40);
        assert_eq!(e.corrected_per_gib(), Some(10.0));
        assert_eq!(e.per_gib(e.rereads), Some(0.75));
    }

    #[test]
    fn no_bytes_read_is_no_rate() {
        let fixture = include_str!("../../tests/fixtures/sg_logs/hp_lto6_page_0x03.txt");
        let e = ReadErrors::from_decoded_0x03(fixture);
        assert_eq!(e, ReadErrors::default(), "the real all-zero page");
        assert_eq!(e.corrected_per_gib(), None);
    }

    fn trend(rates: &[Option<f64>]) -> CartridgeTrend {
        CartridgeTrend {
            cartridge: "C1".into(),
            points: rates
                .iter()
                .map(|r| TrendPoint {
                    at: "t".into(),
                    volume: "V".into(),
                    drive_serial: None,
                    gib_read: 1.0,
                    corrected: 0,
                    uncorrected: 0,
                    corrected_per_gib: *r,
                })
                .collect(),
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
        // An unrated verify (no bytes counted) is skipped, not read as 0.
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
