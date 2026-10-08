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
//! - **verifies** — completed verifies of its volumes (`passed` or
//!   `failed`; one still `in_progress`, or `aborted`, read nothing). A
//!   failed verify is not a reason against the cartridge: ADR-0012
//!   (2026-09-17, 2026-09-18) rules that one which did not prove the medium
//!   bad — a drive or transport error, a short read — says nothing about the
//!   medium, and one that did prove it quarantined the volume, which is
//!   counted below. So a volume whose last verify failed and which is NOT
//!   quarantined is named as a note (the drive could not read it), and only
//!   a PASSED verify is evidence that the cartridge was looked at;
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
//! health reading and no verify that read it back: unexamined, not healthy
//! — the #293 lesson, a report about risk must not pass "never looked" off
//! as "fine". `clean` is evidence and nothing adverse in it. Every
//! registered cartridge appears, whatever it has: the list is driven from
//! `cartridges`.
//!
//! # Nothing that names no cartridge is dropped
//!
//! A reading, a TapeAlert or a read-error trend is a cartridge's through the
//! contact that took it, and a contact identifies a cartridge only when a
//! binding or a chip serial said which (every tape written before
//! 2026-09-13, every `MemStore` volume, every contact before
//! identification has none). Those are reported under [`Unattributed`] —
//! with the contact and the volume that reach them — never left out.
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
    /// Completed full and quick verifies of its volumes (`passed` or
    /// `failed`; never one still `in_progress`, or `aborted`).
    pub verifies: i64,
    /// Of those, the ones that passed: the verifies that read it back.
    pub verifies_passed: i64,
    /// Its volumes whose most recent completed verify failed and which are
    /// not quarantined: a failure that did not prove the medium bad (a
    /// drive or transport error, a short read — ADR-0012). A note, never a
    /// reason.
    pub unproven_failed_verifies: Vec<String>,
    /// Its volumes quarantined on medium evidence.
    pub medium_quarantines: Vec<String>,
    /// Its volumes quarantined by a session finding, not the medium.
    pub session_quarantines: Vec<String>,
    /// The read-error trend's rise message, when its rate is rising.
    pub read_errors_rising: Option<String>,
    pub verdict: Verdict,
}

/// The fleet report: every registered cartridge, worst first, and what
/// resolves to none of them.
#[derive(Debug, Clone, PartialEq)]
pub struct Fleet {
    pub cartridges: Vec<CartridgeHealth>,
    pub unattributed: Unattributed,
}

/// Evidence whose contact identified no registered cartridge (issue #307's
/// third case): reported, never dropped.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Unattributed {
    /// `health_logs` rows on no cartridge, oldest first.
    pub readings: Vec<UnattributedReading>,
    /// Sightings of a MEDIUM-class TapeAlert on no cartridge. (A drive or
    /// cleaning flag is the drive's wherever it was raised; `report health`
    /// names those.)
    pub medium_alerts: Vec<UnattributedAlert>,
    /// Rising read-error trends keyed to no cartridge: `(trend key — the
    /// volume, as `volume:<label>` — , rise message)`.
    pub read_errors_rising: Vec<(String, String)>,
}

impl Unattributed {
    pub fn is_empty(&self) -> bool {
        self.readings.is_empty()
            && self.medium_alerts.is_empty()
            && self.read_errors_rising.is_empty()
    }
}

/// One `health_logs` row that names no cartridge.
#[derive(Debug, Clone, PartialEq)]
pub struct UnattributedReading {
    pub id: i64,
    pub logged_at: String,
    pub volume: String,
    /// `None`: a reading from before contacts were recorded (migration 020).
    pub contact_id: Option<i64>,
    pub operation: String,
    pub total_uncorrected: Option<i64>,
    pub tape_alerts: Option<i64>,
}

/// One medium TapeAlert sighting that names no cartridge.
#[derive(Debug, Clone, PartialEq)]
pub struct UnattributedAlert {
    pub contact_id: Option<i64>,
    pub at: String,
    /// The contact's volume, when it recorded one.
    pub volume: Option<String>,
    /// The medium-class flags raised: `(code, name)`.
    pub flags: Vec<(u8, String)>,
}

/// Every registered cartridge's health, worst first, and the evidence that
/// resolves to none of them.
pub fn fleet(conn: &Connection, rise_factor: f64) -> Result<Fleet> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.barcode, c.status, c.total_load_count, c.last_use,
                (SELECT COUNT(*) FROM cartridge_contacts cc WHERE cc.cartridge_id = c.id),
                (SELECT COUNT(*) FROM verification_sessions s
                   JOIN cartridge_volumes cv ON cv.volume_id = s.volume_id
                  WHERE cv.cartridge_id = c.id AND s.completed_at IS NOT NULL
                    AND s.outcome IN ('passed', 'failed')),
                (SELECT COUNT(*) FROM verification_sessions s
                   JOIN cartridge_volumes cv ON cv.volume_id = s.volume_id
                  WHERE cv.cartridge_id = c.id AND s.completed_at IS NOT NULL
                    AND s.outcome = 'passed')
           FROM cartridges c
          ORDER BY c.barcode",
    )?;
    #[allow(clippy::type_complexity)]
    let base: Vec<(
        i64,
        String,
        String,
        Option<i64>,
        Option<String>,
        i64,
        i64,
        i64,
    )> = stmt
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
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut unattributed = Unattributed {
        readings: unattributed_readings(conn)?,
        ..Unattributed::default()
    };
    let readings: BTreeMap<i64, health::CartridgeReadings> = health::readings_by_cartridge(conn)?
        .into_iter()
        .map(|r| (r.cartridge_id, r))
        .collect();
    #[allow(clippy::type_complexity)]
    let mut alerts: BTreeMap<String, (Vec<(u8, String)>, Vec<(u8, String)>)> = BTreeMap::new();
    for s in crate::cli::report::tape_alert_sightings(conn)? {
        let Some(barcode) = s.cartridge_barcode.clone() else {
            let flags: Vec<(u8, String)> = s
                .flags
                .iter()
                .filter(|f| alert_flags::class_of(u16::from(f.0)) == AlertClass::Medium)
                .cloned()
                .collect();
            if !flags.is_empty() {
                unattributed.medium_alerts.push(UnattributedAlert {
                    volume: contact_volume(conn, s.contact_id)?,
                    contact_id: s.contact_id,
                    at: s.at.clone(),
                    flags,
                });
            }
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
    let mut rising: BTreeMap<String, String> = crate::tape::read_errors::trends(conn)?
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
    for (id, barcode, status, loads, last_bind, contacts, verifies, verifies_passed) in base {
        let r = readings.get(&id);
        let (medium_alerts, drive_alerts) = alerts.remove(&barcode).unwrap_or_default();
        let (medium_quarantines, session_quarantines) = quarantines(conn, id)?;
        let mut h = CartridgeHealth {
            last_contact: last_contact(conn, id)?,
            unproven_failed_verifies: unproven_failed_verifies(conn, id, &medium_quarantines)?,
            read_errors_rising: rising.remove(&barcode),
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
            verifies_passed,
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
    // What no registered cartridge took: a trend keyed to a volume (its
    // verify's contact identified no cartridge).
    unattributed.read_errors_rising = rising.into_iter().collect();
    Ok(Fleet {
        cartridges: out,
        unattributed,
    })
}

/// `health_logs` rows that name no registered cartridge: no contact, or a
/// contact that identified none. The complement of
/// [`health::readings_by_cartridge`]'s join.
fn unattributed_readings(conn: &Connection) -> Result<Vec<UnattributedReading>> {
    let mut stmt = conn.prepare(
        "SELECT h.id, h.logged_at, COALESCE(v.label, '?'), h.contact_id, h.operation,
                h.total_uncorrected, h.tape_alerts
           FROM health_logs h
           LEFT JOIN volumes v ON v.id = h.volume_id
           LEFT JOIN cartridge_contacts cc ON cc.id = h.contact_id
           LEFT JOIN cartridges c ON c.id = cc.cartridge_id
          WHERE c.id IS NULL
          ORDER BY h.logged_at, h.id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(UnattributedReading {
                id: r.get(0)?,
                logged_at: r.get(1)?,
                volume: r.get(2)?,
                contact_id: r.get(3)?,
                operation: r.get(4)?,
                total_uncorrected: r.get(5)?,
                tape_alerts: r.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The label of the volume contact `id` recorded, if any.
fn contact_volume(conn: &Connection, id: Option<i64>) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    let Some(id) = id else {
        return Ok(None);
    };
    Ok(conn
        .query_row(
            "SELECT v.label FROM cartridge_contacts cc JOIN volumes v ON v.id = cc.volume_id
              WHERE cc.id = ?1",
            [id],
            |r| r.get(0),
        )
        .optional()?)
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
    if let Some(msg) = &h.read_errors_rising {
        reasons.push(format!("read errors rising: {msg}"));
    }
    if !reasons.is_empty() {
        Verdict::Attention(reasons)
    } else if h.readings == 0 && h.verifies_passed == 0 {
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

/// Volumes on cartridge `id` whose most recent completed verify failed and
/// which are not quarantined on medium evidence (`medium_quarantines`). A
/// verify that proved the medium bad quarantines the volume (ADR-0012,
/// 2026-09-17), so a failed last verify on any other volume — including one
/// a write-session finding quarantined — is, by construction, one that did
/// not: the drive could not read it.
fn unproven_failed_verifies(
    conn: &Connection,
    id: i64,
    medium_quarantines: &[String],
) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT v.label FROM cartridge_volumes cv
           JOIN volumes v ON v.id = cv.volume_id
          WHERE cv.cartridge_id = ?1
            AND (SELECT s.outcome FROM verification_sessions s
                  WHERE s.volume_id = v.id AND s.completed_at IS NOT NULL
                    AND s.outcome IN ('passed', 'failed')
                  ORDER BY s.completed_at DESC, s.id DESC LIMIT 1) = 'failed'
          ORDER BY v.label",
    )?;
    let rows = stmt
        .query_map([id], |r| r.get(0))?
        .collect::<std::result::Result<Vec<String>, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|label| !medium_quarantines.contains(label))
        .collect())
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
            "    - never examined: no health reading and no verify that read it back on \
             record — run `tapectl volume verify` on a volume it carries"
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
    for label in &h.unproven_failed_verifies {
        lines.push(format!(
            "    note: volume {label}: its last verify failed without proving the medium bad \
             — the drive could not read it, which says nothing about the cartridge"
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

/// The report's closing section: what resolves to no registered cartridge.
/// Always printed — "none" is said, not left to an absent section.
pub fn render_unattributed(u: &Unattributed) -> Vec<String> {
    if u.is_empty() {
        return vec![
            "unattributed: none — every reading, medium TapeAlert and read-error trend \
             resolves to a registered cartridge"
                .to_string(),
        ];
    }
    let mut lines = vec![format!(
        "unattributed — evidence whose contact identified no registered cartridge \
         (a tape written before 2026-09-13, or never bound): {} reading(s), {} medium \
         TapeAlert sighting(s), {} rising read-error trend(s)",
        u.readings.len(),
        u.medium_alerts.len(),
        u.read_errors_rising.len()
    )];
    // Readings, one line per volume: how many, which contacts, the worst
    // uncorrected count and how many raised a TapeAlert.
    let mut by_volume: BTreeMap<&str, Vec<&UnattributedReading>> = BTreeMap::new();
    for r in &u.readings {
        by_volume.entry(r.volume.as_str()).or_default().push(r);
    }
    for (volume, rows) in by_volume {
        let contacts: Vec<String> = rows
            .iter()
            .map(|r| {
                r.contact_id
                    .map(|c| format!("contact {c}"))
                    .unwrap_or_else(|| "no contact".to_string())
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let worst = rows
            .iter()
            .filter_map(|r| r.total_uncorrected)
            .max()
            .map(|n| n.to_string())
            .unwrap_or_else(|| "not recorded".into());
        let alerted = rows
            .iter()
            .filter(|r| r.tape_alerts.unwrap_or(0) > 0)
            .count();
        lines.push(format!(
            "    readings: volume {volume}: {} ({}); max uncorrected {worst}; {alerted} \
             with a TapeAlert raised — `tapectl report health --volume {volume}`",
            rows.len(),
            contacts.join(", "),
        ));
    }
    for a in &u.medium_alerts {
        let flags: Vec<String> = a
            .flags
            .iter()
            .map(|(c, n)| format!("TapeAlert {c} ({n})"))
            .collect();
        lines.push(format!(
            "    medium alert: {} at {}{}: {}",
            a.contact_id
                .map(|c| format!("contact {c}"))
                .unwrap_or_else(|| "no contact".to_string()),
            a.at,
            a.volume
                .as_deref()
                .map(|v| format!(" (volume {v})"))
                .unwrap_or_default(),
            flags.join(", ")
        ));
    }
    for (key, msg) in &u.read_errors_rising {
        lines.push(format!("    read errors rising: {key}: {msg}"));
    }
    lines
}

/// `report cartridge-health --json`: `{"cartridges": [...],
/// "unattributed": {...}}` — every registered cartridge a row
/// ([`to_json`]), and what resolves to none of them beside it, its arrays
/// empty rather than absent. NULL stays `null`.
pub fn fleet_json(f: &Fleet) -> serde_json::Value {
    let u = &f.unattributed;
    let flags = |v: &[(u8, String)]| {
        v.iter()
            .map(|(c, n)| serde_json::json!({"code": c, "name": n}))
            .collect::<Vec<_>>()
    };
    serde_json::json!({
        "cartridges": f.cartridges.iter().map(to_json).collect::<Vec<_>>(),
        "unattributed": {
            "readings": u.readings.iter().map(|r| serde_json::json!({
                "id": r.id,
                "logged_at": r.logged_at,
                "volume": r.volume,
                "contact_id": r.contact_id,
                "operation": r.operation,
                "total_uncorrected": r.total_uncorrected,
                "tape_alerts": r.tape_alerts,
            })).collect::<Vec<_>>(),
            "medium_alerts": u.medium_alerts.iter().map(|a| serde_json::json!({
                "contact_id": a.contact_id,
                "at": a.at,
                "volume": a.volume,
                "flags": flags(&a.flags),
            })).collect::<Vec<_>>(),
            "read_errors_rising": u.read_errors_rising.iter().map(|(k, m)| serde_json::json!({
                "trend": k,
                "message": m,
            })).collect::<Vec<_>>(),
        },
    })
}

/// One cartridge's row in `report cartridge-health --json`. NULL stays
/// `null`.
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
        "verifies_passed": h.verifies_passed,
        "unproven_failed_verifies": h.unproven_failed_verifies,
        "medium_quarantines": h.medium_quarantines,
        "session_quarantines": h.session_quarantines,
        "read_errors_rising": h.read_errors_rising,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Five cartridges and one unbound volume:
    /// - `A-BAD` carries `V-A` (and `V-A2`): readings with a medium TapeAlert
    ///   (19, Nearing media life) and an uncorrected count, a failed verify
    ///   that quarantined it on medium evidence — and the never-written wear columns
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
        // - `D-ABORTED` carries `V-D`, whose only verifies never completed
        //   (one still `in_progress`, one `aborted`): not examined;
        // - `E-UNREAD` carries `V-E`, whose only verify failed without
        //   proving the medium bad (the drive could not read it; the volume
        //   is not quarantined): not examined either, and not attention;
        // - `V-LOOSE` is bound to no cartridge, and its contact 103
        //   identified none: a reading with 5 uncorrected errors, a medium
        //   TapeAlert and a rising read-error trend that resolve to no
        //   cartridge, and must surface as unattributed.
        conn.execute_batch(
            "INSERT INTO cartridges (id, barcode, media_type, nominal_capacity, status,
                                     total_load_count)
             VALUES (4, 'D-ABORTED', 'LTO-6', 2500000000000, 'in_use', 2),
                    (5, 'E-UNREAD', 'LTO-6', 2500000000000, 'in_use', 5);
             INSERT INTO volumes (id, label, backend_type, backend_name, media_type,
                                  capacity_bytes, status, observed_condition)
             VALUES (14, 'V-D', 'lto', 'p', 'LTO-6', 1, 'sealed', 'ok'),
                    (15, 'V-E', 'lto', 'p', 'LTO-6', 1, 'sealed', 'ok'),
                    (16, 'V-LOOSE', 'lto', 'p', 'LTO-6', 1, 'sealed', 'ok');
             INSERT INTO cartridge_volumes (cartridge_id, volume_id, mounted_at)
             VALUES (4, 14, '2026-09-06 00:00:00'),
                    (5, 15, '2026-09-06 00:00:00');
             INSERT INTO cartridge_contacts (id, cartridge_id, volume_id, drive_id, operation,
                                             device, opened_at, closed_at, outcome)
             VALUES (103, NULL, 16, 1, 'volume verify', '/dev/nst0', '2026-09-06 01:00:00',
                     '2026-09-06 02:00:00', 'ok');
             INSERT INTO health_logs (volume_id, contact_id, operation, total_uncorrected,
                                      tape_alerts)
             VALUES (16, 103, 'verify', 5, 1);
             INSERT INTO verification_sessions (id, volume_id, started_at, completed_at,
                                                verify_type, outcome)
             VALUES (30, 14, '2026-09-07 01:00:00', NULL, 'full', 'in_progress'),
                    (31, 14, '2026-09-07 03:00:00', '2026-09-07 03:30:00', 'full', 'aborted'),
                    (32, 15, '2026-09-08 01:00:00', '2026-09-08 02:00:00', 'full', 'failed'),
                    (33, 16, '2026-09-06 01:00:00', '2026-09-06 02:00:00', 'full', 'passed'),
                    (34, 16, '2026-09-09 01:00:00', '2026-09-09 02:00:00', 'full', 'passed'),
                    (35, 13, '2026-09-04 01:00:00', '2026-09-04 02:00:00', 'quick', 'failed');
             INSERT INTO events (timestamp, entity_type, entity_id, entity_label, action,
                                 field, new_value, details)
             VALUES ('2026-09-06 02:00:00', 'volume', 16, 'V-LOOSE', 'verify_read_errors',
                     'corrected_per_gib', '0.010000',
                     '{\"session_id\": 33, \"cartridge_id\": null, \"corrected_per_gib\": 0.01,
                       \"bytes_processed\": 1073741824, \"corrected\": 1, \"uncorrected\": 0}'),
                    ('2026-09-09 02:00:00', 'volume', 16, 'V-LOOSE', 'verify_read_errors',
                     'corrected_per_gib', '1.000000',
                     '{\"session_id\": 34, \"cartridge_id\": null, \"corrected_per_gib\": 1.0,
                       \"bytes_processed\": 1073741824, \"corrected\": 100, \"uncorrected\": 0}');",
        )
        .unwrap();
        // Contact 101 journalled page 0x2E with flag 19 raised (the real HP
        // bytes, one value byte set).
        let mut raw = include_bytes!(
            "../../tests/fixtures/sg_logs/hp_lto6_sg0_fuji_ew7vwmvkf6/page_0x2e.bin"
        )
        .to_vec();
        raw[4 + 18 * 5 + 4] = 1;
        for contact in [101, 103] {
            conn.execute(
                "INSERT INTO log_page_journal (contact_id, device_sg, trigger, page_code, ok,
                                               tool_argv, raw, tapectl_version)
                 VALUES (?1, '/dev/sg0', 'volume verify', 46, 1, '[]', ?2, 'test')",
                rusqlite::params![contact, raw],
            )
            .unwrap();
        }
        conn
    }

    /// #307's acceptance: every registered cartridge appears; A shows its
    /// alert, uncorrected count and quarantine; B is present with an
    /// explicit no-evidence verdict; C is clean despite a session
    /// quarantine. Worst first.
    #[test]
    fn the_fleet_ranks_every_cartridge_and_names_why() {
        let conn = seed();
        let fleet = fleet(&conn, 2.0).unwrap().cartridges;
        let order: Vec<(&str, &str)> = fleet
            .iter()
            .map(|h| (h.barcode.as_str(), h.verdict.as_str()))
            .collect();
        assert_eq!(
            order,
            vec![
                ("A-BAD", "attention"),
                ("B-NEW", "no_evidence"),
                ("D-ABORTED", "no_evidence"),
                ("E-UNREAD", "no_evidence"),
                ("C-OK", "clean")
            ]
        );
        let a = &fleet[0];
        assert_eq!(
            a.medium_alerts,
            vec![(19, "Nearing media life".to_string())]
        );
        assert_eq!(a.max_uncorrected, Some(2));
        assert_eq!(a.medium_quarantines, vec!["V-A".to_string()]);
        // V-A's failed verify is the quarantine's: not named twice.
        assert!(a.unproven_failed_verifies.is_empty());
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
        assert!(text.contains("did not close"), "{text}");

        let b = render(&fleet[1]).join("\n");
        assert!(
            b.starts_with("B-NEW [available] NO_EVIDENCE: loads=unknown contacts=0"),
            "{b}"
        );
        assert!(b.contains("never examined"), "{b}");

        let c = &fleet[4];
        assert!(c.medium_quarantines.is_empty());
        assert_eq!(c.session_quarantines, vec!["V-C2".to_string()]);
        assert!(render(c)
            .join("\n")
            .contains("not by evidence about the medium"));
    }

    /// ADR-0012 (2026-09-17, 2026-09-18): a verify that could not read the
    /// tape says nothing about the medium. A verify that never completed is
    /// not evidence at all, and one that failed without proving the medium
    /// bad (the volume was not quarantined) is a note, not a reason — and
    /// neither turns "never looked" into CLEAN.
    #[test]
    fn a_verify_that_did_not_read_the_tape_is_neither_a_reason_nor_evidence() {
        let conn = seed();
        let fleet = fleet(&conn, 2.0).unwrap().cartridges;
        let by = |b: &str| fleet.iter().find(|h| h.barcode == b).unwrap();

        let d = by("D-ABORTED");
        assert_eq!(d.verdict, Verdict::NoEvidence);
        assert_eq!(d.verifies, 0, "in_progress and aborted are not verifies");

        let e = by("E-UNREAD");
        assert_eq!(e.verdict, Verdict::NoEvidence);
        assert_eq!(e.verifies, 1);
        assert_eq!(e.unproven_failed_verifies, vec!["V-E".to_string()]);
        let text = render(e).join("\n");
        assert!(
            text.contains(
                "note: volume V-E: its last verify failed without proving the medium bad"
            ),
            "{text}"
        );
        assert!(!text.contains("    - the last verify"), "{text}");

        // The positive control: C's passed verify is evidence.
        let c = by("C-OK");
        assert_eq!(c.verdict, Verdict::Clean);
        // V-C2 is quarantined, but by a write-session finding, not the
        // medium: its failed verify is still the unproven kind, and named.
        assert_eq!(c.unproven_failed_verifies, vec!["V-C2".to_string()]);
    }

    /// #307's third case: a reading on a volume bound to no cartridge, whose
    /// contact identified none, is not dropped — it is named under
    /// `unattributed`, with the medium TapeAlert and the rising read-error
    /// trend that resolve to no cartridge either. The positive control: the
    /// bound cartridges' readings are not there, and are counted against
    /// their cartridges.
    #[test]
    fn evidence_on_no_cartridge_is_reported_as_unattributed() {
        let conn = seed();
        let f = fleet(&conn, 2.0).unwrap();
        let u = &f.unattributed;
        assert_eq!(u.readings.len(), 1, "{:?}", u.readings);
        assert_eq!(u.readings[0].volume, "V-LOOSE");
        assert_eq!(u.readings[0].contact_id, Some(103));
        assert_eq!(u.readings[0].total_uncorrected, Some(5));
        assert_eq!(u.medium_alerts.len(), 1, "{:?}", u.medium_alerts);
        assert_eq!(u.medium_alerts[0].contact_id, Some(103));
        assert_eq!(
            u.medium_alerts[0].flags,
            vec![(19, "Nearing media life".to_string())]
        );
        assert_eq!(u.read_errors_rising.len(), 1, "{:?}", u.read_errors_rising);
        assert!(u.read_errors_rising[0].0.contains("V-LOOSE"));
        // Positive control: the bound readings stay with their cartridge.
        let a = f.cartridges.iter().find(|h| h.barcode == "A-BAD").unwrap();
        assert_eq!(a.readings, 2);
        assert!(f.cartridges.iter().all(|h| h.max_uncorrected != Some(5)));

        let text = render_unattributed(u).join("\n");
        assert!(text.contains("V-LOOSE"), "{text}");
        assert!(text.contains("contact 103"), "{text}");
        let json = fleet_json(&f);
        assert_eq!(json["unattributed"]["readings"][0]["volume"], "V-LOOSE");
        assert_eq!(json["unattributed"]["readings"][0]["contact_id"], 103);
        assert_eq!(
            json["unattributed"]["medium_alerts"][0]["flags"][0]["code"],
            19
        );
        assert_eq!(
            json["unattributed"]["read_errors_rising"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    /// Nothing unattributed still says so: the section is never absent.
    #[test]
    fn an_empty_unattributed_section_still_says_so() {
        let text = render_unattributed(&Unattributed::default()).join("\n");
        assert!(text.contains("unattributed: none"), "{text}");
    }

    /// The never-written wear columns are proven unread by SENTINEL: the
    /// seeded values appear nowhere in the text or the JSON, while what
    /// should appear (the barcode, the load count) does — so "absent"
    /// cannot mean "rendered nothing".
    #[test]
    fn the_confident_zero_columns_never_reach_the_output() {
        let conn = seed();
        let f = fleet(&conn, 2.0).unwrap();
        let text: String = f
            .cartridges
            .iter()
            .flat_map(render)
            .chain(render_unattributed(&f.unattributed))
            .collect::<Vec<_>>()
            .join("\n");
        let json = fleet_json(&f).to_string();
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

    /// JSON: an object with every cartridge a row under `cartridges` and
    /// the `unattributed` block beside it; a missing load count is `null`,
    /// the verdict and reasons are carried.
    #[test]
    fn the_json_keeps_null_as_null_and_every_cartridge() {
        let conn = seed();
        let json = fleet_json(&fleet(&conn, 2.0).unwrap());
        let rows = json["cartridges"].as_array().unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[1]["barcode"], "B-NEW");
        assert_eq!(rows[1]["loads"], serde_json::Value::Null);
        assert_eq!(rows[1]["verdict"], "no_evidence");
        assert_eq!(rows[1]["last_contact"], serde_json::Value::Null);
        assert_eq!(rows[0]["last_bind"], "2026-09-01 00:00:00");
        assert_eq!(rows[0]["reasons"].as_array().unwrap().len(), 3);
        assert_eq!(rows[3]["barcode"], "E-UNREAD");
        assert_eq!(rows[3]["unproven_failed_verifies"][0], "V-E");
        assert!(json["unattributed"].is_object());
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
