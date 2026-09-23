# 2026-09-23: real-drive rehearsal on the HP LTO-6

The CTO's pre-production rehearsal (handoff step 3), run on the finished tree
(master `a51b744`, every `review-2026-09-13` issue closed but #323) against the
real HP Ultrium 6-SCSI, serial HUJ808A5L4, at
`/dev/tape/by-id/scsi-HUJ808A5L4-nst`, with the CTO's expendable FUJIFILM LTO-6
cartridge EW7VWMVKF6 (authorized for overwrite). Every command ran in a
temporary home under `/scratch/tapectl-lifecycle/run-20260923-*`, never
`~/.tapectl`.

## Verdict

**GREEN on hardware: 15 lifecycle scenarios, 0 failures**, plus a DR rebuild
and restore from the real tape with byte-identical output. Two findings, both
in the harness, one fixed in this record's commit and one an open question for
the CTO.

## The first run was RED, and it was the harness

`lifecycle-suite.sh --scenario first-year --device <by-id> --erase short
--single-cartridge --i-will-lose-the-cartridge EW7VWMVKF6` failed 31 of 47
checks. `fy.write` failed at `volume init`:

> an LTO-8 drive cannot write LTO-6 media. This is a physical limit of the
> drive, not a policy — --force does not override it.

tapectl was right. The harness wrote its temp config's `[[backends.lto]]` entry
as `generation = "LTO-8"` (mhvtl's generation) for every drive, plus mhvtl's
`capacity_override`. That hardcoding came in with ADR-0010 (2026-09-13), after
the last real-drive pass (2026-09-11, 45/45), so the suite's real-drive mode
had been broken for ten days and nothing ran it. This is exactly what a
rehearsal is for.

**Fix (this commit):** in real-drive mode the generation comes from the model
the kernel read at probe time (`/sys/class/scsi_tape/<node>/device/model`:
`Ultrium 6-SCSI` → LTO-6, `ULT3580-TDn` → LTO-n), and `capacity_override` is
written only for mhvtl, so a real drive sizes the cartridge the way production
does (from the detected generation, ADR-0010). Verified on both: mhvtl
`first-year` 47/47 with LTO-8 and the override present; the real drive with
LTO-6 and no override.

## Scenario results on the real drive

| Scenario | Checks | Passed | Failed | Skipped | Run |
|---|---|---|---|---|---|
| first-year | 47 | 47 | 0 | 0 | run-20260923-184033 |
| evolving-source | 41 | 40 | 0 | 1 | 184340 |
| key-rotation | 12 | 9 | 0 | 3 | 184651 |
| tenant-reassign | 18 | 16 | 0 | 2 | 184822 |
| tape-only-and-reclaim | 10 | 6 | 0 | 4 | 185010 |
| retire-and-reuse | 10 | 3 | 0 | 7 | 185246 |
| db-loss | 6 | 6 | 0 | 0 | 185338 |
| escrow-ordering | 29 | 29 | 0 | 0 | 185439 |
| restore-file-and-catalog | 10 | 10 | 0 | 0 | 185535 |
| quick-archive | 14 | 14 | 0 | 0 | 185628 |
| collection | 27 | 25 | 0 | 2 | 185711 |
| permute (seed 1, 40 steps) | 112 | 106 | 0 | 6 | 185807 |
| stale-catalog-sealed-tape (#208) | 4 | 4 | 0 | 0 | 190927 |
| cartridge-displacement | 1 | 0 | 0 | 1 | 191004 |
| collection-second-copy | 1 | 0 | 0 | 1 | 191005 |
| **total** | **342** | **315** | **0** | **27** | |

`compaction` was not run: it needs four distinct cartridges.

**Every skip is stated and structural**, three kinds:

1. *Single-cartridge reuse* (16 skips across evolving-source, key-rotation,
   tenant-reassign, tape-only-and-reclaim, retire-and-reuse): the earlier
   volume's cartridge was erased to become the next volume, so "restore from
   the old volume" and "second independent copy" checks have nothing to read.
   In particular `retire-and-reuse`'s own ADR-0003 refusal checks were
   skipped for this reason; the same refusal is proven on hardware by
   `stale-catalog-sealed-tape` (4/4: a sealed tape refuses a write the catalog
   still thinks is allowed).
2. *Whole-scenario impossibility with one cartridge* (2): cartridge-displacement
   needs a real erase (hours on real LTO) plus a second cartridge;
   collection-second-copy needs two simultaneously distinct cartridges.
3. *Precondition skips identical to the mhvtl run* (9, in collection and
   permute): no second tenant to cross-test, nothing staged at that step.

Timing: each scenario took 1–3 minutes on the real drive (small microcosm
data); the whole pass, 33 minutes.

## Forensic verification beyond the pass count (ADR-0013)

Every scenario's catalog was checked with a script after the run
(`scripts/realdrive-forensics.py`, criteria below).
**All 15 GREEN.** Across the runs:

- **Drive attribution:** every contact opened through the configured backend
  names drive HUJ808A5L4 (the serial read from sysfs VPD 0x80). Operations
  seen on hardware: volume init, volume write, volume verify, restore unit,
  restore raw-volume, catalog rebuild.
- **Log-page sweep:** every closed write/verify/restore contact read exactly
  the 22 pages the drive's own page 0x00 lists, each once, each as ONE LOG
  SENSE (`--maxlen=65532`, #328), every row `ok=1` with raw bytes kept. The
  first-year run alone journalled 352 page reads. No `volume init` contact
  sweeps — see the open question.
- **Health readings:** every reading has counters (no NULLs); uncorrected
  errors 0 throughout; **no TapeAlert flag was raised on any read**, so
  0x2E read-to-clear remains unanswered (as on 2026-09-23 morning).
- **Capacity from the detected generation:** every volume has
  `capacity_bytes = 2 500 000 000 000` (the LTO-6 generation table); MAM
  reports 2 620 446 998 528 native. The cartridge row is
  serial EW7VWMVKF6, media LTO-6, `in_use`.
- **#327 on hardware:** after every `weof 1` at BOT the real drive reads File
  0 as EMPTY and `volume init --force` logs "--force overriding an empty File
  0 (a filemark at BOT)" — 21 times across the pass. (mhvtl returns the old
  bytes instead; the refusal and the flag are the same either way.)

## DR from the real tape: rebuild and restore

After the pass the cartridge held `VOL-S` (sealed, from
stale-catalog-sealed-tape). With the run's operator key:

1. **Heir-style rebuild** into a bare home with NO drive configured:
   `catalog rebuild --from-volume --device <by-id> --key <operator>` → rc 0,
   1 unit, 1 snapshot, 1 stage set, 1 write, 2 envelopes opened, tenants from
   the on-tape `catalog.db`, cartridge EW7VWMVKF6 registered and bound. The
   contact is recorded with reason "no LTO backend is configured on this host"
   and tapectl warned that no drive health was collected — the DR read stays
   honest about what it could not attribute.
2. **Rebuild with the drive configured** → the contact names HUJ808A5L4 and
   cartridge EW7VWMVKF6 (#335, the rebuild attaching the cartridge it
   registered), one 22-page sweep, one `restore` health reading with 0
   uncorrected errors.
3. **Restore from the rebuilt catalog**: `restore unit --unit photos --from
   VOL-S` → `diff -r` against the run's source tree: **identical**.

## Open question for the CTO

`volume init` takes no log-page sweep. That matches ADR-0013's 2026-09-23
amendment, which names write/resume/verify and the read paths, but init does
write File 0 and takes two MAM reads. Adding it would cost one 22-page sweep
per init. Not changed here; it is a ruling, not a defect.

## Not done

- **The ENOSPC fill** (write the cartridge to physical end of tape): 6+ hours
  at this drive's rate; only on the CTO's word.
- **TapeAlert read-to-clear**: needs a cartridge that raises a flag.
- `compaction`, `cartridge-displacement` and `collection-second-copy` on
  hardware: need more than one cartridge.
