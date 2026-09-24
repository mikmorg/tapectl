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

## STOP — read this before the first production write (updated 2026-09-14)

The media-generation redesign (ADR-0010, ADR-0011) is landed, gated and
hardware-verified: 1087 tests, the mhvtl verify gate GREEN 26/26 against an
empty EXPECTED_FAIL, and the lifecycle suite's `first-year` scenario GREEN
45/45. One LTO-6 drive now handles LTO-5 and LTO-6 cartridges with nothing to
change between tapes.

The adversarial review that followed it
(`docs/audits/2026-09-13-post-redesign-review.md`, 59 confirmed findings,
~44 distinct) was grilled question by question and **every ruling was
ratified on 2026-09-14**. The decisions are in
`docs/adr/0012-copies-are-identical-content-cartridges-are-known-by-serial.md`;
the corrections to ADR-0010/0011's own text are dated inside them.

**The first production write now waits on three things, and only these:**

1. **The GitHub label `review-2026-09-13` is empty** — every finding and every
   ratified ruling is filed under it, severity-ordered, and autopilot works
   it to zero. *Everything* is in scope before the first write, documentation
   cleanups included (the CTO's Q3 ruling overrode "code first, docs later").
   Autopilot may correct a record's *facts*; it may not re-open a *decision*.
   Decisions it cannot make are parked and batched to you, and it never
   reports the queue empty while anything is parked.
2. **The adversarial review is re-run on the resulting diff**, the same way
   (`docs/audits/` gets the record), before the write — not after.
3. **A real-drive rehearsal on an expendable cartridge** (DONE 2026-09-23 on this VM with
   the drive passed through — `docs/runs/2026-09-23-real-drive-rehearsal.md`; production runs
   here, not on home2, ruled the same day), which also
   settles the one open measurement: MAM's maximum-capacity attribute is
   either MiB or MB (a 10 % question; the code assumes MiB) — **settled: MiB**
   (`docs/runs/2026-09-23-lto6-capacity-measurement.md`). Write that cartridge
   to end-of-tape and record where ENOSPC fell — **done 2026-09-24: 2.5020 TB,
   1.0008 x the planning figure; the gate's 0.92 factor is sound**
   (`docs/runs/2026-09-23-real-drive-rehearsal.md`, "The end-of-tape fill").

**Run that rehearsal early** (ruled 2026-09-15) — whenever your week allows,
not after the queue drains. It measures hardware facts, which none of the
queued defects can distort, and it is the only step that can invalidate an
assumption *before* forty issues of work are built on it. `scripts/lto6-measure.sh`
is the harness; extend it rather than writing a new one. A second, final
rehearsal on the finished tree still happens before the first write.

The finding that can lose data is still **#153 — copy counting treats
versions as copies** (v1 on one tape and v2 on another read as two copies, so
`unit mark-tape-only` tells you it is safe to delete the source). It is
severity-first in the queue and its ruling is ADR-0012's first paragraph.

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
- **copy the escrow secret by hand into the sheet's box marked "WRITE IT
  HERE"** — the kit prints only the escrow *identity* (the public half); the
  secret was shown once by `init` and is stored nowhere, and without it the
  sheet opens nothing. Check the pair with `age-keygen -y` as the sheet says
  (issue #341);
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

**2026-09-14 — the pre-production rulings**, recorded in ADR-0012 (the
decisions), dated corrections inside ADR-0010/0011, and `CONTEXT.md` (*Copy*,
*Version*, *Cartridge Identity*, *Barcode*). The process rulings that are not
domain decisions live only here:

1. The queue is the GitHub label `review-2026-09-13`, worked in severity
   order; the ~44 distinct findings were deduplicated before filing (the
   critic named the ten clusters) and each issue states its ADR-0012 ruling
   as *the* fix, not an option.
2. `media-generation-model` merged to master before autopilot started.
3. All work — code and documentation — before the first production write.
4. Autopilot corrects a record's facts, never its decisions.
5. The mhvtl gate runs per item only for write-path and restore-path changes;
   everything else is gated in batches.
6. `#143` (config set/add/remove) and `#144` (--policy-aware packing) are
   excluded from the queue; `#145` (volume calibrate) is rejected.
7. Decisions autopilot cannot make are parked, batched, and never reported
   as an empty queue.
8. The adversarial review is re-run on the diff before the write.
9. The real-drive rehearsal on an expendable cartridge is a hard prerequisite.

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
