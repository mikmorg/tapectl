//! ADR-0004 / CONTEXT.md "Copy": a unit's stage_set claim counts toward
//! coverage in derivations only while the volume holding it is sealed.
//! CONTEXT.md's **Sealed** entry states the rule directly: "Only sealed
//! volumes contribute claims to derivations."
//!
//! A write's own `status = 'completed'` is necessary but not sufficient.
//! It is set exactly once, at confirm time, in the same transaction that
//! flips `volumes.status` to `'sealed'` (`src/volume/session.rs`) — so
//! `completed` implies "this volume was sealed at write time." But
//! `volumes.status` keeps moving afterwards (`retired`, `erased`;
//! `src/db/migrations/026_drop_unwritten_states.sql` is the live set) —
//! `quarantined` is NOT among
//! them: ADR-0012's 2026-09-17 amendment (issue #242) moved it to
//! `volumes.observed_condition`, a separate column [`eligible`] also
//! consults (see [`condition_ok`]) — while the
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

use rusqlite::{params, Connection};

/// The ADR-0004 eligibility predicate, rendered as a SQL boolean
/// expression against `{volume_alias}.status` AND
/// `{volume_alias}.observed_condition` (ADR-0012's 2026-09-17 amendment
/// folded the latter in — see [`condition_ok`]).
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
    format!(
        "({alias}.status = 'sealed' AND {cond})",
        alias = volume_alias,
        cond = condition_ok(volume_alias)
    )
}

/// The condition half of every occupancy/eligibility predicate in this
/// module (ADR-0012's 2026-09-17 amendment, "the status column is the
/// operator's; a medium's condition is its own fact", issue #242):
/// `true` only when tapectl has not observed a reason to distrust this
/// volume's medium (`observed_condition = 'ok'`).
///
/// Before this amendment, quarantine worked by moving `status` OFF
/// `sealed` — so [`eligible`] alone used to be sufficient. Now that the
/// four quarantine writers (three in `volume::session`, one in
/// `volume::write`'s `quarantine_on_medium_evidence`) target
/// `observed_condition` instead and leave `status` untouched, `eligible`
/// must consult BOTH columns or a quarantine silently stops reducing the
/// copy count — the exact coverage-misstatement class issue #153 was.
/// [`in_service`] and [`in_service_or_provisioned`] need the same half for
/// the same reason: "untrusted" media was always excluded from both
/// (see their own doc comments), and that exclusion used to ride on the
/// same `status` move.
///
/// Never inlined outside this module — [`eligible`]'s own doc already
/// states the rule this function exists to enforce: `coverage.rs` is the
/// declared sole owner of every `volumes.status`/`observed_condition`
/// predicate (issue #96).
fn condition_ok(volume_alias: &str) -> String {
    format!("{volume_alias}.observed_condition = 'ok'")
}

/// The inventory predicate (issue #96), rendered as a SQL boolean
/// expression against `{volume_alias}.status` AND
/// `{volume_alias}.observed_condition` (see [`condition_ok`]).
///
/// [`eligible`] and this function answer two DIFFERENT questions, and
/// conflating them is what issue #96 was:
///
/// - [`eligible`] — "does this volume contribute a COPY right now?" That
///   is a durability claim, so it is `sealed`-only (ADR-0004).
/// - `in_service` — "does this volume's PHYSICAL MEDIA count toward
///   inventory and capacity?" That is an accounting question, and the
///   answer includes `active` (a tape `volume import` describes, written
///   elsewhere): the cartridge exists and holds bytes. Dropping it here
///   would silently under-report physical media — trading one
///   under-report for another. (`full` was in this list as a
///   "pre-renovation" sealed-equivalent until migration 032 dropped it:
///   nothing ever wrote it — ADR-0012 amendment 2026-10-07 item 16.)
///
/// Both exclude `retired`/`erased`/`quarantined`: media that is
/// gone, wiped, or untrusted is neither a copy nor live inventory. Since
/// issue #242, "quarantined" is a value of `observed_condition`, not
/// `status` — [`condition_ok`] carries that exclusion now, alongside the
/// status list.
///
/// Pass the table name (`"volumes"`) when the query has no alias.
pub fn in_service(volume_alias: &str) -> String {
    format!(
        "({} AND {})",
        status_in(volume_alias, &["active", "sealed"]),
        condition_ok(volume_alias)
    )
}

/// [`in_service`] widened to include `initialized` — media that is
/// provisioned but has not yet received bytes.
///
/// This exists as its own function rather than as an argument to
/// [`in_service`] so that nobody later "simplifies" the two into one and
/// silently drops `initialized`. Its callers: `report capacity
/// --per-volume`, whose whole job is to show the operator the state of each
/// cartridge — including a blank tape standing ready; and the TapeAlert
/// paths (`audit`'s `tape_alert`, `drive poll`'s "the catalog has L6-0001
/// on it"), which must not drop the `initialized` volume an interrupted
/// write left — the very tape `volume resume` will write to next (issue
/// #308). The aggregate totals deliberately do NOT use it: an initialized volume
/// contributes 0 bytes written, so folding it into the fleet-wide
/// utilization percentage would dilute that number with media nothing has
/// been asked to fill yet.
pub fn in_service_or_provisioned(volume_alias: &str) -> String {
    format!(
        "({} AND {})",
        status_in(volume_alias, &["active", "sealed", "initialized"]),
        condition_ok(volume_alias)
    )
}

/// ADR-0012: the one status `volume write`/`volume resume` may target.
///
/// Deliberately narrower than [`in_service_or_provisioned`], which folds in
/// `active`/`sealed` for inventory/capacity accounting — neither is a
/// write target. `active` is written only by `tapectl import` (a tape
/// written elsewhere, not a v2 write session). `blank`/`missing` (migration
/// 026) and `full` (032), which never had a writer, left the schema. A
/// `sealed` volume is never written again (ADR-0003), and
/// `retired`/`erased` are catalog facts that make writing wrong regardless
/// of what the loaded tape looks like. `quarantined` is NOT a `status`
/// value (ADR-0012's 2026-09-17 amendment, issue #242) — it is the
/// `condition` parameter's job, below, to refuse that case.
///
/// The "interrupted, ready to resume" state is NOT a `volumes.status`
/// value — it lives in `writes.status` (`planned → in_progress →
/// interrupted`), so `volume_resume` targets exactly the same status as
/// `volume_write`: the row never leaves `initialized` between `plan` and
/// `confirm`.
///
/// This is one third of the write-target check. [`has_completed_write`] is
/// the second (ADR-0012's 2026-09-16 amendment, issue #199) — see its doc
/// for why a status alone stopped being sufficient. The third is the
/// `condition` parameter itself (ADR-0012's 2026-09-17 amendment, "the
/// status column is the operator's; a medium's condition is its own fact",
/// issue #242):
///
/// A write-path quarantine (`volume::session`'s three writers) used to move
/// `volumes.status` OFF `initialized` as a side effect of recording the
/// quarantine, so a quarantined volume automatically failed this predicate
/// without the predicate ever having to know why. Now that quarantine
/// writes `observed_condition` instead and leaves `status` alone, that side
/// effect is gone — a volume a write-time contact check quarantined
/// mid-session would silently become a legal write target again the moment
/// nothing else changed its status. This predicate checks BOTH columns
/// directly so that regression cannot reopen.
pub fn is_write_target(status: &str, condition: &str) -> bool {
    status == "initialized" && condition == "ok"
}

/// ADR-0012, amendment 2026-09-16 (issue #199): the second half of the
/// write-target check, alongside [`is_write_target`]'s status test.
/// Answers a fact `volumes.status` cannot: whether this volume ALREADY
/// has bytes tapectl knows about, regardless of what its status says. See
/// the dated amendment in
/// `docs/adr/0012-copies-are-identical-content-cartridges-are-known-by-serial.md`,
/// "The write target."
///
/// `catalog rebuild --from-volume` can attach a rebuilt row's contents to
/// a pre-existing `initialized` row — `rebuild::ensure_write` inserts its
/// `writes` row as `status = 'completed'` when the tape's seal marker was
/// read (a tape without one gets `'aborted'`, #458) — without ever
/// moving that row off `initialized`: #158 deliberately never overwrites
/// an existing row's status, on the grounds that overwriting an
/// operator's `quarantined` finding would destroy a fact a failed
/// `volume verify` established. So a rebuilt-but-never-verified row could
/// pass [`is_write_target`]'s status test alone while already holding a
/// whole tape's worth of attached content — this closes that gap.
///
/// `'completed'` counts, and so does `'cut'` (#420; ADR-0012 item 112): a cut
/// write is on a sealed volume whose tape ended early, and its slices that
/// reached the tape are on it. It is never a copy, but the volume holds bytes
/// tapectl knows about and is no write target, whatever its status says (a
/// rebuild or an adoption can leave a short tape's rows on an `initialized`
/// row exactly as it leaves `completed` ones). So does `'range'` (#478;
/// ADR-0012 item 121): the run of a spannable set's slices that the early
/// warning ended on this tape. A range is on the tape, so a volume holding
/// one holds known bytes and is no write target, for the same reason as a
/// cut. `session::SealedPending::confirm` moves
/// every `writes` row of a session to `completed` in the SAME transaction
/// that flips `volumes.status` to `sealed`, so in NORMAL operation
/// `completed` and `initialized` never coexist on one volume — only the
/// rebuild path above (or direct DB tampering) produces it, which is
/// exactly the gap this closes. `planned`/`in_progress`/`interrupted`
/// rows do NOT count, on purpose: `volume resume` exists to continue
/// exactly those, and `volume_write`'s own separate "unresolved write
/// session" check (immediately after this one, in `write.rs`) already
/// refuses them for a *fresh* write with a message that points at
/// `volume resume` — counting them here too would only produce a worse
/// message for that case, not a safer one. `aborted`/`failed` rows do not
/// count either: `docs/design/layout-session.md` is explicit that an
/// aborted session's tape "is not a copy".
///
/// `write_positions`/`stage_slices` rows are deliberately NOT consulted:
/// they carry no signal beyond `writes` here. `write_positions` reaches a
/// volume only through `writes.id` (it has no `volume_id` of its own), and
/// `stage_slices` has no `volume_id` at all; `rebuild::ensure_write`
/// inserts its `writes` row (`completed` for a sealed tape, `aborted`
/// otherwise, #458) in the very same transaction as
/// any slice/position row it attaches (`rebuild.rs`'s `insert_all`), so a
/// `writes` row is never absent while a slice attributable to this volume
/// is present. Joining either table would duplicate a fact this query
/// already has, not add one.
pub fn has_completed_write(conn: &Connection, volume_id: i64) -> crate::error::Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM writes
                       WHERE volume_id = ?1 AND status IN ('completed', 'cut', 'range'))",
        params![volume_id],
        |row| row.get(0),
    )?)
}

/// An in-service ([`in_service`]) volume already holding a completed write
/// of `unit_id`'s snapshot `version`, if any — the source `volume
/// read-slices --from` can pull the identical ciphertext back from without
/// touching the unit's source directory. Asked by `audit`'s copy remedies
/// and by `volume write`'s refusal of an incomplete stage set (issue #402).
///
/// [`in_service_copy_of_version_excluding`] with nothing excluded.
pub fn in_service_copy_of_version(
    conn: &Connection,
    unit_id: i64,
    version: i64,
) -> crate::error::Result<Option<String>> {
    in_service_copy_of_version_excluding(conn, unit_id, version, None)
}

/// [`in_service_copy_of_version`], never naming `exclude_volume_id` (issue
/// #474): a volume that has just failed to give up a slice (`compact-read`
/// skipped one that did not match its hash) is still in service, and the
/// newest copy — which is the order this answers in — so naming it as the
/// place to read the version back from sends the operator to the slice that
/// just failed.
pub fn in_service_copy_of_version_excluding(
    conn: &Connection,
    unit_id: i64,
    version: i64,
    exclude_volume_id: Option<i64>,
) -> crate::error::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        &format!(
            "SELECT v.label FROM writes w
             JOIN stage_sets ss ON ss.id = w.stage_set_id
             JOIN snapshots s ON s.id = ss.snapshot_id
             JOIN volumes v ON v.id = w.volume_id
             WHERE s.unit_id = ?1 AND s.version = ?2 AND w.status = 'completed' AND {}
               AND (?3 IS NULL OR v.id <> ?3)
             ORDER BY w.id DESC LIMIT 1",
            in_service("v")
        ),
        params![unit_id, version, exclude_volume_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

// ── The retire family's zero-copy floor sees a sealed tape (issue #276) ──

/// A THIRD question, distinct from both [`eligible`] ("is this a copy
/// right now?") and [`in_service`] ("does this count as inventory?"):
/// does this volume PHYSICALLY hold restorable bytes right now?
///
/// A strict SUPERSET of [`eligible`], widened by exactly one state:
/// sealed-but-unconfirmed — `sealed_at` set (migration 018) while `status`
/// is still `'initialized'`. That is migration 018's case (b): `seal()`
/// succeeded (the seal marker binding the front index is physically on the
/// tape) but `SealedPending::confirm` has not yet passed — an `Inconclusive`
/// confirm, or a crash between `seal()` returning and `confirm` running.
/// Migration 018's header names both states verbatim; read it before
/// touching this function again.
///
/// `{alias}.sealed_at IS NOT NULL` is the line, and it is drawn there
/// deliberately: an execute that finished WITHOUT a successful `seal()`
/// (migration 018's case (a) — interrupted between execute and seal) leaves
/// a tape with slices on it but no seal marker binding the front index, and
/// `docs/design/layout-session.md` is explicit that such a tape "is not a
/// copy" — `sealed_at` unset correctly leaves that case excluded.
/// `sealed_at` SET means the seal marker itself is on the tape, which is
/// exactly the fact this predicate exists to recognise.
///
/// Still consults [`condition_ok`] — this is the deliberate escape hatch
/// ADR-0012 requires: an operator retiring a tape BECAUSE it no longer
/// reads says so with `volume verify`, which quarantines
/// (`observed_condition = 'quarantined'`), and a quarantined volume is then
/// Tier 2 at most, exactly as it is for [`eligible`].
///
/// The "not gone" half is written as an EXCLUSION of
/// `retired`/`erased` ([`status_not_in`]), not an inclusion list,
/// so a future `volumes.status` value cannot silently drop out of the
/// Tier-3 floor by omission — the legal set since migration 032 is
/// `initialized`, `active`, `retired`, `erased`, `sealed`
/// (`032_drop_volume_status_full.sql`).
///
/// **Must NOT be used for copy counting.** An unconfirmed volume is not a
/// Copy (ADR-0004) and must not contribute to `min_copies`,
/// [`copy_count_expr`], [`location_count_expr`], `report copies`, `audit`,
/// or `unit mark-tape-only` — only [`versions_at_stake`] (the retire
/// family's Tier-3 floor) consults this function, and deliberately does not
/// let it anywhere near `copies_after`/`locations_after`.
pub fn holds_sealed_bytes(volume_alias: &str) -> String {
    format!(
        "(({alias}.status = 'sealed' OR {alias}.sealed_at IS NOT NULL) AND {not_gone} AND {cond})",
        alias = volume_alias,
        not_gone = status_not_in(volume_alias, &["retired", "erased"]),
        cond = condition_ok(volume_alias)
    )
}

/// "Does this volume hold bytes a verify could check?" -- the question
/// `report verify-status` asks before it synthesises a "never verified"
/// line for a volume with no verification session (issue #336).
///
/// Bytes: `active` (imported), `sealed`, or a recorded seal on a volume
/// whose confirm never landed (`sealed_at` set while `initialized` -- the
/// state [`holds_sealed_bytes`] also widens to). Not gone: `retired`
/// and `erased` media is not verified. Unlike
/// [`holds_sealed_bytes`] this does NOT exclude a quarantined condition: a
/// quarantined volume is exactly one to verify, since a clean full verify is
/// what clears it (ADR-0012, 2026-09-17). `initialized` without a
/// recorded seal holds nothing to verify.
pub fn holds_bytes_to_verify(volume_alias: &str) -> String {
    format!(
        "(({in_bytes} OR {alias}.sealed_at IS NOT NULL) AND {not_gone})",
        in_bytes = status_in(volume_alias, &["active", "sealed"]),
        alias = volume_alias,
        not_gone = status_not_in(volume_alias, &["retired", "erased"]),
    )
}

/// A write's status set that means "this write's bytes are physically ON
/// the tape right now", regardless of whether confirm has yet recorded an
/// outcome for it (issue #276).
///
/// `completed` is confirm's `passed` branch. `in_progress` is the window
/// `volume::write::finish_session` occupies between `seal()` returning
/// (where `sealed_at` is recorded) and `SealedPending::confirm` landing an
/// outcome — `writes.status` only moves to `completed` inside confirm's
/// `passed` branch, so a row sits `in_progress` for the whole of that
/// window. A crash in exactly that window is swept to `interrupted` by
/// `db::open`'s `recover_orphaned_sessions`. All three names are the SAME
/// physical fact: bytes `seal()`ed onto tape whose confirm outcome the
/// catalog has not (yet, or ever again) recorded either way. `aborted`
/// (confirm's proves-medium-bad branch — already excluded downstream by
/// [`condition_ok`]/[`holds_sealed_bytes`]) and `planned` (never reached
/// the tape) are deliberately excluded.
///
/// `range` (#478; ADR-0012 item 121) is confirm's outcome for a spannable
/// set the early warning ended inside: its written slices are on this tape,
/// and for some of them this may be the only tape. Item 121 puts it here so
/// the retire floor sees it, and `volume rewrite`'s consent with it, since
/// that reads [`versions_at_stake`] through `cli::operations::retire_impacts`.
/// What a range is a copy OF is item 127's per-slice count, not this
/// predicate's question.
///
/// One `pub fn`, not a hand-copy at each call site, for the reason
/// [`status_in`] itself exists: [`versions_at_stake`] (the gate) and
/// `cli::operations::retire_impacts` (its own display) must never drift on
/// what "physically on tape" means — issue #276's own requirement is that
/// the display is never narrower than the gate.
pub fn write_reaches_tape(write_alias: &str) -> String {
    status_in(
        write_alias,
        &["completed", "range", "in_progress", "interrupted"],
    )
}

/// True when `volume_label` is sealed-but-unconfirmed right now: the seal
/// marker is physically on the tape (`sealed_at IS NOT NULL`) but confirm
/// has not (yet, or ever again) flipped `status` to `'sealed'` (issue
/// #276). Owned here rather than in `cli::operations`, per this module's
/// header: `coverage.rs` is the declared sole owner of every
/// `volumes.status` predicate (issue #96).
///
/// Purely a MESSAGING helper — `cli::operations::refuse_last_eligible_copy`
/// uses it to decide whether to name `tapectl volume resume <label>` as the
/// cheapest first act in its refusal text. It answers a narrower question
/// than [`holds_sealed_bytes`] (which also excludes
/// `retired`/`erased` and consults [`condition_ok`]): this
/// function does not need those exclusions, because by the time
/// `refuse_last_eligible_copy` runs, `volume_label` is already the specific
/// volume the operator is trying to retire, not a candidate being filtered
/// from a set. Concretely: `refuse_last_eligible_copy` only reaches this
/// call when `volume_label` produced an `at_stake` row in the first place,
/// which means [`holds_sealed_bytes`] already admitted it — so
/// [`condition_ok`] is already established for it and this predicate does
/// not need to re-check it.
pub fn is_sealed_but_unconfirmed(
    conn: &Connection,
    volume_label: &str,
) -> crate::error::Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM volumes \
         WHERE label = ?1 AND status <> 'sealed' AND sealed_at IS NOT NULL)",
        params![volume_label],
        |row| row.get(0),
    )?)
}

/// "Has the catalog given this volume up?" — the one test `cartridge erase`
/// asks of a volume it finds on the tape in the drive, by File 0's uuid or by
/// the chip serial's open mount (ADR-0012 2026-10-12 item 51: a volume the
/// catalog counts as a live copy is refused, and `volume retire` stays the
/// only way to give one up).
///
/// `retired` or `erased`, and nothing else. Deliberately NOT the negation of
/// [`eligible`]: a quarantined sealed volume, an `active` import, an
/// `initialized` volume mid-write and a sealed-but-unconfirmed one are none
/// of them copies, and every one of them is still a volume the catalog has
/// not let go of — its bytes are the evidence a verify, a resume or a
/// salvage would read. Erasing one would make the catalog describe a tape
/// that no longer exists without anyone having said so; `volume retire` is
/// where that is said, with its own Tier-3 floor.
pub fn given_up(volume_alias: &str) -> String {
    status_in(volume_alias, &["retired", "erased"])
}

/// [`given_up`] for one volume by id.
pub fn is_given_up(conn: &Connection, volume_id: i64) -> crate::error::Result<bool> {
    Ok(conn.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM volumes WHERE id = ?1 AND {})",
            given_up("volumes")
        ),
        params![volume_id],
        |row| row.get(0),
    )?)
}

/// True when `volume_label` counts as a Copy RIGHT NOW (issue #280) —
/// exactly [`eligible`], evaluated for one volume by label rather than
/// embedded in a larger `JOIN`/aggregate. Built directly from `eligible`'s
/// own expression, never a second hand-written `status = 'sealed' AND ...`
/// — that duplication is how issue #96 happened, and this module's header
/// names `coverage.rs` the sole owner of every `volumes.status` predicate
/// for exactly that reason.
///
/// Exists so `volume verify`'s clean-clear message (the "RETURNED TO
/// SERVICE" text and its `--json` twin) can say whether the volume it just
/// cleared `observed_condition` on actually rejoins the coverage count,
/// without that claim ever being able to drift from what `audit`/`report
/// copies`/every `min_copies` gate would count for the same volume. Before
/// this existed, the clean-clear message asserted "it counts as a copy
/// again" unconditionally — true only while `status = 'sealed'`, and
/// silently false for a `retired` volume (salvaged after quarantine, then
/// deliberately retired) or one stuck `initialized` after a confirm that
/// never reached the sealing `UPDATE` (`session.rs`'s `proves_medium_bad`
/// arm) — both real, reachable states where "returned to service" is a lie
/// an operator would read as a durability guarantee.
///
/// A clean clear only ever sets `observed_condition = 'ok'`
/// (`clear_condition_on_clean_full_verify`), so after one runs this reduces
/// to "is `status = 'sealed'`" — but the call site must not hand-derive
/// that either; it asks this function, exactly as every other copy-counting
/// call site asks [`eligible`].
pub fn counts_as_copy(conn: &Connection, volume_label: &str) -> crate::error::Result<bool> {
    Ok(conn.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM volumes WHERE label = ?1 AND {})",
            eligible("volumes")
        ),
        params![volume_label],
        |row| row.get(0),
    )?)
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
    /// `current_only` restricts to the unit's CURRENT snapshots — plural:
    /// `snapshots.status` permits more than one `'current'` row per unit
    /// at once (issue #153), and `copy_count_expr`/`location_count_expr`
    /// take the MIN across all of them, not their union. Most callers
    /// want that (`audit`, `unit mark-tape-only`, the reports);
    /// `volume retire`'s impact analysis deliberately does not, because it
    /// asks what coverage a unit has on ANY snapshot the retired cartridge
    /// participates in — which is what it DISPLAYS. What it GATES on is a
    /// third thing again: [`versions_at_stake`], one row per current
    /// version the retirement actually removes coverage from (issue #147).
    Unit {
        id_expr: &'a str,
        current_only: bool,
    },
    /// One specific snapshot's coverage, by `stage_sets.snapshot_id` — one
    /// version's copies (ADR-0012: a copy is identical content, counted
    /// per version), not the unit as a whole. It is the building block of
    /// every per-version question: the MIN that [`CoverageScope::Unit`]
    /// takes across current versions, [`short_of_copies_at`],
    /// [`versions_at_stake`], `snapshot mark-reclaimable` (the SUPERSEDING
    /// snapshot), `report pending`/`report summary` (a stage set's own
    /// version) and `unit mark-tape-only`'s thinnest-version line.
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

// ── The least-covered slice (#478; ADR-0012 items 76, 127, 134, 138) ──
//
// From 1.7.0 a spannable version can be on tape as `range` rows, each
// holding only a run of its stage set's slices, so a copy is counted slice
// by slice (item 76): a version is as covered as its least-covered slice.
// Every count below — copies, places, deposits, the copies at a named place,
// [`slice_copies`] — is this one derivation, asked of a different thing.
//
// - **Who holds a slice.** An [`eligible`] volume with a `completed` write of
//   the slice's stage set (`layout-session.md`: `completed` means every slice
//   of the set, 1 to N, is on that volume, so the row holds them all by its
//   status), or with a `range` write whose `write_positions` row for the
//   slice is `written` (the range is the row's written positions, item 121).
//   A `cut` write holds nothing that counts (item 112), and no other status
//   is a finished write on a sealed tape. A recorded warehouse deposit of a
//   holder holds what its volume holds (item 127: "a deposit of a range
//   volume counts for that range"; ADR-0006).
// - **Item 138's credit.** Each tape, deposit and place counts for ONE stage
//   set only: the lowest-numbered stage set it holds among the stage sets of
//   the count's scope (a version's, for every per-version count). Without
//   it two stagings of one version on one tape, or at one place, would count
//   that tape or place twice.
// - **Per stage set**, the copies of its least-covered slice; **per scope**,
//   the sum over its stage sets (slices of two stage sets never combine,
//   item 102). A stage set with no slice rows counts its whole writes.
//
// With whole writes only this is exactly 1.6.0's count: every credited
// holder holds every slice, so the least-covered slice is the number of
// distinct volumes (and deposits, and places), each counted once. The SQL
// keeps that case cheap: a set's whole holders are counted from `writes`
// alone, and its slices are walked only when it has a `range` row.

/// What the per-slice derivation counts at each slice of a stage set.
#[derive(Debug, Clone, Copy)]
enum Counted<'a> {
    /// Copies: each credited holder volume, and each deposit of one.
    Copies,
    /// The deposits alone, for display ([`deposit_count_expr`]).
    Deposits,
    /// The copies at one place (`place_expr`, SQL evaluating to a location
    /// id): a credited holder shelved there, and each deposit of one there.
    CopiesAt(&'a str),
    /// Distinct places: a holder's shelf and a deposit's location, each
    /// place credited to one stage set.
    Places,
}

/// The stage sets among which item 138 credits each tape, deposit and
/// place: those of the count's own scope.
#[derive(Debug, Clone, Copy)]
enum Partition<'a> {
    /// One version's stage sets; SQL evaluating to its snapshot id.
    Version(&'a str),
    /// A unit's stage sets, or only its current versions'.
    Unit {
        id_expr: &'a str,
        current_only: bool,
    },
}

impl Partition<'_> {
    /// The joins after `stage_sets xss` and the predicate that keep `xss` in
    /// this partition.
    fn of_xss(&self) -> (String, String) {
        match *self {
            Partition::Version(snapshot) => {
                (String::new(), format!("xss.snapshot_id = {snapshot}"))
            }
            Partition::Unit {
                id_expr,
                current_only,
            } => (
                " JOIN snapshots xs ON xs.id = xss.snapshot_id".to_string(),
                format!(
                    "xs.unit_id = {id_expr}{}",
                    if current_only {
                        " AND xs.status = 'current'"
                    } else {
                        ""
                    }
                ),
            ),
        }
    }
}

/// `{write_alias}`'s status is one whose slices can count: `completed` or
/// `range`.
fn holds_slices(write_alias: &str) -> String {
    status_in(write_alias, &["completed", "range"])
}

/// A SQL boolean: the `range` write `cw` has a `written` position of the
/// slice `slice_expr`.
fn range_holds(slice_expr: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM write_positions cp
                 WHERE cp.write_id = cw.id AND cp.stage_slice_id = {slice_expr}
                   AND cp.status = 'written')"
    )
}

/// One stage set's per-slice count: the building blocks every count in this
/// module is made of. Aliases: `cw` (writes), `cv` (volumes), `cd`
/// (deposits), `cp` (positions), `cl` (the set's slices) and `xss`/`xs`/
/// `xw`/`xv`/`xd` for item 138's lower stage sets.
#[derive(Debug, Clone, Copy)]
struct SliceCount<'a> {
    /// SQL evaluating to the stage set's id.
    set: &'a str,
    partition: Partition<'a>,
    /// SQL evaluating to a volume id to pretend away
    /// ([`CoverageQuery::exclude_volume`]).
    exclude: Option<&'a str>,
    counted: Counted<'a>,
}

impl SliceCount<'_> {
    /// `FROM … WHERE …` of the set's writes of `status` on [`eligible`]
    /// volumes, not the excluded one, with `extra_join` after `volumes cv`.
    fn writes(&self, status: &str, extra_join: &str) -> String {
        let exclude = self
            .exclude
            .map(|e| format!(" AND cw.volume_id != {e}"))
            .unwrap_or_default();
        format!(
            "FROM writes cw JOIN volumes cv ON cv.id = cw.volume_id{extra_join}
             WHERE cw.stage_set_id = {set} AND cw.status = '{status}' AND {eligible}{exclude}",
            set = self.set,
            eligible = eligible("cv"),
        )
    }

    /// Item 138 for the tape `cw` is on, and its deposits: no lower stage
    /// set of the partition is held on the same volume.
    fn tape_credited(&self) -> String {
        let (joins, partition) = self.partition.of_xss();
        format!(
            "NOT EXISTS (SELECT 1 FROM writes xw
                         JOIN stage_sets xss ON xss.id = xw.stage_set_id{joins}
                         WHERE xw.volume_id = cw.volume_id
                           AND xw.stage_set_id < cw.stage_set_id
                           AND {holds} AND {partition})",
            holds = holds_slices("xw"),
        )
    }

    /// Item 138 for the place `place` (SQL for a location id): no lower stage
    /// set of the partition is held there, on an eligible volume shelved
    /// there or by a deposit there of one.
    fn place_credited(&self, place: &str) -> String {
        let (joins, partition) = self.partition.of_xss();
        let exclude = self
            .exclude
            .map(|e| format!(" AND xw.volume_id != {e}"))
            .unwrap_or_default();
        format!(
            "NOT EXISTS (SELECT 1 FROM stage_sets xss{joins}
                         JOIN writes xw ON xw.stage_set_id = xss.id
                         JOIN volumes xv ON xv.id = xw.volume_id
                         WHERE {partition} AND xss.id < {set} AND {holds}
                           AND {eligible}{exclude}
                           AND (xv.location_id = {place}
                                OR EXISTS (SELECT 1 FROM volume_deposits xd
                                           WHERE xd.volume_id = xv.id
                                             AND xd.location_id = {place})))",
            set = self.set,
            holds = holds_slices("xw"),
            eligible = eligible("xv"),
        )
    }

    /// What one holder `cw` adds at a slice it holds, for the counts that
    /// are sums: itself and its deposits, its deposits, or those at a place.
    fn weight(&self) -> String {
        let deposits = |at: &str| {
            format!(
                "(SELECT COUNT(*) FROM volume_deposits cd WHERE cd.volume_id = cw.volume_id{at})"
            )
        };
        match self.counted {
            Counted::Copies => format!("1 + {}", deposits("")),
            Counted::Deposits => deposits(""),
            Counted::CopiesAt(place) => format!(
                "(CASE WHEN cv.location_id = {place} THEN 1 ELSE 0 END) + {}",
                deposits(&format!(" AND cd.location_id = {place}"))
            ),
            Counted::Places => unreachable!("places are counted distinct, never summed"),
        }
    }

    /// The places of the set's `status` writes (for a range, those holding
    /// `slice`), as one column `column`: each holder's shelf, and each
    /// deposit's location.
    fn place_rows(&self, status: &str, column: &str, slice: Option<&str>) -> String {
        let holds = slice
            .map(|s| format!(" AND {}", range_holds(s)))
            .unwrap_or_default();
        format!(
            "SELECT cv.location_id AS {column} {} AND cv.location_id IS NOT NULL{holds}
             UNION ALL
             SELECT cd.location_id {}{holds}",
            self.writes(status, ""),
            self.writes(
                status,
                " JOIN volume_deposits cd ON cd.volume_id = cw.volume_id"
            ),
        )
    }

    /// What the set's credited WHOLE holders (`completed` writes) count at
    /// every one of its slices.
    fn whole(&self) -> String {
        match self.counted {
            Counted::Places => format!(
                "(SELECT COUNT(DISTINCT cov_wplace) FROM ({}) WHERE {})",
                self.place_rows("completed", "cov_wplace", None),
                self.place_credited("cov_wplace"),
            ),
            _ => format!(
                "(SELECT COALESCE(SUM({}), 0) {} AND {})",
                self.weight(),
                self.writes("completed", ""),
                self.tape_credited(),
            ),
        }
    }

    /// What the set's credited `range` holders add at the slice `slice`
    /// (SQL for a `stage_slices` id), over [`Self::whole`]. A place that
    /// already holds the set whole is not counted again.
    fn range_at(&self, slice: &str) -> String {
        match self.counted {
            Counted::Places => format!(
                "(SELECT COUNT(DISTINCT cov_rplace) FROM ({}) WHERE {} AND cov_rplace NOT IN ({}))",
                self.place_rows("range", "cov_rplace", Some(slice)),
                self.place_credited("cov_rplace"),
                self.place_rows("completed", "cov_place", None),
            ),
            _ => format!(
                "(SELECT COALESCE(SUM({}), 0) {} AND {} AND {})",
                self.weight(),
                self.writes("range", ""),
                self.tape_credited(),
                range_holds(slice),
            ),
        }
    }

    /// The count at the slice `slice`: [`Self::whole`] plus [`Self::range_at`].
    fn at_slice(&self, slice: &str) -> String {
        format!("({} + {})", self.whole(), self.range_at(slice))
    }

    /// The count at the set's least-covered slice. Its slices are walked only
    /// when it has a `range` row: otherwise every slice counts the whole
    /// holders alone, and so does a set with no slice rows (item 138).
    fn least_covered(&self) -> String {
        format!(
            "({whole} + COALESCE(CASE WHEN EXISTS (SELECT 1 {ranges}) THEN
                 (SELECT {range} AS cov_n FROM stage_slices cl
                   WHERE cl.stage_set_id = {set} ORDER BY cov_n LIMIT 1)
             END, 0))",
            whole = self.whole(),
            ranges = self.writes("range", ""),
            range = self.range_at("cl.id"),
            set = self.set,
        )
    }
}

/// `counted` for `q`'s scope: the sum, over the scope's stage sets, of each
/// one's least-covered slice ([`SliceCount::least_covered`]), item 138's
/// credit taken among the scope's own stage sets. For
/// [`CoverageScope::Snapshot`] that is one version's; for
/// [`CoverageScope::Unit`] it is the unit's (current, with `current_only`)
/// versions', so a tape holding two versions counts once there, as 1.6.0's
/// union across versions did. The `MIN` over current versions is
/// [`copy_count_expr`]'s and [`location_count_expr`]'s, above this.
fn scope_sum(q: &CoverageQuery, counted: Counted) -> String {
    let (from, partition) = match q.scope {
        CoverageScope::Snapshot { id_expr } => (
            format!("FROM stage_sets css WHERE css.snapshot_id = {id_expr}"),
            Partition::Version(id_expr),
        ),
        CoverageScope::Unit {
            id_expr,
            current_only,
        } => (
            format!(
                "FROM snapshots cs JOIN stage_sets css ON css.snapshot_id = cs.id
                 WHERE cs.unit_id = {id_expr}{}",
                if current_only {
                    " AND cs.status = 'current'"
                } else {
                    ""
                }
            ),
            Partition::Unit {
                id_expr,
                current_only,
            },
        ),
    };
    let per_set = SliceCount {
        set: "css.id",
        partition,
        exclude: q.exclude_volume,
        counted,
    }
    .least_covered();
    format!("(SELECT COALESCE(SUM({per_set}), 0) {from})")
}

/// **The** copy-count expression: a parenthesised SQL scalar subquery
/// counting a unit's (or snapshot's) copies, warehouse deposits included.
///
/// A copy of a slice is one of two things, and both count:
/// - a distinct eligible VOLUME holding it (a `completed` write of its stage
///   set, or a `range` write with the slice `written`), and
/// - a recorded warehouse DEPOSIT of such a volume (ADR-0006: "the catalog
///   must claim them to reason about them" — the rejected alternative,
///   "cloud as external practice only with no model presence", is exactly
///   the under-count this expression exists to prevent).
///
/// A version's copies are its least-covered slice's, per stage set, summed
/// over its stage sets, each tape and deposit credited to one stage set
/// (#478, ADR-0012 items 127 and 138; the derivation is described above
/// [`Counted`]). With whole writes only this is 1.6.0's count: the distinct
/// eligible volumes with a completed write, plus their deposits.
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
///
/// **Per-version rule (issue #153, ADR-0012).** `snapshots.status`
/// permits more than one `'current'` row per unit at once — each sealed
/// write promotes its snapshot to `'current'` and never demotes its
/// predecessor (`'superseded'` never had a writer and left the schema in
/// migration 026). A newer
/// Version is not a copy of an older one, so when `q.scope` is
/// [`CoverageScope::Unit`] with `current_only: true`, this is NOT the
/// count across every current snapshot (that would count "how many volumes
/// hold ANY version," not how covered the unit actually is). It is instead
/// the MINIMUM, over the unit's current snapshots, of each one's own copy
/// count — ADR-0012: "a unit is as covered as its least-covered live
/// version." Every other scope ([`CoverageScope::Snapshot`], and
/// [`CoverageScope::Unit`] with `current_only: false` — `volume retire`'s
/// impact analysis, which deliberately asks about ANY snapshot) takes no
/// such minimum: the unit form counts each holder once across all of the
/// unit's versions, as the plain union did.
pub fn copy_count_expr(q: &CoverageQuery) -> String {
    if let CoverageScope::Unit {
        id_expr,
        current_only: true,
    } = q.scope
    {
        let per_snapshot = CoverageQuery {
            scope: CoverageScope::Snapshot { id_expr: "pcur.id" },
            exclude_volume: q.exclude_volume,
        };
        // COALESCE matters: MIN() over zero current snapshots is NULL,
        // and a unit with no current snapshot must still read 0 (same as
        // today), not NULL.
        return format!(
            "(SELECT COALESCE(MIN(per.c), 0) FROM (
                SELECT ({}) AS c
                FROM snapshots pcur
                WHERE pcur.unit_id = {id_expr} AND pcur.status = 'current'
             ) per)",
            copy_count_expr(&per_snapshot)
        );
    }
    scope_sum(q, Counted::Copies)
}

/// **The** distinct-location expression, warehouse deposits included.
///
/// A place holds a slice when an eligible volume shelved there (a non-NULL
/// `location_id`) holds it, or a deposit there of a volume that does. Each
/// place is counted once per slice, never once per tape or deposit there,
/// because a doubled location count would silently satisfy a
/// `required_locations` policy that is not actually met; "no location
/// recorded" is no place. Per stage set the places of its least-covered
/// slice, summed over the stage sets, each place credited to one stage set
/// (#478, ADR-0012 items 127, 134 and 138: "locations use the same
/// minimum"). With whole writes only this is 1.6.0's union of the eligible
/// volumes' and the deposits' locations.
///
/// Same warning as [`copy_count_expr`]: one expression, N call sites.
///
/// **Same per-version rule as [`copy_count_expr`] (issue #153, ADR-0012),
/// same reason:** for [`CoverageScope::Unit`] with `current_only: true`,
/// this is the MINIMUM over the unit's current snapshots of each one's
/// own location count — a unit is as covered as its least-covered live
/// version, and that applies to locations exactly as it does to copies.
/// Every other scope takes no such minimum.
pub fn location_count_expr(q: &CoverageQuery) -> String {
    if let CoverageScope::Unit {
        id_expr,
        current_only: true,
    } = q.scope
    {
        let per_snapshot = CoverageQuery {
            scope: CoverageScope::Snapshot { id_expr: "pcur.id" },
            exclude_volume: q.exclude_volume,
        };
        return format!(
            "(SELECT COALESCE(MIN(per.c), 0) FROM (
                SELECT ({}) AS c
                FROM snapshots pcur
                WHERE pcur.unit_id = {id_expr} AND pcur.status = 'current'
             ) per)",
            location_count_expr(&per_snapshot)
        );
    }
    scope_sum(q, Counted::Places)
}

/// How many of the copies counted by [`copy_count_expr`] are warehouse
/// deposits. Purely for DISPLAY: the advisory surfaces print it beside the
/// total so a warehouse copy is visibly distinguishable rather than
/// silently folded into a number that looks like tapes on a shelf.
///
/// The same derivation counting the deposits alone (#478, ADR-0012 items 127
/// and 138): per stage set, the deposits holding its least-covered slice, so
/// deposits of two range volumes that together hold a version count as one
/// copy, never more than the copies they are part of; each deposit credited
/// to one stage set. With whole writes only, every deposit of an eligible
/// volume with a completed write in scope, once.
///
/// Deliberately NOT per-version-MINed (issue #153): nothing gates on this
/// value, and a display count that disagrees with the total it is meant
/// to break down would confuse rather than protect — for a unit's current
/// versions it is taken across them all, as it has always been.
pub fn deposit_count_expr(q: &CoverageQuery) -> String {
    scope_sum(q, Counted::Deposits)
}

// ── A stage set's slices, one by one (#478; ADR-0012 items 122, 127-129) ──

/// One slice of a stage set and what holds it, counted as [`copy_count_expr`]
/// and [`location_count_expr`] count it at that slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceCopies {
    pub stage_slice_id: i64,
    pub slice_number: i64,
    /// The credited [`eligible`] volumes holding the slice, plus their
    /// warehouse deposits.
    pub copies: i64,
    /// The distinct places holding it: an eligible holder's shelf, a
    /// deposit's location.
    pub locations: i64,
}

/// Every slice of `stage_set_id`, in slice order, with its copies and
/// places: the per-slice view of the one derivation the counts above are made
/// of. A version's copies are, per stage set, the least of these, summed over
/// its stage sets; so for a version with one stage set, the fewest `copies`
/// here IS its [`copy_count_expr`]. Item 138's credit is taken among the
/// version's stage sets, as for the version's own count: a slice of a staging
/// whose tape is credited to a lower staging reads short here exactly when the
/// version's count does.
///
/// For the plan and the release (`layout-session.md` "Spanning"): a slice is
/// OWED while its `copies` are below its set's copy floor ([`copy_floor`]),
/// and a slice whose copies meet the floor is released from staging (item
/// 129). A stage set with no slice rows has none to list, and an unknown id
/// lists none.
pub fn slice_copies(
    conn: &Connection,
    stage_set_id: i64,
) -> crate::error::Result<Vec<SliceCopies>> {
    let mut stmt = conn.prepare(&slice_copies_sql())?;
    let rows = stmt
        .query_map(params![stage_set_id], |r| {
            Ok(SliceCopies {
                stage_slice_id: r.get(0)?,
                slice_number: r.get(1)?,
                copies: r.get(2)?,
                locations: r.get(3)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// [`slice_copies`]' statement: the stage set `?1`'s slices, each with its
/// copies and places.
fn slice_copies_sql() -> String {
    let count = |counted| SliceCount {
        set: "css.id",
        partition: Partition::Version("css.snapshot_id"),
        exclude: None,
        counted,
    };
    format!(
        "SELECT cl.id, cl.slice_number, {}, {}
         FROM stage_sets css JOIN stage_slices cl ON cl.stage_set_id = css.id
         WHERE css.id = ?1
         ORDER BY cl.slice_number",
        count(Counted::Copies).at_slice("cl.id"),
        count(Counted::Places).at_slice("cl.id"),
    )
}

/// A SQL boolean: every slice of the stage set `set_expr` is on a SEALED
/// volume. This is ADR-0012 item 128's promotion rule (`layout-session.md`
/// "Promotion and the retire floor"): a spanning version becomes `current`
/// once every slice of one of its stage sets is on a sealed volume. Confirm,
/// the adoption of a lost write and `catalog rebuild` ask it in the
/// transaction that records the seal or the rebuild.
///
/// A slice is on a volume when the volume holds it as the counts above read
/// a holder: a `completed` write of the set, or a `range` write with the
/// slice `written`. "Sealed" is the volume's status alone (coordinator fork
/// X33): a verify's quarantine does not undo a seal, and the rule asks what
/// the tapes hold, not what counts as a copy.
///
/// `also_sealed` is SQL for one volume id counted as sealed whatever its
/// status says, "the volume being sealed counted": `catalog rebuild` records
/// a tape's seal without ever changing a volume's status (item 130), so a
/// rebuilt tape on an `initialized` row names itself here. `None` when the
/// caller has already moved the status in its transaction.
///
/// A stage set with no slice rows has nothing on any tape and never meets
/// the rule: a whole write is promoted by confirm's own rule, not this one.
/// Embeds the aliases `ql`, `qw`, `qv` and `qp`; `set_expr` is the caller's.
pub fn every_slice_on_sealed_volume_sql(set_expr: &str, also_sealed: Option<&str>) -> String {
    let sealed = match also_sealed {
        Some(volume) => format!("({} OR qv.id = {volume})", status_in("qv", &["sealed"])),
        None => status_in("qv", &["sealed"]),
    };
    format!(
        "(EXISTS (SELECT 1 FROM stage_slices ql WHERE ql.stage_set_id = {set_expr})
          AND NOT EXISTS (
            SELECT 1 FROM stage_slices ql
             WHERE ql.stage_set_id = {set_expr}
               AND NOT EXISTS (
                 SELECT 1 FROM writes qw JOIN volumes qv ON qv.id = qw.volume_id
                  WHERE qw.stage_set_id = {set_expr} AND {sealed}
                    AND (qw.status = 'completed'
                         OR (qw.status = 'range'
                             AND EXISTS (SELECT 1 FROM write_positions qp
                                          WHERE qp.write_id = qw.id
                                            AND qp.stage_slice_id = ql.id
                                            AND qp.status = 'written'))))))"
    )
}

// ── Named required locations (issue #348; ADR-0012 2026-10-07 items 15, 34) ──

/// Whose coverage [`short_of_copies_at`], [`copies_at_every_location`] and
/// [`short_of_location_floor`] judge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedScope {
    /// A unit's CURRENT coverage, by unit id — `audit`'s
    /// `location_presence` and `unit mark-tape-only`. **Per-version rule
    /// (issue #153, ADR-0012)**, the same as [`location_count_expr`]'s MIN:
    /// a unit is as covered as its least-covered live version, so the
    /// copies a name holds are the FEWEST any current version has there. v1
    /// at `home-rack` and v2 at `offsite` holds 0 at either name. A unit
    /// with no current snapshot holds 0 everywhere.
    Unit(i64),
    /// ONE version's coverage, by snapshot id — `snapshot mark-reclaimable`
    /// and `report supersedable` ask it of the SUPERSEDING version, the one
    /// that must carry the policy once the released version is gone. The
    /// unit form would be the wrong question there: it takes the minimum
    /// over every current version, the one being released included.
    Snapshot(i64),
}

// ── The tape-only multiplier (ADR-0012 2026-10-07 items 15, 34; 2026-10-12 items 43, 46) ──

/// **The** tape-only rule: the factor every COPY floor is multiplied by for
/// a unit whose status is (or, for `unit mark-tape-only`, is about to be)
/// `unit_status` — `[compaction] tape_only_safety_multiplier` for
/// `tape_only`, 1 for every other status. `Config::load` refuses a
/// multiplier below 1, so this is always at least 1.
///
/// ADR-0012 2026-10-12 item 43 (#423 part 2) extended item 34 from the
/// per-name count to every copy floor: `audit`'s `copy_count`, `unit
/// mark-tape-only`, `snapshot mark-reclaimable`, `report supersedable`, the
/// retire family's Tier-2 facts, the staging retention gate and `report
/// pending` all judge a tape-only unit's copies against `floor × this`.
/// Item 46 bounds it: the multiplier raises copy floors only — the resolved
/// `min_copies` ([`copy_floor`]) and the copies each location of the
/// `copies_at` map must hold ([`location_target`]) — and NEVER a count
/// of distinct places (`[defaults] min_locations`, the number of named
/// locations), which a tape-only unit meets as set. At the defaults that is
/// 4 copies in 2 places, not 4 in 4. A copy floor is never multiplied
/// anywhere but through these two.
pub fn tape_only_multiplier(config: &crate::config::Config, unit_status: &str) -> i64 {
    if unit_status == "tape_only" {
        i64::from(config.compaction.tape_only_safety_multiplier)
    } else {
        1
    }
}

/// A copy floor `base` as a unit of status `unit_status` must meet it: `base
/// ×` [`tape_only_multiplier`]. `base` is the unit's
/// [`crate::policy::ResolvedPolicy::copy_floor_base`] (the larger of
/// `min_copies` and the `copies_at` sum, #493 item 149; never `min_copies`
/// read on its own) or one location's target ([`location_target`]). Never hand
/// it a count of distinct places: those are not multiplied (ADR-0012
/// 2026-10-12 item 46).
pub fn copy_floor(config: &crate::config::Config, base: i64, unit_status: &str) -> i64 {
    base * tape_only_multiplier(config, unit_status)
}

/// The suffix a copy floor's message carries when the multiplier raised it:
/// `" (tape-only 3x)"` for a tape-only unit whose multiplier is above 1,
/// empty otherwise — so a message never shows a number the policy alone
/// does not explain. A place-count message never carries it (item 46).
pub fn tape_only_note(config: &crate::config::Config, unit_status: &str) -> String {
    match tape_only_multiplier(config, unit_status) {
        1 => String::new(),
        m => format!(" (tape-only {m}x)"),
    }
}

// ── The distinct-place floor (ADR-0012 2026-10-12 items 46, 47; issue #425) ──

/// **The** distinct-place floor: how many different places a unit's copies
/// must span, for a unit whose status is (or, for `unit mark-tape-only`, is
/// about to be) `unit_status` — `[defaults] min_locations` for `tape_only`,
/// 0 (no floor) for every other status.
///
/// It is read from `[defaults]` and not from the unit's resolved policy
/// because it is not a layer of that chain: `policy::resolve` carries
/// `min_copies` and `required_locations`, and no dotfile or archive set can
/// set a count of places (an archive set names places with
/// `required_locations`). It is `config`, never an `i64`, so no caller can
/// hand it a multiplied number: ADR-0012 2026-10-12 item 46 — the tape-only
/// multiplier raises copy floors ([`copy_floor`], [`location_target`]) and
/// never a count of distinct places. At
/// the defaults a tape-only unit needs 4 copies in 2 places, not 4 in 4.
///
/// It is a tape-only floor because the claim it guards is the one `unit
/// mark-tape-only` makes — the source is going away, so the tape copies
/// must not all sit in one fire, flood or theft. An active unit still has
/// its source on disk, and a catalog that never ran `location add` has every
/// volume at no place at all. A tape-only unit is held to the floor after the
/// marking exactly as at it: `audit`'s `location_count`, `snapshot
/// mark-reclaimable`/`report supersedable` ([`short_of_location_floor`]) and
/// `unit mark-tape-only` ([`PlaceShortfall::judge`], since it shows the count
/// whether or not it is short) all go through this floor and that one
/// comparison.
pub fn location_floor(config: &crate::config::Config, unit_status: &str) -> i64 {
    if unit_status == "tape_only" {
        i64::from(config.defaults.min_locations)
    } else {
        0
    }
}

/// A scope whose copies sit in fewer distinct places than its
/// [`location_floor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaceShortfall {
    /// The distinct places the scope's copies span ([`location_count_expr`]:
    /// the fewest any examined version has).
    pub places: i64,
    /// The floor it fell short of — as set, never multiplied.
    pub floor: i64,
}

impl PlaceShortfall {
    /// The comparison itself, and the only place it is made: `Some` when
    /// `places` is under the [`location_floor`] for `unit_status`, `None`
    /// when it meets it or the status has no floor.
    pub fn judge(
        config: &crate::config::Config,
        unit_status: &str,
        places: i64,
    ) -> Option<PlaceShortfall> {
        let floor = location_floor(config, unit_status);
        (places < floor).then_some(PlaceShortfall { places, floor })
    }

    /// `"copies in 1 location(s), needs 2 ([defaults] min_locations)"`: the
    /// words `audit`'s `location_count` and `snapshot mark-reclaimable` put
    /// after their subject (`"has "`, `"superseding v2 has "`). It carries no
    /// tape-only note: the floor is not multiplied.
    pub fn describe(&self) -> String {
        format!(
            "copies in {} location(s), needs {} ([defaults] min_locations)",
            self.places, self.floor
        )
    }
}

/// The distinct places `scope`'s copies span, by [`location_count_expr`]: a
/// recorded warehouse deposit is a place (ADR-0006), a volume nobody has
/// shelved is none, and only [`eligible`] volumes count (ADR-0004).
/// [`NamedScope::Unit`] is the unit's CURRENT coverage — the fewest places
/// any current version has, the per-version rule of issue #153.
/// [`NamedScope::Snapshot`] is one version.
pub fn places_held(conn: &Connection, scope: NamedScope) -> crate::error::Result<i64> {
    places_held_excluding(conn, scope, None)
}

/// [`places_held`] with `exclude_volume` (a volume id) pretended away: the
/// places the scope's copies WOULD span once that volume is given up —
/// [`CoverageQuery::exclude_volume`]'s "pretend this volume is not there",
/// removing its tape claim and any deposit recorded from it. `None` is
/// [`places_held`] itself.
pub fn places_held_excluding(
    conn: &Connection,
    scope: NamedScope,
    exclude_volume: Option<i64>,
) -> crate::error::Result<i64> {
    let (q, id) = match scope {
        NamedScope::Unit(id) => (CoverageQuery::current_unit("?1"), id),
        NamedScope::Snapshot(id) => (
            CoverageQuery {
                scope: CoverageScope::Snapshot { id_expr: "?1" },
                exclude_volume: None,
            },
            id,
        ),
    };
    let q = CoverageQuery {
        exclude_volume: exclude_volume.map(|_| "?2"),
        ..q
    };
    let sql = format!("SELECT {}", location_count_expr(&q));
    Ok(conn.query_row(
        &sql,
        rusqlite::params_from_iter([Some(id), exclude_volume].into_iter().flatten()),
        |row| row.get(0),
    )?)
}

/// **The** distinct-place predicate: `Some` when `scope`'s copies sit in
/// fewer distinct places ([`places_held`]) than the [`location_floor`] for
/// `unit_status`, `None` when they meet it or the status has no floor (no
/// statement is run then, so an active unit costs `audit` nothing).
///
/// `audit`'s `location_count` asks it of [`NamedScope::Unit`] — the unit's
/// current coverage (`unit mark-tape-only` asks the same of the same scope,
/// by [`places_held`] and [`PlaceShortfall::judge`], because it prints the
/// count too). `snapshot mark-reclaimable` and `report supersedable` ask it
/// of [`NamedScope::Snapshot`], the SUPERSEDING version, which must carry
/// the unit once the released one is gone.
pub fn short_of_location_floor(
    conn: &Connection,
    config: &crate::config::Config,
    scope: NamedScope,
    unit_status: &str,
) -> crate::error::Result<Option<PlaceShortfall>> {
    short_of_location_floor_excluding(conn, config, scope, unit_status, None)
}

/// [`short_of_location_floor`] asked "after `exclude_volume` is given up"
/// (see [`places_held_excluding`]) — the question the retire family's
/// Tier-2 gate puts to each version it consumes coverage from. `None` is
/// [`short_of_location_floor`] itself.
pub fn short_of_location_floor_excluding(
    conn: &Connection,
    config: &crate::config::Config,
    scope: NamedScope,
    unit_status: &str,
    exclude_volume: Option<i64>,
) -> crate::error::Result<Option<PlaceShortfall>> {
    if location_floor(config, unit_status) <= 0 {
        return Ok(None);
    }
    Ok(PlaceShortfall::judge(
        config,
        unit_status,
        places_held_excluding(conn, scope, exclude_volume)?,
    ))
}

// ── Copies at a location (#493; ADR-0012 items 149, 150, 156-158, 160) ──
//
// A unit's per-location targets are its resolved `copies_at` map (item 156:
// the most specific layer that sets it wins entirely, an archive set's
// `required_locations` folded in as one copy at each name). That map is the
// ONE source of per-location targets here: nothing below reads
// `ResolvedPolicy::required_locations`, which would count a folded name twice
// or bring back a name a dotfile's map replaced.
//
// A copy "at" a location (item 150) is counted by [`Counted::CopiesAt`], the
// one per-slice derivation every count in this module is: an eligible tape
// holding the slice and shelved there, or a recorded deposit there of one (a
// warehouse is counted by its deposits, item 157); per stage set its
// least-covered slice, summed over the version's stage sets with each tape
// and deposit credited to one stage set (items 127, 138); and per unit the
// fewest any current version has there (issue #153).

/// **The** copies-at-a-place expression: a parenthesised SQL scalar counting
/// `q`'s copies at the place `place_expr` (SQL evaluating to a location id),
/// by [`Counted::CopiesAt`]. For [`CoverageScope::Unit`] with `current_only`
/// it is the MINIMUM over the unit's current versions, as
/// [`copy_count_expr`] is (a unit with no current version holds 0
/// everywhere); every other scope takes no minimum.
pub fn copies_at_expr(q: &CoverageQuery, place_expr: &str) -> String {
    if let CoverageScope::Unit {
        id_expr,
        current_only: true,
    } = q.scope
    {
        let per_snapshot = CoverageQuery {
            scope: CoverageScope::Snapshot { id_expr: "pcur.id" },
            exclude_volume: q.exclude_volume,
        };
        return format!(
            "(SELECT COALESCE(MIN(per.c), 0) FROM (
                SELECT ({}) AS c
                FROM snapshots pcur
                WHERE pcur.unit_id = {id_expr} AND pcur.status = 'current'
             ) per)",
            copies_at_expr(&per_snapshot, place_expr)
        );
    }
    scope_sum(q, Counted::CopiesAt(place_expr))
}

/// `scope` as a [`CoverageQuery`] over the bound parameter `?1`, with the
/// volume `?2` pretended away when `exclude`, and the id to bind as `?1`.
fn named_query(scope: NamedScope, exclude: bool) -> (CoverageQuery<'static>, i64) {
    let (scope, id) = match scope {
        NamedScope::Unit(id) => (
            CoverageScope::Unit {
                id_expr: "?1",
                current_only: true,
            },
            id,
        ),
        NamedScope::Snapshot(id) => (CoverageScope::Snapshot { id_expr: "?1" }, id),
    };
    (
        CoverageQuery {
            scope,
            exclude_volume: exclude.then_some("?2"),
        },
        id,
    )
}

/// The copies `scope` holds at EVERY registered location, by name (item
/// 150), 0 where it holds none, in one statement. [`NamedScope::Unit`] is the
/// unit's current coverage, the fewest copies any current version has at
/// each place; [`NamedScope::Snapshot`] is one version.
pub fn copies_at_every_location(
    conn: &Connection,
    scope: NamedScope,
) -> crate::error::Result<std::collections::BTreeMap<String, i64>> {
    let (q, id) = named_query(scope, false);
    let sql = format!(
        "SELECT l.name, {} FROM locations l",
        copies_at_expr(&q, "l.id")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// The copies `scope` holds at the one location named `location` (item 150),
/// with `exclude_volume` (a volume id) pretended away when given, as
/// [`places_held_excluding`] does. A name matching no location holds 0:
/// nothing can be at a place the catalog does not know.
pub fn copies_at_location(
    conn: &Connection,
    scope: NamedScope,
    location: &str,
    exclude_volume: Option<i64>,
) -> crate::error::Result<i64> {
    let (sql, id) = copies_at_location_sql(scope, exclude_volume.is_some());
    let mut bound: Vec<rusqlite::types::Value> = vec![id.into()];
    bound.extend(exclude_volume.map(rusqlite::types::Value::from));
    bound.push(location.to_string().into());
    Ok(conn.query_row(&sql, rusqlite::params_from_iter(bound.iter()), |r| r.get(0))?)
}

/// [`copies_at_location`]'s statement, and the id to bind as `?1`: the scope
/// `?1`, the volume `?2` pretended away when `exclude`, the name last.
fn copies_at_location_sql(scope: NamedScope, exclude: bool) -> (String, i64) {
    let (q, id) = named_query(scope, exclude);
    let name = if exclude { "?3" } else { "?2" };
    let sql = format!(
        "SELECT COALESCE((SELECT {} FROM locations l WHERE l.name = {name}), 0)",
        copies_at_expr(&q, "l.id")
    );
    (sql, id)
}

/// Every unit's copies at every registered location, keyed by unit id then
/// location name (item 150), from ONE statement: the whole-catalog form for a
/// report, where [`copies_at_every_location`] is the one-unit form for a
/// command. Each unit's current coverage, as [`NamedScope::Unit`]: a unit
/// with no current version reads 0 at every place.
pub fn copies_at_every_location_by_unit(
    conn: &Connection,
) -> crate::error::Result<std::collections::HashMap<i64, std::collections::BTreeMap<String, i64>>> {
    let sql = format!(
        "SELECT u.id, l.name, {} FROM units u CROSS JOIN locations l",
        copies_at_expr(&CoverageQuery::current_unit("u.id"), "l.id")
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut by_unit: std::collections::HashMap<i64, std::collections::BTreeMap<String, i64>> =
        Default::default();
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get(2)?))
    })?;
    for row in rows {
        let (unit, location, copies) = row?;
        by_unit.entry(unit).or_default().insert(location, copies);
    }
    Ok(by_unit)
}

/// One location of a unit's `copies_at` map that holds fewer copies than its
/// target (#493, ADR-0012 item 149).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationShort {
    pub location: String,
    /// The copies there ([`copies_at_expr`]).
    pub copies: i64,
    /// The target there: the map's count, times the tape-only multiplier for a
    /// tape-only unit ([`location_target`]).
    pub target: i64,
}

impl LocationShort {
    /// How many more copies the location needs.
    pub fn short_by(&self) -> i64 {
        self.target - self.copies
    }
}

/// The copies a unit of status `unit_status` must hold at `location` under
/// its resolved policy: the `copies_at` map's count there (0 when it names no
/// count), as a [`copy_floor`], so a tape-only unit's target is multiplied
/// (item 149, as item 46 raised a named location's count). 0 means no target
/// there: a 0 in the map asks for nothing (`config::validate_copies_at`).
pub fn location_target(
    config: &crate::config::Config,
    resolved: &crate::policy::ResolvedPolicy,
    location: &str,
    unit_status: &str,
) -> i64 {
    copy_floor(
        config,
        resolved.copies_at.get(location).copied().unwrap_or(0),
        unit_status,
    )
}

/// The locations of `resolved`'s `copies_at` map whose copies (`at`, by name;
/// a name absent from it holds 0) are under their [`location_target`], in name
/// order: the per-location half of [`Shortfall`], as a pure judgement so a
/// report that has fetched every count in one pass judges by the same rule.
pub fn judge_copies_at(
    config: &crate::config::Config,
    resolved: &crate::policy::ResolvedPolicy,
    unit_status: &str,
    at: &std::collections::BTreeMap<String, i64>,
) -> Vec<LocationShort> {
    resolved
        .copies_at
        .keys()
        .filter_map(|location| {
            let target = location_target(config, resolved, location, unit_status);
            let copies = at.get(location).copied().unwrap_or(0);
            (copies < target).then(|| LocationShort {
                location: location.clone(),
                copies,
                target,
            })
        })
        .collect()
}

/// **The** per-location predicate (#493, ADR-0012 items 149 and 160): the
/// locations of `resolved`'s `copies_at` map at which `scope` holds fewer
/// copies than their [`location_target`] for `unit_status`, in name order,
/// with `exclude_volume` (a volume id) pretended away when given, as
/// [`places_held_excluding`] does. Empty when every target is met, and when
/// the map has no target above 0, in which case no statement is run (so a
/// catalog without targets costs nothing more). Otherwise one statement,
/// counting only the target names. A name matching no `locations` row holds
/// 0: nothing can be at a place the catalog does not know.
///
/// `audit`'s `location_presence`, `unit mark-tape-only`'s location gate,
/// `snapshot mark-reclaimable`/`report supersedable` (of the superseding
/// version) and the retire family's Tier-2 facts (each version, the volume
/// given up excluded) all ask this one function, so they can never disagree
/// about whether a location's target is met. It replaced a count-only
/// comparison under which copies at `home-rack` and `garage` satisfied
/// `["home-rack","offsite"]` (issue #348), and since #493 it reads the
/// resolved map, not `required_locations`, which the map already holds at
/// one copy each (item 156): a name a dotfile's map replaced is not asked
/// for, and a folded name is not asked for twice.
pub fn short_of_copies_at(
    conn: &Connection,
    config: &crate::config::Config,
    scope: NamedScope,
    resolved: &crate::policy::ResolvedPolicy,
    unit_status: &str,
    exclude_volume: Option<i64>,
) -> crate::error::Result<Vec<LocationShort>> {
    let at = copies_at_targets(conn, scope, resolved, exclude_volume)?;
    Ok(judge_copies_at(config, resolved, unit_status, &at))
}

/// The copies `scope` holds at each location `resolved`'s `copies_at` map
/// gives a target above 0, by name (a name no location has is absent, so it
/// holds 0), with `exclude_volume` pretended away: one statement, or none when
/// the map has no such target.
fn copies_at_targets(
    conn: &Connection,
    scope: NamedScope,
    resolved: &crate::policy::ResolvedPolicy,
    exclude_volume: Option<i64>,
) -> crate::error::Result<std::collections::BTreeMap<String, i64>> {
    let names: Vec<&String> = resolved
        .copies_at
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(name, _)| name)
        .collect();
    if names.is_empty() {
        return Ok(Default::default());
    }
    let (sql, id) = copies_at_targets_sql(scope, exclude_volume.is_some(), names.len());
    let mut bound: Vec<rusqlite::types::Value> = vec![id.into()];
    bound.extend(exclude_volume.map(rusqlite::types::Value::from));
    bound.extend(names.iter().map(|name| (*name).clone().into()));
    let mut stmt = conn.prepare(&sql)?;
    let at = stmt
        .query_map(rusqlite::params_from_iter(bound.iter()), |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(at)
}

/// [`copies_at_targets`]' statement, and the id to bind as `?1`: the scope
/// `?1`, the volume `?2` pretended away when `exclude`, then `names` names.
fn copies_at_targets_sql(scope: NamedScope, exclude: bool, names: usize) -> (String, i64) {
    let (q, id) = named_query(scope, exclude);
    let first = 2 + usize::from(exclude);
    let placeholders = (0..names)
        .map(|i| format!("?{}", first + i))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT l.name, {} FROM locations l WHERE l.name IN ({placeholders})",
        copies_at_expr(&q, "l.id")
    );
    (sql, id)
}

/// The locations in a [`short_of_copies_at`] answer that hold no copy at all,
/// in name order: the "no copy at required location(s) …" half every caller
/// reports first.
pub fn locations_without_a_copy(short: &[LocationShort]) -> Vec<String> {
    short
        .iter()
        .filter(|s| s.copies == 0)
        .map(|s| s.location.clone())
        .collect()
}

/// One wording for a [`short_of_copies_at`] answer, after a caller's subject:
/// "1 copy at required location offsite, needs 2", with "at each" when several
/// share one target, and each location's own "needs N" when the targets
/// differ ("2 copies at required location home, needs 3; 0 copies at required
/// location offsite, needs 1"). The caller adds the "(tape-only Nx)" note.
pub fn describe_short_locations(short: &[LocationShort]) -> String {
    let at = |s: &LocationShort| {
        let noun = if s.copies == 1 { "copy" } else { "copies" };
        format!("{} {noun} at required location {}", s.copies, s.location)
    };
    match short.first() {
        Some(first) if short.iter().all(|s| s.target == first.target) => {
            let each = if short.len() == 1 { "" } else { " at each" };
            format!(
                "{}, needs {}{each}",
                short.iter().map(at).collect::<Vec<_>>().join(", "),
                first.target
            )
        }
        _ => short
            .iter()
            .map(|s| format!("{}, needs {}", at(s), s.target))
            .collect::<Vec<_>>()
            .join("; "),
    }
}

/// A `copies_at` map as a policy states it, for "(policy requires …)": the
/// names alone when every target is 1 (the shorthand `required_locations`
/// writes, item 149), else each as "N at NAME"; a 0 target asks for nothing
/// and is left out. Never multiplied: the caller's "(tape-only Nx)" note says
/// what the multiplier did.
pub fn describe_targets(copies_at: &std::collections::BTreeMap<String, i64>) -> String {
    let targets: Vec<(&String, &i64)> = copies_at.iter().filter(|(_, n)| **n > 0).collect();
    if targets.iter().all(|(_, n)| **n == 1) {
        targets
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        targets
            .iter()
            .map(|(name, n)| format!("{n} at {name}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// What a unit needs at one location X (#493, ADR-0012 item 158), in item
/// 158's two groups: the answer `stage for-location X` picks units by, and
/// `report needs --location X` shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NeedAt {
    /// Group 1: the unit's target at X is unmet. `target` is the map's count
    /// at X times the tape-only multiplier ([`location_target`]); `copies_here`
    /// the copies at X; `copies` the unit's copies in all (item 152's
    /// tie-break).
    Target {
        target: i64,
        copies_here: i64,
        copies: i64,
    },
    /// Group 2: the unit is short of its total copy floor (`floor`, through
    /// [`copy_floor`]), has no target at X and no copy at X yet. A unit
    /// holding a copy at X is never in this group, so one holding all its
    /// copies at X is never picked for X by it.
    Floor { floor: i64, copies: i64 },
    /// Neither: nothing for X to do for this unit.
    None,
}

impl NeedAt {
    /// The judgement itself, from counts a caller already has: `copies`, the
    /// unit's copies in all ([`copy_count_expr`]), and `copies_here`, its
    /// copies at `location` ([`copies_at_expr`]). A 0 target is no target
    /// (`config::validate_copies_at`: "0 asks for none there"), so a unit whose
    /// map says `X = 0` and that is short of its floor is in group 2 for X
    /// while it has no copy there, exactly as one whose map does not name X.
    pub fn judge(
        config: &crate::config::Config,
        resolved: &crate::policy::ResolvedPolicy,
        unit_status: &str,
        location: &str,
        copies: i64,
        copies_here: i64,
    ) -> NeedAt {
        let target = location_target(config, resolved, location, unit_status);
        if target > 0 {
            return if copies_here < target {
                NeedAt::Target {
                    target,
                    copies_here,
                    copies,
                }
            } else {
                NeedAt::None
            };
        }
        let floor = copy_floor(config, resolved.copy_floor_base(), unit_status);
        if copies < floor && copies_here == 0 {
            NeedAt::Floor { floor, copies }
        } else {
            NeedAt::None
        }
    }

    /// 1 or 2, item 158's group, or `None`.
    pub fn group(&self) -> Option<u8> {
        match self {
            NeedAt::Target { .. } => Some(1),
            NeedAt::Floor { .. } => Some(2),
            NeedAt::None => None,
        }
    }

    /// How far short: of the target at X (group 1) or of the copy floor
    /// (group 2); 0 for [`NeedAt::None`].
    pub fn short_by(&self) -> i64 {
        match *self {
            NeedAt::Target {
                target,
                copies_here,
                ..
            } => target - copies_here,
            NeedAt::Floor { floor, copies } => floor - copies,
            NeedAt::None => 0,
        }
    }
}

/// [`NeedAt`] for `unit` at `location` under its resolved policy `resolved`:
/// two statements, its copies in all and its copies at `location` (a name no
/// location has holds 0).
pub fn need_at(
    conn: &Connection,
    config: &crate::config::Config,
    unit: &crate::db::models::Unit,
    resolved: &crate::policy::ResolvedPolicy,
    location: &str,
) -> crate::error::Result<NeedAt> {
    let copies: i64 = conn.query_row(
        &format!(
            "SELECT {}",
            copy_count_expr(&CoverageQuery::current_unit("?1"))
        ),
        params![unit.id],
        |row| row.get(0),
    )?;
    let copies_here = copies_at_location(conn, NamedScope::Unit(unit.id), location, None)?;
    Ok(NeedAt::judge(
        config,
        resolved,
        &unit.status,
        location,
        copies,
        copies_here,
    ))
}

// ── What a new copy would resolve (ADR-0012 2026-10-07 item 26, #144) ──

/// How far a unit's CURRENT coverage falls short of its resolved policy, on
/// exactly the two terms `audit`'s `copy_count` and `location_presence`
/// checks judge it by: [`copy_count_expr`] against the [`copy_floor`] of
/// [`crate::policy::ResolvedPolicy::copy_floor_base`] (#493, ADR-0012 item
/// 149: the larger of `min_copies` and the `copies_at` sum), and each
/// location of the resolved `copies_at` map against its [`location_target`]
/// ([`judge_copies_at`]; items 149 and 160 — the map is the one source of
/// per-location targets, an archive set's `required_locations` folded into it,
/// so it is never also judged on its own). These are the findings a write can
/// resolve, which is what `--policy-aware` (`collection plan`/`run`, `volume
/// plan`) orders by and what `collection status` counts as under-copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortfall {
    pub copies: i64,
    /// The copy floor: the larger of the resolved `min_copies` and the
    /// `copies_at` sum, times the tape-only multiplier for a tape-only unit
    /// ([`copy_floor`]).
    pub min_copies: i64,
    /// The locations of the `copies_at` map under their target, in name order:
    /// those with no copy and those with some but too few.
    pub locations: Vec<LocationShort>,
    /// The [`tape_only_multiplier`] the floor and every target were raised by.
    pub multiplier: i64,
}

impl Shortfall {
    /// The judgement, from counts a caller already has: `copies`, the unit's
    /// copies in all, and `at`, its copies at each location by name (a name
    /// absent holds 0; only the map's names are read). [`shortfall`] fetches
    /// them for one unit; a report that fetched every unit's in one pass
    /// ([`copies_at_every_location_by_unit`]) judges by this same rule.
    pub fn judge(
        config: &crate::config::Config,
        resolved: &crate::policy::ResolvedPolicy,
        unit_status: &str,
        copies: i64,
        at: &std::collections::BTreeMap<String, i64>,
    ) -> Shortfall {
        Shortfall {
            copies,
            min_copies: copy_floor(config, resolved.copy_floor_base(), unit_status),
            locations: judge_copies_at(config, resolved, unit_status, at),
            multiplier: tape_only_multiplier(config, unit_status),
        }
    }

    /// `audit`'s `copy_count` finding.
    pub fn under_copied(&self) -> bool {
        self.copies < self.min_copies
    }

    /// Either finding: a write would help this unit.
    pub fn any(&self) -> bool {
        self.under_copied() || !self.locations.is_empty()
    }

    /// The map's locations with no copy at all, in name order.
    pub fn missing_locations(&self) -> Vec<String> {
        locations_without_a_copy(&self.locations)
    }

    /// The map's locations holding some copies but fewer than their target.
    pub fn thin_locations(&self) -> Vec<LocationShort> {
        self.locations
            .iter()
            .filter(|s| s.copies > 0)
            .cloned()
            .collect()
    }

    /// The findings in words, for a plan's listing: `"1 of 2 copies; no copy
    /// at offsite"`. Empty when there is no shortfall.
    pub fn describe(&self) -> String {
        let note = if self.multiplier > 1 {
            format!(" (tape-only {}x)", self.multiplier)
        } else {
            String::new()
        };
        let mut parts = Vec::new();
        if self.under_copied() {
            parts.push(format!(
                "{} of {} copies{note}",
                self.copies, self.min_copies
            ));
        }
        let missing = self.missing_locations();
        if !missing.is_empty() {
            parts.push(format!("no copy at {}", missing.join(", ")));
        }
        let thin = self.thin_locations();
        if !thin.is_empty() {
            parts.push(format!("{}{note}", describe_short_locations(&thin)));
        }
        parts.join("; ")
    }
}

/// [`Shortfall`] for unit `unit_id`, whose status is `unit_status`, under
/// its resolved policy `resolved`: the copy floor and every per-location
/// target through the one tape-only rule ([`copy_floor`]). One statement for
/// the copies, and one more only when the `copies_at` map has a target
/// ([`short_of_copies_at`]). A unit with no current snapshot has 0 copies and
/// is at no location, so a never-archived unit always falls short of a
/// non-zero floor.
pub fn shortfall(
    conn: &Connection,
    config: &crate::config::Config,
    unit_id: i64,
    unit_status: &str,
    resolved: &crate::policy::ResolvedPolicy,
) -> crate::error::Result<Shortfall> {
    let copies: i64 = conn.query_row(
        &format!(
            "SELECT {}",
            copy_count_expr(&CoverageQuery::current_unit("?1"))
        ),
        params![unit_id],
        |row| row.get(0),
    )?;
    let at = copies_at_targets(conn, NamedScope::Unit(unit_id), resolved, None)?;
    Ok(Shortfall::judge(config, resolved, unit_status, copies, &at))
}

/// [`shortfall`] under the unit's own resolved policy (`policy::resolve`,
/// whose error — a policy layer that cannot be read — is returned as is).
pub fn unit_shortfall(
    conn: &Connection,
    config: &crate::config::Config,
    unit: &crate::db::models::Unit,
) -> crate::error::Result<Shortfall> {
    shortfall(
        conn,
        config,
        unit.id,
        &unit.status,
        &crate::policy::resolve(conn, config, unit)?,
    )
}

// ── Copy distinctness at plan time (ADR-0012 2026-10-07 item 26, #144) ──

/// A copy that already exists of a version a write is about to put on a
/// destination volume, and what it shares with that destination.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExistingCopy {
    pub unit: String,
    pub version: i64,
    /// The volume holding the existing copy — for a warehouse deposit, the
    /// volume it was deposited from.
    pub volume: String,
    /// What is shared: the cartridge's barcode for
    /// [`Distinctness::same_cartridge`], the location's name for
    /// [`Distinctness::shared_location`].
    pub shared: String,
    /// The existing copy is a recorded warehouse deposit (ADR-0006), not the
    /// tape itself.
    pub deposit: bool,
    /// The run of slices the volume holds, `(first, last)`, when it holds a
    /// `range` of the version rather than the whole of it (#478); `None` for
    /// a whole copy.
    pub range: Option<(i64, i64)>,
}

impl ExistingCopy {
    fn describe(&self) -> String {
        let slices = self.range.map(|(first, last)| {
            if first == last {
                format!("slice {first}")
            } else {
                format!("slices {first}-{last}")
            }
        });
        match (self.deposit, slices) {
            (true, None) => format!(
                "{} v{} (a warehouse deposit of volume \"{}\")",
                self.unit, self.version, self.volume
            ),
            (false, None) => format!(
                "{} v{} (volume \"{}\")",
                self.unit, self.version, self.volume
            ),
            (true, Some(slices)) => format!(
                "{} v{} ({slices}, in a warehouse deposit of volume \"{}\")",
                self.unit, self.version, self.volume
            ),
            (false, Some(slices)) => format!(
                "{} v{} ({slices} on volume \"{}\")",
                self.unit, self.version, self.volume
            ),
        }
    }
}

/// How a write's destination stands against the copies its versions already
/// have: the two questions design v4 §2.17's "copies don't share volumes"
/// promise reduces to once a copy is identical content counted per version
/// (ADR-0012).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Distinctness {
    /// Existing copies on a volume of the destination's own cartridge. A
    /// second copy there is the same tape again — one copy, not two — so
    /// the write is REFUSED ([`Distinctness::refusal`]).
    ///
    /// No catalog tapectl writes can fill this. Every binding goes through
    /// `binding::mount_and_record` (`volume init`, `bind_late`, `catalog
    /// rebuild`), which marks every other volume mounted on the cartridge
    /// `erased` in the same transaction (ADR-0010), and the only other
    /// writer that closes a mount, `cartridge mark-erased`, erases too. So
    /// the destination's mount is never open beside a live copy's: by then
    /// the copy was displaced at `volume init`, whose warning names any unit
    /// left without a copy. This half is a guard against an inconsistent
    /// catalog (two live volumes on one cartridge), not a protection the
    /// operator meets.
    pub same_cartridge: Vec<ExistingCopy>,
    /// Existing copies at the destination's location (a tape shelved there,
    /// or a warehouse deposit there). Named in a WARNING only
    /// ([`Distinctness::warning`]): location policy is advisory (ADR-0004)
    /// and `audit` still reports a required location with no copy.
    pub shared_location: Vec<ExistingCopy>,
}

fn list_copies(copies: &[ExistingCopy]) -> String {
    copies
        .iter()
        .map(ExistingCopy::describe)
        .collect::<Vec<_>>()
        .join(", ")
}

impl Distinctness {
    /// The refusal for writing to `dest_label`, when one of its versions
    /// already has a copy on the destination's cartridge.
    pub fn refusal(&self, dest_label: &str) -> Option<String> {
        let first = self.same_cartridge.first()?;
        Some(format!(
            "volume \"{dest_label}\" is on cartridge \"{}\", which already holds a copy of \
             what this write would put on it: {}. Two copies on one cartridge are one copy, \
             not two (ADR-0012), so tapectl will not write it. tapectl never records two \
             live volumes on one cartridge (`volume init` marks the one it displaces \
             erased), so the catalog is inconsistent: load the cartridge and run \
             `tapectl volume identify` to learn which volume it really carries before \
             writing to either.",
            first.shared,
            list_copies(&self.same_cartridge),
        ))
    }

    /// The warning for writing to `dest_label`, when its versions already
    /// have a copy at the destination's location. Never a refusal.
    pub fn warning(&self, dest_label: &str) -> Option<String> {
        let first = self.shared_location.first()?;
        Some(format!(
            "warning: volume \"{dest_label}\" is at location \"{}\", where these versions \
             already have a copy: {}. Copies in one place are lost together. Writing anyway: \
             location policy is advisory (ADR-0004), and `tapectl audit` reports any required \
             location still without a copy.",
            first.shared,
            list_copies(&self.shared_location),
        ))
    }
}

/// [`Distinctness`] for writing the stage sets `stage_set_ids` to the volume
/// `dest_volume_id`: the existing copies of each one's VERSION (any stage
/// set of the same snapshot — ADR-0012 counts copies per version) that are
/// copies right now ([`eligible`], a completed write) and share the
/// destination's cartridge or location.
///
/// **The cartridge** is the destination's open mount in `cartridge_volumes`
/// and the existing copy's binding row (`UNIQUE(volume_id)`: a volume is
/// only ever bound to one cartridge). Binding erases a copy it displaces
/// (see [`Distinctness::same_cartridge`]), so this half only ever finds an
/// inconsistent catalog. A destination with no open mount — a
/// legacy volume `volume init` could not bind — has no cartridge the catalog
/// can compare, and nothing is refused on that half; its first contact binds
/// it (`bind_late`), and the File 0 check refuses a cartridge that still
/// carries another volume's identity.
///
/// **The location** is the destination's `location_id` against the same
/// union [`location_count_expr`] counts: an eligible copy's own
/// `location_id`, and a warehouse deposit of it. A destination with no
/// location shares none.
///
/// **A set that spans (#478; ADR-0012 items 122, 127 and 134).** Two holders
/// of the SAME slice must not share a place (item 127's per-slice location
/// count); two tapes holding different runs of one set are one copy, and item
/// 134 wants them kept together. So when the stage set being written has a
/// `range` row on an [`eligible`] volume, the destination takes the set's
/// least-covered slices next (the planner continues from the lowest per-slice
/// shortfall, item 122): those whose [`slice_copies`] equals the set's
/// minimum. A holder of the set at the destination's place is named only when
/// it holds one of them: a whole copy always does, and a range does when its
/// run meets them. A range holding none of them is the same copy's other part
/// and draws no warning. Item 93's tape 3 (owed slices K+1..N) beside tape 2
/// (whole) is warned; beside tape 1 (slices 1..K) it is not. A set with no
/// range row, or whose slices all have the same copies, keeps the rule above:
/// every slice is next. A holder of ANOTHER stage set of the version holds
/// different bytes of the same version (slices of two stage sets never
/// combine, item 102), and is named as before. A range holder is named with
/// its slice run.
pub fn copy_distinctness(
    conn: &Connection,
    dest_volume_id: i64,
    stage_set_ids: &[i64],
) -> crate::error::Result<Distinctness> {
    // The existing copies of stage set ?1's version: writes `holds` admits
    // of any stage set of the same snapshot, on an eligible volume other
    // than the destination ?2. Aliases as the copy counts use them.
    let copies_of = |holds: &str, extra_joins: &str, cond: &str, cols: &str| {
        format!(
            "SELECT {cols}
             FROM stage_sets ss
             JOIN snapshots cs ON cs.id = ss.snapshot_id
             JOIN units u ON u.id = cs.unit_id
             JOIN stage_sets css ON css.snapshot_id = cs.id
             JOIN writes cw ON cw.stage_set_id = css.id AND {holds}
             JOIN volumes cv ON cv.id = cw.volume_id
             JOIN volumes d ON d.id = ?2
             {extra_joins}
             WHERE ss.id = ?1 AND cv.id != ?2 AND {eligible} AND {cond}",
            eligible = eligible("cv"),
        )
    };
    let same_cartridge = copies_of(
        "cw.status = 'completed'",
        "JOIN cartridge_volumes cm ON cm.volume_id = cv.id
         JOIN cartridge_volumes dm ON dm.volume_id = d.id AND dm.unmounted_at IS NULL
         JOIN cartridges c ON c.id = dm.cartridge_id",
        "cm.cartridge_id = dm.cartridge_id",
        "u.name, cs.version, cv.label, c.barcode, 0",
    );
    // The location union `location_count_expr` counts: the tape's own
    // shelf, and a warehouse deposit of it; a whole copy's or a range's
    // (`holds_slices`, the counts' own reading of a holder). Each row also
    // carries the holder's stage set and write, and whether it is a range,
    // for the spanning rule above.
    let holds = holds_slices("cw");
    let range = status_in("cw", &["range"]);
    let shared_location = format!(
        "{} UNION {}",
        copies_of(
            &holds,
            "JOIN locations l ON l.id = d.location_id",
            "cv.location_id = d.location_id",
            &format!("u.name, cs.version, cv.label, l.name, 0, css.id, cw.id, {range}"),
        ),
        copies_of(
            &holds,
            "JOIN locations l ON l.id = d.location_id
             JOIN volume_deposits cd ON cd.volume_id = cv.id",
            "cd.location_id = d.location_id",
            &format!("u.name, cs.version, cv.label, l.name, 1, css.id, cw.id, {range}"),
        ),
    );

    let copy = |r: &rusqlite::Row| -> rusqlite::Result<ExistingCopy> {
        Ok(ExistingCopy {
            unit: r.get(0)?,
            version: r.get(1)?,
            volume: r.get(2)?,
            shared: r.get(3)?,
            deposit: r.get(4)?,
            range: None,
        })
    };
    let mut same: Vec<ExistingCopy> = Vec::new();
    let mut stmt = conn.prepare(&same_cartridge)?;
    for &ss in stage_set_ids {
        for row in stmt.query_map(params![ss, dest_volume_id], copy)? {
            same.push(row?);
        }
    }

    let mut shared: Vec<ExistingCopy> = Vec::new();
    let mut stmt = conn.prepare(&shared_location)?;
    for &ss in stage_set_ids {
        // The slices the destination takes next, when the set spans.
        let next = next_slices_of_a_spanning_set(conn, ss)?;
        let rows = stmt
            .query_map(params![ss, dest_volume_id], |r| {
                Ok((
                    copy(r)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, bool>(7)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (mut existing, holder_set, write_id, is_range) in rows {
            let held = if is_range {
                Some(range_held(conn, write_id)?)
            } else {
                None
            };
            let keep = match (&next, &held) {
                (Some(next), Some(held)) if holder_set == ss => {
                    held.iter().any(|(id, _)| next.contains(id))
                }
                _ => true,
            };
            if keep {
                existing.range = held.and_then(|held| {
                    let first = held.first()?.1;
                    let last = held.last()?.1;
                    Some((first, last))
                });
                shared.push(existing);
            }
        }
    }
    // Two stage sets of one version find the same copies.
    for out in [&mut same, &mut shared] {
        out.sort();
        out.dedup();
    }
    Ok(Distinctness {
        same_cartridge: same,
        shared_location: shared,
    })
}

/// The slices a write of the stage set `stage_set_id` takes next when the set
/// spans (it has a `range` row on an [`eligible`] volume): its least-covered
/// slices, those whose [`slice_copies`] equals the set's minimum (item 122).
/// `None` when the set has no such range row (every slice is next then).
fn next_slices_of_a_spanning_set(
    conn: &Connection,
    stage_set_id: i64,
) -> crate::error::Result<Option<std::collections::BTreeSet<i64>>> {
    // `writes` reads only the set, the status and eligibility; the
    // partition is item 138's, for the counts, and is not read here.
    let ranges = SliceCount {
        set: "?1",
        partition: Partition::Version("NULL"),
        exclude: None,
        counted: Counted::Copies,
    }
    .writes("range", "");
    let spans: bool = conn.query_row(
        &format!("SELECT EXISTS (SELECT 1 {ranges})"),
        params![stage_set_id],
        |r| r.get(0),
    )?;
    if !spans {
        return Ok(None);
    }
    let slices = slice_copies(conn, stage_set_id)?;
    let least = slices.iter().map(|s| s.copies).min();
    Ok(Some(
        slices
            .iter()
            .filter(|s| Some(s.copies) == least)
            .map(|s| s.stage_slice_id)
            .collect(),
    ))
}

/// The slices the `range` write `write_id` holds (`written`, as
/// [`range_holds`] reads them), as `(stage_slice_id, slice_number)` in slice
/// order.
fn range_held(conn: &Connection, write_id: i64) -> crate::error::Result<Vec<(i64, i64)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT cl.id, cl.slice_number
         FROM writes cw JOIN stage_slices cl ON cl.stage_set_id = cw.stage_set_id
         WHERE cw.id = ?1 AND {}
         ORDER BY cl.slice_number",
        range_holds("cl.id")
    ))?;
    let rows = stmt
        .query_map(params![write_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

// ── The retire family's floor (ADR-0008 Tier 3 / ADR-0012, issue #147) ──

/// One version of a unit whose coverage a retirement is about to consume
/// (a CURRENT one, or one still being spanned: [`versions_at_stake`]), and
/// what would be left of that version afterwards.
///
/// The rows exist only for versions the act ACTUALLY REMOVES coverage from
/// — see [`versions_at_stake`] for the three conditions — so the counts
/// here are always strictly "one fewer than before", never a restatement of
/// a shortfall that already existed.
#[derive(Debug, Clone)]
pub struct VersionAtStake {
    /// `snapshots.id` of the version's snapshot — what the by-name location
    /// predicates ([`short_of_copies_at`] with a volume excluded,
    /// [`short_of_location_floor_excluding`]) take as `NamedScope::Snapshot`.
    pub snapshot_id: i64,
    /// The volume being given up: the one [`versions_at_stake`] excluded to
    /// get the counts below, carried on the row so a consumer asking a
    /// further "after this volume is gone" question of the same version
    /// excludes the same volume (a cartridge's merged impacts span several).
    pub volume_id: i64,
    /// `snapshots.version` of the version's snapshot.
    pub version: i64,
    /// Copies this VERSION would still have, [`copy_count_expr`] with the
    /// volume excluded. `0` is ADR-0008's absolute floor.
    pub copies_after: i64,
    /// Distinct locations this VERSION would still be in,
    /// [`location_count_expr`] with the volume excluded.
    pub locations_after: i64,
}

/// The versions of `unit_id` whose coverage retiring `volume_id` consumes,
/// with what each would have left — the ONE derivation behind the retire
/// family's ADR-0008 tiers (`volume retire`, `cartridge retire`,
/// `volume compact-finish`; issue #147).
///
/// **Three conditions decide whether a version is at stake, and together
/// they are ADR-0012's "defined by what the act REMOVES".**
///
/// 1. The snapshot is `'current'` (or a span is in progress: below). A
///    `reclaimable` or `purged` version has been released by the operator
///    (`snapshot mark-reclaimable`, whose own `--force` is that statement in
///    so many words) and the floor does not protect what has been given up.
/// 2. The volume carries a write of that snapshot that REACHED THE TAPE
///    ([`write_reaches_tape`]: `completed`, `range`, `in_progress`, or
///    `interrupted` — issue #276. `in_progress`/`interrupted` are the same
///    physical fact as `completed` here: bytes `seal()`ed onto tape whose
///    confirm outcome the catalog has not recorded either way, see
///    [`write_reaches_tape`]'s own doc for why; `range` is a spannable
///    set's slices on this tape, ADR-0012 item 121). Coverage it never held
///    cannot be coverage it removes.
/// 3. The volume itself HOLDS SEALED BYTES right now
///    ([`holds_sealed_bytes`], issue #276 — a strict superset of
///    [`eligible`], widened by exactly the sealed-but-unconfirmed state).
///    A quarantined, never-sealed or already-retired volume counts as
///    nothing in every derivation in this module, so retiring it removes
///    nothing — ADR-0012 says so in as many words, and it is the escape the
///    hard case needs: a tape with read errors is quarantined by a failed
///    `volume verify`, and retiring it is then Tier 2 at most. Getting this
///    wrong in the "safe" direction would make the command useless exactly
///    when an operator needs it most.
///
/// Those three together mean excluding the volume reduces each returned
/// version's count by exactly one, so `copies_after == 0` is precisely "this
/// was the last eligible copy of a live version" — ADR-0012's per-version
/// reading of "last one" (issue #153: a unit with v1 elsewhere and v2 only
/// here has ZERO remaining for v2, and that is the case the floor exists
/// for), not a question about the unit as a whole.
///
/// **A span in progress (#478, ADR-0012 item 128).** A spanning version is
/// promoted `current` only once every slice of a stage set of it is on a
/// sealed volume ([`every_slice_on_sealed_volume_sql`]), and until then
/// condition 1 would not see it. With staging released slice by slice (item
/// 129), at a copy target of 1 tape 1's range may be the only place its
/// slices are. So a version that is neither current nor released
/// (`reclaimable`/`purged` are released, as in condition 1) is at stake too
/// when the volume holds a `range` of it, is [`eligible`], and is the only
/// holder of at least one of the range's slices: no other eligible volume
/// has a `written` position of it in a `completed` or `range` write. That is
/// [`sole_slices`]' own reading of a copy elsewhere, so the floor and the
/// rewrite's list agree about which slices a volume alone holds. Its
/// `copies_after` is the version's count without the volume, as for a
/// current one, and it is 0 whenever the volume alone held a slice.
///
/// **`copies_after`/`locations_after` do NOT widen with condition 3.** They
/// stay on [`copy_count_expr`]/[`location_count_expr`], which route through
/// [`eligible`] — never [`holds_sealed_bytes`] — because an unconfirmed
/// volume is not a Copy and must not inflate the remaining count. This
/// asymmetry (widen the SUBJECT of the floor, never the COUNT the floor
/// measures against) is the entire correctness of issue #276's fix. One
/// deliberate, conservative consequence: a unit whose only holdings are TWO
/// sealed-but-unconfirmed volumes now refuses retiring EITHER one, because
/// neither counts as a copy that rescues the other.
///
/// Counted through [`copy_count_expr`] and [`location_count_expr`] at
/// `Snapshot` scope, never hand-written: a warehouse deposit of some other
/// eligible volume is a copy here exactly as it is in `audit` and the
/// reports (ADR-0006, issue #73), and issue #96 is the record of what a
/// seventh hand-written copy of this SQL costs.
pub fn versions_at_stake(
    conn: &Connection,
    unit_id: i64,
    volume_id: i64,
) -> crate::error::Result<Vec<VersionAtStake>> {
    let after = |q: &CoverageQuery| (copy_count_expr(q), location_count_expr(q));
    let q = CoverageQuery {
        scope: CoverageScope::Snapshot { id_expr: "s.id" },
        exclude_volume: Some("?2"),
    };
    let (copies_after, locations_after) = after(&q);
    // `DISTINCT` because a snapshot can have several stage_sets written to
    // the same volume; the two scalar subqueries depend only on `s.id`, so
    // the duplicate rows they would produce are identical.
    let sql = format!(
        "SELECT DISTINCT s.id, s.version, {copies_after} AS copies_after,
                {locations_after} AS locations_after
         FROM snapshots s
         JOIN stage_sets ss ON ss.snapshot_id = s.id
         JOIN writes w ON w.stage_set_id = ss.id
         JOIN volumes v ON v.id = w.volume_id
         WHERE s.unit_id = ?1 AND w.volume_id = ?2
           AND ((s.status = 'current' AND {reaches} AND {sealed_bytes})
                OR (s.status NOT IN ('current', 'reclaimable', 'purged')
                    AND w.status = 'range' AND {eligible}
                    AND EXISTS (SELECT 1 FROM write_positions sp
                                 WHERE sp.write_id = w.id AND sp.status = 'written'
                                   AND NOT {elsewhere})))
         ORDER BY s.version",
        reaches = write_reaches_tape("w"),
        sealed_bytes = holds_sealed_bytes("v"),
        eligible = eligible("v"),
        elsewhere = held_elsewhere("sp.stage_slice_id", "?2"),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params![unit_id, volume_id], |row| {
            Ok(VersionAtStake {
                snapshot_id: row.get(0)?,
                volume_id,
                version: row.get(1)?,
                copies_after: row.get(2)?,
                locations_after: row.get(3)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

// ── What a `volume rewrite` asks of the catalog (ADR-0012 2026-10-19 item 106) ──

/// A SQL boolean: the slice `slice_expr` (a `stage_slices` id) has a
/// `written` position in a `completed` or `range` write on an [`eligible`]
/// volume other than `volume_expr`: "a copy elsewhere", as [`sole_slices`]
/// and [`versions_at_stake`]' floor for a span in progress both ask it, so
/// the two never disagree about which slices a volume alone holds. A
/// warehouse deposit is not a copy elsewhere here: the bytes these callers
/// rely on are on a cartridge. Aliases `wp2`, `w2`, `v2`.
fn held_elsewhere(slice_expr: &str, volume_expr: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM write_positions wp2
                 JOIN writes w2 ON w2.id = wp2.write_id
                 JOIN volumes v2 ON v2.id = w2.volume_id
                 WHERE wp2.stage_slice_id = {slice_expr} AND w2.volume_id <> {volume_expr}
                   AND {holds} AND wp2.status = 'written' AND {eligible})",
        holds = holds_slices("w2"),
        eligible = eligible("v2"),
    )
}

/// One live slice of a volume that no other eligible volume carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoleSlice {
    pub stage_slice_id: i64,
    pub stage_set_id: i64,
    pub unit_name: String,
    /// `snapshots.version` of the slice's snapshot.
    pub version: i64,
    pub encrypted_bytes: i64,
}

/// The live slices of `volume_id` that have NO copy on another volume that is
/// [`eligible`] right now: the bytes whose only home, once the volume is given
/// up, would be the staging disk a `volume rewrite` read them into.
///
/// "Live" is `compact-read`'s own reading: a slice of a completed write whose
/// snapshot is neither `reclaimable` nor `purged`, so the slices returned are
/// a subset of exactly the slices that command stages. "A copy elsewhere" is a
/// `written` position of the same slice in a completed write on another
/// eligible volume — a retired, erased, quarantined or unsealed volume carries
/// nothing that counts, and neither does a warehouse deposit (the bytes a
/// rewrite's operator can rely on are on a cartridge). That is deliberately
/// stricter than `compact-finish`'s own gate, which asks only for a completed
/// write on another volume of any status.
///
/// From 1.7.0 a `range` write counts on both sides (#478, ADR-0012 item 128):
/// its `written` slices are on the volume, the range may be the only place a
/// slice of a span in progress is, and a range elsewhere holds its own run.
/// Both sides read `written` positions, as they always did, where the copy
/// counts read a `completed` write as holding every slice by its status; the
/// two agree on every catalog the session writes, which records a `written`
/// position for every slice of a completed write. The other side is
/// [`held_elsewhere`], which [`versions_at_stake`]' floor for a span in
/// progress shares.
pub fn sole_slices(conn: &Connection, volume_id: i64) -> crate::error::Result<Vec<SoleSlice>> {
    let sql = format!(
        "SELECT sl.id, sts.id, u.name, s.version, sl.encrypted_bytes
         FROM write_positions wp
         JOIN writes w ON w.id = wp.write_id
         JOIN stage_slices sl ON sl.id = wp.stage_slice_id
         JOIN stage_sets sts ON sts.id = sl.stage_set_id
         JOIN snapshots s ON s.id = sts.snapshot_id
         JOIN units u ON u.id = s.unit_id
         WHERE w.volume_id = ?1 AND {holds} AND wp.status = 'written'
           AND s.status NOT IN ('reclaimable', 'purged')
           AND NOT {elsewhere}
         GROUP BY sl.id
         ORDER BY u.name, s.version, sl.slice_number",
        holds = holds_slices("w"),
        elsewhere = held_elsewhere("wp.stage_slice_id", "?1"),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![volume_id], |row| {
            Ok(SoleSlice {
                stage_slice_id: row.get(0)?,
                stage_set_id: row.get(1)?,
                unit_name: row.get(2)?,
                version: row.get(3)?,
                encrypted_bytes: row.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// A SQL boolean expression: stage set `stage_set_expr` has a copy on an
/// [`eligible`] volume that was FULLY READ BACK — a `completed` write with
/// `write_verified = 1`, which only an Integrity confirm or a passing full
/// `volume verify` sets (ADR-0012 2026-10-06 item 24). It is the fact that
/// lifts a pending rewrite's hold on a staged set: item 106 releases the
/// staged slices only after the new volume has passed a full readback, and
/// asking for the fact rather than the command that produced it makes the
/// release independent of which command ran the readback.
///
/// Embeds `writes`/`volumes` under the aliases `hw`/`hv`; `stage_set_expr`
/// is the caller's own column or placeholder.
pub fn fully_read_back_copy(stage_set_expr: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM writes hw JOIN volumes hv ON hv.id = hw.volume_id
                 WHERE hw.stage_set_id = {stage_set_expr} AND hw.status = 'completed'
                   AND hw.write_verified = 1 AND {})",
        eligible("hv")
    )
}

// ── The cross-volume index's versions and holders (ADR-0012 items 78-80) ──

/// The volumes the cross-volume index (`volume-format-v2.md` §1.5) names as
/// holding a version: sealed, and neither retired nor erased, **whatever
/// their condition**. A volume has one `status`, so `sealed` alone says
/// "sealed and unretired"; a verify-quarantined volume (`observed_condition`,
/// [`condition_ok`]) is still `sealed`, and the index lists it, marked, since
/// a medium proven bad may still read in part (item 80).
///
/// A FOURTH question beside [`eligible`] ("is this a copy?"), [`in_service`]
/// ("is this inventory?") and [`holds_sealed_bytes`] ("does it physically hold
/// restorable bytes?"), and it is none of them: [`eligible`] drops the
/// quarantined volume the index keeps, [`in_service`] adds the imported
/// `active` volume the index leaves out (no tapectl layout wrote it), and
/// [`holds_sealed_bytes`] adds the sealed-but-unconfirmed volume (status
/// still `initialized`) §1.5 calls unsealed. **Never a copy count.**
pub fn index_holder(volume_alias: &str) -> String {
    status_in(volume_alias, &["sealed"])
}

/// What an index row says of the volume it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexHolderState {
    /// Sealed, and no reason to distrust the medium.
    Sealed,
    /// Sealed, but a verify proved the medium bad (it may still partly read).
    Quarantined,
    /// The tape the index is on.
    ThisTape,
}

impl IndexHolderState {
    /// The spelling in `units.tsv`'s `state` column.
    pub fn as_str(self) -> &'static str {
        match self {
            IndexHolderState::Sealed => "sealed",
            IndexHolderState::Quarantined => "quarantined",
            IndexHolderState::ThisTape => "this-tape",
        }
    }
}

/// The tape a write is building, as File 0 will say it — taken from the
/// write's own facts rather than the catalog, because the binding that gives
/// the catalog its cartridge serial runs AFTER the Layout is built, and the
/// index must agree with File 0 on what it says of this tape.
#[derive(Debug, Clone, Copy)]
pub struct IndexThisTape<'a> {
    pub label: &'a str,
    pub uuid: &'a str,
    /// The chip serial, or `""` when none was read (an operator-named
    /// cartridge's barcode is not one).
    pub cartridge_serial: &'a str,
    /// File 0's `media_type`.
    pub generation: &'a str,
}

/// One stage set a write carries, and the run of its slices this tape's plan
/// holds: what the `this-tape` row of the index says in `first_slice` and
/// `last_slice` (`volume-format-v2.md` §1.5; ADR-0012 item 126). A whole set
/// is the run 1 to its number of slices; a spannable set's range is the one
/// unbroken run this tape plans (item 122), which the seal's `ended_after`
/// may end sooner (§4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexRun {
    pub stage_set_id: i64,
    /// The first and last slice of the run, by slice number; `None` for the
    /// whole set.
    pub slices: Option<(i64, i64)>,
}

impl IndexRun {
    /// `stage_set_id`, whole.
    pub fn whole(stage_set_id: i64) -> Self {
        IndexRun {
            stage_set_id,
            slices: None,
        }
    }

    /// Each of `stage_set_ids`, whole: the runs of a write that carries no
    /// range.
    pub fn whole_sets(stage_set_ids: &[i64]) -> Vec<Self> {
        stage_set_ids.iter().copied().map(IndexRun::whole).collect()
    }
}

/// The run of a whole stage set of `num_slices` slices: 1 to `num_slices`,
/// or `(0, 0)` ("not known") for a set with none recorded.
fn whole_run(num_slices: i64) -> (i64, i64) {
    if num_slices > 0 {
        (1, num_slices)
    } else {
        (0, 0)
    }
}

/// One (version, holding volume) pair of the index: a row of `units.tsv`
/// before it is formatted. Facts only; the file's sort order and spelling are
/// `volume::index`'s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexVersionRow {
    pub tenant_id: i64,
    pub tenant: String,
    pub unit_id: i64,
    pub unit: String,
    pub unit_uuid: String,
    pub snapshot_id: i64,
    pub version: i64,
    /// The holder's own stage set's slices (`COUNT` over `stage_slices`, not
    /// the nullable `stage_sets.num_slices`).
    pub num_slices: i64,
    /// Their ciphertext bytes, all slices.
    pub version_bytes: i64,
    /// The run of those slices the volume holds, by slice number (§1.5): `1`
    /// and `num_slices` for a whole (`completed`) write; for a `range` write
    /// the first and last slice the catalog records `written` there; on this
    /// tape's row the run its plan holds ([`IndexRun`]). `0` when not known:
    /// a stage set with no slices recorded, or a range with no slice written.
    pub first_slice: i64,
    pub last_slice: i64,
    pub volume: String,
    pub volume_uuid: String,
    pub cartridge_serial: String,
    pub generation: String,
    pub state: IndexHolderState,
    /// The catalog's `volumes.sealed_at` text (`YYYY-MM-DD HH:MM:SS`, UTC);
    /// empty for this tape, and when the catalog recorded none.
    pub sealed_at: String,
}

/// [`index_versions_for_runs`] for a write that carries each of
/// `write_stage_sets` whole: this tape's rows say 1 to each set's number of
/// slices.
pub fn index_versions(
    conn: &Connection,
    this: &IndexThisTape<'_>,
    write_stage_sets: &[i64],
) -> crate::error::Result<Vec<IndexVersionRow>> {
    index_versions_for_runs(conn, this, &IndexRun::whole_sets(write_stage_sets))
}

/// **The** cross-volume index query (ADR-0012 item 80, `volume-format-v2.md`
/// §1.5): every version the index lists, with each volume that holds it.
///
/// The versions are every CURRENT version with at least one `completed` or
/// `range` write on an [`index_holder`] volume, plus every version
/// `write_runs` carries — by id, never by status: at build time this write's
/// snapshots are `staged`, not yet `current`. The holders of a listed version
/// are its `completed` and `range` writes on [`index_holder`] volumes
/// (quarantined ones marked), each with the run of slices it holds, and this
/// tape, as [`IndexHolderState::ThisTape`], for each version `write_runs`
/// carries, with the run its plan holds (§1.5 "Ranges", 1.7.0; ADR-0012 items
/// 121, 126 and 141). A holder of a range is listed with its run and is no
/// copy on its own (item 127): the index never joins runs, a reader takes a
/// slice from another tape only by its hash (§4.4 R8). A spanning version is
/// current only once every slice of a stage set of it is on a sealed volume
/// (item 128), so until then it is listed only by a write that carries it.
/// Unsealed, retired and erased volumes hold nothing here, and a reclaimable
/// or purged version is listed only when this write itself carries it.
///
/// ONE statement for the holders and one for this tape's own rows, for every
/// tenant at once (the caller splits by `tenant_id`), however many units
/// there are. One row per (snapshot, volume): a snapshot with two stage sets
/// on one volume reports the lower stage set id's. Unordered;
/// `volume::index` sorts.
pub fn index_versions_for_runs(
    conn: &Connection,
    this: &IndexThisTape<'_>,
    write_runs: &[IndexRun],
) -> crate::error::Result<Vec<IndexVersionRow>> {
    let write_stage_sets: Vec<i64> = write_runs.iter().map(|r| r.stage_set_id).collect();
    // ?1 is this tape's label; ?2.. are the write's stage sets.
    let in_list = (0..write_stage_sets.len())
        .map(|i| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");
    let slices = |stage_set: &str| {
        format!(
            "(SELECT COUNT(*) FROM stage_slices sl WHERE sl.stage_set_id = {stage_set}),
             (SELECT COALESCE(SUM(sl.encrypted_bytes), 0) FROM stage_slices sl
               WHERE sl.stage_set_id = {stage_set})"
        )
    };
    // A range's run: the lowest and highest slice it has `written`.
    let written_run = |end: &str| {
        format!(
            "(SELECT {end}(sl.slice_number) FROM write_positions wp
               JOIN stage_slices sl ON sl.id = wp.stage_slice_id
              WHERE wp.write_id = w.id AND wp.status = 'written')"
        )
    };
    let holders_sql = format!(
        "SELECT t.id, t.name, u.id, u.name, u.uuid, s.id, s.version,
                {holder_slices},
                v.label, COALESCE(v.uuid, ''), COALESCE(c.serial_number, ''),
                COALESCE(v.media_type, ''),
                COALESCE(v.sealed_at, ''),
                CASE WHEN {condition_ok} THEN 0 ELSE 1 END, w.stage_set_id,
                w.status = 'range',
                CASE WHEN w.status = 'range' THEN {first} END,
                CASE WHEN w.status = 'range' THEN {last} END
         FROM writes w
         JOIN stage_sets ss ON ss.id = w.stage_set_id
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN units u ON u.id = s.unit_id
         JOIN tenants t ON t.id = u.tenant_id
         JOIN volumes v ON v.id = w.volume_id
         -- The cartridge the volume was written on, mounted or no (a volume
         -- has one cartridge_volumes row): not `unmounted_at IS NULL`, which
         -- would drop the serial of a sealed volume whose mount has closed.
         LEFT JOIN cartridge_volumes cv ON cv.volume_id = v.id
         LEFT JOIN cartridges c ON c.id = cv.cartridge_id
         WHERE {holds} AND {holder} AND v.label <> ?1
           AND (s.status = 'current'
                OR s.id IN (SELECT snapshot_id FROM stage_sets WHERE id IN ({in_list})))",
        holder_slices = slices("w.stage_set_id"),
        condition_ok = condition_ok("v"),
        first = written_run("MIN"),
        last = written_run("MAX"),
        holds = holds_slices("w"),
        holder = index_holder("v"),
    );
    let this_sql = format!(
        "SELECT t.id, t.name, u.id, u.name, u.uuid, s.id, s.version,
                {this_slices}, ss.id
         FROM stage_sets ss
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN units u ON u.id = s.unit_id
         JOIN tenants t ON t.id = u.tenant_id
         WHERE ss.id IN ({in_list})",
        this_slices = slices("ss.id"),
    );

    let mut bound: Vec<rusqlite::types::Value> = vec![this.label.to_string().into()];
    bound.extend(write_stage_sets.iter().map(|id| (*id).into()));
    let planned: std::collections::HashMap<i64, Option<(i64, i64)>> = write_runs
        .iter()
        .map(|r| (r.stage_set_id, r.slices))
        .collect();

    // (row, the stage set it came from: the tie-break below).
    let mut rows: Vec<(IndexVersionRow, i64)> = Vec::new();
    {
        let mut stmt = conn.prepare(&holders_sql)?;
        let mapped = stmt.query_map(rusqlite::params_from_iter(bound.iter()), |r| {
            let quarantined: i64 = r.get(14)?;
            let num_slices: i64 = r.get(7)?;
            let (first_slice, last_slice) = if r.get::<_, bool>(16)? {
                (
                    r.get::<_, Option<i64>>(17)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(18)?.unwrap_or(0),
                )
            } else {
                whole_run(num_slices)
            };
            Ok((
                IndexVersionRow {
                    tenant_id: r.get(0)?,
                    tenant: r.get(1)?,
                    unit_id: r.get(2)?,
                    unit: r.get(3)?,
                    unit_uuid: r.get(4)?,
                    snapshot_id: r.get(5)?,
                    version: r.get(6)?,
                    num_slices,
                    version_bytes: r.get(8)?,
                    first_slice,
                    last_slice,
                    volume: r.get(9)?,
                    volume_uuid: r.get(10)?,
                    cartridge_serial: r.get(11)?,
                    generation: r.get(12)?,
                    state: if quarantined == 0 {
                        IndexHolderState::Sealed
                    } else {
                        IndexHolderState::Quarantined
                    },
                    sealed_at: r.get(13)?,
                },
                r.get::<_, i64>(15)?,
            ))
        })?;
        for row in mapped {
            rows.push(row?);
        }
    }
    if !write_stage_sets.is_empty() {
        let mut stmt = conn.prepare(&this_sql)?;
        let mapped = stmt.query_map(rusqlite::params_from_iter(bound.iter()), |r| {
            let num_slices: i64 = r.get(7)?;
            let stage_set_id: i64 = r.get(9)?;
            let (first_slice, last_slice) = match planned.get(&stage_set_id).copied().flatten() {
                Some(run) => run,
                None => whole_run(num_slices),
            };
            Ok((
                IndexVersionRow {
                    tenant_id: r.get(0)?,
                    tenant: r.get(1)?,
                    unit_id: r.get(2)?,
                    unit: r.get(3)?,
                    unit_uuid: r.get(4)?,
                    snapshot_id: r.get(5)?,
                    version: r.get(6)?,
                    num_slices,
                    version_bytes: r.get(8)?,
                    first_slice,
                    last_slice,
                    volume: this.label.to_string(),
                    volume_uuid: this.uuid.to_string(),
                    cartridge_serial: this.cartridge_serial.to_string(),
                    generation: this.generation.to_string(),
                    state: IndexHolderState::ThisTape,
                    sealed_at: String::new(),
                },
                stage_set_id,
            ))
        })?;
        for row in mapped {
            rows.push(row?);
        }
    }

    // One row per (snapshot, volume): the lowest stage set's.
    rows.sort_by(|a, b| {
        (a.0.snapshot_id, &a.0.volume, a.1).cmp(&(b.0.snapshot_id, &b.0.volume, b.1))
    });
    rows.dedup_by(|later, kept| {
        kept.0.snapshot_id == later.0.snapshot_id && kept.0.volume == later.0.volume
    });
    Ok(rows.into_iter().map(|(row, _)| row).collect())
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

/// `{alias}.status NOT IN ('a','b',...)`. Sibling to [`status_in`] for an
/// EXCLUSION list (issue #276's [`holds_sealed_bytes`]), so the quoting
/// discipline stays written in exactly one place for both directions.
fn status_not_in(volume_alias: &str, statuses: &[&str]) -> String {
    let list = statuses
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{volume_alias}.status NOT IN ({list})")
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

    /// One more copy for [`setup_unit_with_deposit`]'s unit: a sealed volume
    /// `label` shelved at the existing location `location`, holding the
    /// unit's one stage set (a completed write).
    pub(crate) fn add_tape_copy_at(conn: &Connection, label: &str, location: &str) {
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status, location_id)
             VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed',
                     (SELECT id FROM locations WHERE name = ?2))",
            params![label, location],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             SELECT ss.id, ss.snapshot_id, ?1, 'completed' FROM stage_sets ss",
            params![conn.last_insert_rowid()],
        )
        .unwrap();
    }

    fn scalar(conn: &Connection, expr: &str, unit_id: i64) -> i64 {
        conn.query_row(&format!("SELECT {expr}"), params![unit_id], |r| r.get(0))
            .unwrap()
    }

    // ── shortfall (issue #144) ──

    /// A resolved policy with `min_copies` and `required_locations` set, the
    /// names folded into `copies_at` at one copy each as `policy::resolve`
    /// folds an archive set's (#493, ADR-0012 item 156).
    fn policy(
        conn: &Connection,
        min_copies: i64,
        required: &[&str],
    ) -> crate::policy::ResolvedPolicy {
        let unit = crate::db::queries::get_unit_by_name(conn, "photos")
            .unwrap()
            .unwrap();
        let mut resolved =
            crate::policy::resolve(conn, &crate::config::Config::default(), &unit).unwrap();
        resolved.min_copies = min_copies;
        resolved.required_locations = names(required);
        resolved.copies_at = required.iter().map(|n| (n.to_string(), 1)).collect();
        resolved
    }

    /// The fixture's unit has two copies (a tape at `home`, its deposit at
    /// `glacier`): judged by the audit's own two predicates it is short of
    /// three copies and of `offsite`, and short of nothing against two
    /// copies at `home`.
    #[test]
    fn shortfall_is_the_audits_copy_and_location_findings() {
        let (conn, unit_id, _vol) = setup_unit_with_deposit("active");
        let config = crate::config::Config::default();
        let short = shortfall(
            &conn,
            &config,
            unit_id,
            "active",
            &policy(&conn, 3, &["home", "offsite"]),
        )
        .unwrap();
        assert_eq!(short.copies, 2);
        assert!(short.under_copied());
        assert_eq!(short.missing_locations(), names(&["offsite"]));
        assert!(short.any());
        assert_eq!(short.describe(), "2 of 3 copies; no copy at offsite");

        let met = shortfall(
            &conn,
            &config,
            unit_id,
            "active",
            &policy(&conn, 2, &["home"]),
        )
        .unwrap();
        assert!(!met.any(), "{met:?}");
        assert_eq!(met.describe(), "");

        // ADR-0012 2026-10-07 item 34, as `location_presence` judges it: at
        // the tape-only multiplier (2 by default) each named location is
        // asked for `per_name` copies, so one copy at `home` is short of
        // two. `min_copies = 1` keeps the copy floor (1 x 2) met, so the
        // named half is the only finding.
        let thin = shortfall(
            &conn,
            &config,
            unit_id,
            "tape_only",
            &policy(&conn, 1, &["home"]),
        )
        .unwrap();
        assert!(!thin.under_copied(), "{thin:?}");
        assert!(thin.missing_locations().is_empty());
        assert_eq!(
            thin.thin_locations(),
            vec![LocationShort {
                location: "home".to_string(),
                copies: 1,
                target: 2
            }]
        );
        assert!(thin.any());
        assert_eq!(
            thin.describe(),
            "1 copy at required location home, needs 2 (tape-only 2x)"
        );
    }

    /// ADR-0012 2026-10-12 item 43: a tape-only unit's copy floor is
    /// `min_copies` times the configured multiplier, so the two copies that
    /// meet `min_copies = 2` for an active unit are short of six at 3x —
    /// the one derivation `audit`'s `copy_count`, `collection status` and
    /// `--policy-aware` share. The same unit judged active is the positive
    /// control: nothing is multiplied.
    #[test]
    fn shortfall_multiplies_the_copy_floor_of_a_tape_only_unit_only() {
        let (conn, unit_id, _vol) = setup_unit_with_deposit("tape_only");
        let mut config = crate::config::Config::default();
        config.compaction.tape_only_safety_multiplier = 3;
        let resolved = policy(&conn, 2, &[]);

        let tape_only = shortfall(&conn, &config, unit_id, "tape_only", &resolved).unwrap();
        assert_eq!((tape_only.copies, tape_only.min_copies), (2, 6));
        assert!(tape_only.under_copied());
        assert_eq!(tape_only.describe(), "2 of 6 copies (tape-only 3x)");

        let active = shortfall(&conn, &config, unit_id, "active", &resolved).unwrap();
        assert_eq!(active.min_copies, 2);
        assert!(!active.any(), "{active:?}");
    }

    // ── copy_distinctness (issue #144) ──

    /// The fixture plus a destination `L6-0009` (initialised, nothing
    /// written) at `dest_location`, with the existing copy `L6-0003` and the
    /// destination bound to cartridges `copy_cart` and `dest_cart`. Returns
    /// the connection, the destination's id and the fixture's stage set.
    fn distinctness_fixture(
        dest_location: Option<&str>,
        copy_cart: &str,
        dest_cart: &str,
    ) -> (Connection, i64, i64) {
        let (conn, _unit, vol) = setup_unit_with_deposit("active");
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status, location_id)
             VALUES ('L6-0009', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized',
                     (SELECT id FROM locations WHERE name = ?1))",
            params![dest_location],
        )
        .unwrap();
        let dest = conn.last_insert_rowid();
        for barcode in [copy_cart, dest_cart] {
            conn.execute(
                "INSERT OR IGNORE INTO cartridges (barcode, media_type, nominal_capacity)
                 VALUES (?1, 'LTO-6', 2500000000000)",
                params![barcode],
            )
            .unwrap();
        }
        for (volume, barcode) in [(vol, copy_cart), (dest, dest_cart)] {
            conn.execute(
                "INSERT INTO cartridge_volumes (cartridge_id, volume_id)
                 VALUES ((SELECT id FROM cartridges WHERE barcode = ?1), ?2)",
                params![barcode, volume],
            )
            .unwrap();
        }
        let ss: i64 = conn
            .query_row("SELECT id FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        (conn, dest, ss)
    }

    fn copy(volume: &str, shared: &str, deposit: bool) -> ExistingCopy {
        ExistingCopy {
            unit: "photos".into(),
            version: 1,
            volume: volume.into(),
            shared: shared.into(),
            deposit,
            range: None,
        }
    }

    /// ADR-0012 2026-10-07 item 26: a destination on the cartridge that
    /// already holds the version's copy is named for refusal — that is one
    /// copy, not two. The fixture's raw INSERTs build a state no tapectl
    /// path produces (binding erases the copy it displaces — see
    /// `binding_a_destination_onto_the_copys_cartridge_erases_the_copy_first`),
    /// so this pins the guard against an inconsistent catalog.
    #[test]
    fn a_destination_on_the_copys_own_cartridge_is_refused() {
        let (conn, dest, ss) = distinctness_fixture(None, "BC1", "BC1");
        let d = copy_distinctness(&conn, dest, &[ss]).unwrap();
        assert_eq!(d.same_cartridge, vec![copy("L6-0003", "BC1", false)]);
        let refusal = d.refusal("L6-0009").expect("one cartridge is one copy");
        assert!(
            refusal.contains("cartridge \"BC1\"")
                && refusal.contains("photos v1 (volume \"L6-0003\")")
                && refusal.contains("one copy"),
            "{refusal}"
        );
    }

    /// A copy that is no longer a copy (its volume erased) is not a clash,
    /// and two cartridges are two copies.
    #[test]
    fn only_a_live_copy_on_the_same_cartridge_is_refused() {
        let (conn, dest, ss) = distinctness_fixture(None, "BC1", "BC2");
        let d = copy_distinctness(&conn, dest, &[ss]).unwrap();
        assert_eq!(d, Distinctness::default());
        assert_eq!(d.refusal("L6-0009"), None);
        assert_eq!(d.warning("L6-0009"), None);

        let (conn, dest, ss) = distinctness_fixture(None, "BC1", "BC1");
        conn.execute(
            "UPDATE volumes SET status = 'erased' WHERE label = 'L6-0003'",
            [],
        )
        .unwrap();
        let d = copy_distinctness(&conn, dest, &[ss]).unwrap();
        assert!(d.same_cartridge.is_empty(), "{d:?}");
    }

    /// A destination at a location that already holds a copy — the tape at
    /// `home`, or the warehouse deposit at `glacier` — is named in a warning,
    /// never a refusal (ADR-0004).
    #[test]
    fn a_shared_location_is_a_warning_naming_the_copy_there() {
        let (conn, dest, ss) = distinctness_fixture(Some("home"), "BC1", "BC2");
        let d = copy_distinctness(&conn, dest, &[ss]).unwrap();
        assert!(d.same_cartridge.is_empty());
        assert_eq!(d.refusal("L6-0009"), None);
        assert_eq!(d.shared_location, vec![copy("L6-0003", "home", false)]);
        let warning = d.warning("L6-0009").unwrap();
        assert!(
            warning.starts_with("warning:")
                && warning.contains("location \"home\"")
                && warning.contains("photos v1 (volume \"L6-0003\")"),
            "{warning}"
        );

        let (conn, dest, ss) = distinctness_fixture(Some("glacier"), "BC1", "BC2");
        let d = copy_distinctness(&conn, dest, &[ss]).unwrap();
        assert_eq!(d.shared_location, vec![copy("L6-0003", "glacier", true)]);
        assert!(d
            .warning("L6-0009")
            .unwrap()
            .contains("a warehouse deposit of volume \"L6-0003\""));
    }

    // ── copy_distinctness for a set that spans (#478; items 122, 127, 134) ──

    /// Item 93's layout at tape 3's plan: SPAN-1 holds slices 1-2 at `home`,
    /// SPAN-2 holds the version whole at `vault`, and the destination SPAN-3
    /// (initialised, nothing written) is at `dest_place`. Slices 1-2 have two
    /// copies, 3-6 one, so 3-6 are next. Returns the catalog, the destination
    /// and the set.
    fn tape_3_of_item_93(dest_place: &str) -> (SpanCatalog, i64, i64) {
        let c = SpanCatalog::new();
        let snap = c.version(1, "current");
        let set = c.stage_set(snap, 6);
        let t1 = c.volume("SPAN-1", Some("home"));
        let t2 = c.volume("SPAN-2", Some("vault"));
        c.range(set, t1, 1, 2, 6);
        c.whole(set, 6, t2);
        let dest = c.volume("SPAN-3", Some(dest_place));
        c.conn
            .execute(
                "UPDATE volumes SET status = 'initialized', sealed_at = NULL WHERE id = ?1",
                params![dest],
            )
            .unwrap();
        (c, dest, set)
    }

    fn span_copy(volume: &str, shared: &str, range: Option<(i64, i64)>) -> ExistingCopy {
        ExistingCopy {
            unit: "span".into(),
            version: 1,
            volume: volume.into(),
            shared: shared.into(),
            deposit: false,
            range,
        }
    }

    /// Tape 3 takes slices 3-6. Beside tape 1 (slices 1-2) it is the same
    /// copy's other part, kept together (item 134): no warning. Beside tape
    /// 2, which holds slices 3-6 too, two holders of one slice share a place
    /// (item 127): the warning, its text a whole copy's, byte for byte.
    #[test]
    fn tape_3_beside_tape_1_is_one_copy_and_beside_tape_2_is_warned() {
        let (c, dest, set) = tape_3_of_item_93("home");
        let d = copy_distinctness(&c.conn, dest, &[set]).unwrap();
        assert_eq!(d, Distinctness::default(), "{d:?}");
        assert_eq!(d.warning("SPAN-3"), None);

        let (c, dest, set) = tape_3_of_item_93("vault");
        let d = copy_distinctness(&c.conn, dest, &[set]).unwrap();
        assert_eq!(d.shared_location, vec![span_copy("SPAN-2", "vault", None)]);
        assert_eq!(
            d.warning("SPAN-3").unwrap(),
            "warning: volume \"SPAN-3\" is at location \"vault\", where these versions \
             already have a copy: span v1 (volume \"SPAN-2\"). Copies in one place are lost \
             together. Writing anyway: location policy is advisory (ADR-0004), and `tapectl \
             audit` reports any required location still without a copy."
        );
    }

    /// A set with no range row keeps the whole-copy rule: a holder at the
    /// destination's place is named, whatever the slices' copies.
    #[test]
    fn a_set_with_no_range_is_warned_as_before() {
        let c = SpanCatalog::new();
        let snap = c.version(1, "current");
        let set = c.stage_set(snap, 6);
        let t2 = c.volume("SPAN-2", Some("home"));
        c.whole(set, 6, t2);
        let dest = c.volume("SPAN-3", Some("home"));
        let d = copy_distinctness(&c.conn, dest, &[set]).unwrap();
        assert_eq!(d.shared_location, vec![span_copy("SPAN-2", "home", None)]);
        assert!(d
            .warning("SPAN-3")
            .unwrap()
            .contains("span v1 (volume \"SPAN-2\")"));
    }

    /// Every slice with the same copies (one each, on two ranges): every
    /// slice is next, so a range holder at the destination's place is warned
    /// as a whole copy is, named with its slice run. A `cut` write left
    /// nothing on its tape and is no holder.
    #[test]
    fn a_set_with_equal_copies_on_every_slice_warns_naming_the_range() {
        let c = SpanCatalog::new();
        let snap = c.version(1, "current");
        let set = c.stage_set(snap, 6);
        let t1 = c.volume("SPAN-1", Some("home"));
        let t3 = c.volume("SPAN-3", Some("vault"));
        let w1 = c.range(set, t1, 1, 2, 6);
        c.range(set, t3, 3, 6, 6);
        let dest = c.volume("SPAN-4", Some("home"));
        let d = copy_distinctness(&c.conn, dest, &[set]).unwrap();
        assert_eq!(
            d.shared_location,
            vec![span_copy("SPAN-1", "home", Some((1, 2)))]
        );
        assert!(
            d.warning("SPAN-4")
                .unwrap()
                .contains("already have a copy: span v1 (slices 1-2 on volume \"SPAN-1\")."),
            "{:?}",
            d.warning("SPAN-4")
        );

        c.conn
            .execute(
                "UPDATE writes SET status = 'cut' WHERE id = ?1",
                params![w1],
            )
            .unwrap();
        let d = copy_distinctness(&c.conn, dest, &[set]).unwrap();
        assert!(d.shared_location.is_empty(), "{d:?}");
    }

    /// Copies are counted per version (ADR-0012): another stage set of the
    /// same snapshot is the same content, and a write of it to the copy's
    /// cartridge is refused just the same (the same impossible-state guard
    /// as `a_destination_on_the_copys_own_cartridge_is_refused`).
    #[test]
    fn a_second_stage_set_of_the_same_version_is_the_same_content() {
        let (conn, dest, _ss) = distinctness_fixture(None, "BC1", "BC1");
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size)
             SELECT snapshot_id, 'staged', 104857600 FROM stage_sets",
            [],
        )
        .unwrap();
        let second = conn.last_insert_rowid();
        let d = copy_distinctness(&conn, dest, &[second]).unwrap();
        assert_eq!(d.same_cartridge, vec![copy("L6-0003", "BC1", false)]);
    }

    /// The path an operator can actually take to put a second copy on the
    /// first copy's cartridge: `volume init` of the destination on it. Every
    /// binding goes through `binding::mount_and_record` (`volume init`,
    /// `bind_late` at first write contact, `catalog rebuild`), and it marks
    /// every other volume mounted on that cartridge `erased` in the same
    /// transaction (ADR-0010 displacement, ADR-0012 item 26 notwithstanding).
    /// So by the time the destination's mount is open the copy is already
    /// gone: nothing is left for [`copy_distinctness`] to refuse, the
    /// version's copy count is 0 — the fixture's glacier deposit drops out
    /// too, because its source volume is no longer eligible — and the
    /// displacement names the unit it left uncovered. That warning, at
    /// `volume init`, is the operator's signal; the plan-time refusal only
    /// guards a catalog in which two live volumes share one cartridge.
    #[test]
    fn binding_a_destination_onto_the_copys_cartridge_erases_the_copy_first() {
        let (conn, unit_id, copy_vol) = setup_unit_with_deposit("active");
        let q = CoverageQuery::current_unit("?1");
        assert_eq!(scalar(&conn, &copy_count_expr(&q), unit_id), 2);
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status)
             VALUES ('BC1', 'LTO-6', 2500000000000, 'in_use')",
            [],
        )
        .unwrap();
        let cart = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO cartridge_volumes (cartridge_id, volume_id, identity_source)
             VALUES (?1, ?2, 'mam')",
            params![cart, copy_vol],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES ('L6-0009', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
            [],
        )
        .unwrap();
        let dest = conn.last_insert_rowid();

        let outcome = crate::volume::binding::mount_and_record(
            &conn,
            dest,
            cart,
            "BC1",
            "in_use",
            Some("SER-1"),
            &crate::tape::mam::MamInfo::default(),
            "volume init",
            true,
        )
        .unwrap();

        let status: String = conn
            .query_row(
                "SELECT status FROM volumes WHERE id = ?1",
                params![copy_vol],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "erased");
        assert_eq!(outcome.displaced.len(), 1);
        assert_eq!(outcome.displaced[0].label, "L6-0003");
        assert!(
            outcome.displaced[0]
                .impacts
                .iter()
                .any(|i| i.unit_name == "photos"),
            "the displacement must name the unit it leaves uncovered"
        );

        let ss: i64 = conn
            .query_row("SELECT id FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        let d = copy_distinctness(&conn, dest, &[ss]).unwrap();
        assert!(d.same_cartridge.is_empty(), "{d:?}");
        assert_eq!(d.refusal("L6-0009"), None);
        assert_eq!(scalar(&conn, &copy_count_expr(&q), unit_id), 0);
    }

    // ── by-name location targets (issue #348; ADR-0012 2026-10-07 item 34; #493) ──

    /// A default resolved policy whose `copies_at` map is `map`.
    fn targets(map: &[(&str, i64)]) -> crate::policy::ResolvedPolicy {
        let conn = crate::db::open_memory().unwrap();
        let unit = crate::db::models::Unit {
            id: 0,
            uuid: "u-targets".into(),
            name: "targets".into(),
            tenant_id: 0,
            archive_set_id: None,
            current_path: None,
            checksum_mode: "mtime_size".into(),
            encrypt: true,
            status: "active".into(),
            created_at: String::new(),
            last_scanned: None,
            notes: None,
            copies_at_frozen: None,
        };
        let mut p =
            crate::policy::resolve(&conn, &crate::config::Config::default(), &unit).unwrap();
        p.copies_at = map.iter().map(|(n, c)| (n.to_string(), *c)).collect();
        p
    }

    /// The by-name question as an archive set's `required_locations` asks it
    /// since #493: the names folded into the `copies_at` map at one copy each
    /// (item 156), each target raised to `per_name` (a tape-only unit at that
    /// multiplier), asked of `scope` by [`short_of_copies_at`] with `exclude`
    /// pretended away: `(name, copies)` of each short name, in name order.
    fn short_named(
        conn: &Connection,
        scope: NamedScope,
        required: &[String],
        per_name: i64,
        exclude: Option<i64>,
    ) -> Vec<(String, i64)> {
        let mut config = crate::config::Config::default();
        config.compaction.tape_only_safety_multiplier = per_name.try_into().unwrap();
        let map: Vec<(&str, i64)> = required.iter().map(|n| (n.as_str(), 1)).collect();
        short_of_copies_at(conn, &config, scope, &targets(&map), "tape_only", exclude)
            .unwrap()
            .into_iter()
            .map(|s| (s.location, s.copies))
            .collect()
    }

    /// The names with no copy, at one copy each.
    fn missing_required_locations(
        conn: &Connection,
        unit_id: i64,
        required: &[String],
    ) -> crate::error::Result<Vec<String>> {
        missing_required_locations_in(conn, NamedScope::Unit(unit_id), required)
    }

    fn missing_required_locations_for_snapshot(
        conn: &Connection,
        snapshot_id: i64,
        required: &[String],
    ) -> crate::error::Result<Vec<String>> {
        missing_required_locations_in(conn, NamedScope::Snapshot(snapshot_id), required)
    }

    fn missing_required_locations_in(
        conn: &Connection,
        scope: NamedScope,
        required: &[String],
    ) -> crate::error::Result<Vec<String>> {
        let map: Vec<(&str, i64)> = required.iter().map(|n| (n.as_str(), 1)).collect();
        let short = short_of_copies_at(
            conn,
            &crate::config::Config::default(),
            scope,
            &targets(&map),
            "active",
            None,
        )?;
        Ok(locations_without_a_copy(&short))
    }

    /// Item 34: each name is asked for `per_name` copies, and the count is
    /// copies there — tape and warehouse alike, so the deposit at `glacier`
    /// is one copy there. At 1 both names are met; at 2 each holds one. A
    /// name given twice is asked for once (the map has one key).
    #[test]
    fn each_named_location_is_asked_for_per_name_copies() {
        let (conn, unit_id, _vol) = setup_unit_with_deposit("tape_only");
        let req = names(&["home", "glacier", "home"]);
        let unit = NamedScope::Unit(unit_id);
        assert!(short_named(&conn, unit, &req, 1, None).is_empty());
        let short = short_named(&conn, unit, &req, 2, None);
        assert_eq!(
            short,
            vec![("glacier".to_string(), 1), ("home".to_string(), 1)]
        );
        let mut config = crate::config::Config::default();
        config.compaction.tape_only_safety_multiplier = 2;
        let short = short_of_copies_at(
            &conn,
            &config,
            unit,
            &targets(&[("home", 1), ("glacier", 1)]),
            "tape_only",
            None,
        )
        .unwrap();
        assert!(locations_without_a_copy(&short).is_empty());
        assert_eq!(
            describe_short_locations(&short),
            "1 copy at required location glacier, 1 copy at required location home, \
             needs 2 at each"
        );
        assert_eq!(
            describe_short_locations(&short[..1]),
            "1 copy at required location glacier, needs 2"
        );
    }

    /// Issue #463: the retire family asks the by-name and place-floor
    /// predicates "after this volume is given up". The deposit recorded FROM
    /// the excluded volume goes with it (`CoverageQuery::exclude_volume`), so
    /// excluding the tape at `home` empties both names and both places.
    #[test]
    fn excluding_a_volume_empties_the_names_and_places_it_and_its_deposit_held() {
        let (conn, unit_id, vol) = setup_unit_with_deposit("tape_only");
        let snapshot: i64 = conn
            .query_row(
                "SELECT id FROM snapshots WHERE unit_id = ?1",
                params![unit_id],
                |r| r.get(0),
            )
            .unwrap();
        let req = names(&["home", "glacier"]);
        let config = crate::config::Config::default();
        for scope in [NamedScope::Snapshot(snapshot), NamedScope::Unit(unit_id)] {
            assert_eq!(places_held_excluding(&conn, scope, None).unwrap(), 2);
            assert_eq!(places_held(&conn, scope).unwrap(), 2);
            assert_eq!(places_held_excluding(&conn, scope, Some(vol)).unwrap(), 0);
            assert!(short_named(&conn, scope, &req, 1, None).is_empty());
            let short = short_named(&conn, scope, &req, 1, Some(vol));
            assert_eq!(
                short,
                vec![("glacier".to_string(), 0), ("home".to_string(), 0)]
            );
            assert_eq!(
                short_of_location_floor_excluding(&conn, &config, scope, "tape_only", None)
                    .unwrap(),
                None
            );
            assert_eq!(
                short_of_location_floor_excluding(&conn, &config, scope, "tape_only", Some(vol))
                    .unwrap(),
                Some(PlaceShortfall {
                    places: 0,
                    floor: 2
                })
            );
            // An active unit has no place floor, whatever it keeps.
            assert_eq!(
                short_of_location_floor_excluding(&conn, &config, scope, "active", Some(vol))
                    .unwrap(),
                None
            );
        }
    }

    /// The per-version rule for counts: a unit holds at a name the FEWEST
    /// copies any current version has there. v1 has two tapes at
    /// `home-rack`, v2 one, so the unit holds one there.
    #[test]
    fn a_unit_holds_at_a_name_the_fewest_copies_of_any_current_version() {
        let (conn, unit_id) =
            setup_unit_with_two_current_snapshots("mv-count", Some("home-rack"), Some("garage"));
        conn.execute(
            "UPDATE volumes SET location_id = (SELECT id FROM locations WHERE name = 'home-rack')
             WHERE label = 'mv-count-B'",
            [],
        )
        .unwrap();
        let unit = NamedScope::Unit(unit_id);
        let req = names(&["home-rack"]);
        assert!(short_named(&conn, unit, &req, 1, None).is_empty());
        conn.execute_batch(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status, location_id)
                 SELECT 'MV-EXTRA', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed', id
                 FROM locations WHERE name = 'home-rack';",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             SELECT ss.id, s.id, (SELECT id FROM volumes WHERE label = 'MV-EXTRA'), 'completed'
             FROM snapshots s JOIN stage_sets ss ON ss.snapshot_id = s.id
             WHERE s.unit_id = ?1 AND s.version = 1",
            params![unit_id],
        )
        .unwrap();
        assert_eq!(
            short_named(&conn, unit, &req, 2, None),
            vec![("home-rack".to_string(), 1)],
            "v2 holds one copy at home-rack, so the unit holds one"
        );
        let v1: i64 = conn
            .query_row(
                "SELECT id FROM snapshots WHERE unit_id = ?1 AND version = 1",
                params![unit_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            short_named(&conn, NamedScope::Snapshot(v1), &req, 2, None).is_empty(),
            "v1 alone holds two"
        );
    }

    #[test]
    fn the_named_location_multiplier_applies_to_a_tape_only_unit_only() {
        let mut config = crate::config::Config::default();
        config.compaction.tape_only_safety_multiplier = 3;
        let one_each = targets(&[("home", 1)]);
        assert_eq!(location_target(&config, &one_each, "home", "tape_only"), 3);
        for status in ["active", "missing"] {
            assert_eq!(
                location_target(&config, &one_each, "home", status),
                1,
                "{status}"
            );
        }
    }

    /// ADR-0012 2026-10-12 items 43 and 46: the one rule every COPY floor asks.
    /// A tape-only unit's floor is the base times the CONFIGURED multiplier
    /// (3 here, so it cannot be mistaken for a literal 2), and the note
    /// names it; every other status is the base itself, with no note — the
    /// positive control that the rule touches tape-only units only.
    #[test]
    fn every_copy_floor_takes_the_tape_only_multiplier_for_a_tape_only_unit_only() {
        let mut config = crate::config::Config::default();
        config.compaction.tape_only_safety_multiplier = 3;
        assert_eq!(tape_only_multiplier(&config, "tape_only"), 3);
        assert_eq!(copy_floor(&config, 2, "tape_only"), 6);
        assert_eq!(
            copy_floor(&config, 0, "tape_only"),
            0,
            "no floor stays none"
        );
        assert_eq!(tape_only_note(&config, "tape_only"), " (tape-only 3x)");
        for status in ["active", "missing"] {
            assert_eq!(tape_only_multiplier(&config, status), 1, "{status}");
            assert_eq!(copy_floor(&config, 2, status), 2, "{status}");
            assert_eq!(tape_only_note(&config, status), "", "{status}");
        }
        // At 1x the floor is the policy's own number, so nothing to name.
        config.compaction.tape_only_safety_multiplier = 1;
        assert_eq!(copy_floor(&config, 2, "tape_only"), 2);
        assert_eq!(tape_only_note(&config, "tape_only"), "");
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Tape at `home`, warehouse deposit at `glacier`: both names are met —
    /// the deposit side counts exactly as it does for the location count —
    /// and a name with no copy (`offsite`) is reported by name.
    #[test]
    fn required_locations_are_checked_by_name_including_deposits() {
        let (conn, unit_id, _vol) = setup_unit_with_deposit("active");
        assert!(
            missing_required_locations(&conn, unit_id, &names(&["home", "glacier"]))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            missing_required_locations(&conn, unit_id, &names(&["home", "offsite", "offsite"]))
                .unwrap(),
            names(&["offsite"]),
            "two locations hold copies, but not the two the policy names"
        );
    }

    /// ADR-0004: an ineligible volume is not a copy, so it places the unit
    /// nowhere — and its deposit goes with it.
    #[test]
    fn a_retired_volume_places_the_unit_at_no_location() {
        let (conn, unit_id, vol) = setup_unit_with_deposit("active");
        conn.execute(
            "UPDATE volumes SET status = 'retired' WHERE id = ?1",
            params![vol],
        )
        .unwrap();
        assert_eq!(
            missing_required_locations(&conn, unit_id, &names(&["home", "glacier"])).unwrap(),
            names(&["glacier", "home"])
        );
    }

    /// Issue #153's per-version rule: v1 at `home-rack`, v2 at `garage`. A
    /// unit is as covered as its least-covered live version, so neither
    /// name is met by the unit as a whole.
    #[test]
    fn a_required_location_must_hold_every_current_version() {
        let (conn, unit_id) =
            setup_unit_with_two_current_snapshots("mv-named", Some("home-rack"), Some("garage"));
        assert_eq!(
            missing_required_locations(&conn, unit_id, &names(&["home-rack", "garage"])).unwrap(),
            names(&["garage", "home-rack"])
        );
    }

    /// The one-version form answers for that version alone: v2 (at
    /// `garage`) is missing `home-rack` though v1 is there, and v1 is
    /// missing `garage`. The unit form above misses both; this is what
    /// `snapshot mark-reclaimable` asks of the version it keeps.
    #[test]
    fn the_one_version_form_judges_that_version_alone() {
        let (conn, unit_id) =
            setup_unit_with_two_current_snapshots("mv-one", Some("home-rack"), Some("garage"));
        let snap = |v: i64| -> i64 {
            conn.query_row(
                "SELECT id FROM snapshots WHERE unit_id = ?1 AND version = ?2",
                params![unit_id, v],
                |r| r.get(0),
            )
            .unwrap()
        };
        let wanted = names(&["home-rack", "garage", "garage"]);
        assert_eq!(
            missing_required_locations_for_snapshot(&conn, snap(2), &wanted).unwrap(),
            names(&["home-rack"])
        );
        assert_eq!(
            missing_required_locations_for_snapshot(&conn, snap(1), &wanted).unwrap(),
            names(&["garage"])
        );
        assert!(missing_required_locations_for_snapshot(&conn, snap(1), &[])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn no_current_snapshot_means_every_required_location_is_missing() {
        let (conn, unit_id, _vol) = setup_unit_with_deposit("active");
        conn.execute("UPDATE snapshots SET status = 'created'", [])
            .unwrap();
        assert_eq!(
            missing_required_locations(&conn, unit_id, &names(&["home"])).unwrap(),
            names(&["home"])
        );
        assert!(missing_required_locations(&conn, unit_id, &[])
            .unwrap()
            .is_empty());
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
        // Issue #242: quarantine is now a condition, not a status move — the
        // volume stays `sealed` (as `setup_unit_with_deposit` left it) and
        // only `observed_condition` records the fact.
        conn.execute(
            "UPDATE volumes SET observed_condition = 'quarantined' WHERE id = ?1",
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
        assert_eq!(
            eligible("v"),
            "(v.status = 'sealed' AND v.observed_condition = 'ok')"
        );
        assert_eq!(
            eligible("v2"),
            "(v2.status = 'sealed' AND v2.observed_condition = 'ok')"
        );
    }

    /// THE negative control for issue #242 (ADR-0012's 2026-09-17 amendment
    /// "the status column is the operator's; a medium's condition is its
    /// own fact"): a `sealed` volume whose `observed_condition` is
    /// `quarantined` must contribute ZERO copies. Written and run BEFORE
    /// `eligible` gained the condition half — this must fail red first (see
    /// the worker's report for the captured failure), because `eligible`
    /// used to be `status = 'sealed'` alone and a quarantine no longer moves
    /// `status` off `sealed` at all now that the writers target the new
    /// column. Miss this and every quarantine silently stops reducing the
    /// copy count — exactly the coverage-misstatement class issue #153 was.
    #[test]
    fn a_sealed_volume_with_a_quarantined_condition_contributes_zero_copies() {
        let (conn, unit_id, vol) = setup_unit_with_deposit("active");
        // The deposit is gated on its source volume's eligibility too (see
        // `a_deposit_of_an_ineligible_volume_does_not_count` above), so
        // dropping it here isolates exactly the tape-eligibility half this
        // test is about.
        conn.execute("DELETE FROM volume_deposits", []).unwrap();
        conn.execute(
            "UPDATE volumes SET observed_condition = 'quarantined' WHERE id = ?1",
            params![vol],
        )
        .unwrap();
        let q = CoverageQuery::current_unit("?1");
        assert_eq!(
            scalar(&conn, &copy_count_expr(&q), unit_id),
            0,
            "a sealed-but-quarantined volume must not count as a copy"
        );
    }

    #[test]
    fn in_service_keeps_imported_active_and_adds_sealed() {
        assert_eq!(
            in_service("v"),
            "(v.status IN ('active','sealed') AND v.observed_condition = 'ok')",
            "dropping imported 'active' would under-report physical media"
        );
    }

    #[test]
    fn in_service_takes_a_bare_table_name_for_unaliased_queries() {
        assert_eq!(
            in_service("volumes"),
            "(volumes.status IN ('active','sealed') AND volumes.observed_condition = 'ok')"
        );
    }

    #[test]
    fn in_service_or_provisioned_adds_initialized_and_nothing_else() {
        assert_eq!(
            in_service_or_provisioned("volumes"),
            "(volumes.status IN ('active','sealed','initialized') AND \
             volumes.observed_condition = 'ok')"
        );
    }

    /// Issue #242: `in_service` must exclude a quarantined-but-otherwise-live
    /// volume, not just one whose status names it directly. Before the
    /// 2026-09-17 amendment this rode on `status` leaving `sealed`; now
    /// quarantine leaves `status` alone.
    #[test]
    fn in_service_excludes_a_sealed_volume_with_a_quarantined_condition() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status, observed_condition)
             VALUES ('L6-INSVC', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed', 'quarantined')",
            [],
        )
        .unwrap();
        let count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM volumes v WHERE {}", in_service("v")),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 0,
            "a quarantined medium is untrusted; it must not count as live inventory"
        );
    }

    /// The two predicates must never converge: `eligible` is a durability
    /// claim, `in_service` an inventory one (issue #96).
    #[test]
    fn eligible_and_in_service_stay_distinct() {
        assert_ne!(eligible("v"), in_service("v"));
        assert!(!in_service("v").contains("= 'sealed'"));
    }

    // ── Issue #153: a unit's coverage is its LEAST-covered current
    // version, not the union of volumes across versions ──
    //
    // `session.rs` promotes each sealed snapshot to `'current'` and never
    // demotes its predecessor (`'superseded'` never had a writer and
    // left the schema in migration 026), so a unit can have
    // MULTIPLE `'current'` snapshots at once. The pre-fix expressions
    // UNIONed eligible volumes across every current snapshot, which
    // counts "how many volumes hold ANY version of this unit" rather
    // than "how many copies does the unit's thinnest version have."
    // ADR-0012: "a unit is as covered as its least-covered live version."

    /// Regression fixture: TWO `'current'` snapshots of one unit, each
    /// with its own completed write to its own SEALED volume — the shape
    /// the write path actually produces. `vol_a_location`/`vol_b_location`
    /// optionally shelve each volume at a named location, for the
    /// location-count variant of the same regression. Returns
    /// `(conn, unit_id)`.
    fn setup_unit_with_two_current_snapshots(
        unit_name: &str,
        vol_a_location: Option<&str>,
        vol_b_location: Option<&str>,
    ) -> (Connection, i64) {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES (?1, ?2, ?3, 'mtime_size', 1, 'active')",
            params![format!("uuid-{unit_name}"), unit_name, tid],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();

        fn insert_location(conn: &Connection, name: Option<&str>) -> Option<i64> {
            name.map(|n| {
                conn.execute(
                    "INSERT INTO locations (name, kind) VALUES (?1, 'shelf')",
                    params![n],
                )
                .unwrap();
                conn.last_insert_rowid()
            })
        }
        let a_loc = insert_location(&conn, vol_a_location);
        let b_loc = insert_location(&conn, vol_b_location);

        conn.execute(
            "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
             VALUES (?1, 1, 'full', 'current', '/src')",
            params![unit_id],
        )
        .unwrap();
        let snap1_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
            params![snap1_id],
        )
        .unwrap();
        let ss1_id = conn.last_insert_rowid();
        conn.execute(
            &format!(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                      capacity_bytes, status, location_id)
                 VALUES ('{unit_name}-A', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed', ?1)"
            ),
            params![a_loc],
        )
        .unwrap();
        let vol_a = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'completed')",
            params![ss1_id, snap1_id, vol_a],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
             VALUES (?1, 2, 'full', 'current', '/src')",
            params![unit_id],
        )
        .unwrap();
        let snap2_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
            params![snap2_id],
        )
        .unwrap();
        let ss2_id = conn.last_insert_rowid();
        conn.execute(
            &format!(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                      capacity_bytes, status, location_id)
                 VALUES ('{unit_name}-B', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed', ?1)"
            ),
            params![b_loc],
        )
        .unwrap();
        let vol_b = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'completed')",
            params![ss2_id, snap2_id, vol_b],
        )
        .unwrap();

        (conn, unit_id)
    }

    /// Regression fixture: ONE `'current'` snapshot written to TWO
    /// distinct sealed volumes — the common case the per-version MIN fix
    /// must NOT change (no deposit, no second version). Returns
    /// `(conn, unit_id)`.
    fn setup_unit_with_one_snapshot_two_volumes(unit_name: &str) -> (Connection, i64) {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES (?1, ?2, ?3, 'mtime_size', 1, 'active')",
            params![format!("uuid-{unit_name}"), unit_name, tid],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
             VALUES (?1, 1, 'full', 'current', '/src')",
            params![unit_id],
        )
        .unwrap();
        let snap_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
            params![snap_id],
        )
        .unwrap();
        let ss_id = conn.last_insert_rowid();

        for label in [format!("{unit_name}-A"), format!("{unit_name}-B")] {
            conn.execute(
                &format!(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                          capacity_bytes, status)
                     VALUES ('{label}', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')"
                ),
                [],
            )
            .unwrap();
            let vol_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, 'completed')",
                params![ss_id, snap_id, vol_id],
            )
            .unwrap();
        }

        (conn, unit_id)
    }

    #[test]
    fn current_unit_copy_count_is_min_across_current_snapshots_not_union() {
        let (conn, unit_id) = setup_unit_with_two_current_snapshots("mv-photos", None, None);
        let q = CoverageQuery::current_unit("?1");
        let sql = copy_count_expr(&q);
        eprintln!("copy_count_expr(current_unit) SQL:\n{sql}");
        assert_eq!(
            scalar(&conn, &sql, unit_id),
            1,
            "each version has exactly 1 copy on its own volume; the unit's \
             coverage is its least-covered version (ADR-0012), not the \
             union of volumes across versions"
        );
    }

    #[test]
    fn current_unit_location_count_is_min_across_current_snapshots_not_union() {
        let (conn, unit_id) =
            setup_unit_with_two_current_snapshots("mv-photos-loc", Some("home"), Some("bank"));
        let q = CoverageQuery::current_unit("?1");
        assert_eq!(
            scalar(&conn, &location_count_expr(&q), unit_id),
            1,
            "each version is at exactly 1 location (v1 at home, v2 at \
             bank); the unit's location coverage is its least-covered \
             version, not the union across versions"
        );
    }

    /// Regression pin: a unit with ONE current snapshot on two volumes
    /// must still read 2 copies -- the per-version MIN fix must not
    /// change the common, non-multi-version case.
    #[test]
    fn one_current_snapshot_on_two_volumes_still_counts_two_copies() {
        let (conn, unit_id) = setup_unit_with_one_snapshot_two_volumes("mv-single-version");
        let q = CoverageQuery::current_unit("?1");
        assert_eq!(scalar(&conn, &copy_count_expr(&q), unit_id), 2);
    }

    /// Regression pin: a unit with ZERO current snapshots must still
    /// read 0, not NULL -- `MIN` over an empty set of per-version counts
    /// is NULL, so `COALESCE(..., 0)` is load-bearing here.
    #[test]
    fn zero_current_snapshots_counts_zero_not_null() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('uuid-mv-none', 'mv-none', ?1, 'mtime_size', 1, 'active')",
            params![tid],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        let q = CoverageQuery::current_unit("?1");
        assert_eq!(scalar(&conn, &copy_count_expr(&q), unit_id), 0);
        assert_eq!(scalar(&conn, &location_count_expr(&q), unit_id), 0);
    }

    /// Regression pin: `CoverageScope::Snapshot` names one exact snapshot
    /// already, so it must not be touched by the per-version MIN fix,
    /// which is scoped to `CoverageScope::Unit { current_only: true }`
    /// only.
    #[test]
    fn snapshot_scope_is_unaffected_by_the_per_version_min_fix() {
        let (conn, _unit_id, _vol) = setup_unit_with_deposit("active");
        let snap_id: i64 = conn
            .query_row("SELECT id FROM snapshots LIMIT 1", [], |r| r.get(0))
            .unwrap();
        let q = CoverageQuery {
            scope: CoverageScope::Snapshot { id_expr: "?1" },
            exclude_volume: None,
        };
        let sql = copy_count_expr(&q);
        assert!(
            !sql.contains("MIN("),
            "Snapshot scope must not gain the per-version MIN wrapper: {sql}"
        );
        assert_eq!(
            scalar(&conn, &sql, snap_id),
            2,
            "unchanged: one sealed tape plus its warehouse deposit"
        );
    }

    /// Regression pin: `CoverageScope::Unit { current_only: false }` is
    /// `volume retire`'s impact analysis, which deliberately asks about
    /// ANY snapshot -- it must keep counting the union across every
    /// snapshot, current or not, unaffected by the per-version MIN fix.
    #[test]
    fn unit_scope_with_current_only_false_is_unaffected_by_the_per_version_min_fix() {
        let (conn, unit_id) = setup_unit_with_two_current_snapshots("mv-any-snapshot", None, None);
        let q = CoverageQuery {
            scope: CoverageScope::Unit {
                id_expr: "?1",
                current_only: false,
            },
            exclude_volume: None,
        };
        let sql = copy_count_expr(&q);
        assert!(
            !sql.contains("MIN("),
            "current_only: false must not gain the per-version MIN wrapper: {sql}"
        );
        assert_eq!(
            scalar(&conn, &sql, unit_id),
            2,
            "volume retire deliberately asks about ANY snapshot, not just \
             the unit's least-covered current one"
        );
    }

    /// ADR-0012 (issue #161): `is_write_target` must admit exactly
    /// `initialized` (with `observed_condition = 'ok'`), and this must be
    /// re-checked against the schema's own status set every time it
    /// changes — a status added later to the CHECK constraint without a
    /// corresponding classification here would otherwise silently fall
    /// through as "not a write target" (or worse, silently become one) with
    /// nothing failing to say so.
    ///
    /// Repointed to migration 017 (issue #242): `quarantined` left the
    /// `status` CHECK entirely when it became a value of
    /// `observed_condition` instead, so the schema-set pin below must read
    /// the CURRENT authority, not 003 — reading the superseded file would
    /// keep passing with 'quarantined' still in `statuses`, silently
    /// pinning a status set the live schema no longer has.
    ///
    /// Repointed again, to the LIVE schema (issue #362): reading 017's
    /// text is the same trap one migration later -- 026 dropped `blank`
    /// and `missing` and this pin kept passing against 017. Reading the
    /// migrated schema itself means the next migration cannot strand it
    /// (032 dropped `full`, and this pin named it as the first to notice).
    #[test]
    fn is_write_target_admits_exactly_initialized() {
        let statuses = ["initialized", "active", "retired", "erased", "sealed"];

        // The live `CHECK(status IN (...))` set, pinned against `statuses`
        // by SET EQUALITY, not mere containment -- a one-directional "does
        // each of mine appear somewhere" check would keep passing after a
        // new status was added to the CHECK and never classified here,
        // which is exactly the drift this pin exists to catch.
        let schema_statuses = crate::db::live_status_check("volumes");
        let mut pinned_statuses: Vec<String> = statuses.iter().map(|s| s.to_string()).collect();
        pinned_statuses.sort_unstable();
        assert_eq!(
            schema_statuses, pinned_statuses,
            "the live `volumes.status` CHECK no longer matches the set \
             `is_write_target_admits_exactly_initialized` classifies -- a status was \
             added or removed without updating is_write_target (and this test) to \
             account for it"
        );

        for status in statuses {
            let expected = status == "initialized";
            assert_eq!(
                is_write_target(status, "ok"),
                expected,
                "is_write_target({status:?}, \"ok\") should be {expected}"
            );
        }
    }

    /// THE negative control for issue #242's `is_write_target` regression
    /// (ADR-0012's 2026-09-17 amendment, point 5, "is_write_target is the
    /// trap"): an `initialized` volume whose `observed_condition` is
    /// `quarantined` must NOT be a write target. Run BEFORE the condition
    /// parameter was wired into the body (it was, at first, accepted and
    /// ignored) -- see the worker's report for the captured red failure.
    /// Without this, the write-path quarantine writers moving off `status`
    /// (rather than off `observed_condition`) makes a mid-write-quarantined
    /// volume a legal write target again the instant nothing else changes
    /// its status -- a live regression this change would otherwise
    /// introduce.
    #[test]
    fn is_write_target_refuses_an_initialized_volume_with_a_quarantined_condition() {
        assert!(
            !is_write_target("initialized", "quarantined"),
            "a quarantined medium must never be a write target, even while status reads \
             'initialized'"
        );
        assert!(
            is_write_target("initialized", "ok"),
            "precondition: an ordinary initialized/ok volume must still be a write target"
        );
    }

    /// ADR-0012 amendment, issue #199: `has_completed_write` must
    /// disqualify a volume with a `completed` `writes` row, and must NOT
    /// disqualify one whose only row is `planned`/`in_progress`/
    /// `interrupted`/`aborted`/`failed` -- exactly the resumable-or-
    /// terminal-but-not-a-copy states `volume resume` and the retry paths
    /// still need to work on. Checked one status at a time, per the same
    /// "test per state" discipline the ruling asked for on the `write.rs`
    /// resume tests, rather than folding them into one aggregate assert.
    #[test]
    fn has_completed_write_disqualifies_only_completed_rows() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('u-hcw', 'hcw', ?1, 'mtime_size', 1, 'active')",
            params![tid],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
             VALUES (?1, 1, 'full', 'staged', '/tmp/hcw')",
            params![unit_id],
        )
        .unwrap();
        let snap_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
            params![snap_id],
        )
        .unwrap();
        let stage_set_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES ('L6-HCW', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
            [],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();

        assert!(
            !has_completed_write(&conn, volume_id).unwrap(),
            "a freshly initialized volume with no writes rows must not be flagged"
        );

        for status in ["planned", "in_progress", "interrupted", "aborted", "failed"] {
            conn.execute(
                "DELETE FROM writes WHERE volume_id = ?1",
                params![volume_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, ?4)",
                params![stage_set_id, snap_id, volume_id, status],
            )
            .unwrap();
            assert!(
                !has_completed_write(&conn, volume_id).unwrap(),
                "status {status:?} must not disqualify -- only `completed` does"
            );
        }

        conn.execute(
            "DELETE FROM writes WHERE volume_id = ?1",
            params![volume_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'completed')",
            params![stage_set_id, snap_id, volume_id],
        )
        .unwrap();
        assert!(
            has_completed_write(&conn, volume_id).unwrap(),
            "a `completed` writes row must disqualify"
        );

        // #420: a `cut` row is on a sealed tape that ended early; the volume
        // holds bytes tapectl knows about, so it disqualifies too.
        conn.execute(
            "UPDATE writes SET status = 'cut' WHERE volume_id = ?1",
            params![volume_id],
        )
        .unwrap();
        assert!(
            has_completed_write(&conn, volume_id).unwrap(),
            "a `cut` writes row must disqualify"
        );

        // #478 (ADR-0012 item 121): a `range` row is the run of a spannable
        // set's slices this tape holds. A volume holding only one holds bytes
        // tapectl knows about and is no write target.
        conn.execute(
            "UPDATE writes SET status = 'range', may_span = 1 WHERE volume_id = ?1",
            params![volume_id],
        )
        .unwrap();
        assert!(
            has_completed_write(&conn, volume_id).unwrap(),
            "a `range` writes row must disqualify"
        );
    }

    /// #420 (ADR-0012 item 112), on the shared short-tape fixture: a `cut`
    /// row is never a copy. The unit whole on the short tape has its copy;
    /// the two the early end cut have none, though one of them has a slice on
    /// the tape; a volume holding only cut writes is still no write target.
    #[test]
    fn a_cut_row_is_never_a_copy() {
        use crate::volume::restore::short_tape;
        let t = short_tape::short_tape();
        let copies = |unit: &str| -> i64 {
            let sql = format!(
                "SELECT {}",
                copy_count_expr(&CoverageQuery::current_unit("?1"))
            );
            let id: i64 = t
                .conn
                .query_row("SELECT id FROM units WHERE name = ?1", [unit], |r| r.get(0))
                .unwrap();
            t.conn.query_row(&sql, params![id], |r| r.get(0)).unwrap()
        };
        assert_eq!(copies("alpha"), 1, "the positive control");
        assert_eq!(copies("bravo"), 0);
        assert_eq!(copies("charlie"), 0);
        let bravo: i64 = t
            .conn
            .query_row("SELECT id FROM units WHERE name = 'bravo'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(in_service_copy_of_version(&t.conn, bravo, 1).unwrap(), None);
        let sole = sole_slices(&t.conn, t.volume_id).unwrap();
        assert_eq!(sole.len(), 2, "alpha's two slices: {sole:?}");
        assert!(sole.iter().all(|s| s.unit_name == "alpha"), "{sole:?}");

        let cut_only = short_tape::cut_only_tape();
        assert!(has_completed_write(&cut_only.conn, cut_only.volume_id).unwrap());
    }

    // ── versions_at_stake (ADR-0008 Tier 3 / ADR-0012, issue #147) ──
    //
    // The derivation both consent tiers of the retire family read. Every
    // test here asks the same question in a different shape: what does
    // retiring THIS volume actually REMOVE?

    /// A unit `photos` with one CURRENT v1 written to the sealed volume
    /// `HERE`. `elsewhere` adds a second sealed volume `AWAY` carrying the
    /// same snapshot, so v1 survives HERE's retirement.
    /// Returns `(conn, unit_id, here_id)`.
    fn setup_at_stake(elsewhere: bool) -> (Connection, i64, i64) {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('u-stake', 'photos', ?1, 'mtime_size', 1, 'active')",
            params![tid],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO locations (name, kind) VALUES ('home', 'shelf')",
            [],
        )
        .unwrap();
        let home = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status, location_id)
             VALUES ('HERE', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed', ?1)",
            params![home],
        )
        .unwrap();
        let here = conn.last_insert_rowid();
        add_version(&conn, unit_id, here, 1, "current", elsewhere);
        (conn, unit_id, here)
    }

    /// Add one version of `unit_id` written to `vol_id`, plus (when
    /// `elsewhere`) a second sealed volume carrying the same snapshot.
    fn add_version(
        conn: &Connection,
        unit_id: i64,
        vol_id: i64,
        version: i64,
        status: &str,
        elsewhere: bool,
    ) {
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
             VALUES (?1, ?2, 'full', ?3, '/src')",
            params![unit_id, version, status],
        )
        .unwrap();
        let snap = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size)
             VALUES (?1, 'staged', 524288)",
            params![snap],
        )
        .unwrap();
        let ss = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'completed')",
            params![ss, snap, vol_id],
        )
        .unwrap();
        if elsewhere {
            conn.execute(
                &format!(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                          capacity_bytes, status)
                     VALUES ('AWAY-v{version}', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')"
                ),
                [],
            )
            .unwrap();
            let away = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO stage_sets (snapshot_id, status, slice_size)
                 VALUES (?1, 'staged', 524288)",
                params![snap],
            )
            .unwrap();
            let ss2 = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (?1, ?2, ?3, 'completed')",
                params![ss2, snap, away],
            )
            .unwrap();
        }
    }

    #[test]
    fn at_stake_reports_zero_when_the_volume_is_the_last_eligible_copy() {
        let (conn, unit_id, here) = setup_at_stake(false);
        let rows = versions_at_stake(&conn, unit_id, here).unwrap();
        assert_eq!(rows.len(), 1, "one current version is at stake: {rows:?}");
        assert_eq!(rows[0].version, 1);
        assert_eq!(
            rows[0].copies_after, 0,
            "nothing else carries v1, so retiring HERE takes it to zero"
        );
    }

    #[test]
    fn at_stake_reports_the_survivor_when_another_volume_carries_the_version() {
        let (conn, unit_id, here) = setup_at_stake(true);
        let rows = versions_at_stake(&conn, unit_id, here).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].copies_after, 1,
            "AWAY-v1 is sealed and carries the same snapshot"
        );
    }

    /// ADR-0012's per-version reading of "last one" (issue #153): a unit
    /// with v1 elsewhere and v2 only here has ZERO remaining for v2, and
    /// that is exactly the case the floor exists for. A per-UNIT count
    /// would read this unit as covered and wave the retirement through.
    #[test]
    fn at_stake_is_per_version_not_per_unit() {
        let (conn, unit_id, here) = setup_at_stake(true);
        add_version(&conn, unit_id, here, 2, "current", false);
        let rows = versions_at_stake(&conn, unit_id, here).unwrap();
        assert_eq!(
            rows.len(),
            2,
            "both current versions are at stake: {rows:?}"
        );
        assert_eq!(rows[0].version, 1);
        assert_eq!(rows[0].copies_after, 1);
        assert_eq!(rows[1].version, 2);
        assert_eq!(
            rows[1].copies_after, 0,
            "v2 is only here; the unit having v1 elsewhere does not cover it"
        );
    }

    /// The other half of ADR-0012's rule, and the one that matters for the
    /// read-error tape: the floor is defined by what the act REMOVES. A
    /// volume that counts for nothing removes nothing, so NO version is at
    /// stake and the retirement is Tier 2 at most. Refusing here would make
    /// the command useless exactly when an operator needs it most.
    fn at_stake_is_empty_for_volume_status(status: &str) {
        let (conn, unit_id, here) = setup_at_stake(false);
        // Issue #242: 'quarantined' is a condition now, not a status --
        // translate it onto `observed_condition`, leaving `status` at
        // whatever `setup_at_stake` gave it ('sealed').
        if status == "quarantined" {
            conn.execute(
                "UPDATE volumes SET observed_condition = 'quarantined' WHERE id = ?1",
                params![here],
            )
            .unwrap();
        } else {
            conn.execute(
                "UPDATE volumes SET status = ?1 WHERE id = ?2",
                params![status, here],
            )
            .unwrap();
        }
        let rows = versions_at_stake(&conn, unit_id, here).unwrap();
        assert!(
            rows.is_empty(),
            "a {status} volume counts for nothing, so it removes nothing: {rows:?}"
        );
    }

    #[test]
    fn at_stake_is_empty_for_a_quarantined_volume() {
        at_stake_is_empty_for_volume_status("quarantined");
    }

    #[test]
    fn at_stake_is_empty_for_an_unsealed_volume() {
        at_stake_is_empty_for_volume_status("active");
    }

    #[test]
    fn at_stake_is_empty_for_an_already_retired_volume() {
        at_stake_is_empty_for_volume_status("retired");
    }

    /// A RELEASED version is one the operator has already given up
    /// (`snapshot mark-reclaimable`, whose own `--force` is that statement
    /// in so many words). The floor does not protect it.
    #[test]
    fn at_stake_skips_a_released_version() {
        let (conn, unit_id, here) = setup_at_stake(false);
        conn.execute(
            "UPDATE snapshots SET status = 'reclaimable' WHERE unit_id = ?1",
            params![unit_id],
        )
        .unwrap();
        let rows = versions_at_stake(&conn, unit_id, here).unwrap();
        assert!(
            rows.is_empty(),
            "a released version is not at stake: {rows:?}"
        );
    }

    /// ADR-0006 / issue #73: the count runs through `copy_count_expr`, so a
    /// recorded warehouse deposit of some OTHER eligible volume is a
    /// remaining copy here exactly as it is in `audit` and the reports.
    /// A hand-written version of this SQL would under-count it and refuse a
    /// retirement that is perfectly safe.
    #[test]
    fn at_stake_counts_a_warehouse_deposit_of_another_volume() {
        let (conn, unit_id, here) = setup_at_stake(true);
        conn.execute(
            "INSERT INTO locations (name, kind) VALUES ('glacier', 'warehouse')",
            [],
        )
        .unwrap();
        let glacier = conn.last_insert_rowid();
        let away: i64 = conn
            .query_row("SELECT id FROM volumes WHERE label = 'AWAY-v1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute(
            "INSERT INTO volume_deposits (volume_id, location_id) VALUES (?1, ?2)",
            params![away, glacier],
        )
        .unwrap();
        let rows = versions_at_stake(&conn, unit_id, here).unwrap();
        assert_eq!(
            rows[0].copies_after, 2,
            "the tape AND its deposit: {rows:?}"
        );
    }

    /// The location half, for ADR-0008's `min_locations` limb of Tier 2.
    #[test]
    fn at_stake_reports_remaining_locations() {
        let (conn, unit_id, here) = setup_at_stake(true);
        conn.execute(
            "INSERT INTO locations (name, kind) VALUES ('offsite', 'shelf')",
            [],
        )
        .unwrap();
        let offsite = conn.last_insert_rowid();
        conn.execute(
            "UPDATE volumes SET location_id = ?1 WHERE label = 'AWAY-v1'",
            params![offsite],
        )
        .unwrap();
        let rows = versions_at_stake(&conn, unit_id, here).unwrap();
        assert_eq!(
            rows[0].locations_after, 1,
            "only AWAY-v1's location survives HERE's retirement: {rows:?}"
        );
    }

    /// A completed write to a volume the unit never had is not coverage the
    /// act removes: an unrelated volume yields no rows at all.
    #[test]
    fn at_stake_is_empty_for_a_volume_the_unit_was_never_written_to() {
        let (conn, unit_id, _here) = setup_at_stake(true);
        let away: i64 = conn
            .query_row("SELECT id FROM volumes WHERE label = 'AWAY-v1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute("DELETE FROM writes WHERE volume_id = ?1", params![away])
            .unwrap();
        let rows = versions_at_stake(&conn, unit_id, away).unwrap();
        assert!(rows.is_empty(), "{rows:?}");
    }

    /// #478 (ADR-0012 item 121): a `range` write is physically on the tape,
    /// so a current version whose only write on a sealed volume is a range
    /// is at stake when that volume is retired. The control: the same row as
    /// `aborted` (confirm's proves-medium-bad branch) is not. What
    /// `copies_after` counts for a range is item 127's per-slice count, not
    /// asked here.
    #[test]
    fn at_stake_sees_a_range_write_on_a_sealed_volume() {
        let (conn, unit_id, here) = setup_at_stake(false);
        let set = |status: &str| {
            conn.execute(
                "UPDATE writes SET status = ?1, may_span = 1 WHERE volume_id = ?2",
                params![status, here],
            )
            .unwrap();
        };
        set("aborted");
        assert!(
            versions_at_stake(&conn, unit_id, here).unwrap().is_empty(),
            "control: an aborted write never reached the floor"
        );
        set("range");
        let rows = versions_at_stake(&conn, unit_id, here).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "a range on a sealed volume is on the tape, so retiring it is at stake: {rows:?}"
        );
        assert_eq!(rows[0].version, 1);
    }

    // ── the floor sees a sealed-but-unconfirmed volume (issue #276) ──
    //
    // Migration 018's case (b) verbatim: `seal()` succeeded (`sealed_at`
    // set) but confirm never passed, so `status` stayed `'initialized'`
    // and the write's own row was swept to `'interrupted'`.

    /// Rewrites `HERE` (built `sealed`/`completed` by [`setup_at_stake`])
    /// into migration 018's case (b): still holds v1's only write, but as
    /// a physically sealed tape whose confirm never completed.
    fn make_sealed_but_unconfirmed(conn: &Connection, vol_id: i64) {
        conn.execute(
            "UPDATE volumes SET status = 'initialized', sealed_at = datetime('now') \
             WHERE id = ?1",
            params![vol_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE writes SET status = 'interrupted' WHERE volume_id = ?1",
            params![vol_id],
        )
        .unwrap();
    }

    /// Test 1: the floor must see this state at all -- before issue #276's
    /// fix, `w.status = 'completed'` and `eligible("v")` both excluded it,
    /// so retiring HERE showed NO version at stake despite HERE holding the
    /// unit's only copy.
    #[test]
    fn at_stake_sees_a_sealed_but_unconfirmed_volume_as_the_last_copy() {
        let (conn, unit_id, here) = setup_at_stake(false);
        make_sealed_but_unconfirmed(&conn, here);
        let rows = versions_at_stake(&conn, unit_id, here).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "a sealed-but-unconfirmed volume still physically holds the bytes: {rows:?}"
        );
        assert_eq!(rows[0].version, 1);
        assert_eq!(
            rows[0].copies_after, 0,
            "nothing else carries v1, so retiring HERE would take it to zero"
        );
    }

    /// Test 2, the negative half of the pair above: `sealed_at` NULL means
    /// `seal()` never ran (migration 018's case (a)) -- execute finished but
    /// the tape is "not a copy" (`docs/design/layout-session.md`). This
    /// must stay excluded, or the check above would be vacuous (anything
    /// with an `interrupted` write would pass, not just a sealed one).
    #[test]
    fn at_stake_excludes_an_interrupted_volume_that_never_reached_seal() {
        let (conn, unit_id, here) = setup_at_stake(false);
        // Same as `make_sealed_but_unconfirmed` except `sealed_at` stays
        // NULL.
        conn.execute(
            "UPDATE volumes SET status = 'initialized' WHERE id = ?1",
            params![here],
        )
        .unwrap();
        conn.execute(
            "UPDATE writes SET status = 'interrupted' WHERE volume_id = ?1",
            params![here],
        )
        .unwrap();
        let rows = versions_at_stake(&conn, unit_id, here).unwrap();
        assert!(
            rows.is_empty(),
            "sealed_at is NULL -- this tape is not a copy, and must not be treated as \
             one: {rows:?}"
        );
    }

    /// Test 3: the escape hatch still works. A sealed-but-unconfirmed
    /// volume the operator has since PROVED bad (`volume verify` ->
    /// `observed_condition = 'quarantined'`) counts for nothing, exactly
    /// as it does for an ordinary sealed volume.
    #[test]
    fn at_stake_excludes_a_sealed_but_unconfirmed_volume_that_is_quarantined() {
        let (conn, unit_id, here) = setup_at_stake(false);
        make_sealed_but_unconfirmed(&conn, here);
        conn.execute(
            "UPDATE volumes SET observed_condition = 'quarantined' WHERE id = ?1",
            params![here],
        )
        .unwrap();
        let rows = versions_at_stake(&conn, unit_id, here).unwrap();
        assert!(
            rows.is_empty(),
            "a quarantined volume counts for nothing even when sealed-but-unconfirmed: \
             {rows:?}"
        );
    }

    /// Test 4: an already-retired volume counts for nothing regardless of
    /// `sealed_at` -- `holds_sealed_bytes`'s exclusion list must win over
    /// its `sealed_at IS NOT NULL` half.
    #[test]
    fn at_stake_excludes_a_sealed_but_unconfirmed_volume_that_is_retired() {
        let (conn, unit_id, here) = setup_at_stake(false);
        make_sealed_but_unconfirmed(&conn, here);
        conn.execute(
            "UPDATE volumes SET status = 'retired' WHERE id = ?1",
            params![here],
        )
        .unwrap();
        let rows = versions_at_stake(&conn, unit_id, here).unwrap();
        assert!(
            rows.is_empty(),
            "a retired volume counts for nothing, sealed_at notwithstanding: {rows:?}"
        );
    }

    /// Test 5: the asymmetry that is the entire correctness of issue #276's
    /// fix. `holds_sealed_bytes` widens who the Tier-3 floor considers at
    /// stake, but `copy_count_expr` (what `audit`/`report copies`/
    /// `min_copies` all read) must NOT widen with it -- an unconfirmed
    /// volume is not a Copy (ADR-0004).
    #[test]
    fn a_sealed_but_unconfirmed_volume_does_not_inflate_copy_count_expr() {
        let (conn, unit_id, here) = setup_at_stake(false);
        make_sealed_but_unconfirmed(&conn, here);
        let q = CoverageQuery::current_unit("?1");
        let sql = format!("SELECT {}", copy_count_expr(&q));
        let count: i64 = conn
            .query_row(&sql, params![unit_id], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 0,
            "an unconfirmed volume must not count as a copy, or `audit`/`report copies` \
             would overstate coverage this unit does not have"
        );
    }

    // ── counts_as_copy (issue #280) ──
    //
    // `volume verify`'s clean-clear message needs a per-volume answer to
    // "does this count as a copy right now", built from `eligible` rather
    // than a second hand-written status check. These four cases are the
    // ones the message itself must distinguish: `setup_at_stake`'s `HERE`
    // volume is built `status = 'sealed'`, `observed_condition = 'ok'`
    // (migration 017's default) — each test mutates exactly the column(s)
    // its name says and nothing else, so a failure here points at exactly
    // one column's handling.

    #[test]
    fn counts_as_copy_is_true_for_a_sealed_volume_in_ok_condition() {
        let (conn, _unit_id, _here) = setup_at_stake(false);
        assert!(
            counts_as_copy(&conn, "HERE").unwrap(),
            "a sealed, ok-condition volume is exactly what `eligible` admits"
        );
    }

    #[test]
    fn counts_as_copy_is_false_for_a_retired_volume() {
        let (conn, _unit_id, here) = setup_at_stake(false);
        conn.execute(
            "UPDATE volumes SET status = 'retired' WHERE id = ?1",
            params![here],
        )
        .unwrap();
        assert!(
            !counts_as_copy(&conn, "HERE").unwrap(),
            "a retired volume is not `sealed`, so `eligible` excludes it and this must too"
        );
    }

    #[test]
    fn counts_as_copy_is_false_for_an_initialized_volume() {
        let (conn, _unit_id, here) = setup_at_stake(false);
        conn.execute(
            "UPDATE volumes SET status = 'initialized' WHERE id = ?1",
            params![here],
        )
        .unwrap();
        assert!(
            !counts_as_copy(&conn, "HERE").unwrap(),
            "an `initialized` volume (never sealed, or sealed-but-unconfirmed per issue \
             #276) is not a Copy under ADR-0004"
        );
    }

    #[test]
    fn counts_as_copy_is_false_for_a_sealed_but_quarantined_volume() {
        let (conn, _unit_id, here) = setup_at_stake(false);
        conn.execute(
            "UPDATE volumes SET observed_condition = 'quarantined' WHERE id = ?1",
            params![here],
        )
        .unwrap();
        assert!(
            !counts_as_copy(&conn, "HERE").unwrap(),
            "`status = 'sealed'` alone is not enough -- `eligible` also requires \
             `observed_condition = 'ok'`, and this is the case a clean verify's clear is \
             about to fix"
        );
    }

    // --- issue #373: the coverage expressions are driven by keys ---

    /// `EXPLAIN QUERY PLAN` detail lines for `sql`, on the live schema with
    /// no planner statistics (the production shape until `PRAGMA optimize`
    /// has something to analyze).
    fn query_plan(sql: &str) -> Vec<String> {
        query_plan_on(&crate::db::open_memory().unwrap(), sql)
    }

    fn query_plan_on(conn: &Connection, sql: &str) -> Vec<String> {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        stmt.query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// The pin: in every shape of the copy count (a unit's current versions,
    /// one snapshot, a unit's every snapshot), `writes` is searched by
    /// `stage_set_id` and no single-column status index drives any table.
    /// Before migration 027 the planner started from `idx_snapshots_status`
    /// and `idx_writes_status`, so each current snapshot's subquery scanned
    /// every completed write in the catalog.
    #[test]
    fn copy_count_searches_writes_by_stage_set_id() {
        let shapes = [
            CoverageQuery::current_unit("u.id"),
            CoverageQuery {
                scope: CoverageScope::Snapshot { id_expr: "u.id" },
                exclude_volume: None,
            },
            CoverageQuery {
                scope: CoverageScope::Unit {
                    id_expr: "u.id",
                    current_only: false,
                },
                exclude_volume: Some("0"),
            },
        ];
        for q in shapes {
            let plan = query_plan(&format!(
                "SELECT u.id, {} FROM units u",
                copy_count_expr(&q)
            ));
            let writes: Vec<&String> = plan.iter().filter(|l| l.contains(" cw ")).collect();
            assert!(
                !writes.is_empty(),
                "positive control: writes is in the plan: {plan:#?}"
            );
            for line in &writes {
                assert!(
                    line.contains("USING INDEX idx_writes_stage_set (stage_set_id=?)"),
                    "writes must be searched by stage_set_id, got {line:?} in {plan:#?}"
                );
            }
            assert!(
                !plan.iter().any(|l| l.contains("_status")),
                "a status index drives the plan: {plan:#?}"
            );
        }

        // Positive control: on the schema before 027 the same check sees the
        // status indexes drive the current-versions count.
        let old = query_plan_on(
            &crate::db::open_memory_at_version(26),
            &format!(
                "SELECT u.id, {} FROM units u",
                copy_count_expr(&CoverageQuery::current_unit("u.id"))
            ),
        );
        assert!(
            old.iter().any(|l| l.contains("idx_writes_status")),
            "the check must see the pre-027 plan: {old:#?}"
        );
    }

    // ── the distinct-place floor (issue #425) ──

    /// The floor belongs to a tape-only unit alone, is `[defaults]
    /// min_locations` AS SET at any multiplier (not 2, so a literal cannot
    /// pass: 3x), and is never handed to a caller as a number to multiply.
    #[test]
    fn the_place_floor_is_min_locations_as_set_for_a_tape_only_unit_alone() {
        let mut config = crate::config::Config::default();
        config.compaction.tape_only_safety_multiplier = 3;
        for min_locations in [1, 2, 5] {
            config.defaults.min_locations = min_locations;
            assert_eq!(
                location_floor(&config, "tape_only"),
                i64::from(min_locations)
            );
            for other in ["active", "missing"] {
                assert_eq!(location_floor(&config, other), 0, "{other}");
            }
        }
        // Zero is "no floor", as it was for `unit mark-tape-only`.
        config.defaults.min_locations = 0;
        assert_eq!(location_floor(&config, "tape_only"), 0);
    }

    /// The fixture's unit holds 2 copies in 2 places (a tape at `home`, a
    /// deposit at `glacier`): it meets the floor of 2, is short of 3, and
    /// moving the deposit to `home` makes it 1 place. The `describe` words
    /// are the ones `audit` and `snapshot mark-reclaimable` print.
    #[test]
    fn the_place_predicate_counts_distinct_places_including_deposits() {
        let (conn, unit_id, _vol) = setup_unit_with_deposit("tape_only");
        let mut config = crate::config::Config::default();
        let ask = |conn: &Connection, config: &crate::config::Config, status: &str| {
            short_of_location_floor(conn, config, NamedScope::Unit(unit_id), status).unwrap()
        };
        assert_eq!(places_held(&conn, NamedScope::Unit(unit_id)).unwrap(), 2);
        assert_eq!(ask(&conn, &config, "tape_only"), None);

        config.defaults.min_locations = 3;
        let short = ask(&conn, &config, "tape_only").expect("2 places is short of 3");
        assert_eq!(
            short,
            PlaceShortfall {
                places: 2,
                floor: 3
            }
        );
        assert_eq!(
            short.describe(),
            "copies in 2 location(s), needs 3 ([defaults] min_locations)"
        );
        assert_eq!(ask(&conn, &config, "active"), None, "no floor, no question");

        conn.execute(
            "UPDATE volume_deposits
             SET location_id = (SELECT id FROM locations WHERE name = 'home')",
            [],
        )
        .unwrap();
        assert_eq!(places_held(&conn, NamedScope::Unit(unit_id)).unwrap(), 1);
        config.defaults.min_locations = 2;
        assert_eq!(
            ask(&conn, &config, "tape_only"),
            Some(PlaceShortfall {
                places: 1,
                floor: 2
            })
        );
    }

    /// The scopes are the two the callers need: a unit's CURRENT coverage is
    /// the fewest places any current version has (issue #153), and one
    /// snapshot is judged alone — `snapshot mark-reclaimable` asks it of the
    /// version that supersedes, not of the one it releases.
    #[test]
    fn the_place_predicate_judges_a_unit_by_its_thinnest_version_and_a_snapshot_alone() {
        let (conn, unit_id, _vol) = setup_unit_with_deposit("tape_only");
        let config = crate::config::Config::default();
        let v1: i64 = conn
            .query_row("SELECT id FROM snapshots WHERE version = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        // v2: current, written to one tape at `home` only (a different tape
        // from v1's, whose deposit at `glacier` would otherwise follow it).
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status, location_id)
             VALUES ('L6-0009', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed',
                     (SELECT id FROM locations WHERE name = 'home'))",
            [],
        )
        .unwrap();
        let v2_tape = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
             VALUES (?1, 2, 'full', 'current', '/tmp/photos')",
            params![unit_id],
        )
        .unwrap();
        let v2 = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size, num_slices)
             VALUES (?1, 'staged', 104857600, 1)",
            params![v2],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'completed')",
            params![conn.last_insert_rowid(), v2, v2_tape],
        )
        .unwrap();

        let unit = short_of_location_floor(&conn, &config, NamedScope::Unit(unit_id), "tape_only")
            .unwrap();
        assert_eq!(
            unit,
            Some(PlaceShortfall {
                places: 1,
                floor: 2
            }),
            "the unit is as covered as v2, its thinnest current version"
        );
        assert_eq!(
            short_of_location_floor(&conn, &config, NamedScope::Snapshot(v1), "tape_only").unwrap(),
            None,
            "v1 alone sits in 2 places"
        );
        assert_eq!(
            short_of_location_floor(&conn, &config, NamedScope::Snapshot(v2), "tape_only").unwrap(),
            Some(PlaceShortfall {
                places: 1,
                floor: 2
            })
        );
    }
    // ── the cross-volume index's versions and holders (ADR-0012 item 80) ──

    /// A catalog to ask [`index_versions`] about: tenants `alpha` and `bravo`,
    /// units, volumes in any state, versions in any state, and who wrote what
    /// where. The shared fixture of this module's item-80 tests and of
    /// `volume::index`'s.
    pub(crate) struct IndexCatalog {
        pub conn: Connection,
    }

    impl IndexCatalog {
        pub(crate) fn new() -> Self {
            let conn = crate::db::open_memory().unwrap();
            for (name, op) in [("operator", 1), ("alpha", 0), ("bravo", 0)] {
                conn.execute(
                    "INSERT INTO tenants (name, is_operator, status) VALUES (?1, ?2, 'active')",
                    params![name, op],
                )
                .unwrap();
            }
            IndexCatalog { conn }
        }

        pub(crate) fn tenant(&self, name: &str) -> i64 {
            self.conn
                .query_row(
                    "SELECT id FROM tenants WHERE name = ?1",
                    params![name],
                    |r| r.get(0),
                )
                .unwrap()
        }

        pub(crate) fn unit(&self, tenant: &str, name: &str) -> i64 {
            self.conn
                .execute(
                    "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                     VALUES (?1, ?2, ?3, 'mtime_size', 1, 'active')",
                    params![format!("uuid-{name}"), name, self.tenant(tenant)],
                )
                .unwrap();
            self.conn.last_insert_rowid()
        }

        /// A volume `label` (uuid `uuid-<label>`, LTO-6) in `status` and
        /// `condition`, sealed at `sealed_at`.
        pub(crate) fn volume(
            &self,
            label: &str,
            status: &str,
            condition: &str,
            sealed_at: Option<&str>,
        ) -> i64 {
            self.conn
                .execute(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                          capacity_bytes, status, observed_condition, uuid,
                                          sealed_at)
                     VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, ?2, ?3, ?4, ?5)",
                    params![label, status, condition, format!("uuid-{label}"), sealed_at],
                )
                .unwrap();
            self.conn.last_insert_rowid()
        }

        /// The chip serial `serial` of the cartridge `volume_id` is bound to.
        pub(crate) fn bind_serial(&self, volume_id: i64, serial: &str) {
            self.conn
                .execute(
                    "INSERT INTO cartridges (barcode, media_type, serial_number, nominal_capacity)
                     VALUES (?1, 'LTO-6', ?2, 2500000000000)",
                    params![format!("BC-{serial}"), serial],
                )
                .unwrap();
            let cartridge = self.conn.last_insert_rowid();
            self.conn
                .execute(
                    "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
                    params![cartridge, volume_id],
                )
                .unwrap();
        }

        /// Version `version` of `unit_id`, its snapshot in `status`, with a
        /// stage set whose slices have `slice_bytes` ciphertext bytes each.
        /// Returns `(snapshot_id, stage_set_id)`.
        pub(crate) fn version(
            &self,
            unit_id: i64,
            version: i64,
            status: &str,
            slice_bytes: &[i64],
        ) -> (i64, i64) {
            self.conn
                .execute(
                    "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
                     VALUES (?1, ?2, 'full', ?3, '/tmp/src')",
                    params![unit_id, version, status],
                )
                .unwrap();
            let snapshot_id = self.conn.last_insert_rowid();
            self.conn
                .execute(
                    "INSERT INTO stage_sets (snapshot_id, status, slice_size)
                     VALUES (?1, 'staged', 1048576)",
                    params![snapshot_id],
                )
                .unwrap();
            let stage_set_id = self.conn.last_insert_rowid();
            for (i, bytes) in slice_bytes.iter().enumerate() {
                self.conn
                    .execute(
                        "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes,
                                                   encrypted_bytes, sha256_plain, sha256_encrypted)
                         VALUES (?1, ?2, ?3, ?3, 'p', ?4)",
                        params![
                            stage_set_id,
                            i as i64 + 1,
                            bytes,
                            format!("e-{stage_set_id}-{i}")
                        ],
                    )
                    .unwrap();
            }
            (snapshot_id, stage_set_id)
        }

        /// A write of `stage_set_id` to `volume_id` in `status`.
        pub(crate) fn write(&self, stage_set_id: i64, volume_id: i64, status: &str) {
            self.conn
                .execute(
                    "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                     SELECT id, snapshot_id, ?2, ?3 FROM stage_sets WHERE id = ?1",
                    params![stage_set_id, volume_id, status],
                )
                .unwrap();
        }

        /// A `range` write of `stage_set_id` to `volume_id` (planned
        /// spannable) holding the slices `written`, with every later slice
        /// of the set planned but `pending` (the tape ended first).
        pub(crate) fn range(
            &self,
            stage_set_id: i64,
            volume_id: i64,
            written: std::ops::RangeInclusive<i64>,
        ) {
            self.conn
                .execute(
                    "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, may_span)
                     SELECT id, snapshot_id, ?2, 'range', 1 FROM stage_sets WHERE id = ?1",
                    params![stage_set_id, volume_id],
                )
                .unwrap();
            let write_id = self.conn.last_insert_rowid();
            self.conn
                .execute(
                    "INSERT INTO write_positions (write_id, stage_slice_id, position, status)
                     SELECT ?1, id, CAST(5 + slice_number AS TEXT),
                            CASE WHEN slice_number <= ?4 THEN 'written' ELSE 'pending' END
                       FROM stage_slices WHERE stage_set_id = ?2 AND slice_number >= ?3",
                    params![write_id, stage_set_id, written.start(), written.end()],
                )
                .unwrap();
        }

        /// `(path, kind, size)` entries as the paths of `snapshot_id`.
        pub(crate) fn paths(
            &self,
            snapshot_id: i64,
            entries: &[(&str, crate::db::files::FileKind, i64)],
        ) {
            crate::db::files::insert_version(
                &self.conn,
                snapshot_id,
                entries.iter().map(|(path, kind, size)| {
                    Ok(crate::db::files::FileEntry {
                        path: path.to_string(),
                        kind: *kind,
                        size_bytes: *size,
                        mtime_ns: None,
                        sha256: None,
                        link_target: None,
                    })
                }),
            )
            .unwrap();
        }
    }

    pub(crate) const THIS: IndexThisTape<'static> = IndexThisTape {
        label: "L6-0100",
        uuid: "uuid-L6-0100",
        cartridge_serial: "THISSERIAL",
        generation: "LTO-6",
    };

    /// `(unit, version, volume, state)` of each row, sorted.
    fn index_keys(rows: &[IndexVersionRow]) -> Vec<(String, i64, String, &'static str)> {
        let mut keys: Vec<_> = rows
            .iter()
            .map(|r| {
                (
                    r.unit.clone(),
                    r.version,
                    r.volume.clone(),
                    r.state.as_str(),
                )
            })
            .collect();
        keys.sort();
        keys
    }

    fn key(
        unit: &str,
        version: i64,
        volume: &str,
        state: &'static str,
    ) -> (String, i64, String, &'static str) {
        (unit.to_string(), version, volume.to_string(), state)
    }

    /// Item 80, in one catalog: what is listed and who holds it.
    #[test]
    fn the_index_lists_current_versions_with_a_live_copy_and_this_writes_own() {
        let c = IndexCatalog::new();
        let photos = c.unit("alpha", "photos");
        let docs = c.unit("bravo", "docs");
        let sealed = c.volume("L6-0001", "sealed", "ok", Some("2026-10-01 10:00:00"));
        let bad = c.volume(
            "L6-0002",
            "sealed",
            "quarantined",
            Some("2026-10-02 10:00:00"),
        );
        let retired = c.volume("L6-0003", "retired", "ok", Some("2026-10-03 10:00:00"));
        let erased = c.volume("L6-0004", "erased", "ok", Some("2026-10-04 10:00:00"));
        let half = c.volume("L6-0005", "initialized", "ok", Some("2026-10-05 10:00:00"));
        let imported = c.volume("L6-0006", "active", "ok", None);

        // photos v1: current, on a good sealed volume. This write copies it too.
        let (_, p1) = c.version(photos, 1, "current", &[1000, 2000]);
        c.write(p1, sealed, "completed");
        // photos v2: current, on a quarantined volume only: listed, marked.
        let (_, p2) = c.version(photos, 2, "current", &[500]);
        c.write(p2, bad, "completed");
        // photos v3 reclaimable and v4 purged, both on a good sealed volume:
        // released by the operator, so not listed.
        let (_, p3) = c.version(photos, 3, "reclaimable", &[10]);
        c.write(p3, sealed, "completed");
        let (_, p4) = c.version(photos, 4, "purged", &[10]);
        c.write(p4, sealed, "completed");
        // photos v5: this write's own, new (staged: not current yet).
        let (_, p5) = c.version(photos, 5, "staged", &[300]);

        // docs: current versions, none with a live holder.
        let (_, d1) = c.version(docs, 1, "current", &[10]);
        c.write(d1, retired, "completed");
        let (_, d2) = c.version(docs, 2, "current", &[10]);
        c.write(d2, erased, "completed");
        let (_, d3) = c.version(docs, 3, "current", &[10]);
        c.write(d3, half, "completed");
        let (_, d4) = c.version(docs, 4, "current", &[10]);
        c.write(d4, sealed, "aborted");
        let (_, d5) = c.version(docs, 5, "current", &[10]);
        c.write(d5, sealed, "interrupted");
        let (_, d6) = c.version(docs, 6, "current", &[10]);
        c.write(d6, imported, "completed");
        // docs v7: current, on the sealed volume AND a retired one: the
        // retired one is not a holder.
        let (_, d7) = c.version(docs, 7, "current", &[10]);
        c.write(d7, sealed, "completed");
        c.write(d7, retired, "completed");

        let rows = index_versions(&c.conn, &THIS, &[p1, p5]).unwrap();
        assert_eq!(
            index_keys(&rows),
            vec![
                // The older current version with a live copy, and this
                // write's second copy of it.
                key("docs", 7, "L6-0001", "sealed"),
                key("photos", 1, "L6-0001", "sealed"),
                key("photos", 1, "L6-0100", "this-tape"),
                // A quarantined holder is listed and marked.
                key("photos", 2, "L6-0002", "quarantined"),
                // This write's own new version: this tape alone.
                key("photos", 5, "L6-0100", "this-tape"),
            ]
        );
    }

    #[test]
    fn an_index_row_carries_the_holders_own_facts() {
        let c = IndexCatalog::new();
        let photos = c.unit("alpha", "photos");
        let tape = c.volume("L6-0001", "sealed", "ok", Some("2026-10-01 10:00:00"));
        c.bind_serial(tape, "HUM00042");
        let (snap, ss) = c.version(photos, 1, "current", &[1000, 2000, 500]);
        c.write(ss, tape, "completed");
        // A second, unbound holder.
        let other = c.volume("L6-0002", "sealed", "ok", None);
        c.write(ss, other, "completed");

        let rows = index_versions(&c.conn, &THIS, &[]).unwrap();
        assert_eq!(rows.len(), 2);
        let r = rows.iter().find(|r| r.volume == "L6-0001").unwrap();
        assert_eq!(r.tenant, "alpha");
        assert_eq!(r.tenant_id, c.tenant("alpha"));
        assert_eq!(
            (r.unit.as_str(), r.unit_uuid.as_str()),
            ("photos", "uuid-photos")
        );
        assert_eq!((r.snapshot_id, r.version), (snap, 1));
        assert_eq!((r.num_slices, r.version_bytes), (3, 3500));
        assert_eq!(r.volume_uuid, "uuid-L6-0001");
        assert_eq!(r.cartridge_serial, "HUM00042");
        assert_eq!(r.generation, "LTO-6");
        assert_eq!(r.state, IndexHolderState::Sealed);
        assert_eq!(r.sealed_at, "2026-10-01 10:00:00");
        let u = rows.iter().find(|r| r.volume == "L6-0002").unwrap();
        assert_eq!(u.cartridge_serial, "", "no cartridge bound: not known");
        assert_eq!(u.sealed_at, "");
    }

    #[test]
    fn this_tape_is_described_as_the_write_says_it_and_a_second_stage_set_is_one_row() {
        let c = IndexCatalog::new();
        let photos = c.unit("alpha", "photos");
        let tape = c.volume("L6-0001", "sealed", "ok", Some("2026-10-01 10:00:00"));
        let (snap, first) = c.version(photos, 1, "current", &[100]);
        c.write(first, tape, "completed");
        // A second stage set of the same version, also on the tape: one row.
        c.conn
            .execute(
                "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 1)",
                params![snap],
            )
            .unwrap();
        let second = c.conn.last_insert_rowid();
        c.write(second, tape, "completed");

        let rows = index_versions(&c.conn, &THIS, &[first]).unwrap();
        let this: Vec<_> = rows
            .iter()
            .filter(|r| r.state == IndexHolderState::ThisTape)
            .collect();
        assert_eq!(this.len(), 1);
        assert_eq!(
            (
                this[0].volume.as_str(),
                this[0].volume_uuid.as_str(),
                this[0].cartridge_serial.as_str(),
                this[0].generation.as_str(),
                this[0].sealed_at.as_str()
            ),
            ("L6-0100", "uuid-L6-0100", "THISSERIAL", "LTO-6", "")
        );
        assert_eq!(rows.iter().filter(|r| r.volume == "L6-0001").count(), 1);
        // The tape being written is never its own holder, even when the
        // catalog already calls it sealed.
        let again = index_versions(
            &c.conn,
            &IndexThisTape {
                label: "L6-0001",
                ..THIS
            },
            &[first],
        )
        .unwrap();
        assert!(
            again.iter().all(|r| r.state == IndexHolderState::ThisTape),
            "{again:?}"
        );
    }

    #[test]
    fn the_index_holder_predicate_is_sealed_whatever_the_condition() {
        let c = IndexCatalog::new();
        for (label, status, condition, listed) in [
            ("A", "sealed", "ok", true),
            ("B", "sealed", "quarantined", true),
            ("C", "initialized", "ok", false),
            ("D", "active", "ok", false),
            ("E", "retired", "ok", false),
            ("F", "erased", "quarantined", false),
        ] {
            c.volume(label, status, condition, None);
            let got: bool = c
                .conn
                .query_row(
                    &format!(
                        "SELECT {} FROM volumes WHERE label = ?1",
                        index_holder("volumes")
                    ),
                    params![label],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(got, listed, "{label}: {status}/{condition}");
        }
    }

    // ── the least-covered slice (#478; ADR-0012 items 76, 93, 127, 134, 138) ──

    /// A catalog for the per-slice counts: one unit `span` of tenant `t`,
    /// versions and stage sets of any number of slices, sealed volumes at
    /// any place, and writes of a set to a volume, whole or as a range of
    /// `written` slices (the rest of a range's planned slices `pending`, as
    /// confirm leaves them, item 121).
    pub(crate) struct SpanCatalog {
        pub conn: Connection,
        pub unit_id: i64,
    }

    impl SpanCatalog {
        pub(crate) fn new() -> Self {
            let conn = crate::db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
                [],
            )
            .unwrap();
            let tid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                 VALUES ('u-span', 'span', ?1, 'mtime_size', 1, 'active')",
                params![tid],
            )
            .unwrap();
            let unit_id = conn.last_insert_rowid();
            SpanCatalog { conn, unit_id }
        }

        /// The location `name`, made on first use.
        pub(crate) fn place(&self, name: &str) -> i64 {
            self.conn
                .execute(
                    "INSERT OR IGNORE INTO locations (name, kind) VALUES (?1, 'shelf')",
                    params![name],
                )
                .unwrap();
            self.conn
                .query_row(
                    "SELECT id FROM locations WHERE name = ?1",
                    params![name],
                    |r| r.get(0),
                )
                .unwrap()
        }

        /// Version `version` in `status`; its snapshot id.
        pub(crate) fn version(&self, version: i64, status: &str) -> i64 {
            self.conn
                .execute(
                    "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
                     VALUES (?1, ?2, 'full', ?3, '/src/span')",
                    params![self.unit_id, version, status],
                )
                .unwrap();
            self.conn.last_insert_rowid()
        }

        /// A stage set of `snapshot` with `slices` slices; its id.
        pub(crate) fn stage_set(&self, snapshot: i64, slices: i64) -> i64 {
            self.conn
                .execute(
                    "INSERT INTO stage_sets (snapshot_id, status, slice_size, num_slices)
                     VALUES (?1, 'staged', 1048576, ?2)",
                    params![snapshot, slices],
                )
                .unwrap();
            let set = self.conn.last_insert_rowid();
            for n in 1..=slices {
                self.conn
                    .execute(
                        "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes,
                                                   encrypted_bytes, sha256_plain, sha256_encrypted)
                         VALUES (?1, ?2, 100, 100, 'p', ?3)",
                        params![set, n, format!("e-{set}-{n}")],
                    )
                    .unwrap();
            }
            set
        }

        /// A sealed volume `label` in good condition, shelved at `place`.
        pub(crate) fn volume(&self, label: &str, place: Option<&str>) -> i64 {
            let location = place.map(|p| self.place(p));
            self.conn
                .execute(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                          capacity_bytes, status, location_id, sealed_at)
                     VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed', ?2,
                             '2026-10-21 10:00:00')",
                    params![label, location],
                )
                .unwrap();
            self.conn.last_insert_rowid()
        }

        /// The id of the slice `number` of `set`.
        pub(crate) fn slice(&self, set: i64, number: i64) -> i64 {
            self.conn
                .query_row(
                    "SELECT id FROM stage_slices WHERE stage_set_id = ?1 AND slice_number = ?2",
                    params![set, number],
                    |r| r.get(0),
                )
                .unwrap()
        }

        /// A write of `set` to `volume` in `status` (`range` writes are
        /// planned spannable), with a position per slice from `planned`:
        /// `written` for those in `written`, `pending` for the rest. Its id.
        pub(crate) fn write_planned(
            &self,
            set: i64,
            volume: i64,
            status: &str,
            planned: std::ops::RangeInclusive<i64>,
            written: std::ops::RangeInclusive<i64>,
        ) -> i64 {
            self.conn
                .execute(
                    "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, may_span)
                     SELECT id, snapshot_id, ?2, ?3, ?4 FROM stage_sets WHERE id = ?1",
                    params![set, volume, status, i64::from(status == "range")],
                )
                .unwrap();
            let write = self.conn.last_insert_rowid();
            for (position, n) in planned.enumerate() {
                let state = if written.contains(&n) {
                    "written"
                } else {
                    "pending"
                };
                self.conn
                    .execute(
                        "INSERT INTO write_positions (write_id, stage_slice_id, position, status)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![write, self.slice(set, n), (6 + position).to_string(), state],
                    )
                    .unwrap();
            }
            write
        }

        /// A `completed` write of all `slices` of `set`, every one `written`.
        pub(crate) fn whole(&self, set: i64, slices: i64, volume: i64) -> i64 {
            self.write_planned(set, volume, "completed", 1..=slices, 1..=slices)
        }

        /// A `range` write of `set` holding slices `first..=last`, planned
        /// to `planned_last` (the rest `pending`: the tape ended first).
        pub(crate) fn range(
            &self,
            set: i64,
            volume: i64,
            first: i64,
            last: i64,
            planned_last: i64,
        ) -> i64 {
            self.write_planned(set, volume, "range", first..=planned_last, first..=last)
        }

        /// A warehouse deposit of `volume` at `place`.
        pub(crate) fn deposit(&self, volume: i64, place: &str) {
            let location = self.place(place);
            self.conn
                .execute(
                    "INSERT INTO volume_deposits (volume_id, location_id, deposited_at, receipt)
                     VALUES (?1, ?2, '2026-10-22 00:00:00', 'rcpt')",
                    params![volume, location],
                )
                .unwrap();
        }

        fn scalar(&self, expr: &str, id: i64) -> i64 {
            self.conn
                .query_row(&format!("SELECT {expr}"), params![id], |r| r.get(0))
                .unwrap()
        }

        fn snapshot_scope(&self) -> CoverageQuery<'static> {
            CoverageQuery {
                scope: CoverageScope::Snapshot { id_expr: "?1" },
                exclude_volume: None,
            }
        }

        /// One version's copies.
        pub(crate) fn copies(&self, snapshot: i64) -> i64 {
            self.scalar(&copy_count_expr(&self.snapshot_scope()), snapshot)
        }

        /// One version's distinct places.
        pub(crate) fn places(&self, snapshot: i64) -> i64 {
            self.scalar(&location_count_expr(&self.snapshot_scope()), snapshot)
        }

        /// How many of one version's copies are deposits.
        pub(crate) fn deposits(&self, snapshot: i64) -> i64 {
            self.scalar(&deposit_count_expr(&self.snapshot_scope()), snapshot)
        }

        /// The unit's copies across its current versions.
        pub(crate) fn unit_copies(&self) -> i64 {
            self.scalar(
                &copy_count_expr(&CoverageQuery::current_unit("?1")),
                self.unit_id,
            )
        }
    }

    /// Item 93's layout with a copy target of 2, slices 1..6 cut after 2:
    /// tape 1 holds the range 1..2 (3..6 planned, the tape ended first), tape
    /// 2 the version whole (the owed run 1..6), tape 3 the range 3..6. Every
    /// slice is on two tapes, so the version has 2 copies. Tape 1 alone is
    /// no copy: slices 3..6 are on no tape. Returns the catalog, the
    /// snapshot, the set and the three volumes.
    fn item_93() -> (SpanCatalog, i64, i64, [i64; 3]) {
        let c = SpanCatalog::new();
        let snap = c.version(1, "current");
        let set = c.stage_set(snap, 6);
        let t1 = c.volume("SPAN-1", Some("home"));
        let t2 = c.volume("SPAN-2", Some("home"));
        let t3 = c.volume("SPAN-3", Some("home"));
        c.range(set, t1, 1, 2, 6);
        c.whole(set, 6, t2);
        c.range(set, t3, 3, 6, 6);
        (c, snap, set, [t1, t2, t3])
    }

    #[test]
    fn item_93s_three_tapes_count_two_copies_and_tape_one_alone_none() {
        let (c, snap, _set, [t1, t2, t3]) = item_93();
        assert_eq!(c.copies(snap), 2, "every slice is on two tapes");
        assert_eq!(c.unit_copies(), 2);
        assert_eq!(c.places(snap), 1, "all three tapes are at home");

        // Retire tape 2: tapes 1 and 3 still make one copy between them.
        let without = |vol: i64| -> i64 {
            c.scalar(
                &copy_count_expr(&CoverageQuery {
                    scope: CoverageScope::Snapshot { id_expr: "?1" },
                    exclude_volume: Some(&vol.to_string()),
                }),
                snap,
            )
        };
        assert_eq!(without(t2), 1);
        assert_eq!(without(t1), 1, "tape 2 whole; 3..6 also on tape 3");
        assert_eq!(without(t3), 1);

        // Tape 1 alone holds a range and nothing else: no copy at all.
        c.conn
            .execute(
                "UPDATE volumes SET status = 'retired' WHERE id IN (?1, ?2)",
                params![t2, t3],
            )
            .unwrap();
        assert_eq!(c.copies(snap), 0, "slices 3..6 are on no eligible tape");
        assert_eq!(c.places(snap), 0);
        assert_eq!(c.unit_copies(), 0);
    }

    /// The control for the pending positions: a range is its `written`
    /// slices only. The same three tapes with tape 3's run all `pending`
    /// (planned, never reached) leave slices 3..6 on tape 2 alone.
    #[test]
    fn a_ranges_pending_positions_hold_nothing() {
        let c = SpanCatalog::new();
        let snap = c.version(1, "current");
        let set = c.stage_set(snap, 6);
        let t1 = c.volume("SPAN-1", None);
        let t2 = c.volume("SPAN-2", None);
        let t3 = c.volume("SPAN-3", None);
        c.range(set, t1, 1, 2, 6);
        c.whole(set, 6, t2);
        c.write_planned(set, t3, "range", 3..=6, 3..=5);
        assert_eq!(c.copies(snap), 1, "slice 6 is written on tape 2 alone");
        c.conn
            .execute(
                "UPDATE write_positions SET status = 'written'
                  WHERE write_id = (SELECT id FROM writes WHERE volume_id = ?1)",
                params![t3],
            )
            .unwrap();
        assert_eq!(c.copies(snap), 2, "the positive control");
    }

    /// Item 127: "a deposit of a range volume counts for that range". Two
    /// range tapes make one copy; a deposit of the first adds a copy of 1..2
    /// only, so the version still has one copy until the second's range is
    /// deposited too. The deposit count is the same derivation: deposits that
    /// together hold the version count as one.
    #[test]
    fn a_deposit_of_a_range_volume_counts_for_that_range() {
        let c = SpanCatalog::new();
        let snap = c.version(1, "current");
        let set = c.stage_set(snap, 6);
        let t1 = c.volume("SPAN-1", Some("home"));
        let t3 = c.volume("SPAN-3", Some("home"));
        c.range(set, t1, 1, 2, 6);
        c.range(set, t3, 3, 6, 6);
        assert_eq!(
            (c.copies(snap), c.deposits(snap), c.places(snap)),
            (1, 0, 1)
        );

        c.deposit(t1, "glacier");
        assert_eq!(
            (c.copies(snap), c.deposits(snap), c.places(snap)),
            (1, 0, 1),
            "slices 3..6 are still on one tape, at one place"
        );

        c.deposit(t3, "glacier");
        assert_eq!(
            (c.copies(snap), c.deposits(snap), c.places(snap)),
            (2, 1, 2),
            "every slice on a tape at home and a deposit at glacier"
        );
    }

    /// Item 138's three cases, two stagings of one version (item 102), each
    /// against the count 1.6.0 gave for whole writes, and its documented
    /// under-count for a range sharing a tape with a whole staging.
    #[test]
    fn each_tape_deposit_and_place_counts_for_one_stage_set() {
        // (a) Two stagings whole on one tape: one copy, not two.
        let c = SpanCatalog::new();
        let snap = c.version(1, "current");
        let s1 = c.stage_set(snap, 2);
        let s2 = c.stage_set(snap, 3);
        let tape = c.volume("BOTH", Some("home"));
        c.whole(s1, 2, tape);
        c.whole(s2, 3, tape);
        c.deposit(tape, "glacier");
        assert_eq!(
            (c.copies(snap), c.deposits(snap), c.places(snap)),
            (2, 1, 2),
            "the tape and its deposit, once each; home and glacier, once each"
        );

        // (b) Two stagings at one place: two copies, one place.
        let c = SpanCatalog::new();
        let snap = c.version(1, "current");
        let s1 = c.stage_set(snap, 2);
        let s2 = c.stage_set(snap, 3);
        let a = c.volume("A", Some("home"));
        let b = c.volume("B", Some("home"));
        c.whole(s1, 2, a);
        c.whole(s2, 3, b);
        assert_eq!((c.copies(snap), c.places(snap)), (2, 1));
        let short = short_named(
            &c.conn,
            NamedScope::Snapshot(snap),
            &names(&["home"]),
            3,
            None,
        );
        assert_eq!(short, vec![("home".to_string(), 2)], "two tapes at home");

        // (c) A stage set with no slice rows counts its whole writes.
        let c = SpanCatalog::new();
        let snap = c.version(1, "current");
        let empty = c.stage_set(snap, 0);
        let sliced = c.stage_set(snap, 2);
        let a = c.volume("A", Some("home"));
        let b = c.volume("B", Some("vault"));
        c.whole(empty, 0, a);
        c.whole(sliced, 2, b);
        assert_eq!((c.copies(snap), c.places(snap)), (2, 2));

        // Where the rule is wrong it under-counts, and only when two
        // stagings share a tape: the tape is credited to the lower set, whose
        // range alone is no copy, so the whole higher set on it reads 0.
        let c = SpanCatalog::new();
        let snap = c.version(1, "current");
        let s1 = c.stage_set(snap, 4);
        let s2 = c.stage_set(snap, 2);
        let tape = c.volume("BOTH", None);
        c.range(s1, tape, 1, 2, 4);
        c.whole(s2, 2, tape);
        assert_eq!(c.copies(snap), 0, "item 138's accepted under-count");
        // Its control: the same whole set on a tape of its own is a copy.
        let other = c.volume("OTHER", None);
        c.whole(s2, 2, other);
        assert_eq!(c.copies(snap), 1);
    }

    /// Slices of two stage sets never combine (item 102): a range of one
    /// staging and the rest of another are two halves of nothing.
    #[test]
    fn ranges_of_two_stagings_do_not_combine() {
        let c = SpanCatalog::new();
        let snap = c.version(1, "current");
        let s1 = c.stage_set(snap, 4);
        let s2 = c.stage_set(snap, 4);
        let a = c.volume("A", None);
        let b = c.volume("B", None);
        c.range(s1, a, 1, 2, 4);
        c.range(s2, b, 3, 4, 4);
        assert_eq!(c.copies(snap), 0);
        // The control: the matching half of the same staging makes a copy.
        let d = c.volume("D", None);
        c.range(s1, d, 3, 4, 4);
        assert_eq!(c.copies(snap), 1);
    }

    /// The unit form across ALL versions (`volume retire`'s display): a tape
    /// holding two versions is one holder there, as 1.6.0's union counted
    /// it, and two range tapes of one version make one holder between them.
    #[test]
    fn the_any_version_unit_form_counts_a_holder_once() {
        let c = SpanCatalog::new();
        let v1 = c.version(1, "current");
        let v2 = c.version(2, "current");
        let s1 = c.stage_set(v1, 2);
        let s2 = c.stage_set(v2, 2);
        let tape = c.volume("BOTH", None);
        c.whole(s1, 2, tape);
        c.whole(s2, 2, tape);
        let any = CoverageQuery {
            scope: CoverageScope::Unit {
                id_expr: "?1",
                current_only: false,
            },
            exclude_volume: None,
        };
        assert_eq!(c.scalar(&copy_count_expr(&any), c.unit_id), 1);
        assert_eq!(c.unit_copies(), 1, "each version has one copy");

        let v3 = c.version(3, "staged");
        let s3 = c.stage_set(v3, 4);
        let a = c.volume("A", None);
        let b = c.volume("B", None);
        c.range(s3, a, 1, 2, 4);
        c.range(s3, b, 3, 4, 4);
        assert_eq!(
            c.scalar(&copy_count_expr(&any), c.unit_id),
            2,
            "BOTH, and A and B together"
        );
    }

    /// A required location is met by the tapes there that hold every slice
    /// between them (item 134: "audit's per-slice location count checks"
    /// that the tapes of a whole copy were kept together).
    #[test]
    fn two_range_tapes_at_one_named_place_are_one_copy_there() {
        let c = SpanCatalog::new();
        let snap = c.version(1, "current");
        let set = c.stage_set(snap, 6);
        let t1 = c.volume("SPAN-1", Some("offsite"));
        let t3 = c.volume("SPAN-3", Some("offsite"));
        c.range(set, t1, 1, 2, 6);
        c.range(set, t3, 3, 6, 6);
        let short = |per_name: i64| {
            short_named(
                &c.conn,
                NamedScope::Snapshot(snap),
                &names(&["offsite", "home"]),
                per_name,
                None,
            )
        };
        assert_eq!(short(1), vec![("home".to_string(), 0)]);
        assert_eq!(
            short(2),
            vec![("home".to_string(), 0), ("offsite".to_string(), 1)]
        );

        // Tape 3 moved home: neither place holds every slice.
        c.conn
            .execute(
                "UPDATE volumes SET location_id = ?1 WHERE id = ?2",
                params![c.place("home"), t3],
            )
            .unwrap();
        assert_eq!(
            short(1),
            vec![("home".to_string(), 0), ("offsite".to_string(), 0)]
        );
        assert_eq!(
            c.places(snap),
            1,
            "each slice is at one place, though no one place holds them all"
        );
    }

    /// `(slice_number, copies, locations)` of each slice of `set`.
    fn per_slice(c: &SpanCatalog, set: i64) -> Vec<(i64, i64, i64)> {
        slice_copies(&c.conn, set)
            .unwrap()
            .into_iter()
            .map(|s| (s.slice_number, s.copies, s.locations))
            .collect()
    }

    /// `slice_copies` on item 93's layout: every slice on two tapes, its
    /// fewest copies the version's count; with tape 3 away, slices 3..6
    /// owe a copy. Deposits count for the slices of their volume, places
    /// once each per slice.
    #[test]
    fn slice_copies_counts_each_slice_as_the_version_count_does() {
        let (c, snap, set, [_t1, t2, t3]) = item_93();
        let both = per_slice(&c, set);
        assert_eq!(
            both,
            (1..=6).map(|n| (n, 2, 1)).collect::<Vec<_>>(),
            "two tapes, one place, at every slice"
        );
        assert_eq!(
            both.iter().map(|s| s.1).min(),
            Some(c.copies(snap)),
            "the least-covered slice is the version's count"
        );

        c.conn
            .execute(
                "UPDATE volumes SET status = 'retired' WHERE id = ?1",
                params![t3],
            )
            .unwrap();
        c.deposit(t2, "glacier");
        assert_eq!(
            per_slice(&c, set),
            vec![
                (1, 3, 2),
                (2, 3, 2),
                (3, 2, 2),
                (4, 2, 2),
                (5, 2, 2),
                (6, 2, 2),
            ],
            "tape 2 and its deposit at every slice, tape 1 at 1..2 only"
        );
        assert_eq!(c.copies(snap), 2);
        assert_eq!(c.places(snap), 2);
    }

    /// The empty answers: a set nothing holds lists every slice at zero, a set
    /// with no slice rows (and an unknown id) lists nothing.
    #[test]
    fn slice_copies_of_a_set_nothing_holds_is_every_slice_at_zero() {
        let c = SpanCatalog::new();
        let snap = c.version(1, "staged");
        let set = c.stage_set(snap, 3);
        assert_eq!(per_slice(&c, set), vec![(1, 0, 0), (2, 0, 0), (3, 0, 0)]);
        let empty = c.stage_set(snap, 0);
        assert!(slice_copies(&c.conn, empty).unwrap().is_empty());
        assert!(slice_copies(&c.conn, 9999).unwrap().is_empty());
    }

    /// Item 138 at the slice: a staging whose tape is credited to a lower
    /// staging reads no copy there, as the version's count does.
    #[test]
    fn slice_copies_takes_item_138s_credit() {
        let c = SpanCatalog::new();
        let snap = c.version(1, "current");
        let s1 = c.stage_set(snap, 2);
        let s2 = c.stage_set(snap, 2);
        let tape = c.volume("BOTH", Some("home"));
        c.whole(s1, 2, tape);
        c.whole(s2, 2, tape);
        assert_eq!(per_slice(&c, s1), vec![(1, 1, 1), (2, 1, 1)]);
        assert_eq!(per_slice(&c, s2), vec![(1, 0, 0), (2, 0, 0)]);
        assert_eq!(c.copies(snap), 1);
    }

    fn promoted(c: &SpanCatalog, set: i64, also: Option<i64>) -> bool {
        let also = also.map(|v| v.to_string());
        c.conn
            .query_row(
                &format!(
                    "SELECT {}",
                    every_slice_on_sealed_volume_sql("?1", also.as_deref())
                ),
                params![set],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// Item 128's promotion rule, both ways: tape 1's range alone does not
    /// promote; with tape 3's range on a sealed volume it does, and only once
    /// tape 3 is sealed (or named as the volume being sealed). A quarantined
    /// condition does not unseal a tape (X33); a retired one holds nothing.
    #[test]
    fn every_slice_on_a_sealed_volume_is_items_128_rule() {
        let c = SpanCatalog::new();
        let snap = c.version(1, "staged");
        let set = c.stage_set(snap, 6);
        let t1 = c.volume("SPAN-1", None);
        c.range(set, t1, 1, 2, 6);
        assert!(!promoted(&c, set, None), "slices 3..6 are on no tape");

        let t3 = c.volume("SPAN-3", None);
        c.range(set, t3, 3, 6, 6);
        c.conn
            .execute(
                "UPDATE volumes SET status = 'initialized' WHERE id = ?1",
                params![t3],
            )
            .unwrap();
        assert!(!promoted(&c, set, None), "tape 3 is not sealed yet");
        assert!(
            promoted(&c, set, Some(t3)),
            "the volume being sealed counted"
        );
        assert!(
            !promoted(&c, set, Some(t1)),
            "naming another volume does not"
        );

        c.conn
            .execute(
                "UPDATE volumes SET status = 'sealed', observed_condition = 'quarantined'
                  WHERE id = ?1",
                params![t3],
            )
            .unwrap();
        assert!(promoted(&c, set, None), "sealed, whatever its condition");

        c.conn
            .execute(
                "UPDATE volumes SET status = 'retired' WHERE id = ?1",
                params![t1],
            )
            .unwrap();
        assert!(!promoted(&c, set, None), "1..2 were only on tape 1");
        let t2 = c.volume("SPAN-2", None);
        c.whole(set, 6, t2);
        assert!(promoted(&c, set, None), "a whole write holds every slice");

        // A set with no slice rows is never promoted by this rule.
        let empty = c.stage_set(snap, 0);
        c.whole(empty, 0, t2);
        assert!(!promoted(&c, empty, None));
    }

    /// Item 128's floor at a copy target of 1 (item 129's hazard): after tape
    /// 1 the version is still `staged`, and tape 1 is the only place slices
    /// 1..2 are, so retiring it is at stake with nothing left. Once another
    /// tape holds 1..2 it is not; nor is a released version, a quarantined
    /// tape, or a version tape 1 holds nothing of alone.
    #[test]
    fn at_stake_protects_a_span_in_progress_that_is_not_current() {
        let c = SpanCatalog::new();
        let snap = c.version(1, "staged");
        let set = c.stage_set(snap, 6);
        let t1 = c.volume("SPAN-1", None);
        c.range(set, t1, 1, 2, 6);
        let rows = versions_at_stake(&c.conn, c.unit_id, t1).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            (rows[0].snapshot_id, rows[0].version, rows[0].copies_after),
            (snap, 1, 0)
        );

        // A version the operator released is not protected (condition 1).
        for released in ["reclaimable", "purged"] {
            c.conn
                .execute(
                    "UPDATE snapshots SET status = ?1 WHERE id = ?2",
                    params![released, snap],
                )
                .unwrap();
            assert!(versions_at_stake(&c.conn, c.unit_id, t1)
                .unwrap()
                .is_empty());
        }
        c.conn
            .execute(
                "UPDATE snapshots SET status = 'staged' WHERE id = ?1",
                params![snap],
            )
            .unwrap();

        // A quarantined tape counts for nothing, and retiring it removes
        // nothing (condition 3's escape, for a range).
        c.conn
            .execute(
                "UPDATE volumes SET observed_condition = 'quarantined' WHERE id = ?1",
                params![t1],
            )
            .unwrap();
        assert!(versions_at_stake(&c.conn, c.unit_id, t1)
            .unwrap()
            .is_empty());
        c.conn
            .execute(
                "UPDATE volumes SET observed_condition = 'ok' WHERE id = ?1",
                params![t1],
            )
            .unwrap();

        // Slice 2 also on another tape: slice 1 is still tape 1's alone.
        let other = c.volume("OTHER", None);
        c.range(set, other, 2, 6, 6);
        assert_eq!(versions_at_stake(&c.conn, c.unit_id, t1).unwrap().len(), 1);
        // Slices 1..2 both elsewhere: tape 1 holds nothing alone.
        let again = c.volume("AGAIN", None);
        c.range(set, again, 1, 1, 1);
        assert!(
            versions_at_stake(&c.conn, c.unit_id, t1)
                .unwrap()
                .is_empty(),
            "every slice of tape 1's range has another holder"
        );
        // And the other tape, which alone holds 3..6, is at stake.
        assert_eq!(
            versions_at_stake(&c.conn, c.unit_id, other).unwrap().len(),
            1
        );
    }

    /// The floor for a CURRENT spanning version reads the per-slice count:
    /// with item 93's three tapes, no single retirement leaves no copy, and
    /// after tape 2 is gone, tape 1 and tape 3 are each the last holder of
    /// their slices.
    #[test]
    fn at_stake_counts_a_current_spans_copies_slice_by_slice() {
        let (c, _snap, _set, [t1, t2, t3]) = item_93();
        for t in [t1, t2, t3] {
            let rows = versions_at_stake(&c.conn, c.unit_id, t).unwrap();
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert_eq!(rows[0].copies_after, 1, "volume {t}");
            assert_eq!(rows[0].locations_after, 1);
        }
        c.conn
            .execute(
                "UPDATE volumes SET status = 'retired' WHERE id = ?1",
                params![t2],
            )
            .unwrap();
        for t in [t1, t3] {
            assert_eq!(
                versions_at_stake(&c.conn, c.unit_id, t).unwrap()[0].copies_after,
                0
            );
        }
    }

    /// `sole_slices` with range rows on both sides: a range tape's slices that
    /// no other eligible tape holds, and none that another range holds. A
    /// `pending` position (planned past the cut) is not on the tape.
    #[test]
    fn sole_slices_reads_ranges_on_both_sides() {
        let (c, _snap, set, [t1, t2, t3]) = item_93();
        let numbers = |vol: i64| -> Vec<i64> {
            let sole = sole_slices(&c.conn, vol).unwrap();
            assert!(sole
                .iter()
                .all(|s| s.stage_set_id == set && s.unit_name == "span"));
            let ids: Vec<i64> = sole.iter().map(|s| s.stage_slice_id).collect();
            (1..=6)
                .filter(|n| ids.contains(&c.slice(set, *n)))
                .collect()
        };
        assert!(numbers(t1).is_empty(), "1..2 are on tape 2 too");
        assert!(numbers(t2).is_empty());
        assert!(numbers(t3).is_empty());

        c.conn
            .execute(
                "UPDATE volumes SET status = 'retired' WHERE id = ?1",
                params![t2],
            )
            .unwrap();
        assert_eq!(numbers(t1), vec![1, 2], "not 3..6: planned, never written");
        assert_eq!(numbers(t3), vec![3, 4, 5, 6]);
    }

    /// `(unit, version, volume, state, first_slice, last_slice)` of each row,
    /// sorted.
    fn index_runs(rows: &[IndexVersionRow]) -> Vec<(String, i64, String, &'static str, i64, i64)> {
        let mut runs: Vec<_> = rows
            .iter()
            .map(|r| {
                (
                    r.unit.clone(),
                    r.version,
                    r.volume.clone(),
                    r.state.as_str(),
                    r.first_slice,
                    r.last_slice,
                )
            })
            .collect();
        runs.sort();
        runs
    }

    /// §1.5 from 1.7.0 (ADR-0012 items 121, 126, 141): a `range` write is a
    /// holder, listed with the run of slices the catalog records `written`
    /// there; a whole write's run is 1..N; this tape's row carries the run
    /// its plan holds; a `cut` write holds nothing. A spanning version that is
    /// not yet current is listed only by a write that carries it.
    #[test]
    fn the_index_names_each_holders_run_of_slices() {
        let c = IndexCatalog::new();
        let photos = c.unit("alpha", "photos");
        let docs = c.unit("bravo", "docs");
        let at = Some("2026-10-21 10:00:00");
        let whole = c.volume("L6-0001", "sealed", "ok", at);
        let head = c.volume("L6-0002", "sealed", "ok", at);
        let tail = c.volume("L6-0003", "sealed", "ok", at);
        let cut = c.volume("L6-0004", "sealed", "ok", at);
        let unsealed = c.volume("L6-0006", "initialized", "ok", at);

        let (_, p1) = c.version(photos, 1, "current", &[10, 10, 10, 10]);
        c.write(p1, whole, "completed");
        c.range(p1, head, 1..=2);
        c.range(p1, tail, 3..=4);
        c.write(p1, cut, "cut");
        c.range(p1, unsealed, 1..=4);

        // docs v1: a span in progress, staged, on one tape so far.
        let span = c.volume("L6-0005", "sealed", "ok", at);
        let (_, d1) = c.version(docs, 1, "staged", &[10, 10, 10]);
        c.range(d1, span, 1..=2);

        let photos_rows = vec![
            (
                "photos".to_string(),
                1,
                "L6-0001".to_string(),
                "sealed",
                1,
                4,
            ),
            (
                "photos".to_string(),
                1,
                "L6-0002".to_string(),
                "sealed",
                1,
                2,
            ),
            (
                "photos".to_string(),
                1,
                "L6-0003".to_string(),
                "sealed",
                3,
                4,
            ),
        ];
        assert_eq!(
            index_runs(&index_versions(&c.conn, &THIS, &[]).unwrap()),
            photos_rows,
            "docs is not current and this write does not carry it"
        );

        // This write carries photos' slices 3..4 and the rest of docs.
        let rows = index_versions_for_runs(
            &c.conn,
            &THIS,
            &[
                IndexRun {
                    stage_set_id: p1,
                    slices: Some((3, 4)),
                },
                IndexRun {
                    stage_set_id: d1,
                    slices: Some((3, 3)),
                },
            ],
        )
        .unwrap();
        let mut want = vec![
            ("docs".to_string(), 1, "L6-0005".to_string(), "sealed", 1, 2),
            (
                "docs".to_string(),
                1,
                "L6-0100".to_string(),
                "this-tape",
                3,
                3,
            ),
        ];
        want.extend(photos_rows);
        want.push((
            "photos".to_string(),
            1,
            "L6-0100".to_string(),
            "this-tape",
            3,
            4,
        ));
        want.sort();
        assert_eq!(index_runs(&rows), want);

        // The whole form: this tape's run is every slice.
        let rows = index_versions(&c.conn, &THIS, &[d1]).unwrap();
        let this = rows
            .iter()
            .find(|r| r.state == IndexHolderState::ThisTape)
            .unwrap();
        assert_eq!(
            (this.num_slices, this.first_slice, this.last_slice),
            (3, 1, 3)
        );
    }

    /// A range with no slice `written` (a row no session leaves) states no run,
    /// and a stage set with no slices recorded states none either.
    #[test]
    fn an_index_run_the_catalog_does_not_know_is_zero() {
        let c = IndexCatalog::new();
        let photos = c.unit("alpha", "photos");
        let tape = c.volume("L6-0001", "sealed", "ok", Some("2026-10-21 10:00:00"));
        let (_, p1) = c.version(photos, 1, "current", &[10, 10]);
        c.range(p1, tape, 1..=2);
        c.conn
            .execute("UPDATE write_positions SET status = 'pending'", [])
            .unwrap();
        let (_, p2) = c.version(photos, 2, "current", &[]);
        c.write(p2, tape, "completed");
        let rows = index_runs(&index_versions(&c.conn, &THIS, &[]).unwrap());
        assert_eq!(
            rows,
            vec![
                (
                    "photos".to_string(),
                    1,
                    "L6-0001".to_string(),
                    "sealed",
                    0,
                    0
                ),
                (
                    "photos".to_string(),
                    2,
                    "L6-0001".to_string(),
                    "sealed",
                    0,
                    0
                ),
            ]
        );
    }

    /// The tables a statement reads in full by its plan: a `SCAN` of a table
    /// with no index, or an AUTOMATIC index SQLite builds by reading one
    /// (catalog_budget's reading, on the live schema). Parameters stay unbound
    /// (`raw_query` does not check their count), which plans as for any value.
    fn tables_read_in_full(sql: &str) -> Vec<String> {
        let conn = crate::db::open_memory().unwrap();
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let mut rows = stmt.raw_query();
        let mut plan: Vec<String> = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            plan.push(row.get(3).unwrap());
        }
        plan.into_iter()
            .filter(|l| {
                let scan = l
                    .strip_prefix("SCAN ")
                    .is_some_and(|rest| !rest.starts_with('(') && !rest.starts_with("CONSTANT"))
                    && !l.contains(" USING ");
                scan || l.contains("AUTOMATIC")
            })
            .collect()
    }

    /// #478: every shape of the per-slice counts, and the statements of
    /// `slice_copies`, the named-place count and the promotion rule, is
    /// driven by keys: no table is read in full (#373's rule, catalog_budget's
    /// pin, for the shapes no budgeted command runs). The positive control is
    /// a predicate on an unindexed column.
    #[test]
    fn the_per_slice_counts_read_no_table_in_full() {
        assert_eq!(
            tables_read_in_full("SELECT id FROM writes WHERE status = 'range'").len(),
            1,
            "the reader sees a full scan"
        );
        let scopes = [
            CoverageQuery::current_unit("?1"),
            CoverageQuery {
                scope: CoverageScope::Snapshot { id_expr: "?1" },
                exclude_volume: Some("?2"),
            },
            CoverageQuery {
                scope: CoverageScope::Unit {
                    id_expr: "?1",
                    current_only: false,
                },
                exclude_volume: None,
            },
        ];
        let mut statements = vec![
            slice_copies_sql(),
            // #493: the per-location targets and one location, both forms;
            // a name is found by the locations' unique index.
            copies_at_targets_sql(NamedScope::Unit(1), true, 2).0,
            copies_at_targets_sql(NamedScope::Snapshot(1), false, 1).0,
            copies_at_location_sql(NamedScope::Unit(1), true).0,
            copies_at_location_sql(NamedScope::Snapshot(1), false).0,
            format!(
                "SELECT {}",
                every_slice_on_sealed_volume_sql("?1", Some("?2"))
            ),
        ];
        for q in scopes {
            for expr in [
                copy_count_expr(&q),
                location_count_expr(&q),
                deposit_count_expr(&q),
            ] {
                statements.push(format!("SELECT {expr}"));
            }
        }
        for sql in &statements {
            let full = tables_read_in_full(sql);
            assert!(full.is_empty(), "{full:#?} in the plan of\n{sql}");
        }
    }

    // ── copies at a location (#493; ADR-0012 items 149, 150, 156-158, 160) ──

    /// A resolved policy for `unit` (the defaults: `min_copies` 2) with
    /// `copies_at` set to `map`, as a dotfile's `[policy.copies_at]` resolves.
    fn policy_with_map(
        conn: &Connection,
        unit: &str,
        map: &[(&str, i64)],
    ) -> crate::policy::ResolvedPolicy {
        let unit = crate::db::queries::get_unit_by_name(conn, unit)
            .unwrap()
            .unwrap();
        let mut resolved =
            crate::policy::resolve(conn, &crate::config::Config::default(), &unit).unwrap();
        resolved.copies_at = map.iter().map(|(n, c)| (n.to_string(), *c)).collect();
        resolved
    }

    fn btree(pairs: &[(&str, i64)]) -> std::collections::BTreeMap<String, i64> {
        pairs.iter().map(|(n, c)| (n.to_string(), *c)).collect()
    }

    /// Item 150: a copy at X is an eligible tape shelved at X or a deposit at
    /// X (a warehouse counts by its deposits, item 157), and every location is
    /// answered, 0 where nothing is. The one-location form agrees, pretends a
    /// volume away like every `_excluding` count, and reads 0 for a name no
    /// location has; the whole-catalog form gives the same map in one pass.
    #[test]
    fn copies_at_every_location_counts_shelf_holders_and_deposits() {
        let (conn, unit_id, vol) = setup_unit_with_deposit("active");
        conn.execute(
            "INSERT INTO locations (name, kind) VALUES ('offsite', 'shelf')",
            [],
        )
        .unwrap();
        let unit = NamedScope::Unit(unit_id);
        assert_eq!(
            copies_at_every_location(&conn, unit).unwrap(),
            btree(&[("glacier", 1), ("home", 1), ("offsite", 0)])
        );

        add_tape_copy_at(&conn, "L6-0004", "home");
        let every = copies_at_every_location(&conn, unit).unwrap();
        assert_eq!(every, btree(&[("glacier", 1), ("home", 2), ("offsite", 0)]));
        assert_eq!(copies_at_location(&conn, unit, "home", None).unwrap(), 2);
        assert_eq!(
            copies_at_location(&conn, unit, "home", Some(vol)).unwrap(),
            1,
            "L6-0003 pretended away"
        );
        assert_eq!(
            copies_at_location(&conn, unit, "glacier", Some(vol)).unwrap(),
            0,
            "and the deposit made from it"
        );
        assert_eq!(copies_at_location(&conn, unit, "nowhere", None).unwrap(), 0);

        let snapshot: i64 = conn
            .query_row("SELECT id FROM snapshots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            copies_at_every_location(&conn, NamedScope::Snapshot(snapshot)).unwrap(),
            every
        );
        assert_eq!(
            copies_at_every_location_by_unit(&conn).unwrap()[&unit_id],
            every
        );
    }

    /// Item 150's rule is the least-covered slice's (items 127, 134): two
    /// range tapes at one place make one copy there between them, and a place
    /// holding only some slices holds no copy. Per unit, the fewest any
    /// current version has there (issue #153).
    #[test]
    fn copies_at_a_location_count_a_range_slice_by_slice() {
        let c = SpanCatalog::new();
        let snap = c.version(1, "current");
        let set = c.stage_set(snap, 6);
        let t1 = c.volume("SPAN-1", Some("offsite"));
        let t3 = c.volume("SPAN-3", Some("offsite"));
        c.range(set, t1, 1, 2, 6);
        c.range(set, t3, 3, 6, 6);
        c.place("home");
        let unit = NamedScope::Unit(c.unit_id);
        assert_eq!(
            copies_at_every_location(&c.conn, unit).unwrap(),
            btree(&[("home", 0), ("offsite", 1)])
        );

        // Tape 3 moved home: neither place holds every slice.
        c.conn
            .execute(
                "UPDATE volumes SET location_id = ?1 WHERE id = ?2",
                params![c.place("home"), t3],
            )
            .unwrap();
        assert_eq!(
            copies_at_every_location(&c.conn, unit).unwrap(),
            btree(&[("home", 0), ("offsite", 0)])
        );
        // The control: a whole copy at home is one there.
        let whole = c.volume("WHOLE", Some("home"));
        c.whole(set, 6, whole);
        assert_eq!(copies_at_location(&c.conn, unit, "home", None).unwrap(), 1);

        // A second current version with nothing at home: the unit holds 0
        // there, the fewest of its versions.
        let v2 = c.version(2, "current");
        let s2 = c.stage_set(v2, 1);
        let other = c.volume("V2", Some("offsite"));
        c.whole(s2, 1, other);
        assert_eq!(copies_at_location(&c.conn, unit, "home", None).unwrap(), 0);
        assert_eq!(
            copies_at_location(&c.conn, NamedScope::Snapshot(snap), "home", None).unwrap(),
            1
        );
        assert_eq!(
            copies_at_every_location_by_unit(&c.conn).unwrap()[&c.unit_id]["home"],
            0
        );
    }

    /// Item 138 at a place: two stagings of one version on ONE tape there are
    /// one copy there; on two tapes there, two copies there (and one place).
    #[test]
    fn two_stagings_of_one_version_count_each_tape_at_a_place_once() {
        let c = SpanCatalog::new();
        let snap = c.version(1, "current");
        let s1 = c.stage_set(snap, 2);
        let s2 = c.stage_set(snap, 3);
        let tape = c.volume("BOTH", Some("home"));
        c.whole(s1, 2, tape);
        c.whole(s2, 3, tape);
        let unit = NamedScope::Unit(c.unit_id);
        assert_eq!(copies_at_location(&c.conn, unit, "home", None).unwrap(), 1);

        let c = SpanCatalog::new();
        let snap = c.version(1, "current");
        let s1 = c.stage_set(snap, 2);
        let s2 = c.stage_set(snap, 3);
        let a = c.volume("A", Some("home"));
        let b = c.volume("B", Some("home"));
        c.whole(s1, 2, a);
        c.whole(s2, 3, b);
        assert_eq!((c.copies(snap), c.places(snap)), (2, 1));
        assert_eq!(
            copies_at_location(&c.conn, NamedScope::Unit(c.unit_id), "home", None).unwrap(),
            2
        );
    }

    /// Item 149: the copy floor is `max(min_copies, Σ copies_at) ×
    /// multiplier`, and each location's target is its count × multiplier for
    /// a tape-only unit. With no map the floor is `min_copies`, as before.
    #[test]
    fn the_floor_and_each_target_follow_the_map_and_the_multiplier() {
        let (conn, unit_id, _vol) = setup_unit_with_deposit("active");
        let mut config = crate::config::Config::default();
        config.compaction.tape_only_safety_multiplier = 3;

        let none = policy_with_map(&conn, "photos", &[]);
        let at = copies_at_every_location(&conn, NamedScope::Unit(unit_id)).unwrap();
        assert_eq!(
            Shortfall::judge(&config, &none, "active", 2, &at).min_copies,
            2
        );
        assert_eq!(
            Shortfall::judge(&config, &none, "tape_only", 2, &at).min_copies,
            6
        );

        let map = policy_with_map(&conn, "photos", &[("home", 2), ("glacier", 1)]);
        let active = Shortfall::judge(&config, &map, "active", 2, &at);
        assert_eq!(active.min_copies, 3, "max(2, 2 + 1)");
        assert_eq!(location_target(&config, &map, "home", "active"), 2);
        assert_eq!(location_target(&config, &map, "offsite", "active"), 0);
        let tape_only = Shortfall::judge(&config, &map, "tape_only", 2, &at);
        assert_eq!(tape_only.min_copies, 9, "3 x 3");
        assert_eq!(location_target(&config, &map, "home", "tape_only"), 6);
        assert_eq!(
            tape_only.locations,
            vec![
                LocationShort {
                    location: "glacier".to_string(),
                    copies: 1,
                    target: 3
                },
                LocationShort {
                    location: "home".to_string(),
                    copies: 1,
                    target: 6
                },
            ]
        );
    }

    /// Items 149, 156 and 160: `Shortfall`'s per-location half comes from the
    /// resolved map alone. A dotfile map naming only `glacier` replaces an
    /// archive set's `home` (still in `required_locations`), so `home` is not
    /// asked for; and a name in both is asked for once. The DB form agrees
    /// with the pure judgement.
    #[test]
    fn shortfall_is_judged_from_the_map_alone() {
        let (conn, unit_id, _vol) = setup_unit_with_deposit("active");
        let config = crate::config::Config::default();
        conn.execute(
            "INSERT INTO locations (name, kind) VALUES ('offsite', 'shelf')",
            [],
        )
        .unwrap();

        // The archive set's `required_locations` folded in, as resolve does.
        let mut folded = policy(&conn, 2, &["home", "offsite"]);
        let short = shortfall(&conn, &config, unit_id, "active", &folded).unwrap();
        assert_eq!(short.describe(), "no copy at offsite");
        assert_eq!(short.locations.len(), 1, "{short:?}");

        // A dotfile map replaces it: `home` and `offsite` are no longer asked
        // for, though `required_locations` still names them.
        folded.copies_at = btree(&[("glacier", 2)]);
        assert_eq!(folded.required_locations, names(&["home", "offsite"]));
        let short = shortfall(&conn, &config, unit_id, "active", &folded).unwrap();
        assert_eq!(
            short.locations,
            vec![LocationShort {
                location: "glacier".to_string(),
                copies: 1,
                target: 2
            }]
        );
        assert_eq!(
            short.describe(),
            "1 copy at required location glacier, needs 2",
            "an active unit's target, no tape-only note"
        );
        let at = copies_at_every_location(&conn, NamedScope::Unit(unit_id)).unwrap();
        assert_eq!(short, Shortfall::judge(&config, &folded, "active", 2, &at));

        // Met, and a 0 target asks for nothing.
        folded.copies_at = btree(&[("glacier", 1), ("offsite", 0)]);
        assert!(!shortfall(&conn, &config, unit_id, "active", &folded)
            .unwrap()
            .any());
    }

    thread_local! {
        static STATEMENTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn count_statement(event: rusqlite::trace::TraceEvent<'_>) {
        if matches!(event, rusqlite::trace::TraceEvent::Stmt(..)) {
            STATEMENTS.with(|n| n.set(n.get() + 1));
        }
    }

    /// The statements `f` runs on `conn`.
    fn statements_run(conn: &Connection, f: impl FnOnce()) -> usize {
        STATEMENTS.with(|n| n.set(0));
        conn.trace_v2(
            rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
            Some(count_statement),
        );
        f();
        conn.trace_v2(rusqlite::trace::TraceEventCodes::empty(), None);
        STATEMENTS.with(|n| n.get())
    }

    /// The per-location predicate runs no statement when the map has no
    /// target above 0 (the catalog_budget fixture's case, so `audit` and
    /// `collection status` cost what they did), and one otherwise.
    #[test]
    fn short_of_copies_at_runs_no_statement_without_a_target() {
        let (conn, unit_id, _vol) = setup_unit_with_deposit("active");
        let config = crate::config::Config::default();
        let short = |resolved: &crate::policy::ResolvedPolicy| {
            short_of_copies_at(
                &conn,
                &config,
                NamedScope::Unit(unit_id),
                resolved,
                "active",
                None,
            )
            .unwrap()
        };
        let none = policy_with_map(&conn, "photos", &[]);
        let zero = policy_with_map(&conn, "photos", &[("home", 0)]);
        let two = policy_with_map(&conn, "photos", &[("home", 2), ("glacier", 3)]);
        assert_eq!(
            statements_run(&conn, || assert!(short(&none).is_empty())),
            0
        );
        assert_eq!(
            statements_run(&conn, || assert!(short(&zero).is_empty())),
            0
        );
        assert_eq!(
            statements_run(&conn, || assert_eq!(short(&two).len(), 2)),
            1,
            "the positive control: one statement for every target name"
        );
    }

    /// Item 158's two groups and neither, for one location X = `offsite`.
    #[test]
    fn need_at_a_location_is_item_158s_two_groups() {
        let (conn, _unit_id, _vol) = setup_unit_with_deposit("active");
        let config = crate::config::Config::default();
        let judge = |map: &[(&str, i64)], status: &str, copies: i64, here: i64| {
            NeedAt::judge(
                &config,
                &policy_with_map(&conn, "photos", map),
                status,
                "offsite",
                copies,
                here,
            )
        };

        // Group 1: a target at X, unmet, by how far short.
        let need = judge(&[("offsite", 2)], "active", 3, 0);
        assert_eq!(
            need,
            NeedAt::Target {
                target: 2,
                copies_here: 0,
                copies: 3
            }
        );
        assert_eq!((need.group(), need.short_by()), (Some(1), 2));
        assert_eq!(judge(&[("offsite", 2)], "active", 3, 1).short_by(), 1);
        // A tape-only unit's target is multiplied (2 by default).
        assert_eq!(judge(&[("offsite", 1)], "tape_only", 5, 1).short_by(), 1);
        // Met: nothing, even short of the floor.
        assert_eq!(judge(&[("offsite", 1)], "active", 1, 1), NeedAt::None);

        // Group 2: short of the floor (2), no target at X, no copy at X.
        let need = judge(&[("home", 1)], "active", 1, 0);
        assert_eq!(
            need,
            NeedAt::Floor {
                floor: 2,
                copies: 1
            }
        );
        assert_eq!((need.group(), need.short_by()), (Some(2), 1));
        assert_eq!(
            judge(&[], "active", 0, 0),
            NeedAt::Floor {
                floor: 2,
                copies: 0
            },
            "no map at all"
        );
        assert_eq!(
            judge(&[("offsite", 0)], "active", 0, 0).group(),
            Some(2),
            "a 0 target is no target"
        );
        // A copy at X already: never group 2, so a unit holding all its
        // copies at X is never picked for X.
        assert_eq!(judge(&[("home", 1)], "active", 1, 1), NeedAt::None);
        // The floor met: nothing.
        assert_eq!(judge(&[("home", 1)], "active", 2, 0), NeedAt::None);
    }

    /// `need_at` asks the catalog for the two counts `NeedAt::judge` takes.
    #[test]
    fn need_at_reads_the_counts_from_the_catalog() {
        let (conn, _unit_id, _vol) = setup_unit_with_deposit("active");
        let config = crate::config::Config::default();
        let unit = crate::db::queries::get_unit_by_name(&conn, "photos")
            .unwrap()
            .unwrap();
        let resolved = policy_with_map(&conn, "photos", &[("home", 2)]);
        assert_eq!(
            need_at(&conn, &config, &unit, &resolved, "home").unwrap(),
            NeedAt::Target {
                target: 2,
                copies_here: 1,
                copies: 2
            }
        );
        // 2 copies meet the floor of max(2, 2): nothing for glacier.
        assert_eq!(
            need_at(&conn, &config, &unit, &resolved, "glacier").unwrap(),
            NeedAt::None
        );
    }

    /// The wordings: one target shared ("needs 2 at each"), targets that
    /// differ (each its own "needs N"), and a policy's targets as stated.
    #[test]
    fn the_per_location_wordings() {
        let at = |location: &str, copies: i64, target: i64| LocationShort {
            location: location.to_string(),
            copies,
            target,
        };
        assert_eq!(
            describe_short_locations(&[at("home", 1, 2), at("offsite", 1, 2)]),
            "1 copy at required location home, 1 copy at required location offsite, \
             needs 2 at each"
        );
        assert_eq!(
            describe_short_locations(&[at("home", 2, 3), at("offsite", 0, 1)]),
            "2 copies at required location home, needs 3; 0 copies at required location \
             offsite, needs 1"
        );
        assert_eq!(
            describe_targets(&btree(&[("home", 1), ("offsite", 1)])),
            "home, offsite"
        );
        assert_eq!(
            describe_targets(&btree(&[("garage", 0), ("home", 2), ("offsite", 1)])),
            "2 at home, 1 at offsite"
        );
    }
}
