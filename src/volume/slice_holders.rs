//! Who else holds a slice (ADR-0012 item 101, issue #480).
//!
//! Copies are written from one staging, so every copy of a stage set carries
//! the same ciphertext for a slice, and the catalog records the same
//! `sha256_encrypted` for it. A slice that is damaged on one cartridge can be
//! read from any other cartridge whose completed write holds a slice with
//! that hash: matched by the hash, never by position or number.
//!
//! "Held" and "live" are `policy::coverage`'s predicates, composed here and
//! never spelled out again ([`coverage::in_service`] for a cartridge nothing
//! is known against, [`coverage::holds_bytes_to_verify`] for one that still
//! holds bytes but is quarantined and so only a last resort): this module
//! adds no `volumes.status` test of its own.
//!
//! Also here: the two other catalog questions the substitution needs, which
//! cartridge a label is (its sticker and chip serial, for the prompt) and
//! which other stage sets of the same version hold the same files (item 102).

use rusqlite::{params, Connection};

use crate::error::Result;
use crate::policy::coverage;

/// How far a holder can be trusted to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Standing {
    /// In service: a sealed (or imported) volume nothing is known against.
    InService,
    /// Quarantined: its bytes are still there, but a verify proved the
    /// medium bad. A last resort, said so.
    Quarantined,
}

/// The slice at `wp.position` lies on its volume's tape: at or before the
/// volume's `ended_after` when the tape sealed short (#420). A repair source
/// is only ever a slice of a `completed` write, which holds every slice at
/// or before `ended_after` by construction; this says so in the query too,
/// so a catalog that broke that (which `audit` names) sends no read past
/// the seal of the other tape (`volume-format-v2.md` §4.2 R5). A `cut`
/// write's slices are never a source, even those at or before
/// `ended_after`: §4.2 "Nothing of a cut unit counts on this tape, and no
/// reader restores it from this tape, even though its slices at or before
/// `ended_after` are present and hash correctly".
const ON_TAPE: &str =
    "(v.ended_after IS NULL OR CAST(wp.position AS INTEGER) <= v.ended_after)";

/// One volume holding a completed, written copy of a slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub volume_id: i64,
    pub label: String,
    /// The tape file number of the slice on that volume.
    pub position: u32,
    pub standing: Standing,
}

/// Every volume that holds a completed write of a slice whose ciphertext
/// hashes to `sha256_encrypted`, in-service ones first, each tier in volume
/// order. A volume the catalog has given up (retired, erased) holds nothing.
pub fn holders_of(conn: &Connection, sha256_encrypted: &str) -> Result<Vec<Holder>> {
    let in_service = coverage::in_service("v");
    let mut stmt = conn.prepare(&format!(
        "SELECT v.id, v.label, wp.position, {in_service} AS in_service
           FROM write_positions wp
           JOIN writes w ON w.id = wp.write_id
           JOIN stage_slices sl ON sl.id = wp.stage_slice_id
           JOIN volumes v ON v.id = w.volume_id
          WHERE sl.sha256_encrypted = ?1
            AND w.status = 'completed' AND wp.status = 'written'
            AND {on_tape}
            AND ({in_service} OR {bytes})
          ORDER BY (NOT {in_service}), v.id",
        on_tape = ON_TAPE,
        bytes = coverage::holds_bytes_to_verify("v"),
    ))?;
    let rows = stmt.query_map(params![sha256_encrypted], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, bool>(3)?,
        ))
    })?;
    let mut holders = Vec::new();
    for row in rows {
        let (volume_id, label, position, in_service) = row?;
        // A position the catalog cannot read as a tape file is no holder.
        let Ok(position) = position.parse::<u32>() else {
            continue;
        };
        holders.push(Holder {
            volume_id,
            label,
            position,
            standing: if in_service {
                Standing::InService
            } else {
                Standing::Quarantined
            },
        });
    }
    Ok(holders)
}

/// Whether any live volume other than `volume_id` holds a completed copy of
/// any slice of `stage_set_id` (by ciphertext hash): the question a restore
/// asks once, before it reads, to know whether a damaged slice has anywhere
/// else to come from. When nothing does, it can take what authenticates in
/// the one pass; when something does, it reads strictly so the swap can be
/// offered.
pub fn other_copies_exist(conn: &Connection, stage_set_id: i64, volume_id: i64) -> Result<bool> {
    let in_service = coverage::in_service("v");
    Ok(conn.query_row(
        &format!(
            "SELECT EXISTS(
                 SELECT 1 FROM stage_slices mine
                   JOIN stage_slices theirs ON theirs.sha256_encrypted = mine.sha256_encrypted
                   JOIN write_positions wp ON wp.stage_slice_id = theirs.id
                   JOIN writes w ON w.id = wp.write_id
                   JOIN volumes v ON v.id = w.volume_id
                  WHERE mine.stage_set_id = ?1 AND w.volume_id <> ?2
                    AND w.status = 'completed' AND wp.status = 'written'
                    AND {on_tape}
                    AND ({in_service} OR {bytes}))",
            on_tape = ON_TAPE,
            bytes = coverage::holds_bytes_to_verify("v"),
        ),
        params![stage_set_id, volume_id],
        |r| r.get(0),
    )?)
}

/// What the operator needs to pick a cartridge off the shelf: the sticker
/// (the barcode, which `cartridge relabel` may have changed) and the chip
/// serial, either of which the catalog may not know.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Shelf {
    pub barcode: Option<String>,
    pub serial: Option<String>,
}

impl Shelf {
    /// `cartridge ABC123, chip serial XYZ`, or what of it is known.
    pub fn describe(&self) -> String {
        match (&self.barcode, &self.serial) {
            (Some(b), Some(s)) => format!("cartridge {b}, chip serial {s}"),
            (Some(b), None) => format!("cartridge {b}, chip serial not recorded"),
            (None, Some(s)) => format!("chip serial {s}"),
            (None, None) => "no cartridge recorded for it".to_string(),
        }
    }
}

/// The cartridge bound to volume `label`, as the catalog knows it.
pub fn shelf_of(conn: &Connection, volume_id: i64, label: &str) -> Shelf {
    match crate::volume::binding::claim_for_volume(conn, volume_id, label) {
        Ok(claim) => match claim.bound {
            Some(b) => Shelf {
                barcode: Some(b.barcode),
                serial: b.serial_number,
            },
            None => Shelf::default(),
        },
        Err(_) => Shelf::default(),
    }
}

/// One stage set of a snapshot, and the volumes that hold a completed write
/// of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageCopy {
    pub stage_set_id: i64,
    pub holders: Vec<StageHolder>,
}

/// A volume holding a stage set, as [`Holder`] but with no slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageHolder {
    pub volume_id: i64,
    pub label: String,
    pub standing: Standing,
}

/// The OTHER stage sets of `snapshot_id` (one unit, one version) than
/// `except_stage_set_id` that a live volume holds a completed write of
/// (ADR-0012 item 102): the same files, cut into different slices. Newest
/// stage set first; its volumes in-service first. A stage set no volume
/// holds is not listed.
pub fn other_copies_of_version(
    conn: &Connection,
    snapshot_id: i64,
    except_stage_set_id: i64,
) -> Result<Vec<StageCopy>> {
    let in_service = coverage::in_service("v");
    let mut stmt = conn.prepare(&format!(
        "SELECT w.stage_set_id, v.id, v.label, {in_service} AS in_service
           FROM writes w
           JOIN stage_sets ss ON ss.id = w.stage_set_id
           JOIN volumes v ON v.id = w.volume_id
          WHERE ss.snapshot_id = ?1 AND ss.id <> ?2
            AND w.status = 'completed'
            AND ({in_service} OR {bytes})
          ORDER BY w.stage_set_id DESC, (NOT {in_service}), v.id",
        bytes = coverage::holds_bytes_to_verify("v"),
    ))?;
    let rows = stmt.query_map(params![snapshot_id, except_stage_set_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, bool>(3)?,
        ))
    })?;
    let mut copies: Vec<StageCopy> = Vec::new();
    for row in rows {
        let (stage_set_id, volume_id, label, in_service) = row?;
        let holder = StageHolder {
            volume_id,
            label,
            standing: if in_service {
                Standing::InService
            } else {
                Standing::Quarantined
            },
        };
        match copies.last_mut() {
            Some(c) if c.stage_set_id == stage_set_id => c.holders.push(holder),
            _ => copies.push(StageCopy {
                stage_set_id,
                holders: vec![holder],
            }),
        }
    }
    Ok(copies)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two volumes holding the same slice (one stage set written twice), a
    /// third holding a different stage set of the same snapshot.
    fn catalog() -> Connection {
        let conn = crate::db::open_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO tenants (id, name, is_operator, status) VALUES (1, 't', 0, 'active');
             INSERT INTO units (id, uuid, name, tenant_id) VALUES (1, 'u', 'u', 1);
             INSERT INTO snapshots (id, unit_id, version, source_path) VALUES (1, 1, 1, '/s');
             INSERT INTO stage_sets (id, snapshot_id, status, slice_size) VALUES (1, 1, 'staged', 1000);
             INSERT INTO stage_sets (id, snapshot_id, status, slice_size) VALUES (2, 1, 'staged', 1000);
             INSERT INTO stage_slices (id, stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                       sha256_plain, sha256_encrypted)
               VALUES (1, 1, 1, 10, 20, 'p1', 'aaaa');
             INSERT INTO stage_slices (id, stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                       sha256_plain, sha256_encrypted)
               VALUES (2, 2, 1, 10, 20, 'p2', 'bbbb');
             INSERT INTO volumes (id, label, backend_type, backend_name, capacity_bytes, status)
               VALUES (1, 'V1', 'lto', 'p', 1000, 'sealed');
             INSERT INTO volumes (id, label, backend_type, backend_name, capacity_bytes, status)
               VALUES (2, 'V2', 'lto', 'p', 1000, 'sealed');
             INSERT INTO volumes (id, label, backend_type, backend_name, capacity_bytes, status)
               VALUES (3, 'V3', 'lto', 'p', 1000, 'sealed');
             INSERT INTO writes (id, stage_set_id, snapshot_id, volume_id, status)
               VALUES (1, 1, 1, 1, 'completed');
             INSERT INTO writes (id, stage_set_id, snapshot_id, volume_id, status)
               VALUES (2, 1, 1, 2, 'completed');
             INSERT INTO writes (id, stage_set_id, snapshot_id, volume_id, status)
               VALUES (3, 2, 1, 3, 'completed');
             INSERT INTO write_positions (write_id, stage_slice_id, position, status)
               VALUES (1, 1, '4', 'written');
             INSERT INTO write_positions (write_id, stage_slice_id, position, status)
               VALUES (2, 1, '7', 'written');
             INSERT INTO write_positions (write_id, stage_slice_id, position, status)
               VALUES (3, 2, '9', 'written');",
        )
        .unwrap();
        conn
    }

    fn labels(h: &[Holder]) -> Vec<(&str, u32, Standing)> {
        h.iter()
            .map(|h| (h.label.as_str(), h.position, h.standing))
            .collect()
    }

    #[test]
    fn a_slice_is_held_by_every_volume_that_wrote_its_hash() {
        let conn = catalog();
        assert_eq!(
            labels(&holders_of(&conn, "aaaa").unwrap()),
            vec![
                ("V1", 4, Standing::InService),
                ("V2", 7, Standing::InService)
            ]
        );
        // The other stage set's slice is held by its own volume alone.
        assert_eq!(
            labels(&holders_of(&conn, "bbbb").unwrap()),
            vec![("V3", 9, Standing::InService)]
        );
        assert!(holders_of(&conn, "nothing").unwrap().is_empty());
    }

    #[test]
    fn a_quarantined_volume_is_last_and_a_given_up_one_is_not_a_holder() {
        let conn = catalog();
        conn.execute(
            "UPDATE volumes SET observed_condition = 'quarantined' WHERE label = 'V1'",
            [],
        )
        .unwrap();
        assert_eq!(
            labels(&holders_of(&conn, "aaaa").unwrap()),
            vec![
                ("V2", 7, Standing::InService),
                ("V1", 4, Standing::Quarantined)
            ],
            "in-service first, the quarantined volume as the last resort"
        );
        conn.execute(
            "UPDATE volumes SET status = 'retired' WHERE label = 'V2'",
            [],
        )
        .unwrap();
        assert_eq!(
            labels(&holders_of(&conn, "aaaa").unwrap()),
            vec![("V1", 4, Standing::Quarantined)]
        );
        conn.execute(
            "UPDATE volumes SET status = 'erased' WHERE label = 'V1'",
            [],
        )
        .unwrap();
        assert!(holders_of(&conn, "aaaa").unwrap().is_empty());
    }

    #[test]
    fn a_write_that_never_completed_holds_nothing() {
        let conn = catalog();
        conn.execute(
            "UPDATE writes SET status = 'interrupted' WHERE volume_id = 2",
            [],
        )
        .unwrap();
        assert_eq!(
            labels(&holders_of(&conn, "aaaa").unwrap()),
            vec![("V1", 4, Standing::InService)]
        );
        conn.execute(
            "UPDATE write_positions SET status = 'failed' WHERE write_id = 1",
            [],
        )
        .unwrap();
        assert!(holders_of(&conn, "aaaa").unwrap().is_empty());
    }

    #[test]
    fn the_other_stage_sets_of_a_version_are_listed_with_their_volumes() {
        let conn = catalog();
        let copies = other_copies_of_version(&conn, 1, 1).unwrap();
        assert_eq!(copies.len(), 1);
        assert_eq!(copies[0].stage_set_id, 2);
        assert_eq!(
            copies[0]
                .holders
                .iter()
                .map(|h| h.label.as_str())
                .collect::<Vec<_>>(),
            vec!["V3"]
        );
        // Seen from the other stage set, the first (on two volumes) is the other.
        let copies = other_copies_of_version(&conn, 1, 2).unwrap();
        assert_eq!(copies.len(), 1);
        assert_eq!(copies[0].stage_set_id, 1);
        assert_eq!(copies[0].holders.len(), 2);
    }

    #[test]
    fn other_copies_exist_only_where_another_live_volume_wrote_the_hash() {
        let conn = catalog();
        // Stage set 1 is on V1 and V2: from V1 there is another, and from a
        // volume that is not one of them, both.
        assert!(other_copies_exist(&conn, 1, 1).unwrap());
        assert!(other_copies_exist(&conn, 1, 2).unwrap());
        // Stage set 2 is on V3 alone.
        assert!(!other_copies_exist(&conn, 2, 3).unwrap());
        conn.execute(
            "UPDATE volumes SET status = 'retired' WHERE label = 'V2'",
            [],
        )
        .unwrap();
        assert!(
            !other_copies_exist(&conn, 1, 1).unwrap(),
            "retired holds nothing"
        );
    }

    #[test]
    fn a_shelf_names_the_sticker_and_the_chip() {
        let shelf = Shelf {
            barcode: Some("L6-0002".into()),
            serial: Some("ABC".into()),
        };
        assert_eq!(shelf.describe(), "cartridge L6-0002, chip serial ABC");
        assert_eq!(Shelf::default().describe(), "no cartridge recorded for it");
        let conn = catalog();
        assert_eq!(shelf_of(&conn, 1, "V1"), Shelf::default(), "no binding");
    }
}
