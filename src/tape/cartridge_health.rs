//! Per-cartridge health, for the whole fleet (issue #307): which of my
//! tapes is worst.
//!
//! A cartridge is the thing that wears out, and until this every health
//! record was read by volume or by drive. Everything here is already in the
//! catalog; this module only reads it, cartridge by cartridge, and ranks:
//!
//! - **contacts** — `cartridge_contacts` (migration 020): how many times it
//!   was in a drive and the last time, which `cartridges.last_use` is not
//!   (only a bind sets that — a verify or a restore never did);
//! - **readings** — `health_logs` through each reading's contact
//!   ([`health::readings_by_cartridge`], ADR-0013 §2);
//! - **TapeAlerts** — the flags raised on its contacts, by class
//!   ([`crate::tape::alert_flags`], issue #303). Only a MEDIUM flag counts
//!   against the cartridge; a drive or cleaning flag raised while it was
//!   loaded is named, but it is the drive's;
//! - **verifies** — each volume it carries whose most recent completed
//!   verify failed;
//! - **quarantines** — its volumes at `observed_condition = 'quarantined'`,
//!   split by the event that set it: medium evidence (a verify's, or a
//!   write's failed confirm) counts against the cartridge; a session
//!   divergence (a resume finding another identity, or an existing seal) is
//!   not a fact about the medium (migration 017) and is only named;
//! - **read errors** — a rising corrected-error rate (issue #421).
//!
//! # Three verdicts, and "no evidence" is one of them
//!
//! `attention` names its reasons. `no_evidence` is a cartridge with no
//! health reading and no verify: unexamined, not healthy — the #293 lesson,
//! a report about risk must not pass "never looked" off as "fine". `clean`
//! is evidence and nothing adverse in it. Every registered cartridge
//! appears, whatever it has: the list is driven from `cartridges`.
//!
//! # What it does not read
//!
//! `cartridges.total_bytes_written`/`total_bytes_read`/`first_use`/
//! `error_history` and `verification_results.read_errors_*` have no writer
//! and default to 0 or NULL: a report that read them would assert numbers
//! nobody observed. No threshold either — ADR-0012 sets none; the reasons
//! are facts the hardware or a verify reported.

use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::error::Result;
use crate::tape::alert_flags::{self, AlertClass};
use crate::tape::health;

/// The prefix `write::describe_quarantine` gives a write's failed confirm —
/// the one write-path quarantine that is medium evidence. Pinned by
/// [`tests::a_failed_confirm_quarantine_reads_as_medium_evidence`].
const CONFIRM_FAILED_PREFIX: &str = "confirm chain-walk";

/// A cartridge's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Something adverse was reported; each string says what.
    Attention(Vec<String>),
    /// No health reading and no verify: never examined.
    NoEvidence,
    /// Examined, and nothing adverse.
    Clean,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Attention(_) => "attention",
            Verdict::NoEvidence => "no_evidence",
            Verdict::Clean => "clean",
        }
    }

    /// Rank: attention first (most reasons first), then the unexamined,
    /// then the clean.
    fn rank(&self) -> (u8, std::cmp::Reverse<usize>) {
        match self {
            Verdict::Attention(r) => (0, std::cmp::Reverse(r.len())),
            Verdict::NoEvidence => (1, std::cmp::Reverse(0)),
            Verdict::Clean => (2, std::cmp::Reverse(0)),
        }
    }
}

/// The most recent contact with a cartridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastContact {
    pub at: String,
    pub operation: String,
    /// `None`: the contact never closed (a crash), or is still open.
    pub outcome: Option<String>,
}

/// One cartridge's health, as the fleet report shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct CartridgeHealth {
    pub barcode: String,
    pub status: String,
    /// The chip's load count; `None` is unknown (migration 015).
    pub loads: Option<i64>,
    /// `cartridges.last_use`: the last BIND, not the last use.
    pub last_bind: Option<String>,
    pub contacts: i64,
    pub last_contact: Option<LastContact>,
    pub readings: i64,
    pub max_uncorrected: Option<i64>,
    /// Readings whose TapeAlert page was not read (NULL `tape_alerts`).
    pub readings_alerts_unrecorded: i64,
    /// Medium-class flags raised on its contacts: `(code, name)`, distinct.
    pub medium_alerts: Vec<(u8, String)>,
    /// Drive- or cleaning-class flags raised while it was loaded:
    /// `(code, name)`, distinct. The drive's, not the cartridge's.
    pub drive_alerts: Vec<(u8, String)>,
    /// Full and quick verifies of its volumes.
    pub verifies: i64,
    /// Its volumes whose most recent completed verify failed.
    pub failed_verifies: Vec<String>,
    /// Its volumes quarantined on medium evidence.
    pub medium_quarantines: Vec<String>,
    /// Its volumes quarantined by a session finding, not the medium.
    pub session_quarantines: Vec<String>,
    /// The read-error trend's rise message, when its rate is rising.
    pub read_errors_rising: Option<String>,
    pub verdict: Verdict,
}

/// Every registered cartridge's health, worst first.
pub fn fleet(conn: &Connection, rise_factor: f64) -> Result<Vec<CartridgeHealth>> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.barcode, c.status, c.total_load_count, c.last_use,
                (SELECT COUNT(*) FROM cartridge_contacts cc WHERE cc.cartridge_id = c.id),
                (SELECT COUNT(*) FROM verification_sessions s
                   JOIN cartridge_volumes cv ON cv.volume_id = s.volume_id
                  WHERE cv.cartridge_id = c.id)
           FROM cartridges c
          ORDER BY c.barcode",
    )?;
    #[allow(clippy::type_complexity)]
    let base: Vec<(i64, String, String, Option<i64>, Option<String>, i64, i64)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let readings: BTreeMap<i64, health::CartridgeReadings> = health::readings_by_cartridge(conn)?
        .into_iter()
        .map(|r| (r.cartridge_id, r))
        .collect();
    #[allow(clippy::type_complexity)]
    let mut alerts: BTreeMap<String, (Vec<(u8, String)>, Vec<(u8, String)>)> = BTreeMap::new();
    for s in crate::cli::report::tape_alert_sightings(conn)? {
        let Some(barcode) = s.cartridge_barcode.clone() else {
            continue;
        };
        let entry = alerts.entry(barcode).or_default();
        for flag in &s.flags {
            let into = match alert_flags::class_of(u16::from(flag.0)) {
                AlertClass::Medium => &mut entry.0,
                AlertClass::Drive | AlertClass::Cleaning => &mut entry.1,
                AlertClass::Other => continue,
            };
            if !into.contains(flag) {
                into.push(flag.clone());
            }
        }
    }
    let rising: BTreeMap<String, String> = crate::tape::read_errors::trends(conn)?
        .iter()
        .filter_map(|t| {
            t.rising(rise_factor).map(|rise| {
                (
                    t.cartridge.clone(),
                    crate::tape::read_errors::rise_message(t, rise, rise_factor),
                )
            })
        })
        .collect();

    let mut out = Vec::with_capacity(base.len());
    for (id, barcode, status, loads, last_bind, contacts, verifies) in base {
        let r = readings.get(&id);
        let (medium_alerts, drive_alerts) = alerts.remove(&barcode).unwrap_or_default();
        let (medium_quarantines, session_quarantines) = quarantines(conn, id)?;
        let mut h = CartridgeHealth {
            last_contact: last_contact(conn, id)?,
            failed_verifies: failed_verifies(conn, id)?,
            read_errors_rising: rising.get(&barcode).cloned(),
            barcode,
            status,
            loads,
            last_bind,
            contacts,
            readings: r.map(|r| r.readings).unwrap_or(0),
            max_uncorrected: r.and_then(|r| r.max_uncorrected),
            readings_alerts_unrecorded: r.map(|r| r.readings_alerts_unrecorded).unwrap_or(0),
            medium_alerts,
            drive_alerts,
            verifies,
            medium_quarantines,
            session_quarantines,
            verdict: Verdict::Clean,
        };
        h.verdict = verdict(&h);
        out.push(h);
    }
    out.sort_by(|a, b| {
        a.verdict
            .rank()
            .cmp(&b.verdict.rank())
            .then_with(|| a.barcode.cmp(&b.barcode))
    });
    Ok(out)
}

/// The verdict from the gathered facts.
fn verdict(h: &CartridgeHealth) -> Verdict {
    let mut reasons = Vec::new();
    for (code, name) in &h.medium_alerts {
        reasons.push(format!("TapeAlert {code} ({name}) raised"));
    }
    if let Some(n) = h.max_uncorrected.filter(|n| *n > 0) {
        reasons.push(format!(
            "a reading on one of its contacts counted {n} uncorrected error(s)"
        ));
    }
    for label in &h.medium_quarantines {
        reasons.push(format!("volume {label} quarantined on medium evidence"));
    }
    for label in &h.failed_verifies {
        reasons.push(format!("the last verify of volume {label} failed"));
    }
    if let Some(msg) = &h.read_errors_rising {
        reasons.push(format!("read errors rising: {msg}"));
    }
    if !reasons.is_empty() {
        Verdict::Attention(reasons)
    } else if h.readings == 0 && h.verifies == 0 {
        Verdict::NoEvidence
    } else {
        Verdict::Clean
    }
}

/// The most recent contact with cartridge `id` (`cartridge_contacts`,
/// migration 020), or `None` when it never had one recorded.
pub fn last_contact(conn: &Connection, id: i64) -> Result<Option<LastContact>> {
    use rusqlite::OptionalExtension;
    Ok(conn
        .query_row(
            "SELECT opened_at, operation, outcome FROM cartridge_contacts
              WHERE cartridge_id = ?1 ORDER BY opened_at DESC, id DESC LIMIT 1",
            [id],
            |r| {
                Ok(LastContact {
                    at: r.get(0)?,
                    operation: r.get(1)?,
                    outcome: r.get(2)?,
                })
            },
        )
        .optional()?)
}

/// Volumes on cartridge `id` whose most recent completed verify failed.
fn failed_verifies(conn: &Connection, id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT v.label FROM cartridge_volumes cv
           JOIN volumes v ON v.id = cv.volume_id
          WHERE cv.cartridge_id = ?1
            AND (SELECT s.outcome FROM verification_sessions s
                  WHERE s.volume_id = v.id AND s.completed_at IS NOT NULL
                  ORDER BY s.completed_at DESC, s.id DESC LIMIT 1) = 'failed'
          ORDER BY v.label",
    )?;
    let rows = stmt
        .query_map([id], |r| r.get(0))?
        .collect::<std::result::Result<Vec<String>, _>>()?;
    Ok(rows)
}

/// Quarantined volumes on cartridge `id`, split into medium evidence and
/// session findings by the most recent event that quarantined each.
fn quarantines(conn: &Connection, id: i64) -> Result<(Vec<String>, Vec<String>)> {
    let mut stmt = conn.prepare(
        "SELECT v.label,
                (SELECT e.action || char(31) || COALESCE(e.new_value, '')
                   FROM events e
                  WHERE e.entity_type = 'volume' AND e.entity_id = v.id
                    AND e.action IN ('verify_quarantined', 'write_quarantined')
                  ORDER BY e.id DESC LIMIT 1)
           FROM cartridge_volumes cv
           JOIN volumes v ON v.id = cv.volume_id
          WHERE cv.cartridge_id = ?1 AND v.observed_condition = 'quarantined'
          ORDER BY v.label",
    )?;
    let rows = stmt
        .query_map([id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let (mut medium, mut session) = (Vec::new(), Vec::new());
    for (label, event) in rows {
        let is_medium = match event.as_deref().and_then(|e| e.split_once('\u{1f}')) {
            Some(("verify_quarantined", _)) => true,
            Some(("write_quarantined", reason)) => reason.starts_with(CONFIRM_FAILED_PREFIX),
            // No event says why: counted against the medium, the cautious
            // side — a quarantine with no recorded reason is still one.
            _ => true,
        };
        if is_medium {
            medium.push(label);
        } else {
            session.push(label);
        }
    }
    Ok((medium, session))
}

/// The report's lines for one cartridge: a summary line, then one indented
/// line per reason or note.
pub fn render(h: &CartridgeHealth) -> Vec<String> {
    let last = h
        .last_contact
        .as_ref()
        .map(|c| {
            format!(
                "{} ({}, {})",
                c.at,
                c.operation,
                c.outcome.as_deref().unwrap_or("did not close")
            )
        })
        .unwrap_or_else(|| "none recorded".to_string());
    let mut lines = vec![format!(
        "{} [{}] {}: loads={} contacts={} last contact {} readings={} verifies={}",
        h.barcode,
        h.status,
        h.verdict.as_str().to_uppercase(),
        h.loads
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".into()),
        h.contacts,
        last,
        h.readings,
        h.verifies,
    )];
    match &h.verdict {
        Verdict::Attention(reasons) => {
            lines.extend(reasons.iter().map(|r| format!("    - {r}")));
        }
        Verdict::NoEvidence => lines.push(
            "    - never examined: no health reading and no verify on record — run \
             `tapectl volume verify` on a volume it carries"
                .to_string(),
        ),
        Verdict::Clean => {}
    }
    for (code, name) in &h.drive_alerts {
        lines.push(format!(
            "    note: TapeAlert {code} ({name}) was raised while it was loaded — a drive \
             or cleaning flag, the drive's, not the cartridge's"
        ));
    }
    for label in &h.session_quarantines {
        lines.push(format!(
            "    note: volume {label} is quarantined by a write-session finding, not by \
             evidence about the medium"
        ));
    }
    if h.readings_alerts_unrecorded > 0 {
        lines.push(format!(
            "    note: {} reading(s) did not read the TapeAlert page",
            h.readings_alerts_unrecorded
        ));
    }
    lines
}

/// `report cartridge-health --json`: one object per cartridge, every
/// registered cartridge present. NULL stays `null`.
pub fn to_json(h: &CartridgeHealth) -> serde_json::Value {
    let flags = |v: &[(u8, String)]| {
        v.iter()
            .map(|(c, n)| serde_json::json!({"code": c, "name": n}))
            .collect::<Vec<_>>()
    };
    serde_json::json!({
        "barcode": h.barcode,
        "status": h.status,
        "verdict": h.verdict.as_str(),
        "reasons": match &h.verdict {
            Verdict::Attention(r) => r.clone(),
            _ => Vec::new(),
        },
        "loads": h.loads,
        "last_bind": h.last_bind,
        "contacts": h.contacts,
        "last_contact": h.last_contact.as_ref().map(|c| serde_json::json!({
            "at": c.at, "operation": c.operation, "outcome": c.outcome,
        })),
        "readings": h.readings,
        "max_uncorrected": h.max_uncorrected,
        "readings_alerts_unrecorded": h.readings_alerts_unrecorded,
        "medium_alerts": flags(&h.medium_alerts),
        "drive_alerts_while_loaded": flags(&h.drive_alerts),
        "verifies": h.verifies,
        "failed_verifies": h.failed_verifies,
        "medium_quarantines": h.medium_quarantines,
        "session_quarantines": h.session_quarantines,
        "read_errors_rising": h.read_errors_rising,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three cartridges:
    /// - `A-BAD` carries `V-A` (and `V-A2`): readings with a medium TapeAlert
    ///   (19, Nearing media life) and an uncorrected count, a failed verify,
    ///   a medium-evidence quarantine — and the never-written wear columns
    ///   seeded with sentinels that must never reach the output;
    /// - `B-NEW` is registered with nothing at all;
    /// - `C-OK` carries `V-C`: one clean reading and a passed verify, plus a
    ///   session-finding quarantine on `V-C2`, which must not count.
    fn seed() -> Connection {
        let conn = crate::db::open_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO drives (id, serial) VALUES (1, 'DRV1');
             INSERT INTO cartridges (id, barcode, media_type, nominal_capacity, status,
                                     total_load_count, last_use, total_bytes_written,
                                     total_bytes_read, first_use, error_history)
             VALUES (1, 'A-BAD', 'LTO-6', 2500000000000, 'in_use', 14,
                     '2026-09-01 00:00:00', 987654321, 987654322, '1999-01-01 00:00:00',
                     'SENTINEL-ERROR-HISTORY'),
                    (2, 'B-NEW', 'LTO-6', 2500000000000, 'available', NULL, NULL, 0, 0, NULL,
                     NULL),
                    (3, 'C-OK', 'LTO-6', 2500000000000, 'in_use', 3, NULL, 0, 0, NULL, NULL);
             INSERT INTO volumes (id, label, backend_type, backend_name, media_type,
                                  capacity_bytes, status, observed_condition)
             VALUES (10, 'V-A', 'lto', 'p', 'LTO-6', 1, 'sealed', 'quarantined'),
                    (11, 'V-A2', 'lto', 'p', 'LTO-6', 1, 'sealed', 'ok'),
                    (12, 'V-C', 'lto', 'p', 'LTO-6', 1, 'sealed', 'ok'),
                    (13, 'V-C2', 'lto', 'p', 'LTO-6', 1, 'initialized', 'quarantined');
             INSERT INTO cartridge_volumes (cartridge_id, volume_id, mounted_at, unmounted_at)
             VALUES (1, 10, '2026-09-01 00:00:00', '2026-09-05 00:00:00'),
                    (1, 11, '2026-09-05 00:00:00', NULL),
                    (3, 12, '2026-09-02 00:00:00', '2026-09-03 00:00:00'),
                    (3, 13, '2026-09-03 00:00:00', NULL);
             INSERT INTO cartridge_contacts (id, cartridge_id, volume_id, drive_id, operation,
                                             device, opened_at, closed_at, outcome)
             VALUES (100, 1, 10, 1, 'volume write', '/dev/nst0', '2026-09-01 01:00:00',
                     '2026-09-01 02:00:00', 'ok'),
                    (101, 1, 10, 1, 'volume verify', '/dev/nst0', '2026-09-04 01:00:00',
                     NULL, NULL),
                    (102, 3, 12, 1, 'volume verify', '/dev/nst0', '2026-09-02 05:00:00',
                     '2026-09-02 06:00:00', 'ok');
             INSERT INTO health_logs (volume_id, contact_id, operation, total_uncorrected,
                                      tape_alerts)
             VALUES (10, 100, 'write', 0, 0),
                    (10, 101, 'verify', 2, 1),
                    (12, 102, 'verify', 0, 0);
             INSERT INTO verification_sessions (volume_id, started_at, completed_at,
                                                verify_type, outcome)
             VALUES (10, '2026-09-04 01:00:00', '2026-09-04 03:00:00', 'full', 'failed'),
                    (12, '2026-09-02 05:00:00', '2026-09-02 06:00:00', 'full', 'passed');
             INSERT INTO events (entity_type, entity_id, entity_label, action, field,
                                 old_value, new_value, details)
             VALUES ('volume', 10, 'V-A', 'verify_quarantined', 'observed_condition', 'ok',
                     'quarantined', 'content hash mismatch at position 9'),
                    ('volume', 13, 'V-C2', 'write_quarantined', NULL, NULL,
                     'identity mismatch: expected label=\"V-C2\"', NULL);",
        )
        .unwrap();
        // Contact 101 journalled page 0x2E with flag 19 raised (the real HP
        // bytes, one value byte set).
        let mut raw = include_bytes!(
            "../../tests/fixtures/sg_logs/hp_lto6_sg0_fuji_ew7vwmvkf6/page_0x2e.bin"
        )
        .to_vec();
        raw[4 + 18 * 5 + 4] = 1;
        conn.execute(
            "INSERT INTO log_page_journal (contact_id, device_sg, trigger, page_code, ok,
                                           tool_argv, raw, tapectl_version)
             VALUES (101, '/dev/sg0', 'volume verify', 46, 1, '[]', ?1, 'test')",
            [raw],
        )
        .unwrap();
        conn
    }

    /// #307's acceptance: every registered cartridge appears; A shows its
    /// alert, uncorrected count, failed verify and quarantine; B is present
    /// with an explicit no-evidence verdict; C is clean despite a session
    /// quarantine. Worst first.
    #[test]
    fn the_fleet_ranks_every_cartridge_and_names_why() {
        let conn = seed();
        let fleet = fleet(&conn, 2.0).unwrap();
        let order: Vec<(&str, &str)> = fleet
            .iter()
            .map(|h| (h.barcode.as_str(), h.verdict.as_str()))
            .collect();
        assert_eq!(
            order,
            vec![
                ("A-BAD", "attention"),
                ("B-NEW", "no_evidence"),
                ("C-OK", "clean")
            ]
        );
        let a = &fleet[0];
        assert_eq!(
            a.medium_alerts,
            vec![(19, "Nearing media life".to_string())]
        );
        assert_eq!(a.max_uncorrected, Some(2));
        assert_eq!(a.failed_verifies, vec!["V-A".to_string()]);
        assert_eq!(a.medium_quarantines, vec!["V-A".to_string()]);
        assert_eq!(a.contacts, 2);
        assert_eq!(
            a.last_contact,
            Some(LastContact {
                at: "2026-09-04 01:00:00".into(),
                operation: "volume verify".into(),
                outcome: None
            })
        );
        let text = render(a).join("\n");
        assert!(
            text.contains("TapeAlert 19 (Nearing media life) raised"),
            "{text}"
        );
        assert!(text.contains("2 uncorrected error(s)"), "{text}");
        assert!(
            text.contains("volume V-A quarantined on medium evidence"),
            "{text}"
        );
        assert!(
            text.contains("the last verify of volume V-A failed"),
            "{text}"
        );
        assert!(text.contains("did not close"), "{text}");

        let b = render(&fleet[1]).join("\n");
        assert!(
            b.starts_with("B-NEW [available] NO_EVIDENCE: loads=unknown contacts=0"),
            "{b}"
        );
        assert!(b.contains("never examined"), "{b}");

        let c = &fleet[2];
        assert!(c.medium_quarantines.is_empty());
        assert_eq!(c.session_quarantines, vec!["V-C2".to_string()]);
        assert!(render(c)
            .join("\n")
            .contains("not by evidence about the medium"));
    }

    /// The never-written wear columns are proven unread by SENTINEL: the
    /// seeded values appear nowhere in the text or the JSON, while what
    /// should appear (the barcode, the load count) does — so "absent"
    /// cannot mean "rendered nothing".
    #[test]
    fn the_confident_zero_columns_never_reach_the_output() {
        let conn = seed();
        let fleet = fleet(&conn, 2.0).unwrap();
        let text: String = fleet.iter().flat_map(render).collect::<Vec<_>>().join("\n");
        let json: String = fleet
            .iter()
            .map(|h| to_json(h).to_string())
            .collect::<Vec<_>>()
            .join("\n");
        for out in [&text, &json] {
            for sentinel in [
                "987654321",
                "987654322",
                "1999-01-01",
                "SENTINEL-ERROR-HISTORY",
            ] {
                assert!(!out.contains(sentinel), "{sentinel} leaked: {out}");
            }
            assert!(out.contains("A-BAD"), "positive control: {out}");
            assert!(
                out.contains("14"),
                "positive control, the load count: {out}"
            );
        }
    }

    /// JSON: every cartridge is a row, a missing load count is `null`, the
    /// verdict and reasons are carried.
    #[test]
    fn the_json_keeps_null_as_null_and_every_cartridge() {
        let conn = seed();
        let rows: Vec<serde_json::Value> = fleet(&conn, 2.0).unwrap().iter().map(to_json).collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1]["barcode"], "B-NEW");
        assert_eq!(rows[1]["loads"], serde_json::Value::Null);
        assert_eq!(rows[1]["verdict"], "no_evidence");
        assert_eq!(rows[1]["last_contact"], serde_json::Value::Null);
        assert_eq!(rows[0]["last_bind"], "2026-09-01 00:00:00");
        assert_eq!(rows[0]["reasons"].as_array().unwrap().len(), 4);
    }

    /// The write-path quarantine that IS medium evidence is told apart by
    /// the reason `describe_quarantine` writes for a failed confirm.
    #[test]
    fn a_failed_confirm_quarantine_reads_as_medium_evidence() {
        let evidence = crate::store::Evidence {
            tier: crate::store::Tier::Integrity,
            files_checked: 1,
            mismatches: Vec::new(),
        };
        let reason = crate::volume::write::describe_quarantine(
            &crate::volume::session::QuarantineReason::ConfirmFailed(evidence),
        );
        assert!(reason.starts_with(CONFIRM_FAILED_PREFIX), "{reason}");
    }
}
