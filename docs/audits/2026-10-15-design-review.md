# Design and issue review — 2026-10-15

**Asked for:** "a comprehensive review of the current design and the GitHub issues for
flaws, improvements, missed concerns and areas of research", by the CTO on 2026-10-15,
while `v1.1.1` waited on its last mhvtl gate (master `4cb98a0`, the version 1.1.1,
untagged).

**Range read:** 1.1.1's 97 commits (`v1.1.0..4cb98a0`), the DR and heir path as it stands,
the in-place upgrade of home2 as `docs/handoff.md` (f) writes it, the five open issues, the
design set (ADR-0012 through its 2026-10-15 amendment, `threat-model.md`,
`volume-format-v2.md`, `design-errata.md`) and the operator documents where grepped.

**Method:** three read-only finders (no cargo, no tape, no scripts, no device nodes), one
lens each, drawn from this project's own recent misses: (A) 1.1.1's new paths, data safety
and consent; (B) the home2 upgrade as written, against the scripts, the units and Debian
10; (C) DR, heir path and documentation drift. The coordinator kept the fourth lens (the
open issues' premises and the research frontier), read every cited line of every medium
finding before filing, and corrected one finder (A1 below). Every finding cites a line
read in this pass; the default verdict was "not a defect".

**Result:** no high-severity defect. 34 raw findings: 8 issues filed (#444–#451), 16
documentation corrections landed in this commit, 10 recorded here only. The open
issues' premises are checked below and ten missed concerns and research items are put
to the CTO as a decision list. **Nothing found holds the `v1.1.1` tag:** no finding is a
data-loss or consent defect in 1.1.1's code, and the two mediums in its paths (#444, the
unproven blank proofs; #445, `--init` before the write's refusals) fail closed. The
documentation corrections to handoff (f) matter to the operator at upgrade time, so the
tag should go on this docs-only commit, whose `src/`, `scripts/` and `Cargo.*` are
byte-identical to `4cb98a0`, the commit the gate runs on.

## 1. Filed

| # | Sev | Lens | Finding |
|---|-----|------|---------|
| [#444](https://github.com/mikmorg/tapectl/issues/444) | medium | A | `volume init`'s proven-blank paths (#434, item 57): the verified-erase proof ran on mhvtl only through a scratchpad script outside the repo, neither proof has run on the HP LTO-6, the chip proof reads "0 MiB written" as never written (a sub-MiB foreign write may read 0), and the erase proof cannot see another tool's writes (the journaled lifetime-written figure would) |
| [#445](https://github.com/mikmorg/tapectl/issues/445) | low | A | `volume write --init` initialises the cartridge before the write's catalog-only refusals (no staged data, distinctness, keys), stranding an `initialized` row and, with `--force`, a used tape's File 0 |
| [#446](https://github.com/mikmorg/tapectl/issues/446) | low | A, B | `first-run.sh` step 13 routes an unreadable-File-0 refusal into the SEALED-volume branch; the wrapper comment at `:1672` is pre-#432 |
| [#447](https://github.com/mikmorg/tapectl/issues/447) | low | A | a child-side seccomp failure is silent (#441, performance only); `config check` calls a detached drive a refused backend |
| [#448](https://github.com/mikmorg/tapectl/issues/448) | medium | C | Heir Kit cover sheet and File 1: the only RESTORE.sh example lacks `--all`, so an escrow-key heir can restore one lone unit and read COMPLETE; "a matching generation" where only LTO-6 and LTO-7 drives read LTO-6; a pre-1.1.0 disk-space paragraph; drifting tool lists; no interface, no "tapes are at" box; `escrow_kit_stale` ignores moves |
| [#449](https://github.com/mikmorg/tapectl/issues/449) | low, needs:cto | C | RESTORE.sh items for the next re-pin: lone-unit COMPLETE when other envelopes list more; per-mode tool checks; DAMAGED wording when no key matched; the work directory's size and `TMPDIR` |
| [#450](https://github.com/mikmorg/tapectl/issues/450) | medium | C | no ungated scan of a volume's plaintext zones; no scan anywhere looks for key fingerprints or `age1…` strings, which the invariant lists |
| [#451](https://github.com/mikmorg/tapectl/issues/451) | low | C | `catalog rebuild --from-volume` aborts when any envelope is unreadable in both copies, undocumented; a generation 2/3 rebuild needs only the operator envelope |

Severity notes:
- **#444** is medium for the shape, not the outcome: every path fails closed (a refusal;
  `--force` still works), but `first-run.sh` step 13 now depends on a proof the repo's own
  harness never runs, and f54964c removed the retry that used to cover it. The finder rated
  it medium and called the proof "FakeTape-only"; the coordinator's scratchpad erase-proof
  script (chain29, `c_init_proven` PASS) did run it on mhvtl, which the issue records. The
  real-drive half is still owed, at the step-12 rehearsal handoff (f) step 7 now names.
- **#448** is medium because it is the heir's one page. A single wrong example and a wrong
  word about drives are the two ways the sheet fails the person it is for.
- **#450** is medium, not low, because `threat-model.md` claimed CI coverage that did not
  exist; the sentence is corrected in this commit and the test is the fix.

## 2. Documentation corrected in this commit

| File | What was wrong |
|---|---|
| `docs/handoff.md` (f) | `tapectl-op db backup` without the required `--to`; `tapectl status` as the "nothing running" check, which on 1.1.0 reads the operator's own home; no note that `git checkout v1.1.1` waits for the tag; no copy of the 1.1.0 binary and no rollback story (there is no downgrade from schema 036); step 7's re-entry re-asserts the profile's `staging.jobs`, contenders and dar path; the new wrapper is never exercised by (f), so `check-tapectl-op.sh` is now step 5's proof; the drive-poll timer opens the sg node daily and is now stopped for step 12; the real-drive first run of item 57 is named |
| `docs/handoff.md` (d) | `volume write --keep-staging` does not exist on 1.1.0, which (d) runs on |
| `docs/handoff.md` (e), table, hazards | closed issues listed as to-do (#326, #366, #368, #369, #371, #393, #416, #425); no row for (f); "verify before `staging clean`" written for a write that kept staging; the tape-lock hazard claimed an exclusion the binary does not take part in |
| `docs/releases/1.1.1.md` | "Known open items" omitted #364; no word that there is no downgrade from migration 036 |
| `docs/design/threat-model.md` | §1.1 credited the tape-bytes plaintext scan to `tests/tenant_isolation.rs`, which never touches tape (#450); §2 omitted the unload (`drive eject`, `MTOFFL`) and overstated `MTSETBLK` on every open; §5 said #395's `stage create` refusal was "not yet built" (built in b206116, `staging::check_unit_fits_one_tape`, with three caveats now stated) |
| `docs/design/volume-format-v2.md` | "never on the tape" lacked the cleaning registry and feed ratios; "before `staging clean` releases the staged slices" predates item 63 |
| `docs/design-errata.md` | "as of 2026-10-12"; no rows for item 63 (staged slices persist until `staging clean`, design §2.3) and #431 (`device_sg` optional) |
| `CLAUDE.md` | "home2 runs 1.0.0 … starts fresh" and "Next: the operator's fresh start" after the fresh start ran; the open list named closed issues and omitted #409 |
| `docs/install.md` | "no flag or profile variable carries the key" (`ESCROW_PUBLIC_KEY` and `--escrow-public-key` since 1.1.0), twice |
| `docs/operator-guide.md` | "the `volume verify` that steps 3–5 above run before `staging clean`" referred to steps that now use `--full-confirm` |

Not corrected here, recorded only: `first-run.sh:1672` (in #446, a `scripts/` change);
`src/collection/selector.rs:94-98` says "first-fit" and implements next-fit (a comment, in
§4 item 9 below); `config check`'s detached-drive text (#447); the frozen RESTORE.sh says
"tapectl 1.1.0" in `die_newer_needed` (by item 20 of 2026-10-06, patch releases leave it).

## 3. The open issues: premise checks

Each open issue was ruled. This checks the premise the ruling rests on, in ADR-0012's own
"revisit if" pattern. None of these is a ruling.

### #420 — the short seal (ruled "later", 2026-10-06 item 9)

- **The measurement it waits for is narrower than the issue says.** The 2026-09-23 fill
  (`docs/runs/2026-09-23-real-drive-rehearsal.md`) ended at the first `ENOSPC`, because
  `dd` stops at its first error. It proved where the drive's early warning is (2.5020 TB,
  1.0008 x the generation table) and how big the reserve behind it is (page 0x0c "Minimum
  native capacity from EW to EOP", 99,008 MB). It did **not** prove whether the st driver
  accepts further writes after that one `ENOSPC`. The st driver's semantics as remembered
  (to be confirmed on the HP drive, never taken from memory) are that the early-warning
  `ENOSPC` is returned once and writes then continue to physical end of medium. The
  experiment #420 needs is one afternoon on EW7VWMVKF6: after the first `ENOSPC`, keep
  writing 512 KiB blocks and record how many the drive takes before `EIO`. If the answer is
  about 99 GB, #420's design (finish one 1 GiB slice, write the seal) has 100x the room it
  needs and no mode-page change (PEWS) is required, which keeps the SG boundary of
  `threat-model.md` §2 intact. The same afternoon can take #444's sub-MiB measurement.
- **What it is worth.** The fill ceiling (97%) is the binding constraint today: the
  2026-09-23 data/plan ratio of 1.0008 says the table is exact, so the ceiling gives up
  about 75 GB per LTO-6 cartridge to protect against a late `ENOSPC` that costs the whole
  pass. With a short seal the ceiling can approach 100% of the table and a late end of
  tape costs one unit's re-write. Worth doing after the drive measurement; not before.

### #388 — Logical Block Protection (research)

- **It cannot replace the full readback, and the issue frames it as if it might.** LBP plus
  the drive's read-after-write proves the bytes the host sent reached the medium. The full
  readback proves something else as well: that a sealed tape **reads** in this drive, cold,
  through the whole chain, and (#421) how many corrected read errors it took. The right
  framing is LBP as an *additional* write-time check that catches what the drive's own
  verify cannot (HBA, cable, DMA corruption between host memory and the drive), with the
  full readback kept as the medium's test. The ruling the issue asks for should be put that
  way.
- **It needs MODE SELECT**, which `threat-model.md` §2 says tapectl never sends. Enabling
  LBP is either a one-time `sg_wr_mode` by the operator outside the binary (and then a
  check, not a set, in the pre-flight), or a widening of the SG boundary. The issue does
  not mention the boundary; the research note must.
- **The heir question decides it:** with `LBP_R=0` (the default on any other drive) a
  reader gets the plain blocks and `dd`/RESTORE.sh are unaffected; this is what SSC-4 says
  and what the prototype must show on the HP drive, together with st's tolerance of
  512 KiB + 4-byte blocks in fixed-block mode.

### #418 R4 — the cross-volume per-tenant index (1.2.0 or later)

- **Its marginal value is small now.** The operator envelope on every tape carries the
  whole `catalog.db` as of that write, so the newest tape already indexes every earlier one
  for anyone holding the operator or escrow key, and the Heir Kit's `catalog.db.age` does
  the same off tape. R4 helps only a tenant holding a tenant key and nothing else.
  Recommend: keep it open, mark it low, and let a real tenant restore request reopen it.

### #409 — a failed stage starts over (ruled "not until spanning", 2026-10-07 item 11)

- **The exposure has grown since the ruling, and the cheap mitigation is undocumented.** A
  unit may now be up to half a cartridge (#395), so a stage can run 7–15 hours at home2's
  rates, and `--retry-on-change 0` means **one file changed by anyone during those hours
  refuses the whole stage** (dar exit 11, `troubleshooting.md:1089`). For `keepsake`
  (static) that is moot; for any living collection it is the dominant failure mode of a
  long stage. No document says "stage from a read-only snapshot" or "do not edit the
  source while it stages" (grep for quiesce/snapshot/LVM/btrfs in `docs/` finds nothing).
  Recommend: an operator-guide paragraph now; later, a per-collection snapshot hook so
  `collection run` stages from an LVM, btrfs or ZFS snapshot, which also removes the
  hasher-vs-dar race that the post-dar stat check only detects.

### #364 — staging throughput (epic)

- The bound on home2 was the source's second read; #441 removed it. Re-measure before
  anything else (the handoff owes that run); #369 and #371 closed on 2026-10-14.

## 4. Missed concerns and research areas — a decision list

Numbered for the CTO. Each has a recommendation and the trigger that would reopen it.
None is a ruling; the handoff's do-not-re-litigate list applies. Items marked *premise
check* only re-examine a ratified ruling's premise.

1. **1.1.0 was never adversarially reviewed as code.** The four recorded reviews
   (`docs/audits/2026-09-17` to `2026-09-23`) end at `cf4a371`. Between it and `v1.1.0`
   are 439 commits and 81,648 inserted lines under `src/` and `scripts/`: plaintext-free
   staging, the RESTORE.sh rewrite, `requires`, tenant-envelope backups, catalog.db
   generation 3, `cartridge erase`, the adopt-key path, migrations 027–035. They were
   proven by the harness and the real drive, which test what they were built to do, not
   what a reader would find. This review read 1.1.1's 97 commits and the DR path only.
   **Recommend** one adversarial pass over `cf4a371..v1.1.1` with dimensions drawn from
   the recent misses (consent in `cartridge erase`; the three `requires` readers;
   seccomp/`pre_exec`; migrations; operator text naming refused commands; tests that
   cannot fail), as a multi-agent workflow on the CTO's word. Trigger: before #420 is
   built.
2. **The paper escrow secret is never proven.** `init`'s ceremony prints the secret once
   and says "verify the transcription character-by-character" (`src/cli/key.rs:908`);
   nothing checks the paper. There is no `key escrow-verify`: type the secret from the
   paper, tapectl derives its public half, compares it with the registered escrow
   recipient and discards the input. One mistranscribed character makes every tape
   unreadable by an heir, and today that is found only when the heir needs it (the annual
   drill accepts "operator or escrow secret", `lto6-validation-checklist.md:94`).
   **Recommend** building it (small; a new command, so a CLI-surface decision) and making
   the annual drill use the paper secret only. Trigger: none.
3. **`verify_interval_days` has no default**, so `audit`'s `verify_age` never fires unless
   the operator sets one (`src/cli/audit.rs:915`; `configuration.md:405` says so).
   `no_full_verify` covers a volume that was never read back; nothing covers one read
   back once and never again. home2's profile sets none. For an archive meant to outlive
   its operator the default should be finite. **Recommend** `verify_interval_days = 365`
   on home2's archive sets now (profile) and a code default in 1.2.0. Trigger: none.
4. **A unit's verify age is satisfied by any one copy.** `check_verify_age` takes the
   newest passing full readback over every volume holding the unit, so the home copy
   keeps the clock fresh while the offsite copy goes unread for years. The per-volume view
   exists only in `report verify-status`. **Recommend** a per-volume `verify_age`, or the
   operator rotation (swap the offsite and home copies yearly and verify the one that came
   home). Research: what re-read cadence per copy LTO at domestic storage conditions
   needs; #421's trend data will say.
5. **Heir hardware longevity is not written down.** An LTO-6 cartridge reads only in an
   LTO-6 or LTO-7 drive (`src/media.rs:194-197`; read compatibility broke at LTO-8). In
   fifteen years the heir's problem is not `age` or `dar` but a working drive and an HBA.
   The cover sheet says "a matching generation" (#448 fixes the sheet). The design has no
   migration trigger: **recommend** naming one (copy the fleet to a newer generation while
   LTO-7 drives are still obtainable) and recording the drive models that read LTO-6. The
   tape-to-tape path exists today per unit (`volume read-slices --from <good>` then
   `volume init` and `volume write`, no re-staging, no new version), and per volume
   (`compact-read`, `compact-write --destination`, skipping `compact-finish`), with no
   `--all` and no cross-source merge. A fleet migration would want a `volume copy`.
6. **Staging a living source** (§3, #409): no quiesce or snapshot guidance; a snapshot hook
   later.
7. **The threat model does not state the compromised-host case.** `threat-model.md` §1
   scopes out a party with custody of a cartridge; it says nothing about malware or a
   mistaken `rm` on home2, where the catalog, every tenant secret key and the drive live.
   The defences exist (the offsite copy, ADR-0003's refusals, the on-tape catalog, the
   kit), but the document should say which of them hold against a root compromise of the
   host (the offsite cartridge and the paper secret do; nothing on home2 does). One
   paragraph. Related and not a tapectl defect: home2 runs Debian 10, whose LTS ended in
   June 2024.
8. **The catalog backup never leaves the host.** The daily backup goes to md3 on home2
   (`home2.profile:61`). The off-host copies are the on-tape `catalog.db` (as of each
   write) and the kit's `catalog.db.age` (refreshed by hand). Loss of home2 between a
   write and the next kit refresh loses forensics (events, journals, stage reports), not
   recoverability. **Recommend** the backup unit also copies to a second host when one is
   reachable; low.
9. **The selector is next-fit, documented as first-fit, under a premise that moved**
   (*premise check*). `plan_batches` walks the alphabetical list and closes the batch when
   a unit does not fit (`src/collection/selector.rs:94-98`): that is next-fit. §7 ruled it
   against best-fit-decreasing when units were 2–15 GB against 2.2 TB bins, where the
   waste is under 1%. #395 now admits units up to half a cartridge, where next-fit can
   seal a tape half empty. Re-examine when any unit passes about 10% of a tape; fix the
   docstring with the next `src/` change.
10. **CI cannot run the heir path with the real `age`.** `tests/heir_restore_sh.rs` fakes
    `age` because CI has no age CLI (its header says so); the real CLI is exercised only by
    the mhvtl gate on this VM and by home2's proof. Acceptable, but the release checklist
    should say the gate is the heir path's only test with real tools.

## 5. Recorded only (no issue)

- **A5.** `hold_drive_for_contact` waits 60 s for the drive lock and then proceeds; a
  full-tape unload can take longer, so `drive eject`'s "no command can start mid-eject"
  is only as strong as st's single opener and the volume-lock check. Harmless: a write
  started in the window gets EBUSY or eject gets `UnloadFailed` (exit 2, a `failed`
  contact).
- **B9.** home2's explicit `device_sg` is still judged by `sg_pairing_problem`; a `/dev/sgN`
  renumbering after a reboot makes `volume write`, `drive poll` and `drive eject` refuse by
  name until it is fixed. Removing the line (handoff (f) says how) removes the hazard.
- **C4.** RESTORE.sh, the golden pins and dar's recorded command line are byte-identical
  to `v1.1.0`; `create.rs` changed only to add the fadvise-free command and its EPERM retry.
- **C10.** A 1.1.0 binary rebuilds a 1.1.1 tape (no format file changed, `requires` is
  empty). A 1.1.0 binary does not open a catalog at schema 036: recorded in the release
  notes and handoff (f).
- **C14.** age headers show recipient-stanza counts on every file, which groups envelopes
  and slices by recipient-set size, not by name; not in File 1's "what the unencrypted
  parts reveal". Not a defect under the threat model.
- **C16.** The answer to "a tape-only unit's one copy fails verify": `volume read-slices
  --from <good label> --unit U` re-promotes the stage set from the good copy (verified
  slice by slice), then `volume init` and `volume write`; `audit`'s `copy_count` fix line
  prints exactly this. Item 5 above names the limits.
- **B8.** Step 7's re-entry re-asserts the profile's `staging.jobs`, contender list and
  dar path: now in handoff (f).
- **A2's measurement** and **#420's** share one afternoon on EW7VWMVKF6.
- **Lens B's "not reached"**: whether the operator's account has a `~/.tapectl` (it would
  bypass the pointer; handoff (f) now says to rename it) and whether `keepsake` is in two
  places (the `location_count` check exits 2 otherwise, and the weekly audit timer reports
  that as a failure).

## 6. Ratified behaviour checked and found as ruled

Item 57: the chip proof applies only to an unreadable File 0 with a serial and no live
volume on the cartridge; the erase proof only to an EMPTY File 0 after a short verified
erase; "no write contact since" is by contact id; unreadable MAM proves nothing. Item 63:
release after Ok (quick or full), per unit by the least-covered live version with the
tape-only floor, all or nothing, `--keep-staging` honoured, a failed release a warning;
`copy_count_expr` counts volumes and the same-cartridge case is prevented upstream
(displacement, `copy_distinctness`, the File 0 check). Item 59: `--force` reaches the init
only; `--init` conflicts with `--loaded`; a failed move is a warning. #435/#442/#443: lookup
by uuid only; `Unknown`, `NoIdentity`, `UnderAnotherLabel` refused with no `--force`; a
refused no-label read closes a `failed` contact. Items 61 and 64: eject takes the drive lock
non-blocking, exit 75 on a holder, `MTOFFL` in one place, `cartridge_volumes` never
written. #441: `fadvise64` (221) only, `SECCOMP_RET_ERRNO` with errno 0, `dar -c` only,
nothing allocated in `pre_exec`, EPERM at spawn retried without the filter. #437: root
ignores the pointer; `status` never opens the catalog, so an ops member never runs a
migration. #431: config load never fails on an absent drive. The upgrade (f): step 3 builds
a release binary on home2 with the pinned toolchain under the build lock; step 7 and step
14 are idempotent; the timers' sandboxes allow the migration; no new crate since `v1.1.0`.

## 7. Reconciliation

| Lens | Raw | Filed | Fixed in docs | Recorded only |
|---|---|---|---|---|
| A | 8 | A1, A2, A7 → #444; A4 → #445; A3 → #446; A6, A8 → #447 | — | A5 |
| B | 10 | B10 (script comment) → #446 | B1, B2, B3, B4, B5, B6, B7, B8, B10 (install.md) | B9 |
| C | 16 | C5, C6, C11, C12, C13 → #448; C5, C7, C8 → #449; C3 → #450; C9 → #451 | C1, C2, C15, C10 | C4, C14, C16 |

34 raw = 17 in issues + 14 fixed in docs + 7 recorded only, with C5, C10 and B10 each
counted where they were split. Nothing was dropped.

## 8. Not reached

Nothing was run. `src/tape/cleaning.rs`, `src/cli/report.rs`, `src/tape/poll.rs`,
`policy/tape_alerts.rs`, `src/volume/cost_budget.rs`, `src/staging/jobs.rs`, most of the
`restore.rs` diff, `first-run.sh`'s raised-shell logic and `check-tapectl-op.sh` were not
read. `operator-guide.md` and `troubleshooting.md` were read where grepped. Whether
`runuser -u` on util-linux 2.33 sets `HOME` (the wrapper's comment says so; `check-tapectl-op.sh`
is the proof, now in handoff (f)) and how mhvtl answers a short `cartridge erase` (#444's
gate leg) are the two facts left to the next run.
