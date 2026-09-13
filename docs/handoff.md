# Handoff: what autopilot finishes, and what only you can do

Written 2026-08-01; updated 2026-08-03, master at `3bd65fc`. This document exists so that the
autonomous run and the operator have the same picture of what is left. It is
the answer to one question: **which remaining work needs a person, and which
does not?**

**THE AGENT QUEUE IS EMPTY (2026-08-03).** Every issue of every severity is
closed except the wayfinder map (#1), and the LTO-6 measurement harness is
built (`scripts/lto6-measure.sh`).

**Three things are irreducibly yours**, and they are all that is left: the
Heir Kit ceremony, the LTO-6 session on real media, and the first production
write. None of them is blocked on a decision — each needs your hands.

---

## STOP — read this before the first production write (2026-09-13)

The media-generation redesign (ADR-0010, ADR-0011) is landed, gated and
hardware-verified on branch `media-generation-model`: 1085 tests, the mhvtl
verify gate GREEN 26/26 against an empty EXPECTED_FAIL, and the lifecycle
suite's `first-year` scenario GREEN 45/45. One LTO-6 drive now handles LTO-5
and LTO-6 cartridges with nothing to change between tapes.

**But the adversarial review that followed it found three high-severity
defects, and the first production write should wait for them**
(`docs/audits/2026-09-13-post-redesign-review.md`, 59 confirmed findings):

- **#153 — copy counting treats versions as copies.** A unit with v1 on one
  tape and v2 on another reads as two copies. At the shipped default of two,
  `unit mark-tape-only` passes its consent gate and tells you it is safe to
  delete the source. Reproduced with the real binary. Predates this redesign;
  the same inflated number feeds `audit`, three reports and the location check,
  so nothing contradicts it. **This is the one that can lose data.**
- **#154 — late binding commits the displacement before the tape-contact
  check**, so a `volume write` that is then refused has already marked a live
  volume erased in the catalog.
- **#155 — `volume init --cartridge` displaces a live volume on a typed
  barcode alone** when no medium serial is readable. This is a hole in
  ADR-0010's reasoning, not only its code: the ADR justifies having no second
  consent gate on the grounds that the File 0 check already decided consent,
  which does not hold when the displacement is driven by what was typed.
  ADR-0010 needs amending alongside the fix.

The remaining 56 findings are medium and low and are written up with evidence
and a proposed fix in the same audit. The ones worth knowing before an
operator session: disaster recovery rebuilds no cartridge identity, so a
recovered tape cannot be re-bound; `db fsck --repair` cannot repair, because
its deletes violate the same foreign keys; and generation capacities are
decimal while every operator-facing `--capacity` parses as binary.

The review's completeness critic did not finish (session limit), so that list
is not certified complete.

## Where the project actually stands

- Milestones 0–7 complete; the Layout-v2 regear landed in full (T0–T11).
- `scripts/mhvtl-verify-gate.sh` is **GREEN 26/26 with `EXPECTED_FAIL=()`** —
  zero slack, so any gate failure is a hard stop rather than a known-defect
  pin. Verified across five consecutive runs on 2026-08-01 when the last
  nondeterminism (#113) was removed.
- 766 ungated tests; CI green; `cargo fmt`/`clippy -D warnings` clean.
- **Every issue of every severity above `low` is now closed** (#69, the last
  `severity:high`, landed 2026-08-02). What remains is nine `severity:low`
  issues and the wayfinder map.

One correction to `CLAUDE.md`, which is stale on this point:
`docs/lto6-validation-checklist.md` is **not** a "procedure stub". It is 143
lines, dry-run annotated against mhvtl, and already records the ENOSPC
fidelity gap. Read it before the hardware session; it is usable as-is.

## Tier A — pure software, no human needed — **ALL DONE**

Taken in severity order; every row below is closed.

| Issue | What it is |
|---|---|
| ~~#69~~ | **DONE (`d0c8503`).** `key escrow-kit` ships; `audit` reports kit staleness. Only the ceremony remains, under Tier C. |
| ~~#114~~ | **DONE (`a9eb90b`).** `TapectlError::PolicyUnresolvable` carries a `PolicyLayer`; the action names the layer that actually broke. |
| ~~#112~~ | **DONE (`3bd65fc`).** `main.rs` 728 → 312 lines; all command bodies now live in `src/cli/` where integration tests can reach them. |
| ~~#111~~ | **DONE (`552e44a`).** Device discovery extracted to `scripts/mhvtl-device.sh` — one implementation, gate + e2e both call it. Scratch roots env-overridable; fixtures stay in their TempDir. |
| ~~#110~~ | **DONE (`7ce61c4`).** Six fixes: catalog-ls panic, cartridge-list SQL binding, wrong search help, location JSON description, silent rename dotfile failures, vestigial `_depth`. |
| ~~#109~~ | **DONE (`2902658`).** `--home`/`TAPECTL_HOME` added; `--config`'s relocate behaviour kept but warned; all ten callers migrated. |
| ~~#108~~ | **DONE (`a262f65`).** A failed unlink is now reported with its path and named as permanent; the false comment corrected. |
| ~~#107~~ | **DONE (`c85a86c`, migration 009).** Tape alerts are stored and surfaced by `report health`. **Migration 010 is next.** |
| ~~#106~~ | **DONE (`5908a2c`).** fire-risk now resolves `min_copies` per unit, so it can no longer disagree with `audit`. |
| ~~#100~~ | **DONE (`850b5ab`).** `volume move` refuses a warehouse destination and names `volume deposit add` instead. |

## Tier B — agent-built, but only valuable if you then run it

**The LTO-6 measurement harness — BUILT and RUN (`scripts/lto6-measure.sh`,
2026-09-10; results in the session journal).** For a future cartridge or drive:

```bash
./scripts/lto6-measure.sh --erase-cartridge <BARCODE>
```

`docs/design/v2-open-questions.md` §5 lists
six questions that mhvtl structurally cannot answer, because mhvtl gives a
*false pass* on the most important one: at end-of-tape it accepted every write
without returning ENOSPC and silently produced unreadable slices.

The harness turns the hardware session from an exploration into "run this,
read the output". It will answer:

- block size **512 K vs 1 M** throughput;
- LBP `MODE SELECT` acceptance and `st` readback;
- MAM over-report bounds (this sizes the ENOSPC buffer);
- real ENOSPC behaviour — the clean-abort trigger;
- EOD semantics: confirm forward operations past EOD *error* rather than
  returning stale data (§3.2's physics assumption);
- v1-tape disposal confirmation.

It writes every raw command output to a recording directory.

**Dry-run finding (mhvtl, 2026-08-02):** 1 MiB blocks were refused with
`EBUSY` by the host `st` driver even though the drive advertised a 2 MiB
maximum. That is a host buffer limit, not a drive property — so the
512 K-vs-1 M question may not be answerable on a stock Linux host without
tuning `st` first. Worth knowing before you spend a hardware session on it.

**It erases cartridges.** It is therefore gated behind an explicit
barcode-naming confirmation, following ADR-0008's consent tiers rather than
inventing a new prompt style. It never runs as part of the normal gate.

A production-write rehearsal was considered and **dropped** (operator
decision, 2026-08-01): the mhvtl gate already proves stage → write → verify →
restore end to end, so a synthetic rehearsal would mostly re-prove that.

## Tier C — only you can do these

`scripts/first-run.sh` is the guided version of all three: it stops at the
points below that need your hands (the printed escrow secret, the printed kit,
the barcode of a cartridge you are willing to erase, the label you write on
the production tape) and does everything around them.


### 1. The Heir Kit ceremony (the remainder of #69)

**The command shipped in `d0c8503`** — run
`tapectl key escrow-kit --out <dir>`, which writes `COVER.txt`,
`escrow-kit.html` and `catalog.db.age` and then stops. Yours:

- print `COVER.txt` (and/or the HTML page) — the `.txt` is the artifact with
  the decades-scale claim, readable with `cat` when no browser exists;
- seal into **tamper-evident envelopes**;
- distribute across **≥2 independent failure domains**;
- storage class: UL-350 for paper, Class-125 if stored with tape;
- **refresh after each production write session** — `audit` will warn (exit 1,
  never 2) when volumes have been sealed since the last generation, so you get
  told rather than having to remember.

Two things learned on 2026-09-11 that belong on the cover sheet's mental
model, since the kit is what an heir or a rebuilt machine starts from:

- **The escrow identity can never be replaced, only imported first.** A
  rebuilt machine must `init --no-escrow` and then `key import --escrow` the
  original public key BEFORE restoring or rebuilding anything; a plain
  `init` mints a replacement identity and no command replaces it (the empty
  home has to be deleted and started again — #139 proposes
  `init --escrow-public-key` so there is no wrong order). The operator guide's
  Disaster Recovery section is the procedure.
- **The catalog is reconstructible from tape** with the operator or escrow
  key (`catalog rebuild --from-volume`), and tapes written after 2026-09-11
  carry their escrow receipts; the kit's `catalog.db.age` is still the faster
  and fuller starting point (locations, cartridges, verification history are
  never on tape).

### 2. ~~The LTO-6 hardware session~~ — DONE 2026-09-10

`docs/lto6-session-journal-2026-09-10.md` is the record; the §5 open
questions are answered there (block size 512 K vs 1 M is a wash, MAM
over-report is +2 MiB). Three further real-drive confirmation passes ran on
2026-09-11 (45/45 each) after heir-path and envelope byte changes, with the
changed zones read back off the cartridge — see
`docs/runs/2026-09-11-unattended.md`. The drive numbering hazard is real:
resolve by serial through `/dev/tape/by-id/`, never by `/dev/nstN`.

### 3. The first production write

An explicit hard stop for autopilot. Nothing writes real data to real media
without you.

## Decisions already ratified — do not re-litigate

Recorded in `docs/adr/0009-heir-kit-contents-and-staleness.md` and the
`§2.16` row of `docs/design-errata.md` (2026-08-01):

1. #69's deferral covers the **ceremony, not the command**. Its dependency
   #68 is closed and the escrow recipient is live in the write path.
2. The escrowed bundle is the **full `tapectl.db`**, not #83's filtered
   `catalog.db` — the filtered schema has no `locations` and no `cartridges`,
   so an heir would learn what exists but not which cartridge to fetch. Safe
   because the DB holds no secret material: only `tenants.public_key`, with
   every private half a file under `keys/`.
3. Output is **self-contained HTML (inline SVG QR) plus a plain-text
   `COVER.txt` twin**. PDF rejected — heavy dependency, less inspectable.
4. **Kit staleness is recorded and warned about advisorily.** ADR-0005 names
   that failure mode and then rejects *enforced* discipline, leaving
   discipline-by-memory; an advisory check is neither.
5. QR encoder: **`qrcode` 0.14.1, `default-features = false`, `features =
   ["svg"]`** — verified to pull no image stack, which was the open risk.

## Done

Reached 2026-08-03 at `3bd65fc`. Autopilot stopped rather than idling.

One thing surfaced rather than decided: **the wayfinder map (#1) is now
complete-but-unclosed** — every child issue is closed and the renovation
stage it charts is finished. Closing a map is not autopilot's call (the same
rule that applied to epic #20), so it is left open for you.
