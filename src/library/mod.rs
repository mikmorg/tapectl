//! The Library concept (`docs/design/v2-open-questions.md` §11, finishing
//! the §7 media-library workload sketch): a factory + batch driver over
//! existing unit machinery for append-mostly media roots (thousands of
//! folder-units), so the operator configures one `[[libraries]]` block per
//! root instead of `unit init`-ing each folder by hand.
//!
//! Units remain first-class underneath — this module only automates
//! registration (`sync`), reports readiness (`status`), and batches pending
//! work into tape-sized groups (`plan`) before driving the existing
//! stage/write pipeline once per batch (`batch`).
//!
//! Deliberately out of scope (§11): filesystem watching/daemons (#13
//! verdict), scheduled sync, any cross-library dedup (#12 — full-only
//! stands), and best-fit-decreasing packing (§7 — alphabetical first-fit
//! preserves the name-ordered "tape spine").
//!
//! Built up module-by-module in the T10 branch history: this first commit
//! lands the pure batch selector, which needs nothing else in this module.

pub mod selector;
