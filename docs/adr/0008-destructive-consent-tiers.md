# Destructive consent is tiered; `--force` has a ceiling

Destructive operations (`volume retire`, `unit mark-tape-only`, `compact-finish`,
`db import`, `cartridge mark-erased`, `snapshot delete`) had one consent mechanic
between them: a per-command `--force` that waived every guard on that command
uniformly. We decided consent is **tiered by what is actually at risk**, and that
`--force` stops short of the top tier.

**Tier 1 — staleness: displayed, never gates.** Unchanged from ADR-0004. Evidence
age is shown wherever a destructive operation consumes the copy derivation
("coverage for unit X rests on L6-0003, last verified 15y ago"), but it never
blocks and never requires `--force`.

**Tier 2 — degraded but non-zero coverage: `--force` / `--yes` overrides.** Copy
count below the resolved `min_copies`, locations below `min_locations`, or a
**dirty** unit (source has moved on from its snapshot). Each of these has a
legitimate operator intent — the second copy lands next week, or the operator
knows the delta is junk — and in every case the data still exists somewhere
other than the thing being given up.

**Tier 3 — zero coverage: absolute, no override.** Marking a **Never Archived**
unit tape-only, and writing to an already-sealed cartridge. These are refused
outright; no flag defeats them. This generalizes a precedent the write path
already set: `check_tape_contact` deliberately makes `AlreadySealed`
un-overridable because permitting it would breach ADR-0003, and that refusal is
documented in `session.rs` as something `--force` is *not* allowed to defeat.

The distinction Tier 2/Tier 3 draws is between *risk* and *incoherence*.
`--force` should mean "I accept a degraded but non-zero safety margin." Marking a
unit tape-only when it is on no tape is not a riskier version of that — it is a
contradiction in terms, and it greenlights deleting the only copy of data that
exists nowhere else. The escape hatch for Tier 3 is not a flag but a single
command (`snapshot create`), which resolves the incoherence instead of waiving
it.

**Consent is TTY-aware.** Where consent is required, tapectl prompts when stdin
is a terminal (`std::io::IsTerminal` — no new dependency) and displays the ADR-0004
coverage facts at that moment, which is the one place the operator is guaranteed
to read them. When stdin is **not** a terminal and no `--yes`/`--force` was given,
the operation **refuses with a non-zero exit** — it must never block waiting on a
prompt nobody can answer, and must never silently assume consent. Tier 3 refuses
regardless of `--yes`.

The non-hanging requirement is not hypothetical: the same failure shape (blocking
forever on a handle that will never produce input) was a live bug in the staging
path until issue #33, where opening a FIFO with no writer hung `stage create`
with no timeout. Any consent prompt added without a terminal check reintroduces
exactly that class of hang into cron jobs and the mhvtl verify gate.

Considered and rejected: **uniform `--force`** (one flag waives every guard —
simplest to document, but the flag that waives "you are one copy short" would
also waive "this data exists nowhere but here"); **a second, deliberately awkward
flag for the severe case** (keeps an escape hatch and resists reflex use, but
still concedes that a coherent state can be flagged into existence, and adds a
flag to document and support forever); **always prompt** (strongest interactive
safety, but it is the option that hangs a non-interactive run the moment terminal
detection is missed); and **flags only, never prompt** (zero hang risk and fully
scriptable, but it forgoes ADR-0004's "display coverage at the irreversible
moment" — the facts would land in a message the operator has already committed to
skipping past).
