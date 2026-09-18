//! Whether a written stage set can still be recovered by the CURRENT escrow
//! recipient (ADR-0005).
//!
//! One predicate, one place. `audit`'s `escrow_coverage` check (#125) decides
//! this from `stage_sets.key_fingerprints`, and `catalog locate`,
//! `report copies` and the `volume write` pre-flight show the same fact —
//! sites that must never disagree about whether a tape is escrow-recoverable.
//! The codebase has been bitten by the alternative: five inlined copies of a
//! status filter are how #96 happened, and six hand-written location counts
//! are what #73 had to collapse into `policy::coverage`.
//!
//! Reads only the recorded recipient list and the row's origin. No tape, no
//! key material.
//!
//! # Three answers, not two (#137)
//!
//! A `NULL` recipient list means one of two different things, and the
//! difference matters to the operator reading the finding:
//!
//! * the row was **staged here** and the list was never written down — a
//!   pre-escrow or corrupt row. Fail closed: that is a **gap**.
//! * the row was **rebuilt from a tape** (`catalog rebuild`, #136) and the
//!   tape carried no recipient list to copy. tapectl could not have written
//!   it down. That is **unknown** — still not covered, still fail-closed for
//!   every gate, but reported in words that say what to do about it rather
//!   than accusing the row of something nobody could have prevented.
//!
//! `stage_sets.origin` (migration 010) is the discriminant.
//!
//! # The query half (coordinator decision 2026-09-11, architecture review C1)
//!
//! `classify`/`gap`/`marker` answer the deep question once fed a recipient
//! list. The question that FEEDS them — "which stage sets, on which
//! volumes, with what recorded recipient list and origin" — used to be
//! hand-written SQL in four places (`audit`'s `escrow_coverage` check,
//! `report copies`' `escrow_gaps_by_unit`, `catalog locate`'s
//! `locate_rows`, and the `volume write` pre-flight's
//! `stage_sets_lacking_escrow`), and the copies had already drifted:
//! different reason strings for the same verdict. [`stage_set_coverage`] is
//! the single query behind all four now, mirroring how issue #73 collapsed
//! six hand-written copy/location counts into `policy::coverage`.
//!
//! **Volume filter: [`crate::policy::coverage::in_service`], not
//! [`crate::policy::coverage::eligible`] and not an ad-hoc status list.**
//! Both alternatives were wrong in different directions:
//! - `eligible` (`sealed`-only) is the ADR-0004 durability question ("does
//!   this volume contribute a COPY"). Escrow asks a different one: "does the
//!   CURRENT escrow key open the bytes this volume holds". An `active`/`full`
//!   volume mid-write already holds sealed slices from earlier stage sets on
//!   the same tape and has not yet been sealed itself — `eligible` would
//!   hide those from the escrow check for no reason connected to escrow.
//! - The ad-hoc list (`NOT IN ('retired','erased','missing','blank')`) let
//!   `quarantined` volumes through — exactly the #96 failure mode in a new
//!   dimension: a status added to the schema after the list was written
//!   defaults to being INCLUDED rather than reviewed.
//!
//! `in_service` is "holds bytes we account for" — the right question for
//! escrow, which is about bytes that physically exist somewhere tapectl
//! still considers live inventory.
//!
//! `encrypted = 0` is folded in as its own [`Coverage::Gap`] here (never in
//! [`classify`], whose signature stays fixed): `stage create` has hard-coded
//! `encrypted = 1` on every INSERT since #115 made escrow registration a
//! staging precondition, so a `0` today can only be a legacy or corrupt row
//! — fail-closed reporting is correct, not a new false alarm.

use rusqlite::OptionalExtension;

/// Where a `stage_sets` row came from — the `origin` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `stage create` on this machine. Every row before migration 010.
    Staged,
    /// `catalog rebuild` from a sealed volume's envelope (#136).
    Rebuilt,
}

impl Origin {
    /// Parse the column. Anything other than the literal `rebuilt` is
    /// `Staged`: the column is `NOT NULL` with a `CHECK`, so this is only
    /// ever asked of the two values, and the conservative reading of a
    /// surprise is the one that fails closed.
    pub fn parse(column: &str) -> Origin {
        if column == "rebuilt" {
            Origin::Rebuilt
        } else {
            Origin::Staged
        }
    }
}

/// Can the current escrow recipient recover this stage set?
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Coverage {
    /// The recorded list names the current escrow key.
    Covered,
    /// Rebuilt from a tape that carries no recipient list. Not covered —
    /// every gate treats this exactly like [`Coverage::Gap`] — but the
    /// remedy is different and the wording says so.
    Unknown,
    /// Not covered, with the reason.
    Gap(String),
}

/// The words for [`Coverage::Unknown`], in one place so `audit`, the write
/// pre-flight and `report copies` say the same thing.
pub const UNKNOWN_REASON: &str = "coverage unknown — rebuilt from a tape that carries no \
recipient list; attest it with `catalog rebuild --key <escrow key>`, or re-stage";

/// Classify one stage set.
///
/// **Fails closed.** An absent or unparseable recipient list is never
/// presumed covered: a stage set that recorded no list cannot be *shown* to
/// be escrow-recoverable, and quietly treating "unknown" as "fine" is how an
/// archive reports itself clean while being unrecoverable. `Unknown` is a
/// different *explanation*, not a different *verdict*.
pub fn classify(fingerprints: Option<&str>, origin: Origin, escrow: &str) -> Coverage {
    match fingerprints {
        None => match origin {
            Origin::Rebuilt => Coverage::Unknown,
            Origin::Staged => Coverage::Gap("no recorded recipient list".to_string()),
        },
        Some(json) => match serde_json::from_str::<Vec<String>>(json) {
            Ok(keys) if keys.iter().any(|k| k == escrow) => Coverage::Covered,
            Ok(_) => Coverage::Gap("encrypted without the current escrow recipient".to_string()),
            Err(_) => Coverage::Gap("recipient list is unreadable".to_string()),
        },
    }
}

/// Why the current escrow key cannot recover this stage set, or `None` if it
/// can. `Unknown` renders as [`UNKNOWN_REASON`].
pub fn gap(fingerprints: Option<&str>, origin: Origin, escrow: &str) -> Option<String> {
    match classify(fingerprints, origin, escrow) {
        Coverage::Covered => None,
        Coverage::Unknown => Some(UNKNOWN_REASON.to_string()),
        Coverage::Gap(reason) => Some(reason),
    }
}

/// The one-word column value for `catalog locate` / `report copies`.
///
/// `-` when no escrow recipient is registered at all: there is nothing to
/// compare against, so claiming either "yes" or "NO" would be a false report.
/// That case is caught at stage time instead (#115). `?` is a rebuilt row the
/// tape could not vouch for — see the module header.
pub fn marker(fingerprints: Option<&str>, origin: Origin, escrow: Option<&str>) -> &'static str {
    match escrow {
        None => "-",
        Some(e) => match classify(fingerprints, origin, e) {
            Coverage::Covered => "yes",
            Coverage::Unknown => "?",
            Coverage::Gap(_) => "NO",
        },
    }
}

// ── The query half ──────────────────────────────────────────────────────

/// Which slice of escrow coverage [`stage_set_coverage`] is being asked
/// about. A typed enum, not a free-form predicate, for the same reason
/// [`crate::policy::coverage::CoverageScope`] is: the generated SQL defines
/// its own aliases, and a caller-supplied fragment would bind to whatever
/// happened to be in scope.
#[derive(Debug, Clone, Copy)]
pub enum Scope<'a> {
    /// Every completed write of this unit's stage sets, on an in-service
    /// volume — across every snapshot, not only the current one. An escrow
    /// gap on a superseded snapshot's tape is still a gap; nothing in the
    /// four call sites wants it hidden.
    Unit(i64),
    /// The same, for every unit in the catalog. `report copies --unit` with
    /// no filter, and `report copies` overall, are the callers.
    AllUnits,
    /// A specific set of stage sets by id, with NO `writes`/`volumes` join.
    /// This is the `volume write` pre-flight's shape: a stage set about to
    /// be written has no `writes` row yet, completed or otherwise, so there
    /// is no volume to filter on. A stage set id absent from `stage_sets`
    /// entirely (or whose snapshot/unit chain is broken) still gets a row —
    /// fail-closed, not silently dropped.
    StageSets(&'a [i64]),
    /// Every completed write of this unit's stage sets on ANY volume,
    /// whatever its status — retired, quarantined, erased included — and
    /// including unencrypted stage sets. `catalog locate` lists every
    /// volume a unit was ever written to (issue #57: a retired cartridge
    /// must be distinguishable from a sealed one, not absent), so its
    /// escrow column has to be answered for every row it lists. The
    /// question is about bytes, not custody: a retired cartridge either
    /// opens with the escrow key or it does not.
    UnitAnyVolume(i64),
}

/// One stage set's escrow verdict, as reported by [`stage_set_coverage`].
#[derive(Debug, Clone)]
pub struct CoveredStageSet {
    pub stage_set_id: i64,
    pub unit_name: String,
    /// The volume this stage set's completed write landed on. `None` for
    /// [`Scope::StageSets`] — there is no completed write to name yet.
    pub volume_label: Option<String>,
    pub coverage: Coverage,
}

/// Classify one row already read from the database: the `encrypted = 0`
/// case is folded in here, in the one place, rather than in [`classify`]
/// (whose signature this task must not change) or duplicated at every call
/// site.
fn classify_row(
    fingerprints: Option<&str>,
    origin: &str,
    encrypted: i64,
    escrow: &str,
) -> Coverage {
    if encrypted == 0 {
        return Coverage::Gap("staged with encrypted=0".to_string());
    }
    classify(fingerprints, Origin::parse(origin), escrow)
}

/// **The** query behind "escrow coverage of a stage set" — see the module
/// header for why the volume filter is [`crate::policy::coverage::in_service`]
/// and why `encrypted = 0` is handled here rather than in [`classify`].
///
/// Do not inline this SQL at a fifth call site. Four already drifted from
/// each other before this function existed.
pub fn stage_set_coverage(
    conn: &rusqlite::Connection,
    scope: Scope,
    escrow: &str,
) -> crate::error::Result<Vec<CoveredStageSet>> {
    match scope {
        Scope::StageSets(ids) => stage_set_coverage_by_id(conn, ids, escrow),
        Scope::Unit(unit_id) => {
            stage_set_coverage_via_writes(conn, Some(unit_id), Volumes::InService, escrow)
        }
        Scope::AllUnits => stage_set_coverage_via_writes(conn, None, Volumes::InService, escrow),
        Scope::UnitAnyVolume(unit_id) => {
            stage_set_coverage_via_writes(conn, Some(unit_id), Volumes::Any, escrow)
        }
    }
}

/// Which volumes a via-`writes` scope reports on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Volumes {
    /// `coverage::in_service` — the reporting scopes (`audit`, `report
    /// copies`). These also skip `encrypted = 0` stage sets: `audit`'s
    /// `encryption` VIOLATION already owns "this was written in the clear",
    /// and reporting the same fact a second time as an escrow WARNING is
    /// noise, not information. The write pre-flight (`Scope::StageSets`)
    /// still sees them, because there nothing else does.
    InService,
    /// No status filter and no encryption filter — `catalog locate`.
    Any,
}

/// [`Scope::Unit`] / [`Scope::AllUnits`] / [`Scope::UnitAnyVolume`]: every
/// completed write, joined back to its unit, on the volumes `volumes` says.
fn stage_set_coverage_via_writes(
    conn: &rusqlite::Connection,
    unit_id: Option<i64>,
    volumes: Volumes,
    escrow: &str,
) -> crate::error::Result<Vec<CoveredStageSet>> {
    let mut sql = String::from(
        "SELECT ss.id, u.name, v.label, ss.key_fingerprints, ss.origin, ss.encrypted
         FROM writes w
         JOIN stage_sets ss ON ss.id = w.stage_set_id
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN units u ON u.id = s.unit_id
         JOIN volumes v ON v.id = w.volume_id
         WHERE w.status = 'completed'",
    );
    if volumes == Volumes::InService {
        sql.push_str(&format!(
            " AND {} AND ss.encrypted = 1",
            crate::policy::coverage::in_service("v")
        ));
    }
    if unit_id.is_some() {
        sql.push_str(" AND s.unit_id = ?1");
    }
    sql.push_str(" ORDER BY u.name, v.label, ss.id");

    let mut stmt = conn.prepare(&sql)?;
    #[allow(clippy::type_complexity)]
    let map_row = |row: &rusqlite::Row| -> rusqlite::Result<(i64, String, String, Option<String>, String, i64)> {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
        ))
    };
    let rows: Vec<_> = match unit_id {
        Some(id) => stmt
            .query_map(rusqlite::params![id], map_row)?
            .collect::<std::result::Result<_, _>>()?,
        None => stmt
            .query_map([], map_row)?
            .collect::<std::result::Result<_, _>>()?,
    };

    Ok(rows
        .into_iter()
        .map(
            |(stage_set_id, unit_name, volume_label, fingerprints, origin, encrypted)| {
                CoveredStageSet {
                    stage_set_id,
                    unit_name,
                    volume_label: Some(volume_label),
                    coverage: classify_row(fingerprints.as_deref(), &origin, encrypted, escrow),
                }
            },
        )
        .collect())
}

/// [`Scope::StageSets`]: by id, no `writes`/`volumes` join. Mirrors the
/// per-id lookup `stage_sets_lacking_escrow` used to run inline, including
/// its fail-closed handling of an id with no surviving row.
fn stage_set_coverage_by_id(
    conn: &rusqlite::Connection,
    ids: &[i64],
    escrow: &str,
) -> crate::error::Result<Vec<CoveredStageSet>> {
    let mut stmt = conn.prepare(
        "SELECT u.name, ss.key_fingerprints, ss.origin, ss.encrypted
         FROM stage_sets ss
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN units u ON u.id = s.unit_id
         WHERE ss.id = ?1",
    )?;
    let mut out = Vec::with_capacity(ids.len());
    for &stage_set_id in ids {
        let row: Option<(String, Option<String>, String, i64)> = stmt
            .query_row(rusqlite::params![stage_set_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?;
        let (unit_name, coverage) = match row {
            // No row at all (or a broken snapshot/unit chain): as unprovable
            // as a NULL recipient list — same fail-closed answer, same
            // wording `stage_sets_lacking_escrow` always used.
            None => (
                format!("<unknown unit for stage set {stage_set_id}>"),
                Coverage::Gap("no recorded recipient list".to_string()),
            ),
            Some((unit_name, fingerprints, origin, encrypted)) => (
                unit_name,
                classify_row(fingerprints.as_deref(), &origin, encrypted, escrow),
            ),
        };
        out.push(CoveredStageSet {
            stage_set_id,
            unit_name,
            volume_label: None,
            coverage,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_containing_the_escrow_key_is_covered() {
        let json = r#"["age1alice","age1escrow"]"#;
        assert_eq!(gap(Some(json), Origin::Staged, "age1escrow"), None);
        assert_eq!(
            marker(Some(json), Origin::Staged, Some("age1escrow")),
            "yes"
        );
    }

    #[test]
    fn a_list_without_it_is_reported_with_a_reason() {
        let json = r#"["age1alice","age1operator"]"#;
        assert_eq!(
            gap(Some(json), Origin::Staged, "age1escrow").as_deref(),
            Some("encrypted without the current escrow recipient")
        );
        assert_eq!(marker(Some(json), Origin::Staged, Some("age1escrow")), "NO");
    }

    /// The fail-closed arms. Neither can be shown recoverable, so neither is
    /// allowed to read as covered.
    #[test]
    fn an_absent_or_unreadable_list_fails_closed() {
        assert!(gap(None, Origin::Staged, "age1escrow").is_some());
        assert!(gap(Some("{not json"), Origin::Staged, "age1escrow").is_some());
        assert_eq!(marker(None, Origin::Staged, Some("age1escrow")), "NO");
        assert_eq!(
            marker(Some("{not json"), Origin::Staged, Some("age1escrow")),
            "NO"
        );
    }

    /// With no escrow registered there is no question to answer, and
    /// answering anyway would be a false report in whichever direction.
    #[test]
    fn no_registered_escrow_renders_as_neither_yes_nor_no() {
        assert_eq!(marker(Some(r#"["age1alice"]"#), Origin::Staged, None), "-");
        assert_eq!(marker(None, Origin::Staged, None), "-");
    }

    /// #137: a rebuilt row with no list is a different EXPLANATION, not a
    /// different VERDICT. Still not covered, but says so in words that name
    /// the remedy, and renders as `?` rather than `NO`.
    #[test]
    fn a_rebuilt_row_with_no_list_is_unknown_not_a_gap() {
        assert_eq!(
            classify(None, Origin::Rebuilt, "age1escrow"),
            Coverage::Unknown
        );
        assert_eq!(
            gap(None, Origin::Rebuilt, "age1escrow").as_deref(),
            Some(UNKNOWN_REASON)
        );
        assert_eq!(marker(None, Origin::Rebuilt, Some("age1escrow")), "?");
        // The same NULL, staged here, is the old fail-closed gap.
        assert!(matches!(
            classify(None, Origin::Staged, "age1escrow"),
            Coverage::Gap(_)
        ));
    }

    /// Once attested (the list is filled in), origin no longer matters: a
    /// rebuilt row that names the key is covered like any other.
    #[test]
    fn an_attested_rebuilt_row_is_covered() {
        let json = r#"["age1escrow"]"#;
        assert_eq!(
            classify(Some(json), Origin::Rebuilt, "age1escrow"),
            Coverage::Covered
        );
        assert_eq!(
            marker(Some(json), Origin::Rebuilt, Some("age1escrow")),
            "yes"
        );
        // ...and a rebuilt row naming the WRONG key is a gap, not unknown.
        assert!(matches!(
            classify(Some(r#"["age1old"]"#), Origin::Rebuilt, "age1escrow"),
            Coverage::Gap(_)
        ));
    }

    #[test]
    fn origin_parses_conservatively() {
        assert_eq!(Origin::parse("rebuilt"), Origin::Rebuilt);
        assert_eq!(Origin::parse("staged"), Origin::Staged);
        assert_eq!(Origin::parse("anything else"), Origin::Staged);
    }

    mod query {
        use super::*;
        use crate::db;
        use rusqlite::{params, Connection};

        const ESCROW: &str = "age1escrowescrowescrow";

        /// One unit `photos`, one snapshot, one stage set, one completed
        /// write to a volume `VOL1` of the given `status`. Returns
        /// `(conn, unit_id, stage_set_id)`.
        fn setup(
            status: &str,
            fingerprints: Option<&str>,
            origin: &str,
            encrypted: i64,
        ) -> (Connection, i64, i64) {
            let conn = db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
                [],
            )
            .unwrap();
            let tid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                 VALUES ('u-photos', 'photos', ?1, 'mtime_size', 1, 'active')",
                params![tid],
            )
            .unwrap();
            let unit_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
                 VALUES (?1, 1, 'full', 'current', '/tmp/photos')",
                params![unit_id],
            )
            .unwrap();
            let snap_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO stage_sets
                    (snapshot_id, status, slice_size, encrypted, key_fingerprints, origin)
                 VALUES (?1, 'staged', 524288, ?2, ?3, ?4)",
                params![snap_id, encrypted, fingerprints, origin],
            )
            .unwrap();
            let ss_id = conn.last_insert_rowid();
            // Issue #242: 'quarantined' is a condition now, not a status --
            // translate it onto `observed_condition`, leaving `status` at
            // 'sealed'.
            let (status_value, condition_value) = if status == "quarantined" {
                ("sealed", "quarantined")
            } else {
                (status, "ok")
            };
            conn.execute(
                "INSERT INTO volumes
                    (label, backend_type, backend_name, media_type, capacity_bytes, status,
                     observed_condition)
                 VALUES ('VOL1', 'lto', 'lto0', 'LTO-6', 2500000000000, ?1, ?2)",
                params![status_value, condition_value],
            )
            .unwrap();
            let vol_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, 'completed')",
                params![ss_id, snap_id, vol_id],
            )
            .unwrap();
            (conn, unit_id, ss_id)
        }

        #[test]
        fn a_covered_row() {
            let (conn, unit_id, ss_id) =
                setup("sealed", Some(r#"["age1escrowescrowescrow"]"#), "staged", 1);
            let rows = stage_set_coverage(&conn, Scope::Unit(unit_id), ESCROW).unwrap();
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert_eq!(rows[0].stage_set_id, ss_id);
            assert_eq!(rows[0].volume_label.as_deref(), Some("VOL1"));
            assert_eq!(rows[0].coverage, Coverage::Covered);
        }

        #[test]
        fn a_gap_row() {
            let (conn, unit_id, _ss_id) = setup("sealed", Some(r#"["age1alice"]"#), "staged", 1);
            let rows = stage_set_coverage(&conn, Scope::Unit(unit_id), ESCROW).unwrap();
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert!(matches!(rows[0].coverage, Coverage::Gap(_)));
        }

        #[test]
        fn an_unknown_rebuilt_row() {
            let (conn, unit_id, _ss_id) = setup("sealed", None, "rebuilt", 1);
            let rows = stage_set_coverage(&conn, Scope::Unit(unit_id), ESCROW).unwrap();
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert_eq!(rows[0].coverage, Coverage::Unknown);
        }

        /// `stage create` has hard-coded `encrypted = 1` since #115, so a
        /// `0` today can only be a legacy/corrupt row. Handled here, not in
        /// `classify`.
        #[test]
        fn an_encrypted_zero_row_is_left_to_the_encryption_check_by_the_reporting_scopes() {
            let (conn, unit_id, ss_id) =
                setup("sealed", Some(r#"["age1escrowescrowescrow"]"#), "staged", 0);
            // audit / report copies: not this check's fact — `encryption` owns it.
            let rows = stage_set_coverage(&conn, Scope::Unit(unit_id), ESCROW).unwrap();
            assert!(rows.is_empty(), "{rows:?}");
            let rows = stage_set_coverage(&conn, Scope::AllUnits, ESCROW).unwrap();
            assert!(rows.is_empty(), "{rows:?}");
            // the write pre-flight and locate: still a gap, in those words.
            for scope in [Scope::StageSets(&[ss_id]), Scope::UnitAnyVolume(unit_id)] {
                let rows = stage_set_coverage(&conn, scope, ESCROW).unwrap();
                assert_eq!(rows.len(), 1, "{rows:?}");
                assert_eq!(
                    rows[0].coverage,
                    Coverage::Gap("staged with encrypted=0".to_string())
                );
            }
        }

        /// `catalog locate` lists a retired cartridge on purpose (#57), so
        /// its escrow column is answered there too — the bytes on it either
        /// open with the escrow key or they do not.
        #[test]
        fn unit_any_volume_scope_answers_for_a_retired_volume() {
            let (conn, unit_id, _ss_id) = setup(
                "retired",
                Some(r#"["age1escrowescrowescrow"]"#),
                "staged",
                1,
            );
            assert!(stage_set_coverage(&conn, Scope::Unit(unit_id), ESCROW)
                .unwrap()
                .is_empty());
            let rows = stage_set_coverage(&conn, Scope::UnitAnyVolume(unit_id), ESCROW).unwrap();
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert_eq!(rows[0].coverage, Coverage::Covered);
        }

        /// The volume filter is `in_service`, not `eligible` and not the old
        /// ad-hoc status list — see the module header. A retired volume
        /// fails all three, so this does not distinguish them, but it does
        /// pin that a dead cartridge's stage set is excluded, not reported
        /// as a gap.
        #[test]
        fn a_row_on_a_retired_volume_is_excluded() {
            let (conn, unit_id, _ss_id) = setup(
                "retired",
                Some(r#"["age1escrowescrowescrow"]"#),
                "staged",
                1,
            );
            let rows = stage_set_coverage(&conn, Scope::Unit(unit_id), ESCROW).unwrap();
            assert!(
                rows.is_empty(),
                "a retired volume is not in_service: {rows:?}"
            );
        }

        /// The ad-hoc status list this replaces let `quarantined` volumes
        /// through (they were not in its NOT-IN list) — exactly the #96
        /// failure mode in a new dimension. `in_service` excludes them.
        #[test]
        fn a_row_on_a_quarantined_volume_is_excluded() {
            let (conn, unit_id, _ss_id) = setup(
                "quarantined",
                Some(r#"["age1escrowescrowescrow"]"#),
                "staged",
                1,
            );
            let rows = stage_set_coverage(&conn, Scope::Unit(unit_id), ESCROW).unwrap();
            assert!(
                rows.is_empty(),
                "a quarantined volume is not in_service: {rows:?}"
            );
        }

        #[test]
        fn stage_sets_scope_reports_an_unknown_id_as_a_gap() {
            let conn = db::open_memory().unwrap();
            let rows = stage_set_coverage(&conn, Scope::StageSets(&[999]), ESCROW).unwrap();
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert_eq!(rows[0].stage_set_id, 999);
            assert!(
                rows[0].unit_name.contains("unknown"),
                "{}",
                rows[0].unit_name
            );
            assert_eq!(rows[0].volume_label, None);
            assert_eq!(
                rows[0].coverage,
                Coverage::Gap("no recorded recipient list".to_string())
            );
        }

        /// `Scope::StageSets` has no volume join at all — a stage set about
        /// to be written has no completed write yet. Pin that a real,
        /// covered row still resolves correctly through that path.
        #[test]
        fn stage_sets_scope_covers_a_real_row_with_no_volume_join() {
            let (conn, _unit_id, ss_id) =
                setup("sealed", Some(r#"["age1escrowescrowescrow"]"#), "staged", 1);
            let rows = stage_set_coverage(&conn, Scope::StageSets(&[ss_id]), ESCROW).unwrap();
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert_eq!(rows[0].coverage, Coverage::Covered);
            assert_eq!(rows[0].volume_label, None);
        }

        #[test]
        fn unit_scope_excludes_other_units() {
            let (conn, unit_id, _ss_id) =
                setup("sealed", Some(r#"["age1escrowescrowescrow"]"#), "staged", 1);

            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('t2', 0, 'active')",
                [],
            )
            .unwrap();
            let tid2 = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                 VALUES ('u-other', 'other', ?1, 'mtime_size', 1, 'active')",
                params![tid2],
            )
            .unwrap();
            let other_unit = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
                 VALUES (?1, 1, 'full', 'current', '/tmp/other')",
                params![other_unit],
            )
            .unwrap();
            let snap2 = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO stage_sets (snapshot_id, status, slice_size, encrypted, key_fingerprints)
                 VALUES (?1, 'staged', 524288, 1, '[\"age1alice\"]')",
                params![snap2],
            )
            .unwrap();
            let ss2 = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO volumes
                    (label, backend_type, backend_name, media_type, capacity_bytes, status)
                 VALUES ('VOL2', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
                [],
            )
            .unwrap();
            let vol2 = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, 'completed')",
                params![ss2, snap2, vol2],
            )
            .unwrap();

            let rows = stage_set_coverage(&conn, Scope::Unit(unit_id), ESCROW).unwrap();
            assert_eq!(
                rows.len(),
                1,
                "must not see the other unit's stage set: {rows:?}"
            );
            assert_eq!(rows[0].unit_name, "photos");
        }

        #[test]
        fn all_units_scope_includes_every_unit() {
            let (conn, _unit_id, _ss_id) = setup("sealed", Some(r#"["age1alice"]"#), "staged", 1);
            let rows = stage_set_coverage(&conn, Scope::AllUnits, ESCROW).unwrap();
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert_eq!(rows[0].unit_name, "photos");
        }
    }
}
