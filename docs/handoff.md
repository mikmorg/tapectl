# Handoff: where tapectl stands, and what only you can do

Rewritten 2026-09-24 (master `04e254c` and later), ruled by the CTO that day (ADR-0012's
2026-09-24 amendment, item 10). Earlier versions of this file are in git history; they
are dated records of the 2026-08 and 2026-09-14 states and are superseded by this one.
It answers one question: **which remaining work needs a person, and which does not?**

> **2026-09-28 — production moved to home2** (ADR-0012's amendment of that date). The
> readiness review found that no production install ever existed on `vm-desk1` (only the
> mhvtl rehearsal under `--home`), and a 2026-09-13 pre-redesign attempt on home2 left a
> stale home, binary and a chowned `/srv/acache`. The CTO moved production to home2. The
> procedure is now `contrib/hosts/home2-prep.sh --apply`, then
> `scripts/first-run.sh --profile contrib/hosts/home2.profile` from step 1 (`docs/install.md`,
> "Host profiles"). Items below that say "this VM" are superseded where they conflict.

> **2026-09-29 — the pre-production pass landed** (master `21212a1`; ADR-0012's
> 2026-09-29 amendment). The documentation pass's 18 issues (#345–#362) are fixed and
> closed except #360 (ruled 2026-09-29; built after the first write), with the File 0 `tr` fix (#349's neighbour) and
> the `tar` check in RESTORE.sh. Verified together: 2214 tests, the mhvtl gate GREEN
> **40/40** (new: `empty_drive_refused`), `lifecycle-suite --all` GREEN (399 passed,
> 13 structural skips), `first-run.sh` end to end on mhvtl, migration 026 on a real
> populated catalog. Residual non-blocking items: #363. What changes on home2:
> 1. **Back up the catalog with the binary you have, before pulling** — the new binary
>    applies migration 026 on its first run:
>    `sudo -u tapectl -H tapectl db backup --to /srv/local_backup/tapectl/pre-026.db`
> 2. `git pull`, then `scripts/first-run.sh --profile contrib/hosts/home2.profile --from 3 --to 4`.
>    Step 0 offers to rename `[defaults] min_copies_for_tape_only`/`min_locations_for_tape_only`
>    to `min_copies`/`min_locations` in place — answer y (the new binary refuses the old names).
> 3. `… --from 12 --to 12` — the rehearsal again (a new binary is a new artifact; it erases
>    EW7VWMVKF6).
> 4. `… --from 13` — the first production tape. `volume verify` now exits 2 only when the
>    medium is proven bad and 3 when inconclusive.

> **2026-09-30 — the structural review; RESTORE.sh fixed before the first tape; 1.0.0**
> (ADR-0012's 2026-09-30 amendment). Nothing in the schema or the format had to change;
> the code and catalog findings are issues #373–#385, built after the first write. What
> did change before it: RESTORE.sh decrypts into scratch space inside `--to` (or
> `--scratch DIR`) after a space check, names a full disk as one, streams `--verify`, and
> refuses a layout other than v2; the package version is `1.0.0`. The first collection was
> staging with binary `6a8d1bf` when this landed. Staged slices are unaffected (every
> changed byte is generated at `volume write`), so on home2:
> 1. **Let staging finish.** Step 13 stops at the WRITE confirmation (you type the label):
>    answer no there. Never pull, build or install while a stage, write or confirm runs.
> 2. Back up the catalog: `sudo -u tapectl -H tapectl db backup --to /srv/local_backup/tapectl/pre-1.0.0.db`.
> 3. `git pull`, then `scripts/first-run.sh --profile contrib/hosts/home2.profile --from 3 --to 4`
>    (the release build and the ungated tests). No migration comes with it.
> 4. `… --from 12 --to 12` — the rehearsal on the rebuilt binary (it erases EW7VWMVKF6).
> 5. `… --from 13` — it snapshots again (unchanged units mint nothing), skips what is
>    staged, and returns to the WRITE confirmation. `tapectl --version` should read `1.0.0`.
> 6. After the write session, regenerate the Heir Kit (its cover now states the disk space
>    a restore needs) and back up the catalog.
>
> Operator rules until #376/#377/#378 land: one tapectl writer at a time; no
> `volume abort`, `volume resume`, `staging clean --force` or `db import` during a write or
> confirm; no `unit tag`, `unit rename` or dotfile edit on an archived unit.

## The state, in one paragraph

Every pre-production gate the CTO set is met. The `review-2026-09-13` queue is empty
(four adversarial review rounds, the last recorded in
`docs/audits/2026-09-23-preproduction-review-4.md`). The mhvtl gate is GREEN 39/39 with
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

## What was done before the first production write (ruled 2026-09-24) — ALL DONE

Verified together on master `04e254c` (2026-09-24): 2054 tests, the mhvtl gate GREEN 39/39,
lifecycle `--all` GREEN (412 checks, 402 passed, 0 failed, 10 structural skips), and
`scripts/first-run.sh` end to end on mhvtl.

1. **Build identity** — `tapectl --version` and every journal row name the commit
   (`0.1.0 (<sha>, <date>)`); on-tape bytes unchanged (pinned by test).
2. **#301** — every contact journals the st driver's sysfs counters at its open and close
   (migration 025; gate step `st_stats_recorded`). **#306** — every restore writes a
   `restores` row with dar's report verbatim (migration 024). #300 closed as satisfied.
3. **#342** — write, resume and verify sweep on every outcome after the contact opened.
4. **The install procedure** — `docs/install.md`, `scripts/install-systemd.sh`, the audit
   and catalog-backup timers and the `tapectl-op` wrapper, installed by `first-run.sh`
   step 14 and removable by `--uninstall`.
5. **The quiet-host check** — `tapectl host check`; `volume write` asks when the host is
   loaded, short of memory or running a listed contender; step 13 shows the findings and
   the pause commands. A warning and a question, never a refusal.
6. **This file and `docs/lto6-validation-checklist.md`** rewritten to the current state.

Follow-ups filed, all post-production: #343 (dar's `-c` report at staging), #344 (MTIOCGET
and sense capture).

## Only you can do these

1. **Say when.** The production write is your call. The procedure, on home2, is
   `contrib/hosts/home2-prep.sh --apply`, then
   `scripts/first-run.sh --profile contrib/hosts/home2.profile` from step 1 (every step
   detects work already done). It builds the release binary, rehearses it on the test
   cartridge (step 12 is required; the marker is per binary and per host), then writes,
   verifies, audits and refreshes the heir kit. Run it inside tmux: the first tape
   (~1.3 TB) is days at the drive.
2. **The Heir Kit ceremony.** The kit prints the escrow *identity*; you copy the secret
   `tapectl init` showed you once into the sheet's box by hand, check the pair with
   `age-keygen -y` as the sheet says, seal, and keep two copies in independent failure
   domains. Refresh after each write session (`audit` warns when stale).
3. **Cartridges and places.** Keep EW7VWMVKF6 as the standing test cartridge, never
   production. Have at least two production cartridges so copy 2 follows copy 1 the same
   day (staging still holds the slices: `volume init <label-2>`, `volume write <label-2>`,
   `volume move --to <offsite>`); register two locations first.
4. **Storage.** Each tape holds what staging can hold in one session; on home2 staging
   is `/srv/acache/tapectl-staging` (~2.3 TB free).
5. **A quiet host.** Nothing on home2 touches acache or the drive (2026-09-28); keep it so
   for every write and verify (`docs/operator-guide.md`, "A quiet host while the tape runs").
6. **Two follow-ups when convenient:** a second expendable cartridge for the
   multi-cartridge scenarios on hardware; the write-throughput work (#326) profiled on the
   first production tape, then the reader-thread change.

## The open backlog (post-production, in the order the coordinator proposed)

#308 (audit reads health), #309 (scheduled health), then the rest of the forensics epic
#311 (#299, #302–#305, #307, #310); #326; #143 (won't-fix unless you want those commands);
#144 (until bin-packing matters).

## #416 cost budgets: what landed, and where the rest goes

Branch `recovery`, 2026-10-07. The brief was to build the budget harness and the
budgets #416 lists for the ungated suites, using the counting fakes that already
exist, and to name the budgets that need mhvtl. Of the 13 items:

**Landed (ungated, in `cargo test`):**
- **Item 3, the FakeTape distance model:** `src/volume/cost_budget.rs`. It covers
  a write and its confirm, verify (full and quick), catalog rebuild, the raw dump,
  envelope open, read-slices, compact-read, the resumed readback and binding, on a
  150-slice unit plus a 47-slice unit, with each slice modeled as 10 GiB. The tests
  are `a_write_and_its_confirm_stay_within_their_budgets`,
  `every_read_path_stays_within_its_budget`, and two positive controls:
  `a_seal_first_confirm_after_a_write_breaks_the_confirm_budget` and
  `the_pre_1_0_5_rewind_per_read_breaks_every_multi_file_budget`. This
  approximates item 2's shape under FakeTape, but **item 2's gate leg is not
  built**.
- **Item 6, the overlap budget:** three tests in `src/pipeline.rs`:
  `a_slow_source_and_a_slow_store_overlap`, `a_slow_tape_read_and_a_slow_sink_overlap`
  and `the_same_stages_in_turn_are_over_the_overlap_budget` (the positive control).
- **Item 10, the catalog budgets:** `tests/catalog_budget.rs`, which pins
  statement counts and query plans for ls, search, locate, stats, the restore
  lookup, audit and collection status, with three positive controls. The
  catalog.db build's commit budget is not repeated there. It stays pinned by
  #399's `the_build_commits_once_whatever_the_row_count` in `db::ontape_catalog`.
- **Item 13, ENOSPC under `write_stream`:** `FakeTape`'s `capacity`, and
  `enospc_from_the_drive_at_a_slice_aborts_unsealed` in `src/volume/session.rs`.

**Already pinned on master before this branch:**
- **Item 5, the RESTORE.sh motion log:** `tests/heir_restore_sh.rs`, in
  `a_multi_slice_restore_rewinds_once_and_only_spaces_forward`,
  `verify_rewinds_once_and_reads_the_seal_marker_last` and
  `info_and_find_envelope_rewind_once`. This is the after-#396 bound, not the
  "rewinds = files read" ceiling the issue names as the interim.

**Routed to the coordinator: these need a kernel st driver (mhvtl or the real drive):**
- Item 1: st counter budgets from `st_stats_journal`, in the gate and in
  `realdrive-forensics.py`. The counters are the kernel's.
- Item 2: the wide-tape gate leg (about 150 slices of 1 MiB, plus a 47-slice
  unit, through every path).
- Item 7: mhvtl latency (`vtlcmd` delays in the gate preamble) and the
  phase-duration assertions that depend on it.
- Item 8: the drive duty cycle in forensics (`write_ns`/`read_ns` against the
  phase duration). It is meaningful only on the real drive.
- Item 12, the comparison against the kernel's `other_cnt` delta. The other half
  of item 12, TapeStore motion counters recorded into `phase_timings`, is
  production instrumentation like item 4 below, and it is not built. In tests,
  FakeTape's op log already counts the motions, and item 3 uses it.

**Routed to the gated `TAPECTL_PERF_TESTS` suite:**
- Item 9: the perf harness through a Store, with ratios to an in-test sha256
  calibration.
- Item 11: the 100k-file unit (linear scaling, the statement ceiling, VmHWM).

**Item 4, hash and disk-pass budgets: not built, and outside this brief.** No
existing fake can count it. The write path does not hash through one type:
- `pipeline::hash_stage` (execute's inline L2 hash, the main pass) builds its
  own raw `Sha256`.
- `layout_model::hash_file` (`--prewrite-hash` and the materialized zones) does
  the same.
- `build.rs`'s `sha256_hex` hashes materialized bytes in memory.
- Only confirm's content-file hash (`store::chain_walk`) goes through
  `util::HashingWriter`.

A counting wrapper around `util::HashingReader`/`HashingWriter` would therefore
miss the main pass, which is exactly the missed counter that the 0.95x floor
exists to catch. Item 4 as written needs two new production-side pieces:

1. One counted hasher type that every hashing site uses, plus a counted
   staging read (`util::DropBehind`).
2. A place to persist the counts. "Recorded into `phase_timings`" means either
   new columns, which is a migration (none was expected here), or a new
   convention for the existing `bytes` column, which today means progress bytes.

What covers it today is narrower and does not count passes:
- `execute_reads_the_staged_files_through_drop_behind` (session.rs) checks that
  every staged slice is opened through `DropBehind`.
- `SliceCheck::Size`, the default, reads only metadata. Its tests are in
  `layout_model.rs`.

**Who builds it:** an agent, in a follow-up #416 brief, once the coordinator
rules on where the counts are stored (new `phase_timings` columns or something
else).

## Hazards to keep in mind

- `/dev/nstN` numbering moves across reboots; the real drive is
  `/dev/tape/by-id/scsi-HUJ808A5L4-nst`; mhvtl drives are `scsi-XYZZY_A*`. Every harness
  and `first-run.sh` refuse to default to a device.
- The drive belongs to home2. Lending it to `vm-desk1` means attaching
  `contrib/hosts/home2-lto6-hostdev.xml` there; the two hosts share no tape lock, so detach
  it again before any production contact.
- MAM's "remaining capacity" is not host-writable space: the drive stops host writes at its
  early-warning point with ~107 GB still "remaining". tapectl plans against the generation
  table times the fill ceiling (`fill_ceiling`, 97% by default; 92% before #391), never against MAM remaining.
- A bursty host feed costs tape (1.48 native bytes per data byte measured); a steady one
  does not. Keep the host quiet.
- The tape lock is `/tmp/tapectl-tape.lock`, per host; a second user on the same host is
  refused, not queued.
