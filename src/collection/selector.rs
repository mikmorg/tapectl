//! The batch selector (`docs/design/v2-open-questions.md` §11, formalizing
//! the §7 ruling): alphabetical NEXT-fit over pending units — a batch closes
//! at the first unit that does not fit and is never reopened (issue #456;
//! §7 and this module's docstrings called it first-fit, which would go back
//! to earlier batches for a smaller unit).
//!
//! Pure arithmetic over sizes — no filesystem, no DB, no tape — so it can be
//! drilled at full production scale (~600 units) in milliseconds
//! (`docs/design/v2-implementation-plan.md` T10: "drive this test with
//! synthetic size lists, NOT by generating and staging 600 real fixture
//! folders").
//!
//! §7 ruled out best-fit-decreasing: at 2-15 G units vs 2.2 TB bins, any
//! greedy fill wastes ≈ avg_unit/2 ≈ 4 G ≈ 0.2% per tape (worst 0.7%);
//! size-ordered BFD recovers ≤ 0.6% (~0.04 tapes across the fleet) while
//! destroying the name-ordered "tape spine" property (tape 9 = M-P), which
//! has real operational value. The alphabetical order stands, and with it
//! next-fit: reopening an earlier batch for a later, smaller unit would break
//! the same spine. §7's premise (2-15 G units against 2.2 TB bins) is
//! re-examined when a registered unit passes a tenth of its cartridge
//! generation's capacity (ADR-0012 2026-10-15 item 69); `collection plan`
//! prints the largest pending unit's share of the tape so that is visible.
//!
//! **Spanning** (1.7.0, #478; ADR-0012 items 82, 83, 94, 95, 123 and 147;
//! `layout-session.md` "Spanning"). A unit that allows spanning may be cut
//! by the drive's early warning and continue on the next tape, so it is not
//! held to one tape: [`plan_batches_spanning`] admits it at any size up to
//! the staging area, and fills each batch by the per-tape plan order
//! ([`CapacityBudget::place`], never a copy of it): whole units first, the
//! spannable ones last, except that one larger than one tape comes first and
//! the whole units it displaces ride its batch to its last tape, after its
//! tail ([`after_the_tail`]). With no spannable unit pending it is exactly
//! [`plan_batches`] (or [`plan_batches_prioritised`]).

use std::collections::HashSet;

use crate::volume::layout_model::{
    pad_to_blocks, CapacityBudget, Placed, Placement, PlanClass, PlanItem,
};

/// One pending unit as the selector sees it: a name (the sort key) and a
/// size in bytes. Sourcing the size is the caller's concern — see
/// `collection::sizing` for the real (on-disk, plaintext-walk estimate) source;
/// tests drive this struct with synthetic sizes directly, per the plan's
/// instruction not to stage hundreds of real fixtures just to drill the
/// selector's arithmetic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUnit {
    pub name: String,
    pub size_bytes: u64,
}

/// One filled batch: its units, in placement order (name-ascending — the
/// "tape spine"), plus running totals.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Batch {
    pub units: Vec<PendingUnit>,
    /// Sum of raw (unpadded) unit sizes.
    pub total_bytes: u64,
    /// Sum of block-padded on-tape sizes — what was actually checked against
    /// the budget.
    pub padded_bytes: u64,
}

impl Batch {
    pub fn unit_names(&self) -> Vec<&str> {
        self.units.iter().map(|u| u.name.as_str()).collect()
    }

    /// Fraction of `budget_bytes` this batch's padded bytes occupy —
    /// the fill metric the multi-tape drill asserts on closed batches.
    pub fn fill_fraction(&self, budget_bytes: u64) -> f64 {
        if budget_bytes == 0 {
            return 0.0;
        }
        self.padded_bytes as f64 / budget_bytes as f64
    }
}

/// A unit whose block-padded size alone exceeds the per-tape budget, and
/// which may not span on this drive.
///
/// Per the T10 trap ("a unit too large for a whole tape is a clear error,
/// not a split" — §7: "Unit contiguity is inviolate regardless, a movie
/// folder is never split across tapes"), this is a hard error, checked
/// BEFORE the fits-in-current-batch test so an oversized unit can never
/// silently close an empty batch (which would either loop forever trying to
/// place it, or emit a batch that itself overflows the budget).
///
/// Since 1.7.0 (#478) a unit that allows spanning is not held to one tape
/// (ADR-0012 items 82 and 94) — except on a drive with no
/// `post_warning_bytes`, where no unit may pass the fill ceiling and a span
/// is refused (item 123): such a unit is oversized too, `allows_spanning`
/// set, and says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OversizedUnit {
    pub name: String,
    pub size_bytes: u64,
    pub padded_bytes: u64,
    pub budget_bytes: u64,
    /// The unit allows spanning (`allow_spanning`), and is refused only
    /// because the drive does not declare `post_warning_bytes`.
    pub allows_spanning: bool,
}

impl std::fmt::Display for OversizedUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Issue #391: the overage is named, not only the two sides.
        let over = self.padded_bytes.saturating_sub(self.budget_bytes);
        write!(
            f,
            "unit \"{}\" ({} bytes, {} block-padded) exceeds the per-tape budget \
             ({} bytes) by {} bytes ({}) — {}, so this one cannot be batched at all",
            self.name,
            self.size_bytes,
            self.padded_bytes,
            self.budget_bytes,
            over,
            crate::util::format_bytes_binary(i64::try_from(over).unwrap_or(i64::MAX)),
            if self.allows_spanning {
                "it allows spanning (allow_spanning), but a span is written only by a drive \
                 whose `[[backends.lto]]` entry declares `post_warning_bytes` (ADR-0012 item \
                 123), and this one does not"
            } else {
                "a unit that does not allow spanning (allow_spanning) is never split across \
                 tapes"
            },
        )
    }
}

impl std::error::Error for OversizedUnit {}

/// A unit that allows spanning and is larger than the staging area (ADR-0012
/// items 85 and 94): it is staged whole before its first tape is written, so
/// it could never be staged. `stage create` refuses it by the same rule
/// (`staging::check_unit_fits_one_tape`): its size, unpadded, against the
/// staging filesystem's total size (ruling X14), not its free space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargerThanStaging {
    pub name: String,
    pub size_bytes: u64,
    /// The staging filesystem's total size.
    pub staging_total: u64,
}

/// Every unit [`plan_batches_spanning`] refused, collected like
/// `Layout::validate` (no batch is returned while any unit is refused).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Refused {
    /// Units too large for one tape that may not span on it.
    pub oversized: Vec<OversizedUnit>,
    /// Spannable units larger than the staging area.
    pub larger_than_staging: Vec<LargerThanStaging>,
}

/// What one batch is packed against: the room of one tape, as the per-tape
/// plan sees it (ADR-0012 items 113 and 123).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchRoom {
    /// The room under the fill ceiling.
    pub fill: u64,
    /// The plan bound (item 113): with the drive's `post_warning_bytes`, the
    /// room to the cartridge's whole nominal capacity, where whole units may
    /// reach and past which a spannable unit spans. `None`: nothing passes
    /// the fill ceiling, and nothing spans (item 123).
    pub bound: Option<u64>,
}

impl BatchRoom {
    /// The most a batch of whole units is packed to: the bound, or the fill
    /// without one, as 1.6.0 packs.
    pub fn whole_bytes(self) -> u64 {
        self.bound.unwrap_or(self.fill)
    }

    /// This room as the per-tape plan's budget, with no fixed files and no
    /// reserve under the fill ceiling: `fill` is already net of it.
    pub fn budget(self) -> CapacityBudget {
        CapacityBudget {
            available_bytes: self.fill,
            reserve_bytes: 0,
            plan_bound_bytes: self.bound,
            span_tail_from: None,
        }
    }
}

/// The pending units that allow spanning (ADR-0012 item 82: the policy key
/// `allow_spanning`), and the staging area each must fit (items 85 and 94).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spanning {
    /// Their names. Empty: the plan is 1.6.0's.
    pub units: HashSet<String>,
    /// The staging filesystem's total size (ruling X14), as `stage create`
    /// reads it; `None` when it could not be read, and then nothing is
    /// refused for it, as there.
    pub staging_total: Option<u64>,
}

/// Greedily fill batches, alphabetical NEXT-fit (§7 ruling — explicitly
/// NOT best-fit-decreasing, and not first-fit either: issue #456): sort
/// pending units by name, then walk the sorted list, placing each unit into
/// the current batch if it fits (Σ block-padded sizes ≤ `budget_bytes`),
/// else closing the batch for good and starting a new one with that unit. A
/// closed batch is never reopened for a later, smaller unit; that is what
/// keeps each tape a contiguous run of the alphabet (the "tape spine").
///
/// `budget_bytes` is the caller's already-netted `usable − enospc_buffer`;
/// `block_size` is the format constant the caller pads against (512 KiB in
/// both production and the microcosm — §8: block size never scales).
///
/// Returns every unit whose own padded size exceeds `budget_bytes` as an
/// error (collected like `Layout::validate`, not just the first) — no
/// batches are returned at all in that case, since the input as a whole
/// cannot be planned.
pub fn plan_batches(
    mut units: Vec<PendingUnit>,
    budget_bytes: u64,
    block_size: u64,
) -> Result<Vec<Batch>, Vec<OversizedUnit>> {
    units.sort_by(|a, b| a.name.cmp(&b.name));
    next_fit(units, budget_bytes, block_size)
}

/// `--policy-aware` (ADR-0012, 2026-10-07 item 26; issue #144): the same
/// next-fit as [`plan_batches`], over a different order. The units named
/// in `prioritised` — the caller's set of units whose audit findings a
/// write would resolve (too few copies, a required location with no copy:
/// `policy::coverage::shortfall`) — are placed first, in name order, and
/// every other unit after them, in name order.
///
/// This deliberately gives up the §7 "tape spine" across the two groups
/// (tape 9 is no longer simply M-P): the operator who asks for it has
/// said that getting the unprotected units onto tape first matters more
/// than the alphabetical shelf. Within each group the spine holds. Without
/// the flag nothing calls this, and [`plan_batches`]' order is unchanged.
pub fn plan_batches_prioritised(
    mut units: Vec<PendingUnit>,
    prioritised: &HashSet<String>,
    budget_bytes: u64,
    block_size: u64,
) -> Result<Vec<Batch>, Vec<OversizedUnit>> {
    sort_prioritised(&mut units, prioritised);
    next_fit(units, budget_bytes, block_size)
}

/// [`plan_batches_prioritised`]'s order: the prioritised units first, each
/// group in name order.
fn sort_prioritised(units: &mut [PendingUnit], prioritised: &HashSet<String>) {
    units.sort_by(|a, b| {
        (!prioritised.contains(&a.name), &a.name).cmp(&(!prioritised.contains(&b.name), &b.name))
    });
}

/// [`plan_batches`] — or, with `prioritised`, [`plan_batches_prioritised`] —
/// over pending units some of which may allow spanning (1.7.0, #478), packed
/// against `room`. Without a unit of `spanning.units` among `units` it is
/// exactly that function, packed to [`BatchRoom::whole_bytes`], as 1.6.0
/// packs. Otherwise:
///
/// - **Admission** (ADR-0012 items 82, 85, 94 and 123). A unit that does not
///   allow spanning is refused above one tape, as before. A spannable unit
///   is refused only when it is larger than the staging area
///   ([`LargerThanStaging`], `stage create`'s own rule), or when the drive
///   declares no plan bound and it does not fit under the fill ceiling (no
///   unit may span there: [`OversizedUnit::allows_spanning`]).
/// - **The fill** is still next-fit over the same order: a unit joins the
///   current batch while the per-tape plan order ([`CapacityBudget::place`])
///   places every unit of the batch on its tape, and otherwise starts the
///   next. So whole units are committed to the fill ceiling and, with no
///   spannable unit in their batch, past it within the bound (item 113);
///   spannable units go last (items 83 and 95); the first that crosses the
///   bound spans and nothing after it is planned on that tape (item 123).
///   One larger than one tape starts its batch's first tape instead (items
///   94 and 147), and the batch is one write across its tapes: the whole
///   units it displaces stay in the batch while they fit after its tail on
///   its last tape, by X11's estimate ([`after_the_tail`]), and the first
///   that does not starts the next batch.
/// - **Order within a batch**: its whole units in the order taken, then its
///   spannable units in the order taken (the tape's order; with a unit
///   larger than one tape, that unit comes first on the first tape and the
///   whole units after its tail on its last, `collection::plan::placement`).
///
/// A batch's `padded_bytes` counts each spannable unit whole, including what
/// continues on the next tape. Which unit of a batch spans is
/// [`CapacityBudget::place`]'s to say, over the same room; `collection plan`
/// asks it (`collection::plan::placement`).
pub fn plan_batches_spanning(
    mut units: Vec<PendingUnit>,
    prioritised: Option<&HashSet<String>>,
    spanning: &Spanning,
    room: BatchRoom,
    block_size: u64,
) -> Result<Vec<Batch>, Refused> {
    match prioritised {
        Some(first) => sort_prioritised(&mut units, first),
        None => units.sort_by(|a, b| a.name.cmp(&b.name)),
    }
    if !units.iter().any(|u| spanning.units.contains(&u.name)) {
        return next_fit(units, room.whole_bytes(), block_size).map_err(|oversized| Refused {
            oversized,
            larger_than_staging: Vec::new(),
        });
    }

    let mut refused = Refused::default();
    for u in &units {
        let padded = pad_to_blocks(u.size_bytes, block_size);
        let allows_spanning = spanning.units.contains(&u.name);
        // Item 123: with no plan bound nothing passes the fill ceiling, a
        // spannable unit included.
        let budget_bytes = match (allows_spanning, room.bound) {
            (true, Some(_)) => u64::MAX,
            (true, None) => room.fill,
            (false, _) => room.whole_bytes(),
        };
        if padded > budget_bytes {
            refused.oversized.push(OversizedUnit {
                name: u.name.clone(),
                size_bytes: u.size_bytes,
                padded_bytes: padded,
                budget_bytes,
                allows_spanning,
            });
        }
        // Items 85 and 94, `stage create`'s rule: the size against the
        // staging filesystem's total size.
        if let Some(total) = spanning.staging_total.filter(|_| allows_spanning) {
            if u.size_bytes > total {
                refused.larger_than_staging.push(LargerThanStaging {
                    name: u.name.clone(),
                    size_bytes: u.size_bytes,
                    staging_total: total,
                });
            }
        }
    }
    if refused != Refused::default() {
        return Err(refused);
    }

    let budget = room.budget();
    let mut batches: Vec<Batch> = Vec::new();
    let mut current: Vec<(PendingUnit, PlanItem)> = Vec::new();
    for u in units {
        let item = PlanItem {
            class: if spanning.units.contains(&u.name) {
                PlanClass::Spannable
            } else {
                PlanClass::Whole
            },
            bytes: pad_to_blocks(u.size_bytes, block_size),
        };
        // The admission above guarantees one unit alone is always placed, so
        // an empty batch always takes the next unit.
        if !current.is_empty() && !every_unit_placed(&budget, &current, item) {
            batches.push(batch_in_tape_order(std::mem::take(&mut current)));
        }
        current.push((u, item));
    }
    if !current.is_empty() {
        batches.push(batch_in_tape_order(current));
    }
    Ok(batches)
}

/// Whether the per-tape plan order places every unit of `current` and
/// `next` in one batch: on its tape, none deferred to a later tape and none
/// refused, or, when a unit larger than one tape spans, every whole unit it
/// displaces after its tail on its last tape ([`after_the_tail`]).
fn every_unit_placed(
    budget: &CapacityBudget,
    current: &[(PendingUnit, PlanItem)],
    next: PlanItem,
) -> bool {
    let items: Vec<PlanItem> = current
        .iter()
        .map(|(_, item)| *item)
        .chain(std::iter::once(next))
        .collect();
    let first = budget.place(0, &items);
    all_placed(&first) || after_the_tail(budget, &items, &first).is_some()
}

/// Nothing of the plan waits for a later tape or is refused.
fn all_placed(placement: &Placement) -> bool {
    placement
        .placed
        .iter()
        .all(|p| !matches!(p, Placed::Deferred | Placed::Refused))
}

/// ADR-0012 item 147 in a batch, one write across the tapes of a span. When
/// the batch's first tape (`first`, [`CapacityBudget::place`] of `items`
/// against `budget` with no fixed files) is spanned by a unit larger than
/// one tape, the whole units it displaces are written after its tail on its
/// last tape: they belong to the batch while they all fit there. That tape
/// is the per-tape plan order of the span's last run by X11's estimate (the
/// bound per tape, [`CapacityBudget::span_last_run`]) as its owed run, then
/// those whole units in the batch's order, within item 113's bound (item
/// 123). `Some` with each one's index and where it sits there (under the
/// fill ceiling or past it) when every one fits and nothing else of the
/// batch waits or is refused; `None` otherwise, and for any other `first`.
/// A unit deferred for another reason (one queued after the tail, X13) or
/// refused (X12) closes the batch exactly as before.
pub fn after_the_tail(
    budget: &CapacityBudget,
    items: &[PlanItem],
    first: &Placement,
) -> Option<Vec<(usize, Placed)>> {
    let run = budget.span_last_run(0, items, first)?;
    let mut displaced = Vec::new();
    for (i, p) in first.placed.iter().enumerate() {
        match (items[i].class, p) {
            (PlanClass::Whole, Placed::Deferred) => displaced.push(i),
            (_, Placed::Deferred | Placed::Refused) => return None,
            _ => {}
        }
    }
    let last_tape: Vec<PlanItem> = std::iter::once(PlanItem {
        class: PlanClass::Owed,
        bytes: run,
    })
    .chain(displaced.iter().map(|&i| items[i]))
    .collect();
    let last = budget.place(0, &last_tape);
    all_placed(&last).then(|| {
        displaced
            .into_iter()
            .zip(last.placed[1..].iter().copied())
            .collect()
    })
}

/// One batch of `units`, in its tape's order: the whole units, then the
/// spannable ones (items 83 and 95), each group in the order taken. A unit
/// larger than one tape starts the tape instead, and the whole units go
/// after its tail on its last tape (item 147); the batch keeps this order and
/// `collection::plan::placement` says where each sits.
fn batch_in_tape_order(units: Vec<(PendingUnit, PlanItem)>) -> Batch {
    let (whole, spannable): (Vec<_>, Vec<_>) = units
        .into_iter()
        .partition(|(_, item)| item.class == PlanClass::Whole);
    let mut batch = Batch::default();
    for (u, item) in whole.into_iter().chain(spannable) {
        batch.total_bytes += u.size_bytes;
        batch.padded_bytes += item.bytes;
        batch.units.push(u);
    }
    batch
}

/// The oversized refusal and the next-fit fill, over `units` in the order
/// given — the one implementation both orders share.
fn next_fit(
    units: Vec<PendingUnit>,
    budget_bytes: u64,
    block_size: u64,
) -> Result<Vec<Batch>, Vec<OversizedUnit>> {
    let oversized: Vec<OversizedUnit> = units
        .iter()
        .filter_map(|u| {
            let padded = pad_to_blocks(u.size_bytes, block_size);
            (padded > budget_bytes).then(|| OversizedUnit {
                name: u.name.clone(),
                size_bytes: u.size_bytes,
                padded_bytes: padded,
                budget_bytes,
                allows_spanning: false,
            })
        })
        .collect();
    if !oversized.is_empty() {
        return Err(oversized);
    }

    let mut batches: Vec<Batch> = Vec::new();
    let mut current = Batch::default();

    for u in units {
        let padded = pad_to_blocks(u.size_bytes, block_size);
        // The oversized check above guarantees `padded <= budget_bytes` for
        // every unit, so an empty `current` always accepts the next unit —
        // this guard exists so that guarantee is never load-bearing for
        // correctness, only for efficiency (never closes an empty batch).
        if !current.units.is_empty() && current.padded_bytes + padded > budget_bytes {
            batches.push(std::mem::take(&mut current));
        }
        current.total_bytes += u.size_bytes;
        current.padded_bytes += padded;
        current.units.push(u);
    }
    if !current.units.is_empty() {
        batches.push(current);
    }

    Ok(batches)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(name: &str, size_bytes: u64) -> PendingUnit {
        PendingUnit {
            name: name.to_string(),
            size_bytes,
        }
    }

    /// Hand-verifiable small case, block_size = 1 (no padding distortion) so
    /// the arithmetic is exact: sizes [3,4,3,5,2] for names [a,b,c,d,e],
    /// budget 10. Next-fit: a(3)->3, b(4)->7, c(3)->10 (exactly full),
    /// d(5) doesn't fit (10+5>10) -> close [a,b,c]=10, start d(5)->5,
    /// e(2)->7 (fits) -> final batch [d,e]=7.
    #[test]
    fn greedy_next_fit_matches_hand_computed_batches() {
        let units = vec![
            unit("a", 3),
            unit("b", 4),
            unit("c", 3),
            unit("d", 5),
            unit("e", 2),
        ];
        let batches = plan_batches(units, 10, 1).unwrap();
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].unit_names(), vec!["a", "b", "c"]);
        assert_eq!(batches[0].padded_bytes, 10);
        assert_eq!(batches[1].unit_names(), vec!["d", "e"]);
        assert_eq!(batches[1].padded_bytes, 7);
    }

    /// Issue #456: the selector is NEXT-fit. First-fit would send `c(3)` back
    /// into the first batch's spare 4 and give [a,c] [b]; next-fit closes
    /// [a] at `b` for good and gives [a] [b,c] — the tape spine (each tape a
    /// contiguous run of the alphabet) that §7 ruled for.
    #[test]
    fn a_closed_batch_is_never_reopened_for_a_later_smaller_unit() {
        let units = vec![unit("a", 6), unit("b", 6), unit("c", 3)];
        let batches = plan_batches(units, 10, 1).unwrap();
        assert_eq!(
            batches.iter().map(|b| b.unit_names()).collect::<Vec<_>>(),
            vec![vec!["a"], vec!["b", "c"]],
        );
    }

    #[test]
    fn input_order_does_not_matter_only_name_does() {
        // Deliberately reverse-alphabetical input order.
        let units = vec![unit("c", 3), unit("a", 3), unit("b", 4)];
        let batches = plan_batches(units, 10, 1).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0].unit_names(),
            vec!["a", "b", "c"],
            "selector must sort by name itself, not trust caller order"
        );
    }

    #[test]
    fn block_padding_rounds_each_unit_up_before_summing() {
        // block_size 512: a size-1 unit costs a full block.
        let units = vec![unit("a", 1), unit("b", 1)];
        let batches = plan_batches(units, 1024, 512).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].padded_bytes, 1024, "two padded-to-512 units");
        assert_eq!(batches[0].total_bytes, 2, "raw total stays unpadded");
    }

    #[test]
    fn a_unit_larger_than_the_whole_budget_is_a_hard_error_not_a_split() {
        let units = vec![unit("huge", 100), unit("small", 5)];
        let err = plan_batches(units, 50, 1).unwrap_err();
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].name, "huge");
        assert_eq!(err[0].size_bytes, 100);
        assert_eq!(err[0].budget_bytes, 50);
    }

    #[test]
    fn multiple_oversized_units_are_all_collected_not_just_the_first() {
        let units = vec![unit("huge1", 100), unit("ok", 5), unit("huge2", 200)];
        let err = plan_batches(units, 50, 1).unwrap_err();
        let names: Vec<&str> = err.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["huge1", "huge2"],
            "both oversized, sorted by name"
        );
    }

    #[test]
    fn an_oversized_unit_never_closes_an_empty_batch_or_appears_inside_one() {
        // Regression guard for the exact bug class the T10 trap warns about:
        // if the oversized check ran only against "does it fit the *current*
        // batch" rather than "does it fit *any* batch", a first (empty)
        // batch could close on it, or it could be silently placed alone in
        // an overflowing batch. Assert neither happens: the whole call
        // refuses.
        let units = vec![unit("only", 999)];
        let result = plan_batches(units, 10, 1);
        assert!(result.is_err());
    }

    /// Issue #144 (ADR-0012 2026-10-07 item 26): the prioritised units are
    /// placed first, each group in name order, and next-fit runs over that
    /// order — so the prioritised units fill the first batch.
    #[test]
    fn prioritised_units_go_first_each_group_in_name_order() {
        let units = vec![
            unit("a", 4),
            unit("b", 4),
            unit("c", 4),
            unit("d", 4),
            unit("e", 4),
        ];
        let first: std::collections::HashSet<String> =
            ["d".to_string(), "b".to_string()].into_iter().collect();
        let batches = plan_batches_prioritised(units.clone(), &first, 10, 1).unwrap();
        assert_eq!(
            batches.iter().map(|b| b.unit_names()).collect::<Vec<_>>(),
            vec![vec!["b", "d"], vec!["a", "c"], vec!["e"]],
        );
        // Nothing prioritised: exactly the alphabetical plan.
        assert_eq!(
            plan_batches_prioritised(units.clone(), &Default::default(), 10, 1).unwrap(),
            plan_batches(units, 10, 1).unwrap(),
        );
    }

    #[test]
    fn empty_input_yields_no_batches() {
        assert_eq!(plan_batches(vec![], 1000, 512).unwrap(), Vec::new());
    }

    // ── spanning (1.7.0, #478; ADR-0012 items 82, 83, 94, 95 and 123) ──

    /// A fill ceiling at 8 and a plan bound at 10, block size 1.
    const ROOM: BatchRoom = BatchRoom {
        fill: 8,
        bound: Some(10),
    };

    fn spanning(names: &[&str], staging_total: Option<u64>) -> Spanning {
        Spanning {
            units: names.iter().map(|n| n.to_string()).collect(),
            staging_total,
        }
    }

    fn names_of(batches: &[Batch]) -> Vec<Vec<&str>> {
        batches.iter().map(|b| b.unit_names()).collect()
    }

    /// Item 82: a unit that does not allow spanning is still refused above
    /// one tape, and the refusal names the key.
    #[test]
    fn a_unit_that_does_not_allow_spanning_is_refused_above_one_tape_naming_the_key() {
        let err = plan_batches(vec![unit("huge", 100)], 50, 1).unwrap_err();
        assert!(!err[0].allows_spanning);
        let text = err[0].to_string();
        assert!(
            text.contains(
                "a unit that does not allow spanning (allow_spanning) is never split across tapes"
            ),
            "{text}"
        );
        // The same through the spanning selector, beside a unit that does.
        let err = plan_batches_spanning(
            vec![unit("huge", 100), unit("span", 30)],
            None,
            &spanning(&["span"], None),
            ROOM,
            1,
        )
        .unwrap_err();
        assert_eq!(err.oversized.len(), 1, "{err:?}");
        assert_eq!(
            (
                err.oversized[0].name.as_str(),
                err.oversized[0].budget_bytes
            ),
            ("huge", 10),
            "a whole unit is held to the bound"
        );
        assert!(err.larger_than_staging.is_empty());
    }

    /// Where the per-tape plan order puts `items` (sizes, `true` for a unit
    /// that allows spanning) against [`ROOM`]: the first tape, and the whole
    /// units that go after a span's tail on its last tape.
    fn first_and_after_the_tail(
        items: &[(u64, bool)],
    ) -> (Vec<Placed>, Option<Vec<(usize, Placed)>>) {
        let items: Vec<PlanItem> = items
            .iter()
            .map(|&(bytes, span)| PlanItem {
                class: if span {
                    PlanClass::Spannable
                } else {
                    PlanClass::Whole
                },
                bytes,
            })
            .collect();
        let budget = ROOM.budget();
        let first = budget.place(0, &items);
        let after = after_the_tail(&budget, &items, &first);
        (first.placed, after)
    }

    /// Items 94 and 147: a spannable unit larger than one tape is admitted,
    /// and its batch is one write across its tapes. It starts the first
    /// tape, and the whole units it displaces ride the batch to its last
    /// tape: by X11's estimate (10 per tape) that tape holds the last 5 of
    /// `m`, and `a` and `z` (5 together) fit after that tail within the
    /// bound, `z` past the fill ceiling of 8. So `z`, after `m` in the
    /// alphabet, joins the batch too, and the batch keeps its class order.
    /// Moved and moved back by item 147 (#492): 1.7.0 put `a` and `z`
    /// before `m` on its first tape, its late start leaving them 5 there.
    #[test]
    fn a_spannable_unit_larger_than_one_tape_is_admitted_and_its_whole_units_ride_its_batch() {
        use Placed::{Committed, Deferred, PastCeiling, Tail};
        let units = vec![unit("a", 3), unit("m", 25), unit("z", 2)];
        let batches = plan_batches_spanning(units, None, &spanning(&["m"], None), ROOM, 1).unwrap();
        assert_eq!(names_of(&batches), [vec!["a", "z", "m"]]);
        assert_eq!(
            batches[0].padded_bytes, 30,
            "the spanning unit is counted whole"
        );
        let (first, after) = first_and_after_the_tail(&[(3, false), (2, false), (25, true)]);
        assert_eq!(
            first,
            [Deferred, Deferred, Tail],
            "m alone on its first tape"
        );
        assert_eq!(after, Some(vec![(0, Committed), (1, PastCeiling)]));
    }

    /// Item 147: a whole unit that does not fit after the tail on the span's
    /// last tape starts the next batch. `a` (3) after the tail of 5 ends at
    /// 8; `z` (4) would end at 12, past the bound of 10.
    #[test]
    fn a_whole_unit_that_does_not_fit_after_the_tail_starts_the_next_batch() {
        let units = vec![unit("a", 3), unit("m", 25), unit("z", 4)];
        let batches = plan_batches_spanning(units, None, &spanning(&["m"], None), ROOM, 1).unwrap();
        assert_eq!(names_of(&batches), [vec!["a", "m"], vec!["z"]]);
        let (_, after) = first_and_after_the_tail(&[(3, false), (4, false), (25, true)]);
        assert_eq!(after, None);
    }

    /// Item 147 with next-fit: the whole units ride a span's batch, in name
    /// order, until the first that does not fit after its tail, which closes
    /// the batch for good. After `m`'s tail of 5, `a` ends at 7 and `n` at 9
    /// (past the fill ceiling of 8, within the bound); `p` would end at 11,
    /// so it starts the next batch, and `q`, which would have fitted (10),
    /// goes with it: a closed batch is never reopened. Positive control:
    /// without `p`, `q` rides too.
    #[test]
    fn a_spans_batch_closes_at_the_first_whole_unit_not_fitting_after_its_tail() {
        let span = spanning(&["m"], None);
        let units = vec![
            unit("a", 2),
            unit("m", 25),
            unit("n", 2),
            unit("p", 2),
            unit("q", 1),
        ];
        let batches = plan_batches_spanning(units.clone(), None, &span, ROOM, 1).unwrap();
        assert_eq!(names_of(&batches), [vec!["a", "n", "m"], vec!["p", "q"]]);

        let without_p: Vec<PendingUnit> = units.into_iter().filter(|u| u.name != "p").collect();
        let batches = plan_batches_spanning(without_p, None, &span, ROOM, 1).unwrap();
        assert_eq!(names_of(&batches), [vec!["a", "n", "q", "m"]]);
        let (_, after) =
            first_and_after_the_tail(&[(2, false), (2, false), (1, false), (25, true)]);
        assert_eq!(
            after,
            Some(vec![
                (0, Placed::Committed),
                (1, Placed::PastCeiling),
                (2, Placed::PastCeiling)
            ])
        );
    }

    /// Items 83 and 95: a spannable unit that fits one tape goes last in its
    /// batch, after a whole unit later in the alphabet, and passes the fill
    /// ceiling (a, b and n take 6 of the 8 under it; m takes the tape to 9),
    /// where whole units, with it queued, stop at the ceiling.
    #[test]
    fn a_spannable_unit_that_fits_one_tape_goes_after_the_whole_units() {
        let units = vec![unit("a", 2), unit("b", 2), unit("m", 3), unit("n", 2)];
        let batches = plan_batches_spanning(units, None, &spanning(&["m"], None), ROOM, 1).unwrap();
        assert_eq!(names_of(&batches), [vec!["a", "b", "n", "m"]]);
    }

    /// With a spannable unit queued, whole units stop at the fill ceiling
    /// (item 83): a batch of whole units already past it does not take the
    /// spannable unit, which starts the next batch, and the whole units after
    /// it join it there.
    #[test]
    fn whole_units_past_the_ceiling_keep_their_batch_and_the_span_starts_the_next() {
        let units = vec![
            unit("a", 4),
            unit("b", 3),
            unit("c", 3),
            unit("m", 3),
            unit("n", 4),
        ];
        let batches = plan_batches_spanning(units, None, &spanning(&["m"], None), ROOM, 1).unwrap();
        assert_eq!(names_of(&batches), [vec!["a", "b", "c"], vec!["n", "m"]]);
    }

    /// Item 123 (X13): nothing is planned after the unit that spans; the
    /// next spannable unit starts the next batch.
    #[test]
    fn nothing_follows_the_span_on_its_tape() {
        let units = vec![unit("m", 12), unit("n", 2)];
        let batches =
            plan_batches_spanning(units, None, &spanning(&["m", "n"], None), ROOM, 1).unwrap();
        assert_eq!(names_of(&batches), [vec!["m"], vec!["n"]]);
    }

    /// Item 123: on a drive with no plan bound nothing passes the fill
    /// ceiling. A spannable unit over it is refused, naming
    /// `post_warning_bytes`, not `allow_spanning`; one under it is packed
    /// like a whole unit and goes last in its batch.
    #[test]
    fn without_a_plan_bound_a_spannable_unit_over_the_fill_is_refused_naming_the_drive_key() {
        let no_bound = BatchRoom {
            fill: 8,
            bound: None,
        };
        let err = plan_batches_spanning(
            vec![unit("a", 1), unit("m", 9)],
            None,
            &spanning(&["m"], None),
            no_bound,
            1,
        )
        .unwrap_err();
        assert_eq!(err.oversized.len(), 1, "{err:?}");
        let o = &err.oversized[0];
        assert!(o.allows_spanning && o.budget_bytes == 8, "{o:?}");
        let text = o.to_string();
        assert!(
            text.contains("it allows spanning (allow_spanning)")
                && text.contains("`post_warning_bytes` (ADR-0012 item 123)"),
            "{text}"
        );

        let batches = plan_batches_spanning(
            vec![unit("a", 1), unit("m", 4), unit("n", 3), unit("z", 2)],
            None,
            &spanning(&["m"], None),
            no_bound,
            1,
        )
        .unwrap();
        assert_eq!(names_of(&batches), [vec!["a", "n", "m"], vec!["z"]]);
    }

    /// Items 85 and 94: a spannable unit larger than the staging area is
    /// refused, its size unpadded against the staging filesystem's total, as
    /// `stage create` holds it. Equal is not larger; an unreadable total
    /// refuses nothing.
    #[test]
    fn a_spannable_unit_larger_than_the_staging_area_is_refused() {
        let units = || vec![unit("a", 1), unit("m", 25)];
        let err =
            plan_batches_spanning(units(), None, &spanning(&["m"], Some(24)), ROOM, 1).unwrap_err();
        assert_eq!(
            err.larger_than_staging,
            [LargerThanStaging {
                name: "m".into(),
                size_bytes: 25,
                staging_total: 24,
            }]
        );
        assert!(err.oversized.is_empty(), "a span is not held to one tape");
        for total in [Some(25), None] {
            assert!(
                plan_batches_spanning(units(), None, &spanning(&["m"], total), ROOM, 1).is_ok(),
                "{total:?}"
            );
        }
    }

    /// With no spannable unit pending, the spanning selector IS next-fit at
    /// the bound (or the fill without one), in either order — the 600-unit
    /// drill's units, a set naming only units that are not pending, and the
    /// hand-computed case.
    #[test]
    fn with_no_spannable_unit_pending_the_plan_is_1_6_0s() {
        let drill: Vec<PendingUnit> = (0..600u64)
            .map(|i| unit(&format!("{i:04}"), 1 + (i * 7919) % 8))
            .collect();
        let first: HashSet<String> = ["0007".to_string(), "0300".to_string()].into();
        let not_pending = spanning(&["absent"], Some(1));
        for room in [
            ROOM,
            BatchRoom {
                fill: 8,
                bound: None,
            },
        ] {
            for units in [
                drill.clone(),
                vec![unit("a", 3), unit("b", 4), unit("c", 3)],
            ] {
                assert_eq!(
                    plan_batches_spanning(units.clone(), None, &not_pending, room, 1).unwrap(),
                    plan_batches(units.clone(), room.whole_bytes(), 1).unwrap(),
                );
                assert_eq!(
                    plan_batches_spanning(units.clone(), Some(&first), &not_pending, room, 1)
                        .unwrap(),
                    plan_batches_prioritised(units, &first, room.whole_bytes(), 1).unwrap(),
                );
            }
        }
    }

    /// The multi-tape drill (T10 §4): ~600 units, microcosm sizes, asserting
    /// the "tape spine" (global name order preserved across the batch
    /// concatenation) and the fill guarantee. Synthetic sizes only — no
    /// filesystem, no staging, per the plan's explicit instruction.
    #[test]
    fn multi_tape_drill_600_units_name_ordered_spine_and_high_fill() {
        // Microcosm parameters (docs/design/v2-open-questions.md §8):
        // nominal 2400M tape, 0.92 usable factor, 8M ENOSPC buffer (NOT
        // ÷1024 — a few 512K blocks), 512K block size (format constant,
        // never scales). Sizes drawn 2-15M like `tests/common::generate_collection`,
        // but generated in-memory here — no directories, no bytes on disk.
        const MIB: u64 = 1024 * 1024;
        const BLOCK: u64 = 512 * 1024;
        let nominal = 2400 * MIB;
        let usable = (nominal as f64 * 0.92) as u64;
        let enospc_buffer = 8 * MIB;
        let budget = usable - enospc_buffer;

        let n_units = 600usize;
        let units: Vec<PendingUnit> = (0..n_units)
            .map(|i| {
                // Same derivation shape as tests/common::generate_collection's
                // compute_unit_size (sha256(seed||i) folded into [2M,15M)),
                // reimplemented locally so this test has zero dependency on
                // any filesystem fixture.
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(42u64.to_le_bytes());
                hasher.update((i as u64).to_le_bytes());
                let digest = hasher.finalize();
                let raw = u64::from_le_bytes([
                    digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6],
                    digest[7],
                ]);
                let size = 2 * MIB + (raw % (13 * MIB));
                PendingUnit {
                    name: format!("{i:04}_media_unit"),
                    size_bytes: size,
                }
            })
            .collect();

        let max_padded = units
            .iter()
            .map(|u| pad_to_blocks(u.size_bytes, BLOCK))
            .max()
            .unwrap();

        let mut sorted_names: Vec<String> = units.iter().map(|u| u.name.clone()).collect();
        sorted_names.sort();

        let batches = plan_batches(units, budget, BLOCK).unwrap();

        assert!(
            batches.len() >= 2,
            "600 units at microcosm scale must need 2+ tapes, got {}",
            batches.len()
        );

        // Tape-spine property: concatenating unit names across batches, in
        // order, reproduces the full sorted name list exactly — batch 1 is
        // wholly before batch 2, etc.
        let concatenated: Vec<String> = batches
            .iter()
            .flat_map(|b| b.units.iter().map(|u| u.name.clone()))
            .collect();
        assert_eq!(
            concatenated, sorted_names,
            "name order must be preserved across the whole batch sequence (the tape spine)"
        );

        // Fill guarantee: for any closed (non-final) batch, greedy next-fit
        // proves fill > 1 - max_padded/budget algebraically (the unit that
        // triggered the close didn't fit in the remaining space, and no
        // unit's padded size exceeds max_padded) — independent of the
        // specific size distribution. Assert both the derived tight bound
        // and the spec's concrete "~99%+" figure (T10 §4 / sheet §8).
        let tight_bound = 1.0 - (max_padded as f64 / budget as f64);
        let (last, closed) = batches.split_last().unwrap();
        for (idx, b) in closed.iter().enumerate() {
            let fill = b.fill_fraction(budget);
            assert!(
                fill > tight_bound,
                "batch {idx}: fill {fill:.5} must exceed the algebraic bound {tight_bound:.5}"
            );
            assert!(
                fill >= 0.99,
                "batch {idx}: fill {fill:.5} must be >= 99% net of block padding, per sheet §8"
            );
            assert!(
                b.padded_bytes <= budget,
                "batch {idx} must never exceed the budget"
            );
        }
        // The final batch is whatever's left — no fill requirement (it's
        // the tail, not a closed bin), just must not overflow.
        assert!(last.padded_bytes <= budget);
    }
}
