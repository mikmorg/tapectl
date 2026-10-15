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
//!
//! **What the message says of a cleaning flag.** TapeAlert 20-23 name no
//! fault in the volume or the medium, so the message prints such a flag as
//! the action it asks for (`20 Cleaning required => clean the drive: …`,
//! the words from [`alert_flags::cleaning_action`], issue #430); every other
//! flag is printed `<n> <name>` as before. The finding's severity and its
//! `volume verify` fix do not depend on the flag.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension};

use crate::cli::report::{tape_alert_sightings, TapeAlertSighting, TapeAlertSource};
use crate::error::Result;
use crate::tape::alert_flags;

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
                .map(
                    |(n, name)| match alert_flags::cleaning_action(u16::from(*n)) {
                        // A cleaning flag is an action, not a finding about the
                        // volume (issue #430).
                        Some(action) => format!("{n} {name} => {action}"),
                        None => format!("{n} {name}"),
                    },
                )
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

#[cfg(test)]
mod tests {
    use super::*;

    const CLEAN_THE_DRIVE: &str = "clean the drive: nothing is wrong with the data";

    /// One sighting on a drive poll's contact, flags as `report health`
    /// numbers them.
    fn sighting(flags: &[(u8, &str)]) -> TapeAlertSighting {
        TapeAlertSighting {
            contact_id: Some(7),
            operation: Some("drive poll".into()),
            at: "2026-10-05 06:30:00".into(),
            drive_serial: Some("HUJ808A5L4".into()),
            cartridge_barcode: Some("CART08L6".into()),
            flags: flags
                .iter()
                .map(|(n, name)| (*n, name.to_string()))
                .collect(),
            recorded_count: Some(flags.len() as i64),
            source: crate::cli::report::TapeAlertSource::Journal { row_id: 1 },
            codes_from_bytes: true,
        }
    }

    fn message(flags: &[(u8, &str)]) -> String {
        VolumeAlerts {
            label: "L6-0001".into(),
            sightings: vec![sighting(flags)],
        }
        .message()
    }

    const WHERE_AND_WHEN: &str = "at 2026-10-05 06:30:00 during \"drive poll\" with cartridge \
                                  CART08L6 loaded on drive HUJ808A5L4";

    /// Issue #430: a cleaning flag is printed as the action it asks for,
    /// with the rest of the message as it was.
    #[test]
    fn a_cleaning_flag_reads_as_an_action() {
        assert_eq!(
            message(&[(20, "Cleaning required")]),
            format!(
                "volume \"L6-0001\": the drive raised a TapeAlert — 20 Cleaning required => \
                 {CLEAN_THE_DRIVE} — {WHERE_AND_WHEN}"
            )
        );
        // All four flags of the class (20-23), by number.
        for (n, name) in [
            (20, "Cleaning required"),
            (21, "Cleaning requested"),
            (22, "Expired cleaning media"),
            (23, "Invalid cleaning tape"),
        ] {
            let m = message(&[(n, name)]);
            // 20/21 ask for a cleaning; 22/23 name the cleaning cartridge.
            let action = alert_flags::cleaning_action(n.into()).expect("a cleaning flag");
            assert_eq!(action == CLEAN_THE_DRIVE, n <= 21, "{n}: {action}");
            assert!(m.contains(&format!("{n} {name} => {action}")), "{m}");
        }
    }

    /// The action is attached to the flag that asks for it and to no other.
    #[test]
    fn only_the_cleaning_flag_among_several_carries_the_action() {
        let m = message(&[
            (4, "Media"),
            (20, "Cleaning required"),
            (36, "Drive temperature"),
        ]);
        assert!(
            m.contains(&format!(
                "4 Media; 20 Cleaning required => {CLEAN_THE_DRIVE}; 36 Drive temperature — "
            )),
            "{m}"
        );
        assert_eq!(m.matches("=>").count(), 1, "{m}");
    }

    /// A flag that is not a cleaning flag reads exactly as it did before
    /// #430, byte for byte.
    #[test]
    fn a_non_cleaning_flag_reads_as_it_always_did() {
        assert_eq!(
            message(&[(4, "Media"), (36, "Drive temperature")]),
            format!(
                "volume \"L6-0001\": the drive raised a TapeAlert — 4 Media; \
                 36 Drive temperature — {WHERE_AND_WHEN}"
            )
        );
        assert_eq!(
            message(&[(5, "Read failure")]),
            format!(
                "volume \"L6-0001\": the drive raised a TapeAlert — 5 Read failure — \
                 {WHERE_AND_WHEN}"
            )
        );
        // No decode left to name the flags: the count, unnamed.
        assert_eq!(
            message(&[]),
            format!(
                "volume \"L6-0001\": the drive raised a TapeAlert — 0 flag(s), unnamed (no \
                 0x2E decode left) — {WHERE_AND_WHEN}"
            )
        );
    }

    /// Through the catalog, as `audit` reads it: a live volume whose health
    /// row recorded a raised "Cleaning required" is named, with the action.
    /// The positive control is the same volume's clean twin: not named.
    #[test]
    fn a_cleaning_alert_on_a_live_volume_names_the_action() {
        let conn = crate::db::open_memory().unwrap();
        let volume = |label: &str| -> i64 {
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                      capacity_bytes, status)
                 VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
                params![label],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        // Flag n is the page's n-th flag line; flag 20 is raised.
        let mut raw_log = String::from("=== page 0x2e ===\n");
        for n in 1..=63 {
            let name = if n == 20 {
                "Cleaning required".to_string()
            } else {
                format!("Flag {n}")
            };
            raw_log.push_str(&format!("  {name}: {}\n", u8::from(n == 20)));
        }
        let health = |volume_id: i64, alerts: i64, log: &str| {
            conn.execute(
                "INSERT INTO health_logs (volume_id, operation, tape_alerts, raw_log, logged_at)
                 VALUES (?1, 'verify', ?2, ?3, '2026-10-01 10:00:00')",
                params![volume_id, alerts, log],
            )
            .unwrap();
        };
        health(volume("C-LIVE"), 1, &raw_log);
        health(volume("C-CLEAN"), 0, "");

        let found = on_live_volumes(&conn).unwrap();
        assert_eq!(found.len(), 1, "only the volume with the alert: {found:?}");
        assert_eq!(found[0].label, "C-LIVE");
        let m = found[0].message();
        assert!(
            m.contains(&format!("20 Cleaning required => {CLEAN_THE_DRIVE}")),
            "{m}"
        );
    }
}
