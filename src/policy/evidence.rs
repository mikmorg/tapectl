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

/// Which EVIDENCE CLASS a piece of remaining coverage belongs to
/// (ADR-0006). The two are not interchangeable and the display must never
/// let them read as if they were:
///
/// - `Tape` — evidence comes from physical re-verification at contact and
///   decays with the medium. It can be refreshed: load the cartridge, run
///   `volume verify`, and the age resets.
/// - `WarehouseDeposit` — evidence is the deposit receipt plus provider
///   attestation, "aging without refresh (re-verification costs retrieval
///   and realistically never happens)". There is no verification session
///   for a deposit and there never will be, so the honest thing to state
///   is when it was deposited and that nothing has checked it since.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceKind {
    Tape,
    WarehouseDeposit,
}

/// One contribution to a unit's remaining coverage, after the volume being
/// retired/consumed is excluded.
///
/// For a `Tape` row, `last_verified` is that volume's most recent PASSED
/// verification timestamp (`None` = never verified) and `deposited_at` /
/// `location` are `None`.
///
/// For a `WarehouseDeposit` row, `last_verified` is ALWAYS `None` — by
/// design, not by accident — and `deposited_at` carries the recorded
/// deposit time. The two are deliberately separate fields: folding a
/// deposit date into `last_verified` would make a never-checked cloud
/// object render exactly like a verified cartridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageEvidence {
    pub kind: EvidenceKind,
    pub volume_label: String,
    pub last_verified: Option<String>,
    pub deposited_at: Option<String>,
    pub location: Option<String>,
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
    let tape_row = |row: &rusqlite::Row| -> rusqlite::Result<CoverageEvidence> {
        Ok(CoverageEvidence {
            kind: EvidenceKind::Tape,
            volume_label: row.get(0)?,
            last_verified: row.get(1)?,
            deposited_at: None,
            location: None,
        })
    };
    let mut rows: Vec<CoverageEvidence> = match exclude_volume_id {
        Some(exclude_id) => stmt
            .query_map(params![unit_id, exclude_id], tape_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?,
        None => stmt
            .query_map(params![unit_id], tape_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?,
    };

    // ADR-0006 warehouse deposits (issue #73). A separate query rather than
    // a UNION with the query above: the two halves select different
    // columns and carry genuinely different evidence, and the point of the
    // whole module is that they stay distinguishable all the way to the
    // printed line. Scope and exclusion match the tape half exactly --
    // including that the deposit's SOURCE VOLUME must still pass
    // `coverage::eligible`, the same gate `coverage::copy_count_expr`
    // applies, so the two can never disagree about what coverage exists.
    let deposit_exclude = match exclude_volume_id {
        Some(_) => "AND w.volume_id != ?2",
        None => "",
    };
    let deposit_sql = format!(
        "SELECT v.label, l.name, d.deposited_at
         FROM volume_deposits d
         JOIN volumes v ON v.id = d.volume_id
         JOIN locations l ON l.id = d.location_id
         WHERE {} AND d.volume_id IN (
             SELECT w.volume_id
             FROM writes w
             JOIN stage_sets ss ON ss.id = w.stage_set_id
             JOIN snapshots s ON s.id = ss.snapshot_id
             WHERE s.unit_id = ?1 AND w.status = 'completed' {deposit_exclude}
         )
         ORDER BY v.label, l.name",
        crate::policy::coverage::eligible("v")
    );
    let mut deposit_stmt = conn.prepare(&deposit_sql)?;
    let deposit_row = |row: &rusqlite::Row| -> rusqlite::Result<CoverageEvidence> {
        Ok(CoverageEvidence {
            kind: EvidenceKind::WarehouseDeposit,
            volume_label: row.get(0)?,
            last_verified: None,
            location: row.get(1)?,
            deposited_at: row.get(2)?,
        })
    };
    let deposits: Vec<CoverageEvidence> = match exclude_volume_id {
        Some(exclude_id) => deposit_stmt
            .query_map(params![unit_id, exclude_id], deposit_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?,
        None => deposit_stmt
            .query_map(params![unit_id], deposit_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?,
    };
    rows.extend(deposits);
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
    if evidence.is_empty() {
        return None;
    }
    let tapes: Vec<&CoverageEvidence> = evidence
        .iter()
        .filter(|e| e.kind == EvidenceKind::Tape)
        .collect();
    let deposits: Vec<&CoverageEvidence> = evidence
        .iter()
        .filter(|e| e.kind == EvidenceKind::WarehouseDeposit)
        .collect();

    // Whom to NAME as the weakest. A warehouse deposit and a
    // never-verified tape are not on one scale (ADR-0006: different
    // evidence classes), and totally ordering them means a mixed case can
    // name the deposit and leave the never-verified TAPE unmentioned on
    // the one line `cli::consent::confirm` guarantees the operator reads.
    // So: if any tape is present, the named weakest is the weakest TAPE,
    // and the composition clause states the deposits. Only an
    // all-deposit unit names a deposit.
    let weakest_tape = tapes
        .iter()
        .max_by_key(|e| weakness_rank(&weakness(e, now)));
    let named: &CoverageEvidence = match weakest_tape {
        Some(t) => t,
        // Oldest deposit first: `deposited_at` sorts lexicographically as
        // an ISO-ish stamp, and a missing stamp sorts weakest of all.
        None => deposits
            .iter()
            .min_by_key(|e| e.deposited_at.clone().unwrap_or_default())
            .copied()?,
    };

    if evidence.len() == 1 {
        return Some(match named.kind {
            EvidenceKind::Tape => format!(
                "coverage for unit \"{unit_name}\" rests on {}, {}",
                named.volume_label,
                tape_detail(named, now)
            ),
            EvidenceKind::WarehouseDeposit => format!(
                "coverage for unit \"{unit_name}\" rests on {} — never re-verified, \
                 and warehouse copies do not refresh",
                deposit_phrase(named)
            ),
        });
    }

    // The composition clause appears ONLY when a deposit is involved. An
    // all-tape fleet's wording is untouched (ADR-0004's example, pinned by
    // `weakest_of_several_is_selected_never_ranks_weakest`).
    let composition = if deposits.is_empty() {
        String::new()
    } else {
        let d = deposits.len();
        let deposit_words = if d == 1 {
            "1 warehouse deposit".to_string()
        } else {
            format!("{d} warehouse deposits")
        };
        let tape_words = match tapes.len() {
            0 => "no tape copies".to_string(),
            1 => "1 tape".to_string(),
            n => format!("{n} tapes"),
        };
        format!(" ({tape_words}, {deposit_words})")
    };

    let weakest_clause = match named.kind {
        EvidenceKind::Tape => format!(
            "weakest is {}, {}",
            named.volume_label,
            tape_detail(named, now)
        ),
        EvidenceKind::WarehouseDeposit => format!(
            "weakest is {} — never re-verified, and warehouse copies do not refresh",
            deposit_phrase(named)
        ),
    };

    Some(format!(
        "coverage for unit \"{unit_name}\" rests on {} copies{composition}; {weakest_clause}",
        evidence.len()
    ))
}

/// The age half of a TAPE evidence line.
fn tape_detail(e: &CoverageEvidence, now: chrono::NaiveDateTime) -> String {
    match weakness(e, now) {
        Weakness::Never => "never verified".to_string(),
        Weakness::Age(days) => format!("last verified {days} days ago"),
        Weakness::Unparseable => format!(
            "last verified at {} (unparseable timestamp)",
            e.last_verified.as_deref().unwrap_or("?")
        ),
    }
}

/// A warehouse deposit named the way it must always be named: as a
/// DEPOSIT, of a specific volume, at a specific warehouse, on a specific
/// date. Never as a bare volume label — "rests on L6-0003" would send the
/// operator looking for a cartridge that is not the thing being described.
fn deposit_phrase(e: &CoverageEvidence) -> String {
    let at = match &e.location {
        Some(l) => format!(" at {l}"),
        None => String::new(),
    };
    let when = match &e.deposited_at {
        // Deposit stamps are `datetime('now')` ("YYYY-MM-DD HH:MM:SS");
        // the date alone is what the operator needs.
        Some(d) => format!(" ({})", d.split(' ').next().unwrap_or(d.as_str())),
        None => " (deposit date not recorded)".to_string(),
    };
    format!("a warehouse deposit of {}{at}{when}", e.volume_label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::parse_from_str("2026-07-30 12:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
    }

    fn ev(label: &str, last_verified: Option<&str>) -> CoverageEvidence {
        CoverageEvidence {
            kind: EvidenceKind::Tape,
            volume_label: label.to_string(),
            last_verified: last_verified.map(str::to_string),
            deposited_at: None,
            location: None,
        }
    }

    /// A warehouse-deposit evidence row.
    fn dep(label: &str, location: &str, deposited_at: &str) -> CoverageEvidence {
        CoverageEvidence {
            kind: EvidenceKind::WarehouseDeposit,
            volume_label: label.to_string(),
            last_verified: None,
            deposited_at: Some(deposited_at.to_string()),
            location: Some(location.to_string()),
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

    /// Fixture: one unit with completed writes on two volumes, `V1` and
    /// `V2`, with `V2` carrying a passed verification session.
    fn setup_two_volume_unit() -> (Connection, i64, i64, i64) {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('u1', 'unit1', ?1, 'mtime_size', 1, 'active')",
            params![tid],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
             VALUES (?1, 1, 'full', 'current', '/tmp/u1')",
            params![unit_id],
        )
        .unwrap();
        let snap_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 104857600)",
            params![snap_id],
        )
        .unwrap();
        let ss1_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 104857600)",
            params![snap_id],
        )
        .unwrap();
        let ss2_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('V1', 'lto', 'primary', 'LTO-6', 2500000000000, 'sealed')",
            [],
        )
        .unwrap();
        let v1_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('V2', 'lto', 'primary', 'LTO-6', 2500000000000, 'sealed')",
            [],
        )
        .unwrap();
        let v2_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'completed')",
            params![ss1_id, snap_id, v1_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'completed')",
            params![ss2_id, snap_id, v2_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO verification_sessions (volume_id, completed_at, outcome)
             VALUES (?1, '2020-01-01 00:00:00', 'passed')",
            params![v2_id],
        )
        .unwrap();

        (conn, unit_id, v1_id, v2_id)
    }

    /// `Some(v1)` excludes V1's own coverage row: only V2 remains. This is
    /// the defect #91's test was written to catch -- exclusion must
    /// actually exclude, not just fail to match a sentinel (issue #99).
    #[test]
    fn some_excludes_named_volume() {
        let (conn, unit_id, v1_id, _v2_id) = setup_two_volume_unit();
        let evidence = remaining_coverage_evidence(&conn, unit_id, Some(v1_id)).unwrap();
        let labels: Vec<&str> = evidence.iter().map(|e| e.volume_label.as_str()).collect();
        assert_eq!(labels, vec!["V2"], "V1 must be absent: {labels:?}");
    }

    /// `None` excludes nothing: both eligible volumes' coverage appears.
    #[test]
    fn none_excludes_nothing() {
        let (conn, unit_id, _v1_id, _v2_id) = setup_two_volume_unit();
        let evidence = remaining_coverage_evidence(&conn, unit_id, None).unwrap();
        let mut labels: Vec<&str> = evidence.iter().map(|e| e.volume_label.as_str()).collect();
        labels.sort();
        assert_eq!(
            labels,
            vec!["V1", "V2"],
            "None must exclude nothing: {labels:?}"
        );
    }

    /// ADR-0006's evidence class, rendered so that the line is TRUE READ
    /// ALONE. `cli::consent::confirm` prints each fact as a standalone
    /// line with zero surrounding context (issue #91's second lesson), so
    /// this string must not be mistakable for a verified cartridge: it
    /// says "warehouse deposit", it names the warehouse, it gives the
    /// deposit date, and it says outright that nothing has re-verified it
    /// and nothing ever will.
    #[test]
    fn a_lone_warehouse_deposit_never_reads_as_a_verified_tape() {
        let evidence = vec![dep("L6-0003", "glacier", "2026-01-02 03:04:05")];
        let line = describe("photos", &evidence, now()).unwrap();
        assert_eq!(
            line,
            "coverage for unit \"photos\" rests on a warehouse deposit of L6-0003 at glacier \
             (2026-01-02) — never re-verified, and warehouse copies do not refresh"
        );
        assert!(
            !line.contains("last verified"),
            "a deposit has never been verified; saying otherwise is the defect: {line}"
        );
        assert!(
            !line.contains("rests on L6-0003,"),
            "must not read as a bare cartridge the operator could go fetch: {line}"
        );
    }

    /// A mixed unit must state the composition, and must name the TAPE as
    /// the weakest rather than the deposit -- otherwise a never-verified
    /// cartridge vanishes from the only line the operator is guaranteed to
    /// read, which is the exact failure
    /// `multi_copy_line_never_claims_sole_dependence` exists to prevent.
    #[test]
    fn a_mixed_unit_states_the_composition_and_still_names_the_tape() {
        let evidence = vec![
            ev("L6-0001", None),
            dep("L6-0001", "glacier", "2026-01-02 03:04:05"),
        ];
        let line = describe("photos", &evidence, now()).unwrap();
        assert_eq!(
            line,
            "coverage for unit \"photos\" rests on 2 copies (1 tape, 1 warehouse deposit); \
             weakest is L6-0001, never verified"
        );
    }

    /// All copies in the warehouse and none on tape is the case that most
    /// needs saying out loud: ADR-0006 records that "a warehouse copy dies
    /// weeks after payment stops; tapes are the durable line".
    #[test]
    fn an_all_deposit_unit_says_there_are_no_tape_copies() {
        let evidence = vec![
            dep("L6-0003", "glacier", "2026-01-02 03:04:05"),
            dep("L6-0009", "deep-archive", "2025-06-01 00:00:00"),
        ];
        let line = describe("photos", &evidence, now()).unwrap();
        assert_eq!(
            line,
            "coverage for unit \"photos\" rests on 2 copies (no tape copies, 2 warehouse \
             deposits); weakest is a warehouse deposit of L6-0009 at deep-archive (2025-06-01) \
             — never re-verified, and warehouse copies do not refresh"
        );
    }

    /// An all-tape unit's wording must be BYTE-IDENTICAL to before this
    /// change: the composition clause is added only when a deposit exists.
    #[test]
    fn an_all_tape_unit_keeps_the_pre_existing_wording() {
        let evidence = vec![
            ev("L6-0001", Some("2026-07-29 12:00:00")),
            ev("L6-0002", None),
        ];
        assert_eq!(
            describe("photos", &evidence, now()).unwrap(),
            "coverage for unit \"photos\" rests on 2 copies; weakest is L6-0002, never verified"
        );
    }

    /// A unit whose only surviving coverage is a deposit must produce a
    /// line, not `None` -- `None` is the "no remaining coverage" signal at
    /// all three call sites, and rendering a covered unit that way is
    /// false and alarming.
    #[test]
    fn deposit_only_coverage_is_never_none() {
        let (conn, unit_id, vol) =
            crate::policy::coverage::tests::setup_unit_with_deposit("active");
        let evidence = remaining_coverage_evidence(&conn, unit_id, None).unwrap();
        let deposits_only: Vec<CoverageEvidence> = evidence
            .into_iter()
            .filter(|e| e.kind == EvidenceKind::WarehouseDeposit)
            .collect();
        assert_eq!(deposits_only.len(), 1);
        let line = describe("photos", &deposits_only, now())
            .expect("deposit-only coverage must describe itself, never render as none");
        assert!(
            line.contains("warehouse deposit of L6-0003 at glacier"),
            "{line}"
        );
        let _ = vol;
    }

    /// Exclusion still excludes, and it takes the excluded volume's
    /// deposit with it (deposits are gated on their source volume passing
    /// `coverage::eligible`, the same rule the copy counts apply).
    #[test]
    fn excluding_a_volume_also_drops_its_deposit() {
        let (conn, unit_id, vol) =
            crate::policy::coverage::tests::setup_unit_with_deposit("active");
        let evidence = remaining_coverage_evidence(&conn, unit_id, Some(vol)).unwrap();
        assert!(evidence.is_empty(), "{evidence:?}");
    }

    /// Issue #73 / ADR-0006: a recorded warehouse deposit is remaining
    /// coverage. A unit whose only surviving copy is a deposit must not
    /// render as "no remaining coverage" -- that is false, and at a
    /// destructive moment it is alarming in exactly the wrong direction.
    #[test]
    fn a_warehouse_deposit_is_remaining_coverage() {
        let (conn, unit_id, _vol) =
            crate::policy::coverage::tests::setup_unit_with_deposit("active");
        let evidence = remaining_coverage_evidence(&conn, unit_id, None).unwrap();
        assert_eq!(
            evidence.len(),
            2,
            "one sealed tape plus one recorded deposit is two pieces of evidence: {evidence:?}"
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
