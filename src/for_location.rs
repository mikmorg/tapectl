//! Auto-stage for a location (#493, ADR-0012 items 151-159): what
//! `tapectl stage for-location X --volume LABEL` stages, and which staged
//! sets `tapectl volume write LABEL --for-location X` writes.
//!
//! **One judgement for both.** A version has need at X by item 158's groups
//! ([`NeedAt`], judged by `policy::coverage`): group 1, its target at X is
//! unmet; group 2, it is short of its copy floor with no target and no copy
//! at X. The version judged is the one a write would put on tape, never the
//! unit's current coverage alone: a new or changed unit's new version holds
//! 0 copies everywhere (item 153), so it is judged at 0/0 whatever its older
//! version holds, and a staged set is judged by its own version's counts
//! (`CoverageScope::Snapshot`). `volume write --for-location X` takes exactly
//! the staged sets [`staged_sets_for_location`] says are for X, and `stage
//! for-location` plans those same sets first (they ride, whatever their
//! priority), so the plan says what the write will do.
//!
//! **A span in progress** (item 159): a staged set whose owed run continues a
//! range on another tape (the run starts after slice 1, `layout-session.md`
//! "The plan order", the continuation item 86 keys its generation rule on)
//! goes only onto a tape bound for the location of the tape whose range ends
//! just before the run. Anywhere else it waits, named. An owed run that
//! starts at slice 1 continues no range (item 93's tape 2 begins a copy of
//! its own) and is judged by its need like any version.
//!
//! **The plan** (items 83, 95 and 152): the riding sets first, then the
//! units to stage in priority order, whole units before units that may span
//! (item 83), each next-fit (item 69): a unit is planned while
//! [`CapacityBudget::place`] puts every planned unit on this tape, and the
//! first one it does not closes the tape; the rest wait for the next tape
//! for X. The room is the volume's own (`collection::plan::destination_budget`):
//! its capacity at the fill ceiling less the end-of-tape reserve, and its
//! plan bound when the drive declares `post_warning_bytes` (item 113).
//!
//! **Staging** (items 153 and 154): a new or changed unit is snapshotted and
//! staged as `collection run` does; an unchanged unit's version is staged
//! from its source; a tape-only unit's version is read back with `volume
//! read-slices`, which this command prints and never runs (it moves no
//! tape); a unit whose source is unavailable is named and skipped.
//!
//! [`CapacityBudget::place`]: crate::volume::layout_model::CapacityBudget::place

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use crate::collection::fingerprint::{self, PendingReason};
use crate::collection::selector::BatchRoom;
use crate::config::{Config, TapectlPaths};
use crate::db::models::Unit;
use crate::error::{Result, TapectlError};
use crate::policy::coverage::{self, CoverageQuery, CoverageScope, NamedScope, NeedAt};
use crate::policy::ResolvedPolicy;
use crate::volume::layout_model::{pad_to_blocks, Placed, PlanClass, PlanItem};

/// The format-constant block size every slice is padded to on tape
/// (`cli::volume::DEFAULT_BLOCK_SIZE`).
const BLOCK_SIZE: u64 = 512 * 1024;

/// The location `location` names, trimmed, when a tape can be written for
/// it: a registered location that is not a warehouse. An unknown name is
/// refused listing the known ones; a warehouse is refused naming `volume
/// deposit add` and the tapes that would meet the targets there (ADR-0012
/// item 157): a copy at a warehouse is a recorded deposit of a sealed tape,
/// never a tape written for it.
pub fn check_location(conn: &Connection, config: &Config, location: &str) -> Result<String> {
    let name = location.trim();
    let kind: Option<String> = conn
        .query_row("SELECT kind FROM locations WHERE name = ?1", [name], |r| {
            r.get(0)
        })
        .optional()?;
    match kind.as_deref() {
        Some("warehouse") => Err(TapectlError::Other(warehouse_refusal(conn, config, name)?)),
        Some(_) => Ok(name.to_string()),
        None => {
            let known: Vec<String> = conn
                .prepare("SELECT name FROM locations ORDER BY name")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let list = if known.is_empty() {
                "no locations are defined yet — add one with `tapectl location add <NAME>`"
                    .to_string()
            } else {
                format!("known locations: {}", known.join(", "))
            };
            Err(TapectlError::Other(format!(
                "location \"{name}\" not found ({list}). Nothing was staged or written."
            )))
        }
    }
}

/// Item 157's refusal of the warehouse `name`: what a copy there is, the
/// command that records one, and the sealed tapes holding a current version
/// of each unit whose target at `name` is unmet (group 1) and not deposited
/// there yet, each with those units.
fn warehouse_refusal(conn: &Connection, config: &Config, name: &str) -> Result<String> {
    let mut tapes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut short_with_no_tape: Vec<String> = Vec::new();
    for unit in judged_units(conn)? {
        let Ok(resolved) = crate::policy::resolve(conn, config, &unit) else {
            continue;
        };
        let need = coverage::need_at(conn, config, &unit, &resolved, name)?;
        if !matches!(need, NeedAt::Target { .. }) {
            continue;
        }
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT v.label, sn.version FROM writes w
               JOIN stage_sets ss ON ss.id = w.stage_set_id
               JOIN snapshots sn ON sn.id = ss.snapshot_id
               JOIN volumes v ON v.id = w.volume_id
              WHERE sn.unit_id = ?1 AND sn.status = 'current'
                AND w.status IN ('completed', 'range') AND {eligible}
                AND NOT EXISTS (SELECT 1 FROM volume_deposits d
                                  JOIN locations l ON l.id = d.location_id
                                 WHERE d.volume_id = v.id AND l.name = ?2)
              ORDER BY v.label, sn.version",
            eligible = coverage::eligible("v"),
        ))?;
        let rows: Vec<(String, i64)> = stmt
            .query_map(params![unit.id, name], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        if rows.is_empty() {
            short_with_no_tape.push(unit.name.clone());
        }
        for (label, version) in rows {
            tapes
                .entry(label)
                .or_default()
                .push(format!("{} v{version}", unit.name));
        }
    }
    let mut text = format!(
        "location \"{name}\" is a warehouse: a copy there is a recorded deposit of a sealed \
         tape, not a tape written for it (ADR-0012 item 157), so nothing is staged or written \
         for it. Once a tape's bytes are at the warehouse, record the deposit with `tapectl \
         volume deposit add <LABEL> --to {name}`."
    );
    if tapes.is_empty() && short_with_no_tape.is_empty() {
        text.push_str(&format!(
            " No unit has a target at \"{name}\" that is unmet."
        ));
    }
    if !tapes.is_empty() {
        let list: Vec<String> = tapes
            .iter()
            .map(|(label, units)| format!("{label} ({})", units.join(", ")))
            .collect();
        text.push_str(&format!(
            " The sealed tapes that would meet the targets there: {}.",
            list.join("; ")
        ));
    }
    if !short_with_no_tape.is_empty() {
        text.push_str(&format!(
            " No sealed tape holds a current version of {} yet: write one first.",
            short_with_no_tape.join(", ")
        ));
    }
    Ok(text)
}

/// The units whose need is judged, in name order: the statuses `audit`
/// checks (active, tape-only, missing), as `report needs` lists them.
fn judged_units(conn: &Connection) -> Result<Vec<Unit>> {
    let mut units: Vec<Unit> = crate::db::queries::list_units(conn, None, None)?
        .into_iter()
        .filter(|u| crate::cli::audit::ACTIVE_TAPE_ONLY_MISSING.contains(&u.status.as_str()))
        .collect();
    units.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(units)
}

/// One `volume read-slices` a tape-only unit's version needs: the slices of
/// it on the holder `from` (#493, item 153).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadSlicesCommand {
    pub from: String,
    pub unit: String,
    pub version: i64,
    /// The slices this read is run for, in slice order.
    pub slices: Vec<i64>,
}

impl ReadSlicesCommand {
    /// The command line.
    pub fn command(&self) -> String {
        format!(
            "tapectl volume read-slices --from {} --unit {} --version {}",
            self.from, self.unit, self.version
        )
    }
}

/// How a planned unit's version comes to be staged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum How {
    /// It is staged already: written as it is (never staged again).
    AlreadyStaged { stage_set_id: i64 },
    /// A new or changed unit: snapshotted, then staged (item 153).
    Snapshot { reason: PendingReason },
    /// An existing version, staged from the unit's source: `restage` when
    /// the version was staged before (`stage create --version`).
    Stage { snapshot_id: i64, restage: bool },
    /// No source (a tape-only unit, or a version older than the source):
    /// read back from tape with these commands, which the operator runs.
    ReadSlices { commands: Vec<ReadSlicesCommand> },
}

/// One unit's version as the plan sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub unit: String,
    pub unit_status: String,
    /// `None` for a new or changed unit's version, not snapshotted yet.
    pub version: Option<i64>,
    /// Its need at X (item 158). [`NeedAt::None`] only for a span that
    /// continues at X ([`Self::continues_span`]).
    pub need: NeedAt,
    /// A span in progress whose earlier range sits at X: its owed run
    /// continues on this tape (item 159).
    pub continues_span: bool,
    /// What it is to the plan order.
    pub class: PlanClass,
    /// The slices this tape plans of it, block-padded: exact for a staged
    /// set or a read-back, the source's size for a unit still to stage.
    pub bytes: u64,
    pub how: How,
}

impl Candidate {
    /// Item 152's order: a continuing span, then group 1 by how far short
    /// at X (most first) and the fewest copies in all, then group 2 by the
    /// fewest copies; then the name, and the newest version first.
    fn priority(&self) -> (u8, i64, i64, String, std::cmp::Reverse<i64>) {
        let (group, short, copies) = match self.need {
            _ if self.continues_span => (0, 0, 0),
            NeedAt::Target { copies, .. } => (1, -self.need.short_by(), copies),
            NeedAt::Floor { copies, .. } => (2, 0, copies),
            NeedAt::None => (3, 0, 0),
        };
        (
            group,
            short,
            copies,
            self.unit.clone(),
            std::cmp::Reverse(self.version.unwrap_or(i64::MAX)),
        )
    }

    /// `"alpha" v3`, or `"alpha" (new version)`.
    pub fn name(&self) -> String {
        match self.version {
            Some(v) => format!("\"{}\" v{v}", self.unit),
            None => format!("\"{}\" (new version)", self.unit),
        }
    }

    /// Its need at `location`, in words.
    pub fn describe_need(&self, location: &str) -> String {
        let noun = |n: i64| if n == 1 { "copy" } else { "copies" };
        match self.need {
            _ if self.continues_span => format!(
                "continues its span: its earlier range is on a tape at {location} (item 159)"
            ),
            NeedAt::Target {
                target,
                copies_here,
                copies,
            } => format!(
                "group 1: {copies_here} of its target {target} at {location}, {copies} {} in all",
                noun(copies)
            ),
            NeedAt::Floor { floor, copies } => {
                format!("group 2: {copies} of its copy floor {floor}, none at {location}")
            }
            NeedAt::None => "no need".to_string(),
        }
    }
}

/// A staged set's version whose span continues on a tape at another
/// location (item 159): it waits for a tape bound there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanWaiting {
    pub unit: String,
    pub version: i64,
    /// Where the tape whose range its owed run continues sits; `None` for a
    /// tape with no recorded location.
    pub at: Vec<Option<String>>,
}

impl SpanWaiting {
    /// `home`, or `no recorded location`.
    pub fn describe_at(&self) -> String {
        describe_locations(&self.at)
    }
}

/// Locations as a message names them, `no recorded location` for `None`.
fn describe_locations(at: &[Option<String>]) -> String {
    at.iter()
        .map(|a| {
            a.clone()
                .unwrap_or_else(|| "no recorded location".to_string())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A unit with need at X that this run cannot stage, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub unit: String,
    pub reason: String,
}

/// What `stage for-location X --volume LABEL` plans (#493, item 151).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForLocationPlan {
    pub location: String,
    pub volume: String,
    /// What the units are packed against: the volume's room under the fill
    /// ceiling and its plan bound.
    pub room: BatchRoom,
    /// The units this tape takes, in plan order: the staged sets for X
    /// first (they ride whatever their priority), then the units to stage.
    pub planned: Vec<Candidate>,
    /// The units with need at X the tape has no room for, in priority order:
    /// they wait for the next tape for X.
    pub waiting: Vec<Candidate>,
    /// The staged sets for X the volume cannot hold even before anything is
    /// staged (`volume write --for-location` would not take them all), by
    /// name.
    pub staged_overflow: Vec<String>,
    /// Spans in progress that continue at another location (item 159).
    pub spans_waiting: Vec<SpanWaiting>,
    pub skipped: Vec<Skipped>,
    /// The destination's own warnings (`collection::plan::destination_budget`).
    pub warnings: Vec<String>,
}

impl ForLocationPlan {
    /// The `volume read-slices` commands the planned units need, in the
    /// order to run them: grouped by holder tape in the order the plan
    /// first needs each, so each cartridge is loaded once.
    pub fn read_slices(&self) -> Vec<&ReadSlicesCommand> {
        let commands: Vec<&ReadSlicesCommand> = self
            .planned
            .iter()
            .flat_map(|c| match &c.how {
                How::ReadSlices { commands } => commands.iter().collect(),
                _ => Vec::new(),
            })
            .collect();
        let mut holders: Vec<&str> = Vec::new();
        for c in &commands {
            if !holders.contains(&c.from.as_str()) {
                holders.push(&c.from);
            }
        }
        holders
            .into_iter()
            .flat_map(|h| commands.iter().copied().filter(move |c| c.from == h))
            .collect()
    }

    /// The bytes the planned units take, block-padded.
    pub fn planned_bytes(&self) -> u64 {
        self.planned.iter().map(|c| c.bytes).sum()
    }
}

/// A staged set as [`staged_sets_for_location`] names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedSet {
    pub stage_set_id: i64,
    pub unit: String,
    pub version: i64,
}

/// A staged set `volume write --for-location X` leaves staged, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotForLocation {
    pub set: StagedSet,
    pub why: String,
}

/// The staged sets a write for X takes, and the rest (item 151).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StagedForLocation {
    pub for_location: Vec<StagedSet>,
    pub not_for_location: Vec<NotForLocation>,
}

impl StagedForLocation {
    /// The stage sets to write, in id order.
    pub fn stage_set_ids(&self) -> Vec<i64> {
        self.for_location.iter().map(|s| s.stage_set_id).collect()
    }
}

/// The staged sets `volume write LABEL --for-location X` writes (#493, item
/// 151): every `staged` set whose version still has need at X, or whose span
/// continues at X (item 159). The others stay staged and are named with why:
/// no need at X, a span that continues at another location, or a unit whose
/// policy cannot be resolved (its need cannot be judged). `location` is
/// checked first ([`check_location`]).
pub fn staged_sets_for_location(
    conn: &Connection,
    config: &Config,
    location: &str,
) -> Result<StagedForLocation> {
    let location = check_location(conn, config, location)?;
    let mut out = StagedForLocation::default();
    for unit in units_with_staged_sets(conn)? {
        let sets = staged_sets_of(conn, unit.id)?;
        let resolved = match crate::policy::resolve(conn, config, &unit) {
            Ok(resolved) => resolved,
            Err(e @ TapectlError::PolicyUnresolvable { .. }) => {
                for s in sets {
                    out.not_for_location.push(NotForLocation {
                        set: StagedSet {
                            stage_set_id: s.stage_set_id,
                            unit: unit.name.clone(),
                            version: s.version,
                        },
                        why: format!("its need cannot be judged: {e}"),
                    });
                }
                continue;
            }
            Err(e) => return Err(e),
        };
        for s in sets {
            let judged = judge_staged_set(conn, config, &unit, &resolved, &location, &s)?;
            let set = StagedSet {
                stage_set_id: s.stage_set_id,
                unit: unit.name.clone(),
                version: s.version,
            };
            match judged.verdict {
                Verdict::ForHere => out.for_location.push(set),
                Verdict::NoNeed => out.not_for_location.push(NotForLocation {
                    set,
                    why: format!("no need at {location}"),
                }),
                Verdict::SpanElsewhere(at) => out.not_for_location.push(NotForLocation {
                    why: format!(
                        "its span continues on a tape at {} (item 159)",
                        describe_locations(&at)
                    ),
                    set,
                }),
            }
        }
    }
    out.for_location.sort_by_key(|s| s.stage_set_id);
    Ok(out)
}

/// The units holding a `staged` stage set, in name order.
fn units_with_staged_sets(conn: &Connection) -> Result<Vec<Unit>> {
    let names: Vec<String> = conn
        .prepare(
            "SELECT DISTINCT u.name FROM stage_sets ss
               JOIN snapshots sn ON sn.id = ss.snapshot_id
               JOIN units u ON u.id = sn.unit_id
              WHERE ss.status = 'staged'
                AND EXISTS (SELECT 1 FROM stage_slices sl
                             WHERE sl.stage_set_id = ss.id AND sl.staging_path IS NOT NULL)
              ORDER BY u.name",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut units = Vec::with_capacity(names.len());
    for name in names {
        units.push(
            crate::db::queries::get_unit_by_name(conn, &name)?
                .ok_or_else(|| TapectlError::UnitNotFound(name.clone()))?,
        );
    }
    Ok(units)
}

/// One `staged` stage set of a unit.
#[derive(Debug, Clone)]
struct StagedRow {
    stage_set_id: i64,
    snapshot_id: i64,
    version: i64,
}

/// `unit_id`'s `staged` stage sets with a slice still on disk, in version
/// then id order: the sets `volume write` selects (`ss.status = 'staged'`,
/// a set with no slice on disk planning nothing), as
/// `collection::plan::destination_budget` counts what rides.
fn staged_sets_of(conn: &Connection, unit_id: i64) -> Result<Vec<StagedRow>> {
    let rows = conn
        .prepare(
            "SELECT ss.id, ss.snapshot_id, sn.version FROM stage_sets ss
               JOIN snapshots sn ON sn.id = ss.snapshot_id
              WHERE sn.unit_id = ?1 AND ss.status = 'staged'
                AND EXISTS (SELECT 1 FROM stage_slices sl
                             WHERE sl.stage_set_id = ss.id AND sl.staging_path IS NOT NULL)
              ORDER BY sn.version, ss.id",
        )?
        .query_map(params![unit_id], |r| {
            Ok(StagedRow {
                stage_set_id: r.get(0)?,
                snapshot_id: r.get(1)?,
                version: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Whether a staged set is for X.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    /// Its version has need at X, or its span continues at X.
    ForHere,
    NoNeed,
    /// Its span continues on a tape at these locations (item 159).
    SpanElsewhere(Vec<Option<String>>),
}

/// A staged set judged for X.
struct StagedJudged {
    verdict: Verdict,
    candidate: Candidate,
}

/// One version's copies in all and at `location`: `CoverageScope::Snapshot`
/// counts, the version's own (ADR-0012: a copy is counted per version).
fn version_counts(conn: &Connection, snapshot_id: i64, location: &str) -> Result<(i64, i64)> {
    let copies: i64 = conn.query_row(
        &format!(
            "SELECT {}",
            coverage::copy_count_expr(&CoverageQuery {
                scope: CoverageScope::Snapshot { id_expr: "?1" },
                exclude_volume: None,
            })
        ),
        params![snapshot_id],
        |r| r.get(0),
    )?;
    let here =
        coverage::copies_at_location(conn, NamedScope::Snapshot(snapshot_id), location, None)?;
    Ok((copies, here))
}

/// `stage_set_id`'s slices, block-padded: all of them, only those still on
/// disk (`live`), or only those numbered within `run`.
fn padded_slices(
    conn: &Connection,
    stage_set_id: i64,
    live: bool,
    run: Option<(i64, i64)>,
) -> Result<u64> {
    let sizes: Vec<(i64, i64, Option<String>)> = conn
        .prepare(
            "SELECT slice_number, encrypted_bytes, staging_path FROM stage_slices
              WHERE stage_set_id = ?1",
        )?
        .query_map(params![stage_set_id], |r| {
            Ok((
                r.get(0)?,
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                r.get(2)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(sizes
        .into_iter()
        .filter(|(n, _, path)| {
            (!live || path.is_some()) && run.is_none_or(|(a, b)| (a..=b).contains(n))
        })
        .map(|(_, bytes, _)| pad_to_blocks(bytes.max(0) as u64, BLOCK_SIZE))
        .sum())
}

/// How the write plans a staged set (`volume::write::find_staged_data` and
/// its `plan_span_set`, mirrored from the catalog): a set whose unit allows
/// spanning or that has a `range` row is planned as a span, its owed run
/// ([`PlanClass::Owed`]) when it has a `range` row on a sealed volume and
/// slices short of its copy floor, otherwise the whole set
/// ([`PlanClass::Spannable`]); any other set is a whole unit of its slices
/// still on disk. Returns the class, the owed run, and the block-padded
/// bytes the tape plans.
fn staged_set_plan(
    conn: &Connection,
    config: &Config,
    unit: &Unit,
    resolved: &ResolvedPolicy,
    stage_set_id: i64,
) -> Result<(PlanClass, Option<(i64, i64)>, u64)> {
    let (has_range, sealed_range): (bool, bool) = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM writes WHERE stage_set_id = ?1 AND status = 'range'),
                EXISTS(SELECT 1 FROM writes w JOIN volumes v ON v.id = w.volume_id
                        WHERE w.stage_set_id = ?1 AND w.status = 'range'
                          AND v.status = 'sealed')",
        params![stage_set_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if !(resolved.allow_spanning || has_range) {
        return Ok((
            PlanClass::Whole,
            None,
            padded_slices(conn, stage_set_id, true, None)?,
        ));
    }
    let floor = coverage::copy_floor(config, resolved.copy_floor_base(), &unit.status);
    let owed: Vec<i64> = if sealed_range {
        coverage::slice_copies(conn, stage_set_id)?
            .into_iter()
            .filter(|s| s.copies < floor)
            .map(|s| s.slice_number)
            .collect()
    } else {
        Vec::new()
    };
    match (owed.iter().min(), owed.iter().max()) {
        (Some(&first), Some(&last)) => Ok((
            PlanClass::Owed,
            Some((first, last)),
            padded_slices(conn, stage_set_id, false, Some((first, last)))?,
        )),
        _ => Ok((
            PlanClass::Spannable,
            None,
            padded_slices(conn, stage_set_id, false, None)?,
        )),
    }
}

/// Where the tapes sit whose `range` of `stage_set_id` ends just before
/// slice `first` (sealed volumes; `None` for one with no recorded location):
/// the tape an owed run starting at `first` continues (item 86's rule for
/// which range a run continues). Empty when none does.
fn continued_range_locations(
    conn: &Connection,
    stage_set_id: i64,
    first: i64,
) -> Result<Vec<Option<String>>> {
    let rows = conn
        .prepare(
            "SELECT DISTINCT l.name FROM writes w
               JOIN volumes v ON v.id = w.volume_id
               LEFT JOIN locations l ON l.id = v.location_id
              WHERE w.stage_set_id = ?1 AND w.status = 'range' AND v.status = 'sealed'
                AND (SELECT MAX(sl.slice_number) FROM write_positions wp
                       JOIN stage_slices sl ON sl.id = wp.stage_slice_id
                      WHERE wp.write_id = w.id AND wp.status = 'written') = ?2
              ORDER BY l.name",
        )?
        .query_map(params![stage_set_id, first - 1], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// A staged set judged for `location`: its version's need there by its own
/// counts, how the write plans it, and whether it is for X. A set whose owed
/// run continues a range (it starts after slice 1) is for X exactly when
/// every tape whose range it continues sits at X, whatever its need (item
/// 159); every other set is for X when its version has need there.
fn judge_staged_set(
    conn: &Connection,
    config: &Config,
    unit: &Unit,
    resolved: &ResolvedPolicy,
    location: &str,
    set: &StagedRow,
) -> Result<StagedJudged> {
    let (copies, here) = version_counts(conn, set.snapshot_id, location)?;
    let need = NeedAt::judge(config, resolved, &unit.status, location, copies, here);
    let (class, run, bytes) = staged_set_plan(conn, config, unit, resolved, set.stage_set_id)?;
    let continued = match run {
        Some((first, _)) if first > 1 => continued_range_locations(conn, set.stage_set_id, first)?,
        _ => Vec::new(),
    };
    let continues_here =
        !continued.is_empty() && continued.iter().all(|l| l.as_deref() == Some(location));
    let verdict = if !continued.is_empty() && !continues_here {
        Verdict::SpanElsewhere(continued)
    } else if continues_here || need != NeedAt::None {
        Verdict::ForHere
    } else {
        Verdict::NoNeed
    };
    Ok(StagedJudged {
        verdict,
        candidate: Candidate {
            unit: unit.name.clone(),
            unit_status: unit.status.clone(),
            version: Some(set.version),
            need,
            continues_span: continues_here,
            class,
            bytes,
            how: How::AlreadyStaged {
                stage_set_id: set.stage_set_id,
            },
        },
    })
}

/// Where a unit's files can be read from to stage it.
enum Source {
    /// Its folder is there: `pending` is the fingerprint's verdict (`None`:
    /// its latest snapshot matches the folder).
    Available {
        pending: Option<fingerprint::PendingUnit>,
    },
    /// A tape-only unit: read back from tape.
    TapeOnly,
    /// Missing, or its folder is not there.
    Unavailable(String),
}

fn source_of(conn: &Connection, config: &Config, unit: &Unit) -> Result<Source> {
    if unit.status == "tape_only" {
        return Ok(Source::TapeOnly);
    }
    let path = unit.current_path.as_deref().unwrap_or("");
    if unit.status != "active" {
        return Ok(Source::Unavailable(format!(
            "it is {} (its source at {} is not there)",
            unit.status,
            if path.is_empty() { "?" } else { path }
        )));
    }
    if path.is_empty() || !Path::new(path).is_dir() {
        return Ok(Source::Unavailable(format!(
            "its source folder {} is not there",
            if path.is_empty() { "(no path)" } else { path }
        )));
    }
    let pending = fingerprint::classify(conn, unit, &config.defaults.global_excludes)?;
    Ok(Source::Available { pending })
}

/// A unit's snapshot that a plan may stage.
struct VersionRow {
    snapshot_id: i64,
    version: i64,
    status: String,
    total_size: Option<i64>,
}

/// `unit_id`'s versions that are not given up (created, staged, current), in
/// version order.
fn versions_of(conn: &Connection, unit_id: i64) -> Result<Vec<VersionRow>> {
    let rows = conn
        .prepare(
            "SELECT id, version, status, total_size FROM snapshots
              WHERE unit_id = ?1 AND status IN ('created', 'staged', 'current')
              ORDER BY version",
        )?
        .query_map(params![unit_id], |r| {
            Ok(VersionRow {
                snapshot_id: r.get(0)?,
                version: r.get(1)?,
                status: r.get(2)?,
                total_size: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// What a version's read-back from tape takes: the `volume read-slices`
/// commands, one per holder tape, covering every slice of its
/// lowest-numbered stage set that live volumes hold whole (item 138's
/// credit), and the set's block-padded bytes. Each command takes the holder
/// that holds the most slices still to read among those holding the lowest
/// one (in-service before quarantined, then volume order). `None` when no
/// stage set of the version is held whole.
fn read_back(
    conn: &Connection,
    unit: &str,
    snapshot_id: i64,
    version: i64,
) -> Result<Option<(Vec<ReadSlicesCommand>, u64)>> {
    let sets: Vec<i64> = conn
        .prepare("SELECT id FROM stage_sets WHERE snapshot_id = ?1 ORDER BY id")?
        .query_map(params![snapshot_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for set in sets {
        let slices: Vec<i64> = conn
            .prepare(
                "SELECT slice_number FROM stage_slices WHERE stage_set_id = ?1
                  ORDER BY slice_number",
            )?
            .query_map(params![set], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        if slices.is_empty() {
            continue;
        }
        let holders = crate::volume::slice_holders::holders_of_set(conn, set)?;
        // Holder labels in the order `holders_of_set` gives them for the
        // first slice each holds: in service first, then volume order.
        let mut held: Vec<(String, Vec<i64>)> = Vec::new();
        for h in &holders {
            match held.iter_mut().find(|(l, _)| *l == h.holder.label) {
                Some((_, s)) => s.push(h.slice_number),
                None => held.push((h.holder.label.clone(), vec![h.slice_number])),
            }
        }
        let mut left: Vec<i64> = slices.clone();
        let mut commands = Vec::new();
        while let Some(&lowest) = left.first() {
            let best = held
                .iter()
                .filter(|(_, s)| s.contains(&lowest))
                .max_by_key(|(l, s)| {
                    let n = s.iter().filter(|n| left.contains(n)).count();
                    // `max_by_key` keeps the LAST maximum: rank earlier
                    // holders higher so the first in order wins a tie.
                    let rank = held.len() - held.iter().position(|(x, _)| x == l).unwrap_or(0);
                    (n, rank)
                });
            let Some((label, s)) = best else {
                break;
            };
            let mut covered: Vec<i64> = left.iter().copied().filter(|n| s.contains(n)).collect();
            covered.sort_unstable();
            left.retain(|n| !covered.contains(n));
            commands.push(ReadSlicesCommand {
                from: label.clone(),
                unit: unit.to_string(),
                version,
                slices: covered,
            });
        }
        if left.is_empty() {
            return Ok(Some((commands, padded_slices(conn, set, false, None)?)));
        }
    }
    Ok(None)
}

/// What one unit brings to a plan for X.
#[derive(Default)]
struct UnitJudgement {
    /// Its staged sets for X: they ride (item 151).
    riding: Vec<Candidate>,
    /// Its best version to stage for X, when nothing of it is staged for X.
    to_stage: Option<Candidate>,
    spans_waiting: Vec<SpanWaiting>,
    skipped: Option<Skipped>,
}

/// One unit judged for `location` (items 152, 153, 158 and 159).
///
/// Its staged sets are judged first ([`judge_staged_set`]); those for X
/// ride. When none is, the versions it could stage are judged, each by its
/// own counts: a new or changed version at 0 copies everywhere (item 153),
/// its latest version from the source (staged again when it was staged
/// before), and an older current version, or any version of a tape-only
/// unit, read back from tape; the one with the most urgent need (item 152's
/// order) is its candidate. A unit whose source is unavailable and that has
/// need is skipped and named, as is one whose policy cannot be resolved or
/// whose dotfile cannot be read, and a read-back with no live volume holding
/// a whole stage set.
fn judge_unit(
    conn: &Connection,
    config: &Config,
    unit: &Unit,
    location: &str,
) -> Result<UnitJudgement> {
    let mut out = UnitJudgement::default();
    let resolved = match crate::policy::resolve(conn, config, unit) {
        Ok(resolved) => resolved,
        Err(e @ TapectlError::PolicyUnresolvable { .. }) => {
            out.skipped = Some(Skipped {
                unit: unit.name.clone(),
                reason: format!("its need cannot be judged: {e}"),
            });
            return Ok(out);
        }
        Err(e) => return Err(e),
    };
    let staged = staged_sets_of(conn, unit.id)?;
    for set in &staged {
        let judged = judge_staged_set(conn, config, unit, &resolved, location, set)?;
        match judged.verdict {
            Verdict::ForHere => out.riding.push(judged.candidate),
            Verdict::NoNeed => {}
            Verdict::SpanElsewhere(at) => out.spans_waiting.push(SpanWaiting {
                unit: unit.name.clone(),
                version: set.version,
                at,
            }),
        }
    }
    if !out.riding.is_empty() {
        return Ok(out);
    }
    let staged_snapshots: Vec<i64> = staged.iter().map(|s| s.snapshot_id).collect();
    let class = if resolved.allow_spanning {
        PlanClass::Spannable
    } else {
        PlanClass::Whole
    };
    let judge = |copies: i64, here: i64| {
        NeedAt::judge(config, &resolved, &unit.status, location, copies, here)
    };

    let source = match source_of(conn, config, unit) {
        Ok(source) => source,
        // The fingerprint walk fails only on the unit's own dotfile (issue
        // #285): that unit is refused, never the run.
        Err(e) => {
            out.skipped = Some(Skipped {
                unit: unit.name.clone(),
                reason: format!("its dotfile cannot be read: {e}"),
            });
            return Ok(out);
        }
    };
    let versions = versions_of(conn, unit.id)?;
    let latest = versions.last().map(|v| v.snapshot_id);
    let mut options: Vec<Candidate> = Vec::new();
    let mut unreadable: Vec<i64> = Vec::new();
    let mut unavailable = false;

    if let Source::Available {
        pending: Some(pending),
    } = &source
    {
        let need = judge(0, 0);
        if need != NeedAt::None {
            options.push(Candidate {
                unit: unit.name.clone(),
                unit_status: unit.status.clone(),
                version: None,
                need,
                continues_span: false,
                class,
                bytes: pad_to_blocks(pending.estimated_bytes, BLOCK_SIZE),
                how: How::Snapshot {
                    reason: pending.reason,
                },
            });
        }
    }
    for v in versions
        .iter()
        .filter(|v| !staged_snapshots.contains(&v.snapshot_id))
    {
        let from_source =
            matches!(source, Source::Available { pending: None }) && Some(v.snapshot_id) == latest;
        // An older version, or any version of a changed unit, comes only
        // from tape; and a version never written is on no tape.
        if !from_source && v.status != "current" {
            continue;
        }
        let (copies, here) = version_counts(conn, v.snapshot_id, location)?;
        let need = judge(copies, here);
        if need == NeedAt::None {
            continue;
        }
        let (how, bytes) = if from_source {
            (
                How::Stage {
                    snapshot_id: v.snapshot_id,
                    restage: v.status != "created",
                },
                pad_to_blocks(v.total_size.unwrap_or(0).max(0) as u64, BLOCK_SIZE),
            )
        } else if matches!(source, Source::Unavailable(_)) {
            unavailable = true;
            continue;
        } else {
            match read_back(conn, &unit.name, v.snapshot_id, v.version)? {
                Some((commands, bytes)) => (How::ReadSlices { commands }, bytes),
                None => {
                    unreadable.push(v.version);
                    continue;
                }
            }
        };
        options.push(Candidate {
            unit: unit.name.clone(),
            unit_status: unit.status.clone(),
            version: Some(v.version),
            need,
            continues_span: false,
            class,
            bytes,
            how,
        });
    }
    options.sort_by_key(Candidate::priority);
    out.to_stage = options.into_iter().next();
    if out.to_stage.is_none() {
        if let Source::Unavailable(why) = &source {
            // A unit never archived whose folder is gone has need too: it
            // has no copy anywhere.
            let never_archived = versions.is_empty() && judge(0, 0) != NeedAt::None;
            if unavailable || never_archived {
                out.skipped = Some(Skipped {
                    unit: unit.name.clone(),
                    reason: format!("{why}, so it cannot be staged (item 153)"),
                });
            }
        } else if !unreadable.is_empty() {
            out.skipped = Some(Skipped {
                unit: unit.name.clone(),
                reason: format!(
                    "no live volume holds every slice of {}, so it cannot be read back",
                    unreadable
                        .iter()
                        .map(|v| format!("v{v}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
    }
    Ok(out)
}

/// Whether `placement` puts every unit on this tape: none deferred to a
/// later tape and none refused.
fn every_unit_on_this_tape(placement: &crate::volume::layout_model::Placement) -> bool {
    placement
        .placed
        .iter()
        .all(|p| !matches!(p, Placed::Deferred | Placed::Refused))
}

/// Where a class goes in the walk: owed runs, whole units, units that may
/// span (items 83, 95 and 122).
fn class_rank(class: PlanClass) -> u8 {
    match class {
        PlanClass::Owed => 0,
        PlanClass::Whole => 1,
        PlanClass::Spannable => 2,
    }
}

/// Plan `stage for-location X --volume LABEL` (#493, item 151) from the
/// catalog and the sources alone: nothing is snapshotted, staged or written
/// (the `--dry-run` answer, and what a real run then stages).
///
/// `label` must be a write target, and its room is
/// `collection::plan::destination_budget`'s: an empty volume's room under
/// the fill ceiling (`unit_limit_bytes`) and its plan bound, with nothing
/// subtracted for what is staged — the staged sets a write for X takes are
/// planned here, and those it does not take do not ride. `device` names the
/// drive whose fill ceiling and reserve apply (no drive is touched).
pub fn plan(
    conn: &Connection,
    config: &Config,
    location: &str,
    label: &str,
    device: &str,
) -> Result<ForLocationPlan> {
    let location = check_location(conn, config, location)?;
    let destination =
        crate::collection::plan::destination_budget(conn, config, device, &[label.to_string()])?;
    let room = BatchRoom {
        fill: destination.unit_limit_bytes,
        bound: destination.plan_bound_bytes,
    };

    let mut riding: Vec<Candidate> = Vec::new();
    let mut to_stage: Vec<Candidate> = Vec::new();
    let mut spans_waiting = Vec::new();
    let mut skipped = Vec::new();
    for unit in judged_units(conn)? {
        let judged = judge_unit(conn, config, &unit, &location)?;
        riding.extend(judged.riding);
        to_stage.extend(judged.to_stage);
        spans_waiting.extend(judged.spans_waiting);
        skipped.extend(judged.skipped);
    }
    let order = |c: &Candidate| (class_rank(c.class), c.priority());
    riding.sort_by_key(order);
    to_stage.sort_by_key(order);

    let budget = room.budget();
    let item = |c: &Candidate| PlanItem {
        class: c.class,
        bytes: c.bytes,
    };
    let mut items: Vec<PlanItem> = riding.iter().map(item).collect();
    let riding_placement = budget.place(0, &items);
    let staged_overflow: Vec<String> = riding
        .iter()
        .zip(&riding_placement.placed)
        .filter(|(_, p)| matches!(p, Placed::Deferred | Placed::Refused))
        .map(|(c, _)| c.name())
        .collect();
    let mut planned = riding;
    let mut waiting = Vec::new();
    let mut closed = !staged_overflow.is_empty();
    for c in to_stage {
        if !closed {
            items.push(item(&c));
            if every_unit_on_this_tape(&budget.place(0, &items)) {
                planned.push(c);
                continue;
            }
            items.pop();
            closed = true;
        }
        waiting.push(c);
    }
    Ok(ForLocationPlan {
        location,
        volume: label.to_string(),
        room,
        planned,
        waiting,
        staged_overflow,
        spans_waiting,
        skipped,
        warnings: destination.warnings,
    })
}

/// What staging one planned unit did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Staged {
    /// Staged by this run: the stage set and the version it holds.
    Now { stage_set_id: i64, version: i64 },
    /// Staged already: written as it is.
    Already,
    /// Read back by the `volume read-slices` commands the plan prints; this
    /// run staged nothing of it.
    ReadSlices,
    /// Nothing to stage, and why.
    Nothing(String),
}

/// Stage `plan`'s units (#493, items 151 and 153): snapshot each new or
/// changed unit, then stage it, and stage each version from its source, with
/// `stage create`'s pipeline (`staging::jobs::stage_many`, `jobs` at once).
/// A staged set is never staged again, and a read-back is left to the
/// operator (this command moves no tape). One answer per planned unit, in
/// plan order. A snapshot that finds the version staged already, or already
/// on tape, stages nothing, as `collection run` does.
pub fn stage(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    plan: &ForLocationPlan,
    jobs: usize,
    assume_yes: bool,
) -> Result<Vec<Staged>> {
    use crate::staging::{jobs::StageJob, Admission};
    let mut results: Vec<Option<Staged>> = vec![None; plan.planned.len()];
    let mut to_stage: Vec<(usize, StageJob)> = Vec::new();
    let job = |unit: &str, snapshot_id: i64, admission: Admission| StageJob {
        unit_name: unit.to_string(),
        snapshot_id,
        admission,
        view: None,
    };
    for (i, c) in plan.planned.iter().enumerate() {
        match &c.how {
            How::AlreadyStaged { .. } => results[i] = Some(Staged::Already),
            How::ReadSlices { .. } => results[i] = Some(Staged::ReadSlices),
            How::Stage {
                snapshot_id,
                restage,
            } => {
                let admission = if *restage {
                    Admission::Restage
                } else {
                    Admission::Unstaged
                };
                to_stage.push((i, job(&c.unit, *snapshot_id, admission)));
            }
            How::Snapshot { .. } => {
                let outcome = crate::staging::snapshot_create_detailed(conn, &c.unit, config)?;
                match (outcome.minted, outcome.status.as_str()) {
                    (false, "staged") => results[i] = Some(Staged::Already),
                    (false, "current") => {
                        results[i] = Some(Staged::Nothing(format!(
                            "its folder matches v{}, already on tape: nothing to stage",
                            outcome.version
                        )))
                    }
                    _ => to_stage.push((i, job(&c.unit, outcome.snapshot_id, Admission::Unstaged))),
                }
            }
        }
    }
    if !to_stage.is_empty() {
        let stage_jobs: Vec<StageJob> = to_stage.iter().map(|(_, j)| j.clone()).collect();
        let outcomes = crate::staging::jobs::stage_many(
            conn,
            paths,
            config,
            &stage_jobs,
            jobs,
            assume_yes,
            &mut std::io::stderr(),
            &crate::progress::stderr_println,
        )?;
        for (unit, stage_set_id) in crate::staging::jobs::first_failure(outcomes)? {
            let version: i64 = conn.query_row(
                "SELECT sn.version FROM stage_sets ss JOIN snapshots sn ON sn.id = ss.snapshot_id
                  WHERE ss.id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )?;
            if let Some((i, _)) = to_stage.iter().find(|(_, j)| j.unit_name == unit) {
                results[*i] = Some(Staged::Now {
                    stage_set_id,
                    version,
                });
            }
        }
    }
    Ok(results
        .into_iter()
        .map(|r| r.unwrap_or_else(|| Staged::Nothing("not staged".to_string())))
        .collect())
}

fn bytes_text(bytes: u64) -> String {
    crate::util::format_bytes_binary(i64::try_from(bytes).unwrap_or(i64::MAX))
}

fn class_text(class: PlanClass) -> &'static str {
    match class {
        PlanClass::Owed => "owed",
        PlanClass::Whole => "whole",
        PlanClass::Spannable => "spannable",
    }
}

/// How a planned unit is or would be staged, in words: what `stage` did
/// when it ran (`staged`), or what it would do.
fn disposition(c: &Candidate, staged: Option<&Staged>) -> String {
    match (staged, &c.how) {
        (
            Some(Staged::Now {
                stage_set_id,
                version,
            }),
            _,
        ) => format!("staged now (v{version}, stage set {stage_set_id})"),
        (Some(Staged::Nothing(why)), _) => why.clone(),
        (_, How::AlreadyStaged { .. }) => "already staged".to_string(),
        (_, How::ReadSlices { .. }) => "needs volume read-slices (below)".to_string(),
        (_, How::Snapshot { reason }) => format!(
            "to snapshot and stage ({})",
            match reason {
                PendingReason::New => "never archived",
                PendingReason::Dirty => "changed since its last version",
            }
        ),
        (_, How::Stage { restage: false, .. }) => "to stage from its source".to_string(),
        (_, How::Stage { restage: true, .. }) => "to stage again from its source".to_string(),
    }
}

/// The JSON word for [`disposition`].
fn disposition_key(c: &Candidate, staged: Option<&Staged>) -> &'static str {
    match (staged, &c.how) {
        (Some(Staged::Now { .. }), _) => "staged_now",
        (Some(Staged::Nothing(_)), _) => "nothing_to_stage",
        (_, How::AlreadyStaged { .. }) => "already_staged",
        (_, How::ReadSlices { .. }) => "read_slices",
        (_, How::Snapshot { .. }) => "to_snapshot_and_stage",
        (_, How::Stage { .. }) => "to_stage",
    }
}

fn candidate_line(c: &Candidate, location: &str, tail: &str) -> String {
    let class = match c.class {
        PlanClass::Owed => " (its owed run, first on the tape)",
        PlanClass::Spannable => " (may span: last on the tape)",
        PlanClass::Whole => "",
    };
    format!(
        "  {}: {}; {}{class}{tail}",
        c.name(),
        c.describe_need(location),
        bytes_text(c.bytes)
    )
}

/// `stage for-location`'s text: the lines for stdout, then those for stderr
/// (the warnings). `staged` is [`stage`]'s answer, `None` for `--dry-run`.
pub fn text_lines(plan: &ForLocationPlan, staged: Option<&[Staged]>) -> (Vec<String>, Vec<String>) {
    let x = &plan.location;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let bound = match plan.room.bound {
        Some(b) => format!(", and on to its plan bound of {}", bytes_text(b)),
        None => String::new(),
    };
    out.push(format!(
        "stage for-location {x} --volume {}: {} unit(s), {}, against the volume's {} under its \
         fill ceiling{bound}{}",
        plan.volume,
        plan.planned.len(),
        bytes_text(plan.planned_bytes()),
        bytes_text(plan.room.fill),
        if staged.is_none() {
            " (dry run: nothing was snapshotted or staged)"
        } else {
            ""
        }
    ));
    if plan.planned.is_empty() {
        out.push(format!("  nothing this run can stage needs {x}"));
    }
    for (i, c) in plan.planned.iter().enumerate() {
        let s = staged.and_then(|s| s.get(i));
        out.push(candidate_line(c, x, &format!(" — {}", disposition(c, s))));
    }
    let reads = plan.read_slices();
    if !reads.is_empty() {
        out.push(
            "read these back first, in this order (each reads a tape; this command moved none):"
                .to_string(),
        );
        out.extend(reads.iter().map(|r| format!("  {}", r.command())));
    }
    if !plan.planned.is_empty() {
        out.push(format!(
            "then write them: tapectl volume write {} --for-location {x}",
            plan.volume
        ));
    }
    if !plan.waiting.is_empty() {
        out.push(format!(
            "waiting for the next tape for {x} ({} unit(s)): the units above fill this one",
            plan.waiting.len()
        ));
        out.extend(plan.waiting.iter().map(|c| candidate_line(c, x, "")));
    }
    if !plan.spans_waiting.is_empty() {
        out.push("spans waiting for their location (item 159):".to_string());
        out.extend(plan.spans_waiting.iter().map(|s| {
            format!(
                "  \"{}\" v{}: continues on a tape at {}",
                s.unit,
                s.version,
                s.describe_at()
            )
        }));
    }
    if !plan.skipped.is_empty() {
        out.push("skipped:".to_string());
        out.extend(
            plan.skipped
                .iter()
                .map(|s| format!("  \"{}\": {}", s.unit, s.reason)),
        );
    }
    if !plan.staged_overflow.is_empty() {
        err.push(format!(
            "warning: what is already staged for {x} does not all fit volume \"{}\" ({}): \
             nothing more was planned, and `tapectl volume write {} --for-location {x}` leaves \
             what does not fit staged for the next tape, or refuses",
            plan.volume,
            plan.staged_overflow.join(", "),
            plan.volume
        ));
    }
    err.extend(plan.warnings.iter().cloned());
    (out, err)
}

fn candidate_json(c: &Candidate, staged: Option<&Staged>) -> serde_json::Value {
    let (group, target, floor, copies_here, copies) = match c.need {
        NeedAt::Target {
            target,
            copies_here,
            copies,
        } => (Some(1), Some(target), None, Some(copies_here), Some(copies)),
        NeedAt::Floor { floor, copies } => (Some(2), None, Some(floor), Some(0), Some(copies)),
        NeedAt::None => (None, None, None, None, None),
    };
    let (stage_set_id, version) = match (staged, &c.how) {
        (
            Some(Staged::Now {
                stage_set_id,
                version,
            }),
            _,
        ) => (Some(*stage_set_id), Some(*version)),
        (_, How::AlreadyStaged { stage_set_id }) => (Some(*stage_set_id), c.version),
        _ => (None, c.version),
    };
    let read_slices: Vec<String> = match &c.how {
        How::ReadSlices { commands } => commands.iter().map(ReadSlicesCommand::command).collect(),
        _ => Vec::new(),
    };
    serde_json::json!({
        "unit": c.unit,
        "unit_status": c.unit_status,
        "version": version,
        "group": group,
        "short_by": c.need.short_by(),
        "target": target,
        "floor": floor,
        "copies_here": copies_here,
        "copies": copies,
        "continues_span": c.continues_span,
        "class": class_text(c.class),
        "bytes": c.bytes,
        "staging": disposition_key(c, staged),
        "stage_set_id": stage_set_id,
        "read_slices": read_slices,
    })
}

/// `stage for-location --json`: the plan's facts, one object.
pub fn to_json(plan: &ForLocationPlan, staged: Option<&[Staged]>) -> serde_json::Value {
    serde_json::json!({
        "location": plan.location,
        "volume": plan.volume,
        "dry_run": staged.is_none(),
        "room": {"fill_bytes": plan.room.fill, "bound_bytes": plan.room.bound},
        "planned_bytes": plan.planned_bytes(),
        "planned": plan.planned.iter().enumerate()
            .map(|(i, c)| candidate_json(c, staged.and_then(|s| s.get(i))))
            .collect::<Vec<_>>(),
        "read_slices": plan.read_slices().iter().map(|r| r.command()).collect::<Vec<_>>(),
        "next": (!plan.planned.is_empty()).then(|| format!(
            "tapectl volume write {} --for-location {}", plan.volume, plan.location
        )),
        "waiting": plan.waiting.iter().map(|c| candidate_json(c, None)).collect::<Vec<_>>(),
        "spans_waiting": plan.spans_waiting.iter().map(|s| serde_json::json!({
            "unit": s.unit, "version": s.version, "at": s.at,
        })).collect::<Vec<_>>(),
        "skipped": plan.skipped.iter().map(|s| serde_json::json!({
            "unit": s.unit, "reason": s.reason,
        })).collect::<Vec<_>>(),
        "staged_overflow": plan.staged_overflow,
        "warnings": plan.warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LtoBackendConfig;

    const MIB: u64 = 1024 * 1024;

    /// A catalog with `home` and `offsite` (shelves), `glacier` (a
    /// warehouse), one tenant, and the volume `L1`: initialized, 10 MiB, on
    /// a drive with no fill ceiling margin and no reserve, so its room is
    /// exactly 10 MiB.
    struct Cat {
        conn: Connection,
        config: Config,
        tenant: i64,
        tmp: tempfile::TempDir,
    }

    impl Cat {
        fn new() -> Self {
            let conn = crate::db::open_memory().unwrap();
            conn.execute_batch(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active');
                 INSERT INTO locations (name, kind) VALUES ('home', 'shelf');
                 INSERT INTO locations (name, kind) VALUES ('offsite', 'shelf');
                 INSERT INTO locations (name, kind) VALUES ('glacier', 'warehouse');",
            )
            .unwrap();
            let tenant = conn
                .query_row("SELECT id FROM tenants WHERE name = 't'", [], |r| r.get(0))
                .unwrap();
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
                 VALUES ('L1', 'lto', 'p', ?1, 'initialized')",
                params![10 * MIB as i64],
            )
            .unwrap();
            let mut config = Config::default();
            config.backends.lto.push(LtoBackendConfig {
                name: "p".into(),
                device_tape: "/dev/null".into(),
                device_sg: "/dev/null".into(),
                generation: "LTO-8".into(),
                capacity_override: Some("10M".into()),
                fill_ceiling: 1.0,
                enospc_buffer: "0".into(),
                post_warning_bytes: None,
            });
            Cat {
                conn,
                config,
                tenant,
                tmp: tempfile::tempdir().unwrap(),
            }
        }

        /// A unit `name` in `status`, its source a folder of `files` (name,
        /// bytes) when given.
        fn unit(&self, name: &str, status: &str, files: Option<&[(&str, usize)]>) -> i64 {
            let path = files.map(|files| {
                let dir = self.tmp.path().join("src").join(name);
                std::fs::create_dir_all(&dir).unwrap();
                for (f, bytes) in files {
                    std::fs::write(dir.join(f), vec![7u8; *bytes]).unwrap();
                }
                dir.to_string_lossy().into_owned()
            });
            self.conn
                .execute(
                    "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status,
                                        current_path)
                     VALUES (?1, ?1, ?2, 'mtime_size', 1, ?3, ?4)",
                    params![name, self.tenant, status, path],
                )
                .unwrap();
            self.conn.last_insert_rowid()
        }

        /// `unit`'s dotfile layer (frozen, as with its source gone).
        fn targets(&self, unit: i64, json: &str) {
            self.conn
                .execute(
                    "UPDATE units SET copies_at_frozen = ?2 WHERE id = ?1",
                    params![unit, json],
                )
                .unwrap();
        }

        fn version(&self, unit: i64, version: i64, status: &str) -> i64 {
            self.conn
                .execute(
                    "INSERT INTO snapshots (unit_id, version, status, source_path, file_count,
                                            total_size)
                     VALUES (?1, ?2, ?3, '/src', 1, 10)",
                    params![unit, version, status],
                )
                .unwrap();
            self.conn.last_insert_rowid()
        }

        /// A stage set of `snapshot` in `status` with one slice per size,
        /// each on disk (`staging_path` set) when `live`.
        fn set(&self, snapshot: i64, sizes: &[u64], status: &str, live: bool) -> i64 {
            self.conn
                .execute(
                    "INSERT INTO stage_sets (snapshot_id, status, slice_size, num_slices)
                     VALUES (?1, ?2, 1048576, ?3)",
                    params![snapshot, status, sizes.len() as i64],
                )
                .unwrap();
            let set = self.conn.last_insert_rowid();
            for (i, bytes) in sizes.iter().enumerate() {
                let path = live.then(|| format!("/staging/{set}-{i}"));
                self.conn
                    .execute(
                        "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes,
                                                   encrypted_bytes, sha256_plain,
                                                   sha256_encrypted, staging_path)
                         VALUES (?1, ?2, ?3, ?3, 'p', ?4, ?5)",
                        params![
                            set,
                            i as i64 + 1,
                            *bytes as i64,
                            format!("e-{set}-{i}"),
                            path
                        ],
                    )
                    .unwrap();
            }
            set
        }

        /// A sealed volume `label` at `location`.
        fn tape(&self, label: &str, location: Option<&str>) -> i64 {
            self.conn
                .execute(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                          capacity_bytes, status, location_id, sealed_at)
                     VALUES (?1, 'lto', 'p', 'LTO-8', 10485760, 'sealed',
                             (SELECT id FROM locations WHERE name = ?2), datetime('now'))",
                    params![label, location],
                )
                .unwrap();
            self.conn.last_insert_rowid()
        }

        /// A `status` write of `set` to `volume`, slices `first..=last`
        /// written at positions from 8.
        fn write(&self, set: i64, volume: i64, status: &str, first: i64, last: i64) {
            self.conn
                .execute(
                    "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, may_span)
                     SELECT id, snapshot_id, ?2, ?3, ?4 FROM stage_sets WHERE id = ?1",
                    params![set, volume, status, i64::from(status == "range")],
                )
                .unwrap();
            let write = self.conn.last_insert_rowid();
            for (i, n) in (first..=last).enumerate() {
                self.conn
                    .execute(
                        "INSERT INTO write_positions (write_id, stage_slice_id, position, status)
                         SELECT ?1, id, ?3, 'written' FROM stage_slices
                          WHERE stage_set_id = ?2 AND slice_number = ?4",
                        params![write, set, (8 + i).to_string(), n],
                    )
                    .unwrap();
            }
        }

        /// A whole copy of `set` (`slices` slices) on `volume`.
        fn whole(&self, set: i64, volume: i64, slices: i64) {
            self.write(set, volume, "completed", 1, slices);
        }

        fn plan(&self, location: &str) -> Result<ForLocationPlan> {
            plan(&self.conn, &self.config, location, "L1", "/dev/null")
        }

        fn count(&self, table: &str) -> i64 {
            self.conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap()
        }
    }

    fn names(cs: &[Candidate]) -> Vec<String> {
        cs.iter().map(Candidate::name).collect()
    }

    /// Item 157: a warehouse is refused, by `stage for-location` and by
    /// `volume write --for-location` alike, naming `volume deposit add` and
    /// the sealed tapes holding a current version of each unit whose target
    /// there is unmet; a tape already deposited there is not named.
    #[test]
    fn a_warehouse_is_refused_naming_deposits_and_the_tapes_that_would_meet_it() {
        let c = Cat::new();
        let photos = c.unit("photos", "tape_only", None);
        c.targets(photos, r#"{"glacier":1}"#);
        let v1 = c.version(photos, 1, "current");
        let set = c.set(v1, &[MIB], "cleaned", false);
        let t1 = c.tape("T1", Some("home"));
        c.whole(set, t1, 1);
        let t2 = c.tape("T2", Some("offsite"));
        c.whole(set, t2, 1);
        c.conn
            .execute(
                "INSERT INTO volume_deposits (volume_id, location_id, deposited_at)
                 VALUES (?1, (SELECT id FROM locations WHERE name = 'glacier'), datetime('now'))",
                params![t2],
            )
            .unwrap();
        // The deposit of T2 meets a target of 1; raise it so T1 is wanted.
        c.targets(photos, r#"{"glacier":2}"#);

        let err = c.plan("glacier").unwrap_err().to_string();
        assert!(err.contains("is a warehouse"), "{err}");
        assert!(
            err.contains("tapectl volume deposit add <LABEL> --to glacier"),
            "{err}"
        );
        assert!(err.contains("T1 (photos v1)"), "{err}");
        assert!(!err.contains("T2"), "T2 is deposited there already: {err}");
        let err = staged_sets_for_location(&c.conn, &c.config, "glacier")
            .unwrap_err()
            .to_string();
        assert!(err.contains("volume deposit add"), "{err}");
        // A shelf is not refused, and an unknown name lists the known ones.
        assert_eq!(
            check_location(&c.conn, &c.config, " home ").unwrap(),
            "home"
        );
        let err = check_location(&c.conn, &c.config, "attic")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("known locations: glacier, home, offsite"),
            "{err}"
        );
    }

    /// Item 152's order over item 158's groups: group 1 by how far short
    /// at X, the fewest copies in all breaking ties, then group 2 by the
    /// fewest copies, each then by name. A unit whose target at X is met,
    /// or with no target and a copy at X already, is not for X. The staged
    /// sets a write for X takes are exactly those the plan rides.
    #[test]
    fn groups_and_priority_order() {
        let c = Cat::new();
        let home = c.tape("H1", Some("home"));
        let off = c.tape("O1", Some("offsite"));
        // (name, targets, copies at home, copies at offsite)
        let units: &[(&str, Option<&str>, usize, usize)] = &[
            ("short2", Some(r#"{"offsite":2}"#), 1, 0),
            ("short1-one-copy", Some(r#"{"offsite":1}"#), 1, 0),
            ("short1-no-copy", Some(r#"{"offsite":1}"#), 0, 0),
            ("floor-one-copy", None, 1, 0),
            ("floor-no-copy", None, 0, 0),
            ("target-met", Some(r#"{"offsite":1}"#), 1, 1),
            ("has-a-copy-there", None, 0, 1),
        ];
        for (name, targets, at_home, at_off) in units {
            let u = c.unit(name, "active", None);
            if let Some(json) = targets {
                c.targets(u, json);
            }
            let v = c.version(u, 1, "current");
            let set = c.set(v, &[MIB / 2], "staged", true);
            if *at_home > 0 {
                c.whole(set, home, 1);
            }
            if *at_off > 0 {
                c.whole(set, off, 1);
            }
        }
        let plan = c.plan("offsite").unwrap();
        assert_eq!(
            names(&plan.planned),
            [
                "\"short2\" v1",
                "\"short1-no-copy\" v1",
                "\"short1-one-copy\" v1",
                "\"floor-no-copy\" v1",
                "\"floor-one-copy\" v1",
            ]
        );
        assert!(plan
            .planned
            .iter()
            .all(|p| matches!(p.how, How::AlreadyStaged { .. })));
        assert_eq!(plan.planned[0].need.group(), Some(1));
        assert_eq!(plan.planned[0].need.short_by(), 2);
        assert_eq!(plan.planned[3].need.group(), Some(2));
        assert!(plan.waiting.is_empty());

        let staged = staged_sets_for_location(&c.conn, &c.config, "offsite").unwrap();
        let mut for_x: Vec<String> = staged.for_location.iter().map(|s| s.unit.clone()).collect();
        for_x.sort();
        let mut planned: Vec<String> = plan.planned.iter().map(|p| p.unit.clone()).collect();
        planned.sort();
        assert_eq!(for_x, planned, "the write takes what the plan rides");
        let not: Vec<(&str, &str)> = staged
            .not_for_location
            .iter()
            .map(|n| (n.set.unit.as_str(), n.why.as_str()))
            .collect();
        assert_eq!(
            not,
            [
                ("has-a-copy-there", "no need at offsite"),
                ("target-met", "no need at offsite"),
            ]
        );
    }

    /// Item 159: a staged set whose owed run continues a range (the run
    /// starts after slice 1) is for X only when the tape holding that range
    /// sits at X; elsewhere it waits, named. An owed run starting at slice 1
    /// continues no range and is judged by its need like any version.
    #[test]
    fn a_span_continues_only_at_its_location() {
        let mut c = Cat::new();
        c.config.defaults.allow_spanning = true;
        c.config.defaults.min_copies = 1;
        let u = c.unit("span", "active", None);
        let v = c.version(u, 1, "staged");
        let set = c.set(v, &[MIB, MIB, MIB, MIB], "staged", true);
        let r1 = c.tape("R1", Some("home"));
        c.write(set, r1, "range", 1, 2);

        // Slices 3-4 are owed: the run continues R1's range, at home.
        let at_home = c.plan("home").unwrap();
        assert_eq!(names(&at_home.planned), ["\"span\" v1"]);
        let p = &at_home.planned[0];
        assert!(p.continues_span);
        assert_eq!(p.class, PlanClass::Owed);
        assert_eq!(p.bytes, 2 * MIB, "the owed run, slices 3 and 4");
        let at_off = c.plan("offsite").unwrap();
        assert!(at_off.planned.is_empty());
        assert_eq!(
            at_off.spans_waiting,
            [SpanWaiting {
                unit: "span".into(),
                version: 1,
                at: vec![Some("home".into())]
            }]
        );
        let staged = staged_sets_for_location(&c.conn, &c.config, "offsite").unwrap();
        assert!(staged.for_location.is_empty());
        assert_eq!(
            staged.not_for_location[0].why,
            "its span continues on a tape at home (item 159)"
        );
        assert_eq!(
            staged_sets_for_location(&c.conn, &c.config, "home")
                .unwrap()
                .stage_set_ids(),
            [set]
        );

        // At a copy floor of 2 every slice is owed: the run starts at slice 1
        // and begins a copy of its own, so it goes where it has need.
        c.config.defaults.min_copies = 2;
        let at_off = c.plan("offsite").unwrap();
        assert_eq!(names(&at_off.planned), ["\"span\" v1"]);
        assert!(!at_off.planned[0].continues_span);
        assert_eq!(at_off.planned[0].class, PlanClass::Owed);
        assert_eq!(at_off.planned[0].need.group(), Some(2));
        assert!(at_off.spans_waiting.is_empty());
    }

    /// Packing to the volume's room (item 151): whole units first in priority
    /// order (here all group 2 at 0 copies, so by name), then the units that
    /// may span (items 83 and 95), next-fit: the first that does not fit
    /// closes the tape, so a smaller one after it waits too (item 69), and
    /// every waiting unit is named with its need.
    #[test]
    fn packing_fills_the_volume_with_units_that_may_span_last_and_names_the_waiting() {
        let c = Cat::new();
        let mib = MIB as usize;
        c.unit("a", "active", Some(&[("f", 3 * mib)]));
        c.unit("b", "active", Some(&[("f", 4 * mib)]));
        c.unit("c", "active", Some(&[("f", 5 * mib)]));
        c.unit("d", "active", Some(&[("f", mib)]));
        let span = c.unit("aa-span", "active", Some(&[("f", mib)]));
        let dir: String = c
            .conn
            .query_row(
                "SELECT current_path FROM units WHERE id = ?1",
                params![span],
                |r| r.get(0),
            )
            .unwrap();
        std::fs::write(
            Path::new(&dir).join(".tapectl-unit.toml"),
            "[unit]\nuuid = \"aa-span\"\nname = \"aa-span\"\ncreated = \"2026-10-25\"\n\
             tenant = \"t\"\n\n[policy]\nallow_spanning = true\n",
        )
        .unwrap();

        let plan = c.plan("offsite").unwrap();
        assert_eq!(
            names(&plan.planned),
            ["\"a\" (new version)", "\"b\" (new version)"]
        );
        assert_eq!(plan.planned_bytes(), 7 * MIB);
        assert_eq!(
            names(&plan.waiting),
            [
                "\"c\" (new version)",
                "\"d\" (new version)",
                "\"aa-span\" (new version)"
            ],
            "c closes the tape; d would fit but waits; the unit that may span is last"
        );
        assert_eq!(plan.waiting[2].class, PlanClass::Spannable);
        assert!(plan.waiting.iter().all(
            |w| w.describe_need("offsite") == "group 2: 0 of its copy floor 2, none at offsite"
        ));

        // With room for all but c, the unit that may span is planned last.
        std::fs::remove_dir_all(c.tmp.path().join("src").join("c")).unwrap();
        let plan = c.plan("offsite").unwrap();
        assert_eq!(
            names(&plan.planned),
            [
                "\"a\" (new version)",
                "\"b\" (new version)",
                "\"d\" (new version)",
                "\"aa-span\" (new version)"
            ]
        );
        assert_eq!(
            plan.skipped,
            [Skipped {
                unit: "c".into(),
                reason: format!(
                    "its source folder {} is not there, so it cannot be staged (item 153)",
                    c.tmp.path().join("src").join("c").display()
                ),
            }],
            "a unit whose source is unavailable is named and skipped"
        );
    }

    /// Item 153, `--dry-run`: a new unit and a changed one are planned as new
    /// versions at 0 copies everywhere, and the plan mints no snapshot and
    /// stages nothing; a changed unit's old version, fully covered, does not
    /// hide its new one.
    #[test]
    fn new_and_changed_units_are_planned_as_new_versions_and_a_plan_changes_nothing() {
        let c = Cat::new();
        let mib = MIB as usize;
        c.unit("fresh", "active", Some(&[("f", mib)]));
        let changed = c.unit("changed", "active", Some(&[("f", mib)]));
        crate::staging::snapshot_create(&c.conn, "changed", &c.config).unwrap();
        let v1: i64 = c
            .conn
            .query_row(
                "SELECT id FROM snapshots WHERE unit_id = ?1",
                params![changed],
                |r| r.get(0),
            )
            .unwrap();
        c.conn
            .execute(
                "UPDATE snapshots SET status = 'current' WHERE id = ?1",
                params![v1],
            )
            .unwrap();
        let set = c.set(v1, &[MIB], "cleaned", false);
        for (label, place) in [("H1", "home"), ("H2", "home"), ("O1", "offsite")] {
            let t = c.tape(label, Some(place));
            c.whole(set, t, 1);
        }
        let unchanged = c.plan("offsite").unwrap();
        assert_eq!(
            names(&unchanged.planned),
            ["\"fresh\" (new version)"],
            "the changed unit's v1 has its copies, one at offsite"
        );

        let dir: String = c
            .conn
            .query_row(
                "SELECT current_path FROM units WHERE id = ?1",
                params![changed],
                |r| r.get(0),
            )
            .unwrap();
        std::fs::write(Path::new(&dir).join("g"), b"more").unwrap();
        let (snapshots, sets) = (c.count("snapshots"), c.count("stage_sets"));
        let plan = c.plan("offsite").unwrap();
        assert_eq!(
            names(&plan.planned),
            ["\"changed\" (new version)", "\"fresh\" (new version)"]
        );
        assert_eq!(
            plan.planned[0].how,
            How::Snapshot {
                reason: PendingReason::Dirty
            }
        );
        assert_eq!(
            plan.planned[1].how,
            How::Snapshot {
                reason: PendingReason::New
            }
        );
        assert_eq!(
            (c.count("snapshots"), c.count("stage_sets")),
            (snapshots, sets),
            "a plan mints no snapshot and stages nothing"
        );
        let (out, _) = text_lines(&plan, None);
        assert!(
            out[0].ends_with("(dry run: nothing was snapshotted or staged)"),
            "{out:?}"
        );
    }

    /// Item 153: a tape-only unit is read back with `volume read-slices`,
    /// one command per holder tape, grouped by tape in the order the plan
    /// needs them; nothing is read or staged by the plan, and `stage` leaves
    /// it to the operator. A version held whole on no live tape is skipped.
    #[test]
    fn tape_only_units_get_read_slices_commands_and_no_tape_is_touched() {
        let c = Cat::new();
        let home = c.tape("H1", Some("home"));
        let h2 = c.tape("H2", Some("home"));
        let whole = c.unit("whole", "tape_only", None);
        c.targets(whole, r#"{"offsite":1}"#);
        let v = c.version(whole, 2, "current");
        let set = c.set(v, &[MIB, MIB], "cleaned", false);
        c.whole(set, home, 2);
        let split = c.unit("split", "tape_only", None);
        c.targets(split, r#"{"offsite":1}"#);
        let v = c.version(split, 1, "current");
        let set = c.set(v, &[MIB, MIB, MIB], "cleaned", false);
        c.write(set, h2, "range", 1, 2);
        c.write(set, home, "range", 3, 3);
        let lost = c.unit("lost", "tape_only", None);
        c.targets(lost, r#"{"offsite":1}"#);
        let v = c.version(lost, 1, "current");
        let set = c.set(v, &[MIB, MIB], "cleaned", false);
        c.write(set, home, "range", 1, 1);

        let plan = c.plan("offsite").unwrap();
        assert_eq!(names(&plan.planned), ["\"split\" v1", "\"whole\" v2"]);
        assert_eq!(plan.planned[1].bytes, 2 * MIB);
        let commands: Vec<String> = plan.read_slices().iter().map(|r| r.command()).collect();
        assert_eq!(
            commands,
            [
                "tapectl volume read-slices --from H2 --unit split --version 1",
                "tapectl volume read-slices --from H1 --unit split --version 1",
                "tapectl volume read-slices --from H1 --unit whole --version 2",
            ]
        );
        assert_eq!(
            plan.skipped,
            [Skipped {
                unit: "lost".into(),
                reason: "no live volume holds every slice of v1, so it cannot be read back".into(),
            }]
        );
        let paths = TapectlPaths::new(c.tmp.path().join("home"));
        let staged = stage(&c.conn, &paths, &c.config, &plan, 1, true).unwrap();
        assert_eq!(staged, [Staged::ReadSlices, Staged::ReadSlices]);
        let (out, _) = text_lines(&plan, Some(&staged));
        assert!(
            out.iter()
                .any(|l| l == "  tapectl volume read-slices --from H2 --unit split --version 1"),
            "{out:?}"
        );
        assert!(out
            .iter()
            .any(|l| l == "then write them: tapectl volume write L1 --for-location offsite"));
    }

    /// A version already staged is reused, never staged again: `stage`
    /// stages nothing for it and makes no stage set.
    #[test]
    fn an_already_staged_set_is_reused() {
        let c = Cat::new();
        let u = c.unit("kept", "active", None);
        let v = c.version(u, 1, "staged");
        let set = c.set(v, &[MIB], "staged", true);
        let plan = c.plan("offsite").unwrap();
        assert_eq!(
            plan.planned[0].how,
            How::AlreadyStaged { stage_set_id: set }
        );
        let sets = c.count("stage_sets");
        let paths = TapectlPaths::new(c.tmp.path().join("home"));
        let staged = stage(&c.conn, &paths, &c.config, &plan, 1, true).unwrap();
        assert_eq!(staged, [Staged::Already]);
        assert_eq!(c.count("stage_sets"), sets);
        let json = to_json(&plan, Some(&staged));
        assert_eq!(json["planned"][0]["staging"], "already_staged");
        assert_eq!(json["planned"][0]["stage_set_id"], set);
        assert_eq!(json["dry_run"], false);
        assert_eq!(
            json["next"],
            "tapectl volume write L1 --for-location offsite"
        );
    }

    /// The volume must be a write target, as `volume write` requires.
    #[test]
    fn a_volume_that_is_not_a_write_target_is_refused() {
        let c = Cat::new();
        c.conn
            .execute(
                "UPDATE volumes SET status = 'sealed' WHERE label = 'L1'",
                [],
            )
            .unwrap();
        let err = c.plan("offsite").unwrap_err();
        assert!(
            matches!(err, TapectlError::VolumeNotWriteTarget { .. }),
            "{err}"
        );
    }

    /// Item 153, end to end with dar: `stage` snapshots a new unit and a
    /// changed one and stages each, and the round trip holds: what it staged
    /// for X is exactly what `volume write --for-location X` takes, the
    /// changed unit's new version included although its old version has
    /// every copy its floor asks for.
    #[test]
    fn new_and_changed_units_are_snapshotted_and_staged_and_a_write_for_x_takes_them() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().join("home"));
        paths.ensure_dirs().unwrap();
        let conn = crate::db::open(&paths.db_file).unwrap();
        let staging = tmp.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let mut config = Cat::new().config;
        config.dar.binary = "dar".into();
        config.staging.directory = staging.to_string_lossy().into_owned();
        config.defaults.slice_size = "1M".into();
        crate::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('escrow-holder', 0, 'active')",
            [],
        )
        .unwrap();
        let holder = conn.last_insert_rowid();
        let kp = crate::crypto::keys::generate_keypair();
        crate::db::queries::insert_escrow_key(
            &conn,
            holder,
            "test-escrow",
            &kp.fingerprint,
            &kp.public_key,
            None,
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO locations (name, kind) VALUES ('home', 'shelf');
             INSERT INTO locations (name, kind) VALUES ('offsite', 'shelf');
             INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
             VALUES ('L1', 'lto', 'p', 10485760, 'initialized');",
        )
        .unwrap();
        for name in ["fresh", "changed"] {
            let src = tmp.path().join("src").join(name);
            std::fs::create_dir_all(&src).unwrap();
            std::fs::write(src.join("f.bin"), vec![3u8; 4000]).unwrap();
            crate::unit::init_unit(
                &conn,
                &paths,
                src.to_str().unwrap(),
                "alice",
                Some(name),
                &[],
                None,
            )
            .unwrap();
        }
        let plan_x = || plan(&conn, &config, "offsite", "L1", "/dev/null").unwrap();

        // Both never archived: both snapshotted and staged.
        let first = plan_x();
        assert_eq!(
            names(&first.planned),
            ["\"changed\" (new version)", "\"fresh\" (new version)"]
        );
        let staged = stage(&conn, &paths, &config, &first, 1, true).unwrap();
        assert!(
            staged
                .iter()
                .all(|s| matches!(s, Staged::Now { version: 1, .. })),
            "{staged:?}"
        );

        // `changed` v1 goes onto two tapes (its floor) and its staging is
        // released; then its folder changes.
        let (v1, set1): (i64, i64) = conn
            .query_row(
                "SELECT sn.id, ss.id FROM snapshots sn
                   JOIN units u ON u.id = sn.unit_id
                   JOIN stage_sets ss ON ss.snapshot_id = sn.id
                  WHERE u.name = 'changed'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        conn.execute(
            "UPDATE snapshots SET status = 'current' WHERE id = ?1",
            params![v1],
        )
        .unwrap();
        for label in ["H1", "H2"] {
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status,
                                      location_id)
                 VALUES (?1, 'lto', 'p', 10485760, 'sealed',
                         (SELECT id FROM locations WHERE name = 'home'))",
                params![label],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, (SELECT id FROM volumes WHERE label = ?3), 'completed')",
                params![set1, v1, label],
            )
            .unwrap();
        }
        conn.execute(
            "UPDATE stage_sets SET status = 'cleaned' WHERE id = ?1",
            params![set1],
        )
        .unwrap();
        conn.execute(
            "UPDATE stage_slices SET staging_path = NULL WHERE stage_set_id = ?1",
            params![set1],
        )
        .unwrap();
        std::fs::write(tmp.path().join("src/changed/g.bin"), b"new content").unwrap();

        let second = plan_x();
        assert_eq!(
            names(&second.planned),
            ["\"fresh\" v1", "\"changed\" (new version)"],
            "the staged set rides first; the changed unit is a new version"
        );
        let staged = stage(&conn, &paths, &config, &second, 1, true).unwrap();
        assert_eq!(staged[0], Staged::Already);
        let Staged::Now {
            stage_set_id: set2,
            version: 2,
        } = staged[1]
        else {
            panic!("the changed unit's v2 is staged: {staged:?}");
        };
        let for_x = staged_sets_for_location(&conn, &config, "offsite").unwrap();
        let fresh_set = match &second.planned[0].how {
            How::AlreadyStaged { stage_set_id } => *stage_set_id,
            other => panic!("{other:?}"),
        };
        let mut want = vec![fresh_set, set2];
        want.sort_unstable();
        assert_eq!(for_x.stage_set_ids(), want);
    }
}
