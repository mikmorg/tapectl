# Handoff: where tapectl stands, and what only you can do

Rewritten 2026-09-24 (master after `d3c514e`), ruled by the CTO that day (ADR-0012's
2026-09-24 amendment, item 10). Earlier versions of this file are in git history; they
are dated records of the 2026-08 and 2026-09-14 states and are superseded by this one.
It answers one question: **which remaining work needs a person, and which does not?**

## The state, in one paragraph

Every pre-production gate the CTO set is met. The `review-2026-09-13` queue is empty
(four adversarial review rounds, the last recorded in
`docs/audits/2026-09-23-preproduction-review-4.md`). The mhvtl gate is GREEN 38/38 with
`EXPECTED_FAIL=()`; the lifecycle suite is GREEN across all 16 scenarios on mhvtl. The
real-drive rehearsal ran on 2026-09-23 on the HP LTO-6 passed through to this VM
(`docs/runs/2026-09-23-real-drive-rehearsal.md`): 15 lifecycle scenarios, 342 checks,
0 failed; a DR rebuild and restore from the real tape byte-identical; the release
binary rehearsed 47/47; the end-of-tape fill measured 2.5020 TB before ENOSPC, 1.0008
times the 2.5 TB planning figure. The bootstrap procedure `scripts/first-run.sh` was
reviewed against the current binary, rewritten as a tutorial and verified end to end on
mhvtl on 2026-09-24. **The first production write is on hold by the CTO's word** (2026-09-24)
and starts only when the CTO says so.

## What is done, with the record for each

| Gate | Record |
|---|---|
| The 2026-09-13 review's queue worked to zero | `docs/audits/2026-09-13-post-redesign-review.md`, then `...-09-17`, `...-09-18`, `...-09-21`, `...-09-23-preproduction-review-4.md` |
| Real-drive rehearsal, DR from real tape, release-binary rehearsal, EOT fill | `docs/runs/2026-09-23-real-drive-rehearsal.md` |
| MAM capacity unit (MiB) and the feed-rate effect | `docs/runs/2026-09-23-lto6-capacity-measurement.md` (#182, #323) |
| Hardware facts from the first LTO-6 session | `docs/lto6-session-journal-2026-09-10.md`, `docs/runs/2026-09-11-unattended.md` |
| The forensics record (ADR-0013): every contact sweeps once, journals verbatim | ADR-0013 and its 2026-09-23 amendments; #298, #320, #339 |
| The install/bootstrap procedure | `scripts/first-run.sh` (verified on mhvtl 2026-09-24); `docs/install.md` (the runbook, ADR-0012 2026-09-24 item 6) |

## Decisions already ratified — do not re-litigate

- **2026-09-14:** ADR-0012 (copies are identical content, cartridges are known by serial)
  and the dated corrections inside ADR-0010/0011; `CONTEXT.md` vocabulary.
- **2026-09-16 to 2026-09-23:** dated amendments in ADR-0012 and ADR-0013 (resume adoption
  of aborted sessions, read paths sweep, every contact sweeps, the evening rulings of
  2026-09-23: #323 closed, write throughput is not a gate, nothing on the tape thread may
  stall the drive, one dd fill, TapeAlert stays unanswered but surfaced, production on
  this VM with one release-build rehearsal).
- **2026-09-24:** ADR-0012's amendment "what is done before the first production write"
  (build identity, #301/#306 before production, backups, a formal install procedure, the
  quiet-host check, this rewrite) and ADR-0005's amendment (#341, option 2).
- The process rules that outlived the queue: `.claude/skills/unattended-run/SKILL.md`
  and the autopilot policy block. The mhvtl gate runs per item for write- and
  restore-path changes and in batches otherwise; the lifecycle suite's measured-green
  invocation is `--all --device /dev/nst1 --erase short` on mhvtl.

## What remains before the first production write (all ruled 2026-09-24)

Agent work, in flight or queued; none needs the CTO's hands:

1. **Build identity** — `--version` and every journal row name the commit the binary was
   built from; on-tape bytes unchanged.
2. **#301** (kernel per-device tape counters at each contact) and **#306** (`restore`
   records what it did, keeps dar's report) — the two forensics items whose data is lost
   if not captured. #300 is closed as satisfied by #320 and #339.
3. **#342** — `volume write`/`verify` sweep on every outcome after the contact opened.
4. **A formal install procedure** — `docs/install.md`, `scripts/install-systemd.sh`, the
   audit and catalog-backup timers and the `tapectl-op` wrapper, all installed by
   `first-run.sh`, all removable.
5. **The quiet-host check** — step 13 and `volume write`'s pre-flight warn when known
   contenders are active or the host is loaded, and ask; never a refusal.
6. **This file and `docs/lto6-validation-checklist.md` rewritten** to the current state.

## Only you can do these

1. **Say when.** The production write is your call. The procedure is
   `scripts/first-run.sh` from a bare machine, or `--from 13` on this one; it builds the
   release binary, rehearses it on the test cartridge (step 12 is required; the marker is
   per binary), then writes, verifies, audits and refreshes the heir kit.
2. **The Heir Kit ceremony.** The kit prints the escrow *identity*; you copy the secret
   `tapectl init` showed you once into the sheet's box by hand, check the pair with
   `age-keygen -y` as the sheet says, seal, and keep two copies in independent failure
   domains. Refresh after each write session (`audit` warns when stale).
3. **Cartridges and places.** Keep EW7VWMVKF6 as the standing test cartridge, never
   production. Have at least two production cartridges so copy 2 follows copy 1 the same
   day (staging still holds the slices: `volume init <label-2>`, `volume write <label-2>`,
   `volume move --to <offsite>`); register two locations first.
4. **Storage.** Each tape holds what staging can hold in one session; `/scratch` today is
   about 90 GB. Attach a dedicated staging disk (2.5–3 TB) before any tape you would not
   want split across cartridges (ruled: after the first small cycle).
5. **A quiet host.** For every write and verify, pause the CI runner's timers and heavy
   lanes on this VM (`docs/operator-guide.md`, "A quiet host while the tape runs").
6. **Two follow-ups when convenient:** a second expendable cartridge for the
   multi-cartridge scenarios on hardware; the write-throughput work (#326) profiled on the
   first production tape, then the reader-thread change.

## The open backlog (post-production, in the order the coordinator proposed)

#308 (audit reads health), #309 (scheduled health), then the rest of the forensics epic
#311 (#299, #302–#305, #307, #310); #326; #143 (won't-fix unless you want those commands);
#144 (until bin-packing matters).

## Hazards to keep in mind

- `/dev/nstN` numbering moves across reboots; the real drive is
  `/dev/tape/by-id/scsi-HUJ808A5L4-nst`; mhvtl drives are `scsi-XYZZY_A*`. Every harness
  and `first-run.sh` refuse to default to a device.
- MAM's "remaining capacity" is not host-writable space: the drive stops host writes at its
  early-warning point with ~107 GB still "remaining". tapectl plans against the generation
  table times 0.92, never against MAM remaining.
- A bursty host feed costs tape (1.48 native bytes per data byte measured); a steady one
  does not. Keep the host quiet.
- The tape lock is `/tmp/tapectl-tape.lock`; a second user is refused, not queued.
