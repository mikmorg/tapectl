//! The previous reading of a drive's counter pages within one load: the
//! baseline a contact's page 0x02 or 0x03 is differenced against (ADR-0012
//! item 170's rule, built for #488 and shared with #495).
//!
//! **Why a baseline.** HP: "All these counters are updated when the data set
//! is physically written. They relate to the current tape and are cleared
//! when the tape is unloaded" (HP-V3 p63, page 0x02; p64 says the same of
//! page 0x03 for data sets read). The 2026-09-23 captures agree: the
//! no-medium pages still held the last tape's counts, and both read all
//! zeros with a cartridge just loaded that had 32 GB written in its life
//! (`tests/fixtures/sg_logs/hp_lto6_sg0_fuji_ew7vwmvkf6`). So a reading
//! covers everything since the load, and ADR-0013 reads each page at most
//! once per contact, after the command. What ONE command did is its
//! contact's reading less the reading of the previous contact in the same
//! load, read back from the journal — never a second read of the drive.
//!
//! **One rule, two pages.** `feed_ratio` takes page 0x02 across a write and
//! `read_errors` page 0x03 across a verify, both through
//! [`same_load_baseline`]. It decides WHICH earlier reading is the baseline
//! and reads no page itself: each consumer reads its own page of that
//! contact from the journal ([`journalled_decode`]) and parses it its own
//! way.

use rusqlite::{Connection, OptionalExtension};

use crate::error::Result;

/// Where a contact's counter pages started from, within its load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadBaseline {
    /// This contact is the first of its load on an HP drive: the pages
    /// started from zero.
    LoadZero,
    /// The previous contact on the same drive, in the same load: its
    /// journalled page is the baseline. If it journalled none, nothing is
    /// known — no older contact stands in for it (one between them that took
    /// no sweep may have written or read).
    Contact(i64),
    /// Not known: a NULL drive, cartridge or load count on either side, or
    /// the first contact of a load on a drive whose pages carry no statement
    /// that they clear at unload.
    Unknown,
}

/// A contact's drive, cartridge and MAM load count, each as recorded.
struct ContactLoad {
    drive_id: Option<i64>,
    cartridge_id: Option<i64>,
    chip_load_count: Option<i64>,
}

/// Where this contact's counter pages started from ([`LoadBaseline`]).
///
/// The previous contact is the one immediately before this one on the same
/// drive (`cartridge_contacts.drive_id`), never an older one. It is in the
/// same load when it names the same cartridge and the same MAM load count
/// (`chip_load_count`). A previous contact on another cartridge or load
/// count, or none at all, makes this the first contact of its load: zero on
/// an HP drive (HP-V3 p63-64: "cleared when the tape is unloaded"), and not
/// known on another vendor's, whose pages have no such statement behind
/// them. A NULL drive, cartridge or load count on this contact, or a NULL
/// cartridge or load count on a previous contact naming the same cartridge,
/// is not known.
pub fn same_load_baseline(conn: &Connection, contact_id: i64) -> Result<LoadBaseline> {
    let this = conn
        .query_row(
            "SELECT drive_id, cartridge_id, chip_load_count FROM cartridge_contacts
              WHERE id = ?1",
            [contact_id],
            |r| {
                Ok(ContactLoad {
                    drive_id: r.get(0)?,
                    cartridge_id: r.get(1)?,
                    chip_load_count: r.get(2)?,
                })
            },
        )
        .optional()?;
    let Some(ContactLoad {
        drive_id: Some(drive_id),
        cartridge_id: Some(cartridge_id),
        chip_load_count: Some(load_count),
    }) = this
    else {
        return Ok(LoadBaseline::Unknown);
    };
    let vendor: Option<String> = conn
        .query_row("SELECT vendor FROM drives WHERE id = ?1", [drive_id], |r| {
            r.get(0)
        })
        .optional()?
        .flatten();
    let first_of_load = if crate::tape::log_pages::is_hp_vendor(vendor.as_deref()) {
        LoadBaseline::LoadZero
    } else {
        LoadBaseline::Unknown
    };
    let previous: Option<(i64, ContactLoad)> = conn
        .query_row(
            "SELECT id, drive_id, cartridge_id, chip_load_count FROM cartridge_contacts
              WHERE drive_id = ?1 AND id < ?2
              ORDER BY id DESC LIMIT 1",
            rusqlite::params![drive_id, contact_id],
            |r| {
                Ok((
                    r.get(0)?,
                    ContactLoad {
                        drive_id: r.get(1)?,
                        cartridge_id: r.get(2)?,
                        chip_load_count: r.get(3)?,
                    },
                ))
            },
        )
        .optional()?;
    let Some((prev, load)) = previous else {
        return Ok(first_of_load);
    };
    Ok(match (load.cartridge_id, load.chip_load_count) {
        (Some(other), _) if other != cartridge_id => first_of_load,
        (Some(_), Some(loads)) if loads != load_count => first_of_load,
        (Some(_), Some(_)) => LoadBaseline::Contact(prev),
        _ => LoadBaseline::Unknown,
    })
}

/// The newest successful decode of `page_code` (subpage 0) that `contact_id`'s
/// sweep journalled. One sweep per contact (migration 023) means at most one
/// such row; the newest is taken should a future change ever leave two. A
/// failed read or a failed decode is `None`: the row exists, but says
/// nothing.
pub fn journalled_decode(
    conn: &Connection,
    contact_id: i64,
    page_code: u8,
) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT decoded FROM log_page_journal
              WHERE contact_id = ?1 AND page_code = ?2 AND subpage_code = 0
                AND ok = 1 AND decoded IS NOT NULL
              ORDER BY id DESC LIMIT 1",
            rusqlite::params![contact_id, page_code],
            |r| r.get(0),
        )
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(conn: &Connection, serial: &str, vendor: &str) -> i64 {
        conn.execute(
            "INSERT INTO drives (serial, vendor, model) VALUES (?1, ?2, 'Ultrium 6-SCSI')",
            [serial, vendor],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn contact(conn: &Connection, drive: i64, cartridge: Option<i64>, loads: Option<i64>) -> i64 {
        conn.execute(
            "INSERT INTO cartridge_contacts (operation, device, drive_id, cartridge_id,
                                             chip_load_count)
             VALUES ('volume verify', '/dev/nst0', ?1, ?2, ?3)",
            rusqlite::params![drive, cartridge, loads],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// The rule names the contact and reads no page: a previous contact in
    /// the same load is the baseline whether or not it journalled anything
    /// (each consumer reads its own page of it, and finds none). Another
    /// cartridge before it is the load's zero on an HP drive and unknown on
    /// another vendor's.
    #[test]
    fn the_baseline_is_the_previous_contact_in_the_load_and_reads_no_page() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity)
             VALUES ('C1', 'LTO-6', 2500000000000), ('C2', 'LTO-6', 2500000000000)",
            [],
        )
        .unwrap();
        let hp = drive(&conn, "HP-1", "HP");
        let first = contact(&conn, hp, Some(1), Some(3));
        let second = contact(&conn, hp, Some(1), Some(3));
        assert_eq!(
            same_load_baseline(&conn, first).unwrap(),
            LoadBaseline::LoadZero
        );
        assert_eq!(
            same_load_baseline(&conn, second).unwrap(),
            LoadBaseline::Contact(first)
        );
        assert_eq!(journalled_decode(&conn, first, 0x03).unwrap(), None);
        let other = contact(&conn, hp, Some(2), Some(3));
        assert_eq!(
            same_load_baseline(&conn, other).unwrap(),
            LoadBaseline::LoadZero
        );

        let ibm = drive(&conn, "IBM-1", "IBM");
        let w = contact(&conn, ibm, Some(1), Some(3));
        assert_eq!(same_load_baseline(&conn, w).unwrap(), LoadBaseline::Unknown);
        let nameless = contact(&conn, hp, None, Some(3));
        assert_eq!(
            same_load_baseline(&conn, nameless).unwrap(),
            LoadBaseline::Unknown
        );
    }
}
