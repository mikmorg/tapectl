//! Releasing staged data once the copies written satisfy policy — the ONE
//! place that decision lives (issue #440).
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
//! The rule is all-or-nothing over the units given, as `collection run` has
//! always had it: stage once, write N times (`v2-open-questions.md` §11)
//! means the next copy must find the same staged content this one wrote, so
//! one short unit keeps every unit's staging. `staging clean` is the
//! per-unit tool — it releases the covered units and retains the short ones.
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
//! for tape 3, nor for another unit's next copy. The all-or-nothing rule
//! above is unchanged for the sets with no `range` row; a unit whose staged
//! data is only sets holding a range is left out of it
//! ([`release_if_covered`] says why).

use rusqlite::{params, Connection, OptionalExtension};

use crate::config::Config;
use crate::error::{Result, TapectlError};
use crate::policy::coverage::{copy_count_expr, copy_floor, tape_only_note, CoverageQuery};
use crate::staging::clean::{
    clean_staging, release_blocker, release_covered_slices, slices_short_of,
    staged_sets_holding_a_range, CleanReport, CleanScope, ReleaseBlocker,
};

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
    /// `Some` when staging was released (every unit the whole-set rule
    /// judges now meets its own resolved `min_copies`); `None` when it was
    /// deliberately retained. Only the stage sets with no `range` row: a set
    /// holding one is released slice by slice, in `slices`.
    pub cleaned: Option<CleanReport>,
    /// Non-empty exactly when `cleaned` is `None`: which units are still
    /// short, and by how much.
    pub under_copied: Vec<CopyProgress>,
    /// The per-slice release of the stage sets that hold a range (1.7.0,
    /// issue #478; ADR-0012 item 129), made whatever `cleaned` says (ruling
    /// X15): the slices released (`slices_released`, their files counted in
    /// `files_removed`/`bytes_freed`, a failed unlink in `stranded`), a set
    /// whose last slice went (`sets_cleaned`), and the slices still short of
    /// copies (`slices_short`). Its counts are never also in `cleaned`.
    pub slices: CleanReport,
}

/// Releases the staged data of `unit_ids` only when every one of those units
/// now meets its own resolved `min_copies`; otherwise retains it all and
/// says which units are short.
///
/// The release itself is `clean_staging(force = false, CleanScope::Units)`,
/// so its own guard still applies and is what keeps a set that has no
/// completed write — or any write that is neither `completed` nor `cut` —
/// staged. This
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
///
/// **Sets that hold a range** (1.7.0, issue #478; ADR-0012 items 85 and
/// 129; ruling X15). `clean_staging` never releases one without `force`: its
/// slices are on different volumes. Each such `'staged'` set of `unit_ids`
/// is released slice by slice ([`release_ranges`]), against its own unit's
/// copy floor and whatever the gate below decides, into
/// [`ReleaseOutcome::slices`]. A unit whose staged data here is only such
/// sets is left out of the gate: its version is not `current` until every
/// slice is on a sealed volume (item 128), so the unit reads 0 copies for
/// the whole span, and judging it would keep the staging of every whole
/// unit written beside it until the span ended. Every other unit is judged
/// all or nothing as before, so for a write with no range this is 1.6.0's
/// release exactly.
pub fn release_if_covered(
    conn: &Connection,
    config: &Config,
    unit_ids: &[i64],
) -> Result<ReleaseOutcome> {
    let gate_ids = units_judged_whole(conn, unit_ids)?;
    let under_copied = under_copied_units(conn, config, &gate_ids)?;
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
    let slices = release_ranges(conn, config, CleanScope::Units(unit_ids))?;
    Ok(ReleaseOutcome {
        cleaned,
        under_copied,
        slices,
    })
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
/// run`) and `staging clean`.
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

    /// Two units on one write, one short: the staging of NEITHER is released
    /// and only the short one is named — stage once, write N times, so the
    /// next copy finds the same content this one wrote.
    #[test]
    fn a_short_unit_keeps_the_staging_of_every_unit_judged_with_it() {
        let conn = db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), 2);
        let t = tenant(&conn);
        let alpha = unit(&conn, t, "testlib/alpha");
        let beta = unit(&conn, t, "testlib/beta");
        let a = staged_version(&conn, alpha, 1, "current", dir.path(), 2);
        let b = staged_version(&conn, beta, 1, "current", dir.path(), 1);

        let out = release_if_covered(&conn, &config, &[alpha, beta]).unwrap();

        assert!(out.cleaned.is_none());
        let named: Vec<_> = out.under_copied.iter().map(|p| p.describe()).collect();
        assert_eq!(named, ["unit testlib/beta has 1 copy, needs 2"]);
        assert!(
            a.file.exists() && b.file.exists(),
            "the covered unit's staging waits for the short one"
        );
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

    #[test]
    fn the_reason_is_plural_for_zero_copies() {
        let p = CopyProgress {
            unit_name: "u".into(),
            copies: 0,
            min_copies: 2,
            floor_note: String::new(),
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

    /// `collection run` judges a batch together (all or nothing). A batch
    /// whose write sealed short holds a unit with no copy at all -- its only
    /// version was cut, so it has no `current` snapshot and reads 0, never
    /// NULL -- and so the staging of every unit in the batch is kept, the
    /// cut one named. `volume write` judges only the units it completed on
    /// the volume (`unit_ids_written_to`), which leaves the cut one out.
    #[test]
    fn a_cut_unit_judged_with_a_written_one_keeps_the_staging_of_both() {
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

        assert!(out.cleaned.is_none());
        assert_eq!(
            out.under_copied
                .iter()
                .map(|p| p.describe())
                .collect::<Vec<_>>(),
            ["unit testlib/beta has 0 copies, needs 1"]
        );
        assert!(a.file.exists() && b.file.exists());

        assert_eq!(
            unit_ids_written_to(&conn, "V-SHORT").unwrap(),
            [alpha],
            "the cut unit is not one the short-sealed volume was written for"
        );
        let out = release_if_covered(&conn, &config, &[alpha]).unwrap();
        assert_eq!(out.cleaned.expect("alpha is covered").sets_cleaned, 1);
        assert!(!a.file.exists());
        assert!(b.file.exists(), "the cut unit's staging stays");
        assert_eq!(stage_set_status(&conn, b.stage_set_id), "staged");
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
    /// whole beside the span, is short (tape-only at 3x), so the
    /// all-or-nothing gate keeps beta's staging; alpha's covered slices go
    /// anyway.
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
        assert!(out.cleaned.is_none(), "beta keeps the whole-set staging");
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
}
