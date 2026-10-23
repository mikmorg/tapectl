//! Who else holds a slice (ADR-0012 item 101, issue #480).
//!
//! Copies are written from one staging, so every copy of a stage set carries
//! the same ciphertext for a slice, and the catalog records the same
//! `sha256_encrypted` for it. A slice that is damaged on one cartridge can be
//! read from any other cartridge whose write holds a slice with that hash:
//! matched by the hash, never by position or number. From 1.7.0 (#478) a
//! holder is a `completed` write or a `range` write with the slice
//! `written` (item 101 sees ranges; `volume-format-v2.md` §4.4 R8), so the
//! same answer serves a damaged slice and a version read across cartridges.
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
/// volume's `ended_after` when the tape sealed short (#420). A source is
/// only ever a `written` slice of a `completed` write, which holds every
/// slice at or before `ended_after` by construction, or of a `range` write,
/// whose slices are `written` up to the tape's end and `pending` past it
/// (#478, item 121). This says so in the query too, so a catalog that broke
/// that (which `audit` names) sends no read past the seal of the other tape:
/// tapectl takes `ended_after` from the catalog, which recorded the tape's
/// seal when it sealed or was rebuilt (`volume-format-v2.md` §4.2 R1 and
/// R5). A `cut` write's slices are never a source, even those at or before
/// `ended_after`: §4.2 "Nothing of a cut unit counts on this tape, and no
/// reader restores it from this tape, even though its slices at or before
/// `ended_after` are present and hash correctly".
const ON_TAPE: &str = "(v.ended_after IS NULL OR CAST(wp.position AS INTEGER) <= v.ended_after)";

/// The one test of "the volume `v` holds the slice `wp` of the write `w`",
/// over the aliases `w`, `wp` and `v`: a `completed` or `range` write, the
/// position `written` and on the tape ([`ON_TAPE`]), and a volume in service
/// or, as a last resort, quarantined but still holding its bytes. Every
/// question this module answers about a slice's holders is asked with it.
fn holds() -> String {
    format!(
        "w.status IN ('completed', 'range') AND wp.status = 'written' AND {ON_TAPE}
         AND ({in_service} OR {bytes})",
        in_service = coverage::in_service("v"),
        bytes = coverage::holds_bytes_to_verify("v"),
    )
}

/// One volume holding a written copy of a slice, in a completed write or a
/// range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub volume_id: i64,
    pub label: String,
    /// The tape file number of the slice on that volume.
    pub position: u32,
    pub standing: Standing,
}

/// Every volume that holds a slice whose ciphertext hashes to
/// `sha256_encrypted` ([`holds`]: a completed write, or a range with the
/// slice written), in-service ones first, each tier in volume order. A
/// volume the catalog has given up (retired, erased) holds nothing.
pub fn holders_of(conn: &Connection, sha256_encrypted: &str) -> Result<Vec<Holder>> {
    let in_service = coverage::in_service("v");
    let mut stmt = conn.prepare(&format!(
        "SELECT v.id, v.label, wp.position, {in_service} AS in_service
           FROM write_positions wp
           JOIN writes w ON w.id = wp.write_id
           JOIN stage_slices sl ON sl.id = wp.stage_slice_id
           JOIN volumes v ON v.id = w.volume_id
          WHERE sl.sha256_encrypted = ?1
            AND {holds}
          ORDER BY (NOT {in_service}), v.id",
        holds = holds(),
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

/// Whether any live volume other than `volume_id` holds any slice of
/// `stage_set_id` (by ciphertext hash, [`holds`]: a range included): the
/// question a restore asks once, before it reads, to know whether a damaged
/// slice has anywhere else to come from. When nothing does, it can take what
/// authenticates in the one pass; when something does, it reads strictly so
/// the swap can be offered.
pub fn other_copies_exist(conn: &Connection, stage_set_id: i64, volume_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        &format!(
            "SELECT EXISTS(
                 SELECT 1 FROM stage_slices mine
                   JOIN stage_slices theirs ON theirs.sha256_encrypted = mine.sha256_encrypted
                   JOIN write_positions wp ON wp.stage_slice_id = theirs.id
                   JOIN writes w ON w.id = wp.write_id
                   JOIN volumes v ON v.id = w.volume_id
                  WHERE mine.stage_set_id = ?1 AND w.volume_id <> ?2
                    AND {holds})",
            holds = holds(),
        ),
        params![stage_set_id, volume_id],
        |r| r.get(0),
    )?)
}

/// One slice of a stage set and a volume holding it ([`holders_of_set`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceHolder {
    pub slice_number: i64,
    pub holder: Holder,
}

/// Every holder of every slice of `stage_set_id`, by each slice's ciphertext
/// hash and [`holds`], as [`holders_of`] answers for one slice: in slice
/// order, and within a slice in-service volumes first, each tier in volume
/// order. One statement for the whole set: what a restore across cartridges
/// plans from (#478, ADR-0012 item 131). A slice no live volume holds has no
/// row.
pub fn holders_of_set(conn: &Connection, stage_set_id: i64) -> Result<Vec<SliceHolder>> {
    let in_service = coverage::in_service("v");
    let mut stmt = conn.prepare(&format!(
        "SELECT mine.slice_number, v.id, v.label, wp.position, {in_service} AS in_service
           FROM stage_slices mine
           JOIN stage_slices sl ON sl.sha256_encrypted = mine.sha256_encrypted
           JOIN write_positions wp ON wp.stage_slice_id = sl.id
           JOIN writes w ON w.id = wp.write_id
           JOIN volumes v ON v.id = w.volume_id
          WHERE mine.stage_set_id = ?1
            AND {holds}
          ORDER BY mine.slice_number, (NOT {in_service}), v.id",
        holds = holds(),
    ))?;
    let rows = stmt.query_map(params![stage_set_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, bool>(4)?,
        ))
    })?;
    let mut held = Vec::new();
    for row in rows {
        let (slice_number, volume_id, label, position, in_service) = row?;
        let Ok(position) = position.parse::<u32>() else {
            continue;
        };
        let holder = Holder {
            volume_id,
            label,
            position,
            standing: if in_service {
                Standing::InService
            } else {
                Standing::Quarantined
            },
        };
        // One volume holds a slice once; a second row would be the same
        // ciphertext recorded twice on it.
        if !held.iter().any(|h: &SliceHolder| {
            h.slice_number == slice_number && h.holder.volume_id == volume_id
        }) {
            held.push(SliceHolder {
                slice_number,
                holder,
            });
        }
    }
    Ok(held)
}

/// Where the catalog last saw a slice of a stage set that no live volume
/// holds ([`traces_of_set`]): one write's record of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceTrace {
    pub slice_number: i64,
    pub label: String,
    /// The volume is given up: retired or erased ([`coverage::given_up`]).
    pub given_up: bool,
    /// The volume is in service ([`coverage::in_service`]).
    pub in_service: bool,
    /// `writes.status`.
    pub write_status: String,
    /// `write_positions.status`: `written`, or `pending` for a slice planned
    /// on a tape that ended before it.
    pub position_status: String,
}

/// Every write that recorded a slice of `stage_set_id` (by ciphertext hash),
/// whatever became of it: what a refusal names when a slice has no holder
/// (#478, "say what would hold them"). In slice order, then volume order.
pub fn traces_of_set(conn: &Connection, stage_set_id: i64) -> Result<Vec<SliceTrace>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT mine.slice_number, v.label, {given_up}, {in_service}, w.status, wp.status
           FROM stage_slices mine
           JOIN stage_slices sl ON sl.sha256_encrypted = mine.sha256_encrypted
           JOIN write_positions wp ON wp.stage_slice_id = sl.id
           JOIN writes w ON w.id = wp.write_id
           JOIN volumes v ON v.id = w.volume_id
          WHERE mine.stage_set_id = ?1
          ORDER BY mine.slice_number, v.id",
        given_up = coverage::given_up("v"),
        in_service = coverage::in_service("v"),
    ))?;
    let rows = stmt
        .query_map(params![stage_set_id], |r| {
            Ok(SliceTrace {
                slice_number: r.get(0)?,
                label: r.get(1)?,
                given_up: r.get(2)?,
                in_service: r.get(3)?,
                write_status: r.get(4)?,
                position_status: r.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
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
///
/// Completed writes only, never a `range` (ADR-0012 item 143, #478): a
/// damaged file is filled only from a copy held whole on one tape. The
/// ranges this leaves out are [`ranges_of_version`]'s, which the fill names
/// in its refusal.
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

/// One `range` write of another stage set of a version: what the fill names
/// when it cannot use it (ADR-0012 item 143).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeCopy {
    pub stage_set_id: i64,
    pub label: String,
    /// The first and last slice the range holds written.
    pub first: i64,
    pub last: i64,
}

/// The `range` writes, on live volumes, of the OTHER stage sets of
/// `snapshot_id` than `except_stage_set_id`, with the run each holds: the
/// copies [`other_copies_of_version`] leaves out because a range is never a
/// fill source (ADR-0012 item 143). Newest stage set first, then volume
/// order.
pub fn ranges_of_version(
    conn: &Connection,
    snapshot_id: i64,
    except_stage_set_id: i64,
) -> Result<Vec<RangeCopy>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT w.stage_set_id, v.label, MIN(sl.slice_number), MAX(sl.slice_number)
           FROM writes w
           JOIN stage_sets ss ON ss.id = w.stage_set_id
           JOIN volumes v ON v.id = w.volume_id
           JOIN write_positions wp ON wp.write_id = w.id
           JOIN stage_slices sl ON sl.id = wp.stage_slice_id
          WHERE ss.snapshot_id = ?1 AND ss.id <> ?2
            AND w.status = 'range' AND {holds}
          GROUP BY w.id
          ORDER BY w.stage_set_id DESC, v.id",
        holds = holds(),
    ))?;
    let rows = stmt
        .query_map(params![snapshot_id, except_stage_set_id], |r| {
            Ok(RangeCopy {
                stage_set_id: r.get(0)?,
                label: r.get(1)?,
                first: r.get(2)?,
                last: r.get(3)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
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

    /// Stage set 1 gets a second slice (`cccc`), and V3 holds a `range` of it:
    /// slice 2 written, slice 1 planned but `pending` (its tape ended first).
    fn with_a_range(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO stage_slices (id, stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                       sha256_plain, sha256_encrypted)
               VALUES (3, 1, 2, 10, 20, 'p3', 'cccc');
             INSERT INTO volumes (id, label, backend_type, backend_name, capacity_bytes, status)
               VALUES (4, 'V4', 'lto', 'p', 1000, 'sealed');
             INSERT INTO writes (id, stage_set_id, snapshot_id, volume_id, status, may_span)
               VALUES (4, 1, 1, 4, 'range', 1);
             INSERT INTO write_positions (write_id, stage_slice_id, position, status)
               VALUES (4, 3, '5', 'written');
             INSERT INTO write_positions (write_id, stage_slice_id, position, status)
               VALUES (4, 1, '6', 'pending');",
        )
        .unwrap();
    }

    /// Item 101 sees ranges (#478): a range's written slice is held, its
    /// pending one is not, and one past its tape's end is not either.
    #[test]
    fn a_range_holds_its_written_slices_and_nothing_else() {
        let conn = catalog();
        with_a_range(&conn);
        assert_eq!(
            labels(&holders_of(&conn, "cccc").unwrap()),
            vec![("V4", 5, Standing::InService)],
            "the range's written slice"
        );
        assert_eq!(
            labels(&holders_of(&conn, "aaaa").unwrap()),
            vec![
                ("V1", 4, Standing::InService),
                ("V2", 7, Standing::InService)
            ],
            "the slice the range planned but never wrote is not on V4"
        );
        // The range's tape ended before position 5 (a catalog `audit` names):
        // nothing past the seal is a source.
        conn.execute("UPDATE volumes SET ended_after = 4 WHERE id = 4", [])
            .unwrap();
        assert!(holders_of(&conn, "cccc").unwrap().is_empty());
    }

    /// A range is another copy of the slices it holds, for the restore's
    /// one question too.
    #[test]
    fn other_copies_exist_counts_a_range() {
        let conn = catalog();
        with_a_range(&conn);
        conn.execute(
            "UPDATE volumes SET status = 'retired' WHERE id IN (1, 2)",
            [],
        )
        .unwrap();
        // Nothing live holds stage set 1 but V4's range, so from V4 there is
        // no other copy, and from V1 there is (V4).
        assert!(!other_copies_exist(&conn, 1, 4).unwrap());
        assert!(other_copies_exist(&conn, 1, 1).unwrap());
        // The positive control: without the range nothing live holds it.
        conn.execute("UPDATE writes SET status = 'cut' WHERE id = 4", [])
            .unwrap();
        assert!(!other_copies_exist(&conn, 1, 1).unwrap());
    }

    /// The whole set's holders in one answer, slice by slice, the same
    /// tiers as [`holders_of`].
    #[test]
    fn holders_of_set_lists_every_slices_holders_in_slice_order() {
        let conn = catalog();
        with_a_range(&conn);
        conn.execute(
            "UPDATE volumes SET observed_condition = 'quarantined' WHERE id = 1",
            [],
        )
        .unwrap();
        let held = holders_of_set(&conn, 1).unwrap();
        let held: Vec<(i64, &str, u32, Standing)> = held
            .iter()
            .map(|h| {
                (
                    h.slice_number,
                    h.holder.label.as_str(),
                    h.holder.position,
                    h.holder.standing,
                )
            })
            .collect();
        assert_eq!(
            held,
            vec![
                (1, "V2", 7, Standing::InService),
                (1, "V1", 4, Standing::Quarantined),
                (2, "V4", 5, Standing::InService),
            ]
        );
        assert!(holders_of_set(&conn, 99).unwrap().is_empty());
    }

    /// Item 143: the fill's copies are whole writes only, and the ranges it
    /// leaves out are listed for its refusal.
    #[test]
    fn a_range_is_never_a_fill_copy_and_is_listed_as_a_range() {
        let conn = catalog();
        // Stage set 2's only holder becomes a range of it.
        conn.execute(
            "UPDATE writes SET status = 'range', may_span = 1 WHERE id = 3",
            [],
        )
        .unwrap();
        assert!(other_copies_of_version(&conn, 1, 1).unwrap().is_empty());
        assert_eq!(
            ranges_of_version(&conn, 1, 1).unwrap(),
            vec![RangeCopy {
                stage_set_id: 2,
                label: "V3".into(),
                first: 1,
                last: 1,
            }]
        );
        // Seen from stage set 2 itself, its own range is not "another".
        assert!(ranges_of_version(&conn, 1, 2).unwrap().is_empty());
    }

    /// Where a slice no live volume holds was last seen.
    #[test]
    fn traces_name_every_write_of_a_slice_whatever_became_of_it() {
        let conn = catalog();
        with_a_range(&conn);
        conn.execute("UPDATE volumes SET status = 'retired' WHERE id = 2", [])
            .unwrap();
        let traces: Vec<(i64, String, bool, bool, String, String)> = traces_of_set(&conn, 1)
            .unwrap()
            .into_iter()
            .map(|t| {
                (
                    t.slice_number,
                    t.label,
                    t.given_up,
                    t.in_service,
                    t.write_status,
                    t.position_status,
                )
            })
            .collect();
        let row = |n, l: &str, gone: bool, ws: &str, ps: &str| {
            (
                n,
                l.to_string(),
                gone,
                !gone,
                ws.to_string(),
                ps.to_string(),
            )
        };
        assert_eq!(
            traces,
            vec![
                row(1, "V1", false, "completed", "written"),
                row(1, "V2", true, "completed", "written"),
                row(1, "V4", false, "range", "pending"),
                row(2, "V4", false, "range", "written"),
            ]
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
