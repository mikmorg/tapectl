//! A raised TapeAlert on a live volume's cartridge (issue #308; ADR-0012,
//! 2026-10-07 item 29) — what `audit`'s `tape_alert` check reports.
//!
//! The sightings are `report health`'s ([`tape_alert_sightings`]): every
//! journalled page 0x2E with a raised flag, and every `health_logs` row with
//! `tape_alerts > 0` the journal does not already speak for. This module
//! only decides which LIVE volume each one is about, and whether it is
//! still news:
//!
//! - **Which volume.** A sighting's contact names a volume (a write, a
//!   verify, a restore) and/or a cartridge (every contact whose chip serial
//!   is registered — `drive poll`'s included, which names no volume because
//!   it never reads the tape). The volumes it is about are the contact's own
//!   and every volume the catalog has bound to that cartridge; a pre-021
//!   health row, which has no contact, names its volume directly. A
//!   drive-only sighting (no cartridge, no volume) is about the drive, not
//!   an archive copy, and is the poll's to report (`/fail`), not the
//!   audit's.
//! - **Live** is `coverage::in_service_or_provisioned` (issue #96: never an
//!   inline status list): retired, erased and quarantined media drop out —
//!   a quarantined volume is already out of the copy count, where
//!   `copy_count` takes over — while an `initialized` volume an interrupted
//!   write left, the next `volume resume` target, stays in.
//! - **Still news** is "not logged before the volume's last PASSED full
//!   readback STARTED". Whether page 0x2E clears on read is unsettled
//!   (ADR-0013's hazard), so a later clean reading proves nothing; a passed
//!   full verify does — every byte read back. That makes the remedy the
//!   finding names the one that resolves it: the verify either passes (the
//!   warning goes) or fails and quarantines the volume (the warning goes,
//!   and the copy checks speak). By its START, never its `completed_at`
//!   (ADR-0012, 2026-10-07 item 30: freshness of a full readback is judged
//!   by its start): an alert raised during the readback — including by the
//!   verify's own post-readback sweep, which runs after `completed_at` is
//!   written and usually logs in the same second — is not something that
//!   readback can answer. A continued readback is dated from its oldest
//!   checkpoint (item 1), which only makes this stricter.
//!
//! A warning, never a violation (exit 1): the count cannot yet tell
//! "this medium is failing" from "clean the drive", and the copy is still
//! sealed and counted (ADR-0004; an alert is a risk, not a proof — ADR-0008).
//! A TapeAlert never changes `coverage::eligible`.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension};

use crate::cli::report::{tape_alert_sightings, TapeAlertSighting, TapeAlertSource};
use crate::error::Result;

/// One live volume and the sightings that are still news for it, oldest
/// first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VolumeAlerts {
    pub(crate) label: String,
    pub(crate) sightings: Vec<TapeAlertSighting>,
}

impl VolumeAlerts {
    /// The finding's message: the volume, how many contacts raised what,
    /// and the newest one's date, operation, drive and flags.
    pub(crate) fn message(&self) -> String {
        let newest = self.sightings.last().expect("never built empty");
        let flags = if newest.flags.is_empty() {
            format!(
                "{} flag(s), unnamed (no 0x2E decode left)",
                newest.recorded_count.unwrap_or(0)
            )
        } else {
            newest
                .flags
                .iter()
                .map(|(n, name)| format!("{n} {name}"))
                .collect::<Vec<_>>()
                .join("; ")
        };
        let earlier = match self.sightings.len() {
            1 => String::new(),
            n => format!(" ({} earlier contact(s) raised alerts too)", n - 1),
        };
        format!(
            "volume \"{}\": the drive raised a TapeAlert — {flags} — at {} during \"{}\"{}{}{}",
            self.label,
            newest.at,
            newest.operation.as_deref().unwrap_or("?"),
            newest
                .cartridge_barcode
                .as_deref()
                .map(|c| format!(" with cartridge {c} loaded"))
                .unwrap_or_default(),
            newest
                .drive_serial
                .as_deref()
                .map(|d| format!(" on drive {d}"))
                .unwrap_or_default(),
            earlier,
        )
    }
}

/// Every live volume with a raised TapeAlert not logged before its last
/// passed full verify started, in label order.
pub(crate) fn on_live_volumes(conn: &Connection) -> Result<Vec<VolumeAlerts>> {
    let mut by_volume: BTreeMap<String, Vec<TapeAlertSighting>> = BTreeMap::new();
    for sighting in tape_alert_sightings(conn)? {
        for (label, readback_started) in live_volumes_of(conn, &sighting)? {
            // Strictly before: a sighting in the readback's first second may
            // be its own, so it stays news.
            if readback_started
                .as_deref()
                .is_some_and(|started| sighting.at.as_str() < started)
            {
                continue;
            }
            by_volume.entry(label).or_default().push(sighting.clone());
        }
    }
    Ok(by_volume
        .into_iter()
        .map(|(label, sightings)| VolumeAlerts { label, sightings })
        .collect())
}

/// The live volumes `sighting` is about, each with the `started_at` of its
/// last passed full verify.
fn live_volumes_of(
    conn: &Connection,
    sighting: &TapeAlertSighting,
) -> Result<Vec<(String, Option<String>)>> {
    let (direct_volume, cartridge): (Option<i64>, Option<i64>) = match sighting.contact_id {
        Some(cid) => conn
            .query_row(
                "SELECT volume_id, cartridge_id FROM cartridge_contacts WHERE id = ?1",
                params![cid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((None, None)),
        None => match sighting.source {
            TapeAlertSource::HealthLog { row_id } => (
                conn.query_row(
                    "SELECT volume_id FROM health_logs WHERE id = ?1",
                    params![row_id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten(),
                None,
            ),
            TapeAlertSource::Journal { .. } => (None, None),
        },
    };
    if direct_volume.is_none() && cartridge.is_none() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT v.label,
                (SELECT MAX(vs.started_at) FROM verification_sessions vs
                  WHERE vs.volume_id = v.id AND vs.verify_type = 'full'
                    AND vs.outcome = 'passed')
           FROM volumes v
          WHERE (v.id = ?1
                 OR v.id IN (SELECT cv.volume_id FROM cartridge_volumes cv
                              WHERE cv.cartridge_id = ?2))
            AND {}
          ORDER BY v.label",
        crate::policy::coverage::in_service_or_provisioned("v")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![direct_volume, cartridge], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}
