//! Releasing staged data once the copies written satisfy policy — the ONE
//! place that decision lives (issue #440; per unit since 1.9.0, ADR-0012
//! item 217).
//!
//! `clean_staging` stays policy-free (ADR-0012's #244 amendment): it knows
//! which stage sets are safe to delete (a completed write, and every other
//! row completed or `cut`), not which are *worth* deleting. This module is
//! the gate in front of it. Two
//! callers share it: `collection run`'s batch (via
//! `collection::batch::release_if_covered`) and `volume write` / `volume
//! resume` (via `cli::volume`), each right after a copy is sealed AND
//! confirmed (ADR-0012 2026-10-07, items 2 and 33: staging is released
//! after a quick confirm too, and a failed release is a warning, never a
//! failed write).
//!
//! **The rule is per unit** (ADR-0012 item 217, amending #440's
//! all-or-nothing): every unit judged whose copy floor
//! ([`copy_floor`]) AND every `copies_at` target ([`short_of_copies_at`])
//! are met is released, and every unit still short of either stays staged,
//! named with why. It is the per-unit split `stage clean` already makes,
//! run after each write. Twin copies stay twins without a rule of their
//! own: when copies 1 and 2 carry the same units, every one of them is
//! short until copy 2, and covered after it. On mhvtl the all-or-nothing
//! rule wrote two covered units a third time, held back by a third unit
//! that was short.
//!
//! A `cut` write (1.6.0, issue #420; ADR-0012 item 112) is no write here: a
//! stage set planned past where its tape sealed short. It never counts as a
//! copy (`copy_count_expr` counts only `completed` writes), so a cut unit is
//! short and keeps its staging; and it never holds the staging either
//! (`clean_staging`'s guard skips it), so once the version has a `completed`
//! write elsewhere it is released by the same rule as any other.
//!
//! A stage set that holds a `range` (1.7.0, issue #478; ADR-0012 items 85,
//! 121 and 129) is released slice by slice instead, each slice once its
//! copies meet the set's copy floor, and independently of every other unit
//! the write judges (coordinator ruling X15): tape 1's slices need not wait
//! for tape 3, nor for another unit's next copy. A unit whose staged data is
//! only sets holding a range is not judged whole ([`release_if_covered`]
//! says why).

use rusqlite::{params, Connection, OptionalExtension};

use crate::config::Config;
use crate::error::{Result, TapectlError};
use crate::policy::coverage::{
    copy_count_expr, copy_floor, describe_short_locations, short_of_copies_at, tape_only_note,
    CoverageQuery, LocationShort, NamedScope,
};
use crate::staging::clean::{
    clean_staging, release_blocker, release_covered_slices, slices_short_of,
    staged_sets_holding_a_range, CleanReport, CleanScope, ReleaseBlocker,
};

/// One unit's copy count against its copy floor, and the `copies_at`
/// targets it is still short of, computed AFTER the write(s) landed. Only
/// reported for a unit whose staging was kept (see
/// [`ReleaseOutcome::under_copied`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyProgress {
    pub unit_name: String,
    pub copies: i64,
    /// The copy floor: the resolved `copy_floor_base` (the larger of
    /// `min_copies` and the `copies_at` sum), times the tape-only multiplier
    /// for a tape-only unit ([`copy_floor`]). Named `copy_floor` in JSON.
    pub min_copies: i64,
    /// `" (tape-only 3x)"` when the multiplier raised the floor, empty
    /// otherwise ([`tape_only_note`]), so [`CopyProgress::describe`] never
    /// shows a number the policy alone does not explain.
    pub floor_note: String,
    /// The per-location targets of its unit still unmet
    /// ([`short_of_copies_at`]; #493, ADR-0012 items 149 and 217), in name
    /// order. Empty when every target is met or the unit has none, and
    /// always under [`Hold::CopyFloor`].
    pub short_at: Vec<LocationShort>,
}

impl CopyProgress {
    /// `unit alpha has 1 copy, needs 2` — why its staging was kept; with
    /// its unmet per-location targets after it (#493, ADR-0012 item 217):
    /// `unit alpha has 2 copies, its floor 2; still needed: 0 copies at
    /// required location offsite, needs 1`.
    pub fn describe(&self) -> String {
        let noun = if self.copies == 1 { "copy" } else { "copies" };
        let mut text = if self.copies < self.min_copies || self.short_at.is_empty() {
            format!(
                "unit {} has {} {noun}, needs {}{}",
                self.unit_name, self.copies, self.min_copies, self.floor_note
            )
        } else {
            format!(
                "unit {} has {} {noun}, its floor {}{}",
                self.unit_name, self.copies, self.min_copies, self.floor_note
            )
        };
        if !self.short_at.is_empty() {
            text.push_str(&format!(
                "; still needed: {}",
                describe_short_locations(&self.short_at)
            ));
        }
        text
    }
}

/// What a release after a write waits for, per unit (ADR-0012 items 154
/// and 217).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Hold {
    /// The default since 1.9.0 (ADR-0012 item 217): a unit is released once
    /// its copy floor AND every per-location target of its `copies_at` map
    /// are met. A spanning set's slices are released by the floor alone, one
    /// by one ([`release_ranges`]).
    #[default]
    CopyTargets,
    /// The copy floor alone: 1.7.1's default (ADR-0012 item 154) and `stage
    /// clean`'s rule. No production caller asks for it since item 217; it is
    /// kept so the default is one line ([`release_if_covered`], and
    /// `cli::volume`'s `StagingKeep::Release`).
    CopyFloor,
    /// `--keep-staging-for-locations` (#493, ADR-0012 item 154): as
    /// [`Hold::CopyTargets`] for the units judged whole, and a unit short of
    /// a per-location target also keeps every slice of a set that spans
    /// cartridges, since there is no per-slice per-location count to release
    /// them by.
    EveryLocation,
}

/// What [`release_if_covered`] decided and did.
#[derive(Debug)]
pub struct ReleaseOutcome {
    /// `Some` when `clean_staging` ran: over every unit given when none was
    /// short, otherwise over the covered units alone (ADR-0012 item 217).
    /// `None` when no unit judged whole was covered, so nothing was asked of
    /// it. Only the stage sets with no `range` row: a set holding one is
    /// released slice by slice, in `slices`.
    pub cleaned: Option<CleanReport>,
    /// The units judged whole that were covered and had a stage set
    /// released by this call, by name, in the order given.
    pub released_units: Vec<String>,
    /// The units judged whole that are still short of their floor or of a
    /// per-location target, and by how much: their staging was kept.
    pub under_copied: Vec<CopyProgress>,
    /// The per-slice release of the stage sets that hold a range (1.7.0,
    /// issue #478; ADR-0012 item 129), made whatever the units judged whole
    /// decided (ruling X15): the slices released (`slices_released`, their
    /// files counted in `files_removed`/`bytes_freed`, a failed unlink in
    /// `stranded`), a set whose last slice went (`sets_cleaned`), and the
    /// slices still short of copies (`slices_short`). Its counts are never
    /// also in `cleaned`.
    pub slices: CleanReport,
}

/// Releases the staged data of each of `unit_ids` whose copy floor and
/// every per-location target are now met, and keeps the staged data of each
/// unit still short of either, saying which and why (ADR-0012 item 217,
/// [`Hold::CopyTargets`]).
///
/// The release itself is `clean_staging(force = false, CleanScope::Units)`
/// over the covered units, so its own guard still applies and is what keeps
/// a set that has no completed write — or any write that is neither
/// `completed` nor `cut` — staged. This function adds the policy question
/// that guard cannot answer (issue #229's second half): after one copy of a
/// unit whose floor is 2, the guard passes vacuously (one `writes` row,
/// `completed`) and would release the only cheap route to the second copy.
///
/// Issue #248: `clean_staging`'s eligibility SQL has no batch/unit predicate
/// of its own, so `CleanScope::Whole` here would release EVERY eligible
/// `'staged'` set in the database — including a different unit that is still
/// short. `CleanScope::Units` narrows the release to units this gate judged
/// covered. With nothing short the scope is every unit given, the call 1.8
/// made; with every unit judged whole short, `clean_staging` is not called.
///
/// **Sets that hold a range** (1.7.0, issue #478; ADR-0012 items 85 and
/// 129; ruling X15). `clean_staging` never releases one without `force`: its
/// slices are on different volumes. Each such `'staged'` set of `unit_ids`
/// is released slice by slice ([`release_ranges`]), against its own unit's
/// copy floor and whatever the units judged whole do, into
/// [`ReleaseOutcome::slices`]. A unit whose staged data here is only such
/// sets is not judged whole: its version is not `current` until every slice
/// is on a sealed volume (item 128), so the unit reads 0 copies for the
/// whole span, and naming it short would say nothing its slices do not.
pub fn release_if_covered(
    conn: &Connection,
    config: &Config,
    unit_ids: &[i64],
) -> Result<ReleaseOutcome> {
    release_if_covered_holding(conn, config, unit_ids, Hold::CopyTargets)
}

/// [`release_if_covered`], waiting for `hold` (#493, ADR-0012 items 154 and
/// 217). Each unit judged whole is released or kept on its own: short of
/// its floor, or (unless [`Hold::CopyFloor`]) of a per-location target of
/// its current coverage (the write just promoted the version it wrote),
/// it keeps its staging and is reported with what it lacks
/// ([`CopyProgress`]); covered, it is released whatever the others do.
/// Under [`Hold::EveryLocation`] a unit short of a target also keeps every
/// slice of its sets that hold a range.
pub fn release_if_covered_holding(
    conn: &Connection,
    config: &Config,
    unit_ids: &[i64],
    hold: Hold,
) -> Result<ReleaseOutcome> {
    let gate_ids = units_judged_whole(conn, unit_ids)?;
    let mut under_copied = Vec::new();
    let mut kept_ids: Vec<i64> = Vec::new();
    let mut held_for_locations: Vec<i64> = Vec::new();
    for &unit_id in unit_ids {
        let whole = gate_ids.contains(&unit_id);
        // A unit judged only by its slices needs no judgement here unless
        // its slices wait for its targets too.
        if !whole && hold != Hold::EveryLocation {
            continue;
        }
        let (unit, resolved) = unit_and_policy(conn, config, unit_id)?;
        let short_at = match hold {
            Hold::CopyFloor => Vec::new(),
            Hold::CopyTargets | Hold::EveryLocation => short_of_copies_at(
                conn,
                config,
                NamedScope::Unit(unit_id),
                &resolved,
                &unit.status,
                None,
            )?,
        };
        if hold == Hold::EveryLocation && !short_at.is_empty() {
            held_for_locations.push(unit_id);
        }
        if !whole {
            continue;
        }
        let copies: i64 = conn.query_row(
            &format!(
                "SELECT {}",
                copy_count_expr(&CoverageQuery::current_unit("?1"))
            ),
            params![unit_id],
            |row| row.get(0),
        )?;
        let floor = copy_floor(config, resolved.copy_floor_base(), &unit.status);
        if copies < floor || !short_at.is_empty() {
            kept_ids.push(unit_id);
            under_copied.push(CopyProgress {
                unit_name: unit.name.clone(),
                copies,
                min_copies: floor,
                floor_note: tape_only_note(config, &unit.status),
                short_at,
            });
        }
    }
    let covered: Vec<i64> = gate_ids
        .iter()
        .copied()
        .filter(|id| !kept_ids.contains(id))
        .collect();
    // With nothing short, the scope 1.8 gave: every unit judged. Otherwise
    // the covered units alone, and nothing at all when none is.
    let scope: &[i64] = if kept_ids.is_empty() {
        unit_ids
    } else {
        &covered
    };
    let (cleaned, released_units) = if scope.is_empty() && !kept_ids.is_empty() {
        (None, Vec::new())
    } else {
        let before = staged_whole_sets(conn, &covered)?;
        let report = clean_staging(conn, config, false, CleanScope::Units(scope))?;
        (Some(report), units_released(conn, &before)?)
    };
    let released: Vec<i64> = unit_ids
        .iter()
        .copied()
        .filter(|id| !held_for_locations.contains(id))
        .collect();
    let slices = release_ranges(conn, config, CleanScope::Units(&released))?;
    Ok(ReleaseOutcome {
        cleaned,
        released_units,
        under_copied,
        slices,
    })
}

/// The `'staged'` stage sets of `unit_ids` that hold no `range` row, with
/// their unit's name, in `unit_ids` order: what a whole-set release of those
/// units may clean. One statement.
fn staged_whole_sets(conn: &Connection, unit_ids: &[i64]) -> Result<Vec<(i64, i64, String)>> {
    if unit_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = (1..=unit_ids.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT ss.id, u.id, u.name FROM stage_sets ss
           JOIN snapshots sn ON sn.id = ss.snapshot_id
           JOIN units u ON u.id = sn.unit_id
          WHERE ss.status = 'staged' AND u.id IN ({placeholders})
            AND NOT EXISTS (SELECT 1 FROM writes w
                             WHERE w.stage_set_id = ss.id AND w.status = 'range')
          ORDER BY ss.id"
    ))?;
    let mut rows = stmt
        .query_map(rusqlite::params_from_iter(unit_ids.iter()), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    rows.sort_by_key(|(set, unit, _)| {
        (
            unit_ids
                .iter()
                .position(|u| u == unit)
                .unwrap_or(usize::MAX),
            *set,
        )
    });
    Ok(rows)
}

/// The names of the units, in `before`'s order and once each, one of whose
/// sets in `before` ([`staged_whole_sets`], read before the release) reads
/// `cleaned` now: the units this release freed. One statement per set.
fn units_released(conn: &Connection, before: &[(i64, i64, String)]) -> Result<Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    for (set, _, name) in before {
        if names.contains(name) {
            continue;
        }
        let status: String = conn.query_row(
            "SELECT status FROM stage_sets WHERE id = ?1",
            params![set],
            |r| r.get(0),
        )?;
        if status == "cleaned" {
            names.push(name.clone());
        }
    }
    Ok(names)
}

/// `unit_id`'s row and resolved policy, refusing an id the catalog lost.
fn unit_and_policy(
    conn: &Connection,
    config: &Config,
    unit_id: i64,
) -> Result<(crate::db::models::Unit, crate::policy::ResolvedPolicy)> {
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
    Ok((unit, resolved))
}

/// `unit_ids` less the units whose `'staged'` data is only stage sets that
/// hold a range, in `unit_ids` order: the units [`release_if_covered`]'s
/// all-or-nothing gate judges. A unit with no staged set at all stays, as in
/// 1.6.0; so does one with any staged set that holds no range.
fn units_judged_whole(conn: &Connection, unit_ids: &[i64]) -> Result<Vec<i64>> {
    let spanning: std::collections::HashSet<i64> =
        staged_sets_holding_a_range(conn, CleanScope::Units(unit_ids))?
            .into_iter()
            .map(|(_, unit_id)| unit_id)
            .collect();
    let mut judged = Vec::with_capacity(unit_ids.len());
    for &unit_id in unit_ids {
        let whole_set_staged = spanning.contains(&unit_id)
            && conn.query_row(
                "SELECT EXISTS (
                     SELECT 1 FROM stage_sets ss JOIN snapshots sn ON sn.id = ss.snapshot_id
                      WHERE sn.unit_id = ?1 AND ss.status = 'staged'
                        AND NOT EXISTS (SELECT 1 FROM writes w
                                         WHERE w.stage_set_id = ss.id AND w.status = 'range'))",
                params![unit_id],
                |row| row.get::<_, bool>(0),
            )?;
        if !spanning.contains(&unit_id) || whole_set_staged {
            judged.push(unit_id);
        }
    }
    Ok(judged)
}

/// The copy floor the slices of `unit_id`'s stage sets are released
/// against: its resolved policy's `copy_floor_base` (the larger of
/// `min_copies` and the `copies_at` sum, #493 ADR-0012 item 149) through the
/// one tape-only rule ([`copy_floor`]), the floor [`under_copied_units`]
/// judges the unit by.
fn unit_copy_floor(conn: &Connection, config: &Config, unit_id: i64) -> Result<i64> {
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
    Ok(copy_floor(config, resolved.copy_floor_base(), &unit.status))
}

/// The per-slice release (1.7.0, issue #478; ADR-0012 items 85 and 129) of
/// every `'staged'` stage set within `scope` that holds a range, each
/// against its own unit's copy floor ([`unit_copy_floor`]): the policy half
/// in front of `clean::release_covered_slices`, which stays policy-free, as
/// [`release_if_covered`] is in front of `clean_staging`. Shared by
/// `release_if_covered` (so `volume write`, `volume resume` and `collection
/// run`) and `stage clean`.
pub(crate) fn release_ranges(
    conn: &Connection,
    config: &Config,
    scope: CleanScope,
) -> Result<CleanReport> {
    let mut report = CleanReport::default();
    for (stage_set_id, unit_id) in staged_sets_holding_a_range(conn, scope)? {
        let floor = unit_copy_floor(conn, config, unit_id)?;
        report.absorb(release_covered_slices(conn, config, stage_set_id, floor)?);
    }
    Ok(report)
}

/// [`release_blocker`] with the policy it leaves out: for a stage set that
/// holds a range and that its writes no longer hold, the staged slices still
/// short of its unit's copy floor, as [`ReleaseBlocker::SlicesShort`]
/// (ADR-0012 item 129: "the release gate's refusal names the slices still
/// short of copies"). Every other answer is `release_blocker`'s own.
pub(crate) fn release_blocker_judged(
    conn: &Connection,
    config: &Config,
    stage_set_id: i64,
) -> Result<ReleaseBlocker> {
    let blocker = release_blocker(conn, stage_set_id)?;
    if blocker != ReleaseBlocker::None {
        return Ok(blocker);
    }
    let unit_id: Option<i64> = conn
        .query_row(
            "SELECT sn.unit_id FROM stage_sets ss JOIN snapshots sn ON sn.id = ss.snapshot_id
              WHERE ss.id = ?1
                AND EXISTS (SELECT 1 FROM writes w
                             WHERE w.stage_set_id = ss.id AND w.status = 'range')",
            params![stage_set_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(unit_id) = unit_id else {
        return Ok(blocker);
    };
    let floor = unit_copy_floor(conn, config, unit_id)?;
    let slices = slices_short_of(conn, stage_set_id, floor.max(1))?;
    Ok(if slices.is_empty() {
        ReleaseBlocker::None
    } else {
        ReleaseBlocker::SlicesShort { slices }
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
        let min_copies = copy_floor(config, resolved.copy_floor_base(), &unit.status);
        if copies < min_copies {
            under.push(CopyProgress {
                unit_name: name,
                copies,
                min_copies,
                floor_note: tape_only_note(config, &unit.status),
                short_at: Vec::new(),
            });
        }
    }
    Ok(under)
}

/// The ids of the units that have a `completed` or `range` write on the
/// volume labelled `volume_label`, in id order — the units a `volume write`
/// or `volume resume` of that volume has just put on tape, and so the units
/// [`release_if_covered`] must judge together. A `range` write (1.7.0, issue
/// #478; ADR-0012 item 121) put some of its set's slices on the tape, and
/// those slices may now have their copies (item 129). A `cut` one put none.
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
         WHERE v.label = ?1 AND w.status IN ('completed', 'range')
         ORDER BY sn.unit_id",
    )?;
    let ids = stmt
        .query_map(params![volume_label], |row| row.get(0))?
        .collect::<std::result::Result<Vec<i64>, _>>()?;
    Ok(ids)
}

/// Catalog fixtures for this module's tests and for the `volume write` /
/// `volume resume` tests in `cli::volume`, which exercise the same rule.
#[cfg(test)]
pub(crate) mod fixtures {
    use std::path::{Path, PathBuf};

    use rusqlite::{params, Connection};

    use crate::config::Config;

    /// One staged version of a unit, as the catalog and the disk hold it.
    pub(crate) struct Staged {
        pub stage_set_id: i64,
        pub snapshot_id: i64,
        pub file: PathBuf,
        /// Labels of the `sealed` volumes carrying a completed write of it.
        pub volumes: Vec<String>,
    }

    pub(crate) fn tenant(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// A config whose staging directory is `dir` and whose default
    /// `min_copies` is `min_copies`.
    pub(crate) fn config(dir: &Path, min_copies: i32) -> Config {
        let mut config = Config {
            staging: crate::config::StagingConfig {
                directory: dir.to_string_lossy().to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        config.defaults.min_copies = min_copies;
        config
    }

    pub(crate) fn unit(conn: &Connection, tenant_id: i64, name: &str) -> i64 {
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status) \
             VALUES (?1, ?1, ?2, 'mtime_size', 1, 'active')",
            params![name, tenant_id],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// Version `version` of `unit_id`: a snapshot with status
    /// `snapshot_status`, one `'staged'` stage set with a real slice file
    /// under `dir`, and `copies` completed writes, each on its own `sealed`
    /// volume.
    pub(crate) fn staged_version(
        conn: &Connection,
        unit_id: i64,
        version: i64,
        snapshot_status: &str,
        dir: &Path,
        copies: i64,
    ) -> Staged {
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size) \
             VALUES (?1, ?2, ?3, '/tmp', 1, 10)",
            params![unit_id, version, snapshot_status],
        )
        .unwrap();
        let snapshot_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
            params![snapshot_id],
        )
        .unwrap();
        let stage_set_id = conn.last_insert_rowid();
        let file = dir.join(format!("u{unit_id}_v{version}_slice_1.age"));
        std::fs::write(&file, b"staged slice bytes").unwrap();
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                        sha256_plain, sha256_encrypted, staging_path)
             VALUES (?1, 1, 19, 19, 'deadbeef', 'deadbeef', ?2)",
            params![stage_set_id, file.to_string_lossy()],
        )
        .unwrap();
        let mut staged = Staged {
            stage_set_id,
            snapshot_id,
            file,
            volumes: Vec::new(),
        };
        for i in 0..copies {
            let label = format!("V-u{unit_id}-v{version}-{i}");
            add_write(conn, &staged, &label, "sealed", "completed");
            staged.volumes.push(label);
        }
        staged
    }

    /// A `writes` row of `staged`'s stage set with status `write_status`, on
    /// the volume `label` (created with `volume_status` if it is new).
    pub(crate) fn add_write(
        conn: &Connection,
        staged: &Staged,
        label: &str,
        volume_status: &str,
        write_status: &str,
    ) {
        conn.execute(
            "INSERT OR IGNORE INTO volumes (label, backend_type, backend_name, capacity_bytes, status) \
             VALUES (?1, 'lto', 'p', 10485760, ?2)",
            params![label, volume_status],
        )
        .unwrap();
        let volume_id: i64 = conn
            .query_row(
                "SELECT id FROM volumes WHERE label = ?1",
                params![label],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                staged.stage_set_id,
                staged.snapshot_id,
                volume_id,
                write_status
            ],
        )
        .unwrap();
    }

    /// `staged`'s stage set cut from the volume `label`, which sealed short
    /// with `ended_after` (1.6.0, ADR-0012 item 112): a `cut` row on a
    /// `sealed` volume whose `ended_after` is set, the only shape the spec
    /// allows a `cut` row in.
    pub(crate) fn cut_from(conn: &Connection, staged: &Staged, label: &str, ended_after: i64) {
        add_write(conn, staged, label, "sealed", "cut");
        conn.execute(
            "UPDATE volumes SET ended_after = ?2, sealed_at = datetime('now') WHERE label = ?1",
            params![label, ended_after],
        )
        .unwrap();
    }

    /// [`staged_version`] with `n` slices, each a real file, and no write:
    /// a spannable set for the range tests (1.7.0, issue #478). Returns the
    /// version and its slice files by slice number from 1.
    pub(crate) fn staged_span(
        conn: &Connection,
        unit_id: i64,
        version: i64,
        snapshot_status: &str,
        dir: &Path,
        n: i64,
    ) -> (Staged, Vec<PathBuf>) {
        let staged = staged_version(conn, unit_id, version, snapshot_status, dir, 0);
        let mut files = vec![staged.file.clone()];
        for k in 2..=n {
            let file = dir.join(format!("u{unit_id}_v{version}_slice_{k}.age"));
            std::fs::write(&file, b"staged slice bytes").unwrap();
            conn.execute(
                "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                            sha256_plain, sha256_encrypted, staging_path)
                 VALUES (?1, ?2, 19, 19, 'p', ?3, ?4)",
                params![
                    staged.stage_set_id,
                    k,
                    format!("e{}-{k}", staged.stage_set_id),
                    file.to_string_lossy()
                ],
            )
            .unwrap();
            files.push(file);
        }
        (staged, files)
    }

    /// A `range` write of `staged`'s set (planned spannable) on the volume
    /// `label`, created `sealed` if it is new, holding the slices `written`
    /// and every later slice planned but `pending` (the tape ended first):
    /// the row confirm leaves on a tape the early warning ended inside the
    /// set (ADR-0012 item 121).
    pub(crate) fn range_on(
        conn: &Connection,
        staged: &Staged,
        label: &str,
        written: std::ops::RangeInclusive<i64>,
    ) {
        conn.execute(
            "INSERT OR IGNORE INTO volumes (label, backend_type, backend_name, capacity_bytes, status) \
             VALUES (?1, 'lto', 'p', 10485760, 'sealed')",
            params![label],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, may_span)
             SELECT ?1, ?2, id, 'range', 1 FROM volumes WHERE label = ?3",
            params![staged.stage_set_id, staged.snapshot_id, label],
        )
        .unwrap();
        let write_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO write_positions (write_id, stage_slice_id, position, status)
             SELECT ?1, id, CAST(5 + slice_number AS TEXT),
                    CASE WHEN slice_number <= ?4 THEN 'written' ELSE 'pending' END
               FROM stage_slices WHERE stage_set_id = ?2 AND slice_number >= ?3",
            params![
                write_id,
                staged.stage_set_id,
                written.start(),
                written.end()
            ],
        )
        .unwrap();
    }

    pub(crate) fn stage_set_status(conn: &Connection, stage_set_id: i64) -> String {
        conn.query_row(
            "SELECT status FROM stage_sets WHERE id = ?1",
            params![stage_set_id],
            |r| r.get(0),
        )
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use crate::db;

    /// Released at the floor: a unit with exactly its `min_copies` loses its
    /// staged bytes and its stage set reads `cleaned`.
    #[test]
    fn staging_is_released_once_the_unit_meets_its_min_copies() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 2);
        let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
        let s = staged_version(&conn, alpha, 1, "current", dir.path(), 2);

        let out = release_if_covered(&conn, &config, &[alpha]).unwrap();

        assert!(out.under_copied.is_empty(), "{:?}", out.under_copied);
        let cleaned = out.cleaned.expect("released at the floor");
        assert_eq!(
            (cleaned.sets_cleaned, cleaned.files_removed, cleaned.errors),
            (1, 1, 0)
        );
        assert!(cleaned.bytes_freed > 0, "the bytes freed are reported");
        assert_eq!(stage_set_status(&conn, s.stage_set_id), "cleaned");
        assert!(!s.file.exists());
    }

    /// Kept below the floor, with the reason the operator reads.
    #[test]
    fn staging_is_kept_below_min_copies_and_the_unit_is_named() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 2);
        let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
        let s = staged_version(&conn, alpha, 1, "current", dir.path(), 1);

        let out = release_if_covered(&conn, &config, &[alpha]).unwrap();

        assert!(out.cleaned.is_none(), "one copy of two must not release");
        assert_eq!(out.under_copied.len(), 1, "{:?}", out.under_copied);
        assert_eq!(
            out.under_copied[0].describe(),
            "unit testlib/alpha has 1 copy, needs 2"
        );
        assert_eq!(stage_set_status(&conn, s.stage_set_id), "staged");
        assert!(s.file.exists(), "the next copy reuses these bytes");
    }

    /// Two units on one write, one short: the covered one is released and
    /// the short one is kept and named (ADR-0012 item 217, amending #440's
    /// all-or-nothing). Undone, alpha's staging would wait for beta.
    #[test]
    fn a_short_unit_keeps_only_its_own_staging() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 2);
        let t = tenant(&conn);
        let alpha = unit(&conn, t, "testlib/alpha");
        let beta = unit(&conn, t, "testlib/beta");
        let a = staged_version(&conn, alpha, 1, "current", dir.path(), 2);
        let b = staged_version(&conn, beta, 1, "current", dir.path(), 1);

        let out = release_if_covered(&conn, &config, &[alpha, beta]).unwrap();

        let named: Vec<_> = out.under_copied.iter().map(|p| p.describe()).collect();
        assert_eq!(named, ["unit testlib/beta has 1 copy, needs 2"]);
        assert_eq!(out.released_units, ["testlib/alpha"]);
        assert_eq!(out.cleaned.expect("alpha is covered").sets_cleaned, 1);
        assert!(
            !a.file.exists(),
            "the covered unit does not wait for the short one"
        );
        assert_eq!(stage_set_status(&conn, a.stage_set_id), "cleaned");
        assert!(b.file.exists(), "the next copy reuses beta's staged bytes");
        assert_eq!(stage_set_status(&conn, b.stage_set_id), "staged");
    }

    /// A unit is as covered as its least-covered live version (ADR-0012):
    /// version 1 at two copies does not release anything while version 2,
    /// just written, has one.
    #[test]
    fn a_unit_is_as_covered_as_its_least_covered_version() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 2);
        let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
        let v1 = staged_version(&conn, alpha, 1, "current", dir.path(), 2);
        let v2 = staged_version(&conn, alpha, 2, "current", dir.path(), 1);

        let out = release_if_covered(&conn, &config, &[alpha]).unwrap();

        assert!(out.cleaned.is_none());
        assert_eq!(out.under_copied[0].copies, 1);
        assert!(v1.file.exists() && v2.file.exists());
    }

    /// The release cannot reach a set no write completed: a covered unit's
    /// other version, staged and never written, stays staged on disk.
    #[test]
    fn a_never_written_set_is_never_released() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 2);
        let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
        let written = staged_version(&conn, alpha, 1, "current", dir.path(), 2);
        let unwritten = staged_version(&conn, alpha, 2, "staged", dir.path(), 0);

        let out = release_if_covered(&conn, &config, &[alpha]).unwrap();

        assert!(out.under_copied.is_empty(), "v1 alone is current");
        assert_eq!(out.cleaned.expect("released").sets_cleaned, 1);
        assert_eq!(stage_set_status(&conn, written.stage_set_id), "cleaned");
        assert_eq!(stage_set_status(&conn, unwritten.stage_set_id), "staged");
        assert!(unwritten.file.exists());
    }

    /// Nor a set one of whose writes is not complete — a further copy still
    /// planned or interrupted reads these bytes (`v2-open-questions.md`
    /// §3.5) — even when the unit's copy count says it is covered.
    #[test]
    fn a_set_with_an_unfinished_write_elsewhere_is_never_released() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 2);
        let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
        let s = staged_version(&conn, alpha, 1, "current", dir.path(), 2);
        add_write(&conn, &s, "V-third", "initialized", "interrupted");

        let out = release_if_covered(&conn, &config, &[alpha]).unwrap();

        assert!(out.under_copied.is_empty(), "two sealed copies: covered");
        assert_eq!(out.cleaned.expect("the gate passed").sets_cleaned, 0);
        assert_eq!(stage_set_status(&conn, s.stage_set_id), "staged");
        assert!(s.file.exists());
    }

    /// The floor is the one tape-only rule's: one copy meets `min_copies =
    /// 1` for an active unit; a tape-only unit at 3x needs 3, and the
    /// message says why the number is 3.
    #[test]
    fn the_floor_takes_the_tape_only_multiplier_and_says_so() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(dir.path(), 1);
        config.compaction.tape_only_safety_multiplier = 3;
        let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
        staged_version(&conn, alpha, 1, "current", dir.path(), 1);

        assert!(under_copied_units(&conn, &config, &[alpha])
            .unwrap()
            .is_empty());
        conn.execute("UPDATE units SET status = 'tape_only'", [])
            .unwrap();
        let under = under_copied_units(&conn, &config, &[alpha]).unwrap();
        assert_eq!(under.len(), 1);
        assert_eq!(
            under[0].describe(),
            "unit testlib/alpha has 1 copy, needs 3 (tape-only 3x)"
        );
    }

    /// `home` and `offsite`, and `unit`'s dotfile layer `{"home":1,
    /// "offsite":1}` (frozen: the fixture's units have no source).
    fn two_targets(conn: &Connection, unit_id: i64) {
        conn.execute_batch(
            "INSERT INTO locations (name, kind) VALUES ('home', 'shelf');
             INSERT INTO locations (name, kind) VALUES ('offsite', 'shelf');",
        )
        .unwrap();
        conn.execute(
            "UPDATE units SET copies_at_frozen = '{\"home\":1,\"offsite\":1}' WHERE id = ?1",
            params![unit_id],
        )
        .unwrap();
    }

    fn shelve(conn: &Connection, label: &str, location: &str) {
        conn.execute(
            "UPDATE volumes SET location_id = (SELECT id FROM locations WHERE name = ?2)
              WHERE label = ?1",
            params![label, location],
        )
        .unwrap();
    }

    /// #493 (ADR-0012 item 154): `--keep-staging-for-locations` holds a
    /// written set while a per-location target of its unit is unmet, though
    /// its copy floor is met, and says which; once every target is met the
    /// set is released.
    #[test]
    fn keep_staging_for_locations_holds_a_set_until_every_target_is_met() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 2);
        let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
        two_targets(&conn, alpha);
        let s = staged_version(&conn, alpha, 1, "current", dir.path(), 2);
        for label in &s.volumes {
            shelve(&conn, label, "home");
        }

        let out =
            release_if_covered_holding(&conn, &config, &[alpha], Hold::EveryLocation).unwrap();
        assert!(out.cleaned.is_none(), "held for offsite");
        assert_eq!(out.under_copied.len(), 1);
        assert_eq!(
            out.under_copied[0].describe(),
            "unit testlib/alpha has 2 copies, its floor 2; still needed: 0 copies at required \
             location offsite, needs 1"
        );
        assert_eq!(stage_set_status(&conn, s.stage_set_id), "staged");
        assert!(s.file.exists());

        add_write(&conn, &s, "V-offsite", "sealed", "completed");
        shelve(&conn, "V-offsite", "offsite");
        let out =
            release_if_covered_holding(&conn, &config, &[alpha], Hold::EveryLocation).unwrap();
        assert!(out.under_copied.is_empty(), "{:?}", out.under_copied);
        assert_eq!(out.cleaned.expect("every target met").sets_cleaned, 1);
        assert_eq!(stage_set_status(&conn, s.stage_set_id), "cleaned");
    }

    /// ADR-0012 item 217: the default release waits for every `copies_at`
    /// target as well as the floor, so a unit at its floor whose targets are
    /// unmet keeps its staging and says where it is short, by
    /// `release_if_covered` and its `Hold::CopyTargets` form alike. The
    /// control is `Hold::CopyFloor`, 1.7.1's default (item 154), which
    /// releases the same unit at its floor.
    #[test]
    fn the_default_release_waits_for_every_copies_at_target() {
        for hold in [None, Some(Hold::CopyTargets), Some(Hold::CopyFloor)] {
            let conn = db::open_memory().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let config = config(dir.path(), 2);
            let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
            two_targets(&conn, alpha);
            let s = staged_version(&conn, alpha, 1, "current", dir.path(), 2);
            for label in &s.volumes {
                shelve(&conn, label, "home");
            }
            let out = match hold {
                None => release_if_covered(&conn, &config, &[alpha]).unwrap(),
                Some(h) => release_if_covered_holding(&conn, &config, &[alpha], h).unwrap(),
            };
            if hold == Some(Hold::CopyFloor) {
                assert!(out.under_copied.is_empty(), "{:?}", out.under_copied);
                assert_eq!(out.cleaned.expect("at the floor").sets_cleaned, 1);
                assert_eq!(stage_set_status(&conn, s.stage_set_id), "cleaned");
                continue;
            }
            assert!(out.cleaned.is_none(), "{hold:?}: held for offsite");
            assert!(out.released_units.is_empty());
            assert_eq!(
                out.under_copied
                    .iter()
                    .map(|p| p.describe())
                    .collect::<Vec<_>>(),
                [
                    "unit testlib/alpha has 2 copies, its floor 2; still needed: 0 copies at \
                  required location offsite, needs 1"
                ],
                "{hold:?}"
            );
            assert!(s.file.exists(), "{hold:?}");

            add_write(&conn, &s, "V-offsite", "sealed", "completed");
            shelve(&conn, "V-offsite", "offsite");
            let out = release_if_covered(&conn, &config, &[alpha]).unwrap();
            assert!(out.under_copied.is_empty(), "{:?}", out.under_copied);
            assert_eq!(out.released_units, ["testlib/alpha"]);
            assert_eq!(stage_set_status(&conn, s.stage_set_id), "cleaned");
        }
    }

    #[test]
    fn the_reason_is_plural_for_zero_copies() {
        let p = CopyProgress {
            unit_name: "u".into(),
            copies: 0,
            min_copies: 2,
            floor_note: String::new(),
            short_at: Vec::new(),
        };
        assert_eq!(p.describe(), "unit u has 0 copies, needs 2");
    }

    /// The units a volume was written for are those with a COMPLETED write
    /// on it: not another volume's, not an interrupted one's.
    #[test]
    fn unit_ids_written_to_lists_only_completed_writes_on_that_volume() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let t = tenant(&conn);
        let alpha = unit(&conn, t, "testlib/alpha");
        let beta = unit(&conn, t, "testlib/beta");
        let gamma = unit(&conn, t, "testlib/gamma");
        let a = staged_version(&conn, alpha, 1, "current", dir.path(), 0);
        let b = staged_version(&conn, beta, 1, "current", dir.path(), 0);
        let g = staged_version(&conn, gamma, 1, "current", dir.path(), 0);
        add_write(&conn, &a, "V-ONE", "sealed", "completed");
        add_write(&conn, &b, "V-ONE", "sealed", "completed");
        add_write(&conn, &g, "V-ONE", "sealed", "interrupted");
        add_write(&conn, &g, "V-TWO", "sealed", "completed");

        assert_eq!(unit_ids_written_to(&conn, "V-ONE").unwrap(), [alpha, beta]);
        assert_eq!(unit_ids_written_to(&conn, "V-TWO").unwrap(), [gamma]);
        assert!(unit_ids_written_to(&conn, "V-NONE").unwrap().is_empty());
    }

    // --- `cut` (1.6.0, issue #420; ADR-0012 item 112) ---

    /// A `cut` write is never a copy, even of a version that reads
    /// `current`, so it never makes a unit covered and never releases its
    /// staging. The positive control is the same row `completed`: one copy,
    /// at the floor, released.
    #[test]
    fn a_cut_write_is_never_a_copy_and_keeps_the_staging() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 1);
        let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
        let s = staged_version(&conn, alpha, 1, "current", dir.path(), 0);
        cut_from(&conn, &s, "V-SHORT", 9);

        let out = release_if_covered(&conn, &config, &[alpha]).unwrap();

        assert!(
            out.cleaned.is_none(),
            "a cut is not a copy: nothing released"
        );
        assert_eq!(
            out.under_copied
                .iter()
                .map(|p| p.describe())
                .collect::<Vec<_>>(),
            ["unit testlib/alpha has 0 copies, needs 1"]
        );
        assert_eq!(stage_set_status(&conn, s.stage_set_id), "staged");
        assert!(s.file.exists(), "the next volume write reads these slices");

        conn.execute("UPDATE writes SET status = 'completed'", [])
            .unwrap();
        let out = release_if_covered(&conn, &config, &[alpha]).unwrap();
        assert!(out.under_copied.is_empty(), "{:?}", out.under_copied);
        assert_eq!(out.cleaned.expect("released").sets_cleaned, 1);
        assert!(!s.file.exists());
    }

    /// The cut unit is written whole on another cartridge, and from then on
    /// it is released by the same rule as any other: the `cut` row does not
    /// hold the staging the way an unfinished write would.
    #[test]
    fn a_cut_set_is_released_once_its_version_is_completed_elsewhere() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 1);
        let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
        let s = staged_version(&conn, alpha, 1, "current", dir.path(), 0);
        cut_from(&conn, &s, "V-SHORT", 9);
        add_write(&conn, &s, "V-NEXT", "sealed", "completed");

        assert_eq!(unit_ids_written_to(&conn, "V-NEXT").unwrap(), [alpha]);
        let out = release_if_covered(&conn, &config, &[alpha]).unwrap();

        assert!(out.under_copied.is_empty(), "{:?}", out.under_copied);
        let cleaned = out.cleaned.expect("released");
        assert_eq!((cleaned.sets_cleaned, cleaned.files_removed), (1, 1));
        assert_eq!(stage_set_status(&conn, s.stage_set_id), "cleaned");
        assert!(!s.file.exists());
    }

    /// The shape a short seal leaves when a unit's newer version is the one
    /// cut: version 1 is written and current, version 2 was cut and is
    /// still `staged`. The unit is covered by version 1, so the gate passes
    /// and version 1's staging goes; version 2's stays for the next write.
    #[test]
    fn a_cut_newer_version_stays_staged_while_the_written_one_is_released() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 1);
        let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
        let v1 = staged_version(&conn, alpha, 1, "current", dir.path(), 1);
        let v2 = staged_version(&conn, alpha, 2, "staged", dir.path(), 0);
        cut_from(&conn, &v2, "V-SHORT", 9);

        let out = release_if_covered(&conn, &config, &[alpha]).unwrap();

        assert!(out.under_copied.is_empty(), "v1 alone is current");
        assert_eq!(out.cleaned.expect("released").sets_cleaned, 1);
        assert_eq!(stage_set_status(&conn, v1.stage_set_id), "cleaned");
        assert_eq!(stage_set_status(&conn, v2.stage_set_id), "staged");
        assert!(v2.file.exists());
    }

    /// A batch whose write sealed short holds a unit with no copy at all --
    /// its only version was cut, so it has no `current` snapshot and reads
    /// 0, never NULL. Judged together, per unit (ADR-0012 item 217): the
    /// written unit is released and the cut one keeps its staging, named.
    /// `volume write` judges only the units it completed on the volume
    /// (`unit_ids_written_to`), which leaves the cut one out.
    #[test]
    fn a_cut_unit_judged_with_a_written_one_keeps_only_its_own_staging() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 1);
        let t = tenant(&conn);
        let alpha = unit(&conn, t, "testlib/alpha");
        let beta = unit(&conn, t, "testlib/beta");
        let a = staged_version(&conn, alpha, 1, "staged", dir.path(), 0);
        let b = staged_version(&conn, beta, 1, "staged", dir.path(), 0);
        add_write(&conn, &a, "V-SHORT", "sealed", "completed");
        conn.execute(
            "UPDATE snapshots SET status = 'current' WHERE id = ?1",
            params![a.snapshot_id],
        )
        .unwrap();
        cut_from(&conn, &b, "V-SHORT", 9);

        let out = release_if_covered(&conn, &config, &[alpha, beta]).unwrap();

        assert_eq!(
            out.under_copied
                .iter()
                .map(|p| p.describe())
                .collect::<Vec<_>>(),
            ["unit testlib/beta has 0 copies, needs 1"]
        );
        assert_eq!(out.released_units, ["testlib/alpha"]);
        assert!(!a.file.exists());
        assert!(b.file.exists(), "the cut unit's staging stays");
        assert_eq!(stage_set_status(&conn, b.stage_set_id), "staged");

        assert_eq!(
            unit_ids_written_to(&conn, "V-SHORT").unwrap(),
            [alpha],
            "the cut unit is not one the short-sealed volume was written for"
        );
    }

    // --- `range` (1.7.0, issue #478; ADR-0012 items 121, 127-129) ---

    fn files_exist(files: &[std::path::PathBuf]) -> Vec<bool> {
        files.iter().map(|f| f.exists()).collect()
    }

    /// Copy target 1 on the two-volume range fixture: tape 1 holds slices
    /// 1..2 as a `range` row, tape 2 slices 3..4. After tape 1 its slices are
    /// released though the version is not yet current (item 128 protects
    /// them), and the rest are named short; after tape 2 the rest go and the
    /// set is `cleaned`. The release goes through `release_if_covered`, the
    /// one `volume write` makes, over `unit_ids_written_to` of each tape.
    #[test]
    fn release_at_copy_target_one_frees_each_tapes_range() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 1);
        let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
        let (s, files) = staged_span(&conn, alpha, 1, "staged", dir.path(), 4);

        range_on(&conn, &s, "V-T1", 1..=2);
        let ids = unit_ids_written_to(&conn, "V-T1").unwrap();
        assert_eq!(ids, [alpha], "a range write is a write to the tape");
        let out = release_if_covered(&conn, &config, &ids).unwrap();
        assert!(out.under_copied.is_empty(), "{:?}", out.under_copied);
        assert_eq!(
            out.slices.slices_released,
            vec![(s.stage_set_id, vec![1, 2])]
        );
        assert_eq!(out.slices.slices_short, vec![(s.stage_set_id, vec![3, 4])]);
        assert_eq!(out.slices.files_removed, 2);
        assert_eq!(
            out.cleaned.expect("nothing whole to judge").sets_cleaned,
            0,
            "the per-slice counts are never also in `cleaned`"
        );
        assert_eq!(files_exist(&files), [false, false, true, true]);
        assert_eq!(stage_set_status(&conn, s.stage_set_id), "staged");

        range_on(&conn, &s, "V-T2", 3..=4);
        conn.execute(
            "UPDATE snapshots SET status = 'current' WHERE id = ?1",
            params![s.snapshot_id],
        )
        .unwrap();
        let out = release_if_covered(&conn, &config, &unit_ids_written_to(&conn, "V-T2").unwrap())
            .unwrap();
        assert_eq!(
            out.slices.slices_released,
            vec![(s.stage_set_id, vec![3, 4])]
        );
        assert!(out.slices.slices_short.is_empty());
        assert_eq!(out.slices.sets_cleaned, 1);
        assert_eq!(files_exist(&files), [false; 4]);
        assert_eq!(stage_set_status(&conn, s.stage_set_id), "cleaned");
    }

    /// Copy target 2 on item 93's layout: tape 1 `range` 1..2, tape 2 the
    /// whole set `completed`, tape 3 `range` 3..4. Nothing goes after tape
    /// 1; after tape 2 slices 1..2 have two copies and go, 3..4 one; after
    /// tape 3 the rest go.
    #[test]
    fn release_at_copy_target_two_follows_each_slices_copies() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 2);
        let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
        let (s, files) = staged_span(&conn, alpha, 1, "staged", dir.path(), 4);

        range_on(&conn, &s, "V-T1", 1..=2);
        let out = release_if_covered(&conn, &config, &[alpha]).unwrap();
        assert!(out.slices.slices_released.is_empty());
        assert_eq!(
            out.slices.slices_short,
            vec![(s.stage_set_id, vec![1, 2, 3, 4])]
        );
        assert_eq!(files_exist(&files), [true; 4]);

        add_write(&conn, &s, "V-T2", "sealed", "completed");
        conn.execute(
            "UPDATE snapshots SET status = 'current' WHERE id = ?1",
            params![s.snapshot_id],
        )
        .unwrap();
        let out = release_if_covered(&conn, &config, &[alpha]).unwrap();
        assert_eq!(
            out.slices.slices_released,
            vec![(s.stage_set_id, vec![1, 2])]
        );
        assert_eq!(out.slices.slices_short, vec![(s.stage_set_id, vec![3, 4])]);
        assert_eq!(files_exist(&files), [false, false, true, true]);
        assert_eq!(stage_set_status(&conn, s.stage_set_id), "staged");

        range_on(&conn, &s, "V-T3", 3..=4);
        let out = release_if_covered(&conn, &config, &[alpha]).unwrap();
        assert_eq!(
            out.slices.slices_released,
            vec![(s.stage_set_id, vec![3, 4])]
        );
        assert_eq!(out.slices.sets_cleaned, 1);
        assert_eq!(stage_set_status(&conn, s.stage_set_id), "cleaned");
        assert_eq!(files_exist(&files), [false; 4]);
    }

    /// Ruling X15: a spanning set's slices are released by their own copy
    /// counts whatever the other units of the same write do. Beta, written
    /// whole beside the span, is short (tape-only at 3x), so beta keeps its
    /// staging; alpha's covered slices go anyway.
    #[test]
    fn a_spanning_sets_slices_are_released_whatever_the_other_units_do() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(dir.path(), 1);
        config.compaction.tape_only_safety_multiplier = 3;
        let t = tenant(&conn);
        let alpha = unit(&conn, t, "testlib/alpha");
        let beta = unit(&conn, t, "testlib/beta");
        conn.execute(
            "UPDATE units SET status = 'tape_only' WHERE id = ?1",
            params![beta],
        )
        .unwrap();
        let (a, files) = staged_span(&conn, alpha, 1, "staged", dir.path(), 3);
        let b = staged_version(&conn, beta, 1, "current", dir.path(), 0);
        add_write(&conn, &b, "V-T1", "sealed", "completed");
        range_on(&conn, &a, "V-T1", 1..=2);

        let ids = unit_ids_written_to(&conn, "V-T1").unwrap();
        assert_eq!(ids, [alpha, beta]);
        let out = release_if_covered(&conn, &config, &ids).unwrap();
        assert!(
            out.cleaned.is_none(),
            "beta, the one unit judged whole, is short"
        );
        assert_eq!(
            out.under_copied
                .iter()
                .map(|p| p.describe())
                .collect::<Vec<_>>(),
            ["unit testlib/beta has 1 copy, needs 3 (tape-only 3x)"],
            "only the unit judged whole is named"
        );
        assert!(b.file.exists());
        assert_eq!(
            out.slices.slices_released,
            vec![(a.stage_set_id, vec![1, 2])]
        );
        assert_eq!(files_exist(&files), [false, false, true]);
    }

    /// The other half of X15: a unit whose staged data is only a span in
    /// progress is not judged whole. Its version is not current until every
    /// slice is on a sealed volume (item 128), so it reads 0 copies, and
    /// judging it would keep a covered whole unit's staging for the whole
    /// span. Beta, at its floor, is released beside it.
    #[test]
    fn a_span_in_progress_does_not_keep_a_covered_whole_units_staging() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 1);
        let t = tenant(&conn);
        let alpha = unit(&conn, t, "testlib/alpha");
        let beta = unit(&conn, t, "testlib/beta");
        let (a, _files) = staged_span(&conn, alpha, 1, "staged", dir.path(), 3);
        let b = staged_version(&conn, beta, 1, "current", dir.path(), 0);
        add_write(&conn, &b, "V-T1", "sealed", "completed");
        range_on(&conn, &a, "V-T1", 1..=2);
        assert_eq!(
            under_copied_units(&conn, &config, &[alpha]).unwrap()[0].copies,
            0,
            "positive control: judged whole, the span would read 0 copies"
        );

        let out = release_if_covered(&conn, &config, &[alpha, beta]).unwrap();
        assert!(out.under_copied.is_empty(), "{:?}", out.under_copied);
        assert_eq!(out.cleaned.expect("beta is covered").sets_cleaned, 1);
        assert!(!b.file.exists());
        assert_eq!(stage_set_status(&conn, b.stage_set_id), "cleaned");
        assert_eq!(stage_set_status(&conn, a.stage_set_id), "staged");
    }

    /// `ReleaseBlocker::SlicesShort` names exactly the staged slices short
    /// of the unit's floor, never a covered or released one; with nothing
    /// short, or for a set with no range row, the answer is
    /// `release_blocker`'s own.
    #[test]
    fn release_blocker_judged_names_exactly_the_short_slices() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config2 = config(dir.path(), 2);
        let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
        let (s, _files) = staged_span(&conn, alpha, 1, "staged", dir.path(), 5);
        range_on(&conn, &s, "V-T1", 1..=2);
        add_write(&conn, &s, "V-T2", "sealed", "completed");
        assert_eq!(
            release_blocker(&conn, s.stage_set_id).unwrap(),
            ReleaseBlocker::None,
            "the writes alone block nothing"
        );
        assert_eq!(
            release_blocker_judged(&conn, &config2, s.stage_set_id).unwrap(),
            ReleaseBlocker::SlicesShort {
                slices: vec![3, 4, 5]
            }
        );

        let config1 = config(dir.path(), 1);
        release_if_covered(&conn, &config1, &[alpha]).unwrap();
        assert_eq!(
            release_blocker_judged(&conn, &config1, s.stage_set_id).unwrap(),
            ReleaseBlocker::None,
            "every slice met floor 1 and was released"
        );

        let (v2, _f) = staged_span(&conn, alpha, 2, "staged", dir.path(), 3);
        range_on(&conn, &v2, "V-T3", 1..=1);
        add_write(&conn, &v2, "V-T4", "initialized", "interrupted");
        assert_eq!(
            release_blocker_judged(&conn, &config1, v2.stage_set_id).unwrap(),
            ReleaseBlocker::Interrupted {
                volume_label: Some("V-T4".into())
            },
            "a resumable session outranks the short slices"
        );

        let whole = staged_version(&conn, alpha, 3, "current", dir.path(), 1);
        assert_eq!(
            release_blocker_judged(&conn, &config2, whole.stage_set_id).unwrap(),
            ReleaseBlocker::None,
            "a whole set short of copies is the min_copies split's, not SlicesShort"
        );
    }

    /// A set with no `range` row releases exactly as in 1.6.0, whether or
    /// not its write was planned spannable: whole, every path cleared, no
    /// `released_at` set, nothing in `slices`.
    #[test]
    fn a_set_with_no_range_row_releases_exactly_as_in_1_6_0() {
        for may_span in [0, 1] {
            let conn = db::open_memory().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let config = config(dir.path(), 1);
            let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
            let (s, files) = staged_span(&conn, alpha, 1, "current", dir.path(), 3);
            add_write(&conn, &s, "V-ONE", "sealed", "completed");
            conn.execute("UPDATE writes SET may_span = ?1", params![may_span])
                .unwrap();

            let out = release_if_covered(&conn, &config, &[alpha]).unwrap();
            let cleaned = out.cleaned.expect("released");
            assert_eq!(
                (cleaned.sets_cleaned, cleaned.files_removed),
                (1, 3),
                "may_span={may_span}"
            );
            assert!(out.slices.slices_released.is_empty() && out.slices.files_removed == 0);
            assert_eq!(files_exist(&files), [false; 3]);
            let (paths, released): (i64, i64) = conn
                .query_row(
                    "SELECT COUNT(staging_path), COUNT(released_at) FROM stage_slices",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!((paths, released), (0, 0), "may_span={may_span}");
            assert_eq!(stage_set_status(&conn, s.stage_set_id), "cleaned");
        }
    }

    /// `unit_ids_written_to` counts a `range` write as a write to the tape,
    /// and still never a `cut` one.
    #[test]
    fn unit_ids_written_to_includes_range_writes_and_never_cut_ones() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let t = tenant(&conn);
        let alpha = unit(&conn, t, "testlib/alpha");
        let beta = unit(&conn, t, "testlib/beta");
        let (a, _f) = staged_span(&conn, alpha, 1, "staged", dir.path(), 3);
        let b = staged_version(&conn, beta, 1, "staged", dir.path(), 0);
        range_on(&conn, &a, "V-T1", 1..=2);
        cut_from(&conn, &b, "V-T1", 9);
        assert_eq!(unit_ids_written_to(&conn, "V-T1").unwrap(), [alpha]);
    }

    // --- per unit (ADR-0012 item 217) ---

    /// The brief's first shape: two units after copy 2, one covered and one
    /// at its floor but short of a per-location target. By default only the
    /// covered one is released; the other is kept and says where it is
    /// short. Undone (all or nothing), alpha would wait for beta's offsite
    /// copy; with the targets left out of the default (item 154's floor
    /// alone), beta would be released too.
    #[test]
    fn after_copy_two_only_the_unit_whose_targets_are_met_is_released() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 2);
        let t = tenant(&conn);
        let alpha = unit(&conn, t, "testlib/alpha");
        let beta = unit(&conn, t, "testlib/beta");
        two_targets(&conn, beta);
        let a = staged_version(&conn, alpha, 1, "current", dir.path(), 0);
        let b = staged_version(&conn, beta, 1, "current", dir.path(), 0);
        for (label, place) in [("V-1", "home"), ("V-2", "home")] {
            add_write(&conn, &a, label, "sealed", "completed");
            add_write(&conn, &b, label, "sealed", "completed");
            shelve(&conn, label, place);
        }

        let ids = unit_ids_written_to(&conn, "V-2").unwrap();
        assert_eq!(ids, [alpha, beta]);
        let out = release_if_covered(&conn, &config, &ids).unwrap();

        assert_eq!(out.released_units, ["testlib/alpha"]);
        assert_eq!(
            out.under_copied
                .iter()
                .map(|p| p.describe())
                .collect::<Vec<_>>(),
            [
                "unit testlib/beta has 2 copies, its floor 2; still needed: 0 copies at \
              required location offsite, needs 1"
            ]
        );
        assert_eq!(stage_set_status(&conn, a.stage_set_id), "cleaned");
        assert!(!a.file.exists());
        assert_eq!(stage_set_status(&conn, b.stage_set_id), "staged");
        assert!(b.file.exists());
    }

    /// Twins are emergent (ADR-0012 item 217, OQ14): the same units written
    /// to copies 1 and 2 are all short after copy 1 and all kept, and after
    /// copy 2 the two whose floor is 2 are released together, because each
    /// is covered, not because they are linked. A third unit on both tapes
    /// whose floor is higher (tape-only at 2x: 4) stays, which is what tells
    /// this rule from all or nothing: undone, the twins would wait for it.
    #[test]
    fn twins_are_released_together_after_copy_two_and_a_short_third_unit_stays() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(dir.path(), 2);
        config.compaction.tape_only_safety_multiplier = 2;
        let t = tenant(&conn);
        let alpha = unit(&conn, t, "testlib/alpha");
        let beta = unit(&conn, t, "testlib/beta");
        let gamma = unit(&conn, t, "testlib/gamma");
        conn.execute(
            "UPDATE units SET status = 'tape_only' WHERE id = ?1",
            params![gamma],
        )
        .unwrap();
        let sets: Vec<Staged> = [alpha, beta, gamma]
            .iter()
            .map(|&u| staged_version(&conn, u, 1, "current", dir.path(), 0))
            .collect();

        for s in &sets {
            add_write(&conn, s, "V-COPY1", "sealed", "completed");
        }
        let out = release_if_covered(
            &conn,
            &config,
            &unit_ids_written_to(&conn, "V-COPY1").unwrap(),
        )
        .unwrap();
        assert!(out.cleaned.is_none() && out.released_units.is_empty());
        assert_eq!(
            out.under_copied
                .iter()
                .map(|p| p.describe())
                .collect::<Vec<_>>(),
            [
                "unit testlib/alpha has 1 copy, needs 2",
                "unit testlib/beta has 1 copy, needs 2",
                "unit testlib/gamma has 1 copy, needs 4 (tape-only 2x)",
            ]
        );
        assert!(
            sets.iter().all(|s| s.file.exists()),
            "every twin is short until copy 2"
        );

        for s in &sets {
            add_write(&conn, s, "V-COPY2", "sealed", "completed");
        }
        let out = release_if_covered(
            &conn,
            &config,
            &unit_ids_written_to(&conn, "V-COPY2").unwrap(),
        )
        .unwrap();
        assert_eq!(out.released_units, ["testlib/alpha", "testlib/beta"]);
        assert_eq!(out.cleaned.expect("the twins are covered").sets_cleaned, 2);
        assert_eq!(
            out.under_copied
                .iter()
                .map(|p| p.describe())
                .collect::<Vec<_>>(),
            ["unit testlib/gamma has 2 copies, needs 4 (tape-only 2x)"]
        );
        assert_eq!(
            sets.iter().map(|s| s.file.exists()).collect::<Vec<_>>(),
            [false, false, true]
        );
    }

    /// `--keep-staging-for-locations` (item 154) is per unit too: the unit
    /// whose every target is met is released beside the one still short.
    #[test]
    fn keep_staging_for_locations_holds_each_unit_on_its_own() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 2);
        let t = tenant(&conn);
        let alpha = unit(&conn, t, "testlib/alpha");
        let beta = unit(&conn, t, "testlib/beta");
        two_targets(&conn, alpha);
        conn.execute(
            "UPDATE units SET copies_at_frozen = '{\"home\":1,\"offsite\":1}' WHERE id = ?1",
            params![beta],
        )
        .unwrap();
        let a = staged_version(&conn, alpha, 1, "current", dir.path(), 0);
        let b = staged_version(&conn, beta, 1, "current", dir.path(), 0);
        for (label, place) in [("V-1", "home"), ("V-2", "offsite")] {
            add_write(&conn, &a, label, "sealed", "completed");
            shelve(&conn, label, place);
        }
        add_write(&conn, &b, "V-2", "sealed", "completed");
        add_write(&conn, &b, "V-3", "sealed", "completed");
        shelve(&conn, "V-3", "offsite");

        let out = release_if_covered_holding(&conn, &config, &[alpha, beta], Hold::EveryLocation)
            .unwrap();

        assert_eq!(out.released_units, ["testlib/alpha"]);
        assert_eq!(out.under_copied.len(), 1, "{:?}", out.under_copied);
        assert_eq!(out.under_copied[0].unit_name, "testlib/beta");
        assert_eq!(out.under_copied[0].short_at[0].location, "home");
        assert!(!a.file.exists() && b.file.exists());
    }

    /// The per-slice release of a span keeps its meaning (item 129, OQ14):
    /// by default a set that holds a range releases each slice at the floor,
    /// whatever its unit's per-location targets say; under
    /// `--keep-staging-for-locations` a unit short of a target keeps every
    /// slice. Tape 1 holds slices 1-2 as a range, tape 2 the whole set, both
    /// at home; the unit's targets are one copy at home and one at offsite.
    #[test]
    fn a_span_releases_by_the_floor_unless_its_slices_wait_for_locations() {
        for hold in [Hold::CopyTargets, Hold::EveryLocation] {
            let conn = db::open_memory().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let config = config(dir.path(), 2);
            let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
            two_targets(&conn, alpha);
            let (s, files) = staged_span(&conn, alpha, 1, "staged", dir.path(), 4);
            range_on(&conn, &s, "V-T1", 1..=2);
            add_write(&conn, &s, "V-T2", "sealed", "completed");
            shelve(&conn, "V-T1", "home");
            shelve(&conn, "V-T2", "home");
            conn.execute(
                "UPDATE snapshots SET status = 'current' WHERE id = ?1",
                params![s.snapshot_id],
            )
            .unwrap();

            let out = release_if_covered_holding(&conn, &config, &[alpha], hold).unwrap();

            assert!(out.under_copied.is_empty(), "a span is not judged whole");
            if hold == Hold::CopyTargets {
                assert_eq!(
                    out.slices.slices_released,
                    vec![(s.stage_set_id, vec![1, 2])]
                );
                assert_eq!(files_exist(&files), [false, false, true, true]);
            } else {
                assert!(out.slices.slices_released.is_empty(), "{:?}", out.slices);
                assert_eq!(files_exist(&files), [true; 4]);
            }
        }
    }
}
