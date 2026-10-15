//! Drive cleanings: which cleaning cartridges an operator uses, when each
//! drive was last cleaned, and how many uses each cartridge has left (issue
//! #430; ADR-0012 2026-10-15 item 60).
//!
//! tapectl drives no changer and never loads a cartridge itself, so a
//! cleaning is something the operator does with their hands: the drive asks
//! (TapeAlert flag 20/21, `mt`'s `CLN` bit), the operator inserts the
//! cleaning cartridge, the drive cleans itself and ejects it. What tapectl
//! can do is **remember it** — a record, not an action — so that the
//! frequency of cleanings (a drive-health trend) and the life left in the
//! cleaning cartridge are not kept in someone's head.
//!
//! # Three pieces, and where each lives
//!
//! - **`cleaning_cartridges`** (migration 036): the registry, `serial`,
//!   `registered_at`, `rated_uses`. A cleaning cartridge is NOT a row of
//!   `cartridges`: that table is the cartridges that hold data, with a
//!   status lifecycle, a capacity, volumes and a place, none of which a
//!   cleaning cartridge has, and rebuilding it (twice already, 012 and 033)
//!   to teach it a second kind would touch every row a tape's life depends
//!   on. `serial` is the cartridge's chip serial (a cleaning cartridge's
//!   chip is read the way a data cartridge's is) or, when its chip cannot be
//!   read, whatever the operator uses to tell it apart.
//! - **One event per cleaning**: `entity_type = 'drive'`, `entity_id` the
//!   drive's `drives` row, `action = 'cleaned'`, `field =
//!   'cleaning_cartridge'`, `new_value` the cleaning cartridge's serial (NULL
//!   when the operator did not say which cartridge cleaned it).
//! - **Everything else is derived from those events**, never stored: a
//!   cartridge's uses are the count of events naming it, a drive's last
//!   cleaning the newest event on it. A stored counter is a second copy of
//!   the events that can disagree with them (ADR-0013 §3), and the events
//!   are append-only and never pruned (ADR-0012 2026-10-07 item 28).
//!
//! # Not here
//!
//! No contact. A contact is a time a *data* cartridge was in a drive
//! (`cartridge_contacts.cartridge_id` references `cartridges`), and a
//! cleaning that wrote one would have to say its cartridge is "not
//! registered" when it is known.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::db::events;
use crate::error::{Result, TapectlError};

/// `events.action` of a recorded cleaning.
pub const EVENT_ACTION: &str = "cleaned";
/// `events.entity_type` of a recorded cleaning: the drive that was cleaned.
pub const EVENT_ENTITY: &str = "drive";
/// `events.field` of a recorded cleaning; `new_value` is the cleaning
/// cartridge's serial.
pub const EVENT_FIELD: &str = "cleaning_cartridge";

/// What a registration assumes when the operator gives no rated life. LTO
/// Ultrium universal cleaning cartridges (HPE, IBM, Quantum, Fujifilm) are
/// rated for 50 cleanings; the drive ejects a spent one without cleaning.
pub const DEFAULT_RATED_USES: i64 = 50;

/// How many uses before the rated life a cartridge is called "near" it.
pub const NEAR_END_USES: i64 = 5;

/// A registered cleaning cartridge.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CleaningCartridge {
    pub id: i64,
    pub serial: String,
    pub registered_at: String,
    pub rated_uses: i64,
}

/// Where a cleaning cartridge stands against its rated life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Life {
    Ok,
    /// At most [`NEAR_END_USES`] uses left.
    Near,
    /// At or past its rated uses: the drive will not clean with it.
    Spent,
}

/// A cleaning cartridge and the uses recorded against it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Standing {
    pub serial: String,
    pub registered_at: String,
    pub rated_uses: i64,
    /// Events naming this cartridge.
    pub uses: i64,
    /// `rated_uses - uses`, negative once past the rated life.
    pub remaining: i64,
    pub life: Life,
    /// Timestamp of the newest event naming it.
    pub last_used: Option<String>,
}

impl Standing {
    fn new(c: CleaningCartridge, uses: i64, last_used: Option<String>) -> Standing {
        let remaining = c.rated_uses - uses;
        let life = if remaining <= 0 {
            Life::Spent
        } else if remaining <= NEAR_END_USES {
            Life::Near
        } else {
            Life::Ok
        };
        Standing {
            serial: c.serial,
            registered_at: c.registered_at,
            rated_uses: c.rated_uses,
            uses,
            remaining,
            life,
            last_used,
        }
    }

    /// The warning this standing earns, if any.
    pub fn warning(&self) -> Option<String> {
        match self.life {
            Life::Ok => None,
            Life::Near => Some(format!(
                "cleaning cartridge {} is near its rated life: {} of {} uses recorded, {} left",
                self.serial, self.uses, self.rated_uses, self.remaining
            )),
            Life::Spent => Some(format!(
                "cleaning cartridge {} is at or past its rated life ({} of {} uses recorded): \
                 the drive will not clean with a spent cartridge — replace it",
                self.serial, self.uses, self.rated_uses
            )),
        }
    }

    /// One line for `report health` and `drive status`.
    pub fn line(&self) -> String {
        let base = format!(
            "{}: {} of {} rated uses recorded, {} left{}",
            self.serial,
            self.uses,
            self.rated_uses,
            self.remaining.max(0),
            self.last_used
                .as_deref()
                .map(|t| format!(", last used {t}"))
                .unwrap_or_default()
        );
        match self.life {
            Life::Ok => base,
            Life::Near => format!("{base}  ** NEAR ITS RATED LIFE **"),
            Life::Spent => format!("{base}  ** SPENT — replace it **"),
        }
    }
}

/// One drive's cleaning history.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DriveCleaning {
    pub drive_serial: String,
    /// Cleanings recorded on this drive.
    pub cleanings: i64,
    /// Timestamp of the newest; `None` when none was ever recorded.
    pub last_cleaned: Option<String>,
}

/// Register the cleaning cartridge `serial` with `rated_uses` rated cleanings.
/// Refuses a serial already registered (its rated life is not rewritten
/// under the uses already counted against it) and an empty serial or a rated
/// life under 1.
pub fn register(conn: &Connection, serial: &str, rated_uses: i64) -> Result<CleaningCartridge> {
    let serial = serial.trim();
    if serial.is_empty() {
        return Err(TapectlError::Other(
            "a cleaning cartridge needs a serial (the serial its chip reports, or the label \
             that tells it apart)"
                .into(),
        ));
    }
    if rated_uses < 1 {
        return Err(TapectlError::Other(format!(
            "--rated-uses must be at least 1 (got {rated_uses}); LTO cleaning cartridges are \
             rated for {DEFAULT_RATED_USES}"
        )));
    }
    if let Some(existing) = find(conn, serial)? {
        return Err(TapectlError::Other(format!(
            "cleaning cartridge {serial} is already registered (rated for {} uses, since {})",
            existing.rated_uses, existing.registered_at
        )));
    }
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO cleaning_cartridges (serial, rated_uses) VALUES (?1, ?2)",
        params![serial, rated_uses],
    )?;
    let id = tx.last_insert_rowid();
    events::log_created(&tx, "cleaning_cartridge", id, serial, None)?;
    let registered_at: String = tx.query_row(
        "SELECT registered_at FROM cleaning_cartridges WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )?;
    tx.commit()?;
    Ok(CleaningCartridge {
        id,
        serial: serial.to_string(),
        registered_at,
        rated_uses,
    })
}

/// The registered cleaning cartridge with this serial, if any.
pub fn find(conn: &Connection, serial: &str) -> Result<Option<CleaningCartridge>> {
    Ok(conn
        .query_row(
            "SELECT id, serial, registered_at, rated_uses
               FROM cleaning_cartridges WHERE serial = ?1",
            params![serial],
            |r| {
                Ok(CleaningCartridge {
                    id: r.get(0)?,
                    serial: r.get(1)?,
                    registered_at: r.get(2)?,
                    rated_uses: r.get(3)?,
                })
            },
        )
        .optional()?)
}

/// Uses recorded against `serial`, and when the newest was.
fn uses_of(conn: &Connection, serial: &str) -> Result<(i64, Option<String>)> {
    Ok(conn.query_row(
        "SELECT COUNT(*), MAX(timestamp) FROM events
          WHERE entity_type = ?1 AND action = ?2 AND field = ?3 AND new_value = ?4",
        params![EVENT_ENTITY, EVENT_ACTION, EVENT_FIELD, serial],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

/// The standing of the registered cleaning cartridge `serial`, if there is one.
pub fn standing(conn: &Connection, serial: &str) -> Result<Option<Standing>> {
    let Some(cartridge) = find(conn, serial)? else {
        return Ok(None);
    };
    let (uses, last) = uses_of(conn, serial)?;
    Ok(Some(Standing::new(cartridge, uses, last)))
}

/// Every registered cleaning cartridge with its uses, in registration order.
pub fn standings(conn: &Connection) -> Result<Vec<Standing>> {
    let cartridges: Vec<CleaningCartridge> = conn
        .prepare(
            "SELECT id, serial, registered_at, rated_uses FROM cleaning_cartridges ORDER BY id",
        )?
        .query_map([], |r| {
            Ok(CleaningCartridge {
                id: r.get(0)?,
                serial: r.get(1)?,
                registered_at: r.get(2)?,
                rated_uses: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    cartridges
        .into_iter()
        .map(|c| {
            let (uses, last) = uses_of(conn, &c.serial)?;
            Ok(Standing::new(c, uses, last))
        })
        .collect()
}

/// Every drive the catalog knows, with its cleanings, by serial.
pub fn drive_cleanings(conn: &Connection) -> Result<Vec<DriveCleaning>> {
    Ok(conn
        .prepare(
            "SELECT d.serial,
                    (SELECT COUNT(*) FROM events e
                      WHERE e.entity_type = ?1 AND e.entity_id = d.id AND e.action = ?2),
                    (SELECT MAX(e.timestamp) FROM events e
                      WHERE e.entity_type = ?1 AND e.entity_id = d.id AND e.action = ?2)
               FROM drives d ORDER BY d.serial",
        )?
        .query_map(params![EVENT_ENTITY, EVENT_ACTION], |r| {
            Ok(DriveCleaning {
                drive_serial: r.get(0)?,
                cleanings: r.get(1)?,
                last_cleaned: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// Record one cleaning of the drive `drive_id` (`drive_serial` for the
/// label) as an event, naming `cleaning` when the operator said which
/// cartridge did it. `via` is how the cartridge was identified (`flag`, or
/// `chip` when read from the drive), kept in the event's details. Returns
/// the event id.
pub fn record(
    conn: &Connection,
    drive_id: i64,
    drive_serial: &str,
    device: &str,
    cleaning: Option<&CleaningCartridge>,
    via: Option<&str>,
) -> Result<i64> {
    let details = serde_json::json!({
        "device": device,
        "identified_by": via,
    })
    .to_string();
    events::log_event(
        conn,
        EVENT_ENTITY,
        drive_id,
        Some(drive_serial),
        EVENT_ACTION,
        Some(EVENT_FIELD),
        None,
        cleaning.map(|c| c.serial.as_str()),
        Some(&details),
        None,
    )
}

/// Whether anything about cleaning is on record: a registered cleaning
/// cartridge or a recorded cleaning. `report health` shows its cleaning
/// block only then, so a catalog that never used the feature prints exactly
/// what it printed before it existed.
fn in_use(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM cleaning_cartridges)
             OR EXISTS (SELECT 1 FROM events WHERE action = ?1 AND entity_type = ?2)",
        params![EVENT_ACTION, EVENT_ENTITY],
        |r| r.get(0),
    )?)
}

/// `report health`'s cleaning block: each drive's last cleaning, then each
/// cleaning cartridge's uses against its rated life, a life at its end
/// flagged. Empty until a cleaning cartridge is registered or a cleaning
/// recorded.
pub fn block(conn: &Connection) -> Result<Vec<String>> {
    if !in_use(conn)? {
        return Ok(Vec::new());
    }
    let mut out = vec!["Drive cleanings:".to_string()];
    let drives = drive_cleanings(conn)?;
    if drives.is_empty() {
        out.push("  no drive is known to this catalog".to_string());
    }
    for d in &drives {
        out.push(match &d.last_cleaned {
            Some(at) => format!(
                "  drive {}: last cleaned {at} ({} cleaning(s) recorded)",
                d.drive_serial, d.cleanings
            ),
            None => format!("  drive {}: no cleaning recorded", d.drive_serial),
        });
    }
    let cartridges = standings(conn)?;
    if cartridges.is_empty() {
        out.push(
            "  no cleaning cartridge is registered (tapectl drive cleaning-cartridge add \
             <serial>)"
                .to_string(),
        );
    }
    for c in &cartridges {
        out.push(format!("  cleaning cartridge {}", c.line()));
    }
    Ok(out)
}

/// [`block`] as data, under `report health --json`'s `cleanings` key: absent
/// (`None`) when [`block`] prints nothing.
pub fn json(conn: &Connection) -> Result<Option<serde_json::Value>> {
    if !in_use(conn)? {
        return Ok(None);
    }
    Ok(Some(serde_json::json!({
        "drives": drive_cleanings(conn)?,
        "cleaning_cartridges": standings(conn)?,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        crate::db::open_memory().unwrap()
    }

    fn drive(conn: &Connection, serial: &str) -> i64 {
        conn.execute("INSERT INTO drives (serial) VALUES (?1)", params![serial])
            .unwrap();
        conn.last_insert_rowid()
    }

    /// Recording a cleaning naming a registered cartridge counts one use of
    /// it and dates the drive's last cleaning; recording one naming no
    /// cartridge dates the drive and counts nothing.
    #[test]
    fn uses_and_last_cleaned_are_derived_from_the_events() {
        let conn = conn();
        let d = drive(&conn, "HUJ808A5L4");
        let other = drive(&conn, "OTHERDRIVE");
        let a = register(&conn, "CLNU01", 50).unwrap();
        register(&conn, "CLNU02", 3).unwrap();

        assert_eq!(standing(&conn, "CLNU01").unwrap().unwrap().uses, 0);
        record(&conn, d, "HUJ808A5L4", "/dev/x", Some(&a), Some("flag")).unwrap();
        record(&conn, other, "OTHERDRIVE", "/dev/y", Some(&a), Some("chip")).unwrap();
        record(&conn, d, "HUJ808A5L4", "/dev/x", None, None).unwrap();

        let s = standing(&conn, "CLNU01").unwrap().unwrap();
        assert_eq!((s.uses, s.remaining, s.life), (2, 48, Life::Ok));
        assert!(s.last_used.is_some());
        let untouched = standing(&conn, "CLNU02").unwrap().unwrap();
        assert_eq!(untouched.uses, 0, "another cartridge's uses are its own");

        let drives = drive_cleanings(&conn).unwrap();
        let find_drive = |serial: &str| drives.iter().find(|x| x.drive_serial == serial).unwrap();
        assert_eq!(
            find_drive("HUJ808A5L4").cleanings,
            2,
            "a cleaning with no cartridge counts"
        );
        assert_eq!(find_drive("OTHERDRIVE").cleanings, 1);
        assert!(find_drive("HUJ808A5L4").last_cleaned.is_some());
    }

    #[test]
    fn a_cartridge_is_near_its_life_then_spent() {
        let conn = conn();
        let d = drive(&conn, "D1");
        let c = register(&conn, "CLN", 7).unwrap();
        let life = |conn: &Connection| standing(conn, "CLN").unwrap().unwrap();
        record(&conn, d, "D1", "/dev/x", Some(&c), None).unwrap();
        assert_eq!(life(&conn).life, Life::Ok, "6 left");
        record(&conn, d, "D1", "/dev/x", Some(&c), None).unwrap();
        let near = life(&conn);
        assert_eq!((near.life, near.remaining), (Life::Near, 5));
        assert!(near.warning().unwrap().contains("near its rated life"));
        for _ in 0..5 {
            record(&conn, d, "D1", "/dev/x", Some(&c), None).unwrap();
        }
        let spent = life(&conn);
        assert_eq!((spent.life, spent.remaining), (Life::Spent, 0));
        assert!(spent.warning().unwrap().contains("replace it"));
        record(&conn, d, "D1", "/dev/x", Some(&c), None).unwrap();
        assert_eq!(
            life(&conn).remaining,
            -1,
            "past the rated life stays honest"
        );
        assert!(
            life(&conn).line().contains("0 left"),
            "{}",
            life(&conn).line()
        );
    }

    #[test]
    fn registration_refuses_a_duplicate_a_blank_serial_and_a_zero_life() {
        let conn = conn();
        register(&conn, "CLN", 50).unwrap();
        let dup = register(&conn, "CLN", 10).unwrap_err().to_string();
        assert!(
            dup.contains("already registered") && dup.contains("50"),
            "{dup}"
        );
        assert_eq!(find(&conn, "CLN").unwrap().unwrap().rated_uses, 50);
        assert!(register(&conn, "  ", 50).is_err());
        assert!(register(&conn, "X", 0).is_err());
        assert!(
            find(&conn, "X").unwrap().is_none(),
            "positive control: X was refused"
        );
        // Whitespace around a typed serial is not part of it.
        assert_eq!(register(&conn, " Y ", 50).unwrap().serial, "Y");
    }

    /// A catalog that never used the feature prints and returns nothing; the
    /// first registration, or the first cleaning, brings the block in.
    #[test]
    fn the_block_is_absent_until_the_feature_is_used() {
        let conn = conn();
        let d = drive(&conn, "D1");
        assert!(block(&conn).unwrap().is_empty());
        assert!(json(&conn).unwrap().is_none());

        record(&conn, d, "D1", "/dev/x", None, None).unwrap();
        let lines = block(&conn).unwrap();
        assert!(lines[0].contains("Drive cleanings"), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("drive D1: last cleaned")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("no cleaning cartridge is registered")),
            "{lines:?}"
        );

        let conn2 = self::conn();
        register(&conn2, "CLN", 50).unwrap();
        drive(&conn2, "NEVER");
        let lines = block(&conn2).unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("drive NEVER: no cleaning recorded")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("CLN: 0 of 50 rated uses recorded, 50 left")),
            "{lines:?}"
        );
        let v = json(&conn2).unwrap().unwrap();
        assert_eq!(v["cleaning_cartridges"][0]["serial"], "CLN");
        assert_eq!(v["drives"][0]["last_cleaned"], serde_json::Value::Null);
    }
}
