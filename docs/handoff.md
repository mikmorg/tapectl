# Handoff: what autopilot finishes, and what only you can do

Written 2026-08-01; updated 2026-08-02, master at `d0c8503`. This document exists so that the
autonomous run and the operator have the same picture of what is left. It is
the answer to one question: **which remaining work needs a person, and which
does not?**

The short version: **four software issues remain agent work; the hardware
measurement harness is built (`scripts/lto6-measure.sh`). Three things are irreducibly yours** — the Heir Kit
ceremony, the LTO-6 session on real media, and the first production write.

## Where the project actually stands

- Milestones 0–7 complete; the Layout-v2 regear landed in full (T0–T11).
- `scripts/mhvtl-verify-gate.sh` is **GREEN 26/26 with `EXPECTED_FAIL=()`** —
  zero slack, so any gate failure is a hard stop rather than a known-defect
  pin. Verified across five consecutive runs on 2026-08-01 when the last
  nondeterminism (#113) was removed.
- 729 ungated tests; CI green; `cargo fmt`/`clippy -D warnings` clean.
- **Every issue of every severity above `low` is now closed** (#69, the last
  `severity:high`, landed 2026-08-02). What remains is nine `severity:low`
  issues and the wayfinder map.

One correction to `CLAUDE.md`, which is stale on this point:
`docs/lto6-validation-checklist.md` is **not** a "procedure stub". It is 143
lines, dry-run annotated against mhvtl, and already records the ENOSPC
fidelity gap. Read it before the hardware session; it is usable as-is.

## Tier A — pure software, no human needed

Taken in severity order. All that remain are lows.

| Issue | What it is |
|---|---|
| ~~#69~~ | **DONE (`d0c8503`).** `key escrow-kit` ships; `audit` reports kit staleness. Only the ceremony remains, under Tier C. |
| ~~#114~~ | **DONE (`a9eb90b`).** `TapectlError::PolicyUnresolvable` carries a `PolicyLayer`; the action names the layer that actually broke. |
| #112 | Move four inlined command bodies out of `main.rs` into `src/cli/`. |
| #111 | Test-harness hygiene: hardcoded roots and devices. |
| #110 | Grouped one-line correctness and honesty cleanups. |
| #109 | Separate `--home` from `--config`. **Not gate-free** — the gate script depends on the current hijack, so the two change together. |
| ~~#108~~ | **DONE (`a262f65`).** A failed unlink is now reported with its path and named as permanent; the false comment corrected. |
| ~~#107~~ | **DONE (`c85a86c`, migration 009).** Tape alerts are stored and surfaced by `report health`. **Migration 010 is next.** |
| ~~#106~~ | **DONE (`5908a2c`).** fire-risk now resolves `min_copies` per unit, so it can no longer disagree with `audit`. |
| ~~#100~~ | **DONE (`850b5ab`).** `volume move` refuses a warehouse destination and names `volume deposit add` instead. |

## Tier B — agent-built, but only valuable if you then run it

**The LTO-6 measurement harness — BUILT (`scripts/lto6-measure.sh`).** Run it
first in the hardware session:

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

### 2. The LTO-6 hardware session

Follow `docs/lto6-validation-checklist.md`, with the Tier-B harness doing the
measurement. Pre-flight matters: SCSI enumeration shuffles, so discover the
changer and the drive's sg node rather than assuming `/dev/sg0` or slot 1.

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

## When this is done

Every remaining item will be Tier C. At that point autopilot stops rather
than idling, and the outstanding work is exactly the three things above —
each of which needs your hands, not a decision.
