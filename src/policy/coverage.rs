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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_the_predicate_against_the_given_alias() {
        assert_eq!(eligible("v"), "v.status = 'sealed'");
        assert_eq!(eligible("v2"), "v2.status = 'sealed'");
    }
}
