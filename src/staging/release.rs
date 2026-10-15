//! Releasing staged data once the copies written satisfy policy — the ONE
//! place that decision lives (issue #440).
//!
//! `clean_staging` stays policy-free (ADR-0012's #244 amendment): it knows
//! which stage sets are safe to delete (a completed write on every row), not
//! which are *worth* deleting. This module is the gate in front of it. Two
//! callers share it: `collection run`'s batch (via
//! `collection::batch::release_if_covered`) and `volume write` / `volume
//! resume` (via `cli::volume`), each right after a copy is sealed AND
//! confirmed (ADR-0012 2026-10-06/07, items 2 and 33: staging is released
//! after a quick confirm too, and a failed release is a warning, never a
//! failed write).
//!
//! The rule is all-or-nothing over the units given, as `collection run` has
//! always had it: stage once, write N times (`v2-open-questions.md` §11)
//! means the next copy must find the same staged content this one wrote, so
//! one short unit keeps every unit's staging. `staging clean` is the
//! per-unit tool — it releases the covered units and retains the short ones.

use rusqlite::{params, Connection};

use crate::config::Config;
use crate::error::{Result, TapectlError};
use crate::policy::coverage::{copy_count_expr, copy_floor, tape_only_note, CoverageQuery};
use crate::staging::clean::{clean_staging, CleanReport, CleanScope};

/// One unit's copy count against its own resolved `min_copies`, computed
/// AFTER the write(s) landed. Only reported when the release did NOT happen
/// (see [`ReleaseOutcome::under_copied`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyProgress {
    pub unit_name: String,
    pub copies: i64,
    /// The copy floor: the resolved `min_copies`, times the tape-only
    /// multiplier for a tape-only unit ([`copy_floor`]).
    pub min_copies: i64,
    /// `" (tape-only 3x)"` when the multiplier raised the floor, empty
    /// otherwise ([`tape_only_note`]), so [`CopyProgress::describe`] never
    /// shows a number the policy alone does not explain.
    pub floor_note: String,
}

impl CopyProgress {
    /// `unit alpha has 1 copy, needs 2` — why a release was refused.
    pub fn describe(&self) -> String {
        let noun = if self.copies == 1 { "copy" } else { "copies" };
        format!(
            "unit {} has {} {noun}, needs {}{}",
            self.unit_name, self.copies, self.min_copies, self.floor_note
        )
    }
}

/// What [`release_if_covered`] decided and did.
#[derive(Debug)]
pub struct ReleaseOutcome {
    /// `Some` when staging was released (every unit now meets its own
    /// resolved `min_copies`); `None` when it was deliberately retained.
    pub cleaned: Option<CleanReport>,
    /// Non-empty exactly when `cleaned` is `None`: which units are still
    /// short, and by how much.
    pub under_copied: Vec<CopyProgress>,
}

/// Releases the staged data of `unit_ids` only when every one of those units
/// now meets its own resolved `min_copies`; otherwise retains it all and
/// says which units are short.
///
/// The release itself is `clean_staging(force = false, CleanScope::Units)`,
/// so its own guard still applies and is what keeps a set that has no
/// completed write — or any write that is not `completed` — staged. This
/// function adds the policy question that guard cannot answer (issue #229's
/// second half): after one copy of a unit whose `min_copies` is 2, the
/// guard passes vacuously (one `writes` row, `completed`) and would release
/// the only cheap route to the second copy.
///
/// Issue #248: `clean_staging`'s eligibility SQL has no batch/unit predicate
/// of its own, so `CleanScope::Whole` here would release EVERY eligible
/// `'staged'` set in the database the moment this gate passed — including a
/// different unit that is still short. `CleanScope::Units(unit_ids)`
/// narrows the release to exactly the units the gate looked at.
pub fn release_if_covered(
    conn: &Connection,
    config: &Config,
    unit_ids: &[i64],
) -> Result<ReleaseOutcome> {
    let under_copied = under_copied_units(conn, config, unit_ids)?;
    let cleaned = if under_copied.is_empty() {
        Some(clean_staging(
            conn,
            config,
            false,
            CleanScope::Units(unit_ids),
        )?)
    } else {
        None
    };
    Ok(ReleaseOutcome {
        cleaned,
        under_copied,
    })
}

/// Which of `unit_ids` still have fewer eligible copies than their own
/// resolved `min_copies`, in `unit_ids` order.
///
/// Routed through `policy::coverage::copy_count_expr` — the declared sole
/// owner of "how many copies does this unit have" (issue #96's
/// derivation-discipline rule) — with the call shape `collection::status`
/// uses for its own `under_copied` count, so the surfaces cannot disagree
/// about what "under-copied" means. `CoverageQuery::current_unit` takes the
/// MINIMUM across the unit's current snapshots, so a unit is as covered as
/// its least-covered live version (ADR-0012), against eligible (`sealed`)
/// volumes; a write promotes its snapshot to `'current'` and its volume to
/// `'sealed'` at seal/confirm time (`volume::session`), so the copy a write
/// just finished is counted here, not missed. The floor is
/// [`copy_floor`]: the one tape-only rule (ADR-0012 2026-10-12 item 43).
pub fn under_copied_units(
    conn: &Connection,
    config: &Config,
    unit_ids: &[i64],
) -> Result<Vec<CopyProgress>> {
    let mut under = Vec::new();
    for &unit_id in unit_ids {
        let name: String = conn
            .query_row(
                "SELECT name FROM units WHERE id = ?1",
                params![unit_id],
                |row| row.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => TapectlError::Other(format!(
                    "releasing staging: unit id {unit_id} is missing from the catalog — \
                     the catalog is inconsistent"
                )),
                other => other.into(),
            })?;
        let unit = crate::db::queries::get_unit_by_name(conn, &name)?.ok_or_else(|| {
            TapectlError::Other(format!(
                "releasing staging: unit \"{name}\" is missing from the catalog — \
                 the catalog is inconsistent"
            ))
        })?;
        let resolved = crate::policy::resolve(conn, config, &unit)?;
        let sql = format!(
            "SELECT {}",
            copy_count_expr(&CoverageQuery::current_unit("?1"))
        );
        let copies: i64 = conn.query_row(&sql, params![unit.id], |row| row.get(0))?;
        let min_copies = copy_floor(config, resolved.min_copies, &unit.status);
        if copies < min_copies {
            under.push(CopyProgress {
                unit_name: name,
                copies,
                min_copies,
                floor_note: tape_only_note(config, &unit.status),
            });
        }
    }
    Ok(under)
}

/// The ids of the units that have a `completed` write on the volume labelled
/// `volume_label`, in id order — the units a `volume write` or `volume
/// resume` of that volume has just put on tape, and so the units
/// [`release_if_covered`] must judge together.
///
/// This is a lookup, not a decision: it says which units the gate looks at
/// and which stage sets `clean_staging` is allowed to consider, never
/// whether any of them may be released.
pub fn unit_ids_written_to(conn: &Connection, volume_label: &str) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT sn.unit_id
         FROM writes w
         JOIN volumes v ON v.id = w.volume_id
         JOIN stage_sets ss ON ss.id = w.stage_set_id
         JOIN snapshots sn ON sn.id = ss.snapshot_id
         WHERE v.label = ?1 AND w.status = 'completed'
         ORDER BY sn.unit_id",
    )?;
    let ids = stmt
        .query_map(params![volume_label], |row| row.get(0))?
        .collect::<std::result::Result<Vec<i64>, _>>()?;
    Ok(ids)
}
