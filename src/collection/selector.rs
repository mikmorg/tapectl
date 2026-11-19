//! The batch selector (`docs/design/v2-open-questions.md` §11, formalizing
//! the §7 ruling): alphabetical NEXT-fit over pending units. A batch is the
//! units one tape's plan takes (ADR-0012 items 288 and 289, D19); it closes
//! after the unit that carries the plan past its bound and is never
//! reopened (issue #456; §7 and this module's docstrings called it
//! first-fit, which would go back to earlier batches for a smaller unit).
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
//! **Every unit continues on the next cartridge at the early warning** (ADR-0012
//! items 288 and 289, D19; `layout-session.md` "The plan order"). So no unit
//! is held to one tape, and none is refused for its size against it: units
//! are added to a batch while the plan's end is at or below the bound, and
//! the unit that carries it past is the batch's last (the bound is the
//! cartridge's nominal capacity on a drive that declares `post_warning_bytes`,
//! and the fill ceiling on one that does not). The one limit is the staging
//! area (item 85): a unit is staged whole before its first tape is written.
//! The fill is [`CapacityBudget::place`], never a copy of it. What the
//! selector reads are sizes estimated before staging, and a run takes more
//! units than the estimate says when the staged ones pack smaller
//! (`collection::batch::execute_batch_rounds`, item 289).

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
    /// Sum of block-padded on-tape sizes — what was checked against the
    /// plan's bound. The last unit of a batch may carry it past the bound.
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

/// A unit larger than the staging area (ADR-0012 item 85): it is staged
/// whole before its first tape is written, so it could never be staged.
/// `stage create` refuses it by the same rule
/// (`staging::check_unit_fits_staging`): its size, unpadded, against the
/// staging filesystem's total size (ruling X14), not its free space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargerThanStaging {
    pub name: String,
    pub size_bytes: u64,
    /// The staging filesystem's total size.
    pub staging_total: u64,
}

/// What one batch is packed against: the room of one tape, as the per-tape
/// plan sees it (ADR-0012 items 113 and 289).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchRoom {
    /// The room under the fill ceiling.
    pub fill: u64,
    /// The plan bound (item 113): with the drive's `post_warning_bytes`, the
    /// room to the cartridge's whole nominal capacity, where the plan may
    /// reach and the unit that crosses it is the tail. `None`: the plan stops
    /// at the fill ceiling.
    pub bound: Option<u64>,
}

impl BatchRoom {
    /// What a batch is packed to: the bound, or the fill without one (D19).
    pub fn limit_bytes(self) -> u64 {
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

/// Fill batches, alphabetical NEXT-fit (§7 ruling — explicitly NOT
/// best-fit-decreasing, and not first-fit either: issue #456), over pending
/// units sorted by name — or, with `prioritised`, [`plan_batches`] over a
/// different order (ADR-0012 2026-10-07 item 26, issue #144): the units named
/// in `prioritised` — the caller's set of units whose audit findings a write
/// would resolve (too few copies, a required location with no copy:
/// `policy::coverage::shortfall`) — are placed first, in name order, and
/// every other unit after them, in name order. This deliberately gives up the
/// §7 "tape spine" across the two groups; within each group the spine holds.
///
/// A unit joins the current batch while the per-tape plan order
/// ([`CapacityBudget::place`]) places every unit of the batch on its tape,
/// and otherwise starts the next: the batch closes after the unit that
/// carries the plan past the bound (D19), and a closed batch is never
/// reopened for a later, smaller unit; that is what keeps each tape a
/// contiguous run of the alphabet (the "tape spine"). A batch's
/// `padded_bytes` counts each unit whole, including what continues on the
/// next tape.
///
/// Returns every unit larger than `staging_total` (when it is known) as an
/// error, collected like `Layout::validate`: no batch is returned in that
/// case, since the input as a whole cannot be planned (items 85 and 288).
pub fn plan_batches(
    mut units: Vec<PendingUnit>,
    prioritised: Option<&HashSet<String>>,
    staging_total: Option<u64>,
    room: BatchRoom,
    block_size: u64,
) -> Result<Vec<Batch>, Vec<LargerThanStaging>> {
    match prioritised {
        Some(first) => units.sort_by(|a, b| {
            (!first.contains(&a.name), &a.name).cmp(&(!first.contains(&b.name), &b.name))
        }),
        None => units.sort_by(|a, b| a.name.cmp(&b.name)),
    }
    if let Some(total) = staging_total {
        let larger: Vec<LargerThanStaging> = units
            .iter()
            .filter(|u| u.size_bytes > total)
            .map(|u| LargerThanStaging {
                name: u.name.clone(),
                size_bytes: u.size_bytes,
                staging_total: total,
            })
            .collect();
        if !larger.is_empty() {
            return Err(larger);
        }
    }

    let budget = room.budget();
    let mut batches: Vec<Batch> = Vec::new();
    let mut current: Vec<(PendingUnit, PlanItem)> = Vec::new();
    for u in units {
        let item = PlanItem {
            class: PlanClass::Queued,
            bytes: pad_to_blocks(u.size_bytes, block_size),
        };
        // One unit alone is always placed, so an empty batch always takes
        // the next unit.
        if !current.is_empty() && !every_unit_placed(&budget, &current, item) {
            batches.push(batch_of(std::mem::take(&mut current)));
        }
        current.push((u, item));
    }
    if !current.is_empty() {
        batches.push(batch_of(current));
    }
    Ok(batches)
}

/// Whether the per-tape plan order places every unit of `current` and
/// `next` in one batch: on its tape, none deferred to a later tape (the one
/// that carries the plan past the bound is the tail, and nothing may follow
/// it).
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
    all_placed(&budget.place(0, &items))
}

/// Nothing of the plan waits for a later tape.
fn all_placed(placement: &Placement) -> bool {
    placement.placed.iter().all(|p| *p != Placed::Deferred)
}

/// One batch of `units`, in the order taken.
fn batch_of(units: Vec<(PendingUnit, PlanItem)>) -> Batch {
    let mut batch = Batch::default();
    for (u, item) in units {
        batch.total_bytes += u.size_bytes;
        batch.padded_bytes += item.bytes;
        batch.units.push(u);
    }
    batch
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

    /// A room with no plan bound: the plan stops at the fill ceiling.
    fn fill(fill: u64) -> BatchRoom {
        BatchRoom { fill, bound: None }
    }

    fn names_of(batches: &[Batch]) -> Vec<Vec<&str>> {
        batches.iter().map(|b| b.unit_names()).collect()
    }

    /// Hand-verifiable small case, block_size = 1 (no padding distortion) so
    /// the arithmetic is exact: sizes [3,4,3,5,2] for names [a,b,c,d,e],
    /// room 10 and no bound. Next-fit: a(3)->3, b(4)->7, c(3)->10 (exactly
    /// full), d(5) carries the plan past the ceiling (15 > 10): it is the
    /// batch's last (D19) -> close [a,b,c,d]=15, start e(2) -> [e]=2.
    #[test]
    fn next_fit_closes_a_batch_after_the_unit_that_carries_the_plan_past_the_ceiling() {
        let units = vec![
            unit("a", 3),
            unit("b", 4),
            unit("c", 3),
            unit("d", 5),
            unit("e", 2),
        ];
        let batches = plan_batches(units, None, None, fill(10), 1).unwrap();
        assert_eq!(names_of(&batches), vec![vec!["a", "b", "c", "d"], vec!["e"]]);
        assert_eq!(batches[0].padded_bytes, 15);
        assert_eq!(batches[1].padded_bytes, 2);
    }

    /// A batch that fills the ceiling exactly is not yet past it: the next
    /// unit is the one that carries the plan past, and joins it.
    #[test]
    fn a_unit_ending_exactly_at_the_ceiling_does_not_close_its_batch() {
        let units = vec![unit("a", 5), unit("b", 5), unit("c", 1), unit("d", 1)];
        let batches = plan_batches(units, None, None, fill(10), 1).unwrap();
        assert_eq!(names_of(&batches), vec![vec!["a", "b", "c"], vec!["d"]]);
    }

    /// Issue #456: the selector is NEXT-fit. A closed batch is never
    /// reopened for a later, smaller unit.
    #[test]
    fn a_closed_batch_is_never_reopened_for_a_later_smaller_unit() {
        let units = vec![unit("a", 6), unit("b", 6), unit("c", 3), unit("d", 1)];
        let batches = plan_batches(units, None, None, fill(10), 1).unwrap();
        assert_eq!(names_of(&batches), vec![vec!["a", "b"], vec!["c", "d"]]);
    }

    #[test]
    fn input_order_does_not_matter_only_name_does() {
        // Deliberately reverse-alphabetical input order.
        let units = vec![unit("c", 3), unit("a", 3), unit("b", 4)];
        let batches = plan_batches(units, None, None, fill(10), 1).unwrap();
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
        let batches = plan_batches(units, None, None, fill(1024), 512).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].padded_bytes, 1024, "two padded-to-512 units");
        assert_eq!(batches[0].total_bytes, 2, "raw total stays unpadded");
    }

    /// Item 288: a unit larger than the whole tape is not refused. It
    /// carries the plan past the bound, so it is its batch's last unit, and
    /// the units after it start the next batch.
    #[test]
    fn a_unit_larger_than_a_tape_is_admitted_and_ends_its_batch() {
        let units = vec![unit("a", 3), unit("huge", 100), unit("small", 5)];
        let batches = plan_batches(units, None, None, fill(50), 1).unwrap();
        assert_eq!(names_of(&batches), vec![vec!["a", "huge"], vec!["small"]]);
        assert_eq!(batches[0].padded_bytes, 103);
    }

    /// Item 85: the one refusal left is the staging area, and every unit
    /// above it is named, not only the first.
    #[test]
    fn units_larger_than_the_staging_area_are_all_collected() {
        let units = vec![unit("huge1", 100), unit("ok", 5), unit("huge2", 200)];
        let err = plan_batches(units.clone(), None, Some(50), fill(10), 1).unwrap_err();
        assert_eq!(
            err.iter().map(|u| u.name.as_str()).collect::<Vec<_>>(),
            vec!["huge1", "huge2"],
            "both, sorted by name"
        );
        assert_eq!((err[0].size_bytes, err[0].staging_total), (100, 50));
        // The positive control: a staging area as large as the largest.
        assert!(plan_batches(units, None, Some(200), fill(10), 1).is_ok());
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
        let first: HashSet<String> = ["d".to_string(), "b".to_string()].into_iter().collect();
        let batches = plan_batches(units.clone(), Some(&first), None, fill(7), 1).unwrap();
        assert_eq!(
            names_of(&batches),
            vec![vec!["b", "d"], vec!["a", "c"], vec!["e"]],
        );
        // Nothing prioritised: exactly the alphabetical plan.
        assert_eq!(
            plan_batches(units.clone(), Some(&HashSet::new()), None, fill(7), 1).unwrap(),
            plan_batches(units, None, None, fill(7), 1).unwrap(),
        );
    }

    #[test]
    fn empty_input_yields_no_batches() {
        assert_eq!(
            plan_batches(vec![], None, None, fill(1000), 512).unwrap(),
            Vec::new()
        );
    }

    // ── the plan bound (ADR-0012 items 113, 288 and 289) ──

    /// A fill ceiling at 8 and a plan bound at 10, block size 1.
    const ROOM: BatchRoom = BatchRoom {
        fill: 8,
        bound: Some(10),
    };

    /// With a bound, units are added while the plan's end is at or below the
    /// bound, past the fill ceiling included; the one that crosses the bound
    /// is the last.
    #[test]
    fn with_a_bound_a_batch_reaches_past_the_ceiling_and_ends_at_the_unit_that_crosses_it() {
        let units = vec![unit("a", 4), unit("b", 4), unit("c", 2), unit("d", 3), unit("e", 1)];
        let batches = plan_batches(units, None, None, ROOM, 1).unwrap();
        // a=4, b=8 (the ceiling), c=10 (past the ceiling, at the bound), d=13
        // crosses the bound: the batch's last unit.
        assert_eq!(names_of(&batches), vec![vec!["a", "b", "c", "d"], vec!["e"]]);
        let planned = ROOM.budget().place(
            0,
            &batches[0]
                .units
                .iter()
                .map(|u| PlanItem {
                    class: PlanClass::Queued,
                    bytes: u.size_bytes,
                })
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            planned.placed,
            vec![
                Placed::Committed,
                Placed::Committed,
                Placed::PastCeiling,
                Placed::Tail
            ]
        );
    }

    /// Item 289, a batch queued to fill: it ends a full tape with one unit
    /// across its end, whatever the units' sizes.
    #[test]
    fn a_batch_of_small_units_ends_with_one_unit_across_the_end() {
        let units: Vec<PendingUnit> = (0..9).map(|i| unit(&format!("u{i}"), 3)).collect();
        let batches = plan_batches(units, None, None, ROOM, 1).unwrap();
        assert_eq!(
            batches.iter().map(|b| b.units.len()).collect::<Vec<_>>(),
            vec![4, 4, 1],
            "3+3+3 is 9 (past the ceiling, within the bound), the fourth crosses it"
        );
        assert!(batches[0].padded_bytes > ROOM.limit_bytes());
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

        let batches = plan_batches(units, None, None, fill(budget), BLOCK).unwrap();

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

        // Fill guarantee: every closed (non-final) batch is filled to the
        // budget (the unit that closed it carried the plan past it), and
        // overshoots it by less than its last unit (item 289). Assert the
        // spec's concrete "~99%+" figure (T10 §4 / sheet §8) too.
        let (last, closed) = batches.split_last().unwrap();
        for (idx, b) in closed.iter().enumerate() {
            let fill = b.fill_fraction(budget);
            assert!(
                fill > 1.0,
                "batch {idx}: fill {fill:.5} must pass the budget (the tape is filled to its end)"
            );
            let last_unit = pad_to_blocks(b.units.last().unwrap().size_bytes, BLOCK);
            assert!(
                b.padded_bytes - last_unit <= budget,
                "batch {idx}: only its last unit may carry the plan past the budget"
            );
            assert!(
                b.padded_bytes <= budget + max_padded,
                "batch {idx} overshoots the budget by less than one unit"
            );
        }
        // The final batch is whatever's left — no fill requirement (it's
        // the tail, not a closed bin).
        assert!(last.padded_bytes <= budget + max_padded);
    }
}
