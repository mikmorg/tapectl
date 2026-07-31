//! ADR-0004 Tier-1 evidence-age display (issue #91).
//!
//! ADR-0004 requires that evidence *age* be **displayed** wherever a
//! destructive operation consumes copy coverage — never blocking, never
//! gating, never a flag. ADR-0008 keeps this as Tier 1: unlike Tier 2
//! (`cli::consent::confirm`), Tier 1 facts never reach a consent gate at
//! all, they are printed unconditionally alongside the impact analysis.
//!
//! This module is split into a query half ([`remaining_coverage_evidence`])
//! and a pure formatter half ([`describe`]) so the wording can be unit
//! tested without a database.

use rusqlite::{params, Connection};

use crate::error::Result;

/// One volume's contribution to a unit's remaining coverage, after the
/// volume being retired/consumed is excluded, together with that volume's
/// most recent PASSED verification timestamp (or `None` if it has never
/// passed a verification).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageEvidence {
    pub volume_label: String,
    pub last_verified: Option<String>,
}

/// Per-volume remaining-coverage evidence for `unit_id`, optionally
/// excluding `exclude_volume_id` (the volume being retired/consumed) by
/// identity.
///
/// `exclude_volume_id`:
/// - `Some(vol_id)` — the caller is retiring/consuming `vol_id` and wants
///   the coverage that would REMAIN after it (`compact_finish`,
///   `retire_impacts`).
/// - `None` — there is no volume to exclude; the caller wants ALL eligible
///   coverage for the unit (`unit_mark_tape_only`, which asks "what
///   coverage exists at all?", not "what would remain"). This is a real
///   SQL branch, not a sentinel volume id that happens to match nothing —
///   see issue #99.
///
/// Divergences from `cli::audit`'s `verify_age` query (deliberate, see
/// module doc and issue #91's trap list):
/// - one row **per eligible volume**, not a unit-level `MAX` — the point is
///   attribution ("rests on L6-0003"), not just an age;
/// - `verification_sessions` is **LEFT JOIN**ed (with the `outcome =
///   'passed'` filter living in the join's `ON` clause, not `WHERE`) so an
///   eligible volume with zero passed sessions still appears, rendering as
///   "never verified" rather than vanishing — a `WHERE` filter on a
///   LEFT-JOINed column would silently turn this back into an inner join;
/// - [`crate::policy::coverage::eligible`] is applied to the `volumes`
///   alias directly in the join condition, per that module's doc for plain
///   inner joins with no `GROUP BY`;
/// - the volume being excluded, when present, is excluded by identity
///   (`w.volume_id != ?`), matching `retire_impacts`.
pub fn remaining_coverage_evidence(
    conn: &Connection,
    unit_id: i64,
    exclude_volume_id: Option<i64>,
) -> Result<Vec<CoverageEvidence>> {
    let exclude_clause = match exclude_volume_id {
        Some(_) => "AND w.volume_id != ?2",
        None => "",
    };
    let sql = format!(
        "SELECT v.label, MAX(vs.completed_at) as last_verified
         FROM writes w
         JOIN stage_sets ss ON ss.id = w.stage_set_id
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN volumes v ON v.id = w.volume_id
         LEFT JOIN verification_sessions vs
                ON vs.volume_id = v.id AND vs.outcome = 'passed'
         WHERE s.unit_id = ?1 AND w.status = 'completed'
           {exclude_clause}
           AND {}
         GROUP BY v.id, v.label
         ORDER BY v.label",
        crate::policy::coverage::eligible("v")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<CoverageEvidence> = match exclude_volume_id {
        Some(exclude_id) => stmt
            .query_map(params![unit_id, exclude_id], |row| {
                Ok(CoverageEvidence {
                    volume_label: row.get(0)?,
                    last_verified: row.get(1)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?,
        None => stmt
            .query_map(params![unit_id], |row| {
                Ok(CoverageEvidence {
                    volume_label: row.get(0)?,
                    last_verified: row.get(1)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?,
    };
    Ok(rows)
}

/// The relative weakness of one piece of evidence, used to pick the
/// weakest volume to name in [`describe`]. Never-verified is weakest;
/// otherwise older is weaker than newer. An unparseable stamp is treated as
/// weaker than any parseable age (it is a data-quality problem the operator
/// should see), but distinct from never-verified so it never gets misread
/// as "never".
enum Weakness {
    Never,
    Unparseable,
    Age(i64),
}

fn weakness(evidence: &CoverageEvidence, now: chrono::NaiveDateTime) -> Weakness {
    match &evidence.last_verified {
        None => Weakness::Never,
        Some(raw) => match chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S") {
            Ok(dt) => Weakness::Age((now - dt).num_days()),
            Err(_) => Weakness::Unparseable,
        },
    }
}

fn weakness_rank(w: &Weakness) -> i64 {
    match w {
        Weakness::Never => i64::MAX,
        Weakness::Unparseable => i64::MAX - 1,
        Weakness::Age(days) => *days,
    }
}

/// Format one ADR-0004 evidence line naming the WEAKEST piece of remaining
/// coverage for `unit_name` — never-verified ranks weakest, otherwise the
/// oldest passing verification is weakest. Returns `None` when `evidence`
/// is empty: a zero-copy unit has no evidence to describe, and that case is
/// already covered by the existing ZERO-copies line.
///
/// Takes `now` as a parameter rather than reading `Utc::now()` internally
/// so the age arithmetic is deterministically testable.
///
/// The single-copy case is worded exactly as ADR-0004 words it
/// (`rests on L6-0003, last verified N days ago`). The multi-copy case
/// MUST NOT reuse that wording: "rests on <label>" asserts sole
/// dependence, and with two copies — one verified yesterday, one never —
/// naming only the weakest would tell the operator their coverage hangs
/// on an unverified tape when it does not. That misreading is not
/// hypothetical at the one place it matters most: `cli::consent::confirm`
/// prints each fact as a STANDALONE line, with none of the surrounding
/// copy-count context that `print_retire_impact` and the `--json`
/// `evidence` array supply. So the plural case states the count first and
/// labels the named volume as the weakest of them.
pub fn describe(
    unit_name: &str,
    evidence: &[CoverageEvidence],
    now: chrono::NaiveDateTime,
) -> Option<String> {
    let weakest = evidence
        .iter()
        .max_by_key(|e| weakness_rank(&weakness(e, now)))?;

    let detail = match weakness(weakest, now) {
        Weakness::Never => "never verified".to_string(),
        Weakness::Age(days) => format!("last verified {days} days ago"),
        Weakness::Unparseable => format!(
            "last verified at {} (unparseable timestamp)",
            weakest.last_verified.as_deref().unwrap_or("?")
        ),
    };

    let label = &weakest.volume_label;
    Some(if evidence.len() == 1 {
        format!("coverage for unit \"{unit_name}\" rests on {label}, {detail}")
    } else {
        format!(
            "coverage for unit \"{unit_name}\" rests on {} copies; weakest is {label}, {detail}",
            evidence.len()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::parse_from_str("2026-07-30 12:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
    }

    fn ev(label: &str, last_verified: Option<&str>) -> CoverageEvidence {
        CoverageEvidence {
            volume_label: label.to_string(),
            last_verified: last_verified.map(str::to_string),
        }
    }

    #[test]
    fn empty_evidence_is_none() {
        assert_eq!(describe("photos", &[], now()), None);
    }

    #[test]
    fn never_verified_renders_honestly() {
        let evidence = vec![ev("L6-0003", None)];
        assert_eq!(
            describe("photos", &evidence, now()).unwrap(),
            "coverage for unit \"photos\" rests on L6-0003, never verified"
        );
    }

    #[test]
    fn old_verification_renders_days_ago() {
        let evidence = vec![ev("L6-0003", Some("2011-08-01 00:00:00"))];
        let line = describe("photos", &evidence, now()).unwrap();
        assert!(line.starts_with("coverage for unit \"photos\" rests on L6-0003, last verified "));
        assert!(line.ends_with("days ago"));
        assert!(line.contains("5477") || line.contains("548")); // sanity: large age
    }

    #[test]
    fn recent_verification_renders_small_age() {
        let evidence = vec![ev("L6-0009", Some("2026-07-28 12:00:00"))];
        assert_eq!(
            describe("photos", &evidence, now()).unwrap(),
            "coverage for unit \"photos\" rests on L6-0009, last verified 2 days ago"
        );
    }

    #[test]
    fn unparseable_timestamp_says_so_honestly() {
        let evidence = vec![ev("L6-0003", Some("not-a-timestamp"))];
        assert_eq!(
            describe("photos", &evidence, now()).unwrap(),
            "coverage for unit \"photos\" rests on L6-0003, last verified at not-a-timestamp (unparseable timestamp)"
        );
    }

    #[test]
    fn weakest_of_several_is_selected_never_ranks_weakest() {
        let evidence = vec![
            ev("L6-0001", Some("2026-07-29 12:00:00")), // 1 day
            ev("L6-0002", None),                        // never — weakest
            ev("L6-0003", Some("2020-01-01 00:00:00")), // old but not never
        ];
        let line = describe("photos", &evidence, now()).unwrap();
        assert_eq!(
            line,
            "coverage for unit \"photos\" rests on 3 copies; weakest is L6-0002, never verified"
        );
    }

    #[test]
    fn weakest_of_several_without_never_picks_oldest() {
        let evidence = vec![
            ev("L6-0001", Some("2026-07-29 12:00:00")), // 1 day
            ev("L6-0003", Some("2020-01-01 00:00:00")), // oldest
        ];
        let line = describe("photos", &evidence, now()).unwrap();
        assert!(line.starts_with(
            "coverage for unit \"photos\" rests on 2 copies; weakest is L6-0003, last verified "
        ));
    }

    /// The plural wording is not cosmetic. `cli::consent::confirm` prints
    /// each fact as a standalone line, so a multi-copy unit rendered with
    /// the singular "rests on <label>" would tell the operator, at the
    /// irreversible moment, that their coverage hangs on a never-verified
    /// tape when a freshly-verified copy also exists. Pin both halves: the
    /// count must be stated, and the singular sole-dependence phrasing must
    /// NOT appear.
    #[test]
    fn multi_copy_line_never_claims_sole_dependence() {
        let evidence = vec![
            ev("L6-0001", Some("2026-07-29 12:00:00")), // verified yesterday
            ev("L6-0009", None),                        // never verified — weakest
        ];
        let line = describe("photos", &evidence, now()).unwrap();
        assert!(
            line.contains("rests on 2 copies"),
            "multi-copy line must state the count: {line}"
        );
        assert!(
            !line.contains("rests on L6-0009"),
            "multi-copy line must not assert sole dependence on the weakest copy: {line}"
        );
        assert!(
            line.contains("weakest is L6-0009, never verified"),
            "the weakest copy must still be named and dated: {line}"
        );
    }

    /// ADR-0004's own example wording, pinned verbatim: with exactly one
    /// remaining copy the line must read as the ADR writes it.
    #[test]
    fn single_copy_matches_adr_0004_wording_exactly() {
        let evidence = vec![ev("L6-0003", Some("2011-07-30 12:00:00"))];
        let line = describe("photos", &evidence, now()).unwrap();
        assert!(
            line.starts_with("coverage for unit \"photos\" rests on L6-0003, last verified "),
            "single-copy wording must match ADR-0004's example: {line}"
        );
        assert!(
            !line.contains("copies"),
            "single-copy wording must not pluralize: {line}"
        );
    }
}
