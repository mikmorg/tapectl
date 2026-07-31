//! ADR-0004 / CONTEXT.md "Copy": a unit's stage_set claim counts toward
//! coverage in derivations only while the volume holding it is sealed.
//! CONTEXT.md's **Sealed** entry states the rule directly: "Only sealed
//! volumes contribute claims to derivations."
//!
//! A write's own `status = 'completed'` is necessary but not sufficient.
//! It is set exactly once, at confirm time, in the same transaction that
//! flips `volumes.status` to `'sealed'` (`src/volume/session.rs`) — so
//! `completed` implies "this volume was sealed at write time." But
//! `volumes.status` keeps moving afterwards (`retired`, `quarantined`,
//! `erased`, and — schema-legal though no writer currently sets it —
//! `missing`; `src/db/migrations/003_v2_lifecycle.sql`), while the
//! `writes` row stays `completed` forever. A derivation that only checks
//! `writes.status` is checking eligibility as of write time, not as of
//! now — which is precisely the gap issue #89 closed.
//!
//! This module is the single source of truth for the re-qualification:
//! every copy-count derivation embeds [`eligible`] against its own
//! `volumes` alias rather than hand-writing `status = 'sealed'` itself.
//! Divergent hand-written copies of this condition are how the rule
//! drifted from the ADR in the first place — routing every call site
//! through one function is what keeps destructive gates (`unit
//! mark-tape-only`, `snapshot mark-reclaimable`, `volume retire`) and
//! advisory surfaces (`report copies`/`fire-risk`/`tape-only`, `audit`)
//! from ever disagreeing about what a copy is again.

/// The ADR-0004 eligibility predicate, rendered as a SQL boolean
/// expression against `{volume_alias}.status`.
///
/// Embed this directly in a `JOIN ... ON` condition or `WHERE` clause for
/// queries that use plain (inner) joins and an aggregate with no `GROUP
/// BY` — there every non-matching row is simply absent, and an aggregate
/// over zero rows still correctly returns a single row with count 0.
///
/// For a query that `LEFT JOIN`s `volumes` and `GROUP BY`s to preserve
/// units with zero writes (so they still report `copies = 0` rather than
/// vanishing from the result set), do NOT put this in the join's `ON`
/// clause — `COUNT(DISTINCT w.volume_id)` reads `w.volume_id` from the
/// `writes` row, which stays non-NULL even when the `volumes` join fails
/// to match, so the count would be unaffected and the fix would silently
/// do nothing. Instead wrap it in the aggregate itself:
/// `COUNT(DISTINCT CASE WHEN {predicate} THEN w.volume_id END)` — a
/// non-sealed row (or a row with no write at all) evaluates the `CASE` to
/// NULL, and `COUNT(DISTINCT ...)` / `GROUP_CONCAT(DISTINCT ...)` both
/// ignore NULLs, so the row survives (copies stays visibly 0) while a
/// non-sealed volume's write no longer counts.
pub fn eligible(volume_alias: &str) -> String {
    format!("{volume_alias}.status = 'sealed'")
}

/// The inventory predicate (issue #96), rendered as a SQL boolean
/// expression against `{volume_alias}.status`.
///
/// [`eligible`] and this function answer two DIFFERENT questions, and
/// conflating them is what issue #96 was:
///
/// - [`eligible`] — "does this volume contribute a COPY right now?" That
///   is a durability claim, so it is `sealed`-only (ADR-0004).
/// - `in_service` — "does this volume's PHYSICAL MEDIA count toward
///   inventory and capacity?" That is an accounting question, and the
///   answer includes legacy `full`: `docs/design/layout-session.md` reads
///   `full` as sealed-equivalent for pre-renovation volumes, so the
///   cartridge exists and holds bytes. Dropping it here would silently
///   under-report physical media — trading one under-report for another.
///
/// Both exclude `retired`/`erased`/`missing`/`quarantined`: media that is
/// gone, wiped, or untrusted is neither a copy nor live inventory.
///
/// Pass the table name (`"volumes"`) when the query has no alias.
pub fn in_service(volume_alias: &str) -> String {
    status_in(volume_alias, &["active", "full", "sealed"])
}

/// [`in_service`] widened to include `initialized` — media that is
/// provisioned but has not yet received bytes.
///
/// This exists as its own function rather than as an argument to
/// [`in_service`] so that nobody later "simplifies" the two into one and
/// silently drops `initialized`. It has exactly one caller: `report
/// capacity --per-volume`, whose whole job is to show the operator the
/// state of each cartridge — including a blank tape standing ready. The
/// aggregate totals deliberately do NOT use it: an initialized volume
/// contributes 0 bytes written, so folding it into the fleet-wide
/// utilization percentage would dilute that number with media nothing has
/// been asked to fill yet.
pub fn in_service_or_provisioned(volume_alias: &str) -> String {
    status_in(volume_alias, &["active", "full", "sealed", "initialized"])
}

// ── Deposit-aware copy / location derivations (issue #73, ADR-0006) ──

/// Which slice of a unit's coverage a derivation is asking about.
///
/// This is a TYPED scope, not a free-form SQL predicate, on purpose. The
/// generated expressions define their own `cw`/`css`/`cs`/`cv` aliases
/// internally; a caller-supplied predicate string mentioning `s` or `v`
/// would bind to whichever alias happened to be in scope, which is
/// correct only by luck and unreviewable at the call site. The three axes
/// below are the only ones that actually vary across the call sites.
#[derive(Debug, Clone, Copy)]
pub enum CoverageScope<'a> {
    /// All of a unit's coverage. `id_expr` is SQL evaluating to a unit id:
    /// a bound parameter (`"?1"`) for a standalone query, or an outer
    /// alias (`"u.id"`) when the expression is embedded as a correlated
    /// subquery in a per-unit report.
    ///
    /// `current_only` restricts to the unit's CURRENT snapshot. Most
    /// callers want that (`audit`, `unit mark-tape-only`, the reports);
    /// `volume retire`'s impact analysis deliberately does not, because it
    /// asks what coverage a unit has on ANY snapshot the retired cartridge
    /// participates in.
    Unit {
        id_expr: &'a str,
        current_only: bool,
    },
    /// One specific snapshot's coverage, by `stage_sets.snapshot_id`.
    /// `snapshot mark-reclaimable` is the only caller: it measures the
    /// SUPERSEDING snapshot, not the unit as a whole.
    Snapshot { id_expr: &'a str },
}

/// A coverage question: a [`CoverageScope`] plus an optional volume to
/// exclude by identity (SQL evaluating to a volume id, e.g. `"?1"`).
///
/// Exclusion means "pretend this volume is not there" — it removes the
/// volume's tape claim AND any deposit recorded FROM that volume, because
/// deposits are gated on the source volume passing [`eligible`]. See the
/// residual note in `docs/operator-guide.md`: a warehouse object outlives
/// the cartridge it was copied from, so this is the conservative reading,
/// not the only defensible one.
#[derive(Debug, Clone, Copy)]
pub struct CoverageQuery<'a> {
    pub scope: CoverageScope<'a>,
    pub exclude_volume: Option<&'a str>,
}

impl<'a> CoverageQuery<'a> {
    /// A unit's CURRENT-snapshot coverage, nothing excluded — the common case.
    pub fn current_unit(id_expr: &'a str) -> Self {
        Self {
            scope: CoverageScope::Unit {
                id_expr,
                current_only: true,
            },
            exclude_volume: None,
        }
    }
}

/// The eligible-writes subquery every expression below is built on:
/// completed writes, on volumes that pass [`eligible`] RIGHT NOW, inside
/// the requested scope. Selects `{projection}` from the aliases
/// `cw` (writes), `css` (stage_sets), `cs` (snapshots), `cv` (volumes).
fn eligible_writes(q: &CoverageQuery, projection: &str) -> String {
    let scope = match q.scope {
        CoverageScope::Unit {
            id_expr,
            current_only,
        } => {
            let current = if current_only {
                " AND cs.status = 'current'"
            } else {
                ""
            };
            format!("cs.unit_id = {id_expr}{current}")
        }
        CoverageScope::Snapshot { id_expr } => format!("css.snapshot_id = {id_expr}"),
    };
    let exclude = match q.exclude_volume {
        Some(expr) => format!(" AND cw.volume_id != {expr}"),
        None => String::new(),
    };
    let eligible_cv = eligible("cv");
    format!(
        "SELECT {projection}
         FROM writes cw
         JOIN stage_sets css ON css.id = cw.stage_set_id
         JOIN snapshots cs ON cs.id = css.snapshot_id
         JOIN volumes cv ON cv.id = cw.volume_id
         WHERE cw.status = 'completed' AND {eligible_cv} AND {scope}{exclude}"
    )
}

/// The deposits of the volumes in scope: every recorded warehouse deposit
/// (ADR-0006) whose source volume is currently eligible coverage.
fn scoped_deposits(q: &CoverageQuery, projection: &str) -> String {
    format!(
        "SELECT {projection} FROM volume_deposits cd
         WHERE cd.volume_id IN ({})",
        eligible_writes(q, "cw.volume_id")
    )
}

/// **The** copy-count expression: a parenthesised SQL scalar subquery
/// counting a unit's (or snapshot's) copies, warehouse deposits included.
///
/// A copy is one of two things and the count is their UNION:
/// - a distinct eligible VOLUME carrying a completed write, and
/// - a recorded warehouse DEPOSIT of such a volume (ADR-0006: "the catalog
///   must claim them to reason about them" — the rejected alternative,
///   "cloud as external practice only with no model presence", is exactly
///   the under-count this expression exists to prevent).
///
/// A deposit's WEAKER EVIDENCE is not expressed here. It is surfaced by
/// [`crate::policy::evidence`] at destructive moments and never by
/// excluding the deposit from a count (ADR-0004: advisory, displayed,
/// never gating).
///
/// **Do not inline this.** Six sites previously hand-wrote their own copy
/// and location counts; that divergence is how issue #96 happened, and it
/// is why [`eligible`] exists as a function at all. A seventh hand-written
/// copy will under-count warehouse deposits silently — nothing fails, the
/// operator is simply told to buy a tape they already have a copy on.
pub fn copy_count_expr(q: &CoverageQuery) -> String {
    format!(
        "(SELECT COUNT(*) FROM (
            {}
            UNION
            {}
        ))",
        eligible_writes(q, "'v' || cw.volume_id"),
        scoped_deposits(q, "'d' || cd.id"),
    )
}

/// **The** distinct-location expression, warehouse deposits included.
///
/// The counted set is the UNION of the eligible volumes' non-NULL
/// `location_id`s and the deposits' `location_id`s. `UNION` (not two added
/// counts) because a location could in principle appear on both sides, and
/// a doubled location count would silently satisfy a `required_locations`
/// policy that is not actually met. `IS NOT NULL` is explicit because
/// `UNION` treats NULL as a value and would count "no location recorded"
/// as a location.
///
/// Same warning as [`copy_count_expr`]: one expression, N call sites.
pub fn location_count_expr(q: &CoverageQuery) -> String {
    format!(
        "(SELECT COUNT(*) FROM (
            {} AND cv.location_id IS NOT NULL
            UNION
            {}
        ))",
        eligible_writes(q, "cv.location_id"),
        scoped_deposits(q, "cd.location_id"),
    )
}

/// How many of the copies counted by [`copy_count_expr`] are warehouse
/// deposits. Purely for DISPLAY: the advisory surfaces print it beside the
/// total so a warehouse copy is visibly distinguishable rather than
/// silently folded into a number that looks like tapes on a shelf.
pub fn deposit_count_expr(q: &CoverageQuery) -> String {
    format!("(SELECT COUNT(*) FROM ({}))", scoped_deposits(q, "cd.id"))
}

/// `{alias}.status IN ('a','b',...)`. The single place a status list is
/// turned into SQL, so the quoting is written once.
fn status_in(volume_alias: &str, statuses: &[&str]) -> String {
    let list = statuses
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{volume_alias}.status IN ({list})")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rusqlite::{params, Connection};

    /// Issue #73 / ADR-0006 fixture, shared by every call site that derives
    /// copies or locations: one active unit `photos` with one CURRENT
    /// snapshot written to one SEALED volume `L6-0003` shelved at `home`,
    /// plus a recorded warehouse DEPOSIT of that same volume at the
    /// warehouse location `glacier`.
    ///
    /// The whole point is that the tape half alone yields 1 copy / 1
    /// location: any derivation that ignores deposits under-counts this
    /// unit, which is exactly the defect the shared expressions exist to
    /// prevent.
    ///
    /// Returns `(conn, unit_id, volume_id)`.
    pub(crate) fn setup_unit_with_deposit(unit_status: &str) -> (Connection, i64, i64) {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('u-photos', 'photos', ?1, 'mtime_size', 1, ?2)",
            params![tid, unit_status],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO locations (name, kind) VALUES ('home', 'shelf')",
            [],
        )
        .unwrap();
        let home_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO locations (name, kind) VALUES ('glacier', 'warehouse')",
            [],
        )
        .unwrap();
        let glacier_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status, location_id)
             VALUES ('L6-0003', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed', ?1)",
            params![home_id],
        )
        .unwrap();
        let vol_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
             VALUES (?1, 1, 'full', 'current', '/tmp/photos')",
            params![unit_id],
        )
        .unwrap();
        let snap_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size, num_slices)
             VALUES (?1, 'staged', 104857600, 1)",
            params![snap_id],
        )
        .unwrap();
        let ss_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                       sha256_plain, sha256_encrypted)
             VALUES (?1, 1, 1000, 1000, 'a', 'b')",
            params![ss_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'completed')",
            params![ss_id, snap_id, vol_id],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO volume_deposits (volume_id, location_id, deposited_at, receipt)
             VALUES (?1, ?2, '2026-01-02 00:00:00', 'rcpt-1')",
            params![vol_id, glacier_id],
        )
        .unwrap();

        (conn, unit_id, vol_id)
    }

    fn scalar(conn: &Connection, expr: &str, unit_id: i64) -> i64 {
        conn.query_row(&format!("SELECT {expr}"), params![unit_id], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn a_deposit_counts_as_a_copy_and_as_a_location() {
        let (conn, unit_id, _vol) = setup_unit_with_deposit("active");
        let q = CoverageQuery::current_unit("?1");
        assert_eq!(scalar(&conn, &copy_count_expr(&q), unit_id), 2);
        assert_eq!(scalar(&conn, &location_count_expr(&q), unit_id), 2);
        assert_eq!(scalar(&conn, &deposit_count_expr(&q), unit_id), 1);
    }

    /// A deposit is gated on its SOURCE VOLUME still being eligible: once
    /// the tape is quarantined the unit has no eligible coverage at all,
    /// and the deposit must not keep the count alive on its own. (Pinned
    /// deliberately — see the residual note in the operator guide: the
    /// warehouse object physically outlives the cartridge, so this is the
    /// conservative reading and a future ADR could revisit it.)
    #[test]
    fn a_deposit_of_an_ineligible_volume_does_not_count() {
        let (conn, unit_id, vol) = setup_unit_with_deposit("active");
        conn.execute(
            "UPDATE volumes SET status = 'quarantined' WHERE id = ?1",
            params![vol],
        )
        .unwrap();
        let q = CoverageQuery::current_unit("?1");
        assert_eq!(scalar(&conn, &copy_count_expr(&q), unit_id), 0);
        assert_eq!(scalar(&conn, &location_count_expr(&q), unit_id), 0);
    }

    /// The location set is a UNION, not a sum. If a deposit were somehow
    /// recorded at the very location the cartridge is shelved at, that is
    /// ONE place, and adding two counts would silently satisfy a
    /// two-location policy that is not met.
    #[test]
    fn location_count_unions_rather_than_adds() {
        let (conn, unit_id, vol) = setup_unit_with_deposit("active");
        let home: i64 = conn
            .query_row("SELECT id FROM locations WHERE name = 'home'", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute(
            "UPDATE volume_deposits SET location_id = ?1 WHERE volume_id = ?2",
            params![home, vol],
        )
        .unwrap();
        let q = CoverageQuery::current_unit("?1");
        assert_eq!(
            scalar(&conn, &location_count_expr(&q), unit_id),
            1,
            "one physical place is one location"
        );
        assert_eq!(
            scalar(&conn, &copy_count_expr(&q), unit_id),
            2,
            "but it is still two copies"
        );
    }

    /// A volume with no `location_id` recorded contributes NO location.
    /// `UNION` treats NULL as a value, so the `IS NOT NULL` filter in
    /// `location_count_expr` is load-bearing, not decorative.
    #[test]
    fn a_volume_without_a_location_contributes_no_location() {
        let (conn, unit_id, _vol) = setup_unit_with_deposit("active");
        conn.execute("UPDATE volumes SET location_id = NULL", [])
            .unwrap();
        let q = CoverageQuery::current_unit("?1");
        assert_eq!(
            scalar(&conn, &location_count_expr(&q), unit_id),
            1,
            "only the warehouse deposit's location is known"
        );
    }

    /// Two stage_sets of one snapshot written to the SAME cartridge is one
    /// copy. Copies are distinct volumes everywhere (this is the shape
    /// `unit mark-tape-only` used to get wrong with COUNT(DISTINCT w.id)).
    #[test]
    fn two_writes_to_one_volume_are_one_copy() {
        let (conn, unit_id, vol) = setup_unit_with_deposit("active");
        conn.execute("DELETE FROM volume_deposits", []).unwrap();
        let (snap_id, ss_id): (i64, i64) = conn
            .query_row(
                "SELECT snapshot_id, stage_set_id FROM writes LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let _ = ss_id;
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size)
             VALUES (?1, 'staged', 104857600)",
            params![snap_id],
        )
        .unwrap();
        let ss2 = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'completed')",
            params![ss2, snap_id, vol],
        )
        .unwrap();
        let q = CoverageQuery::current_unit("?1");
        assert_eq!(scalar(&conn, &copy_count_expr(&q), unit_id), 1);
    }

    #[test]
    fn renders_the_predicate_against_the_given_alias() {
        assert_eq!(eligible("v"), "v.status = 'sealed'");
        assert_eq!(eligible("v2"), "v2.status = 'sealed'");
    }

    #[test]
    fn in_service_keeps_legacy_full_and_adds_sealed() {
        assert_eq!(
            in_service("v"),
            "v.status IN ('active','full','sealed')",
            "dropping legacy 'full' would under-report physical media"
        );
    }

    #[test]
    fn in_service_takes_a_bare_table_name_for_unaliased_queries() {
        assert_eq!(
            in_service("volumes"),
            "volumes.status IN ('active','full','sealed')"
        );
    }

    #[test]
    fn in_service_or_provisioned_adds_initialized_and_nothing_else() {
        assert_eq!(
            in_service_or_provisioned("volumes"),
            "volumes.status IN ('active','full','sealed','initialized')"
        );
    }

    /// The two predicates must never converge: `eligible` is a durability
    /// claim, `in_service` an inventory one (issue #96).
    #[test]
    fn eligible_and_in_service_stay_distinct() {
        assert_ne!(eligible("v"), in_service("v"));
        assert!(!in_service("v").contains("= 'sealed'"));
    }
}
