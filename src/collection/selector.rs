//! The batch selector (`docs/design/v2-open-questions.md` §11, formalizing
//! the §7 ruling): alphabetical first-fit over pending units.
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
//! has real operational value. Alphabetical first-fit stands.

use crate::volume::layout_model::pad_to_blocks;

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

/// A unit whose block-padded size alone exceeds the per-tape budget.
///
/// Per the T10 trap ("a unit too large for a whole tape is a clear error,
/// not a split" — §7: "Unit contiguity is inviolate regardless, a movie
/// folder is never split across tapes"), this is a hard error, checked
/// BEFORE the fits-in-current-batch test so an oversized unit can never
/// silently close an empty batch (which would either loop forever trying to
/// place it, or emit a batch that itself overflows the budget).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OversizedUnit {
    pub name: String,
    pub size_bytes: u64,
    pub padded_bytes: u64,
    pub budget_bytes: u64,
}

impl std::fmt::Display for OversizedUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unit \"{}\" ({} bytes, {} block-padded) exceeds the per-tape budget \
             ({} bytes) — units are never split across tapes, so this one cannot \
             be batched at all",
            self.name, self.size_bytes, self.padded_bytes, self.budget_bytes
        )
    }
}

impl std::error::Error for OversizedUnit {}

/// Greedily fill batches, alphabetical first-fit (§7 ruling — explicitly
/// NOT best-fit-decreasing): sort pending units by name, then walk the
/// sorted list, placing each unit into the current batch if it fits (Σ
/// block-padded sizes ≤ `budget_bytes`), else closing the batch and
/// starting a new one with that unit.
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

    let oversized: Vec<OversizedUnit> = units
        .iter()
        .filter_map(|u| {
            let padded = pad_to_blocks(u.size_bytes, block_size);
            (padded > budget_bytes).then(|| OversizedUnit {
                name: u.name.clone(),
                size_bytes: u.size_bytes,
                padded_bytes: padded,
                budget_bytes,
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
    /// budget 10. First-fit: a(3)->3, b(4)->7, c(3)->10 (exactly full),
    /// d(5) doesn't fit (10+5>10) -> close [a,b,c]=10, start d(5)->5,
    /// e(2)->7 (fits) -> final batch [d,e]=7.
    #[test]
    fn greedy_first_fit_matches_hand_computed_batches() {
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

    #[test]
    fn empty_input_yields_no_batches() {
        assert_eq!(plan_batches(vec![], 1000, 512).unwrap(), Vec::new());
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

        // Fill guarantee: for any closed (non-final) batch, greedy first-fit
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
